//! 与数据库迁移对应的数据模型；凭据字段只保存条目引用。
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};
use specta::Type;

macro_rules! text_enum {
    ($name:ident, $doc:literal, {$($variant:ident => $value:literal),+}) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
        pub enum $name {
            $(#[serde(rename = $value)] $variant),+
        }
        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(match self { $(Self::$variant => $value),+ }))
            }
        }
        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                match value.as_str()? {
                    $($value => Ok(Self::$variant),)+
                    _ => Err(FromSqlError::InvalidType),
                }
            }
        }
    };
}
text_enum!(ServerSource, "服务器来源。", {Manual => "manual", SshConfig => "ssh_config"});
text_enum!(Engine, "服务器的连接方式；实际分派还需检查功能是否支持内置。", {Builtin => "builtin", OpenSsh => "openssh"});
text_enum!(AuthMethod, "认证方式；不包含秘密内容。", {Key => "key", Agent => "agent", Password => "password", DedicatedKey => "dedicated_key"});
text_enum!(ConflictPolicy, "同步冲突处理方式。", {Ask => "ask", Newest => "newest"});
text_enum!(EndpointMode, "本机或服务器端点的同步方向。", {SendReceive => "sendreceive", ReceiveOnly => "receiveonly"});

/// 服务器，时间字段为 Unix 毫秒。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Server {
    pub id: String,
    pub name: String,
    pub group_name: Option<String>,
    pub favorite: bool,
    pub sort_order: i64,
    pub source: ServerSource,
    pub connection_engine: Engine,
    /// 使用系统 OpenSSH 的说明；不保存凭据或服务器返回的原文。
    pub engine_note: Option<String>,
    pub ssh_alias: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub auth_method: AuthMethod,
    pub identity_file: Option<String>,
    /// 转换时保留的完整身份文件列表；空列表沿用原认证方式。
    #[serde(default)]
    pub identity_files: Vec<String>,
    #[serde(default)]
    pub identities_only: bool,
    /// 主机密钥查询别名，不改变连接地址。
    #[serde(default)]
    pub host_key_alias: Option<String>,
    pub jump_server_id: Option<String>,
    pub login_shell: Option<String>,
    /// true 跟随全局命令预测设置，false 关闭；旧迁移包缺省为跟随全局。
    #[serde(default = "default_command_prediction")]
    pub command_prediction: bool,
    pub use_tmux: bool,
    pub remember_password: bool,
    pub password_cred: Option<String>,
    pub host_key_sha256: Option<String>,
    pub vnc_autostart: bool,
    pub notes: String,
    pub created_at: i64,
    pub updated_at: i64,
}
impl Server {
    pub(super) fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            group_name: row.get("group_name")?,
            favorite: row.get("favorite")?,
            sort_order: row.get("sort_order")?,
            source: row.get("source")?,
            connection_engine: row.get("connection_engine")?,
            engine_note: row.get("engine_note")?,
            ssh_alias: row.get("ssh_alias")?,
            host: row.get("host")?,
            port: row.get("port")?,
            username: row.get("username")?,
            auth_method: row.get("auth_method")?,
            identity_file: row.get("identity_file")?,
            identity_files: serde_json::from_str(&row.get::<_, String>("identity_files")?)
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
            identities_only: row.get("identities_only")?,
            host_key_alias: row.get("host_key_alias")?,
            jump_server_id: row.get("jump_server_id")?,
            login_shell: row.get("login_shell")?,
            command_prediction: row.get("command_prediction")?,
            use_tmux: row.get("use_tmux")?,
            remember_password: row.get("remember_password")?,
            password_cred: row.get("password_cred")?,
            host_key_sha256: row.get("host_key_sha256")?,
            vnc_autostart: row.get("vnc_autostart")?,
            notes: row.get("notes")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

fn default_command_prediction() -> bool {
    true
}

fn default_sync_mode() -> EndpointMode {
    EndpointMode::SendReceive
}

/// 同步任务，时间字段为 Unix 毫秒。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SyncTask {
    pub id: String,
    pub folder_id: String,
    pub name: String,
    pub local_path: String,
    #[serde(default = "default_sync_mode")]
    pub local_mode: EndpointMode,
    pub conflict_policy: ConflictPolicy,
    pub collab_mode: bool,
    pub watch_delay_s: i64,
    pub versioning_enabled: bool,
    pub versioning_days: i64,
    pub ignore_rules: String,
    pub editor_id: Option<String>,
    pub paused: bool,
    pub created_at: i64,
    pub updated_at: i64,
}
impl SyncTask {
    pub(super) fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            folder_id: row.get("folder_id")?,
            name: row.get("name")?,
            local_path: row.get("local_path")?,
            local_mode: row.get("local_mode")?,
            conflict_policy: row.get("conflict_policy")?,
            collab_mode: row.get("collab_mode")?,
            watch_delay_s: row.get("watch_delay_s")?,
            versioning_enabled: row.get("versioning_enabled")?,
            versioning_days: row.get("versioning_days")?,
            ignore_rules: row.get("ignore_rules")?,
            editor_id: row.get("editor_id")?,
            paused: row.get("paused")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

/// 同步任务的服务器端点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SyncEndpoint {
    pub task_id: String,
    pub server_id: String,
    pub remote_path: String,
    pub mode: EndpointMode,
}
impl SyncEndpoint {
    pub(super) fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            task_id: row.get("task_id")?,
            server_id: row.get("server_id")?,
            remote_path: row.get("remote_path")?,
            mode: row.get("mode")?,
        })
    }
}

