//! 手机入口：把 core 的手机服务接到 Tauri——设置（开关、监听地址、端口）、配对批准框、
//! 设备管理，以及手机能看到和能做的事（概况、终端、确认、操作记录、全部停止）。
//! 只监听用户选定的私网地址（Tailscale / 局域网 / 本机），不对所有网卡开放。
use crate::data::{DbState, data_dir};
use crate::mcp::waiters::Waiters;
use crate::terminal::TerminalState;
use pierlink_core::error::AppError;
use pierlink_core::mobile::{
    AuditRecord, HostFuture, MobileHost, MobileLog, MobileServer, Permissions, TerminalInput,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use specta::Type;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::broadcast;

/// 手机来配对，请电脑上批准
pub const PAIR_EVENT: &str = "pierlink://mobile-pair";
/// 配对请求失效（超时、手机断开），关掉批准框
pub const PAIR_CLOSED_EVENT: &str = "pierlink://mobile-pair-closed";
/// 手机入口状态或设备列表变了
pub const CHANGED_EVENT: &str = "pierlink://mobile-changed";
/// 默认端口
const DEFAULT_PORT: u16 = 48600;
/// 批准框最多等多久（和 core 的配对期限一致）
const PAIR_TIMEOUT: Duration = Duration::from_secs(120);
/// 手机上看的终端最多带多少行（回滚加当前屏幕）
const SCREEN_LINES: usize = 300;
/// 手机操作记录保留天数（logs/mobile/日期.jsonl）
const LOG_RETAIN_DAYS: u32 = 30;
/// 开着入口但监听失败（如 Tailscale 还没连上）时，隔多久重试一次
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
/// 只记录会改变东西的手机操作；只读请求只在出错时记录
const WRITE_ACTIONS: &[&str] = &[
    "pair",
    "terminal_input",
    "answer_confirm",
    "stop_all",
    "logout",
];

/// 存在设置表里的手机入口设置
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct MobileSettings {
    enabled: bool,
    bind: Option<String>,
    port: Option<u16>,
}

/// 请电脑上批准的配对请求
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PairRequestDto {
    pub id: f64,
    pub name: String,
    pub ip: String,
}

/// 本机一个可以监听的地址
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MobileAddressDto {
    pub ip: String,
    /// tailscale / lan / loopback
    pub kind: String,
    /// 网卡名
    pub name: String,
}

#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MobileStatusDto {
    pub enabled: bool,
    pub running: bool,
    pub bind: Option<String>,
    pub port: u16,
    /// 手机上打开的地址（运行中才有）
    pub url: Option<String>,
    pub addresses: Vec<MobileAddressDto>,
    pub error: Option<AppError>,
}

#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MobileInviteDto {
    pub code: String,
    pub expires_at_ms: f64,
    pub url: String,
    /// 二维码 SVG（只有方块，没有外部内容）
    pub qr_svg: String,
}

#[derive(Serialize, Deserialize, Type, Clone, Copy, Debug)]
pub struct MobilePermissionsDto {
    pub view: bool,
    pub input: bool,
    pub confirm: bool,
}

#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MobileDeviceDto {
    pub id: String,
    pub name: String,
    pub created_ms: f64,
    pub last_seen_ms: Option<f64>,
    /// 配对时浏览器报告的 User-Agent
    pub user_agent: Option<String>,
    /// 最近一次连过来的 IP
    pub last_ip: Option<String>,
    pub permissions: MobilePermissionsDto,
}

/// 手机入口的应用状态
pub struct MobileState {
    server: tokio::sync::Mutex<Option<MobileServer>>,
    events: broadcast::Sender<String>,
    pairs: Waiters<PairRequestDto, bool>,
    error: Mutex<Option<AppError>>,
    log: OnceLock<Option<MobileLog>>,
}

