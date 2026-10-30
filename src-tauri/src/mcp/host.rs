//! core 的 MCP 服务需要的宿主能力：权限和目标来自设置与数据库，确认和打开终端交给界面，
//! 服务器命令和同步状态走现有的连接和同步模块。
use super::settings;
use super::waiters::Waiters;
use super::{
    CONFIRM_CLOSED_EVENT, CONFIRM_EVENT, NOTICE_EVENT, OPEN_TERMINAL_EVENT, SESSIONS_EVENT,
};
use crate::data::DbState;
use pierlink_core::error::AppError;
use pierlink_core::mcp::{
    Confirmation, Entry, HostEvent, McpHost, Permissions, RunLimits, RunOutput, SessionInfo,
    SyncTaskState, Target, TargetKind, TerminalOpenRequest, UserTerminal,
};
use pierlink_core::storage::Server;
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// 确认框等多久（不答按拒绝）
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);
/// 界面打开 AI 终端最多等多久（可能要确认指纹、输密码）
const OPEN_TERMINAL_TIMEOUT: Duration = Duration::from_secs(120);

/// 请用户确认的内容
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpConfirmDto {
    pub id: f64,
    /// connect：第一次连接；dangerous：危险命令或危险的文件写入；action：“每次都问”方式下的普通操作
    pub kind: String,
    /// 入口："windows"（本机入口），或某台服务器的名称
    pub entry: String,
    /// 客户端自报的名称和版本（不可信，只做显示）
    pub client: String,
    /// connect：这个连接能操作的目标
    pub targets: Vec<String>,
    /// dangerous：在哪台机器上
    pub target: Option<String>,
    /// dangerous：要执行的命令或要写的文件
    pub action: Option<String>,
}

/// 请界面打开一个 AI 终端
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpOpenTerminalDto {
    pub id: f64,
    /// windows（本机 PowerShell）或 server
    pub kind: String,
    pub server_id: Option<String>,
    pub title: Option<String>,
}

/// 推给界面的提示
#[derive(Serialize, Type, Clone, Debug)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum McpNoticeDto {
    /// 服务器要密码或验证码才能连，AI 的操作没法继续：请用户登录
    #[serde(rename_all = "camelCase")]
    LoginNeeded { server_id: String },
    /// AI 终端在等密码：请用户在这个终端里输入
    #[serde(rename_all = "camelCase")]
    PasswordPrompt { terminal_id: String },
    /// AI 打开的终端
    #[serde(rename_all = "camelCase")]
    TerminalOpened { terminal_id: String },
    /// AI 终端已结束或被关掉
    #[serde(rename_all = "camelCase")]
    TerminalClosed { terminal_id: String },
    /// 用户终端已交给某个入口的 AI
    #[serde(rename_all = "camelCase")]
    TerminalHandedOver {
        terminal_id: String,
        /// "windows" 或服务器 ID
        entry: String,
        /// 入口说明："windows"（界面翻译）或服务器名称
        entry_label: String,
    },
    /// 用户终端已收回（终端还开着）
    #[serde(rename_all = "camelCase")]
    TerminalTakenBack { terminal_id: String },
}

/// Tauri 这边的宿主
pub struct TauriHost {
    app: AppHandle,
    pub(super) confirms: Waiters<McpConfirmDto, Confirmation>,
    pub(super) terminals: Waiters<McpOpenTerminalDto, Result<String, AppError>>,
}

