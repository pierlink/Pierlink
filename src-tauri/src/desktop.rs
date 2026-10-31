//! 服务器桌面（VNC）：检测服务器上的桌面、经 SSH 打开到 VNC 的字节流交给界面里的 noVNC、VNC 密码存凭据管理器；
//! 服务器上没有桌面时新建 Pierlink 桌面（只有本人能连的 TigerVNC，关掉查看后继续运行），缺服务端程序时先获取。
//! 本机和服务器都不开端口：内置 SSH 直接开通道，系统 OpenSSH 用 `-W` 或服务器上的中继。
use crate::data::DbState;
use pierlink_core::error::AppError;
use pierlink_core::vnc::{
    self, AcquireOptions, AcquireStep, CreateDesktop, Flavor, ListenScope, StartedBy, Tools,
    VncCloser, VncDetection, VncTarget, VncWriter,
};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::ipc::{Channel, InvokeResponseBody, IpcResponse};
use tauri::{AppHandle, Manager};

/// 连接前探测 RFB 握手最多等多久
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// 一次发给界面的最大字节数
const CHUNK: usize = 64 * 1024;

/// 发给界面的一段 VNC 数据：按原始字节传（不转 JSON），界面收到的是 ArrayBuffer
#[derive(Type)]
pub struct DesktopChunk(Vec<u8>);

impl IpcResponse for DesktopChunk {
    fn body(self) -> tauri::Result<InvokeResponseBody> {
        Ok(InvokeResponseBody::Raw(self.0))
    }
}

/// 连到哪里：服务器上的 Unix 套接字，或服务器 127.0.0.1 的端口
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DesktopTargetDto {
    Unix { path: String },
    Tcp { port: u16 },
}

#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DesktopSessionDto {
    pub key: String,
    pub display: Option<u16>,
    /// tigervnc / turbovnc / x11vnc / other
    pub flavor: String,
    pub target: DesktopTargetDto,
    /// 不只听本机
    pub exposed: bool,
    /// 由系统服务启动（改参数要管理员）
    pub system_unit: Option<String>,
    pub pierlink: bool,
    /// Pierlink 桌面的桌面环境名字（如 Xfce Session）；其他为 null
    pub name: Option<String>,
}

#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DesktopEnvDto {
    pub id: String,
    pub name: String,
    pub heavy: bool,
    /// Pierlink 免 sudo 获取的轻量桌面（名字由界面按语言显示）
    pub light: bool,
}

#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DesktopDetectionDto {
    pub sessions: Vec<DesktopSessionDto>,
    pub desktops: Vec<DesktopEnvDto>,
    /// 能新建 Pierlink 桌面（有桌面环境、工具齐全；服务端程序可以新建时再获取）
    pub can_create: bool,
    /// 新建前要先获取服务端程序（服务器上还没有能用的 TigerVNC）
    pub needs_server_program: bool,
    /// 服务器缺的工具（缺了就不能新建）
    pub missing_tools: Vec<String>,
    /// 可以用 TigerVNC 官方包（服务器或这台电脑下载）
    pub official_ok: bool,
    /// 一键安装命令（需要 sudo）；没有就 null
    pub install_command: Option<String>,
    pub install_command_light: Option<String>,
}

/// 新建桌面的请求
#[derive(Deserialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DesktopCreateRequest {
    /// 这次新建的编号（界面生成，取消时用）
    pub task: String,
    /// 桌面环境（检测结果 desktops 里的 id）
    pub desktop_id: String,
    pub width: u16,
    pub height: u16,
    /// 允许这台电脑下载 TigerVNC 官方包再传上去
    pub allow_official_download: bool,
    /// 用户选的已下载官方包（本机路径）
    pub manual_package: Option<String>,
}

/// 下载还是上传
#[derive(Serialize, Type, Clone, Copy, Debug)]
#[serde(rename_all = "camelCase")]
pub enum TransferPhase {
    Download,
    Upload,
}

