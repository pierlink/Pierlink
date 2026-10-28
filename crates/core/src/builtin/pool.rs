//! 按服务器合并登录，按流量类别复用连接；锁内不握手、不调用界面。
use super::*;
use pierlink_ssh_engine::{Authentication, ConnectionOptions, Session};
use std::{
    collections::HashSet,
    sync::{
        Condvar,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// 按服务器 ID 读取完整记录；None 表示记录已删除，错误必须已脱敏。
pub type ServerResolver = dyn Fn(&str) -> Result<Option<Server>, AppError> + Send + Sync;

struct Entry {
    class: ConnectionClass,
    connection: Connection,
    idle_since: Option<Instant>,
}
#[derive(Default)]
struct SlotState {
    entries: Vec<Entry>,
    connecting: bool,
    flight: Option<Arc<Flight>>,
    requests: Vec<(u64, Weak<AtomicBool>)>,
    state: Option<ConnectionState>,
    ever_connected: bool,
    // 达到登录上限后，后台和自动请求直接拿这个结果，不再去服务器消耗尝试次数；
    // 用户主动连接（比如从 agent 移除多余的密钥后点“重新连接”）照常再试，成功后清除。
    auth_limit: Option<AppError>,
}
#[derive(Default)]
struct Flight {
    result: Mutex<Option<Result<(), AppError>>>,
    changed: Condvar,
}
impl Flight {
    fn wait(
        &self,
        request: &ConnectionRequest<'_>,
        pool: &Pool,
        slot: &Slot,
    ) -> Result<(), AppError> {
        let mut result = self.result.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            check(request, pool, slot)?;
            if let Some(result) = &*result {
                // 其他请求的交互结果不决定本请求的认证；回到池中用自己的权限重试。
                if let Err(error) = result
                    && (error.code == "ssh.auth_cancelled"
                        || request.interactive
                            && matches!(
                                error.code.as_str(),
                                "ssh.password_needed" | "ssh.two_factor_required"
                            ))
                {
                    return Ok(());
                }
                return result.clone();
            }
            result = self
                .changed
                .wait_timeout(result, Duration::from_millis(25))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
    fn finish(&self, result: &Result<Connection, AppError>) {
        *self.result.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(result.as_ref().map(|_| ()).map_err(Clone::clone));
        self.changed.notify_all();
    }
}

#[derive(Default)]
pub(super) struct Slot {
    state: Mutex<SlotState>,
    changed: Condvar,
    pub(super) closed: AtomicBool,
    pub(super) revision: AtomicU64,
}
#[cfg(test)]
impl Slot {
    /// 无参数；返回是否有请求已取得当前 flight 的等待引用，供并发测试同步。
    pub(super) fn has_waiter(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .flight
            .as_ref()
            .is_some_and(|flight| Arc::strong_count(flight) > 2)
    }
}
pub(super) struct Pool {
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    resolver: Mutex<Option<Arc<ServerResolver>>>,
    pub(super) events: state::Events,
    pub(super) network: AtomicU64,
    closed: AtomicBool,
    started: OnceLock<Result<(), AppError>>,
    sequence: AtomicU64,
    idle_timeout: Duration,
    #[cfg(test)]
    pub(super) connector: Mutex<Option<Arc<TestConnector>>>,
}
#[cfg(test)]
pub(super) type TestConnector = dyn Fn(
        &Server,
        Option<&Connection>,
        ConnectionOptions,
        Authentication,
    ) -> pierlink_ssh_engine::Result<Connection>
    + Send
    + Sync;

impl Pool {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: Mutex::new(HashMap::new()),
            resolver: Mutex::new(None),
            events: state::Events::default(),
            network: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            started: OnceLock::new(),
            sequence: AtomicU64::new(0),
            idle_timeout: Duration::from_secs(300),
            #[cfg(test)]
            connector: Mutex::new(None),
        })
    }

    fn start(self: &Arc<Self>) -> Result<(), AppError> {
        self.started
            .get_or_init(|| {
                let weak = Arc::downgrade(self);
                std::thread::Builder::new()
                    .name("ssh-idle".into())
                    .spawn(move || {
                        loop {
                            let Some(pool) = weak.upgrade() else {
                                break;
                            };
                            if pool.closed.load(Ordering::Acquire) {
                                break;
                            }
                            pool.sweep(Instant::now());
                            drop(pool);
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    })
                    .map(|_| ())
                    .map_err(|_| {
                        AppError::new("ssh.task_failed").with("reason", "无法启动连接回收线程")
                    })
            })
            .clone()
    }

    pub(super) fn register(
        &self,
        slot: Arc<Slot>,
        flag: &Arc<AtomicBool>,
    ) -> Result<Registration, AppError> {
        let id = self.sequence.fetch_add(1, Ordering::Relaxed);
        let mut state = slot.state.lock().unwrap_or_else(|p| p.into_inner());
        if self.closed.load(Ordering::Acquire) || slot.closed.load(Ordering::Acquire) {
            return Err(AppError::new("ssh.auth_cancelled"));
        }
        state.requests.push((id, Arc::downgrade(flag)));
        drop(state);
        Ok(Registration { slot, id })
    }

    pub(super) fn slot(&self, id: &str) -> Arc<Slot> {
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(id.into())
            .or_default()
            .clone()
    }

    pub(super) fn publish(&self, id: &str, state: ConnectionState, error: Option<AppError>) {
        let Some(slot) = self
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
        else {
            return;
        };
        if slot.closed.load(Ordering::Acquire) || self.closed.load(Ordering::Acquire) {
            return;
        }
        let mut inner = slot.state.lock().unwrap_or_else(|p| p.into_inner());
        if inner.state == Some(state) {
            return;
        }
        inner.state = Some(state);
        drop(inner);
        self.events.publish(ConnectionEvent {
            server_id: id.into(),
            state,
            error,
        });
    }

    pub(super) fn sweep(&self, now: Instant) {
        let slots: Vec<_> = self
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(id, slot)| (id.clone(), slot.clone()))
            .collect();
        for (id, slot) in slots {
            let mut state = slot.state.lock().unwrap_or_else(|p| p.into_inner());
            let had = !state.entries.is_empty();
            state.entries.retain_mut(|entry| {
                if entry.connection.is_closed() {
                    entry.connection.abort();
                    return false;
                }
                if entry.connection.is_exclusively_owned() {
                    let idle = entry.idle_since.get_or_insert(now);
                    if now.saturating_duration_since(*idle) >= self.idle_timeout {
                        entry.connection.abort();
                        return false;
                    }
                } else {
                    entry.idle_since = None;
                }
                true
            });
            let disconnected = had && state.entries.is_empty() && !state.connecting;
            drop(state);
            if disconnected {
                self.publish(&id, ConnectionState::Disconnected, None);
            }
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        for slot in self
            .slots
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            stop_slot(slot);
        }
    }
}