impl Default for MobileState {
    fn default() -> Self {
        Self {
            server: tokio::sync::Mutex::new(None),
            events: broadcast::channel(64).0,
            pairs: Waiters::default(),
            error: Mutex::new(None),
            log: OnceLock::new(),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn failed(e: impl ToString) -> AppError {
    AppError::new("mobile.task_failed").with("reason", e.to_string())
}

/// 告诉手机页面某类数据变了（confirms / terminals / overview / activity），手机重新拉
pub(crate) fn notify(app: &AppHandle, what: &str) {
    if let Some(state) = app.try_state::<MobileState>() {
        // 没有手机连着时发送失败是正常的
        let _ = state.events.send(what.to_owned());
    }
}

/// 写一行手机操作记录（数据目录 logs/mobile/日期.jsonl，保留 30 天）；不含邀请码、Cookie、输入文字
fn record(app: &AppHandle, entry: Value) {
    let Some(state) = app.try_state::<MobileState>() else {
        return;
    };
    let log = state.log.get_or_init(|| {
        data_dir(app)
            .ok()
            .map(|d| MobileLog::new(d.join("logs").join("mobile"), LOG_RETAIN_DAYS))
    });
    if let Some(log) = log {
        log.write(entry);
    }
}

fn load_settings(app: &AppHandle) -> MobileSettings {
    app.state::<DbState>()
        .with(|db| db.get_setting::<MobileSettings>("mobile"))
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn save_settings(app: &AppHandle, s: &MobileSettings) -> Result<(), AppError> {
    app.state::<DbState>()
        .with(|db| db.set_setting("mobile", s))
}

/// 地址归类：Tailscale（100.64.0.0/10、fd7a:115c:a1e0::/48）、局域网（含 fd00::/8）、本机
fn kind_of(ip: &IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if v4.is_loopback() {
                Some("loopback")
            } else if o[0] == 100 && (64..128).contains(&o[1]) {
                Some("tailscale")
            } else if v4.is_private() {
                Some("lan")
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            if v6.is_loopback() {
                Some("loopback")
            } else if s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0 {
                Some("tailscale")
            } else if (s[0] & 0xff00) == 0xfd00 {
                Some("lan")
            } else {
                None
            }
        }
    }
}

/// 本机可以给手机入口监听的地址（只列私网），Tailscale 在前
fn addresses() -> Vec<MobileAddressDto> {
    let mut out: Vec<MobileAddressDto> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| {
            let ip = i.ip();
            // 只列 IPv4：手机经 Tailscale（100.x）或局域网连 IPv4 就够了；IPv6 地址（如 ::1）
            // 在用户的电脑上打不开，和对应的 IPv4 地址重复列出只会让人困惑
            if !ip.is_ipv4() {
                return None;
            }
            kind_of(&ip).map(|kind| MobileAddressDto {
                ip: ip.to_string(),
                kind: kind.into(),
                name: i.name,
            })
        })
        .collect();
    let rank = |k: &str| match k {
        "tailscale" => 0,
        "lan" => 1,
        _ => 2,
    };
    out.sort_by(|a, b| rank(&a.kind).cmp(&rank(&b.kind)).then(a.ip.cmp(&b.ip)));
    out.dedup_by(|a, b| a.ip == b.ip);
    if !out.iter().any(|a| a.ip == "127.0.0.1") {
        out.push(MobileAddressDto {
            ip: "127.0.0.1".into(),
            kind: "loopback".into(),
            name: "loopback".into(),
        });
    }
    out
}

fn url_of(addr: SocketAddr) -> String {
    match addr {
        SocketAddr::V4(a) => format!("http://{}:{}/", a.ip(), a.port()),
        SocketAddr::V6(a) => format!("http://[{}]:{}/", a.ip(), a.port()),
    }
}

/// 手机页面文件所在目录（安装包里的资源；开发时用工作目录里打出来的 dist-mobile）
fn assets_dir(app: &AppHandle) -> Result<std::path::PathBuf, AppError> {
    let bundled = app.path().resource_dir().map_err(failed)?.join("mobile");
    if bundled.join("index.html").is_file() {
        return Ok(bundled);
    }
    let dev = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dist-mobile");
    if dev.join("index.html").is_file() {
        return Ok(dev);
    }
    Err(AppError::new("mobile.assets").with_reason(
        "mobile.phone_page_missing_from_install",
        "安装包里缺少手机页面",
    ))
}

/// 按设置启动或停止手机入口
async fn apply(app: &AppHandle) {
    let state = app.state::<MobileState>();
    let mut server = state.server.lock().await;
    if let Some(old) = server.take() {
        old.stop();
        eprintln!("手机入口已停止");
    }
    *lock(&state.error) = None;
    let s = load_settings(app);
    let Some(bind) = s.bind.filter(|_| s.enabled) else {
        return;
    };
    let result = async {
        let ip: IpAddr = bind
            .parse()
            .map_err(|_| AppError::new("mobile.invalid_bind"))?;
        let addr = SocketAddr::new(ip, s.port.unwrap_or(DEFAULT_PORT));
        // 设备表和应用共用一个数据库文件，手机入口单独开一个连接
        let db = pierlink_core::storage::open(&data_dir(app)?.join("pierlink.db"))?;
        MobileServer::start(
            addr,
            Arc::new(TauriMobileHost { app: app.clone() }),
            assets_dir(app)?,
            Arc::new(Mutex::new(db)),
        )
        .await
    }
    .await;
    match result {
        Ok(started) => {
            eprintln!("手机入口已开启: 地址={}", url_of(started.local_addr()));
            record(
                app,
                json!({ "action": "listen", "address": started.local_addr().to_string(), "result": "ok" }),
            );
            *server = Some(started);
        }
        Err(e) => {
            eprintln!("手机入口开启失败: 错误={}", e.code);
            record(
                app,
                json!({ "action": "listen", "address": bind, "result": e.code }),
            );
            *lock(&state.error) = Some(e);
        }
    }
    let _ = app.emit(CHANGED_EVENT, ());
}

/// 应用启动时：上次开着就再开起来；之后开着但没监听成功（如 Tailscale 晚于本应用连上）就每 30 秒重试
pub fn start_on_launch(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        apply(&app).await;
        loop {
            tokio::time::sleep(RETRY_INTERVAL).await;
            let s = load_settings(&app);
            let state = app.state::<MobileState>();
            let failed = lock(&state.error).is_some();
            if s.enabled && s.bind.is_some() && failed && state.server.lock().await.is_none() {
                apply(&app).await;
            }
        }
    });
}