/// 新建桌面的进度（获取失败的那一步不单独报：最后的错误里有每一步的原因）
#[derive(Serialize, Type, Clone, Debug)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DesktopCreateProgress {
    /// 开始尝试一种获取方式
    Step { source: vnc::AcquireSource },
    /// 下载或上传了多少字节（总量不知道为 null；用 f64 传给界面）
    Bytes {
        phase: TransferPhase,
        bytes: f64,
        total: Option<f64>,
    },
    /// 服务端程序已就绪
    Ready,
    /// 正在启动桌面
    Starting,
}

impl DesktopCreateProgress {
    fn from_step(step: AcquireStep) -> Option<Self> {
        Some(match step {
            AcquireStep::Started { source } => Self::Step { source },
            AcquireStep::Download { bytes, total, .. } => Self::Bytes {
                phase: TransferPhase::Download,
                bytes: bytes as f64,
                total: total.map(|t| t as f64),
            },
            AcquireStep::Upload { bytes, total } => Self::Bytes {
                phase: TransferPhase::Upload,
                bytes: bytes as f64,
                total: Some(total as f64),
            },
            AcquireStep::Ready { .. } => Self::Ready,
            AcquireStep::Failed { .. } => return None,
        })
    }
}

#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DesktopCreatedDto {
    pub session: DesktopSessionDto,
    /// 服务器会在退出登录时结束用户进程，且没能让它保留：断开后桌面可能被结束
    pub may_end_on_logout: bool,
}

#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DesktopConnectDto {
    /// 连接编号（发送、关闭时用）
    pub id: f64,
    /// 不需要密码
    pub no_auth: bool,
    /// 查看器支持这种认证
    pub supported: bool,
}

struct Conn {
    writer: Mutex<VncWriter>,
    closer: VncCloser,
}

/// 打开着的桌面连接，以及每台服务器最近一次检测到的工具（选中继要用）
#[derive(Default)]
pub struct DesktopState {
    next: AtomicU64,
    conns: Mutex<HashMap<u64, Arc<Conn>>>,
    tools: Mutex<HashMap<String, Tools>>,
    /// 正在新建的桌面：编号 → 取消标志
    creating: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn failed(e: impl ToString) -> AppError {
    AppError::new("desktop.task_failed").with("reason", e.to_string())
}

fn flavor_name(f: Flavor) -> &'static str {
    match f {
        Flavor::TigerVnc => "tigervnc",
        Flavor::TurboVnc => "turbovnc",
        Flavor::X11vnc => "x11vnc",
        Flavor::RealVnc | Flavor::Other => "other",
    }
}

/// 检测结果里的一个会话 → 界面用的条目；连不上的（只听别的网卡地址、没有端口也没有套接字）跳过
fn session_dto(s: &vnc::VncSession, d: &VncDetection) -> Option<DesktopSessionDto> {
    let target = if let Some(path) = s.unix_paths.first() {
        DesktopTargetDto::Unix { path: path.clone() }
    } else {
        let port = s
            .listens
            .iter()
            .find(|l| matches!(l.scope, ListenScope::Loopback | ListenScope::All))?
            .port;
        DesktopTargetDto::Tcp { port }
    };
    let key = match &target {
        DesktopTargetDto::Unix { path } => format!("unix:{path}"),
        DesktopTargetDto::Tcp { port } => format!("tcp:{port}"),
    };
    Some(DesktopSessionDto {
        key,
        display: s.display,
        flavor: flavor_name(s.flavor).into(),
        target,
        exposed: s
            .listens
            .iter()
            .any(|l| !matches!(l.scope, ListenScope::Loopback)),
        system_unit: match &s.started_by {
            StartedBy::SystemUnit(name) => Some(name.clone()),
            _ => None,
        },
        pierlink: s.pierlink,
        name: s
            .pierlink
            .then(|| {
                d.pierlink_desktops
                    .iter()
                    .find(|p| !p.stale && Some(p.display) == s.display)
                    .map(|p| p.desktop_name.clone())
            })
            .flatten(),
    })
}

/// 新建 Pierlink 桌面必需、服务器上却没有的工具
fn missing_tools(t: &Tools) -> Vec<String> {
    let mut missing = Vec::new();
    if !t.setsid {
        missing.push("setsid".to_string());
    }
    if !t.xauth {
        missing.push("xauth".to_string());
    }
    if !t.dbus_run_session && !t.dbus_launch {
        missing.push("dbus-run-session".to_string());
    }
    missing
}