pub(super) struct Registration {
    slot: Arc<Slot>,
    id: u64,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.slot
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .requests
            .retain(|(id, _)| *id != self.id);
        self.slot.changed.notify_all();
    }
}

fn stop_slot(slot: &Slot) -> Vec<Connection> {
    slot.closed.store(true, Ordering::Release);
    let mut state = slot.state.lock().unwrap_or_else(|p| p.into_inner());
    for (_, flag) in &state.requests {
        if let Some(flag) = flag.upgrade() {
            flag.store(true, Ordering::Release);
        }
    }
    let connections: Vec<_> = state
        .entries
        .drain(..)
        .map(|entry| entry.connection)
        .collect();
    for connection in &connections {
        connection.abort();
    }
    slot.changed.notify_all();
    connections
}

fn check(request: &ConnectionRequest<'_>, pool: &Pool, slot: &Slot) -> Result<(), AppError> {
    if pool.closed.load(Ordering::Acquire)
        || slot.closed.load(Ordering::Acquire)
        || request.cancelled.load(Ordering::Acquire)
    {
        Err(AppError::new("ssh.auth_cancelled"))
    } else {
        Ok(())
    }
}

impl ConnectionManager {
    pub(super) fn copy_resolver_from(&self, source: &Self) {
        *self.pool.resolver.lock().unwrap_or_else(|p| p.into_inner()) = source
            .pool
            .resolver
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        #[cfg(test)]
        {
            *self
                .pool
                .connector
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = source
                .pool
                .connector
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
        }
    }
    /// 设置 resolver 供跳板链逐级读取服务器记录；返回值为空，不在池锁内调用 resolver。
    /// 修改服务器后先 close_server，再重新取连接，以免复用旧参数。
    pub fn set_server_resolver(&self, resolver: Arc<ServerResolver>) {
        *self.pool.resolver.lock().unwrap_or_else(|p| p.into_inner()) = Some(resolver);
    }

