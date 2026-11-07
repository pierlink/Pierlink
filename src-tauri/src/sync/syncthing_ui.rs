//! 本机 Syncthing 管理页：在应用内窗口里打开，后台用 GUI 口令登录拿到会话 cookie，
//! 先设好 cookie 再打开页面（用户不用输口令）；关窗时注销。
//! 打开前记下 Pierlink 管的配置，关窗后核对：加固项被改了就静默恢复，
//! 任务的设置被改了列给用户逐项“采用新值”或“恢复”，管理页里新加的东西只列出来。
//!
//! 服务器的管理页同样在应用内窗口打开（经同步隧道），每台服务器一个窗口。服务器一侧的任务设置由 Pierlink 管理，
//! 关窗后被改的加固项和任务设置都直接改回，只告诉用户改回了几项（结果留着，不在同步页时下次进来再看）。
//! 窗口开着期间 Pierlink 自己或别的电脑改的（设备地址、被编辑过的任务、别的电脑加入文件夹）不算，不改回。
use super::{DbState, LocalGuiInfo, SyncState, now_ms};
use pierlink_core::error::AppError;
use pierlink_core::storage::{EndpointMode, SyncTask};
use pierlink_core::sync::{
    self, ConfigTarget, ConfigurationChange, ConfigurationReport, ManagedConfiguration,
    ManagedFolder,
};
use pierlink_core::syncthing::client::Client;
use pierlink_core::syncthing::gui_session::{self, GuiCookie};
use serde::Serialize;
use serde_json::Value;
use specta::Type;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use tauri::webview::cookie::Cookie;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};

/// 管理页窗口的标签（不在权限配置里，页面拿不到应用的命令）
const WINDOW: &str = "syncthing-ui";
/// 服务器管理页窗口标签的前缀，后面接服务器 ID（同样不在权限配置里）
const SERVER_WINDOW_PREFIX: &str = "syncthing-ui-server-";
/// 关窗后有需要用户处理的核对结果时推给界面
pub const CONFIG_REPORT_EVENT: &str = "pierlink://sync-config-report";

/// 一项被改过的任务设置
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChangeDto {
    /// 在本次核对结果里的位置，处理时传回
    pub index: u32,
    /// folder / device / ignores
    pub target: String,
    pub id: String,
    /// 对应的同步任务名称（文件夹和忽略规则才有）
    pub task_name: Option<String>,
    /// 字段名（Syncthing 的叫法）；exists 表示整个被删了，ignore 表示忽略规则
    pub field: String,
    /// Pierlink 的值和现在的值（JSON 文字，字符串不带引号）
    pub expected: String,
    pub actual: String,
    /// 能不能“采用新值”；不能的只能恢复
    pub adoptable: bool,
}

/// 管理页里新加的、不是 Pierlink 建的
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ForeignDto {
    pub target: String,
    pub id: String,
    pub name: String,
    pub path: Option<String>,
}

/// 关掉管理页后的核对结果
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReportDto {
    /// 已经静默恢复的加固项（字段名）
    pub hardened: Vec<String>,
    pub changes: Vec<ConfigChangeDto>,
    pub foreign: Vec<ForeignDto>,
    /// 刚恢复的设置让这个任务暂停了（恢复了目录或整个文件夹）：任务名，界面提示确认后点“继续”
    pub paused_task: Option<String>,
    /// 服务器管理页的核对结果：服务器名称；本机的为 null
    pub server: Option<String>,
    /// 服务器管理页：关窗后已经改回的任务设置项数（本机的为 0，本机的任务设置改动逐项让用户选）
    pub restored: u32,
    /// 服务器管理页：没能改回的项数（例如连接断了）
    pub unrestored: u32,
}

/// 打开中的管理页会话
struct Session {
    base: String,
    cookie: GuiCookie,
    expected: ManagedConfiguration,
}

/// 打开中的服务器管理页会话
struct ServerSession {
    name: String,
    base: String,
    cookie: GuiCookie,
    /// 打开时的管理接口：关窗时隧道断着就用它试
    client: Client,
    expected: ManagedConfiguration,
    policy: sync::HardeningPolicy,
    /// 开始打开的时间（毫秒）：之后在 Pierlink 里改过的任务，关窗时不改回
    opened_ms: i64,
}