/// 应用退出时停掉
pub fn shutdown(app: &AppHandle) {
    if let Some(state) = app.try_state::<MobileState>()
        && let Ok(mut server) = state.server.try_lock()
        && let Some(old) = server.take()
    {
        old.stop();
    }
}

async fn status(app: &AppHandle) -> MobileStatusDto {
    let s = load_settings(app);
    let state = app.state::<MobileState>();
    let server = state.server.lock().await;
    let url = server.as_ref().map(|srv| url_of(srv.local_addr()));
    MobileStatusDto {
        enabled: s.enabled,
        running: server.is_some(),
        bind: s.bind,
        port: s.port.unwrap_or(DEFAULT_PORT),
        url,
        addresses: addresses(),
        error: lock(&state.error).clone(),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn mobile_status(app: AppHandle) -> Result<MobileStatusDto, AppError> {
    Ok(status(&app).await)
}

/// 打开或关闭手机入口、改监听地址和端口（只接受本机私网地址）
///
/// :param enabled: 是否打开
/// :param bind: 监听地址（本机的 Tailscale / 局域网 / 回环地址）
/// :param port: 端口（1024–65535）
#[tauri::command]
#[specta::specta]
pub async fn mobile_configure(
    app: AppHandle,
    enabled: bool,
    bind: Option<String>,
    port: u16,
) -> Result<MobileStatusDto, AppError> {
    if port < 1024 {
        return Err(AppError::new("mobile.invalid_port"));
    }
    // 只在打开时检查地址：关闭时地址可能已经不在了（比如先关了 Tailscale），照样要能关掉
    if enabled
        && let Some(b) = &bind
        && !addresses().iter().any(|a| &a.ip == b)
    {
        return Err(AppError::new("mobile.invalid_bind"));
    }
    save_settings(
        &app,
        &MobileSettings {
            enabled,
            bind,
            port: Some(port),
        },
    )?;
    apply(&app).await;
    Ok(status(&app).await)
}

/// 生成配对邀请（一次性、10 分钟）和二维码
#[tauri::command]
#[specta::specta]
pub async fn mobile_invite(app: AppHandle) -> Result<MobileInviteDto, AppError> {
    let state = app.state::<MobileState>();
    let server = state.server.lock().await;
    let srv = server
        .as_ref()
        .ok_or_else(|| AppError::new("mobile.stopped"))?;
    let invite = srv.create_invite()?;
    let url = format!("{}?code={}", url_of(srv.local_addr()), invite.code);
    let qr = qrcode::QrCode::new(url.as_bytes()).map_err(failed)?;
    let svg = qr
        .render::<qrcode::render::svg::Color<'_>>()
        .min_dimensions(200, 200)
        .quiet_zone(true)
        .build();
    eprintln!("生成手机配对邀请");
    Ok(MobileInviteDto {
        code: invite.code.clone(),
        expires_at_ms: invite.expires_at as f64 * 1000.0,
        url,
        qr_svg: svg,
    })
}

fn device_dto(d: pierlink_core::mobile::Device) -> MobileDeviceDto {
    MobileDeviceDto {
        id: d.id,
        name: d.name,
        // core 的时间是秒
        created_ms: d.created_at as f64 * 1000.0,
        last_seen_ms: Some(d.last_active_at as f64 * 1000.0),
        user_agent: d.user_agent,
        last_ip: d.last_ip,
        permissions: MobilePermissionsDto {
            view: d.permissions.view,
            input: d.permissions.input,
            confirm: d.permissions.confirm,
        },
    }
}

/// 已配对、还有效的手机（入口关着时也能看、能撤销）
#[tauri::command]
#[specta::specta]
pub async fn mobile_devices(app: AppHandle) -> Result<Vec<MobileDeviceDto>, AppError> {
    let state = app.state::<MobileState>();
    let server = state.server.lock().await;
    let list = match server.as_ref() {
        Some(srv) => {
            // 和入口关着时一致：只列有效的（没撤销、30 天内用过）
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or_default();
            srv.list_devices()?
                .into_iter()
                .filter(|d| d.active(now))
                .collect()
        }
        None => app
            .state::<DbState>()
            .with(|db| pierlink_core::mobile::devices_in(db))?,
    };
    Ok(list.into_iter().map(device_dto).collect())
}

/// 改一台手机的权限（开着的连接立即断开，按新权限重新连）
#[tauri::command]
#[specta::specta]
pub async fn mobile_set_permissions(
    app: AppHandle,
    id: String,
    permissions: MobilePermissionsDto,
) -> Result<(), AppError> {
    let p = Permissions {
        view: permissions.view,
        input: permissions.input,
        confirm: permissions.confirm,
    };
    let state = app.state::<MobileState>();
    let server = state.server.lock().await;
    match server.as_ref() {
        Some(srv) => srv.set_permissions(&id, p)?,
        None => app
            .state::<DbState>()
            .with(|db| pierlink_core::mobile::set_permissions_in(db, &id, p))?,
    }
    eprintln!(
        "修改手机权限: 设备={id}, 查看={}, 输入={}, 确认={}",
        permissions.view, permissions.input, permissions.confirm
    );
    record(
        &app,
        json!({ "action": "set_permissions", "device": id, "permissions": p, "result": "ok" }),
    );
    Ok(())
}

/// 给一台手机改名（入口关着时也能改）
#[tauri::command]
#[specta::specta]
pub async fn mobile_rename(app: AppHandle, id: String, name: String) -> Result<(), AppError> {
    let state = app.state::<MobileState>();
    let server = state.server.lock().await;
    match server.as_ref() {
        Some(srv) => srv.rename_device(&id, &name)?,
        None => app
            .state::<DbState>()
            .with(|db| pierlink_core::mobile::rename_in(db, &id, &name))?,
    }
    eprintln!("手机改名: 设备={id}");
    record(
        &app,
        json!({ "action": "rename", "device": id, "result": "ok" }),
    );
    let _ = app.emit(CHANGED_EVENT, ());
    Ok(())
}

/// 作废还没用过的邀请码（设置页关掉二维码时）；入口没开时什么都不做
#[tauri::command]
#[specta::specta]
pub async fn mobile_cancel_invite(app: AppHandle) -> Result<(), AppError> {
    let state = app.state::<MobileState>();
    let server = state.server.lock().await;
    if let Some(srv) = server.as_ref() {
        srv.cancel_invites();
        eprintln!("作废手机邀请码");
    }
    Ok(())
}

/// 撤销一台手机（id）或全部（null）；开着的连接立即断开
#[tauri::command]
#[specta::specta]
pub async fn mobile_revoke(app: AppHandle, id: Option<String>) -> Result<(), AppError> {
    let state = app.state::<MobileState>();
    let server = state.server.lock().await;
    match (server.as_ref(), &id) {
        (Some(srv), Some(id)) => srv.revoke_device(id)?,
        (Some(srv), None) => srv.revoke_all()?,
        (None, id) => app
            .state::<DbState>()
            .with(|db| pierlink_core::mobile::revoke_in(db, id.as_deref()))?,
    }
    eprintln!("撤销手机: 设备={}", id.as_deref().unwrap_or("全部"));
    record(
        &app,
        json!({ "action": "revoke", "device": id, "result": "ok" }),
    );
    let _ = app.emit(CHANGED_EVENT, ());
    Ok(())
}

/// 回答配对请求
#[tauri::command]
#[specta::specta]
pub async fn mobile_pair_answer(app: AppHandle, id: f64, allow: bool) -> Result<bool, AppError> {
    Ok(app.state::<MobileState>().pairs.answer(id as u64, allow))
}

/// 手机能看到和能做的事：都在电脑上执行，手机只发请求
struct TauriMobileHost {
    app: AppHandle,
}

/// 在阻塞线程里跑（读库、查终端、等批准框都可能阻塞），不卡住手机入口的异步运行时
fn blocking<T: Send + 'static>(
    app: &AppHandle,
    f: impl FnOnce(&AppHandle) -> Result<T, AppError> + Send + 'static,
) -> HostFuture<'static, T> {
    let app = app.clone();
    Box::pin(async move {
        tauri::async_runtime::spawn_blocking(move || f(&app))
            .await
            .map_err(failed)?
    })
}

