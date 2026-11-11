//! MCP 服务和宿主（Tauri）之间的接口。core 不依赖 Tauri：凡是要界面、数据库、凭据、连接池、同步的事，
//! 都经 [`McpHost`] 交给宿主做。这里的类型和方法签名是 core 与 Tauri 两边并行开发的约定，改动要两边一起改。
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

/// 客户端从哪个入口连上来
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    /// Windows 本机：`Pierlink.exe --mcp` 经命名管道
    Windows,
    /// 某台服务器：经这台服务器上的远程转发进来的 HTTP
    Server { server_id: String },
}

/// 客户端自报的名称和版本：只用于显示，不可信，不能作为授权依据
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

/// 这个入口的审批方式
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    /// 执行命令、终端里回车提交、写文件，每次都问
    AskAll,
    /// 只有危险命令和写脚本类文件要问
    #[default]
    Dangerous,
    /// 都不问，只记录
    Auto,
}

/// 一个入口允许做的事
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Permissions {
    /// 操作审批方式；默认只询问危险操作
    pub approval: Approval,
    /// 允许在 Windows 本机执行命令（PowerShell 能读写这个账户的任何文件，等于把账户交出去）
    pub windows_commands: bool,
    /// 本机文件工具（read_file、write_file、list_dir）只能在这些文件夹里；绝对路径
    pub folders: Vec<PathBuf>,
    /// 允许操作的服务器 ID
    pub servers: Vec<String>,
}

/// 目标的种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Windows,
    Server,
}

/// agent 能操作的一台机器
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Target {
    /// 工具参数 `target` 用的名字："windows" 或服务器的显示名称
    pub target: String,
    /// 稳定 ID："windows" 或服务器 ID
    pub id: String,
    /// 显示名称
    pub name: String,
    pub kind: TargetKind,
    /// 内置 SSH 的连接状态；Windows 和系统 OpenSSH 的服务器为 None（不知道）
    pub online: Option<bool>,
    /// 连接方式 "builtin" / "openssh"；Windows 为 None
    pub engine: Option<String>,
    /// 服务器地址（用户@主机:端口）；从服务器入口进来的会话看不到其他服务器的地址，为 None
    pub address: Option<String>,
    /// 几台电脑上都一样的服务器标识（按地址算的短哈希，见 peers::server_key）；不知道地址时为 None。
    /// list_targets 把它当 id 给 agent，工具参数 target 也认它：用户换了电脑，AI 手里的 id 照样能用
    #[serde(skip)]
    pub key: Option<String>,
}

/// 提供给宿主的后台任务快照；命令已脱敏并限制为前 200 个字符，完成记录保留 30 分钟。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobView {
    pub id: String,
    /// windows 或服务器 ID。
    pub entry: String,
    /// 目标 ID。
    pub target: String,
    pub command: String,
    /// Unix 毫秒。
    pub started_ms: u64,
    pub running: bool,
    pub exit_code: Option<i32>,
}

/// 用户对首次连接或单次操作审批的回答；两种审批的记忆范围不同
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    /// 首次连接：允许这个会话；操作审批：只允许这次操作
    Allow,
    /// 连接确认：以后这个入口的连接不再询问；操作审批：这次连接的操作不再询问
    AllowAlways,
    Deny,
    /// 没人回应、超时或取消；不得记住为拒绝
    NoAnswer,
}

/// 一个已连上的客户端会话
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub entry: Entry,
    pub client: ClientInfo,
    /// 会话建立时读到的权限快照
    pub permissions: Permissions,
}

/// 界面上显示的会话状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStatus {
    pub info: SessionInfo,
    /// 有调用正在进行
    pub busy: bool,
    /// 最后一次请求的时间（Unix 毫秒）；调用进行中时为当前时间
    pub last_active_ms: u64,
}

/// 执行命令的限制
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
    pub timeout: Duration,
    /// stdout、stderr 各自保留的字节数（超出时保留开头和结尾）
    pub output_bytes: usize,
}

/// 一次命令执行的结果（本机和服务器共用）
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunOutput {
    /// 进程退出码；被信号结束或超时被结束时可能为 None
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// stdout 或 stderr 超过限制被截断
    pub truncated: bool,
    pub timed_out: bool,
    pub duration_ms: u64,
}

/// 请界面打开一个 AI 终端
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOpenRequest {
    pub session: String,
    pub target: Target,
    /// 打开后先切到这个目录（远程终端以自动化输入发一行 cd，会回显）
    pub cwd: Option<String>,
    pub title: Option<String>,
}