/// 管理页状态：当前会话，以及还没处理完的核对结果
#[derive(Default)]
pub struct SyncthingUiState {
    session: Mutex<Option<Session>>,
    pending: Mutex<Option<(ManagedConfiguration, ConfigurationReport)>>,
    /// 打开中的服务器管理页（按服务器 ID）
    servers: Mutex<HashMap<String, ServerSession>>,
    /// 服务器管理页关掉后的结果，用户还没看过的（早的在前；只弹最近的一条，看过全部清掉）
    server_pending: Mutex<Vec<ConfigReportDto>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn failed(e: impl ToString) -> AppError {
    AppError::new("sync.task_failed").with("reason", e.to_string())
}

fn target_text(t: ConfigTarget) -> &'static str {
    match t {
        ConfigTarget::Folder => "folder",
        ConfigTarget::Device => "device",
        ConfigTarget::Ignores => "ignores",
    }
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 采用新值时，哪些字段能接受：Pierlink 存了的写回任务设置，没存的直接接受
fn adoptable(change: &ConfigurationChange) -> bool {
    match change.target {
        ConfigTarget::Ignores => true,
        ConfigTarget::Folder => matches!(
            change.field.as_str(),
            "label"
                | "type"
                | "versioning"
                | "rescanIntervalS"
                | "fsWatcherEnabled"
                | "fsWatcherDelayS"
                | "paused"
        ),
        ConfigTarget::Device => false,
    }
}

/// 记下现在 Pierlink 管的配置：所有同步任务的文件夹、忽略规则，以及现有设备
fn snapshot(app: &AppHandle, client: &Client) -> Result<ManagedConfiguration, AppError> {
    let tasks = app.state::<DbState>().with(|db| db.list_sync_tasks())?;
    let folders = client.list_folders()?;
    let folders = folders.as_array().cloned().unwrap_or_default();
    let mut managed = Vec::new();
    for task in &tasks {
        let Some(config) = folders
            .iter()
            .find(|f| f["id"].as_str() == Some(task.folder_id.as_str()))
        else {
            continue;
        };
        let ignores = client.get_ignores(&task.folder_id)?;
        let ignore_rules = ignores["ignore"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        managed.push(ManagedFolder {
            config: config.clone(),
            ignore_rules,
        });
    }
    let devices = client
        .list_devices()?
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok(ManagedConfiguration {
        folders: managed,
        devices,
    })
}

fn report_dto(app: &AppHandle, report: &ConfigurationReport) -> ConfigReportDto {
    let tasks = app
        .state::<DbState>()
        .with(|db| db.list_sync_tasks())
        .unwrap_or_default();
    let task_name = |id: &str| {
        tasks
            .iter()
            .find(|t| t.folder_id == id)
            .map(|t| t.name.clone())
    };
    ConfigReportDto {
        hardened: report.hardened.clone(),
        changes: report
            .task_changes
            .iter()
            .enumerate()
            .map(|(i, c)| ConfigChangeDto {
                index: i as u32,
                target: target_text(c.target).into(),
                id: c.id.clone(),
                task_name: match c.target {
                    ConfigTarget::Device => None,
                    _ => task_name(&c.id),
                },
                field: c.field.clone(),
                expected: value_text(&c.expected),
                actual: value_text(&c.actual),
                adoptable: adoptable(c),
            })
            .collect(),
        foreign: report
            .foreign
            .iter()
            .map(|f| ForeignDto {
                target: target_text(f.target).into(),
                id: f.id.clone(),
                name: f.name.clone(),
                path: f.path.clone(),
            })
            .collect(),
        paused_task: None,
        server: None,
        restored: 0,
        unrestored: 0,
    }
}

/// 采用新值后，把它记进期望值：以后再核对时不再算作改动
fn absorb(expected: &mut ManagedConfiguration, change: &ConfigurationChange) {
    let Some(folder) = expected
        .folders
        .iter_mut()
        .find(|f| f.config["id"].as_str() == Some(change.id.as_str()))
    else {
        return;
    };
    match change.target {
        ConfigTarget::Ignores => {
            if let Ok(rules) = serde_json::from_value(change.actual.clone()) {
                folder.ignore_rules = rules;
            }
        }
        ConfigTarget::Folder => {
            if let Some(obj) = folder.config.as_object_mut() {
                obj.insert(change.field.clone(), change.actual.clone());
            }
        }
        ConfigTarget::Device => {}
    }
}

/// 把 cookie 交给窗口
fn to_cookie(c: &GuiCookie) -> Cookie<'static> {
    let mut b = Cookie::build((c.name.clone(), c.value.clone()))
        .domain("127.0.0.1")
        .path(c.path.clone())
        .http_only(c.http_only)
        .secure(c.secure);
    if let Some(s) = c.same_site.as_deref() {
        b = b.same_site(match s.to_ascii_lowercase().as_str() {
            "strict" => tauri::webview::cookie::SameSite::Strict,
            "none" => tauri::webview::cookie::SameSite::None,
            _ => tauri::webview::cookie::SameSite::Lax,
        });
    }
    b.build()
}

/// 管理页关了：注销、删 cookie、核对配置；有要用户处理的就推给界面
fn on_closed(app: AppHandle) {
    let Some(session) = lock(&app.state::<SyncthingUiState>().session).take() else {
        return;
    };
    if let Err(e) = gui_session::logout(&session.base, &session.cookie) {
        eprintln!("注销 Syncthing 管理页会话失败: 错误={}", e.code);
    }
    if let Some(main) = app.get_webview_window("main")
        && let Err(e) = main.delete_cookie(to_cookie(&session.cookie))
    {
        eprintln!("删除管理页 cookie 失败: 错误={e}");
    }
    let state = app.state::<SyncState>();
    let inner = state.lock();
    let (Some(local), Some(gui)) = (inner.local.as_ref(), inner.local_gui.as_ref()) else {
        return;
    };
    match sync::reconcile_configuration(local.client(), &gui.policy, &session.expected) {
        Ok(report) => {
            eprintln!(
                "核对本机 Syncthing 配置: 恢复加固项={}, 任务改动={}, 外来项={}",
                report.hardened.len(),
                report.task_changes.len(),
                report.foreign.len()
            );
            if report.hardened.is_empty()
                && report.task_changes.is_empty()
                && report.foreign.is_empty()
            {
                return;
            }
            let dto = report_dto(&app, &report);
            *lock(&app.state::<SyncthingUiState>().pending) = Some((session.expected, report));
            if let Err(e) = app.emit(CONFIG_REPORT_EVENT, dto) {
                eprintln!("推送配置核对结果失败: 错误={e}");
            }
        }
        Err(e) => eprintln!("核对本机 Syncthing 配置失败: 错误={}", e.code),
    }
}

/// 在应用内窗口打开本机 Syncthing 管理页（自动登录）。已经开着就切到前面
#[tauri::command]
#[specta::specta]
pub async fn sync_open_syncthing_ui(app: AppHandle) -> Result<(), AppError> {
    if let Some(w) = app.get_webview_window(WINDOW) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let handle = app.clone();
    let (base, cookie) = tauri::async_runtime::spawn_blocking(move || {
        let app = handle;
        let state = app.state::<SyncState>();
        let mut inner = state.lock();
        super::ensure_local(&app, &mut inner)?;
        let gui: &LocalGuiInfo = inner
            .local_gui
            .as_ref()
            .ok_or_else(|| AppError::new("sync.local_unavailable"))?;
        let local = inner
            .local
            .as_ref()
            .ok_or_else(|| AppError::new("sync.local_unavailable"))?;
        // 上次的改动还没处理完：沿用当时的期望值，不能把没处理的改动当成新的基线
        let pending = lock(&app.state::<SyncthingUiState>().pending)
            .as_ref()
            .map(|(e, _)| e.clone());
        let expected = match pending {
            Some(e) => e,
            None => snapshot(&app, local.client())?,
        };
        let cookie = gui_session::login(&gui.base, "pierlink", &gui.password)?;
        let base = gui.base.clone();
        *lock(&app.state::<SyncthingUiState>().session) = Some(Session {
            base: base.clone(),
            cookie: cookie.clone(),
            expected,
        });
        Ok::<_, AppError>((base, cookie))
    })
    .await
    .map_err(failed)??;
    let result = (|| -> Result<(), AppError> {
        let window = WebviewWindowBuilder::new(
            &app,
            WINDOW,
            WebviewUrl::External("about:blank".parse().map_err(failed)?),
        )
        .title("Syncthing · Pierlink")
        .inner_size(1180.0, 820.0)
        .min_inner_size(720.0, 480.0)
        .visible(false)
        .build()
        .map_err(failed)?;
        let app2 = app.clone();
        window.on_window_event(move |e| {
            if let WindowEvent::Destroyed = e {
                let app = app2.clone();
                std::thread::spawn(move || on_closed(app));
            }
        });
        window.set_cookie(to_cookie(&cookie)).map_err(failed)?;
        window
            .navigate(format!("{base}/").parse().map_err(failed)?)
            .map_err(failed)?;
        window.show().map_err(failed)?;
        window.set_focus().map_err(failed)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            eprintln!("打开本机 Syncthing 管理页");
            Ok(())
        }
        Err(e) => {
            // 窗口没开起来：注销刚才的会话
            if let Some(w) = app.get_webview_window(WINDOW) {
                let _ = w.destroy();
            } else if let Some(s) = lock(&app.state::<SyncthingUiState>().session).take() {
                let _ = gui_session::logout(&s.base, &s.cookie);
            }
            Err(e)
        }
    }
}

/// 服务器管理页窗口的标签：服务器 ID 里窗口标签不允许的字符换成下划线
fn server_window(server_id: &str) -> String {
    let id: String = server_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{SERVER_WINDOW_PREFIX}{id}")
}

/// 准备服务器管理页：经同步隧道用 Pierlink 的口令登录；登录后、窗口打开前记下服务器上的设置，作为关窗后核对的基准
///
/// :return: 服务器名称、管理接口地址、会话 cookie
fn open_server_session(
    app: &AppHandle,
    server_id: &str,
) -> Result<(String, String, GuiCookie), AppError> {
    let opened_ms = now_ms();
    let server = crate::servers::load(app, server_id)?;
    let instance = app
        .state::<DbState>()
        .with(|db| db.get_instance(server_id))?
        .ok_or_else(|| AppError::new("sync.not_deployed").with("server", &server.name))?;
    let client = {
        let state = app.state::<SyncState>();
        let inner = state.lock();
        inner.links.get(server_id).map(|l| l.client().clone())
    }
    .ok_or_else(|| AppError::new("sync.not_connected"))?;
    // 同步程序是别的电脑部署的：这台电脑没有它的口令，或口令对不上
    let no_login = || AppError::new("sync.server_ui_login").with("server", &server.name);
    let password = super::credential_store()?
        .get(&instance.gui_password_cred)?
        .ok_or_else(no_login)?;
    let base = client.base().to_owned();
    let cookie = gui_session::login(&base, "pierlink", &password).map_err(|e| {
        if e.code == "syncthing.unauthorized" {
            no_login()
        } else {
            e
        }
    })?;
    let prepared = (|| {
        let expected = snapshot(app, &client)?;
        let policy = sync::capture_server_hardening_policy(
            &client,
            "pierlink",
            &instance.socket_path,
            instance.sync_port,
        )?;
        Ok::<_, AppError>((expected, policy))
    })();
    let (expected, policy) = match prepared {
        Ok(v) => v,
        Err(e) => {
            let _ = gui_session::logout(&base, &cookie);
            return Err(e);
        }
    };
    lock(&app.state::<SyncthingUiState>().servers).insert(
        server_id.to_owned(),
        ServerSession {
            name: server.name.clone(),
            base: base.clone(),
            cookie: cookie.clone(),
            client,
            expected,
            policy,
            opened_ms,
        },
    );
    Ok((server.name, base, cookie))
}

/// 恢复目录或整个文件夹后，那边的文件夹是暂停的：任务也记成暂停，让用户确认后再点“继续”
///
/// :return: 任务名；找不到任务或写数据库失败时为 None
fn pause_task_of(app: &AppHandle, folder_id: &str) -> Option<String> {
    let tasks = app
        .state::<DbState>()
        .with(|db| db.list_sync_tasks())
        .ok()?;
    let mut task = tasks.into_iter().find(|t| t.folder_id == folder_id)?;
    task.paused = true;
    task.updated_at = now_ms();
    if let Err(e) = app.state::<DbState>().with(|db| db.update_sync_task(&task)) {
        eprintln!("把任务记成暂停失败: 任务={}, 错误={}", task.name, e.code);
        return None;
    }
    Some(task.name)
}

/// 服务器管理页关窗后要不要改回这一项。窗口开着期间 Pierlink 自己或别的电脑也会改服务器上的设置，这些不算管理页里的改动：
/// 设备地址（隧道重连时更新）、这期间在 Pierlink 里编辑、暂停或删除过的任务的文件夹、文件夹只是多了设备（别的电脑加入）。
fn should_restore(change: &ConfigurationChange, tasks: &[SyncTask], opened_ms: i64) -> bool {
    match change.target {
        ConfigTarget::Device => change.field != "addresses",
        ConfigTarget::Folder | ConfigTarget::Ignores => {
            let Some(task) = tasks.iter().find(|t| t.folder_id == change.id) else {
                return false;
            };
            if task.updated_at >= opened_ms {
                return false;
            }
            if change.field == "devices" {
                let ids = |v: &Value| -> HashSet<String> {
                    v.as_array()
                        .map(|list| {
                            list.iter()
                                .filter_map(|d| d["deviceID"].as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                return !ids(&change.expected).is_subset(&ids(&change.actual));
            }
            true
        }
    }
}

/// 服务器管理页关了：注销、删 cookie，按打开时的基准核对服务器上的设置，被改的直接改回；有改回的告诉界面
fn on_server_closed(app: AppHandle, server_id: String) {
    let Some(session) = lock(&app.state::<SyncthingUiState>().servers).remove(&server_id) else {
        return;
    };
    if let Err(e) = gui_session::logout(&session.base, &session.cookie) {
        eprintln!(
            "注销服务器 Syncthing 管理页会话失败: 服务器={}, 错误={}",
            session.name, e.code
        );
    }
    if let Some(main) = app.get_webview_window("main")
        && let Err(e) = main.delete_cookie(to_cookie(&session.cookie))
    {
        eprintln!("删除服务器管理页 cookie 失败: 错误={e}");
    }
    // 隧道重连过时本机端口会变：优先用现在的管理接口
    let client = {
        let state = app.state::<SyncState>();
        let inner = state.lock();
        inner.links.get(&server_id).map(|l| l.client().clone())
    }
    .unwrap_or_else(|| session.client.clone());
    let report = match sync::reconcile_configuration(&client, &session.policy, &session.expected) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "核对服务器 Syncthing 配置失败: 服务器={}, 错误={}",
                session.name, e.code
            );
            return;
        }
    };
    let tasks = app
        .state::<DbState>()
        .with(|db| db.list_sync_tasks())
        .unwrap_or_default();
    let mut restored = 0u32;
    let mut unrestored = 0u32;
    let mut skipped = 0usize;
    let mut paused_folder = None;
    for change in &report.task_changes {
        if !should_restore(change, &tasks, session.opened_ms) {
            skipped += 1;
            continue;
        }
        match sync::restore_configuration_change(&client, &session.expected, change) {
            Ok(r) => {
                restored += 1;
                if r.paused_for_rejoin {
                    paused_folder = Some(change.id.clone());
                }
            }
            Err(e) => {
                unrestored += 1;
                eprintln!(
                    "改回服务器上被改的设置失败: 服务器={}, 对象={}, 字段={}, 错误={}",
                    session.name, change.id, change.field, e.code
                );
            }
        }
    }
    let paused_task = paused_folder.and_then(|id| pause_task_of(&app, &id));
    // 服务器上新加的文件夹、设备不列：几台电脑共用一个服务器时分不清是谁加的，Pierlink 也不会动它们
    eprintln!(
        "核对服务器 Syncthing 配置: 服务器={}, 恢复加固项={}, 改回任务设置={restored}, 没能改回={unrestored}, 不算管理页改动的={skipped}",
        session.name,
        report.hardened.len()
    );
    if report.hardened.is_empty() && restored == 0 && unrestored == 0 && paused_task.is_none() {
        return;
    }
    let dto = ConfigReportDto {
        hardened: report.hardened,
        changes: Vec::new(),
        foreign: Vec::new(),
        paused_task,
        server: Some(session.name),
        restored,
        unrestored,
    };
    // 留一份：用户不在同步页时，下次进同步页再看（任务被暂停了也要让用户知道）
    lock(&app.state::<SyncthingUiState>().server_pending).push(dto.clone());
    if let Err(e) = app.emit(CONFIG_REPORT_EVENT, dto) {
        eprintln!("推送服务器配置核对结果失败: 错误={e}");
    }
}

/// 在应用内窗口打开一台服务器的 Syncthing 管理页（经同步隧道，自动登录）。这台的已经开着就切到前面。
/// 关窗后核对服务器上的设置：被改的安全设置和同步任务设置都改回 Pierlink 的，改回了几项告诉界面。
///
/// :param server_id: 服务器 ID
/// :return: 服务器没连上返回 sync.not_connected；这台电脑没有它的登录口令（同步程序是别的电脑部署的）返回 sync.server_ui_login
#[tauri::command]
#[specta::specta]
pub async fn sync_open_server_syncthing_ui(
    app: AppHandle,
    server_id: String,
) -> Result<(), AppError> {
    let label = server_window(&server_id);
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let handle = app.clone();
    let id = server_id.clone();
    let (name, base, cookie) =
        tauri::async_runtime::spawn_blocking(move || open_server_session(&handle, &id))
            .await
            .map_err(failed)??;
    let result = (|| -> Result<(), AppError> {
        let window = WebviewWindowBuilder::new(
            &app,
            &label,
            WebviewUrl::External("about:blank".parse().map_err(failed)?),
        )
        .title(format!("Syncthing · {name} · Pierlink"))
        .inner_size(1180.0, 820.0)
        .min_inner_size(720.0, 480.0)
        .visible(false)
        .build()
        .map_err(failed)?;
        let app2 = app.clone();
        let id = server_id.clone();
        window.on_window_event(move |e| {
            if let WindowEvent::Destroyed = e {
                let app = app2.clone();
                let id = id.clone();
                std::thread::spawn(move || on_server_closed(app, id));
            }
        });
        window.set_cookie(to_cookie(&cookie)).map_err(failed)?;
        window
            .navigate(format!("{base}/").parse().map_err(failed)?)
            .map_err(failed)?;
        window.show().map_err(failed)?;
        window.set_focus().map_err(failed)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            eprintln!("打开服务器的 Syncthing 管理页: 服务器={name}");
            Ok(())
        }
        Err(e) => {
            // 窗口没开起来：关窗处理会注销；窗口都没建成时在这里注销
            if let Some(w) = app.get_webview_window(&label) {
                let _ = w.destroy();
            } else if let Some(s) =
                lock(&app.state::<SyncthingUiState>().servers).remove(&server_id)
            {
                let _ = gui_session::logout(&s.base, &s.cookie);
            }
            Err(e)
        }
    }
}

/// 还没处理完的核对结果（界面重新加载、进同步页时取回）：本机的优先，其次是服务器管理页最近的一条
#[tauri::command]
#[specta::specta]
pub async fn sync_config_report(app: AppHandle) -> Result<Option<ConfigReportDto>, AppError> {
    let ui = app.state::<SyncthingUiState>();
    if let Some((_, r)) = lock(&ui.pending).as_ref() {
        return Ok(Some(report_dto(&app, r)));
    }
    Ok(lock(&ui.server_pending).last().cloned())
}

/// 把采用的新值写进任务设置（Pierlink 没存的字段不用写）
fn save_adopted(app: &AppHandle, change: &ConfigurationChange) -> Result<(), AppError> {
    let tasks = app.state::<DbState>().with(|db| db.list_sync_tasks())?;
    let Some(mut task) = tasks.into_iter().find(|t| t.folder_id == change.id) else {
        return Ok(());
    };
    let v = &change.actual;
    match (change.target, change.field.as_str()) {
        (ConfigTarget::Ignores, _) => {
            let rules: Vec<String> = serde_json::from_value(v.clone()).map_err(failed)?;
            task.ignore_rules = rules.join("\n");
        }
        (_, "fsWatcherDelayS") => task.watch_delay_s = v.as_f64().unwrap_or(10.0) as i64,
        (_, "paused") => task.paused = v.as_bool().unwrap_or(task.paused),
        (_, "type") => {
            task.local_mode = match v.as_str() {
                Some("receiveonly") => EndpointMode::ReceiveOnly,
                _ => EndpointMode::SendReceive,
            }
        }
        (_, "versioning") => {
            let on = v["type"].as_str().is_some_and(|t| !t.is_empty());
            task.versioning_enabled = on;
            if let Some(days) = v["params"]["cleanoutDays"]
                .as_str()
                .and_then(|d| d.parse::<i64>().ok())
            {
                task.versioning_days = days;
            }
        }
        _ => return Ok(()),
    }
    task.updated_at = now_ms();
    app.state::<DbState>().with(|db| db.update_sync_task(&task))
}

/// 处理一项被改过的任务设置：采用新值（写进 Pierlink 的任务设置）或恢复成 Pierlink 的值
///
/// :param index: 核对结果里的位置
/// :param adopt: true 采用新值，false 恢复
/// :return: 剩下没处理的结果；全部处理完为 null
#[tauri::command]
#[specta::specta]
pub async fn sync_config_resolve(
    app: AppHandle,
    index: u32,
    adopt: bool,
) -> Result<Option<ConfigReportDto>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let ui = app.state::<SyncthingUiState>();
        let (expected, change) = {
            let pending = lock(&ui.pending);
            let (expected, report) = pending
                .as_ref()
                .ok_or_else(|| AppError::new("sync.configuration_changed"))?;
            let change = report
                .task_changes
                .get(index as usize)
                .cloned()
                .ok_or_else(|| AppError::new("sync.configuration_changed"))?;
            (expected.clone(), change)
        };
        let mut paused = false;
        {
            let state = app.state::<SyncState>();
            let mut inner = state.lock();
            let client = super::ensure_local(&app, &mut inner)?.client().clone();
            if adopt {
                if !adoptable(&change) {
                    return Err(AppError::new("sync.configuration_not_adoptable")
                        .with("field", &change.field));
                }
                sync::adopt_configuration_change(&client, &expected, &change)?;
                save_adopted(&app, &change)?;
            } else {
                paused = sync::restore_configuration_change(&client, &expected, &change)?
                    .paused_for_rejoin;
            }
        }
        // 恢复目录或整个文件夹后 Syncthing 里是暂停的：数据库也记成暂停，让用户确认后再继续
        let paused_task = if paused {
            let tasks = app.state::<DbState>().with(|db| db.list_sync_tasks())?;
            match tasks.into_iter().find(|t| t.folder_id == change.id) {
                Some(mut t) => {
                    t.paused = true;
                    t.updated_at = now_ms();
                    app.state::<DbState>().with(|db| db.update_sync_task(&t))?;
                    Some(t.name)
                }
                None => None,
            }
        } else {
            None
        };
        eprintln!(
            "处理管理页改过的设置: 对象={}, 字段={}, 采用={adopt}",
            change.id, change.field
        );
        let mut pending = lock(&ui.pending);
        let Some((expected, report)) = pending.as_mut() else {
            return Ok(None);
        };
        if adopt {
            absorb(expected, &change);
        }
        report.task_changes.remove(index as usize);
        // 加固项和外来项第一次已经告诉过用户，剩下的只留任务改动
        report.hardened.clear();
        report.foreign.clear();
        let mut dto = report_dto(&app, report);
        dto.paused_task = paused_task;
        if report.task_changes.is_empty() {
            *pending = None;
            // 全部处理完：有暂停提示时还要告诉界面
            return Ok(dto.paused_task.is_some().then_some(dto));
        }
        Ok(Some(dto))
    })
    .await
    .map_err(failed)?
}

/// 用户看过了核对结果。本机的：只剩加固项、外来项时清掉，还有没处理的任务改动时保留（下次进同步页再提示）；
/// 服务器管理页的：只弹最近的一条，看过就全部清掉
///
/// :param server: 看过的是不是服务器管理页的结果
#[tauri::command]
#[specta::specta]
pub async fn sync_config_dismiss(app: AppHandle, server: bool) -> Result<(), AppError> {
    let ui = app.state::<SyncthingUiState>();
    if server {
        lock(&ui.server_pending).clear();
        return Ok(());
    }
    let mut pending = lock(&ui.pending);
    if pending
        .as_ref()
        .is_some_and(|(_, r)| r.task_changes.is_empty())
    {
        *pending = None;
    } else if let Some((_, r)) = pending.as_mut() {
        r.hardened.clear();
        r.foreign.clear();
    }
    Ok(())
}