/// 服务器上已有能用的 TigerVNC（1.9 起支持 Unix 套接字）
fn has_usable_tigervnc(d: &VncDetection) -> bool {
    d.tigervnc
        .as_ref()
        .is_some_and(|b| b.flavor == Flavor::TigerVnc && b.version >= (1, 9, 0))
}

fn vnc_target(t: &DesktopTargetDto) -> VncTarget {
    match t {
        DesktopTargetDto::Unix { path } => VncTarget::UnixPath(path.clone()),
        DesktopTargetDto::Tcp { port } => VncTarget::Loopback(*port),
    }
}

/// 后台命令上下文（缺密码时弹框：打开桌面是用户的操作）
fn context(
    app: &AppHandle,
    server_id: &str,
) -> Result<
    (
        pierlink_core::process::SshProgram,
        pierlink_core::ssh::args::SshCommand,
    ),
    AppError,
> {
    let (program, cmd, _) =
        crate::deploy::background_command(app, server_id, crate::keys::Access::User("服务器桌面"))?;
    Ok((program, cmd))
}

/// 查找服务器上的桌面（只读）
///
/// :param server_id: 服务器 ID
/// :return: 已有的 VNC 桌面、能用的桌面环境、安装命令
#[tauri::command]
#[specta::specta]
pub async fn desktop_detect(
    app: AppHandle,
    server_id: String,
) -> Result<DesktopDetectionDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (program, cmd) = context(&app, &server_id)?;
        let mut d = vnc::detect(&program, &cmd)?;
        // 服务器重启或桌面已结束留下的记录：只清状态文件和残留套接字，不结束任何进程
        if d.pierlink_desktops.iter().any(|p| p.stale) {
            match vnc::cleanup_stale(&program, &cmd) {
                Ok(n) => {
                    eprintln!("清理残留桌面记录: 服务器={server_id}, 条数={n}");
                    d.pierlink_desktops.retain(|p| !p.stale);
                }
                Err(e) => eprintln!("清理残留桌面记录失败: 服务器={server_id}, 错误={}", e.code),
            }
        }
        lock(&app.state::<DesktopState>().tools).insert(server_id.clone(), d.tools.clone());
        let sessions: Vec<DesktopSessionDto> = d
            .sessions
            .iter()
            .filter_map(|s| session_dto(s, &d))
            .collect();
        let missing = missing_tools(&d.tools);
        let dto = DesktopDetectionDto {
            install_command: vnc::install_command(&d.distro, d.package_manager, false),
            install_command_light: vnc::install_command(&d.distro, d.package_manager, true),
            // 没有桌面环境、缺工具时，能免 sudo 获取轻量桌面也能新建（它自带需要的工具）
            can_create: (!d.desktops.is_empty() && missing.is_empty()) || d.light_desktop_available,
            needs_server_program: !has_usable_tigervnc(&d),
            missing_tools: missing,
            official_ok: d.official_pkg.ok,
            desktops: d
                .desktops
                .iter()
                .map(|e| DesktopEnvDto {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    heavy: e.heavy,
                    light: false,
                })
                .chain(
                    (d.desktops.is_empty() && d.light_desktop_available).then(|| DesktopEnvDto {
                        id: vnc::LIGHT_DESKTOP_ID.into(),
                        name: String::new(),
                        heavy: false,
                        light: true,
                    }),
                )
                .collect(),
            sessions,
        };
        eprintln!(
            "查找服务器桌面: 服务器={server_id}, 桌面数={}",
            dto.sessions.len()
        );
        Ok(dto)
    })
    .await
    .map_err(failed)?
}

