//! 窗口外壳：托盘、关闭窗口缩到托盘、退出确认、单实例。
//!
//! 规则（设计 1.4“关闭窗口 / 退出”，异常清单 L2、L3、L11）：
//! - 关闭窗口 = 缩到托盘，同步和终端继续运行（设置里可关掉，关掉后关闭窗口 = 退出）；
//! - 退出应用 = 本机同步停止、终端全部关闭；退出前由界面显示确认框（未同步文件数、打开的终端数）；
//! - 托盘建立失败时不隐藏窗口（否则窗口藏起来就再也找不回），关闭窗口按退出处理；
//! - 界面没有响应退出请求（页面卡死、崩溃）时，几秒后直接退出，保证用户总能关掉应用；
//! - 再次启动应用只会激活已有窗口。

use crate::sync::{self, ExitTaskDto};
use crate::terminal::TerminalState;
use pierlink_core::error::AppError;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Window, Wry};

/// 主窗口标签
const MAIN: &str = "main";
/// 通知界面“用户要退出”的事件名，载荷是退出请求编号
pub const EXIT_EVENT: &str = "pierlink://exit-requested";
/// 界面多久没有确认收到退出请求就直接退出
const EXIT_ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// 托盘菜单文字的最大长度（界面传入，防止异常数据）
const LABEL_MAX: usize = 64;
/// 通知界面切换页面的事件名，载荷是页面名（如 "sync"）
pub const OPEN_PAGE_EVENT: &str = "pierlink://open-page";
/// 托盘同步状态多久刷新一次
const TRAY_REFRESH: Duration = Duration::from_secs(8);

/// 托盘菜单的文字（随界面语言）。状态一行是模板：`{count}` 任务数，`{n}` 同步中/有问题的个数
#[derive(Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TrayLabels {
    pub open: String,
    pub quit: String,
    pub sync_page: String,
    pub pause_all: String,
    pub resume_all: String,
    pub status_none: String,
    pub status_synced: String,
    pub status_syncing: String,
    pub status_paused: String,
    pub status_problem: String,
    pub status_stopped: String,
    pub status_unavailable: String,
}

impl Default for TrayLabels {
    fn default() -> Self {
        Self {
            open: "打开 Pierlink".into(),
            quit: "退出 Pierlink".into(),
            sync_page: "打开同步页".into(),
            pause_all: "暂停全部同步".into(),
            resume_all: "继续全部同步".into(),
            status_none: "同步：没有任务".into(),
            status_synced: "同步：{count} 个任务 · 全部已同步".into(),
            status_syncing: "同步：{count} 个任务 · {n} 个同步中".into(),
            status_paused: "同步：{count} 个任务 · 全部已暂停".into(),
            status_problem: "同步：{count} 个任务 · {n} 个有问题".into(),
            status_stopped: "同步：本机同步服务没有运行".into(),
            status_unavailable: "同步：数据暂不可用".into(),
        }
    }
}

/// 托盘菜单项（句柄，复制很便宜）
#[derive(Clone)]
struct TrayItems {
    open: MenuItem<Wry>,
    quit: MenuItem<Wry>,
    status: MenuItem<Wry>,
    pause: MenuItem<Wry>,
    sync_page: MenuItem<Wry>,
}

/// 菜单文字：去掉首尾空白、限制长度；空的用默认值
fn label(text: &str, fallback: &str) -> String {
    let t: String = text.trim().chars().take(LABEL_MAX).collect();
    if t.is_empty() { fallback.to_owned() } else { t }
}

/// 同步摘要 → 状态一行的文字，以及“暂停/继续全部”该显示哪个
fn status_text(l: &TrayLabels, s: &sync::TraySummary) -> (String, bool) {
    let fill = |tpl: &str, n: usize| {
        tpl.replace("{count}", &s.tasks.to_string())
            .replace("{n}", &n.to_string())
    };
    let all_paused = s.tasks > 0 && s.paused == s.tasks;
    let text = if s.tasks == 0 {
        l.status_none.clone()
    } else if all_paused {
        fill(&l.status_paused, 0)
    } else if s.stopped {
        l.status_stopped.clone()
    } else if s.problems > 0 {
        fill(&l.status_problem, s.problems)
    } else if s.syncing > 0 {
        fill(&l.status_syncing, s.syncing)
    } else {
        fill(&l.status_synced, 0)
    };
    (text, all_paused)
}