/// 丢弃时把标记置为“已取消”
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// 手机上的按键名换成终端按键名
fn key_name(key: &str) -> &str {
    match key {
        "ArrowUp" => "Up",
        "ArrowDown" => "Down",
        "ArrowLeft" => "Left",
        "ArrowRight" => "Right",
        "Escape" => "Esc",
        other => other,
    }
}

impl MobileHost for TauriMobileHost {
    fn approve_pairing<'a>(
        &'a self,
        device_name: &'a str,
        peer_ip: IpAddr,
    ) -> HostFuture<'a, bool> {
        let name = device_name.to_owned();
        // 手机中途断开时入口会丢弃这个等待；丢弃时通知等待线程，电脑上的批准框立即关掉
        let cancelled = Arc::new(AtomicBool::new(false));
        let guard = CancelOnDrop(cancelled.clone());
        let waiting = blocking(&self.app, move |app| {
            crate::app_shell::show_main(app);
            let emitter = app.clone();
            let log_name = name.clone();
            let (id, answer) = app.state::<MobileState>().pairs.ask(
                |id| PairRequestDto {
                    id: id as f64,
                    name,
                    ip: peer_ip.to_string(),
                },
                |dto| {
                    if let Err(e) = emitter.emit(PAIR_EVENT, dto) {
                        eprintln!("推送手机配对请求失败: 错误={e}");
                    }
                },
                PAIR_TIMEOUT,
                &cancelled,
            );
            if answer.is_none() {
                let _ = app.emit(PAIR_CLOSED_EVENT, id as f64);
            }
            let allowed = answer.unwrap_or(false);
            let decision = match answer {
                Some(true) => "approved",
                Some(false) => "denied",
                None if cancelled.load(Ordering::Acquire) => "phone_left",
                None => "timeout",
            };
            eprintln!("手机配对请求: 来源={peer_ip}, 结果={decision}");
            record(
                app,
                json!({ "action": "pair_request", "device": log_name, "peer": peer_ip.to_string(), "result": decision }),
            );
            let _ = app.emit(CHANGED_EVENT, ());
            Ok(allowed)
        });
        Box::pin(async move {
            let _guard = guard;
            waiting.await
        })
    }

    fn overview(&self) -> HostFuture<'_, Value> {
        blocking(&self.app, |app| {
            let servers = app.state::<DbState>().with(|db| db.list_servers())?;
            let builtin = app.try_state::<crate::builtin::BuiltinState>();
            let servers: Vec<Value> = servers
                .iter()
                .map(|s| {
                    let usage = crate::monitor::latest_usage(app, &s.id);
                    json!({
                        "id": s.id,
                        "name": s.name,
                        "online": builtin.as_ref().and_then(|b| b.online(&s.id)),
                        "cpu": usage.and_then(|u| u.0),
                        "mem": usage.and_then(|u| u.1),
                    })
                })
                .collect();
            let sync: Vec<Value> = crate::sync::mcp_task_states(app, None)
                .unwrap_or_default()
                .into_iter()
                .map(|t| {
                    json!({
                        "id": t.id,
                        "name": t.name,
                        "state": if t.paused { "paused".to_owned() } else if t.need_bytes > 0 { "syncing".to_owned() } else { "synced".to_owned() },
                        "percent": Value::Null,
                    })
                })
                .collect();
            Ok(json!({
                "servers": servers,
                "sync": sync,
                "ai": {
                    "sessions": crate::mcp::session_count(app),
                    "pending": crate::mcp::pending_confirms(app).len(),
                },
            }))
        })
    }

    fn terminals(&self) -> HostFuture<'_, Value> {
        blocking(&self.app, |app| {
            let names: std::collections::HashMap<String, String> = app
                .state::<DbState>()
                .with(|db| db.list_servers())?
                .into_iter()
                .map(|s| (s.id, s.name))
                .collect();
            let list: Vec<Value> = app
                .state::<TerminalState>()
                .metas()
                .into_iter()
                .map(|(id, m)| {
                    json!({
                        "id": id,
                        "title": m.title,
                        "kind": if m.server_id.is_some() { "server" } else { "windows" },
                        "server": m.server_id.as_ref().and_then(|s| names.get(s)),
                        "busy": Value::Null,
                        "ai": m.ai_opened,
                    })
                })
                .collect();
            Ok(Value::Array(list))
        })
    }

    fn terminal_screen<'a>(&'a self, id: &'a str) -> HostFuture<'a, Value> {
        let id = id.to_owned();
        blocking(&self.app, move |app| {
            let manager = &app.state::<TerminalState>().manager;
            let lines = manager.recent_lines(&id, SCREEN_LINES)?;
            let screen = manager.screen_text(&id)?;
            Ok(json!({
                "text": lines.join("\n"),
                "password": pierlink_core::mcp::safety::looks_like_password_prompt(&screen),
            }))
        })
    }

    fn terminal_text<'a>(&'a self, id: &'a str) -> HostFuture<'a, String> {
        let id = id.to_owned();
        blocking(&self.app, move |app| {
            app.state::<TerminalState>().manager.screen_text(&id)
        })
    }

    fn terminal_input<'a>(&'a self, id: &'a str, input: TerminalInput) -> HostFuture<'a, Value> {
        let id = id.to_owned();
        let text = input.text.clone();
        let keys = input.keys.clone().unwrap_or_default();
        blocking(&self.app, move |app| {
            let manager = &app.state::<TerminalState>().manager;
            // 写之前再看一次：屏幕在等密码时不发文字（密码只能在电脑上输入）
            if let Some(text) = text.as_deref().filter(|t| !t.is_empty()) {
                if pierlink_core::mcp::safety::looks_like_password_prompt(
                    &manager.screen_text(&id)?,
                ) {
                    return Err(AppError::new("mobile.password_prompt"));
                }
                manager.write(&id, text.as_bytes())?;
            }
            for key in &keys {
                let bytes = pierlink_core::mcp::tools::terminal::key_bytes(key_name(key))?;
                manager.write(&id, &bytes)?;
            }
            Ok(json!({ "ok": true }))
        })
    }

    fn confirms(&self) -> HostFuture<'_, Value> {
        blocking(&self.app, |app| {
            let list: Vec<Value> = crate::mcp::pending_confirms(app)
                .into_iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "kind": c.kind,
                        "entry": c.entry,
                        "client": c.client,
                        "targets": c.targets,
                        "target": c.target,
                        "action": c.action,
                        "ageMs": 0,
                    })
                })
                .collect();
            Ok(Value::Array(list))
        })
    }

    fn answer_confirm<'a>(&'a self, id: &'a str, allow: bool) -> HostFuture<'a, Value> {
        let id = id.to_owned();
        blocking(&self.app, move |app| {
            let id: u64 = id
                .parse::<f64>()
                .map(|v| v as u64)
                .map_err(|_| AppError::new("mobile.invalid_request"))?;
            Ok(json!({ "ok": crate::mcp::answer_from_phone(app, id, allow) }))
        })
    }

    fn stop_all(&self) -> HostFuture<'_, Value> {
        blocking(&self.app, |app| {
            crate::mcp::stop_all_from_phone(app);
            Ok(json!({ "ok": true }))
        })
    }

    fn activity(&self, limit: usize) -> HostFuture<'_, Value> {
        blocking(&self.app, move |app| {
            serde_json::to_value(crate::mcp::recent_activity(app, limit)).map_err(failed)
        })
    }

    fn audit(&self, r: AuditRecord) {
        let writes = WRITE_ACTIONS.contains(&r.action.as_str());
        let failed = r.result != "ok" && r.result != "cancelled";
        if !writes && !failed {
            return;
        }
        eprintln!(
            "手机操作: 设备={}, 来源={}, 动作={}, 目标={}, 结果={}",
            r.device_name,
            r.peer,
            r.action,
            r.target.as_deref().unwrap_or("-"),
            r.result
        );
        record(
            &self.app,
            json!({ "action": r.action, "device": r.device_name, "peer": r.peer, "target": r.target, "result": r.result }),
        );
    }

    fn subscribe(&self) -> Option<broadcast::Receiver<String>> {
        Some(self.app.state::<MobileState>().events.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use super::kind_of;

    #[test]
    fn 只把私网地址当成可选() {
        let k = |s: &str| kind_of(&s.parse().unwrap());
        assert_eq!(k("100.101.1.2"), Some("tailscale"));
        assert_eq!(k("100.63.1.2"), None);
        assert_eq!(k("192.168.1.2"), Some("lan"));
        assert_eq!(k("10.0.0.5"), Some("lan"));
        assert_eq!(k("127.0.0.1"), Some("loopback"));
        assert_eq!(k("8.8.8.8"), None);
        assert_eq!(k("fd7a:115c:a1e0::1"), Some("tailscale"));
        assert_eq!(k("fd00::1"), Some("lan"));
        assert_eq!(k("2001:db8::1"), None);
    }
}
