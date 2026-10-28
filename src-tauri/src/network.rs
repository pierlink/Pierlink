//! 网络断开/恢复（阶段 5 断网重连）：
//! - Windows 网络连通性变化时通知界面（界面显示“本机网络已断开 / 网络已恢复”）；
//! - 从“没有网络”恢复时立即唤醒等待重连的同步隧道，不用等重连间隔。
//!
//! 终端断开后不自动重连（设计 3.9），由用户按 Enter。

use pierlink_core::connectivity::{
    Connectivity, ConnectivityWatch, current_connectivity, watch_connectivity,
};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// 通知界面网络变化的事件名，载荷见 [`name`]
pub const NETWORK_EVENT: &str = "pierlink://network";

/// 订阅句柄（释放即取消）和上一次的等级
#[derive(Default)]
pub struct NetworkState {
    watch: Mutex<Option<ConnectivityWatch>>,
    last: Mutex<Option<Connectivity>>,
}

/// 界面用的名字
fn name(c: Connectivity) -> &'static str {
    match c {
        Connectivity::None => "none",
        Connectivity::LocalOnly => "localOnly",
        Connectivity::Internet => "internet",
        Connectivity::Unknown => "unknown",
    }
}

/// 开始监听网络变化（启动时调用一次；失败只记日志，同步隧道仍按自己的间隔重连）
pub fn start(app: &AppHandle) {
    // 记下启动时的状态：断网时打开应用、之后恢复，也要立即唤醒同步隧道
    let state = NetworkState::default();
    *state.last.lock().unwrap_or_else(|p| p.into_inner()) = Some(current_connectivity());
    app.manage(state);
    let handle = app.clone();
    match watch_connectivity(move |c| on_change(&handle, c)) {
        Ok(w) => {
            *app.state::<NetworkState>()
                .watch
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(w);
        }
        Err(e) => eprintln!("监听网络变化失败: 错误={e:?}"),
    }
}

fn on_change(app: &AppHandle, c: Connectivity) {
    let state = app.state::<NetworkState>();
    let prev = state
        .last
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .replace(c);
    eprintln!("网络变化: 之前={prev:?}, 现在={c:?}");
    if let Err(e) = app.emit(NETWORK_EVENT, name(c)) {
        eprintln!("通知界面网络变化失败: 错误={e}");
    }
    if prev == Some(Connectivity::None) && c != Connectivity::None {
        crate::tunnels::network_restored(app);
        crate::monitor::network_restored(app);
        if let Some(state) = app.try_state::<crate::builtin::BuiltinState>() {
            state.network_restored();
        }
    }
}

/// 当前网络连通性（界面启动时查一次）：none / localOnly / internet / unknown
#[tauri::command]
#[specta::specta]
pub fn app_connectivity() -> String {
    name(current_connectivity()).into()
}
