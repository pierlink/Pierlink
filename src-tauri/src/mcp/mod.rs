//! AI 接入（MCP）：把 core 的 MCP 服务接到 Tauri——设置、界面确认、AI 终端、
//! Windows 入口的命名管道、服务器入口的转发和地址文件。
mod entries;
mod host;
pub mod install;
pub mod settings;
pub(crate) mod waiters;

pub use entries::McpEntryStateDto;
pub use host::McpConfirmDto;
pub use settings::McpSettingsDto;

use crate::terminal::TerminalState;
use host::TauriHost;
use pierlink_core::error::AppError;
use pierlink_core::mcp::{
    AuditRecord, Confirmation, Entry, McpService, RunLimits, RunOutput, SessionInfo, TargetKind,
};
use pierlink_core::terminal::InputSource;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};

/// 会话变化（界面收到后重新取状态）
pub const SESSIONS_EVENT: &str = "pierlink://mcp-sessions";
/// 一次工具调用的记录
pub const ACTIVITY_EVENT: &str = "pierlink://mcp-activity";
/// 请用户确认（第一次连接、危险操作）
pub const CONFIRM_EVENT: &str = "pierlink://mcp-confirm";
/// 确认请求已失效（超时、取消），界面关掉对应的框
pub const CONFIRM_CLOSED_EVENT: &str = "pierlink://mcp-confirm-closed";
/// 请界面打开一个 AI 终端
pub const OPEN_TERMINAL_EVENT: &str = "pierlink://mcp-open-terminal";
/// 其他提示（要登录、AI 终端在等密码、AI 终端打开或结束）
pub const NOTICE_EVENT: &str = "pierlink://mcp-notice";
/// 服务器入口的运行状态
pub const ENTRIES_EVENT: &str = "pierlink://mcp-entries";

pub(crate) use entries::TUNNEL_PREFIX;

/// Windows 入口（命名管道）的状态
#[derive(Clone, Debug, Default)]
struct PipeStatus {
    running: bool,
    error: Option<String>,
}

/// AI 接入的应用状态
pub struct McpState {
    service: Arc<McpService>,
    host: Arc<TauriHost>,
    entries: entries::Entries,
    pipe: Arc<Mutex<PipeStatus>>,
}

/// 一个连着的客户端
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpSessionDto {
    pub id: String,
    /// windows 或服务器 ID
    pub entry: String,
    /// 客户端自报的名称和版本（不可信）
    pub client: String,
}

/// 一次工具调用（已脱敏）
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpActivityDto {
    pub time_ms: f64,
    pub session: String,
    pub entry: String,
    pub client: String,
    pub tool: String,
    pub target: Option<String>,
    pub summary: String,
    /// ok / denied / cancelled / 错误码
    pub outcome: String,
    pub exit_code: Option<i32>,
    pub duration_ms: f64,
    /// 谁批准的：user 用户点了允许、session 这次连接不再问、auto 全自动；空 = 本来就不用问
    pub approval: String,
    /// 命令输出的结尾（已脱敏，只在内存里）；没有输出的操作为 null
    pub output: Option<McpOutputPreviewDto>,
}

#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpOutputPreviewDto {
    pub stdout: String,
    pub stderr: String,
}

/// 设置页显示的整体状态
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpStatusDto {
    /// Windows 入口的命名管道在运行（agent 能连上来）
    pub pipe_running: bool,
    /// 命名管道建不起来的原因
    pub pipe_error: Option<String>,
    pub sessions: Vec<McpSessionDto>,
    pub entries: Vec<McpEntryStateDto>,
}

/// 给用户复制的接入命令
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpConnectInfoDto {
    /// 在这台电脑上运行（Claude Code、其他命令行助手、通用配置）
    pub windows: Vec<McpCommandDto>,
    /// 每个服务器入口：在那台服务器上运行
    pub servers: Vec<McpServerConnectDto>,
}

#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpCommandDto {
    /// 给谁用：claude / codex / json
    pub client: String,
    pub text: String,
}