pub struct ShellState {
    /// 关闭窗口时缩到托盘（界面启动后按设置同步过来）
    close_to_tray: AtomicBool,
    /// 界面已同步过设置。之前一律不缩到托盘：用户可能设置了“关闭即退出”
    configured: AtomicBool,
    /// 托盘图标已建立
    tray_ready: AtomicBool,
    /// 已确认退出：之后的关闭请求一律放行
    quitting: AtomicBool,
    /// 最近一次退出请求的编号
    exit_request: AtomicU64,
    /// 界面已确认收到的退出请求编号
    exit_acked: AtomicU64,
    /// 托盘菜单项（切换语言、同步状态变化时改文字）
    items: Mutex<Option<TrayItems>>,
    /// 托盘菜单文字
    labels: Mutex<TrayLabels>,
    /// 最近一次的同步摘要（语言变化时立即用它重写状态一行）
    summary: Mutex<Option<sync::TraySummary>>,
}

impl Default for ShellState {
    fn default() -> Self {
        Self {
            close_to_tray: AtomicBool::new(true),
            configured: AtomicBool::new(false),
            tray_ready: AtomicBool::new(false),
            quitting: AtomicBool::new(false),
            exit_request: AtomicU64::new(0),
            exit_acked: AtomicU64::new(0),
            items: Mutex::new(None),
            labels: Mutex::new(TrayLabels::default()),
            summary: Mutex::new(None),
        }
    }
}

/// 显示并激活主窗口（托盘点击、托盘菜单“打开”、再次启动应用时）。
///
/// :param app: 应用句柄
pub fn show_main(app: &AppHandle) {
    let Some(w) = app.get_webview_window(MAIN) else {
        eprintln!("显示主窗口失败: 原因=找不到主窗口");
        return;
    };
    // 每一步失败都不影响后续步骤
    if let Err(e) = w.unminimize() {
        eprintln!("还原主窗口失败: 错误={e}");
    }
    if let Err(e) = w.show() {
        eprintln!("显示主窗口失败: 错误={e}");
    }
    if let Err(e) = w.set_focus() {
        eprintln!("激活主窗口失败: 错误={e}");
    }
}

/// 建立托盘图标和菜单。失败时记录原因，应用照常运行（关闭窗口按退出处理）。
///
/// :param app: 应用句柄
pub fn setup_tray(app: &AppHandle) {
    match build_tray(app) {
        Ok(()) => {
            app.state::<ShellState>()
                .tray_ready
                .store(true, Ordering::SeqCst);
        }
        Err(e) => {
            eprintln!("建立托盘图标失败，关闭窗口将直接退出: 错误={e}");
            return;
        }
    }
    // 定时刷新托盘里的同步状态
    let app = app.clone();
    std::thread::spawn(move || {
        loop {
            refresh_tray(&app);
            std::thread::sleep(TRAY_REFRESH);
        }
    });
}

/// 重新查询同步摘要并更新托盘菜单（同步服务正忙时沿用上次的结果）
pub fn refresh_tray(app: &AppHandle) {
    if let Some(s) = sync::tray_summary(app) {
        *app.state::<ShellState>()
            .summary
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(s);
    }
    apply_tray_text(app);
}

/// 按当前文字和最近的摘要写托盘菜单
fn apply_tray_text(app: &AppHandle) {
    let state = app.state::<ShellState>();
    let labels = state
        .labels
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let summary = *state.summary.lock().unwrap_or_else(|p| p.into_inner());
    // 先把菜单项句柄复制出来再放开锁：在非主线程里改菜单文字要等主线程执行，
    // 持着锁等会和主线程上的 app_shell_config（也要这把锁）互相等待，整个应用卡死
    let Some(items) = state
        .items
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
    else {
        return;
    };
    let d = TrayLabels::default();
    let mut texts = vec![
        (&items.open, label(&labels.open, &d.open)),
        (&items.quit, label(&labels.quit, &d.quit)),
        (&items.sync_page, label(&labels.sync_page, &d.sync_page)),
    ];
    // 数据库打不开（含数据搬家需要人工恢复）时托盘不会有摘要：显示“同步：数据暂不可用”，不要一直停在“正在读取”
    if summary.is_none()
        && app
            .try_state::<crate::data::DbState>()
            .is_some_and(|db| db.with(|_| Ok(())).is_err())
    {
        texts.push((
            &items.status,
            label(&labels.status_unavailable, &d.status_unavailable),
        ));
    }
    if let Some(s) = summary {
        let (status, all_paused) = status_text(&labels, &s);
        texts.push((&items.status, label(&status, &d.status_none)));
        let pause = if all_paused {
            &labels.resume_all
        } else {
            &labels.pause_all
        };
        let fallback = if all_paused {
            &d.resume_all
        } else {
            &d.pause_all
        };
        texts.push((&items.pause, label(pause, fallback)));
        if let Err(e) = items.pause.set_enabled(s.tasks > 0) {
            eprintln!("更新托盘菜单失败: 错误={e}");
        }
    }
    for (item, text) in texts {
        if let Err(e) = item.set_text(text) {
            eprintln!("更新托盘菜单文字失败: 错误={e}");
        }
    }
}

