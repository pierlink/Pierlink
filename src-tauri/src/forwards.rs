//! 端口转发的 Tauri 命令：列表、保存、开关、删除；应用启动时自动开启“已启用”的转发。
//! 每条转发是共享隧道管理器（tunnels.rs）里的一条 `ssh -N` 隧道，ID 为 `forward:<转发 ID>`，
//! 断线自动重连；状态变化由后台线程推给界面（事件 `pierlink://forwards`）。

use crate::data::DbState;
use crate::servers;
use crate::tunnels::TunnelsState;
use pierlink_core::error::AppError;
use pierlink_core::storage::{ForwardKind, PortForward, ServerSource};
use pierlink_core::tunnel::{TunnelState, forward};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// 推给界面的状态事件名
pub const FORWARDS_EVENT: &str = "pierlink://forwards";
/// 隧道 ID 前缀（同一个管理器里还有同步隧道）
const PREFIX: &str = "forward:";

/// 一条转发现在的状态
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ForwardStatusDto {
    pub id: String,
    /// stopped / connecting / running / reconnecting / waiting_password（等用户输入密码）/ error
    pub state: String,
    /// 最近一次错误（重连中也可能有，说明为什么断了）
    pub error: Option<AppError>,
    /// 重连中：下次重试的时间（毫秒时间戳）
    pub next_retry_at_ms: Option<f64>,
}

impl ForwardStatusDto {
    fn stopped(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            state: "stopped".into(),
            error: None,
            next_retry_at_ms: None,
        }
    }
}

/// 给前端的转发记录（带当前状态）
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ForwardDto {
    pub id: String,
    pub server_id: String,
    pub name: String,
    pub kind: ForwardKind,
    pub local_port: u16,
    pub remote_host: Option<String>,
    pub remote_port: Option<u16>,
    pub enabled: bool,
    /// 排序号（界面只用来排序，范围限制在 i32）
    pub sort_order: i32,
    pub status: ForwardStatusDto,
}

/// 保存转发的输入；id 为空时新建
#[derive(Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ForwardInput {
    pub id: Option<String>,
    pub server_id: String,
    pub name: String,
    pub kind: ForwardKind,
    pub local_port: u16,
    pub remote_host: Option<String>,
    pub remote_port: Option<u16>,
    pub enabled: bool,
    pub sort_order: i32,
}

#[derive(Default)]
pub struct ForwardsState {
    status: Mutex<HashMap<String, ForwardStatusDto>>,
    /// 用内置 SSH 开的转发：它们的错误标上 engine=builtin（要验证码时界面提供“登录”）
    builtin: Mutex<std::collections::HashSet<String>>,
    /// 状态推送线程是否已启动
    pump: AtomicBool,
}

impl ForwardsState {
    fn status_of(&self, id: &str) -> ForwardStatusDto {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
            .unwrap_or_else(|| ForwardStatusDto::stopped(id))
    }
    /// 记下新状态；有变化时返回 true
    fn set(&self, s: ForwardStatusDto) -> bool {
        let mut map = self.status.lock().unwrap_or_else(|p| p.into_inner());
        // AppError 没有实现相等比较，按调试输出比较（只在状态推送时用，数据量很小）
        if map
            .get(&s.id)
            .is_some_and(|old| format!("{old:?}") == format!("{s:?}"))
        {
            return false;
        }
        map.insert(s.id.clone(), s);
        true
    }
}

fn to_dto(app: &AppHandle, f: PortForward) -> ForwardDto {
    let status = app.state::<ForwardsState>().status_of(&f.id);
    ForwardDto {
        id: f.id,
        server_id: f.server_id,
        name: f.name,
        kind: f.kind,
        local_port: f.local_port,
        remote_host: f.remote_host,
        remote_port: f.remote_port,
        enabled: f.enabled,
        sort_order: f.sort_order.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        status,
    }
}

fn find(app: &AppHandle, id: &str) -> Result<PortForward, AppError> {
    app.state::<DbState>()
        .with(|db| db.list_port_forwards(None))?
        .into_iter()
        .find(|f| f.id == id)
        .ok_or_else(|| AppError::new("forward.not_found"))
}

/// 把状态推给界面
fn emit(app: &AppHandle, list: Vec<ForwardStatusDto>) {
    if list.is_empty() {
        return;
    }
    if let Err(e) = app.emit(FORWARDS_EVENT, list) {
        eprintln!("推送端口转发状态失败: 错误={e}");
    }
}