#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConnectDto {
    pub server_id: String,
    pub url: String,
    pub commands: Vec<String>,
}

/// 界面回报的打开失败原因（后端错误原样带回来）
#[derive(Deserialize, Type, Clone, Debug)]
pub struct McpErrorInput {
    pub code: String,
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, String>,
}

impl From<McpErrorInput> for AppError {
    fn from(e: McpErrorInput) -> Self {
        e.params
            .into_iter()
            .fold(AppError::new(e.code), |error, (k, v)| error.with(k, v))
    }
}

/// 用户对确认框的回答
#[derive(Deserialize, Type, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum McpAnswer {
    Allow,
    AllowAlways,
    Deny,
}

fn entry_text(entry: &Entry) -> String {
    match entry {
        Entry::Windows => "windows".into(),
        Entry::Server { server_id } => server_id.clone(),
    }
}

fn client_text(name: &str, version: &str) -> String {
    format!("{} {}", name.trim(), version.trim())
        .trim()
        .to_owned()
}

impl From<SessionInfo> for McpSessionDto {
    fn from(s: SessionInfo) -> Self {
        Self {
            id: s.id,
            entry: entry_text(&s.entry),
            client: client_text(&s.client.name, &s.client.version),
        }
    }
}

impl From<AuditRecord> for McpActivityDto {
    fn from(r: AuditRecord) -> Self {
        Self {
            time_ms: r.time_ms as f64,
            session: r.session,
            entry: entry_text(&r.entry),
            client: client_text(&r.client.name, &r.client.version),
            tool: r.tool,
            target: r.target,
            summary: r.summary,
            outcome: r.outcome,
            exit_code: r.exit_code,
            duration_ms: r.duration_ms as f64,
            approval: r.approval,
            output: r.output_preview.map(|o| McpOutputPreviewDto {
                stdout: o.stdout,
                stderr: o.stderr,
            }),
        }
    }
}

impl McpState {
    /// 建 MCP 服务、开始在命名管道上接受 Windows 入口的连接（不论设置开没开：
    /// 没开时 agent 能连上、看到工具，第一次调用工具时被告知去设置里打开）
    ///
    /// :param app: 应用句柄（数据库、终端状态要先建好）
    /// :return: 应用状态；数据目录拿不到或运行时建不起来时返回错误
    pub fn start(app: &AppHandle) -> Result<Self, AppError> {
        let host = Arc::new(TauriHost::new(app.clone()));
        let terminals = app.state::<TerminalState>().manager.clone();
        let audit_dir = crate::data::data_dir(app)?.join("logs").join("mcp");
        let service = Arc::new(McpService::new(host.clone(), terminals, audit_dir)?);
        let pipe = Arc::new(Mutex::new(PipeStatus::default()));
        start_pipe(&service, &pipe);
        Ok(Self {
            service,
            host,
            entries: entries::Entries::default(),
            pipe,
        })
    }

    fn status(&self) -> McpStatusDto {
        let pipe = self.pipe.lock().unwrap_or_else(|p| p.into_inner()).clone();
        McpStatusDto {
            pipe_running: pipe.running,
            pipe_error: pipe.error,
            sessions: self
                .service
                .sessions()
                .into_iter()
                .map(Into::into)
                .collect(),
            entries: self.entries.states(),
        }
    }
}