/// 托盘“暂停全部 / 继续全部”：全部已暂停时继续，否则全部暂停
fn toggle_all_paused(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let summary = tauri::async_runtime::spawn_blocking({
            let app = app.clone();
            move || sync::tray_summary(&app)
        })
        .await
        .ok()
        .flatten();
        let Some(s) = summary else {
            return;
        };
        let pause = !(s.tasks > 0 && s.paused == s.tasks);
        sync::set_all_paused(app.clone(), pause).await;
        let _ = tauri::async_runtime::spawn_blocking(move || refresh_tray(&app)).await;
    });
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    // 默认文字用中文；界面启动后按当前语言更新
    let d = TrayLabels::default();
    let open = MenuItem::with_id(app, "open", &d.open, true, None::<&str>)?;
    // 状态一行只显示，不能点
    let status = MenuItem::with_id(app, "status", "同步：正在读取…", false, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", &d.pause_all, false, None::<&str>)?;
    let sync_page = MenuItem::with_id(app, "sync", &d.sync_page, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", &d.quit, true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &PredefinedMenuItem::separator(app)?,
            &status,
            &pause,
            &sync_page,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("Pierlink")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main(app),
            "quit" => request_exit(app),
            "pause" => toggle_all_paused(app),
            "sync" => {
                show_main(app);
                if let Err(e) = app.emit_to(MAIN, OPEN_PAGE_EVENT, "sync") {
                    eprintln!("通知界面打开同步页失败: 错误={e}");
                }
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 左键单击或双击都打开窗口；右键弹出菜单（系统处理）
            let open = matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            );
            if open {
                show_main(tray.app_handle());
            }
        });
    // 没有图标时托盘区会出现一个看不见的空位，窗口缩进去就找不回来，所以不建托盘
    let Some(icon) = app.default_window_icon() else {
        return Err(tauri::Error::InvalidIcon(std::io::Error::other(
            "缺少应用图标",
        )));
    };
    builder = builder.icon(icon.clone());
    builder.build(app)?;
    *app.state::<ShellState>()
        .items
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(TrayItems {
        open,
        quit,
        status,
        pause,
        sync_page,
    });
    Ok(())
}

/// 窗口的关闭请求（标题栏关闭按钮、Alt+F4、任务栏“关闭窗口”）。
///
/// :param window: 收到请求的窗口
/// :param api: 用来阻止关闭
pub fn on_close_requested(window: &Window, api: &tauri::CloseRequestApi) {
    if window.label() != MAIN {
        return;
    }
    let app = window.app_handle();
    let state = app.state::<ShellState>();
    if state.quitting.load(Ordering::SeqCst) {
        return;
    }
    api.prevent_close();
    if state.configured.load(Ordering::SeqCst)
        && state.close_to_tray.load(Ordering::SeqCst)
        && state.tray_ready.load(Ordering::SeqCst)
    {
        if let Err(e) = window.hide() {
            // 藏不起来就按退出处理，不能让关闭按钮失效
            eprintln!("缩到托盘失败，改为退出确认: 错误={e}");
            request_exit(app);
        }
        return;
    }
    request_exit(app);
}

/// 发起退出：显示窗口并让界面弹出确认框。界面在限定时间内没有确认收到时直接退出。
///
/// :param app: 应用句柄
pub fn request_exit(app: &AppHandle) {
    let state = app.state::<ShellState>();
    if state.quitting.load(Ordering::SeqCst) {
        return;
    }
    show_main(app);
    let id = state.exit_request.fetch_add(1, Ordering::SeqCst) + 1;
    if let Err(e) = app.emit_to(MAIN, EXIT_EVENT, id) {
        eprintln!("通知界面退出失败，直接退出: 错误={e}");
        quit(app);
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(EXIT_ACK_TIMEOUT);
        let state = app.state::<ShellState>();
        if state.exit_acked.load(Ordering::SeqCst) < id && !state.quitting.load(Ordering::SeqCst) {
            eprintln!("界面没有响应退出请求，直接退出: 请求={id}");
            quit(&app);
        }
    });
}

/// 退出应用（清理工作在 `RunEvent::Exit` 里完成）
fn quit(app: &AppHandle) {
    app.state::<ShellState>()
        .quitting
        .store(true, Ordering::SeqCst);
    app.exit(0);
}

