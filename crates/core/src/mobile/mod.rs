//! 手机浏览器入口：私网监听、桌面批准配对、独立设备权限和可撤销的 HTTP/SSE 连接。
mod assets;
mod auth;
mod host;
mod http;
mod input;
mod log;

pub use host::{AuditRecord, HostFuture, MobileHost, TerminalInput};
pub use log::MobileLog;

use crate::{error::AppError, storage::Db};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};
use zeroize::Zeroize;

pub(crate) const INACTIVE_SECONDS: i64 = 30 * 24 * 60 * 60;
const MAX_DEVICES: usize = 128;
const MAX_CONNECTIONS: usize = 32;

/// 三项独立权限；默认只能查看。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Permissions {
    pub view: bool,
    pub input: bool,
    pub confirm: bool,
}
impl Default for Permissions {
    fn default() -> Self {
        Self {
            view: true,
            input: false,
            confirm: false,
        }
    }
}

/// 给桌面管理界面的设备记录，不含令牌或摘要；时间为 Unix 秒。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// 配对请求的浏览器标识，清理控制字符和首尾空白后最多 256 个字符。
    pub user_agent: Option<String>,
    /// 最近一次通过认证的请求的 TCP 来源 IP。
    pub last_ip: Option<String>,
    pub permissions: Permissions,
    pub created_at: i64,
    pub last_active_at: i64,
    pub revoked_at: Option<i64>,
}
impl Device {
    /// now 为 Unix 秒；没撤销且 30 天内用过时返回 true。
    pub fn active(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.last_active_at > now - INACTIVE_SECONDS
    }
}

/// 一次性邀请码，仅供桌面显示；expires_at 为 Unix 秒。释放时清零 code。
#[derive(Serialize)]
pub struct Invite {
    pub code: String,
    pub expires_at: i64,
}
impl Drop for Invite {
    fn drop(&mut self) {
        self.code.zeroize();
    }
}

/// 手机服务句柄；释放或 stop 时关闭监听和所有连接，设备凭据仍保存在数据库中。
pub struct MobileServer {
    shared: Arc<Shared>,
    address: SocketAddr,
    listener: Option<JoinHandle<()>>,
}