/// 连接一个桌面：先探测握手（不认证），再打开交给界面的字节流
///
/// :param target: 服务器上的套接字或本机端口
/// :param on_data: 服务器发来的数据（原始字节）
/// :param on_closed: 断开时发一次（出错时带错误，正常结束为 null）
/// :return: 连接编号和认证信息
#[tauri::command]
#[specta::specta]
pub async fn desktop_connect(
    app: AppHandle,
    server_id: String,
    target: DesktopTargetDto,
    on_data: Channel<DesktopChunk>,
    on_closed: Channel<Option<AppError>>,
) -> Result<DesktopConnectDto, AppError> {
    let dto_target = target;
    tauri::async_runtime::spawn_blocking(move || {
        let (program, cmd) = context(&app, &server_id)?;
        let state = app.state::<DesktopState>();
        let tools = lock(&state.tools)
            .get(&server_id)
            .cloned()
            .unwrap_or_default();
        let probe = vnc::probe_rfb(
            vnc::open_stream(&program, &cmd, &tools, vnc_target(&dto_target))?,
            PROBE_TIMEOUT,
        )?;
        let (mut reader, writer, closer) =
            vnc::open_stream(&program, &cmd, &tools, vnc_target(&dto_target))?.split();
        let id = state.next.fetch_add(1, Ordering::Relaxed) + 1;
        let conn = Arc::new(Conn {
            writer: Mutex::new(writer),
            closer,
        });
        lock(&state.conns).insert(id, conn);
        let app2 = app.clone();
        std::thread::Builder::new()
            .name("桌面数据".into())
            .spawn(move || {
                let mut buf = vec![0u8; CHUNK];
                let error = loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break None,
                        Ok(n) => {
                            if on_data.send(DesktopChunk(buf[..n].to_vec())).is_err() {
                                break None;
                            }
                        }
                        Err(e) if e.code == "vnc.stream_closed" => break None,
                        Err(e) => break Some(e),
                    }
                };
                if let Some(c) = lock(&app2.state::<DesktopState>().conns).remove(&id) {
                    c.closer.close();
                }
                if let Some(e) = &error {
                    eprintln!("服务器桌面断开: 连接={id}, 错误={}", e.code);
                }
                let _ = on_closed.send(error);
            })
            .map_err(failed)?;
        eprintln!(
            "连接服务器桌面: 服务器={server_id}, 连接={id}, 无密码={}",
            probe.no_auth
        );
        Ok(DesktopConnectDto {
            id: id as f64,
            no_auth: probe.no_auth,
            supported: probe.novnc_supported,
        })
    })
    .await
    .map_err(failed)?
}

/// 新建 Pierlink 桌面：服务器上没有能用的 TigerVNC 时先获取（进度经 on_progress），再启动桌面
///
/// :param request: 桌面环境、尺寸、是否允许电脑下载官方包、手动选的官方包
/// :param on_progress: 获取和启动的进度
/// :return: 新桌面（可直接连接），以及断开后是否可能被系统结束
#[tauri::command]
#[specta::specta]
pub async fn desktop_create(
    app: AppHandle,
    server_id: String,
    request: DesktopCreateRequest,
    on_progress: Channel<DesktopCreateProgress>,
) -> Result<DesktopCreatedDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let cancel = Arc::new(AtomicBool::new(false));
        let state = app.state::<DesktopState>();
        lock(&state.creating).insert(request.task.clone(), cancel.clone());
        let result = create(&app, &server_id, &request, &on_progress, &cancel);
        lock(&state.creating).remove(&request.task);
        match &result {
            Ok(r) => eprintln!(
                "新建服务器桌面: 服务器={server_id}, 显示号={:?}, 桌面={}",
                r.session.display, request.desktop_id
            ),
            Err(e) => eprintln!("新建服务器桌面失败: 服务器={server_id}, 错误={}", e.code),
        }
        result
    })
    .await
    .map_err(failed)?
}

