//! AI 接入（MCP）的设置：存在数据库设置表的 `mcp` 键里（JSON）。默认全部关闭。
use crate::data::DbState;
use pierlink_core::error::AppError;
use pierlink_core::mcp::{Approval, Entry, Permissions};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

const SETTINGS_KEY: &str = "mcp";

/// 审批方式（界面用的写法，和 core 的 Approval 一一对应）
#[derive(Serialize, Deserialize, Type, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDto {
    /// 执行命令、终端里回车、写文件每次都问
    AskAll,
    /// 只问危险操作（默认；旧设置里没有这一项时也是它）
    #[default]
    Dangerous,
    /// 全自动，只记录
    Auto,
}

impl From<ApprovalDto> for Approval {
    fn from(value: ApprovalDto) -> Self {
        match value {
            ApprovalDto::AskAll => Approval::AskAll,
            ApprovalDto::Dangerous => Approval::Dangerous,
            ApprovalDto::Auto => Approval::Auto,
        }
    }
}

/// 一个入口的权限和“总是允许”（默认全部关闭）
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct EntrySettingsDto {
    pub enabled: bool,
    /// 审批方式
    pub approval: ApprovalDto,
    /// 允许在 Windows 本机执行命令（等于把这个 Windows 账户交出去）
    pub windows_commands: bool,
    /// 本机文件工具允许的文件夹（绝对路径）
    pub folders: Vec<String>,
    /// 允许操作的服务器 ID
    pub servers: Vec<String>,
    /// 用户选过“总是允许”：第一次连接不再问
    pub remembered: bool,
}

/// 一台服务器上的入口
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServerEntrySettingsDto {
    pub server_id: String,
    /// 服务器上监听的端口（127.0.0.1）
    pub port: u16,
    pub entry: EntrySettingsDto,
}

/// AI 接入的全部设置
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct McpSettingsDto {
    /// 总开关
    pub enabled: bool,
    /// Windows 本机入口（agent 在这台电脑上）
    pub windows: EntrySettingsDto,
    /// 服务器入口（agent 在服务器上）
    pub servers: Vec<ServerEntrySettingsDto>,
}

impl Default for McpSettingsDto {
    fn default() -> Self {
        Self {
            enabled: false,
            // agent 在本机本来就能运行 PowerShell，关掉只是添麻烦；服务器入口默认不给
            windows: EntrySettingsDto {
                windows_commands: true,
                ..EntrySettingsDto::default()
            },
            servers: Vec::new(),
        }
    }
}

impl McpSettingsDto {
    /// 某个入口的设置；没有这个服务器入口时返回 None
    pub fn entry(&self, entry: &Entry) -> Option<&EntrySettingsDto> {
        match entry {
            Entry::Windows => Some(&self.windows),
            Entry::Server { server_id } => self
                .servers
                .iter()
                .find(|s| &s.server_id == server_id)
                .map(|s| &s.entry),
        }
    }

    fn entry_mut(&mut self, entry: &Entry) -> Option<&mut EntrySettingsDto> {
        match entry {
            Entry::Windows => Some(&mut self.windows),
            Entry::Server { server_id } => self
                .servers
                .iter_mut()
                .find(|s| &s.server_id == server_id)
                .map(|s| &mut s.entry),
        }
    }

    /// 入口的权限；总开关或这个入口关闭时返回 None
    pub fn permissions(&self, entry: &Entry) -> Option<Permissions> {
        if !self.enabled {
            return None;
        }
        let e = self.entry(entry).filter(|e| e.enabled)?;
        Some(Permissions {
            approval: e.approval.into(),
            windows_commands: e.windows_commands,
            folders: e.folders.iter().map(PathBuf::from).collect(),
            servers: e.servers.clone(),
        })
    }
}

/// 读设置；没存过或读不出来时返回默认值（全部关闭）
///
/// :param app: 应用句柄
/// :return: 当前设置
pub fn load(app: &AppHandle) -> McpSettingsDto {
    app.state::<DbState>()
        .with(|db| db.get_setting::<McpSettingsDto>(SETTINGS_KEY))
        .unwrap_or_else(|e| {
            eprintln!("读取 AI 接入设置失败，按全部关闭处理: 错误={}", e.code);
            None
        })
        .unwrap_or_default()
}

/// 检查并保存设置
///
/// :param app: 应用句柄
/// :param settings: 界面提交的设置
/// :return: 保存后的设置；文件夹不是绝对路径、端口不合法或重复时返回 server.invalid_field 类的错误
pub fn save(app: &AppHandle, settings: McpSettingsDto) -> Result<McpSettingsDto, AppError> {
    let settings = normalize(settings)?;
    app.state::<DbState>()
        .with(|db| db.set_setting(SETTINGS_KEY, &settings))?;
    Ok(settings)
}