impl TauriHost {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            confirms: Waiters::default(),
            terminals: Waiters::default(),
        }
    }

    /// 入口说明（给界面用）
    pub(super) fn label_of(&self, entry: &Entry) -> String {
        self.entry_label(entry)
    }

    /// 入口能看到的目标（和 AI 调工具时一样）
    pub(super) fn targets_for(
        &self,
        entry: &Entry,
        permissions: &Permissions,
    ) -> Result<Vec<Target>, AppError> {
        McpHost::targets(self, entry, permissions)
    }

    fn servers(&self) -> Result<Vec<Server>, AppError> {
        self.app.state::<DbState>().with(|db| db.list_servers())
    }

    /// 入口的说明：本机入口给 "windows"（界面按语言翻译），服务器入口给服务器名称
    fn entry_label(&self, entry: &Entry) -> String {
        match entry {
            Entry::Windows => "windows".into(),
            Entry::Server { server_id } => crate::servers::load(&self.app, server_id)
                .map(|s| s.name)
                .unwrap_or_else(|_| server_id.clone()),
        }
    }

    fn client_label(session: &SessionInfo) -> String {
        let name = session.client.name.trim();
        let name = if name.is_empty() {
            "未知客户端"
        } else {
            name
        };
        format!("{name} {}", session.client.version.trim())
            .trim()
            .to_owned()
    }

    fn confirm(
        &self,
        request: impl FnOnce(u64) -> McpConfirmDto,
        cancelled: &AtomicBool,
    ) -> Option<Confirmation> {
        crate::app_shell::show_main(&self.app);
        let app = self.app.clone();
        let (id, answer) = self.confirms.ask(
            request,
            |dto| {
                if let Err(e) = app.emit(CONFIRM_EVENT, dto) {
                    eprintln!("推送 AI 接入确认框失败: 错误={e}");
                }
                crate::mobile::notify(&app, "confirms");
            },
            CONFIRM_TIMEOUT,
            cancelled,
        );
        if answer.is_none() {
            let _ = self.app.emit(CONFIRM_CLOSED_EVENT, id as f64);
        }
        crate::mobile::notify(&self.app, "confirms");
        answer
    }
}

/// 给 agent 看的目标名：显示名称；重名时加上 ID 前 8 位区分
fn target_names(servers: &[&Server]) -> HashMap<String, String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for s in servers {
        *counts.entry(s.name.as_str()).or_default() += 1;
    }
    servers
        .iter()
        .map(|s| {
            let name = if counts[s.name.as_str()] > 1 || s.name.eq_ignore_ascii_case("windows") {
                format!("{} ({})", s.name, s.id.chars().take(8).collect::<String>())
            } else {
                s.name.clone()
            };
            (s.id.clone(), name)
        })
        .collect()
}

fn address(server: &Server) -> Option<String> {
    let host = server.host.as_deref()?;
    let port = server.port.unwrap_or(22);
    Some(match server.username.as_deref() {
        Some(user) if !user.is_empty() => format!("{user}@{host}:{port}"),
        _ => format!("{host}:{port}"),
    })
}

impl McpHost for TauriHost {
    fn permissions(&self, entry: &Entry) -> Result<Option<Permissions>, AppError> {
        Ok(settings::load(&self.app).permissions(entry))
    }

    fn targets(&self, entry: &Entry, permissions: &Permissions) -> Result<Vec<Target>, AppError> {
        let mut targets = Vec::new();
        if permissions.windows_commands || !permissions.folders.is_empty() {
            targets.push(Target {
                target: "windows".into(),
                id: "windows".into(),
                name: "Windows".into(),
                kind: TargetKind::Windows,
                online: Some(true),
                engine: None,
                address: None,
            });
        }
        let all = self.servers()?;
        let allowed: Vec<&Server> = all
            .iter()
            .filter(|s| permissions.servers.contains(&s.id))
            .collect();
        let names = target_names(&allowed);
        let builtin = self.app.try_state::<crate::builtin::BuiltinState>();
        for server in allowed {
            let engine = pierlink_core::ssh::engine::engine_for_feature(
                server,
                pierlink_core::ssh::engine::Feature::Background,
            )
            .engine;
            let is_builtin = engine == pierlink_core::storage::Engine::Builtin;
            targets.push(Target {
                target: names[&server.id].clone(),
                id: server.id.clone(),
                name: server.name.clone(),
                kind: TargetKind::Server,
                online: if is_builtin {
                    builtin.as_ref().and_then(|b| b.online(&server.id))
                } else {
                    None
                },
                engine: Some(if is_builtin { "builtin" } else { "openssh" }.into()),
                // 从服务器入口进来的会话看不到其他服务器的地址（减少横向移动的信息）
                address: matches!(entry, Entry::Windows)
                    .then(|| address(server))
                    .flatten(),
            });
        }
        Ok(targets)
    }