impl MobileServer {
    /// 在 bind 启动入口（端口 0 随机），host 提供桌面能力，assets 为只读网页根目录，db 为已迁移数据库。
    /// 必须在 Tokio 运行时内调用；返回可管理和停止的句柄，地址/目录/监听错误返回 AppError。
    pub async fn start(
        bind: SocketAddr,
        host: Arc<dyn MobileHost>,
        assets: PathBuf,
        db: Arc<Mutex<Db>>,
    ) -> Result<Self, AppError> {
        validate_bind_address(bind)?;
        let shared = Shared::new(host, assets, db)?;
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .map_err(listen_error)?;
        let address = listener.local_addr().map_err(listen_error)?;
        let serving = shared.clone();
        let listener = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    Some(_) = connections.join_next(), if !connections.is_empty() => {},
                    accepted = listener.accept() => {
                        let Ok((stream, peer)) = accepted else { break };
                        if let Some(connection) = serving.register() {
                            connections.spawn(http::serve(stream, serving.clone(), connection, peer.ip()));
                        }
                    }
                }
            }
            serving.close();
        });
        Ok(Self {
            shared,
            address,
            listener: Some(listener),
        })
    }

    /// 无参数；返回实际监听地址，含系统分配的端口。
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// 无参数；生成 10 分钟有效的 8 位邀请码，旧邀请及尚未完成的配对审批立即作废。
    pub fn create_invite(&self) -> Result<Invite, AppError> {
        self.shared.create_invite()
    }

    /// 无参数、无返回值；作废所有未使用的邀请码，不影响已在等待桌面批准的配对。
    pub fn cancel_invites(&self) {
        lock(&self.shared.pairing).cancel_invites();
    }

    /// 无参数；返回设备记录，包含最近撤销或失效的设备（最多 128 条）；撤销 30 天或闲置 60 天的记录自动清理。
    pub fn list_devices(&self) -> Result<Vec<Device>, AppError> {
        let state = lock(&self.shared.state);
        let db = lock(&state.db);
        db.prune_mobile_devices(now())?;
        Ok(db.mobile_devices()?.into_iter().map(|d| d.device).collect())
    }

    /// 修改 id 的三项 permissions，立即断开该设备全部连接使新权限生效；不存在返回 storage.not_found。
    pub fn set_permissions(&self, id: &str, permissions: Permissions) -> Result<(), AppError> {
        let state = lock(&self.shared.state);
        lock(&state.db).set_mobile_permissions(id, permissions)?;
        state.disconnect(Some(id), None);
        Ok(())
    }

    /// 将未撤销的 id 设备改名为 name；名字非法返回 mobile.invalid_request，不存在或已撤销返回 storage.not_found。
    pub fn rename_device(&self, id: &str, name: &str) -> Result<(), AppError> {
        let state = lock(&self.shared.state);
        rename_in(&lock(&state.db), id, name)
    }

    /// 撤销 id 并立即断开其全部连接；重复撤销成功，不存在返回 storage.not_found。
    pub fn revoke_device(&self, id: &str) -> Result<(), AppError> {
        self.shared.revoke(Some(id), None)
    }

    /// 无参数；撤销全部设备、作废邀请和待批准配对，并立即断开已认证连接。
    pub fn revoke_all(&self) -> Result<(), AppError> {
        let mut pairing = lock(&self.shared.pairing);
        pairing.invalidate();
        self.shared.revoke(None, None)
    }

    /// 无参数、无返回值；关闭监听、取消审批及所有连接，可重复调用。
    pub fn stop(&self) {
        self.shared.close();
        if let Some(listener) = &self.listener {
            listener.abort();
        }
    }
}
impl Drop for MobileServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 入口关着时直接读库：有效的设备（撤销和失效的不列）
pub fn devices_in(db: &crate::storage::Db) -> Result<Vec<Device>, AppError> {
    db.prune_mobile_devices(now())?;
    let at = now();
    Ok(db
        .mobile_devices()?
        .into_iter()
        .map(|d| d.device)
        .filter(|d| d.active(at))
        .collect())
}

/// 入口关着时直接改库：撤销一台（id）或全部（None）；下次开入口时这些设备都进不来
pub fn revoke_in(db: &crate::storage::Db, id: Option<&str>) -> Result<(), AppError> {
    db.revoke_mobile_devices(id, now())
}

/// 入口关着时直接改库：改一台设备的权限
pub fn set_permissions_in(
    db: &crate::storage::Db,
    id: &str,
    permissions: Permissions,
) -> Result<(), AppError> {
    db.set_mobile_permissions(id, permissions)
}

/// 使用 db 将未撤销的 id 设备改名为 name，入口关闭时也可调用。
/// 名字去除首尾空白后须为 1–80 个无控制字符的字符；非法返回 mobile.invalid_request，不存在或已撤销返回 storage.not_found。
pub fn rename_in(db: &crate::storage::Db, id: &str, name: &str) -> Result<(), AppError> {
    db.rename_mobile_device(id, device_name(name)?)
}

fn device_name(name: &str) -> Result<&str, AppError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(error(
            "mobile.invalid_request",
            "mobile.device_name_must_contain_1_to_80_printable_characters",
            "设备名必须为一至八十个可显示字符",
        ));
    }
    Ok(name)
}

/// 校验 bind 的 IP，允许回环、RFC1918、100.64/10 和 fd00::/8；端口 0 合法，其他地址返回 mobile.invalid_bind。
pub fn validate_bind_address(bind: SocketAddr) -> Result<(), AppError> {
    let allowed = match bind.ip() {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
        }
        IpAddr::V6(ip) => ip.is_loopback() || ip.octets()[0] == 0xfd,
    };
    if allowed {
        Ok(())
    } else {
        Err(error(
            "mobile.invalid_bind",
            "mobile.only_loopback_private_lan_or_tailscale_addresses_are_allowed",
            "只允许绑定回环、私有局域网或 Tailscale 地址",
        ))
    }
}