#[cfg(windows)]
fn start_pipe(service: &Arc<McpService>, status: &Arc<Mutex<PipeStatus>>) {
    let name = match pierlink_core::mcp::pipe::pipe_name() {
        Ok(name) => name,
        Err(e) => {
            eprintln!("AI 接入的命名管道名取不到: 错误={}", e.code);
            status.lock().unwrap_or_else(|p| p.into_inner()).error = Some(e.code);
            return;
        }
    };
    let weak = Arc::downgrade(service);
    let status = status.clone();
    status.lock().unwrap_or_else(|p| p.into_inner()).running = true;
    service.runtime().spawn(async move {
        let result = pierlink_core::mcp::pipe::serve(name, move |pipe| {
            let Some(service) = weak.upgrade() else {
                return;
            };
            if let Err(e) = service.serve_duplex(Entry::Windows, pipe) {
                eprintln!("接受 AI 接入连接失败: 错误={}", e.code);
            }
        })
        .await;
        if let Err(e) = result {
            eprintln!("AI 接入的命名管道停止: 错误={}", e.code);
            let mut s = status.lock().unwrap_or_else(|p| p.into_inner());
            s.running = false;
            s.error = Some(e.params.get("reason").cloned().unwrap_or(e.code));
        }
    });
    eprintln!("AI 接入的命名管道已开始接受连接");
}

/// 开发时在 Linux 上运行：没有 Windows 入口
#[cfg(not(windows))]
fn start_pipe(_service: &Arc<McpService>, status: &Arc<Mutex<PipeStatus>>) {
    status.lock().unwrap_or_else(|p| p.into_inner()).error = Some("只在 Windows 上提供".into());
}

/// 设置里应该运行的服务器入口（服务器 ID → 端口）
fn wanted_entries(settings: &McpSettingsDto) -> HashMap<String, u16> {
    if !settings.enabled {
        return HashMap::new();
    }
    settings
        .servers
        .iter()
        .filter(|s| s.entry.enabled)
        .map(|s| (s.server_id.clone(), s.port))
        .collect()
}

/// 按设置开关服务器入口（启动时、改设置后调用；会建 SSH 连接，在后台线程里做）
pub fn apply_entries(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let Some(state) = app.try_state::<McpState>() else {
            return;
        };
        state.entries.apply(&app, &state.service, || {
            wanted_entries(&settings::load(&app))
        });
    });
}

/// 端口转发的事件循环转交过来的、服务器入口隧道的状态
pub(crate) fn on_tunnel_event(app: &AppHandle, event: &pierlink_core::tunnel::TunnelEvent) {
    if let Some(state) = app.try_state::<McpState>() {
        state.entries.on_tunnel_event(app, event);
    }
}

/// 退出前：放弃等待中的确认和打开终端请求，断开所有 agent
pub fn shutdown(app: &AppHandle) {
    let Some(state) = app.try_state::<McpState>() else {
        return;
    };
    for id in state.host.confirms.cancel_all() {
        let _ = tauri::Emitter::emit(app, CONFIRM_CLOSED_EVENT, id as f64);
    }
    state.host.terminals.cancel_all();
    state.service.stop_all();
}