/// 开启一条转发：检查服务器、生成隧道并交给共享管理器（连接在后台建立，状态随后推送）
pub(crate) fn start(app: &AppHandle, f: &PortForward) -> Result<(), AppError> {
    let server = servers::load(app, &f.server_id)?;
    // ssh config 里自带 LocalForward/RemoteForward 的别名：每条隧道都会把那些转发再开一遍，第二条起必然失败（异常 S21/F26）
    if server.source == ServerSource::SshConfig
        && let Some(alias) = &server.ssh_alias
    {
        let program = pierlink_core::process::SshProgram {
            path: crate::terminal::find_ssh(app)?,
        };
        let cfg = pierlink_core::ssh::config_file::resolve_alias(
            &program,
            alias,
            Duration::from_secs(5),
        )?;
        if cfg.has_forwards {
            return Err(AppError::new("forward.alias_has_forwards").with("alias", alias));
        }
    }
    let target = servers::target(&server)?;
    let opts = crate::keys::ssh_opts(app, &server, crate::keys::Access::Background("端口转发"))?;
    let spec = forward::to_spec(f, target, opts)?;
    // 用内置 SSH 的服务器：转发通道开在这台服务器的主连接上（和终端共用一次登录）
    let spec = crate::builtin::tunnel(
        app,
        &server,
        pierlink_core::ssh::engine::Feature::PortForward,
        pierlink_core::builtin::ConnectionClass::Main,
        spec,
    )?;
    {
        let forwards = app.state::<ForwardsState>();
        let mut builtin = forwards.builtin.lock().unwrap_or_else(|p| p.into_inner());
        if spec.builtin.is_some() {
            builtin.insert(f.id.clone());
        } else {
            builtin.remove(&f.id);
        }
    }
    let manager = app.state::<TunnelsState>().get(app)?;
    manager.start(spec)?;
    eprintln!(
        "开启端口转发: id={}, 服务器={}, 类型={:?}, 本机端口={}",
        f.id, f.server_id, f.kind, f.local_port
    );
    let state = app.state::<ForwardsState>();
    if state.set(ForwardStatusDto {
        id: f.id.clone(),
        state: "connecting".into(),
        error: None,
        next_retry_at_ms: None,
    }) {
        emit(app, vec![state.status_of(&f.id)]);
    }
    ensure_pump(app);
    Ok(())
}

/// 关掉一条转发的隧道（没有在运行时什么也不做）
pub(crate) fn stop(app: &AppHandle, id: &str) {
    if let Some(m) = app.state::<TunnelsState>().existing()
        && let Err(e) = m.stop(&format!("{PREFIX}{id}"))
        && e.code != "tunnel.not_found"
    {
        eprintln!("关闭端口转发失败: id={id}, 错误={e:?}");
    }
    let state = app.state::<ForwardsState>();
    if state.set(ForwardStatusDto::stopped(id)) {
        emit(app, vec![ForwardStatusDto::stopped(id)]);
    }
}

/// 后台线程：每 0.5 秒取一次隧道状态变化，转发相关的推给界面；AI 接入服务器入口的隧道转交给它
pub(crate) fn ensure_pump(app: &AppHandle) {
    if app
        .state::<ForwardsState>()
        .pump
        .swap(true, Ordering::SeqCst)
    {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let Some(manager) = app.state::<TunnelsState>().existing() else {
                continue;
            };
            let events = match manager.drain_events() {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("读取隧道状态失败: 错误={e:?}");
                    continue;
                }
            };
            let state = app.state::<ForwardsState>();
            let mut changed = Vec::new();
            for ev in events {
                if ev.tunnel_id.starts_with(crate::mcp::TUNNEL_PREFIX) {
                    crate::mcp::on_tunnel_event(&app, &ev);
                    continue;
                }
                let Some(id) = ev.tunnel_id.strip_prefix(PREFIX) else {
                    continue;
                };
                let s = ForwardStatusDto {
                    id: id.to_owned(),
                    state: match ev.state {
                        TunnelState::Connecting => "connecting",
                        TunnelState::Connected => "running",
                        TunnelState::Reconnecting => "reconnecting",
                        TunnelState::WaitingPassword => "waiting_password",
                        TunnelState::Stopped => "stopped",
                        TunnelState::Error => "error",
                    }
                    .into(),
                    error: ev.error.map(|e| {
                        let builtin = state
                            .builtin
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .contains(id);
                        if builtin {
                            e.with("engine", "builtin")
                        } else {
                            e
                        }
                    }),
                    next_retry_at_ms: ev.next_retry_at_ms.map(|x| x as f64),
                };
                if state.set(s.clone()) {
                    changed.push(s);
                }
            }
            emit(&app, changed);
        }
    });
}

/// 应用启动后（数据库打开之后）在后台开启所有“已启用”的转发；单条失败只记日志和状态
/// 某台服务器换了连接方式：停掉它已启用的转发，按新方式重新开启（端口不变）
pub(crate) fn restart_server(app: &AppHandle, server_id: &str) {
    let list = match app
        .state::<DbState>()
        .with(|db| db.list_port_forwards(Some(server_id)))
    {
        Ok(l) => l,
        Err(e) => {
            eprintln!("读取端口转发失败: 服务器={server_id}, 错误={e:?}");
            return;
        }
    };
    for f in list.into_iter().filter(|f| f.enabled) {
        stop(app, &f.id);
        if let Err(e) = start(app, &f) {
            eprintln!("按新的连接方式重开端口转发失败: id={}, 错误={e:?}", f.id);
            let state = app.state::<ForwardsState>();
            state.set(ForwardStatusDto {
                id: f.id.clone(),
                state: "error".into(),
                error: Some(e),
                next_retry_at_ms: None,
            });
            emit(app, vec![state.status_of(&f.id)]);
        }
    }
}