    fn remembered(&self, entry: &Entry) -> bool {
        settings::load(&self.app)
            .entry(entry)
            .is_some_and(|e| e.remembered)
    }

    fn confirm_connect(
        &self,
        session: &SessionInfo,
        targets: &[Target],
        cancelled: &AtomicBool,
    ) -> Result<Confirmation, AppError> {
        let entry = self.entry_label(&session.entry);
        let client = Self::client_label(session);
        let names = targets.iter().map(|t| t.target.clone()).collect();
        let answer = self.confirm(
            |id| McpConfirmDto {
                id: id as f64,
                kind: "connect".into(),
                entry,
                client,
                targets: names,
                target: None,
                action: None,
            },
            cancelled,
        );
        let answer = answer.unwrap_or(Confirmation::Deny);
        if answer == Confirmation::AllowAlways
            && let Err(e) = settings::set_remembered(&self.app, &session.entry, true)
        {
            eprintln!("记住 AI 接入的允许失败: 错误={}", e.code);
        }
        eprintln!(
            "AI 接入第一次连接: 入口={}, 结果={answer:?}",
            self.entry_label(&session.entry)
        );
        Ok(answer)
    }

    fn confirm_action(
        &self,
        session: &SessionInfo,
        target: &Target,
        action: &str,
        dangerous: bool,
        cancelled: &AtomicBool,
    ) -> Result<Confirmation, AppError> {
        let entry = self.entry_label(&session.entry);
        let client = Self::client_label(session);
        let answer = self.confirm(
            |id| McpConfirmDto {
                id: id as f64,
                kind: if dangerous { "dangerous" } else { "action" }.into(),
                entry,
                client,
                targets: Vec::new(),
                target: Some(target.target.clone()),
                action: Some(action.to_owned()),
            },
            cancelled,
        );
        // 60 秒不答、窗口关了都按拒绝
        Ok(answer.unwrap_or(Confirmation::Deny))
    }

    fn run_server_command(
        &self,
        server_id: &str,
        command: &str,
        cwd: Option<&str>,
        limits: RunLimits,
        cancelled: Arc<AtomicBool>,
    ) -> Result<RunOutput, AppError> {
        super::run_server_command(&self.app, server_id, command, cwd, limits, cancelled)
    }

    fn open_terminal(
        &self,
        request: TerminalOpenRequest,
        cancelled: &AtomicBool,
    ) -> Result<String, AppError> {
        crate::app_shell::show_main(&self.app);
        let app = self.app.clone();
        let (kind, server_id) = match request.target.kind {
            TargetKind::Windows => ("windows", None),
            TargetKind::Server => ("server", Some(request.target.id.clone())),
        };
        let title = request.title.clone();
        let (_, answer) = self.terminals.ask(
            |id| McpOpenTerminalDto {
                id: id as f64,
                kind: kind.into(),
                server_id,
                title,
            },
            |dto| {
                if let Err(e) = app.emit(OPEN_TERMINAL_EVENT, dto) {
                    eprintln!("请界面打开 AI 终端失败: 错误={e}");
                }
            },
            OPEN_TERMINAL_TIMEOUT,
            cancelled,
        );
        let id = answer.unwrap_or_else(|| {
            Err(AppError::new("mcp.ui_timeout").with_reason(
                "mcp.ui_did_not_open_the_terminal",
                "Pierlink 界面没有打开终端（可能还在等你确认指纹或输入密码）",
            ))
        })?;
        if let Some(cwd) = request.cwd.as_deref() {
            super::change_directory(&self.app, &id, request.target.kind, cwd)?;
        }
        Ok(id)
    }