/// 在服务器上原样执行一条命令（后台方式：缺密码或验证码时返回错误，不弹框）
pub(crate) fn run_server_command(
    app: &AppHandle,
    server_id: &str,
    command: &str,
    cwd: Option<&str>,
    limits: RunLimits,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Result<RunOutput, AppError> {
    let server = crate::servers::load(app, server_id)?;
    let access = crate::keys::Access::Background("AI 接入");
    let program = pierlink_core::process::SshProgram {
        path: crate::terminal::find_ssh(app)?,
    };
    let openssh = || {
        let target = crate::servers::target(&server)?;
        pierlink_core::ssh::args::build_args(
            &target,
            &pierlink_core::ssh::args::Purpose::Background,
            &crate::keys::ssh_opts(app, &server, access)?,
        )
    };
    let builtin = app.state::<crate::builtin::BuiltinState>();
    let passwords = app.state::<crate::passwords::PasswordState>();
    let request = pierlink_core::builtin::ConnectionRequest {
        server: &server,
        class: pierlink_core::builtin::ConnectionClass::Main,
        vault: passwords.vault.clone(),
        callbacks: builtin.callbacks.clone(),
        interactive: false,
        cancelled: cancelled.clone(),
    };
    pierlink_core::remote::raw::run_raw(
        &builtin.manager,
        request,
        &program,
        openssh,
        command,
        cwd,
        limits,
        cancelled,
    )
}

/// AI 要打开终端的目录能不能用：给终端的输入等于打字，路径里的换行会提前回车，把后半截当命令执行。
/// 在请界面开终端之前检查，免得开出一个用不了的终端。
pub(crate) fn validate_cwd(cwd: &str) -> Result<(), AppError> {
    if cwd.chars().any(char::is_control) {
        return Err(AppError::new("mcp.invalid_cwd").with_reason(
            "mcp.folder_has_control_characters",
            "目录里不能有换行等控制字符",
        ));
    }
    Ok(())
}

/// 放进 PowerShell 单引号字符串：PowerShell 把 ' 和弯引号 ‘ ’ ‚ ‛ 都当单引号，每个都要写两遍，
/// 否则目录名里的弯引号会提前结束字符串，后半截被当成命令执行
fn ps_single_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// AI 终端打开后切到 cwd：以自动化输入发一行切换目录的命令（会回显，用户按键优先）
pub(crate) fn change_directory(
    app: &AppHandle,
    terminal_id: &str,
    kind: TargetKind,
    cwd: &str,
) -> Result<(), AppError> {
    validate_cwd(cwd)?;
    let line = match kind {
        TargetKind::Windows => format!("Set-Location -LiteralPath {}\r", ps_single_quoted(cwd)),
        TargetKind::Server => format!("cd -- {}\r", pierlink_core::ssh::quote::sh_quote(cwd)?),
    };
    app.state::<TerminalState>().manager.write_from(
        terminal_id,
        line.as_bytes(),
        InputSource::Automation,
    )
}

// ---------- 命令 ----------

fn state(app: &AppHandle) -> Result<tauri::State<'_, McpState>, AppError> {
    app.try_state::<McpState>().ok_or_else(|| {
        AppError::new("mcp.unavailable").with_reason("mcp.ai_access_not_started", "AI 接入没有启动")
    })
}

/// 读 AI 接入设置
#[tauri::command]
#[specta::specta]
pub async fn mcp_settings_get(app: AppHandle) -> Result<McpSettingsDto, AppError> {
    Ok(settings::load(&app))
}

/// 保存 AI 接入设置，并按新设置开关服务器入口
///
/// :param settings: 界面提交的设置
/// :return: 保存后的设置（文件夹去重、服务器列表排序）
#[tauri::command]
#[specta::specta]
pub async fn mcp_settings_set(
    app: AppHandle,
    settings: McpSettingsDto,
) -> Result<McpSettingsDto, AppError> {
    let saved = settings::save(&app, settings)?;
    apply_entries(&app);
    Ok(saved)
}

/// 整体状态：Windows 入口是否在运行、连着的客户端、服务器入口
#[tauri::command]
#[specta::specta]
pub async fn mcp_status(app: AppHandle) -> Result<McpStatusDto, AppError> {
    Ok(state(&app)?.status())
}

/// 给用户复制的接入命令
#[tauri::command]
#[specta::specta]
pub async fn mcp_connect_info(app: AppHandle) -> Result<McpConnectInfoDto, AppError> {
    let exe = std::env::current_exe()
        .map_err(|e| AppError::new("mcp.exe_unknown").with("reason", e.to_string()))?;
    let exe = exe.display().to_string();
    let json = serde_json::json!({"mcpServers": {"pierlink": {"command": exe, "args": ["--mcp"]}}});
    let windows = vec![
        McpCommandDto {
            client: "claude".into(),
            text: format!("claude mcp add --scope user pierlink -- \"{exe}\" --mcp"),
        },
        McpCommandDto {
            client: "codex".into(),
            text: format!("codex mcp add pierlink -- \"{exe}\" --mcp"),
        },
        McpCommandDto {
            client: "json".into(),
            text: serde_json::to_string_pretty(&json).unwrap_or_default(),
        },
    ];
    let servers = settings::load(&app)
        .servers
        .into_iter()
        .map(|s| {
            let url = entries::url(s.port);
            McpServerConnectDto {
                server_id: s.server_id,
                commands: pierlink_core::mcp::token::connect_commands(&url),
                url,
            }
        })
        .collect();
    Ok(McpConnectInfoDto { windows, servers })
}