/// 一台服务器在某个同步任务里的状态
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SyncServerState {
    /// 该任务在服务器上的绝对路径，由宿主从同步端点填写。
    pub remote_path: String,
    pub server_id: String,
    pub name: String,
    pub connected: bool,
    /// Syncthing 报的完成度（0–100）；查不到为 None
    pub completion: Option<f64>,
    pub need_items: Option<u64>,
    pub need_deletes: Option<u64>,
}

/// 一个同步任务的状态
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SyncTaskState {
    /// 本机 Windows 绝对路径，由宿主从同步任务填写。
    pub local_path: String,
    pub id: String,
    pub name: String,
    /// 本机 Syncthing 文件夹 ID（立即扫描用）
    pub folder_id: String,
    /// Syncthing 文件夹状态（idle、scanning、syncing……）
    pub state: String,
    /// 同步引擎报告的错误列表；无错误时为空，宿主不得包含凭据。
    pub errors: Vec<String>,
    pub paused: bool,
    pub need_bytes: u64,
    pub servers: Vec<SyncServerState>,
}

/// 仅在内存中提供给界面的输出尾部，双流各最多 8 KiB，已脱敏。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputPreview {
    pub stdout: String,
    pub stderr: String,
}

/// 一次工具调用的记录：写审计日志，也推给界面的操作记录。文字字段都已脱敏
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// 活动记录中的输出预览；审计写盘前移除此字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_preview: Option<OutputPreview>,
    /// Unix 毫秒
    pub time_ms: u64,
    pub session: String,
    pub entry: Entry,
    pub client: ClientInfo,
    pub tool: String,
    pub target: Option<String>,
    /// 命令前 2 KB、文件路径、终端 ID、发送文字前 200 字等；因密码提示被拒的发送不记文字
    pub summary: String,
    /// "ok"、"denied"、"cancelled" 或错误码
    pub outcome: String,
    /// "user"：单次允许；"session"：连接内不再询问；"auto"：全自动；空串：无需审批或未获允许
    #[serde(default)]
    pub approval: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// 这次调用转给了哪台电脑执行（电脑名）；那台电脑自己的记录为准
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_to: Option<String>,
    /// 这次调用是哪台电脑转来的（电脑名）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_from: Option<String>,
}

/// 推给宿主（界面）的事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    SessionOpened(SessionInfo),
    /// 会话 ID
    SessionClosed(String),
    /// 可见会话集合发生变化，请重新读取 sessions()；不携带数据。
    SessionsChanged,
    /// 后台任务开始、结束或保留记录过期；宿主重新读取 jobs() 刷新界面。
    JobsChanged,
    Activity(AuditRecord),
    TerminalOpened {
        session: String,
        terminal_id: String,
    },
    /// 终端 ID
    TerminalClosed(String),
    /// 服务器要密码或验证码才能连：工具已返回错误，界面提示用户登录
    LoginNeeded {
        server_id: String,
    },
    /// AI 终端在等密码，拒绝了 agent 发送的文字：界面提示用户在这个终端里输入
    PasswordPrompt {
        terminal_id: String,
    },
}