pub fn resume_enabled(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let list = match app
            .state::<DbState>()
            .with(|db| db.list_port_forwards(None))
        {
            Ok(l) => l,
            Err(e) => {
                eprintln!("读取端口转发失败: 错误={e:?}");
                return;
            }
        };
        let enabled: Vec<_> = list.into_iter().filter(|f| f.enabled).collect();
        if !enabled.is_empty() {
            eprintln!("恢复已启用的端口转发: 数量={}", enabled.len());
        }
        for f in enabled {
            if let Err(e) = start(&app, &f) {
                eprintln!("恢复端口转发失败: id={}, 错误={e:?}", f.id);
                let state = app.state::<ForwardsState>();
                state.set(ForwardStatusDto {
                    id: f.id.clone(),
                    state: "error".into(),
                    error: Some(e),
                    next_retry_at_ms: None,
                });
                emit(&app, vec![state.status_of(&f.id)]);
            }
        }
    });
}

async fn blocking<T, F>(app: AppHandle, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&AppHandle) -> Result<T, AppError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || f(&app))
        .await
        .map_err(|e| AppError::new("forward.task_failed").with("reason", e.to_string()))?
}

/// 列出端口转发（server_id 为空时列出全部）
#[tauri::command]
#[specta::specta]
pub async fn forward_list(
    app: AppHandle,
    server_id: Option<String>,
) -> Result<Vec<ForwardDto>, AppError> {
    blocking(app, move |app| {
        let list = app
            .state::<DbState>()
            .with(|db| db.list_port_forwards(server_id.as_deref()))?;
        Ok(list.into_iter().map(|f| to_dto(app, f)).collect())
    })
    .await
}

/// 新建或修改转发；已启用的会按新设置重新开启。开启失败时记录会保存，但改为未启用并返回错误
#[tauri::command]
#[specta::specta]
pub async fn forward_save(app: AppHandle, input: ForwardInput) -> Result<ForwardDto, AppError> {
    blocking(app, move |app| {
        let f = PortForward {
            id: input
                .id
                .filter(|x| !x.is_empty())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            server_id: input.server_id,
            name: input.name.trim().to_owned(),
            kind: input.kind,
            local_port: input.local_port,
            remote_host: input
                .remote_host
                .map(|h| h.trim().to_owned())
                .filter(|h| !h.is_empty()),
            remote_port: input.remote_port,
            enabled: input.enabled,
            sort_order: input.sort_order.into(),
        };
        let existed = find(app, &f.id).is_ok();
        app.state::<DbState>()
            .with(|db| db.upsert_port_forward(&f))?;
        eprintln!(
            "保存端口转发: id={}, 服务器={}, 已启用={}",
            f.id, f.server_id, f.enabled
        );
        stop(app, &f.id);
        if f.enabled
            && let Err(e) = start(app, &f)
        {
            // 开不起来：新建的整条撤销（对话框保持打开，改了再点不会多出一条），已有的改为未启用
            let undo = if existed {
                app.state::<DbState>()
                    .with(|db| db.set_port_forward_enabled(&f.id, false))
            } else {
                app.state::<DbState>()
                    .with(|db| db.delete_port_forward(&f.id))
            };
            if let Err(u) = undo {
                eprintln!("撤销端口转发失败: id={}, 错误={u:?}", f.id);
            }
            return Err(e);
        }
        Ok(to_dto(app, find(app, &f.id)?))
    })
    .await
}

/// 开启或关闭一条转发（记住开关，下次启动应用时按它自动开启）
#[tauri::command]
#[specta::specta]
pub async fn forward_set_enabled(
    app: AppHandle,
    id: String,
    enabled: bool,
) -> Result<ForwardDto, AppError> {
    blocking(app, move |app| {
        let f = find(app, &id)?;
        if enabled {
            // 用户重新开启：之前因为登录上限停下的，照常再去服务器登录一次
            crate::builtin::clear_auth_limit(app, &f.server_id);
            app.state::<DbState>()
                .with(|db| db.set_port_forward_enabled(&id, true))?;
            if let Err(e) = start(app, &PortForward { enabled: true, ..f }) {
                if let Err(u) = app
                    .state::<DbState>()
                    .with(|db| db.set_port_forward_enabled(&id, false))
                {
                    eprintln!("撤销启用失败: id={id}, 错误={u:?}");
                }
                return Err(e);
            }
        } else {
            app.state::<DbState>()
                .with(|db| db.set_port_forward_enabled(&id, false))?;
            stop(app, &id);
        }
        Ok(to_dto(app, find(app, &id)?))
    })
    .await
}

/// 删除转发（先关闭隧道）
#[tauri::command]
#[specta::specta]
pub async fn forward_delete(app: AppHandle, id: String) -> Result<(), AppError> {
    blocking(app, move |app| {
        stop(app, &id);
        app.state::<DbState>()
            .with(|db| db.delete_port_forward(&id))?;
        eprintln!("删除端口转发: id={id}");
        Ok(())
    })
    .await
}