/// 换一个服务器入口的令牌（旧令牌立即失效，服务器上的文件随后重写）
#[tauri::command]
#[specta::specta]
pub async fn mcp_token_rotate(app: AppHandle, server_id: String) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = state(&app)?;
        state.entries.rotate(&app, &state.service, &server_id)
    })
    .await
    .map_err(|e| AppError::new("mcp.task_failed").with("reason", e.to_string()))?
}

/// 最近的操作记录（新的在前）
#[tauri::command]
#[specta::specta]
pub async fn mcp_activity(app: AppHandle, limit: u32) -> Result<Vec<McpActivityDto>, AppError> {
    Ok(state(&app)?
        .service
        .activity(limit.min(200) as usize)
        .into_iter()
        .map(Into::into)
        .collect())
}

/// 全部停止：取消正在进行的操作、断开所有 agent、关闭这次打开的 AI 终端
#[tauri::command]
#[specta::specta]
pub async fn mcp_stop_all(app: AppHandle) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        state(&app)?.service.stop_all();
        eprintln!("AI 接入：用户全部停止");
        Ok(())
    })
    .await
    .map_err(|e| AppError::new("mcp.task_failed").with("reason", e.to_string()))?
}

/// 回答确认框
///
/// :param id: 确认请求编号
/// :param answer: allow / allow_always / deny
/// :return: 请求已失效（超时、取消）时返回 false
#[tauri::command]
#[specta::specta]
pub async fn mcp_confirm_answer(
    app: AppHandle,
    id: f64,
    answer: McpAnswer,
) -> Result<bool, AppError> {
    let answer = match answer {
        McpAnswer::Allow => Confirmation::Allow,
        McpAnswer::AllowAlways => Confirmation::AllowAlways,
        McpAnswer::Deny => Confirmation::Deny,
    };
    Ok(state(&app)?.host.confirms.answer(id as u64, answer))
}

/// 还没回答的确认（界面重新加载后取回）
#[tauri::command]
#[specta::specta]
pub async fn mcp_confirm_pending(app: AppHandle) -> Result<Vec<McpConfirmDto>, AppError> {
    Ok(state(&app)?.host.confirms.pending())
}

/// 界面打开 AI 终端后回报结果
///
/// :param id: 打开请求编号
/// :param terminal_id: 打开成功的终端 ID
/// :param error: 打开失败的原因（用户取消了指纹或密码、连不上……）
#[tauri::command]
#[specta::specta]
pub async fn mcp_terminal_opened(
    app: AppHandle,
    id: f64,
    terminal_id: Option<String>,
    error: Option<McpErrorInput>,
) -> Result<bool, AppError> {
    let opened = terminal_id.clone();
    let result = match (terminal_id, error) {
        (Some(terminal), _) => {
            // AI 自己开的终端不算用户终端，不出现在“交给 AI”的列表里
            app.state::<TerminalState>()
                .update_meta(&terminal, |m| m.ai_opened = true);
            Ok(terminal)
        }
        (None, Some(error)) => Err(error.into()),
        (None, None) => Err(AppError::new("mcp.terminal_failed")
            .with_reason("mcp.terminal_not_open", "终端没有打开")),
    };
    let delivered = state(&app)?.host.terminals.answer(id as u64, result);
    // AI 已经等不及走了（比如用户过了很久才确认指纹）：这个终端交还给用户，能在“交给 AI”里重新交出去
    if !delivered && let Some(terminal) = opened {
        app.state::<TerminalState>()
            .update_meta(&terminal, |m| m.ai_opened = false);
    }
    Ok(delivered)
}

/// 可以接手某个终端的入口
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpHandoverEntryDto {
    /// "windows" 或服务器 ID
    pub entry: String,
    /// "windows"（界面翻译）或服务器名称
    pub label: String,
}