    /// 按 target 解析最多四级服务器链；返回由跳板到目标的记录或缺失、环路错误。
    pub(crate) fn chain(&self, target: &Server) -> Result<Vec<Server>, AppError> {
        let resolver = self
            .pool
            .resolver
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let mut seen = HashSet::new();
        let mut chain = Vec::new();
        let mut server = target.clone();
        loop {
            if !seen.insert(server.id.clone()) {
                return Err(AppError::new("ssh.jump_cycle").with("reason", "跳板链存在环路"));
            }
            if chain.len() == 4 {
                return Err(AppError::new("ssh.jump_too_deep").with("limit", 4));
            }
            let next = server.jump_server_id.clone();
            chain.push(server);
            let Some(id) = next else {
                break;
            };
            server = resolver.as_ref().ok_or_else(|| {
                AppError::new("ssh.jump_not_found").with("reason", "未配置服务器查询入口")
            })?(&id)
            .map_err(|_| {
                AppError::new("ssh.jump_lookup_failed")
                    .with("server_id", &id)
                    .with("reason", "读取跳板服务器记录失败")
            })?
            .filter(|server| server.id == id)
            .ok_or_else(|| AppError::new("ssh.jump_not_found").with("server_id", &id))?;
        }
        chain.reverse();
        for server in &chain {
            connection_options(server)?;
        }
        Ok(chain)
    }

    pub(super) fn pooled(
        &self,
        request: ConnectionRequest<'_>,
        rejected: Option<&Connection>,
    ) -> Result<Connection, AppError> {
        if self.pool.closed.load(Ordering::Acquire) {
            return Err(AppError::new("ssh.auth_cancelled"));
        }
        self.pool.slot(&request.server.id);
        let chain = self.chain(request.server)?;
        self.pool.start()?;
        let _registrations = chain
            .iter()
            .map(|server| {
                self.pool
                    .register(self.pool.slot(&server.id), &request.cancelled)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut parent = None;
        for (index, server) in chain.iter().enumerate() {
            let target = index + 1 == chain.len();
            let current = ConnectionRequest {
                server,
                class: if target {
                    request.class
                } else {
                    ConnectionClass::Main
                },
                vault: request.vault.clone(),
                callbacks: request.callbacks.clone(),
                interactive: request.interactive,
                cancelled: request.cancelled.clone(),
            };
            parent = Some(
                self.pooled_one(
                    current,
                    parent.as_ref(),
                    if target { rejected } else { None },
                    None,
                )
                .map_err(|error| {
                    if matches!(
                        error.code.as_str(),
                        "ssh.password_needed"
                            | "ssh.auth_failed"
                            | "ssh.two_factor_required"
                            | "ssh.too_many_auth_attempts"
                    ) {
                        error.with("server_id", &server.id)
                    } else {
                        error
                    }
                })?,
            );
        }
        parent.ok_or_else(|| AppError::new("ssh.failed").with("reason", "服务器链为空"))
    }

    fn pooled_one(
        &self,
        request: ConnectionRequest<'_>,
        parent: Option<&Connection>,
        rejected: Option<&Connection>,
        supplied_options: Option<ConnectionOptions>,
    ) -> Result<Connection, AppError> {
        let id = &request.server.id;
        let slot = self.pool.slot(id);
        let _registration = self.pool.register(slot.clone(), &request.cancelled)?;
        let mut state = slot.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            check(&request, &self.pool, &slot)?;
            // 单条验证码连接优先复用已登录连接，后台不再次发起认证。
            let single = request.vault.seen_two_factor(id);
            let class = if single {
                ConnectionClass::Main
            } else {
                request.class
            };
            state.entries.retain(|entry| !entry.connection.is_closed());
            let first = rejected
                .and_then(|r| {
                    state
                        .entries
                        .iter()
                        .position(|e| e.connection.same_connection(r))
                })
                .map_or(0, |index| index + 1);
            let suitable = |entry: &&mut Entry| {
                (single || entry.class == class)
                    && !rejected.is_some_and(|r| r.same_connection(&entry.connection))
            };
            if let Some(entry) = state.entries.iter_mut().skip(first).rev().find(suitable) {
                entry.idle_since = None;
                return Ok(entry.connection.clone());
            }
            if single && rejected.is_some() {
                return Err(AppError::new("ssh.session_open_failed").with(
                    "reason",
                    "需要验证码的服务器仅使用一条连接，请关闭部分会话后重试",
                ));
            }
            if !request.interactive
                && let Some(error) = &state.auth_limit
            {
                return Err(error.clone());
            }
            if let Some(flight) = state.flight.clone() {
                drop(state);
                flight.wait(&request, &self.pool, &slot)?;
                state = slot.state.lock().unwrap_or_else(|p| p.into_inner());
                continue;
            }
            state.connecting = true;
            let flight = Arc::new(Flight::default());
            state.flight = Some(flight.clone());
            let connecting = if state.ever_connected {
                ConnectionState::Reconnecting
            } else {
                ConnectionState::Connecting
            };
            drop(state);
            self.publish(id, connecting, None);
            let options = match supplied_options.clone() {
                Some(options) => options,
                None => connection_options(request.server)?,
            };
            let result = self.connect_with(
                ConnectionRequest {
                    server: request.server,
                    class,
                    vault: request.vault.clone(),
                    callbacks: request.callbacks.clone(),
                    interactive: request.interactive,
                    cancelled: request.cancelled.clone(),
                },
                options,
                |options, auth| self.dial(request.server, parent, options, auth),
            );
            state = slot.state.lock().unwrap_or_else(|p| p.into_inner());
            state.connecting = false;
            state.flight = None;
            let result = match check(&request, &self.pool, &slot) {
                Ok(()) => result,
                Err(error) => {
                    if let Ok(connection) = result {
                        connection.abort();
                    }
                    Err(error)
                }
            };
            if let Ok(connection) = &result {
                state.ever_connected = true;
                state.entries.push(Entry {
                    class,
                    connection: connection.clone(),
                    idle_since: None,
                });
                slot.revision.fetch_add(1, Ordering::Release);
            }
            match &result {
                Ok(_) => state.auth_limit = None,
                Err(error) if error.code == "ssh.too_many_auth_attempts" => {
                    state.auth_limit = Some(error.clone());
                }
                Err(_) => {}
            }
            flight.finish(&result);
            slot.changed.notify_all();
            drop(state);
            match &result {
                Ok(_) => self.publish(id, ConnectionState::Connected, None),
                Err(error) => self.publish(id, error_state(error), Some(error.clone())),
            }
            return result;
        }
    }

    fn dial(
        &self,
        _server: &Server,
        parent: Option<&Connection>,
        options: ConnectionOptions,
        auth: Authentication,
    ) -> pierlink_ssh_engine::Result<Connection> {
        #[cfg(test)]
        if let Some(connect) = self
            .pool
            .connector
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return connect(_server, parent, options, auth);
        }
        let engine = self
            .engine
            .get_or_init(Engine::new)
            .as_ref()
            .map_err(Clone::clone)?;
        match parent {
            Some(parent) => engine.connect_via(parent, options, auth),
            None => engine.connect(options, auth),
        }
    }

