//! pierlink_status 工具：Pierlink 自己的基本情况，给 AI 排查连接、装桌面、同步这类问题用。
//! 只给名字和状态：不给地址、端口、用户名、密钥和密码、令牌、备注、路径（同步任务的报错只给条数）。
use crate::data::DbState;
use pierlink_core::error::AppError;
use pierlink_core::storage::Engine;
use serde_json::{Value, json};
use std::collections::HashMap;
use tauri::{AppHandle, Manager};

/// 收集基本情况
///
/// :param app: 应用句柄
/// :return: {"servers", "terminals", "desktops_open", "sync", "mcp_entries"}
pub(super) fn collect(app: &AppHandle) -> Result<Value, AppError> {
    let servers = app.state::<DbState>().with(|db| db.list_servers())?;
    let names: HashMap<&str, &str> = servers
        .iter()
        .map(|s| (s.id.as_str(), s.name.as_str()))
        .collect();
    let name_of = |id: &str| names.get(id).copied().unwrap_or("（已删除的服务器）");
    let builtin = app.try_state::<crate::builtin::BuiltinState>();
    let (desktops_open, systems) = app
        .try_state::<crate::desktop::DesktopState>()
        .map(|state| state.summary())
        .unwrap_or_default();
    let server_list: Vec<Value> = servers
        .iter()
        .map(|s| {
            // 系统 OpenSSH 不保持连接：不知道在不在线
            let online = (s.connection_engine == Engine::Builtin)
                .then(|| builtin.as_ref().and_then(|b| b.online(&s.id)))
                .flatten();
            json!({
                "name": s.name,
                "group": s.group_name,
                "engine": s.connection_engine,
                "auth": s.auth_method,
                "password_saved": s.remember_password && s.password_cred.is_some(),
                "jump": s.jump_server_id.as_deref().map(name_of),
                "tmux": s.use_tmux,
                "online": online,
                "system": systems.get(&s.id),
            })
        })
        .collect();
    let terminals: Vec<Value> = app
        .state::<crate::terminal::TerminalState>()
        .metas()
        .into_iter()
        .map(|(_, m)| {
            json!({
                "title": m.title,
                "server": m.server_id.as_deref().map(name_of),
                "shell": m.shell,
                "ai": m.ai_opened,
            })
        })
        .collect();
    let sync: Vec<Value> = crate::sync::mcp_task_states(app, None)
        .unwrap_or_default()
        .into_iter()
        .map(|t| {
            json!({
                "name": t.name,
                "state": t.state,
                "paused": t.paused,
                "need_bytes": t.need_bytes,
                "errors": t.errors.len(),
                "servers": t.servers.iter().map(|s| json!({
                    "name": s.name,
                    "connected": s.connected,
                    "completion": s.completion,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let entries: Vec<Value> = app
        .try_state::<super::McpState>()
        .map(|state| state.entries.states())
        .unwrap_or_default()
        .into_iter()
        .map(|e| {
            json!({
                "server": name_of(&e.server_id),
                "state": e.state,
                "port": e.secondary.as_ref().map_or(e.port, |s| s.port),
                "via": e.secondary.as_ref().and_then(|s| s.via.clone()),
                "error": e.error.as_ref().map(|error| error.code.clone()),
            })
        })
        .collect();
    Ok(json!({
        "servers": server_list,
        "terminals": terminals,
        "desktops_open": desktops_open,
        "sync": sync,
        "mcp_entries": entries,
    }))
}