/// 把某个入口标为“总是允许”或取消
///
/// :param app: 应用句柄
/// :param entry: 入口
/// :param remembered: 是否总是允许
pub fn set_remembered(app: &AppHandle, entry: &Entry, remembered: bool) -> Result<(), AppError> {
    let mut settings = load(app);
    if let Some(e) = settings.entry_mut(entry) {
        e.remembered = remembered;
    }
    save(app, settings).map(|_| ())
}

fn invalid(field: &str) -> AppError {
    AppError::new("mcp.invalid_setting").with("field", field)
}

fn normalize_entry(mut e: EntrySettingsDto) -> Result<EntrySettingsDto, AppError> {
    let mut folders = Vec::new();
    for folder in e.folders {
        let folder = folder.trim().to_owned();
        if folder.is_empty() {
            continue;
        }
        if !Path::new(&folder).is_absolute() {
            return Err(invalid("folders").with("value", folder));
        }
        // 网络盘上的文件夹不能授权（规范化成 UNC 后会被拒，AI 那边看不到它）：保存时就说清楚。
        // 只拦这一种：暂时不存在的文件夹（如移动硬盘没插）照常保存，AI 列授权目录时会标出原因
        if let Err(e) = pierlink_core::mcp::tools::files::validate_folder(Path::new(&folder))
            && e.code == "mcp.network_folder"
        {
            return Err(e);
        }
        if !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    e.folders = folders;
    e.servers.sort();
    e.servers.dedup();
    Ok(e)
}

fn normalize(mut settings: McpSettingsDto) -> Result<McpSettingsDto, AppError> {
    settings.windows = normalize_entry(settings.windows)?;
    let mut servers: Vec<ServerEntrySettingsDto> = Vec::new();
    for mut s in settings.servers {
        if s.port < 1024 {
            return Err(invalid("port").with("value", s.port));
        }
        // 同一台服务器只留第一条
        if servers.iter().any(|x| x.server_id == s.server_id) {
            continue;
        }
        s.entry = normalize_entry(s.entry)?;
        servers.push(s);
    }
    settings.servers = servers;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 服务器入口的默认端口（界面新建入口时用这个值）
    const DEFAULT_PORT: u16 = 47821;

    fn server_entry(id: &str, port: u16, enabled: bool) -> ServerEntrySettingsDto {
        ServerEntrySettingsDto {
            server_id: id.into(),
            port,
            entry: EntrySettingsDto {
                enabled,
                ..EntrySettingsDto::default()
            },
        }
    }

    #[test]
    fn 默认全部关闭_本机入口默认允许执行命令() {
        let s = McpSettingsDto::default();
        assert!(!s.enabled && !s.windows.enabled);
        assert!(s.windows.windows_commands);
        assert_eq!(s.permissions(&Entry::Windows), None);
    }

    #[test]
    fn 总开关和入口开关都打开才有权限() {
        let mut s = McpSettingsDto::default();
        s.windows.enabled = true;
        assert_eq!(s.permissions(&Entry::Windows), None);
        s.enabled = true;
        let p = s.permissions(&Entry::Windows).unwrap();
        assert!(p.windows_commands);
        s.servers.push(server_entry("a", DEFAULT_PORT, false));
        let server = Entry::Server {
            server_id: "a".into(),
        };
        assert_eq!(s.permissions(&server), None);
        s.servers[0].entry.enabled = true;
        assert!(!s.permissions(&server).unwrap().windows_commands);
        assert_eq!(
            s.permissions(&Entry::Server {
                server_id: "b".into()
            }),
            None
        );
    }

    #[test]
    fn 保存前检查文件夹和端口并去重() {
        let mut s = McpSettingsDto::default();
        s.windows.folders = vec![" ".into(), "relative".into()];
        assert_eq!(
            normalize(s.clone()).unwrap_err().code,
            "mcp.invalid_setting"
        );
        let absolute = if cfg!(windows) { r"D:\work" } else { "/work" };
        s.windows.folders = vec![absolute.into(), absolute.into(), "".into()];
        s.windows.servers = vec!["b".into(), "a".into(), "b".into()];
        s.servers = vec![
            server_entry("a", DEFAULT_PORT, true),
            server_entry("a", 50000, true),
        ];
        let n = normalize(s.clone()).unwrap();
        assert_eq!(n.windows.folders, vec![absolute.to_owned()]);
        assert_eq!(n.windows.servers, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(n.servers.len(), 1);
        s.servers = vec![server_entry("a", 22, true)];
        assert_eq!(normalize(s).unwrap_err().code, "mcp.invalid_setting");
    }

    #[test]
    fn 旧数据缺字段时按默认值读() {
        let s: McpSettingsDto = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(s.enabled && s.windows.windows_commands && s.servers.is_empty());
    }
}