    /// 在 request 对应类别调用 open；会话已满时增加连接，验证码服务器只返回错误。
    /// open 只负责打开会话，不能重复远程操作。新连接也拒绝首个会话时停止；并发竞争最多重试 32 次。
    pub fn open_session<T>(
        &self,
        request: ConnectionRequest<'_>,
        open: impl Fn(&Connection) -> pierlink_ssh_engine::Result<T>,
    ) -> Result<T, AppError> {
        let copy_request = || ConnectionRequest {
            server: request.server,
            class: request.class,
            vault: request.vault.clone(),
            callbacks: request.callbacks.clone(),
            interactive: request.interactive,
            cancelled: request.cancelled.clone(),
        };
        let mut connection = self.get_connection(copy_request())?;
        for attempt in 0..=32 {
            if request.cancelled.load(Ordering::Acquire) {
                return Err(AppError::new("ssh.auth_cancelled"));
            }
            match open(&connection) {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let mapped = error::map_engine_error(&error, error::Operation::OpenSession);
                    if mapped.code != "ssh.session_open_failed"
                        || attempt == 32
                        || attempt > 0 && connection.statistics().started_sessions <= 1
                    {
                        return Err(mapped);
                    }
                    connection = self.pooled(copy_request(), Some(&connection))?;
                }
            }
        }
        Err(AppError::new("ssh.session_open_failed"))
    }

    pub(super) fn connect_direct(
        &self,
        request: ConnectionRequest<'_>,
        options: ConnectionOptions,
    ) -> Result<Connection, AppError> {
        self.pool.start()?;
        self.pooled_one(request, None, None, Some(options))
    }

    /// 在 request 对应类别上开 PTY shell；会话已满时自动增加同类连接。
    pub fn shell(
        &self,
        request: ConnectionRequest<'_>,
        pty: &pierlink_ssh_engine::Pty,
        env: &[(&str, &str)],
    ) -> Result<Session, AppError> {
        self.open_session(request, |connection| connection.shell(pty, env))
    }

    /// 网络恢复时对全部现有连接立即探活，三秒无应答则切断；唤醒后台使用者立即重试。
    /// 应在工作线程调用，Windows 现有 network_restored 入口可直接转发至此。
    pub fn network_restored(&self) {
        let connections: Vec<_> = self
            .pool
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .flat_map(|slot| {
                slot.state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .entries
                    .iter()
                    .map(|e| e.connection.clone())
                    .collect::<Vec<_>>()
            })
            .collect();
        std::thread::scope(|scope| {
            for connection in connections {
                scope.spawn(move || {
                    if connection.probe(Duration::from_secs(3)).is_err() {
                        connection.abort();
                    }
                });
            }
        });
        self.pool.network.fetch_add(1, Ordering::Release);
    }

    /// 用户明确要求重试（比如重新开启端口转发）时，清除 server_id 记住的登录上限（agent 密钥太多被服务器断开），
    /// 下一次取连接照常去服务器登录；不影响已有连接。没有记住的上限时什么也不做。
    pub fn clear_auth_limit(&self, server_id: &str) {
        let slot = self
            .pool
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(server_id)
            .cloned();
        if let Some(slot) = slot {
            slot.state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .auth_limit = None;
        }
    }

    /// 关闭 server_id 的全部连接并取消等待；至多三秒等待登录和协议资源释放。
    /// 之后允许重新取连接；通过此服务器的下级连接随传输断开。
    pub fn close_server(&self, server_id: &str) -> Result<(), AppError> {
        let slot = self
            .pool
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(server_id);
        if let Some(slot) = slot {
            finish_close(vec![slot])?;
        }
        self.pool.events.publish(ConnectionEvent {
            server_id: server_id.into(),
            state: ConnectionState::Disconnected,
            error: None,
        });
        Ok(())
    }

    /// 关闭全部连接、取消等待并擦除口令缓存；总计至多三秒。关闭后本管理器不再建立连接。
    /// 退出时先关闭终端和后台句柄，再调用此函数，最后清空 PasswordVault。
    pub fn close_all(&self) -> Result<(), AppError> {
        self.pool.closed.store(true, Ordering::Release);
        let slots: Vec<_> = self
            .pool
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
            .collect();
        let result = finish_close(slots.iter().map(|(_, s)| s.clone()).collect());
        for (id, _) in slots {
            self.pool.events.publish(ConnectionEvent {
                server_id: id,
                state: ConnectionState::Disconnected,
                error: None,
            });
        }
        self.passphrases
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        result
    }
}