fn create(
    app: &AppHandle,
    server_id: &str,
    request: &DesktopCreateRequest,
    on_progress: &Channel<DesktopCreateProgress>,
    cancel: &AtomicBool,
) -> Result<DesktopCreatedDto, AppError> {
    let (program, cmd) = context(app, server_id)?;
    let d = vnc::detect(&program, &cmd)?;
    lock(&app.state::<DesktopState>().tools).insert(server_id.to_string(), d.tools.clone());
    let opts = AcquireOptions {
        allow_official_download: request.allow_official_download,
        manual_package: request
            .manual_package
            .as_ref()
            .map(std::path::PathBuf::from),
    };
    let report = |step: AcquireStep| {
        if let Some(p) = DesktopCreateProgress::from_step(step) {
            let _ = on_progress.send(p);
        }
    };
    let xvnc = vnc::acquire_xvnc(&program, &cmd, &d, opts, &report, cancel)?;
    // 轻量桌面：先取（带进度），新建时就不用再下载
    if request.desktop_id == vnc::LIGHT_DESKTOP_ID {
        vnc::acquire_light_desktop(&program, &cmd, &report, cancel)?;
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(AppError::new("vnc.cancelled"));
    }
    let _ = on_progress.send(DesktopCreateProgress::Starting);
    let created = vnc::create_desktop(
        &program,
        &cmd,
        CreateDesktop {
            desktop_id: request.desktop_id.clone(),
            width: request.width,
            height: request.height,
            xvnc,
        },
    )?;
    // 启动途中点了取消：桌面已经建出来了，关掉它再报取消
    if cancel.load(Ordering::Relaxed) {
        let _ = vnc::stop_desktop(&program, &cmd, created.display);
        return Err(AppError::new("vnc.cancelled"));
    }
    let kills_on_logout = d
        .logind
        .as_ref()
        .is_some_and(|l| l.kill_user_processes && !l.linger);
    Ok(DesktopCreatedDto {
        session: DesktopSessionDto {
            key: format!("unix:{}", created.socket),
            display: Some(created.display),
            flavor: "tigervnc".into(),
            target: DesktopTargetDto::Unix {
                path: created.socket.clone(),
            },
            exposed: false,
            system_unit: None,
            pierlink: true,
            name: Some(created.desktop_name.clone()),
        },
        may_end_on_logout: created.linger_failed && kills_on_logout,
    })
}

/// 选择已下载的 TigerVNC 官方包（tigervnc-1.16.2.x86_64.tar.gz）；取消选择为 null
#[tauri::command]
#[specta::specta]
pub async fn desktop_pick_package(app: AppHandle) -> Result<Option<String>, AppError> {
    use tauri_plugin_dialog::DialogExt;
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .add_filter("TigerVNC", &["gz"])
            .blocking_pick_file()
            .map(|p| {
                p.into_path()
                    .map(|p| p.to_string_lossy().into_owned())
                    .map_err(failed)
            })
            .transpose()
    })
    .await
    .map_err(failed)?
}

/// 取消正在进行的新建（在获取的阶段之间和传输的块之间生效）
#[tauri::command]
#[specta::specta]
pub async fn desktop_create_cancel(app: AppHandle, task: String) -> Result<(), AppError> {
    if let Some(c) = lock(&app.state::<DesktopState>().creating).get(&task) {
        c.store(true, Ordering::Relaxed);
    }
    Ok(())
}

/// 关闭 Pierlink 桌面：结束服务器上的这个桌面（里面的程序都会结束）
///
/// :param display: 显示号
#[tauri::command]
#[specta::specta]
pub async fn desktop_stop(app: AppHandle, server_id: String, display: u16) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (program, cmd) = context(&app, &server_id)?;
        vnc::stop_desktop(&program, &cmd, display)?;
        eprintln!("关闭服务器桌面: 服务器={server_id}, 显示号={display}");
        Ok(())
    })
    .await
    .map_err(failed)?
}

/// 把界面发来的数据写给服务器（base64：方向上只有键盘、鼠标、剪贴板，量很小）
#[tauri::command]
#[specta::specta]
pub async fn desktop_send(app: AppHandle, id: f64, data: String) -> Result<(), AppError> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|_| AppError::new("desktop.invalid_data"))?;
    let conn = lock(&app.state::<DesktopState>().conns)
        .get(&(id as u64))
        .cloned()
        .ok_or_else(|| AppError::new("vnc.stream_closed"))?;
    tauri::async_runtime::spawn_blocking(move || lock(&conn.writer).write(&bytes))
        .await
        .map_err(failed)?
}

/// 断开一个桌面连接（服务器上的桌面照常运行）
#[tauri::command]
#[specta::specta]
pub async fn desktop_close(app: AppHandle, id: f64) -> Result<(), AppError> {
    if let Some(c) = lock(&app.state::<DesktopState>().conns).remove(&(id as u64)) {
        c.closer.close();
    }
    Ok(())
}