fn entry_from_text(entry: &str) -> Entry {
    if entry == "windows" {
        Entry::Windows
    } else {
        Entry::Server {
            server_id: entry.to_owned(),
        }
    }
}

/// 终端对应的目标：本机终端是 windows，服务器终端是服务器 ID；没登记或是 AI 自己开的返回 None
fn terminal_target(app: &AppHandle, terminal_id: &str) -> Option<(TargetKind, String)> {
    let (_, meta) = app
        .state::<TerminalState>()
        .metas()
        .into_iter()
        .find(|(id, m)| id == terminal_id && !m.ai_opened)?;
    Some(match meta.server_id {
        Some(server) => (TargetKind::Server, server),
        None => (TargetKind::Windows, "windows".to_owned()),
    })
}

/// 入口有没有操作这个目标终端的权限（本机终端要“执行命令”，服务器终端要勾了这台服务器）
fn entry_allows(permissions: &pierlink_core::mcp::Permissions, kind: TargetKind, id: &str) -> bool {
    match kind {
        TargetKind::Windows => permissions.windows_commands,
        TargetKind::Server => permissions.servers.iter().any(|s| s == id),
    }
}

/// 能接手这个终端的入口（已打开、并且有这个终端目标的权限）
///
/// :param terminal_id: 终端 ID
/// :return: 入口列表；终端不能交出（快速连接、AI 自己开的）时为空
#[tauri::command]
#[specta::specta]
pub async fn mcp_handover_entries(
    app: AppHandle,
    terminal_id: String,
) -> Result<Vec<McpHandoverEntryDto>, AppError> {
    let Some((kind, id)) = terminal_target(&app, &terminal_id) else {
        return Ok(Vec::new());
    };
    let settings = settings::load(&app);
    let mut entries = vec![Entry::Windows];
    entries.extend(settings.servers.iter().map(|s| Entry::Server {
        server_id: s.server_id.clone(),
    }));
    let state = state(&app)?;
    Ok(entries
        .into_iter()
        .filter(|e| {
            settings
                .permissions(e)
                .is_some_and(|p| entry_allows(&p, kind, &id))
        })
        .map(|e| McpHandoverEntryDto {
            label: state.host.label_of(&e),
            entry: entry_text(&e),
        })
        .collect())
}

/// 把用户终端交给某个入口的 AI：AI 能往里输入、读输出，用户随时可以收回
///
/// :param terminal_id: 终端 ID
/// :param entry: "windows" 或服务器 ID
#[tauri::command]
#[specta::specta]
pub async fn mcp_hand_over(
    app: AppHandle,
    terminal_id: String,
    entry: String,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (kind, id) = terminal_target(&app, &terminal_id).ok_or_else(|| {
            AppError::new("mcp.terminal_not_found")
                .with_reason("mcp.terminal_cannot_be_handed_to_ai", "这个终端不能交给 AI")
        })?;
        let entry = entry_from_text(&entry);
        let state = state(&app)?;
        let permissions = settings::load(&app).permissions(&entry).ok_or_else(|| {
            AppError::new("mcp.entry_disabled")
                .with_reason("mcp.this_mcp_entry_is_disabled", "这个 MCP 入口未启用")
        })?;
        let target = state
            .host
            .targets_for(&entry, &permissions)?
            .into_iter()
            .find(|t| t.kind == kind && t.id == id)
            .ok_or_else(|| {
                AppError::new("mcp.target_denied").with_reason(
                    "mcp.this_entry_cannot_operate_on_the_terminal_target",
                    "这个入口没有终端目标的操作权限",
                )
            })?;
        state.service.hand_over(&terminal_id, entry, target)?;
        eprintln!("AI 接入：终端交给 AI，终端={terminal_id}");
        Ok(())
    })
    .await
    .map_err(|e| AppError::new("mcp.task_failed").with("reason", e.to_string()))?
}