struct Shared {
    host: Arc<dyn MobileHost>,
    assets: PathBuf,
    state: Mutex<State>,
    pairing: Mutex<auth::Pairing>,
    limits: Limits,
    // 同一终端的检查和输入串行执行，避免两个手机请求交错。
    input_gate: tokio::sync::Mutex<()>,
    pending_lines: Mutex<HashMap<String, input::PendingLine>>,
}
struct State {
    db: Arc<Mutex<Db>>,
    stopped: bool,
    next_id: u64,
    connections: HashMap<u64, ConnectionState>,
}
struct ConnectionState {
    device: Option<String>,
    stop: watch::Sender<bool>,
}
struct Connection {
    id: u64,
    shared: Arc<Shared>,
    stopped: watch::Receiver<bool>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        lock(&self.shared.state).connections.remove(&self.id);
    }
}
impl State {
    fn disconnect(&self, device: Option<&str>, except: Option<u64>) {
        for (id, connection) in &self.connections {
            if Some(*id) != except && device.is_none_or(|d| connection.device.as_deref() == Some(d))
            {
                connection.stop.send_replace(true);
            }
        }
    }
}
#[derive(Clone, Copy)]
struct Limits {
    header: Duration,
    idle: Duration,
    approval: Duration,
    failure_delay: Duration,
    heartbeat: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            header: Duration::from_secs(10),
            idle: Duration::from_secs(60),
            approval: Duration::from_secs(120),
            failure_delay: Duration::from_millis(200),
            heartbeat: Duration::from_secs(15),
        }
    }
}
impl Shared {
    fn new(
        host: Arc<dyn MobileHost>,
        assets: PathBuf,
        db: Arc<Mutex<Db>>,
    ) -> Result<Arc<Self>, AppError> {
        let assets = std::fs::canonicalize(assets).map_err(|_| {
            error(
                "mobile.assets",
                "mobile.couldn_t_open_the_web_asset_directory",
                "无法打开网页资源目录",
            )
        })?;
        if !assets.is_dir() {
            return Err(error(
                "mobile.assets",
                "mobile.web_asset_path_must_be_a_directory",
                "网页资源路径必须为目录",
            ));
        }
        lock(&db).prune_mobile_devices(now())?;
        Ok(Arc::new(Self {
            host,
            assets,
            state: Mutex::new(State {
                db,
                stopped: false,
                next_id: 0,
                connections: HashMap::new(),
            }),
            pairing: Mutex::new(auth::Pairing::default()),
            limits: Limits::default(),
            input_gate: tokio::sync::Mutex::new(()),
            pending_lines: Mutex::default(),
        }))
    }
    fn register(self: &Arc<Self>) -> Option<Connection> {
        let mut state = lock(&self.state);
        if state.stopped || state.connections.len() >= MAX_CONNECTIONS {
            return None;
        }
        state.next_id = state.next_id.wrapping_add(1);
        let id = state.next_id;
        let (stop, stopped) = watch::channel(false);
        state
            .connections
            .insert(id, ConnectionState { device: None, stop });
        Some(Connection {
            id,
            shared: self.clone(),
            stopped,
        })
    }
    fn close(&self) {
        lock(&self.pairing).invalidate();
        let mut state = lock(&self.state);
        state.stopped = true;
        state.disconnect(None, None);
    }
    fn revoke(&self, id: Option<&str>, except: Option<u64>) -> Result<(), AppError> {
        let state = lock(&self.state);
        lock(&state.db).revoke_mobile_devices(id, now())?;
        state.disconnect(id, except);
        Ok(())
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
fn error(code: &str, text_key: &'static str, reason: &str) -> AppError {
    AppError::new(code).with_reason(text_key, reason)
}
fn listen_error(e: std::io::Error) -> AppError {
    error(
        "mobile.listen_failed",
        "mobile.couldn_t_start_the_mobile_entry_listener",
        "无法启动手机入口监听",
    )
    .with("io_kind", format!("{:?}", e.kind()))
}

#[cfg(test)]
mod tests;