/// 应用退出时断开全部桌面连接
pub fn shutdown(app: &AppHandle) {
    if let Some(state) = app.try_state::<DesktopState>() {
        for (_, c) in lock(&state.conns).drain() {
            c.closer.close();
        }
        for c in lock(&state.creating).values() {
            c.store(true, Ordering::Relaxed);
        }
    }
}

/// 凭据管理器里一台服务器的全部 VNC 密码（JSON：桌面键 → 密码）；删除服务器时整条删掉
pub(crate) fn passwords_ref(server_id: &str) -> String {
    format!("pierlink/server/{server_id}/vnc")
}

fn load_passwords(server_id: &str) -> Result<HashMap<String, String>, AppError> {
    let text = crate::sync::credential_store()?.get(&passwords_ref(server_id))?;
    Ok(text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default())
}

fn store_passwords(server_id: &str, map: &HashMap<String, String>) -> Result<(), AppError> {
    let store = crate::sync::credential_store()?;
    if map.is_empty() {
        return store.delete(&passwords_ref(server_id));
    }
    store.set(
        &passwords_ref(server_id),
        &serde_json::to_string(map).map_err(failed)?,
    )
}

/// 读记住的 VNC 密码（没有为 null）
#[tauri::command]
#[specta::specta]
pub async fn desktop_password(server_id: String, key: String) -> Result<Option<String>, AppError> {
    Ok(load_passwords(&server_id)?.remove(&key))
}

/// 记住 VNC 密码
#[tauri::command]
#[specta::specta]
pub async fn desktop_save_password(
    app: AppHandle,
    server_id: String,
    key: String,
    password: String,
) -> Result<(), AppError> {
    // 只给存在的服务器存，避免留下孤立的凭据
    if app
        .state::<DbState>()
        .with(|db| db.get_server(&server_id))?
        .is_none()
    {
        return Err(AppError::new("server.not_found"));
    }
    let mut map = load_passwords(&server_id)?;
    map.insert(key, password);
    store_passwords(&server_id, &map)?;
    eprintln!("记住桌面密码: 服务器={server_id}");
    Ok(())
}

/// 忘掉 VNC 密码（密码错了时）
#[tauri::command]
#[specta::specta]
pub async fn desktop_forget_password(server_id: String, key: String) -> Result<(), AppError> {
    let mut map = load_passwords(&server_id)?;
    if map.remove(&key).is_some() {
        store_passwords(&server_id, &map)?;
    }
    Ok(())
}

/// 在独立窗口里打开服务器桌面（每台服务器一个，已经开着就提到前面）。
/// 由后端创建：窗口的浏览器参数要和主窗口一致（自动化测试时主窗口带了调试端口参数），前端创建会失败
///
/// :param server_id: 服务器 ID（窗口名是 desktop-服务器ID）
/// :param title: 窗口标题
/// :param url: 页面地址，必须是 index.html#desktop-window?… 这样的独立窗口地址
#[tauri::command]
#[specta::specta]
pub async fn desktop_open_window(
    app: AppHandle,
    server_id: String,
    title: String,
    url: String,
) -> Result<(), AppError> {
    let failed = |key: &'static str, reason: &str| {
        AppError::new("vnc.window_failed").with_reason(key, reason)
    };
    if server_id.is_empty()
        || !server_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(failed("vnc.invalid_server_id", "服务器 ID 不合法"));
    }
    if !url.starts_with("index.html#desktop-window?") {
        return Err(failed("vnc.invalid_window_address", "窗口地址不合法"));
    }
    let label = format!("desktop-{server_id}");
    if let Some(win) = app.get_webview_window(&label) {
        let _ = win.unminimize();
        let _ = win.set_focus();
        return Ok(());
    }
    let mut builder =
        tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App(url.into()))
            .title(&title)
            .inner_size(1280.0, 800.0)
            .min_inner_size(640.0, 480.0);
    if let Some(args) = crate::e2e_browser_args() {
        builder = builder.additional_browser_args(&args);
    }
    builder
        .build()
        .map_err(|e| AppError::new("vnc.window_failed").with("reason", e))?;
    eprintln!("打开桌面独立窗口: 服务器={server_id}");
    Ok(())
}
