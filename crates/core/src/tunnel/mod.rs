//! 隧道后台调度；同一服务器最多三个连接建立中，事件仅保留每条隧道的最新值。
use crate::{
    error::AppError,
    process::{Reader, SshProcess, SshProgram, io_error},
    remote::run::classify_error,
    ssh::{
        args::{CommonOpts, Forward, ForwardTarget, Purpose, build_args},
        target::SshTarget,
    },
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
mod builtin;
pub use builtin::{BuiltinTunnel, builtin_spec};
pub mod forward;
#[cfg(windows)]
mod owner;
pub use crate::ssh::args::Readiness;

#[derive(Clone)]
pub struct TunnelSpec {
    /// 由 builtin_spec 填充；None 保持系统 OpenSSH 路径。
    pub builtin: Option<Arc<BuiltinTunnel>>,
    pub id: String,
    /// 同一实际服务器必须使用相同 ID，避免别名绕过并发上限。
    pub server_id: String,
    pub target: SshTarget,
    pub options: CommonOpts,
    pub forwards: Vec<TunnelForward>,
    pub readiness: Readiness,
}
#[derive(Debug, Clone)]
pub enum TunnelForward {
    Local {
        /// None 表示系统分配；Some 表示优先使用此端口。
        local_port: Option<u16>,
        remote: ForwardTarget,
        /// 用户固定端口设为 false，冲突返回端口及可取得的占用进程信息。
        replace_on_conflict: bool,
    },
    Remote {
        remote_port: u16,
        local_port: u16,
    },
    Dynamic {
        local_port: u16,
    },
}
impl TunnelForward {
    fn listening_port(&self) -> Option<u16> {
        match self {
            Self::Local { local_port, .. } => *local_port,
            Self::Dynamic { local_port } => Some(*local_port),
            Self::Remote { .. } => None,
        }
    }
    fn binds_local(&self) -> bool {
        !matches!(self, Self::Remote { .. })
    }
    fn replace_on_conflict(&self) -> bool {
        matches!(
            self,
            Self::Local {
                replace_on_conflict: true,
                ..
            }
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelState {
    Connecting,
    Connected,
    Reconnecting,
    /// 等待有效密码或内置主连接完成验证码登录；不计重试次数，收到对应通知后立即恢复。
    WaitingPassword,
    Stopped,
    Error,
}
#[derive(Debug, Clone, Serialize)]
pub struct TunnelEvent {
    pub tunnel_id: String,
    pub server_id: String,
    pub state: TunnelState,
    pub local_ports: Vec<u16>,
    pub attempt: u32,
    pub next_retry_at_ms: Option<u64>,
    pub last_connected_at_ms: Option<u64>,
    pub timestamp_ms: u64,
    pub error: Option<AppError>,
}

type Events = Arc<Mutex<BTreeMap<String, TunnelEvent>>>;
enum Request {
    Start(Box<TunnelSpec>, SyncSender<Result<Vec<u16>, AppError>>),
    Stop(String, SyncSender<Result<(), AppError>>),
    Observe(String, SyncSender<()>, SyncSender<Result<(), AppError>>),
    NetworkRestored,
    ReconnectAll,
    ReconnectIfCurrent(String, Option<u64>),
    ProbeSucceededIfCurrent(String, Option<u64>),
    Status(String, SyncSender<Option<TunnelEvent>>),
    Shutdown,
}
/// 自动调度所有隧道；释放时关闭工作线程和所有 SSH 进程。已停止条目立即移除。
pub struct TunnelManager {
    /// 与隧道相同的 SSH 程序（同步连接经 SSH 访问远端文件时复用）
    program: SshProgram,
    tx: SyncSender<Request>,
    events: Events,
    worker: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}
#[derive(Clone)]
pub(crate) struct TunnelControl(SyncSender<Request>);
impl TunnelControl {
    /// id 的管理接口探活成功时清零重建退避；仅接受 connected_at 对应连接的结果，返回排队结果。
    pub(crate) fn probe_succeeded_if_current(
        &self,
        id: &str,
        connected_at: Option<u64>,
    ) -> Result<(), AppError> {
        self.0
            .try_send(Request::ProbeSucceededIfCurrent(id.into(), connected_at))
            .map_err(|_| AppError::new("tunnel.busy"))
    }

    /// 管理接口探活失败后请求重建 id；仅作用于 connected_at 对应的旧连接，返回排队结果。
    pub(crate) fn reconnect_if_current(
        &self,
        id: &str,
        connected_at: Option<u64>,
    ) -> Result<(), AppError> {
        self.0
            .try_send(Request::ReconnectIfCurrent(id.into(), connected_at))
            .map_err(|_| AppError::new("tunnel.busy"))
    }

    /// 订阅 id 的实际连接成功通知；容量为一，重复通知合并，不经过界面防抖。
    pub(crate) fn observe_connected(&self, id: &str) -> Result<Receiver<()>, AppError> {
        let (notify, receiver) = mpsc::sync_channel(1);
        let (reply, response) = mpsc::sync_channel(1);
        self.0
            .try_send(Request::Observe(id.into(), notify, reply))
            .map_err(|_| AppError::new("tunnel.busy"))?;
        response
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| AppError::new("tunnel.busy"))??;
        Ok(receiver)
    }

    pub(crate) fn status(&self, id: &str) -> Result<TunnelEvent, AppError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.0
            .try_send(Request::Status(id.into(), tx))
            .map_err(|_| AppError::new("tunnel.busy"))?;
        rx.recv_timeout(Duration::from_secs(2))
            .map_err(|_| AppError::new("tunnel.busy"))?
            .ok_or_else(|| AppError::new("tunnel.not_found"))
    }
    pub(crate) fn stop(&self, id: &str) -> Result<(), AppError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.0
            .try_send(Request::Stop(id.into(), tx))
            .map_err(|_| AppError::new("tunnel.busy"))?;
        rx.recv_timeout(Duration::from_secs(2))
            .map_err(|_| AppError::new("tunnel.busy"))?
    }
}
impl TunnelManager {
    pub(crate) fn program(&self) -> &SshProgram {
        &self.program
    }
    pub(crate) fn control(&self) -> TunnelControl {
        TunnelControl(self.tx.clone())
    }
    /// 使用 program 创建管理器并启动调度线程，返回管理器或线程创建错误。
    pub fn new(program: SshProgram) -> Result<Self, AppError> {
        let (tx, rx) = mpsc::sync_channel(64);
        let events = Arc::new(Mutex::new(BTreeMap::new()));
        let shared = events.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let kept = program.clone();
        let worker = thread::Builder::new()
            .name("ssh隧道".into())
            .spawn(move || schedule(program, rx, shared, worker_shutdown))
            .map_err(|_| io_error("tunnel.create_tunnel_thread", "创建隧道线程"))?;
        Ok(Self {
            program: kept,
            tx,
            events,
            worker: Some(worker),
            shutdown,
        })
    }
    /// 启动 spec 对应隧道并返回初始本地端口；实际连接状态和换端口结果经事件输出，重复 ID 返回错误。
    pub fn start(&self, spec: TunnelSpec) -> Result<Vec<u16>, AppError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .try_send(Request::Start(Box::new(spec), tx))
            .map_err(|_| AppError::new("tunnel.busy"))?;
        rx.recv_timeout(Duration::from_secs(2))
            .map_err(|_| AppError::new("tunnel.busy"))?
    }
    /// 停止 id 对应隧道并释放进程；不存在返回 tunnel.not_found。
    pub fn stop(&self, id: &str) -> Result<(), AppError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .try_send(Request::Stop(id.into(), tx))
            .map_err(|_| AppError::new("tunnel.busy"))?;
        rx.recv_timeout(Duration::from_secs(2))
            .map_err(|_| AppError::new("tunnel.busy"))?
    }
    /// 网络恢复时立即重试断开的隧道；存活连接只通知设备线程探活、重拨，探活失败才重建。
    /// 无参数，返回排队结果；仍受每服务器最多三个连接建立中的限制，认证或主机密钥错误不自动重试。
    pub fn network_restored(&self) -> Result<(), AppError> {
        self.tx
            .try_send(Request::NetworkRestored)
            .map_err(|_| io_error("tunnel.notify_network_recovery", "通知网络恢复"))
    }
    /// 无参数；主动结束已连接及正在建立的隧道，立即排队重连；仍遵守每服务器三个并发上限。
    /// 返回排队结果；队列满或管理器已关闭时返回 tunnel.busy，不重试认证或主机密钥错误。
    pub fn reconnect_all(&self) -> Result<(), AppError> {
        self.tx
            .try_send(Request::ReconnectAll)
            .map_err(|_| AppError::new("tunnel.busy"))
    }
    /// 无参数；取走每条隧道尚未消费的最新事件；状态稳定三秒才发布，初始连接和显式停止/终止错误立即发布。
    pub fn drain_events(&self) -> Result<Vec<TunnelEvent>, AppError> {
        let mut events = self
            .events
            .lock()
            .map_err(|_| io_error("tunnel.read_tunnel_events", "读取隧道事件"))?;
        Ok(std::mem::take(&mut *events).into_values().collect())
    }
}
impl Drop for TunnelManager {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.tx.try_send(Request::Shutdown) {
                Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => break,
                Err(mpsc::TrySendError::Full(_)) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(_) => break,
            }
        }
        // 队列持续拥塞时也必须退出，外部控制句柄可能仍持有发送端。
        self.shutdown.store(true, Ordering::Release);
        let (replacement, _) = mpsc::sync_channel(1);
        let old = std::mem::replace(&mut self.tx, replacement);
        drop(old);
        if let Some(w) = self.worker.take() {
            crate::process::join_bounded(w, Duration::from_secs(2));
        }
    }
}
/// 无参数；绑定 IPv4 回环地址的随机空闲端口后释放，最多尝试三次，返回端口或 tunnel.port_unavailable。
pub fn allocate_port() -> Result<u16, AppError> {
    for _ in 0..3 {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", 0))
            && let Ok(addr) = listener.local_addr()
        {
            return Ok(addr.port());
        }
    }
    Err(AppError::new("tunnel.port_unavailable"))
}
fn store_event(events: &Events, event: TunnelEvent) {
    if let Ok(mut events) = events.lock() {
        // 无消费者时仍有界：每条隧道只保留最新事件，最多 1024 条。
        if events.len() >= 1024
            && !events.contains_key(&event.tunnel_id)
            && let Some(oldest) = events
                .values()
                .min_by_key(|e| e.timestamp_ms)
                .map(|e| e.tunnel_id.clone())
        {
            events.remove(&oldest);
        }
        events.insert(event.tunnel_id.clone(), event);
    }
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
pub(crate) fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs([1, 2, 5, 10, 30, 60][attempt.saturating_sub(1).min(5) as usize])
}
fn port_error(port: u16) -> AppError {
    let error = AppError::new("tunnel.port_in_use").with("port", port);
    #[cfg(windows)]
    {
        owner::enrich(error, port)
    }
    #[cfg(not(windows))]
    {
        error
    }
}
fn bind_failure(text: &str) -> bool {
    [
        "Address already in use",
        "cannot listen to port",
        "Could not request local forwarding",
        "bind: Permission denied",
    ]
    .iter()
    .any(|p| text.contains(p))
}
fn unique_port(
    used: &[u16],
    mut allocate: impl FnMut() -> Result<u16, AppError>,
) -> Result<u16, AppError> {
    for _ in 0..16 {
        let port = allocate()?;
        if !used.contains(&port) {
            return Ok(port);
        }
    }
    Err(AppError::new("tunnel.port_unavailable"))
}
fn failed_ports(text: &str) -> Vec<u16> {
    text.lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once("bind [127.0.0.1]:")?;
            let (port, reason) = rest.split_once(':')?;
            if !reason.contains("Address already in use") {
                return None;
            }
            port.parse().ok()
        })
        .collect()
}
struct Connection {
    process: SshProcess,
    stderr: Reader,
    started: Instant,
    ready: bool,
    exited: Option<Instant>,
    authenticated_since: Option<Instant>,
    prohibited_logged: bool,
}
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.process.terminate();
        self.stderr.join();
    }
}
struct Entry {
    spec: TunnelSpec,
    ports: Vec<u16>,
    connection: Option<Connection>,
    builtin_connection: Option<builtin::Attempt>,
    builtin_watch: Option<builtin::Watch>,
    due: Instant,
    event: TunnelEvent,
    pending: Option<(Instant, TunnelEvent)>,
    connected_observer: Option<SyncSender<()>>,
    binding_retries: u8,
    // 与 SSH 连接失败次数独立；SSH 监听就绪不能证明远端同步服务已恢复。
    probe_rebuild_attempt: u32,
    fatal: bool,
    password_wait: Option<crate::security::vault::VaultSubscription>,
}
impl Entry {
    fn new(spec: TunnelSpec) -> Result<Self, AppError> {
        Self::new_with_allocator(spec, allocate_port)
    }
    fn new_with_allocator(
        spec: TunnelSpec,
        mut allocate: impl FnMut() -> Result<u16, AppError>,
    ) -> Result<Self, AppError> {
        if spec.id.is_empty() || spec.server_id.is_empty() || spec.forwards.is_empty() {
            return Err(AppError::new("tunnel.invalid_spec"));
        }
        if spec.readiness == Readiness::ProbeLocalPorts
            && spec
                .forwards
                .iter()
                .any(|f| !matches!(f, TunnelForward::Local { .. }))
        {
            return Err(AppError::new("tunnel.invalid_spec"));
        }
        let mut ports = Vec::new();
        // 先避开所有固定端口，避免先分配的随机端口撞上后面的固定端口。
        let mut reserved: Vec<_> = spec
            .forwards
            .iter()
            .filter_map(TunnelForward::listening_port)
            .collect();
        for forward in spec.forwards.iter().filter(|f| f.binds_local()) {
            let port = match forward.listening_port() {
                Some(0) => return Err(AppError::new("tunnel.invalid_port")),
                Some(p) => p,
                None => unique_port(&reserved, &mut allocate)?,
            };
            if ports.contains(&port) {
                return Err(AppError::new("tunnel.duplicate_port"));
            }
            ports.push(port);
            reserved.push(port);
        }
        let event = TunnelEvent {
            tunnel_id: spec.id.clone(),
            server_id: spec.server_id.clone(),
            state: TunnelState::Connecting,
            local_ports: ports.clone(),
            attempt: 0,
            next_retry_at_ms: None,
            last_connected_at_ms: None,
            timestamp_ms: now_ms(),
            error: None,
        };
        let builtin_watch = spec.builtin.clone().map(builtin::Watch::new).transpose()?;
        let entry = Self {
            builtin_connection: None,
            builtin_watch,
            spec,
            ports,
            connection: None,
            due: Instant::now(),
            event,
            pending: None,
            connected_observer: None,
            binding_retries: 0,
            probe_rebuild_attempt: 0,
            fatal: false,
            password_wait: None,
        };
        if entry.spec.builtin.is_some() {
            builtin::validate(&entry.spec)?;
        } else {
            entry.command()?;
        }
        Ok(entry)
    }
    fn command(&self) -> Result<crate::ssh::args::SshCommand, AppError> {
        let mut ports = self.ports.iter();
        let forwards = self
            .spec
            .forwards
            .iter()
            .map(|f| {
                let local_port = |ports: &mut std::slice::Iter<'_, u16>| {
                    ports
                        .next()
                        .copied()
                        .ok_or_else(|| AppError::new("tunnel.invalid_spec"))
                };
                Ok(match f {
                    TunnelForward::Local { remote, .. } => Forward::Local {
                        local_port: local_port(&mut ports)?,
                        remote: remote.clone(),
                    },
                    TunnelForward::Remote {
                        remote_port,
                        local_port,
                    } => Forward::Remote {
                        remote_port: *remote_port,
                        local_port: *local_port,
                    },
                    TunnelForward::Dynamic { .. } => Forward::Dynamic {
                        local_port: local_port(&mut ports)?,
                    },
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        build_args(
            &self.spec.target,
            &Purpose::Tunnel {
                forwards,
                readiness: self.spec.readiness,
            },
            &self.spec.options,
        )
    }
    fn publish(&self, events: &Events) {
        store_event(events, self.event.clone());
    }
    fn change(
        &mut self,
        state: TunnelState,
        error: Option<AppError>,
        immediate: bool,
        events: &Events,
    ) {
        if state == TunnelState::Connected {
            self.notify_connected();
        }
        self.event.state = state;
        self.event.error = error;
        self.event.local_ports = self.ports.clone();
        self.event.timestamp_ms = now_ms();
        if immediate {
            self.pending = None;
            self.publish(events);
        } else {
            let since = self
                .pending
                .as_ref()
                .filter(|(_, e)| e.state == state)
                .map_or_else(Instant::now, |(time, _)| *time);
            self.pending = Some((since, self.event.clone()));
        }
    }
    fn notify_connected(&self) {
        if let Some(observer) = &self.connected_observer {
            let _ = observer.try_send(());
        }
    }
    fn flush(&mut self, events: &Events) {
        self.flush_at(events, Instant::now());
    }
    fn flush_at(&mut self, events: &Events, now: Instant) {
        if self.pending.as_ref().is_some_and(|(since, _)| {
            now.saturating_duration_since(*since) >= Duration::from_secs(3)
        }) && let Some((_, mut event)) = self.pending.take()
        {
            event.timestamp_ms = now_ms();
            store_event(events, event);
        }
    }
    fn has_connection(&self) -> bool {
        self.connection.is_some() || self.builtin_connection.is_some()
    }
    fn waiting_password(&self) -> bool {
        self.password_wait.is_some() || self.builtin_watch.as_ref().is_some_and(|w| w.waiting)
    }
    fn fail(&mut self, mut error: AppError, events: &Events) {
        self.builtin_connection = None;
        if error.code == "tunnel.remote_forward_failed" && !error.params.contains_key("port") {
            let ports = self
                .spec
                .forwards
                .iter()
                .filter_map(|forward| match forward {
                    TunnelForward::Remote { remote_port, .. } => Some(remote_port.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            error = error.with("port", ports.join(", "));
        }
        if self.spec.builtin.is_some() && error.code == "tunnel.port_in_use" {
            let reported = error
                .params
                .get("port")
                .map(|port| format!("bind [127.0.0.1]:{port}: Address already in use"));
            if let Some(reported) = reported {
                match self.replace_busy_ports(Some(&reported)) {
                    Ok(()) => {
                        self.due = Instant::now();
                        return;
                    }
                    Err(binding_error) => error = binding_error,
                }
            }
        }
        if let Some(watch) = &mut self.builtin_watch {
            if watch.wait_for(&error) {
                self.event.next_retry_at_ms = None;
                self.change(TunnelState::WaitingPassword, Some(error), true, events);
                return;
            }
            watch.waiting = false;
            if error.code == "ssh.auth_cancelled" && self.builtin_cancelled() {
                self.fatal = true;
                self.event.next_retry_at_ms = None;
                self.change(TunnelState::Error, Some(error), true, events);
                return;
            }
        }
        let used_password = self
            .connection
            .as_ref()
            .and_then(|c| c.process.askpass.as_ref())
            .is_some_and(|guard| guard.status().used_generation.is_some());
        let error = self
            .connection
            .as_ref()
            .map_or_else(|| error.clone(), |c| c.process.finish_error(error.clone()));
        self.connection = None;
        if (error.code == "ssh.password_needed" || error.code == "ssh.auth_failed" && used_password)
            && let Some(ask) = &self.spec.options.askpass
        {
            self.password_wait = Some(ask.registration.subscribe());
            self.event.next_retry_at_ms = None;
            self.change(TunnelState::WaitingPassword, Some(error), true, events);
            return;
        }
        self.event.attempt = self.event.attempt.saturating_add(1);
        if matches!(
            error.code.as_str(),
            "ssh.auth_failed"
                | "ssh.too_many_auth_attempts"
                | "ssh.host_key_unknown"
                | "ssh.host_key_changed"
                | "ssh.forwarding_prohibited"
                | "ssh.streamlocal_open_failed"
                | "tunnel.port_reserved"
                | "tunnel.port_in_use"
                | "tunnel.port_unavailable"
                | "ssh.invalid_argument"
                | "ssh.two_factor_required"
        ) {
            self.fatal = true;
            self.event.next_retry_at_ms = None;
            self.change(TunnelState::Error, Some(error), true, events);
        } else {
            let delay = retry_delay(self.event.attempt);
            self.due = Instant::now() + delay;
            self.event.next_retry_at_ms = Some(now_ms().saturating_add(delay.as_millis() as u64));
            self.change(TunnelState::Reconnecting, Some(error), false, events);
        }
    }
    fn replace_busy_ports(&mut self, failed: Option<&str>) -> Result<(), AppError> {
        for port in &self.ports {
            if let Err(error) = TcpListener::bind(("127.0.0.1", *port))
                && error.kind() == std::io::ErrorKind::PermissionDenied
            {
                return Err(AppError::new("tunnel.port_reserved").with("port", port));
            }
        }
        self.replace_busy_ports_with(
            failed,
            |port| TcpListener::bind(("127.0.0.1", port)).is_ok(),
            allocate_port,
        )
    }
    fn replace_busy_ports_with(
        &mut self,
        failed: Option<&str>,
        mut available: impl FnMut(u16) -> bool,
        mut allocate: impl FnMut() -> Result<u16, AppError>,
    ) -> Result<(), AppError> {
        let reported = failed.map(failed_ports).unwrap_or_default();
        let mut changed = false;
        for (i, forward) in self
            .spec
            .forwards
            .iter()
            .filter(|f| f.binds_local())
            .enumerate()
        {
            if !reported.is_empty() && !reported.contains(&self.ports[i]) {
                continue;
            }
            if reported.contains(&self.ports[i]) && !forward.replace_on_conflict() {
                return Err(port_error(self.ports[i]));
            }
            if !available(self.ports[i]) {
                if !forward.replace_on_conflict() || self.binding_retries >= 3 {
                    return Err(port_error(self.ports[i]));
                }
                self.ports[i] = unique_port(&self.ports, &mut allocate)?;
                changed = true;
            }
        }
        // 竞争者可能已退出；可绑定的端口原样重试，但仍计数，避免无限循环。
        if changed || failed.is_some() {
            if self.binding_retries >= 3 {
                return Err(AppError::new("tunnel.port_unavailable"));
            }
            self.binding_retries += 1;
        }
        Ok(())
    }
    fn reconnect(&mut self, events: &Events) {
        self.reconnect_after(Duration::ZERO, events);
    }
    fn reconnect_after(&mut self, delay: Duration, events: &Events) {
        if self.waiting_password() {
            return;
        }
        self.builtin_connection = None;
        self.connection = None;
        self.due = Instant::now() + delay;
        self.event.next_retry_at_ms = Some(now_ms().saturating_add(delay.as_millis() as u64));
        self.change(TunnelState::Reconnecting, None, true, events);
    }
    fn network_restored(&mut self, events: &Events) {
        if self.fatal || self.waiting_password() {
            return;
        }
        if !self.has_connection() {
            self.reconnect(events);
        } else if self.event.state == TunnelState::Connected {
            self.notify_connected();
        }
    }
    fn reconnect_if_current(&mut self, connected_at: Option<u64>, events: &Events) {
        if !self.fatal
            && self.event.state == TunnelState::Connected
            && self.event.last_connected_at_ms == connected_at
        {
            // 连续探活失败按 1、2、5、10、30、60 秒退避，之后维持 60 秒；
            // 每次连接还会等待五秒探活宽限，避免服务停机时反复 SSH 登录。
            self.probe_rebuild_attempt = self.probe_rebuild_attempt.saturating_add(1);
            self.reconnect_after(retry_delay(self.probe_rebuild_attempt), events);
        }
    }
    fn probe_succeeded_if_current(&mut self, connected_at: Option<u64>) {
        if self.event.state == TunnelState::Connected
            && self.event.last_connected_at_ms == connected_at
        {
            self.probe_rebuild_attempt = 0;
        }
    }
    fn password_available(&mut self, events: &Events) {
        if let Some(watch) = &mut self.builtin_watch
            && watch.waiting
            && watch.available()
        {
            watch.waiting = false;
            self.reconnect(events);
        }
        if let Some(wait) = &self.password_wait {
            let _ = wait.wait(Duration::ZERO);
            let available = self
                .spec
                .options
                .askpass
                .as_ref()
                .is_some_and(|ask| ask.registration.has_password());
            if available {
                self.password_wait = None;
                self.reconnect(events);
            }
        }
    }
    fn connect(&mut self, program: &SshProgram) -> Result<(), AppError> {
        if let Some(context) = self.spec.builtin.clone() {
            if self.builtin_cancelled() {
                return Err(AppError::new("ssh.auth_cancelled"));
            }
            self.builtin_connection =
                Some(builtin::Attempt::start(context, &self.spec, &self.ports)?);
            self.event.next_retry_at_ms = None;
            return Ok(());
        }
        let command = self.command()?;
        // 在端口探测和启动 SSH 前先检查密码；真正 spawn 时仍使用独立登记再次检查。
        if let Some(guard) = &command.askpass {
            guard.prepare()?;
        }
        self.replace_busy_ports(None)?;
        let mut process = program.spawn(&command)?;
        drop(process.child.stdin.take());
        drop(process.child.stdout.take());
        let input = process
            .child
            .stderr
            .take()
            .ok_or_else(|| io_error("tunnel.get_tunnel_output", "获取隧道输出"))?;
        let stderr = if self.spec.readiness == Readiness::AuthMarker {
            // ProxyJump 的日志级别只通过 -v 继承，不传递 LogLevel=VERBOSE；日志地址可与别名不同。
            Reader::tail_authenticated(input, 16 * 1024)?
        } else {
            Reader::tail(input, 16 * 1024)?
        };
        self.connection = Some(Connection {
            process,
            stderr,
            started: Instant::now(),
            ready: false,
            exited: None,
            authenticated_since: None,
            prohibited_logged: false,
        });
        self.event.next_retry_at_ms = None;
        Ok(())
    }
    fn poll(&mut self, events: &Events) -> Result<(), AppError> {
        self.poll_at(events, Instant::now())
    }
    fn poll_at(&mut self, events: &Events, now: Instant) -> Result<(), AppError> {
        if self.spec.builtin.is_some() && !self.fatal {
            return self.poll_builtin(events);
        }
        let Some(connection) = self.connection.as_mut() else {
            return Ok(());
        };
        let data = connection
            .stderr
            .state
            .lock()
            .map_err(|_| io_error("tunnel.read_tunnel_output", "读取隧道输出"))?;
        let text = String::from_utf8_lossy(&data.bytes);
        if data.exceeded {
            return Err(AppError::new("remote.output_too_large"));
        }
        if data.failed {
            return Err(io_error("tunnel.read_tunnel_output", "读取隧道输出"));
        }
        if data.prohibited {
            // 同步隧道整条依赖转发：被禁止就是不可用，始终致命；端口转发就绪后只是某个目标被拒，记日志继续
            if !connection.ready || self.spec.readiness == Readiness::ProbeLocalPorts {
                return Err(AppError::new("ssh.forwarding_prohibited"));
            }
            if !connection.prohibited_logged {
                eprintln!("服务器拒绝了一个转发目标，已就绪的隧道继续运行");
                connection.prohibited_logged = true;
            }
        }
        if let Some(status) = connection
            .process
            .try_wait()
            .map_err(|_| io_error("tunnel.query_tunnel", "查询隧道"))?
        {
            if !data.done
                && now.saturating_duration_since(*connection.exited.get_or_insert(now))
                    < Duration::from_millis(500)
            {
                return Ok(());
            }
            let authentication =
                connection
                    .process
                    .finish_error(classify_error(status.code(), &text, &[]));
            if matches!(
                authentication.code.as_str(),
                "ssh.password_needed"
                    | "ssh.two_factor_required"
                    | "ssh.auth_failed"
                    | "ssh.too_many_auth_attempts"
            ) {
                return Err(authentication);
            }
            if text.contains("bind: Permission denied")
                || text.contains(": Permission denied") && text.contains("bind [")
            {
                return Err(classify_error(status.code(), &text, &[]));
            }
            if bind_failure(&text) {
                let text = text.into_owned();
                drop(data);
                self.connection = None;
                self.replace_busy_ports(Some(&text))?;
                self.due = Instant::now();
                return Ok(());
            }
            // 就绪后单个目标被拒绝不代表整条隧道不可用，旧提示不能掩盖后续断网错误。
            let text = if connection.ready {
                text.lines()
                    .filter(|line| !line.contains("administratively prohibited"))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                text.into_owned()
            };
            return Err(classify_error(status.code(), &text, &[]));
        }
        if !connection.ready {
            if now.saturating_duration_since(connection.started) >= Duration::from_secs(15) {
                return Err(AppError::new("ssh.unreachable")
                    .with_reason("tunnel.connection_setup_timed_out", "连接建立超时")
                    .with_text(
                        "stderr_tail",
                        "tunnel.connection_setup_timed_out",
                        "连接建立超时",
                    ));
            }
            // 预先验证端口可绑定，并在探测成功后再次确认 SSH 未退出，避免把占用者当作隧道。
            let listening = match self.spec.readiness {
                Readiness::AuthMarker => {
                    data.authenticated
                        && now.saturating_duration_since(
                            *connection.authenticated_since.get_or_insert(now),
                        ) >= Duration::from_millis(500)
                }
                Readiness::ProbeLocalPorts => {
                    now.saturating_duration_since(connection.started) >= Duration::from_millis(100)
                        && self.ports.iter().all(|port| {
                            TcpStream::connect_timeout(
                                &SocketAddr::from(([127, 0, 0, 1], *port)),
                                Duration::from_millis(20),
                            )
                            .is_ok()
                        })
                }
            };
            if listening
                && connection
                    .process
                    .try_wait()
                    .map_err(|_| io_error("tunnel.confirm_tunnel", "确认隧道"))?
                    .is_none()
            {
                connection.ready = true;
                self.binding_retries = 0;
                // 同一毫秒内重建及系统时钟回拨时，仍可识别过期的探活结果。
                self.event.last_connected_at_ms = Some(
                    now_ms().max(
                        self.event
                            .last_connected_at_ms
                            .map_or(0, |last| last.saturating_add(1)),
                    ),
                );
                drop(data);
                self.change(TunnelState::Connected, None, false, events);
            }
        } else if now.saturating_duration_since(connection.started) >= Duration::from_secs(3) {
            self.event.attempt = 0;
        }
        Ok(())
    }
}
fn schedule(program: SshProgram, rx: Receiver<Request>, events: Events, shutdown: Arc<AtomicBool>) {
    let mut entries: HashMap<String, Entry> = HashMap::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    while !shutdown.load(Ordering::Acquire) {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Request::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Request::Status(id, reply)) => {
                let _ = reply.send(entries.get(&id).map(|entry| {
                    let mut event = entry.event.clone();
                    event.local_ports = entry.ports.clone();
                    event
                }));
            }
            Ok(Request::Start(spec, reply)) => {
                let result = if entries.contains_key(&spec.id) {
                    Err(AppError::new("tunnel.already_exists"))
                } else if entries.len() >= 1024 {
                    Err(AppError::new("tunnel.limit_reached"))
                } else {
                    Entry::new(*spec).map(|entry| {
                        let ports = entry.ports.clone();
                        entry.publish(&events);
                        queue.push_back(entry.spec.id.clone());
                        entries.insert(entry.spec.id.clone(), entry);
                        ports
                    })
                };
                let _ = reply.send(result);
            }
            Ok(Request::Stop(id, reply)) => {
                let result = if let Some(mut entry) = entries.remove(&id) {
                    entry.connection = None;
                    entry.builtin_connection = None;
                    entry.event.next_retry_at_ms = None;
                    entry.change(TunnelState::Stopped, None, true, &events);
                    queue.retain(|key| key != &id);
                    Ok(())
                } else {
                    Err(AppError::new("tunnel.not_found"))
                };
                let _ = reply.send(result);
            }
            Ok(Request::Observe(id, observer, reply)) => {
                let result = entries.get_mut(&id).map_or_else(
                    || Err(AppError::new("tunnel.not_found")),
                    |entry| {
                        entry.connected_observer = Some(observer);
                        if entry.event.state == TunnelState::Connected {
                            entry.notify_connected();
                        }
                        Ok(())
                    },
                );
                let _ = reply.send(result);
            }
            Ok(Request::ReconnectAll) => {
                for entry in entries.values_mut().filter(|e| !e.fatal) {
                    entry.reconnect(&events);
                }
            }
            Ok(Request::NetworkRestored) => {
                for entry in entries.values_mut() {
                    if let Err(error) = entry.poll(&events) {
                        entry.fail(error, &events);
                    }
                    entry.network_restored(&events);
                }
            }
            Ok(Request::ReconnectIfCurrent(id, connected_at)) => {
                if let Some(entry) = entries.get_mut(&id) {
                    entry.reconnect_if_current(connected_at, &events);
                }
            }
            Ok(Request::ProbeSucceededIfCurrent(id, connected_at)) => {
                if let Some(entry) = entries.get_mut(&id) {
                    entry.probe_succeeded_if_current(connected_at);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        for entry in entries.values_mut() {
            entry.password_available(&events);
            if let Err(e) = entry.poll(&events) {
                entry.fail(e, &events);
            }
            entry.flush(&events);
            if !entry.has_connection()
                && !entry.fatal
                && !entry.waiting_password()
                && !queue.contains(&entry.spec.id)
            {
                queue.push_back(entry.spec.id.clone());
            }
        }
        let mut active: HashMap<String, usize> = HashMap::new();
        for entry in entries.values().filter(|e| {
            e.connection.as_ref().is_some_and(|c| !c.ready)
                || e.builtin_connection.as_ref().is_some_and(|c| !c.ready)
        }) {
            *active.entry(entry.spec.server_id.clone()).or_default() += 1;
        }
        for id in ready_batch(&mut queue, &entries, &mut active) {
            if let Some(entry) = entries.get_mut(&id) {
                match entry.connect(&program) {
                    Err(e) => entry.fail(e, &events),
                    Ok(()) if entry.event.attempt > 0 => entry.change(
                        TunnelState::Reconnecting,
                        entry.event.error.clone(),
                        false,
                        &events,
                    ),
                    Ok(()) => {}
                }
            }
        }
    }
}

fn ready_batch(
    queue: &mut VecDeque<String>,
    entries: &HashMap<String, Entry>,
    active: &mut HashMap<String, usize>,
) -> Vec<String> {
    let mut ready = Vec::new();
    for _ in 0..queue.len() {
        let Some(id) = queue.pop_front() else {
            break;
        };
        let Some(entry) = entries.get(&id) else {
            continue;
        };
        if entry.fatal || entry.has_connection() || entry.waiting_password() {
            continue;
        }
        let count = active.entry(entry.spec.server_id.clone()).or_default();
        if *count >= 3 || Instant::now() < entry.due {
            queue.push_back(id);
            continue;
        }
        *count += 1;
        ready.push(id);
    }
    ready
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn n2_n3_退避封顶与事件格式() {
        assert_eq!(
            (1..=8)
                .map(|n| retry_delay(n).as_secs())
                .collect::<Vec<_>>(),
            vec![1, 2, 5, 10, 30, 60, 60, 60]
        );
        assert_eq!(
            serde_json::to_string(&TunnelState::Reconnecting).unwrap(),
            "\"reconnecting\""
        );
    }
    #[test]
    fn p1_p2_端口错误及退出提示() {
        let port = 12345;
        let error = port_error(port);
        assert_eq!(error.params["port"], port.to_string());
        assert!(bind_failure("bind [127.0.0.1]:123: Address already in use"));
        assert!(bind_failure("Could not request local forwarding."));
    }
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;
    fn spec(id: &str, server: &str) -> TunnelSpec {
        TunnelSpec {
            builtin: None,
            id: id.into(),
            server_id: server.into(),
            target: SshTarget::Manual {
                host: "localhost".into(),
                port: 22,
                user: None,
                identity_file: None,
                jump: None,
            },
            options: CommonOpts::default(),
            readiness: Readiness::ProbeLocalPorts,
            forwards: vec![TunnelForward::Local {
                local_port: Some(12345),
                remote: ForwardTarget::Tcp { port: 22 },
                replace_on_conflict: true,
            }],
        }
    }
    #[test]
    fn s2_n6_按服务器排队每次至多三个() {
        let mut entries = HashMap::new();
        let mut queue = VecDeque::new();
        for n in 0..12 {
            let id = n.to_string();
            entries.insert(
                id.clone(),
                Entry::new(spec(&id, if n < 10 { "甲" } else { "乙" })).unwrap(),
            );
            queue.push_back(id);
        }
        let ready = ready_batch(&mut queue, &entries, &mut HashMap::new());
        assert_eq!(ready, vec!["0", "1", "2", "10", "11"]);
        let ready = ready_batch(&mut queue, &entries, &mut HashMap::from([("甲".into(), 2)]));
        assert_eq!(ready, vec!["3"]);
        let ready = ready_batch(&mut queue, &entries, &mut HashMap::new());
        assert_eq!(ready, vec!["4", "5", "6"]);
    }
    #[test]
    fn n2_n3_p8_s3_防抖保留端口和认证错误停止() {
        let mut entry = Entry::new(spec("隧道", "甲")).unwrap();
        let events = Arc::new(Mutex::new(BTreeMap::new()));
        let command = entry.command().unwrap();
        assert!(command.args.contains(&"ServerAliveInterval=15".into()));
        assert!(command.args.contains(&"ServerAliveCountMax=3".into()));
        entry.change(TunnelState::Connected, None, false, &events);
        entry.flush(&events);
        assert!(events.lock().unwrap().is_empty());
        let now = entry.pending.as_ref().unwrap().0 + Duration::from_secs(4);
        entry.flush_at(&events, now);
        assert_eq!(events.lock().unwrap()["隧道"].state, TunnelState::Connected);
        events.lock().unwrap().clear();
        for _ in 0..8 {
            entry.fail(AppError::new("ssh.unreachable"), &events);
        }
        assert_eq!(entry.ports, vec![12345]);
        assert!(!entry.fatal);
        assert!(entry.due > Instant::now() + Duration::from_secs(59));
        let mut queue = VecDeque::from(["隧道".into()]);
        let mut entries = HashMap::from([("隧道".into(), entry)]);
        assert!(ready_batch(&mut queue, &entries, &mut HashMap::new()).is_empty());
        entries.get_mut("隧道").unwrap().due = Instant::now();
        assert_eq!(
            ready_batch(&mut queue, &entries, &mut HashMap::new()),
            vec!["隧道"]
        );
        let entry = entries.get_mut("隧道").unwrap();
        entry.fail(AppError::new("ssh.auth_failed"), &events);
        assert!(entry.fatal);
        assert_eq!(events.lock().unwrap()["隧道"].state, TunnelState::Error);
    }
    #[test]
    fn 修订8_主动重连保留端口并立即入队() {
        let mut entry = Entry::new(spec("隧道", "甲")).unwrap();
        entry.event.state = TunnelState::Connected;
        #[cfg(unix)]
        let pid = {
            let mut process = SshProgram {
                path: "/usr/bin/sleep".into(),
            }
            .spawn(&crate::ssh::args::SshCommand {
                args: vec!["30".into()],
                env: vec![],
                config_text: None,
                hooked: false,
                builtin: None,
                askpass: None,
            })
            .unwrap();
            let pid = process.child.id();
            drop(process.child.stdin.take());
            drop(process.child.stdout.take());
            let stderr = Reader::tail(process.child.stderr.take().unwrap(), 16 * 1024).unwrap();
            entry.connection = Some(Connection {
                process,
                stderr,
                started: Instant::now(),
                ready: true,
                exited: None,
                authenticated_since: None,
                prohibited_logged: false,
            });
            pid
        };
        entry.due = Instant::now() + Duration::from_secs(60);
        let events = Arc::new(Mutex::new(BTreeMap::new()));
        entry.reconnect(&events);
        #[cfg(unix)]
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert!(entry.connection.is_none());
        assert_eq!(entry.ports, vec![12345]);
        assert!(entry.due <= Instant::now());
        assert_eq!(
            events.lock().unwrap()["隧道"].state,
            TunnelState::Reconnecting
        );
    }
    #[test]
    fn 修订10_精确端口冲突与重复分配重试() {
        let mut entry = Entry::new(spec("隧道", "甲")).unwrap();
        entry.spec.forwards.push(TunnelForward::Local {
            local_port: Some(12346),
            remote: ForwardTarget::Tcp { port: 22 },
            replace_on_conflict: true,
        });
        entry.ports.push(12346);
        let message = "bind [127.0.0.1]:12345: Address already in use";
        assert_eq!(failed_ports(message), vec![12345]);
        entry
            .replace_busy_ports_with(Some(message), |_| false, || Ok(23456))
            .unwrap();
        assert_eq!(entry.ports, vec![23456, 12346]);
        let message = "bind [127.0.0.1]:23456: Address already in use";
        entry
            .replace_busy_ports_with(Some(message), |_| true, || panic!("空闲端口不应重分配"))
            .unwrap();
        assert_eq!(entry.ports, vec![23456, 12346]);
        assert_eq!(entry.binding_retries, 2);
        entry
            .replace_busy_ports_with(Some(message), |_| true, || Ok(1))
            .unwrap();
        assert!(
            entry
                .replace_busy_ports_with(Some(message), |_| true, || Ok(1))
                .is_err()
        );
        let mut allocated = spec("自动分配", "甲");
        if let TunnelForward::Local { local_port, .. } = &mut allocated.forwards[0] {
            *local_port = None;
        }
        allocated.forwards.push(TunnelForward::Local {
            local_port: Some(12345),
            remote: ForwardTarget::Tcp { port: 22 },
            replace_on_conflict: true,
        });
        allocated.forwards.push(allocated.forwards[0].clone());
        let mut candidates = [12345, 23456, 23456, 34567].into_iter();
        let allocated =
            Entry::new_with_allocator(allocated, || Ok(candidates.next().unwrap())).unwrap();
        assert_eq!(allocated.ports, vec![23456, 12345, 34567]);
        let mut ports = [12345, 12345, 23456].into_iter();
        assert_eq!(
            unique_port(&[12345], || Ok(ports.next().unwrap())).unwrap(),
            23456
        );
        assert!(unique_port(&[12345], || Ok(12345)).is_err());
    }
    #[test]
    fn 队列满时重试关闭且控制句柄不阻止释放() {
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Request::NetworkRestored).unwrap();
        let control = TunnelControl(tx.clone());
        let received = Arc::new(AtomicBool::new(false));
        let worker_received = received.clone();
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            assert!(matches!(rx.recv().unwrap(), Request::NetworkRestored));
            assert!(matches!(
                rx.recv_timeout(Duration::from_secs(1)).unwrap(),
                Request::Shutdown
            ));
            worker_received.store(true, Ordering::Release);
        });
        let manager = TunnelManager {
            program: SshProgram {
                path: "/bin/false".into(),
            },
            tx,
            events: Arc::new(Mutex::new(BTreeMap::new())),
            worker: Some(worker),
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        drop(manager);
        assert!(received.load(Ordering::Acquire));
        assert!(control.status("已释放").is_err());
    }
    #[test]
    fn 关闭重试超时仍通知工作线程退出() {
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Request::NetworkRestored).unwrap();
        let control = TunnelControl(tx.clone());
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();
        let worker = thread::spawn(move || {
            while !flag.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
            drop(rx);
        });
        let manager = TunnelManager {
            program: SshProgram {
                path: "/bin/false".into(),
            },
            tx,
            events: Arc::new(Mutex::new(BTreeMap::new())),
            worker: Some(worker),
            shutdown,
        };
        drop(manager);
        assert!(control.status("已释放").is_err());
    }
    #[test]
    fn 修订6_管理器请求超时返回忙() {
        let (tx, _rx) = mpsc::sync_channel(64);
        let manager = TunnelManager {
            program: SshProgram {
                path: "/bin/false".into(),
            },
            tx,
            events: Arc::new(Mutex::new(BTreeMap::new())),
            worker: None,
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        let start = Instant::now();
        assert_eq!(
            manager.start(spec("测试", "甲")).unwrap_err().code,
            "tunnel.busy"
        );
        assert_eq!(manager.stop("测试").unwrap_err().code, "tunnel.busy");
        // 两次请求各最多等 2 秒；这里只验证不会卡住，给繁忙的 CI 机器留足余量
        assert!(start.elapsed() < Duration::from_secs(10));
    }
    #[test]
    fn 管理器公共接口和事件缓存上限() {
        let manager = TunnelManager::new(SshProgram {
            path: "/没有此程序".into(),
        })
        .unwrap();
        assert!(manager.drain_events().unwrap().is_empty());
        manager.network_restored().unwrap();
        manager.reconnect_all().unwrap();
        let mut invalid = spec("", "甲");
        invalid.forwards.clear();
        assert_eq!(
            manager.start(invalid).unwrap_err().code,
            "tunnel.invalid_spec"
        );
        assert_eq!(manager.stop("不存在").unwrap_err().code, "tunnel.not_found");
        assert_eq!(
            manager
                .control()
                .observe_connected("不存在")
                .unwrap_err()
                .code,
            "tunnel.not_found"
        );
        let event = Entry::new(spec("原始", "甲")).unwrap().event;
        for n in 0..1100 {
            let mut event = event.clone();
            event.tunnel_id = n.to_string();
            store_event(&manager.events, event);
        }
        assert_eq!(manager.drain_events().unwrap().len(), 1024);
    }

    #[test]
    fn 连接成功立即通知设备且重复通知合并() {
        let mut entry = Entry::new_with_allocator(spec("同步", "甲"), || Ok(12345)).unwrap();
        let events = Arc::new(Mutex::new(BTreeMap::new()));
        let (sender, receiver) = mpsc::sync_channel(1);
        entry.connected_observer = Some(sender);
        entry.change(TunnelState::Connected, None, false, &events);
        assert!(receiver.try_recv().is_ok());
        assert!(
            events.lock().unwrap().is_empty(),
            "设备通知不等待界面三秒防抖"
        );
        for _ in 0..1000 {
            entry.notify_connected();
        }
        assert!(receiver.try_recv().is_ok());
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        entry.reconnect(&events);
        assert!(
            receiver.try_recv().is_err(),
            "重连中不能发送管理凭据或触发设备重拨"
        );
    }

    #[cfg(unix)]
    #[test]
    fn 网络恢复保留存活连接_只唤醒设备线程() {
        let mut entry = Entry::new(spec("同步", "甲")).unwrap();
        let mut process = SshProgram {
            path: "/usr/bin/sleep".into(),
        }
        .spawn(&crate::ssh::args::SshCommand {
            args: vec!["30".into()],
            env: vec![],
            config_text: None,
            hooked: false,
            builtin: None,
            askpass: None,
        })
        .unwrap();
        let pid = process.child.id();
        drop(process.child.stdin.take());
        drop(process.child.stdout.take());
        let stderr = Reader::tail(process.child.stderr.take().unwrap(), 1024).unwrap();
        entry.connection = Some(Connection {
            process,
            stderr,
            started: Instant::now(),
            ready: true,
            exited: None,
            authenticated_since: None,
            prohibited_logged: false,
        });
        let (notify, notices) = mpsc::sync_channel(1);
        entry.connected_observer = Some(notify);
        let events = Arc::new(Mutex::new(BTreeMap::new()));
        entry.network_restored(&events);
        assert!(entry.connection.is_some(), "正在建立的连接保持原进度");
        assert!(notices.try_recv().is_err());
        entry.event.state = TunnelState::Connected;
        entry.event.last_connected_at_ms = Some(123);
        entry.network_restored(&events);
        assert_eq!(entry.connection.as_ref().unwrap().process.child.id(), pid);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
        assert_eq!(entry.event.state, TunnelState::Connected);
        assert!(events.lock().unwrap().is_empty());
        assert!(notices.try_recv().is_ok());
        entry.reconnect_if_current(Some(122), &events);
        assert!(entry.connection.is_some(), "过期探测不能打断新连接");
        entry.reconnect_if_current(Some(123), &events);
        assert!(entry.connection.is_none());
        assert_eq!(entry.ports, vec![12345]);
        assert_eq!(entry.event.state, TunnelState::Reconnecting);
    }

    #[test]
    fn 网络恢复立即重试离线隧道_终止错误保持停止() {
        let events = Arc::new(Mutex::new(BTreeMap::new()));
        let mut entry = Entry::new(spec("同步", "甲")).unwrap();
        entry.fail(AppError::new("ssh.unreachable"), &events);
        assert!(entry.due > Instant::now());
        entry.network_restored(&events);
        assert!(entry.due <= Instant::now());
        assert_eq!(entry.ports, vec![12345]);
        entry.fail(AppError::new("ssh.auth_failed"), &events);
        entry.network_restored(&events);
        entry.reconnect_if_current(entry.event.last_connected_at_ms, &events);
        assert!(entry.fatal);
        assert_eq!(entry.event.state, TunnelState::Error);
    }

    #[test]
    fn 连续探活失败递增退避并封顶_成功才清零() {
        let events = Arc::new(Mutex::new(BTreeMap::new()));
        let mut entries = HashMap::from([("同步".into(), Entry::new(spec("同步", "甲")).unwrap())]);
        for (round, seconds) in [1, 2, 5, 10, 30, 60, 60, 60].into_iter().enumerate() {
            let entry = entries.get_mut("同步").unwrap();
            let generation = Some(round as u64 + 1);
            entry.event.last_connected_at_ms = generation;
            entry.change(TunnelState::Connected, None, false, &events);
            let before = Instant::now();
            entry.reconnect_if_current(generation, &events);
            assert_eq!(entry.probe_rebuild_attempt, round as u32 + 1);
            assert_eq!(entry.event.attempt, 0, "探活次数不改变 SSH 失败退避");
            assert!(entry.due >= before + Duration::from_secs(seconds));
            assert!(entry.due <= Instant::now() + Duration::from_secs(seconds));
            let event = &events.lock().unwrap()["同步"];
            assert_eq!(event.state, TunnelState::Reconnecting);
            assert!(event.next_retry_at_ms.unwrap() >= event.timestamp_ms + seconds * 1000 - 10);
            let due = entry.due;
            entry.reconnect_if_current(generation, &events);
            assert_eq!(entry.due, due, "重复失败结果不重复计数或推迟重试");
            entry.probe_succeeded_if_current(generation);
            assert_eq!(
                entry.probe_rebuild_attempt,
                round as u32 + 1,
                "旧连接成功结果不能清零"
            );
            let mut queue = VecDeque::from(["同步".into()]);
            assert!(ready_batch(&mut queue, &entries, &mut HashMap::new()).is_empty());
            entries.get_mut("同步").unwrap().due = Instant::now();
            assert_eq!(
                ready_batch(&mut queue, &entries, &mut HashMap::new()),
                ["同步"]
            );
        }
        let entry = entries.get_mut("同步").unwrap();
        entry.event.last_connected_at_ms = Some(100);
        entry.change(TunnelState::Connected, None, false, &events);
        entry.probe_succeeded_if_current(Some(99));
        assert_eq!(
            entry.probe_rebuild_attempt, 8,
            "过期成功不能清零新连接的计数"
        );
        entry.probe_succeeded_if_current(Some(100));
        assert_eq!(entry.probe_rebuild_attempt, 0);
        entry.reconnect_if_current(Some(100), &events);
        assert_eq!(entry.probe_rebuild_attempt, 1);
        assert!(entry.due <= Instant::now() + Duration::from_secs(1));
    }

    #[test]
    fn 探活退避不影响其他隧道及正常重连() {
        let events = Arc::new(Mutex::new(BTreeMap::new()));
        let mut entry = Entry::new(spec("同步", "甲")).unwrap();
        entry.probe_rebuild_attempt = 8;
        entry.event.state = TunnelState::Connected;
        entry.reconnect_if_current(Some(1), &events);
        assert_eq!(entry.probe_rebuild_attempt, 8, "过期失败不计数");
        entry.reconnect_if_current(None, &events);
        assert!(entry.due > Instant::now() + Duration::from_secs(59));
        let mut queue = VecDeque::from(["同步".into(), "其他".into()]);
        let entries = HashMap::from([
            ("同步".into(), entry),
            ("其他".into(), Entry::new(spec("其他", "甲")).unwrap()),
        ]);
        assert_eq!(
            ready_batch(&mut queue, &entries, &mut HashMap::new()),
            ["其他"]
        );
        let mut entry = entries
            .into_values()
            .find(|entry| entry.spec.id == "同步")
            .unwrap();
        entry.network_restored(&events);
        assert!(entry.due <= Instant::now(), "网络恢复仍立即重试离线隧道");
        entry.fail(AppError::new("ssh.unreachable"), &events);
        assert_eq!(entry.event.attempt, 1);
        assert!(
            entry.due <= Instant::now() + Duration::from_secs(1),
            "SSH 断开使用自身的退避"
        );
        entry.reconnect(&events);
        assert!(entry.due <= Instant::now(), "主动重连仍立即重试");
        assert_eq!(entry.probe_rebuild_attempt, 9, "只有探活成功才清零");
    }
}

#[cfg(test)]
type TestReconnectNotice = (String, Option<u64>, bool);

/// 无参数；返回记录探活请求的控制器、通知接收端及需回收的线程；通知第三项为是否成功。
#[cfg(test)]
pub(crate) fn test_reconnect_control()
-> (TunnelControl, Receiver<TestReconnectNotice>, JoinHandle<()>) {
    let (tx, rx) = mpsc::sync_channel(8);
    let (notify, notices) = mpsc::channel();
    let worker = thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            match request {
                Request::ReconnectIfCurrent(id, connected_at) => {
                    notify.send((id, connected_at, false)).unwrap()
                }
                Request::ProbeSucceededIfCurrent(id, connected_at) => {
                    notify.send((id, connected_at, true)).unwrap()
                }
                _ => panic!("测试控制器收到意外请求"),
            }
        }
    });
    (TunnelControl(tx), notices, worker)
}

/// 按 events 顺序响应状态查询；返回测试控制句柄及需回收的工作线程。
#[cfg(test)]
pub(crate) fn test_control(mut events: VecDeque<TunnelEvent>) -> (TunnelControl, JoinHandle<()>) {
    let (tx, rx) = mpsc::sync_channel(8);
    let worker = thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            match request {
                Request::Status(_, reply) => {
                    let _ = reply.send(events.pop_front());
                }
                Request::Stop(_, reply) => {
                    let _ = reply.send(Ok(()));
                }
                _ => panic!("测试控制器收到意外请求"),
            }
        }
    });
    (TunnelControl(tx), worker)
}

#[cfg(test)]
mod forward_tests;

#[cfg(all(test, target_os = "linux"))]
mod real_tests;

#[cfg(all(test, unix))]
mod builtin_real_tests;