/// 宿主要提供的能力。方法都在阻塞线程上调用（不在异步运行时里），可以阻塞等待；
/// 带 `cancelled` 的方法在它变为 true 时要尽快返回（错误码 `mcp.cancelled`）。
pub trait McpHost: Send + Sync + 'static {
    /// 入口的权限；None 表示这个入口没有打开（MCP 总开关关闭也算）
    fn permissions(&self, entry: &Entry) -> Result<Option<Permissions>, AppError>;
    /// 按权限列出能操作的目标（不在权限里的服务器不返回）
    fn targets(&self, entry: &Entry, permissions: &Permissions) -> Result<Vec<Target>, AppError>;
    /// 这个入口的新连接要不要先请用户确认（用户在入口设置里打开；默认不问）
    fn confirm_connections(&self, entry: &Entry) -> bool;
    /// 要确认的入口上，第一次工具调用前请用户确认；AllowAlways 表示以后不再问，由宿主自己记住。
    /// 60 秒不答由宿主按 NoAnswer 返回
    fn confirm_connect(
        &self,
        session: &SessionInfo,
        targets: &[Target],
        cancelled: &AtomicBool,
    ) -> Result<Confirmation, AppError>;
    /// 请用户审批 session 对 target 的 action；dangerous 区分危险操作和普通操作。
    /// 返回单次允许、这次连接不再询问或拒绝；宿主在 60 秒未答时返回 NoAnswer，并响应 cancelled。
    fn confirm_action(
        &self,
        session: &SessionInfo,
        target: &Target,
        action: &str,
        dangerous: bool,
        cancelled: &AtomicBool,
    ) -> Result<Confirmation, AppError>;
    /// 在服务器上原样执行命令（按连接方式走内置 SSH 或系统 OpenSSH，后台方式：不弹密码框，
    /// 缺密码或验证码时返回 ssh.password_needed / ssh.two_factor_required）
    fn run_server_command(
        &self,
        server_id: &str,
        command: &str,
        cwd: Option<&str>,
        limits: RunLimits,
        cancelled: Arc<AtomicBool>,
    ) -> Result<RunOutput, AppError>;
    /// 请界面按正常流程打开终端（标“AI”），返回终端 ID；界面 120 秒不回报返回 mcp.ui_timeout
    fn open_terminal(
        &self,
        request: TerminalOpenRequest,
        cancelled: &AtomicBool,
    ) -> Result<String, AppError>;
    /// 返回 terminal_id 当前目录；未知为 None。core 仅为当前入口可操作的终端调用此方法。
    fn terminal_cwd(&self, _terminal_id: &str) -> Result<Option<String>, AppError> {
        Ok(None)
    }
    /// 同步任务状态；task 为任务 ID 或名称，None 表示全部
    fn sync_status(&self, task: Option<&str>) -> Result<Vec<SyncTaskState>, AppError>;
    /// 让本机 Syncthing 立即扫描这个文件夹，扫完返回
    fn sync_rescan(&self, folder_id: &str, timeout: Duration) -> Result<(), AppError>;
    /// 为 AI 在 Windows 和服务器之间传文件，新开一条到 server_id 的 SFTP 会话（和文件栏的传输一样单独开）。
    /// 后台方式：不弹密码框，缺密码或验证码时返回 ssh.password_needed / ssh.two_factor_required；会话随返回值释放
    fn open_sftp(&self, server_id: &str) -> Result<crate::sftp::SftpSession, AppError>;
    /// 把要发给服务器 AI 终端的图片传到 server_id 的临时目录（只有自己能读，和界面在服务器终端里粘贴图片一样，
    /// 最多等 30 秒），返回服务器上的绝对路径（只有字母、数字、横线和点，不用加引号）。
    /// extension 是 png、jpg、gif 或 webp；后台方式连接，不弹密码框
    fn upload_image(
        &self,
        server_id: &str,
        bytes: &[u8],
        extension: &str,
    ) -> Result<String, AppError>;
    /// 通知界面（不阻塞）
    fn notify(&self, event: HostEvent);
    /// 服务器入口在服务器上实际监听的端口（另一台电脑占着设置的端口时会换一个）；入口没在运行为 None
    fn entry_port(&self, _server_id: &str) -> Option<u16> {
        None
    }
    /// 在入口所在的服务器上运行只读脚本（后台方式，不弹密码框），返回脚本输出的 JSON；timeout 内没做完返回错误
    fn entry_script(
        &self,
        _server_id: &str,
        _script: &str,
        _timeout: Duration,
    ) -> Result<serde_json::Value, AppError> {
        Err(AppError::new("mcp.peers_unsupported"))
    }
    /// 本机经这台服务器到服务器上 127.0.0.1:port 的本地转发端口（宿主按需建立并复用，入口停用时一起关）
    fn peer_tunnel(&self, _server_id: &str, _port: u16) -> Result<u16, AppError> {
        Err(AppError::new("mcp.peers_unsupported"))
    }
    /// 这台电脑多久没有键盘鼠标输入（毫秒）；不知道为 None
    fn idle_ms(&self) -> Option<u64> {
        None
    }
    /// 这台电脑上的用户做的指定（AI 操作哪台电脑）；没指定过为 None
    fn pin(&self) -> Option<super::peers::PinRecord> {
        None
    }
    /// Pierlink 自己的基本情况（pierlink_status 工具用）：服务器的名字、分组、连接方式和在线状态，打开着的终端、
    /// 桌面、同步任务、AI 接入入口。不能含地址、端口、用户名、密钥、密码、令牌、备注和路径
    fn pierlink_status(&self) -> Result<serde_json::Value, AppError> {
        Ok(serde_json::json!({}))
    }
}