fn finish_close(slots: Vec<Arc<Slot>>) -> Result<(), AppError> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let connections: Vec<_> = slots.iter().flat_map(|slot| stop_slot(slot)).collect();
    for connection in connections {
        connection
            .wait_disconnected(deadline.saturating_duration_since(Instant::now()))
            .map_err(|e| error::map_engine_error(&e, error::Operation::Other))?;
    }
    for slot in slots {
        let state = slot.state.lock().unwrap_or_else(|p| p.into_inner());
        let (state, _) = slot
            .changed
            .wait_timeout_while(
                state,
                deadline.saturating_duration_since(Instant::now()),
                |s| s.connecting || !s.requests.is_empty(),
            )
            .unwrap_or_else(|p| p.into_inner());
        if state.connecting || !state.requests.is_empty() {
            return Err(AppError::new("ssh.task_failed").with("reason", "关闭连接等待超时"));
        }
    }
    Ok(())
}

pub(super) fn error_state(error: &AppError) -> ConnectionState {
    match error.code.as_str() {
        "ssh.password_needed" | "ssh.auth_failed" => ConnectionState::WaitingPassword,
        "ssh.two_factor_required" => ConnectionState::WaitingTwoFactor,
        _ => ConnectionState::Failed,
    }
}

fn connection_options(server: &Server) -> Result<ConnectionOptions, AppError> {
    use crate::storage::ServerSource;
    if server.source != ServerSource::Manual || server.ssh_alias.is_some() {
        return Err(AppError::new("ssh.builtin_unavailable")
            .with("reason", "内置连接需要完整的应用内服务器记录"));
    }
    let host = server
        .host
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| AppError::new("server.invalid_field").with("field", "host"))?;
    let user = server
        .username
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| AppError::new("server.invalid_field").with("field", "username"))?;
    Ok(ConnectionOptions::new(
        host,
        server.port.unwrap_or(22),
        user,
        pierlink_ssh_engine::HostKeyCheck::Fingerprint(String::new()),
    ))
}