    fn user_terminals(&self) -> Result<Vec<UserTerminal>, AppError> {
        let state = self.app.state::<crate::terminal::TerminalState>();
        Ok(state
            .metas()
            .into_iter()
            .filter(|(_, m)| !m.ai_opened)
            .map(|(id, m)| {
                let (kind, target_id) = match &m.server_id {
                    Some(server) => (TargetKind::Server, server.clone()),
                    None => (TargetKind::Windows, "windows".to_owned()),
                };
                // 只查本机终端有没有在跑程序（服务器终端要连过去查，太慢）
                let busy = match kind {
                    TargetKind::Windows => state.manager.busy(&id).ok().flatten(),
                    TargetKind::Server => None,
                };
                UserTerminal {
                    terminal_id: id,
                    title: m.title,
                    kind,
                    target_id,
                    shell: m.shell,
                    busy,
                    handed_over: false,
                }
            })
            .collect())
    }

    fn terminal_cwd(&self, terminal_id: &str) -> Result<Option<String>, AppError> {
        let state = self.app.state::<crate::terminal::TerminalState>();
        Ok(state
            .metas()
            .into_iter()
            .find(|(id, _)| id == terminal_id)
            .and_then(|(_, m)| m.cwd))
    }

    fn sync_status(&self, task: Option<&str>) -> Result<Vec<SyncTaskState>, AppError> {
        crate::sync::mcp_task_states(&self.app, task)
    }

    fn sync_rescan(&self, folder_id: &str, timeout: Duration) -> Result<(), AppError> {
        crate::sync::mcp_rescan(&self.app, folder_id, timeout)
    }

    fn notify(&self, event: HostEvent) {
        // 手机页面据此重新拉对应的数据
        crate::mobile::notify(
            &self.app,
            match &event {
                HostEvent::Activity(_) => "activity",
                HostEvent::SessionOpened(_) | HostEvent::SessionClosed(_) => "overview",
                _ => "terminals",
            },
        );
        let result = match event {
            HostEvent::SessionOpened(_) | HostEvent::SessionClosed(_) => {
                self.app.emit(SESSIONS_EVENT, ())
            }
            HostEvent::Activity(record) => self
                .app
                .emit(super::ACTIVITY_EVENT, super::McpActivityDto::from(record)),
            HostEvent::TerminalOpened { terminal_id, .. } => self
                .app
                .emit(NOTICE_EVENT, McpNoticeDto::TerminalOpened { terminal_id }),
            HostEvent::TerminalClosed(terminal_id) => self
                .app
                .emit(NOTICE_EVENT, McpNoticeDto::TerminalClosed { terminal_id }),
            HostEvent::LoginNeeded { server_id } => self
                .app
                .emit(NOTICE_EVENT, McpNoticeDto::LoginNeeded { server_id }),
            HostEvent::PasswordPrompt { terminal_id } => self
                .app
                .emit(NOTICE_EVENT, McpNoticeDto::PasswordPrompt { terminal_id }),
            HostEvent::TerminalHandedOver { terminal_id, entry } => self.app.emit(
                NOTICE_EVENT,
                McpNoticeDto::TerminalHandedOver {
                    terminal_id,
                    entry_label: self.entry_label(&entry),
                    entry: super::entry_text(&entry),
                },
            ),
            HostEvent::TerminalTakenBack { terminal_id, .. } => self.app.emit(
                NOTICE_EVENT,
                McpNoticeDto::TerminalTakenBack { terminal_id },
            ),
        };
        if let Err(e) = result {
            eprintln!("推送 AI 接入事件失败: 错误={e}");
        }
    }
}