/// 界面把相关设置同步给外壳：关闭窗口时是否缩到托盘、托盘菜单的文字（随界面语言）。
///
/// :param close_to_tray: 关闭窗口时缩到托盘
/// :param labels: 托盘菜单的文字
#[tauri::command]
#[specta::specta]
pub fn app_shell_config(app: AppHandle, close_to_tray: bool, labels: TrayLabels) {
    let state = app.state::<ShellState>();
    state.close_to_tray.store(close_to_tray, Ordering::SeqCst);
    state.configured.store(true, Ordering::SeqCst);
    *state.labels.lock().unwrap_or_else(|p| p.into_inner()) = labels;
    apply_tray_text(&app);
}

/// 托盘是否可用（界面据此决定设置项是否可选、退出确认框里是否提供“缩到托盘”）
#[tauri::command]
#[specta::specta]
pub fn app_tray_available(app: AppHandle) -> bool {
    app.state::<ShellState>().tray_ready.load(Ordering::SeqCst)
}

/// 界面确认收到退出请求（之后由界面决定退出或取消）
///
/// :param request: 事件里的请求编号
#[tauri::command]
#[specta::specta]
pub fn app_exit_ack(app: AppHandle, request: f64) {
    let state = app.state::<ShellState>();
    // 只接受已经发出的编号：否则一个很大的编号会让之后所有退出请求都被当成“已确认”，看门狗失效
    if !(request.is_finite() && request >= 0.0)
        || request as u64 > state.exit_request.load(Ordering::SeqCst)
    {
        return;
    }
    state.exit_acked.fetch_max(request as u64, Ordering::SeqCst);
}

/// 缩到托盘（退出确认框里的“缩到托盘”）。托盘不可用时不做任何事。
#[tauri::command]
#[specta::specta]
pub fn app_hide_to_tray(app: AppHandle) {
    if !app.state::<ShellState>().tray_ready.load(Ordering::SeqCst) {
        return;
    }
    if let Some(w) = app.get_webview_window(MAIN)
        && let Err(e) = w.hide()
    {
        eprintln!("缩到托盘失败: 错误={e}");
    }
}

/// 退出前的摘要
#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExitSummaryDto {
    /// 正在运行的终端会话数（退出时全部结束）
    pub terminals: f64,
    /// 还有文件没同步完的任务
    pub tasks: Vec<ExitTaskDto>,
    /// 部分任务的状态没查到（同步服务正忙或无响应）
    pub sync_unknown: bool,
}

/// 退出前的摘要：打开的终端数、各任务未同步的文件数。
#[tauri::command]
#[specta::specta]
pub async fn app_exit_summary(app: AppHandle) -> Result<ExitSummaryDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let terminals = app.state::<TerminalState>().open_count() as f64;
        let (tasks, sync_unknown) = sync::exit_summary(&app);
        ExitSummaryDto {
            terminals,
            tasks,
            sync_unknown,
        }
    })
    .await
    .map_err(|e| AppError::new("app.exit_summary_failed").with("reason", e.to_string()))
}

/// 用户确认退出
#[tauri::command]
#[specta::specta]
pub fn app_quit(app: AppHandle) {
    quit(&app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 托盘状态文字() {
        let l = TrayLabels::default();
        let s = |tasks, paused, syncing, problems, stopped| sync::TraySummary {
            tasks,
            paused,
            syncing,
            problems,
            stopped,
        };
        assert_eq!(
            status_text(&l, &s(0, 0, 0, 0, false)),
            ("同步：没有任务".into(), false)
        );
        assert!(status_text(&l, &s(3, 3, 0, 0, false)).1);
        assert_eq!(
            status_text(&l, &s(3, 3, 0, 0, false)).0,
            "同步：3 个任务 · 全部已暂停"
        );
        assert_eq!(
            status_text(&l, &s(3, 0, 0, 0, true)).0,
            "同步：本机同步服务没有运行"
        );
        assert_eq!(
            status_text(&l, &s(3, 1, 1, 1, false)).0,
            "同步：3 个任务 · 1 个有问题"
        );
        assert_eq!(
            status_text(&l, &s(3, 0, 2, 0, false)).0,
            "同步：3 个任务 · 2 个同步中"
        );
        assert_eq!(
            status_text(&l, &s(2, 0, 0, 0, false)).0,
            "同步：2 个任务 · 全部已同步"
        );
        assert_eq!(label("   ", "默认"), "默认");
        assert_eq!(label(&"长".repeat(100), "x").chars().count(), LABEL_MAX);
    }
}