/// 收回交给 AI 的终端（终端继续开着）
///
/// :param terminal_id: 终端 ID
#[tauri::command]
#[specta::specta]
pub async fn mcp_take_back(app: AppHandle, terminal_id: String) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        state(&app)?.service.take_back(&terminal_id)?;
        eprintln!("AI 接入：收回终端，终端={terminal_id}");
        Ok(())
    })
    .await
    .map_err(|e| AppError::new("mcp.task_failed").with("reason", e.to_string()))?
}

// ---------- 给手机入口用 ----------

/// 还没回答的确认（手机上显示）
pub(crate) fn pending_confirms(app: &AppHandle) -> Vec<McpConfirmDto> {
    state(app)
        .map(|s| s.host.confirms.pending())
        .unwrap_or_default()
}

/// 从手机回答确认：和电脑上点一样；电脑上的确认框随之关掉
///
/// :return: 请求已失效时返回 false
pub(crate) fn answer_from_phone(app: &AppHandle, id: u64, allow: bool) -> bool {
    let Ok(state) = state(app) else {
        return false;
    };
    let answer = if allow {
        Confirmation::Allow
    } else {
        Confirmation::Deny
    };
    let ok = state.host.confirms.answer(id, answer);
    let _ = tauri::Emitter::emit(app, CONFIRM_CLOSED_EVENT, id as f64);
    crate::mobile::notify(app, "confirms");
    ok
}

/// 从手机全部停止
pub(crate) fn stop_all_from_phone(app: &AppHandle) {
    if let Ok(state) = state(app) {
        state.service.stop_all();
        eprintln!("AI 接入：手机上全部停止");
    }
}

/// 最近的操作记录（手机上显示）
pub(crate) fn recent_activity(app: &AppHandle, limit: usize) -> Vec<McpActivityDto> {
    state(app)
        .map(|s| {
            s.service
                .activity(limit.min(200))
                .into_iter()
                .map(Into::into)
                .collect()
        })
        .unwrap_or_default()
}

/// 连着的 AI 数
pub(crate) fn session_count(app: &AppHandle) -> usize {
    state(app).map(|s| s.service.sessions().len()).unwrap_or(0)
}

/// 用资源管理器打开审计日志所在的文件夹
#[tauri::command]
#[specta::specta]
pub async fn mcp_open_log_folder(app: AppHandle) -> Result<(), AppError> {
    let dir = crate::data::data_dir(&app)?.join("logs").join("mcp");
    tauri::async_runtime::spawn_blocking(move || {
        std::fs::create_dir_all(&dir)
            .map_err(|e| AppError::new("mcp.log_folder_failed").with("reason", e.to_string()))?;
        pierlink_core::fs_local::reveal_in_explorer(&dir)
    })
    .await
    .map_err(|e| AppError::new("mcp.task_failed").with("reason", e.to_string()))?
}

/// 把一个终端交给用户（拒绝 AI 的输入）或交还给 AI
///
/// :param id: 终端 ID
/// :param taken_over: true 表示用户接管
#[tauri::command]
#[specta::specta]
pub async fn terminal_set_user_control(
    app: AppHandle,
    id: String,
    taken_over: bool,
) -> Result<(), AppError> {
    app.state::<TerminalState>()
        .manager
        .set_user_control(&id, taken_over)
}

#[cfg(test)]
mod quote_tests {
    use super::ps_single_quoted;

    #[test]
    fn 弯引号和直引号都写两遍不会提前结束字符串() {
        assert_eq!(ps_single_quoted(r"C:\Users\a"), r"'C:\Users\a'");
        assert_eq!(ps_single_quoted("it's"), "'it''s'");
        assert_eq!(
            ps_single_quoted("Bob\u{2019}s Files"),
            "'Bob\u{2019}\u{2019}s Files'"
        );
        assert_eq!(
            ps_single_quoted("a\u{2019}; Start-Process calc; \u{2018}"),
            "'a\u{2019}\u{2019}; Start-Process calc; \u{2018}\u{2018}'"
        );
        assert_eq!(
            ps_single_quoted("\u{201a}\u{201b}"),
            "'\u{201a}\u{201a}\u{201b}\u{201b}'"
        );
    }
}