/// 服务器端 Pierlink 实例（每台服务器最多一个）；凭据字段只保存凭据管理器里的条目名。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInstance {
    pub server_id: String,
    pub instance_id: String,
    pub syncthing_version: String,
    pub arch: String,
    pub state_dir: String,
    pub socket_path: String,
    pub sync_port: u16,
    pub device_id: String,
    pub apikey_cred: String,
    pub gui_password_cred: String,
    pub autostart_consent: bool,
    pub deployed_at: i64,
    pub updated_at: i64,
}
impl ServerInstance {
    pub(super) fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            server_id: row.get("server_id")?,
            instance_id: row.get("instance_id")?,
            syncthing_version: row.get("syncthing_version")?,
            arch: row.get("arch")?,
            state_dir: row.get("state_dir")?,
            socket_path: row.get("socket_path")?,
            sync_port: row.get("sync_port")?,
            device_id: row.get("device_id")?,
            apikey_cred: row.get("apikey_cred")?,
            gui_password_cred: row.get("gui_password_cred")?,
            autostart_consent: row.get("autostart_consent")?,
            deployed_at: row.get("deployed_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

text_enum!(ForwardKind, "端口转发类型。", {Local => "local", Remote => "remote", Dynamic => "dynamic"});

/// 持久化的端口转发；enabled 表示应用启动时自动开启。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PortForward {
    pub id: String,
    pub server_id: String,
    pub name: String,
    pub kind: ForwardKind,
    /// 本地/SOCKS 为本机监听端口，远程转发为本机目标端口。
    pub local_port: u16,
    /// 仅本地转发必填；其他类型必须为空。
    pub remote_host: Option<String>,
    /// 本地转发为远端目标端口，远程转发为服务器监听端口；SOCKS 必须为空。
    pub remote_port: Option<u16>,
    pub enabled: bool,
    pub sort_order: i64,
}
impl PortForward {
    pub(super) fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            server_id: row.get("server_id")?,
            name: row.get("name")?,
            kind: row.get("kind")?,
            local_port: row.get("local_port")?,
            remote_host: row.get("remote_host")?,
            remote_port: row.get("remote_port")?,
            enabled: row.get("enabled")?,
            sort_order: row.get("sort_order")?,
        })
    }
}

/// 命令片段；group_name 为空时适用于所有服务器，正文按原样保存。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Snippet {
    pub id: String,
    pub title: String,
    pub body: String,
    pub group_name: Option<String>,
    pub sort_order: i64,
}
impl Snippet {
    pub(super) fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            title: row.get("title")?,
            body: row.get("body")?,
            group_name: row.get("group_name")?,
            sort_order: row.get("sort_order")?,
        })
    }
}
