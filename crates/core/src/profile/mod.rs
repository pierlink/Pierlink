//! 服务器与应用设置的可移植配置；不迁移同步数据，也不读取凭据或私钥内容。
//! JSON 最深 32 层、总计最多 50 万个节点，每台服务器最多 128 个身份文件路径。
//! 主机指纹原样保留；此模块不迁移 known_hosts，连接时仍由 SSH 校验公钥信任。
mod validation;

use crate::{
    error::AppError,
    storage::{Db, PortForward, Server, Snippet},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use specta::Type;
use std::{collections::BTreeMap, time::SystemTime};

/// 配置文件格式标识。
pub const PROFILE_FORMAT: &str = "pierlink-profile";
/// 当前支持的配置版本。
pub const PROFILE_VERSION: u32 = 1;
/// JSON 文件上限为 32 MiB；调用方读取文件时也应使用此上限。
pub const MAX_PROFILE_BYTES: usize = 32 * 1024 * 1024;
/// 服务器、转发、片段分别最多 5000 条。
pub const MAX_PROFILE_ITEMS: usize = 5000;
/// 单个字符串（包括 JSON 键）最多 64 KiB，按 UTF-8 字节计。
pub const MAX_STRING_BYTES: usize = 64 * 1024;

/// 导出时的应用版本和不透明设置。调用方必须先剔除密码、口令、令牌等秘密。
/// `settings` 可同时承载应用设置与 MCP 设置；核心只限制大小，不解释或改写内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileExtras {
    pub app_version: String,
    pub settings: Value,
}

/// 可序列化为 JSON 的迁移文件。时间为 UTC Unix 毫秒。
/// `extras` 原样保留调用方的设置，应用版本提升到头部；导入不会应用这些设置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub format: String,
    pub version: u32,
    pub exported_at: i64,
    pub app_version: String,
    pub servers: Vec<Server>,
    pub port_forwards: Vec<PortForward>,
    pub snippets: Vec<Snippet>,
    pub extras: Value,
}

/// 条目被跳过或停用的原因，可直接用于界面翻译。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ImportReason {
    /// 同一配置已存在，或在文件内重复。
    Duplicate,
    /// 跳板机不在文件中，或它已因无效引用被跳过。
    MissingJumpServer,
    /// 跳板机引用形成环路，或依赖环路中的服务器。
    JumpCycle,
    /// 转发引用的服务器不在文件中，或未能导入。
    MissingServer,
    /// 本机监听端口与已有或本次导入的转发冲突。
    LocalPortConflict,
}

/// 跳过或停用的源条目；`id` 是文件中的旧 ID，片段的 `name` 为标题。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ImportIssue {
    pub id: String,
    pub name: String,
    pub reason: ImportReason,
}

/// 新增服务器的私钥路径不是本机可用文件；路径原样保留且不会读取文件内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MissingIdentityFile {
    /// 导入后的服务器 ID。
    pub server_id: String,
    pub name: String,
    pub path: String,
}

/// 一个事务的导入结果；跳过数等于对应列表长度。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ImportReport {
    pub servers_added: usize,
    pub servers_skipped: Vec<ImportIssue>,
    pub missing_identity_files: Vec<MissingIdentityFile>,
    pub forwards_added: usize,
    pub forwards_skipped: Vec<ImportIssue>,
    pub forwards_disabled: Vec<ImportIssue>,
    pub snippets_added: usize,
    pub snippets_skipped: Vec<ImportIssue>,
    /// 旧服务器 ID → 新增或已存在的服务器 ID；缺失引用和环路条目没有映射。
    pub server_id_map: BTreeMap<String, String>,
}

/// 从 db 的同一读取快照导出服务器、转发、片段，以及调用方传入的 extras。
/// 返回清除密码引用及记住密码标记的配置；不读取设置表、同步数据、凭据或私钥。
/// extras 必须只含非敏感设置；超限返回 `profile.limit_exceeded`，数据库错误原样返回。
pub fn export_profile(db: &Db, extras: ProfileExtras) -> Result<Profile, AppError> {
    let exported_at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .ok_or_else(|| error("profile.invalid_time", "系统时间无法表示为 Unix 毫秒"))?;
    let mut profile = Profile {
        format: PROFILE_FORMAT.into(),
        version: PROFILE_VERSION,
        exported_at,
        app_version: extras.app_version,
        servers: vec![],
        port_forwards: vec![],
        snippets: vec![],
        extras: extras.settings,
    };
    crate::storage::profile::read(db, &mut profile)?;
    for server in &mut profile.servers {
        clear_credentials(server);
    }
    validation::validate(&profile)?;
    Ok(profile)
}

/// 校验不可信 profile，在 db 的一个事务内去重并插入；失败时所有表保持原样。
/// 返回新增数量、跳过/停用原因、缺失私钥路径和服务器 ID 映射，不应用 extras。
/// 即使文件携带密码引用或记住密码标记，也会清除；已有服务器与凭据保持原样。
pub fn import_profile(db: &mut Db, profile: &Profile) -> Result<ImportReport, AppError> {
    validation::validate(profile)?;
    crate::storage::profile::import(db, profile)
}

/// 从不可信 JSON 字节解析配置，限制文件大小并校验结构及容量；不访问数据库。
/// 返回 Profile；格式/版本不兼容、JSON 损坏或超限时返回不包含文件原文的 AppError。
/// 调用方应先限制文件读取大小至 MAX_PROFILE_BYTES，再调用本函数。
pub fn parse_profile(json: &[u8]) -> Result<Profile, AppError> {
    validation::limit("file_bytes", json.len(), MAX_PROFILE_BYTES)?;
    let value: Value = serde_json::from_slice(json)
        .map_err(|_| error("profile.invalid_json", "配置文件不是有效 JSON"))?;
    validation::validate_json(&value)?;
    if value.get("format").and_then(Value::as_str) != Some(PROFILE_FORMAT) {
        return Err(error("profile.invalid_format", "不是 Pierlink 配置文件"));
    }
    if let Some(version) = value.get("version").and_then(Value::as_u64)
        && version != u64::from(PROFILE_VERSION)
    {
        return Err(validation::version_error(version));
    }
    let profile = serde_json::from_value(value)
        .map_err(|_| error("profile.invalid_json", "配置文件字段缺失或类型不正确"))?;
    validation::validate(&profile)?;
    Ok(profile)
}

/// 清除 server 中的密码引用和保存标记；其他配置保持原值。
pub(crate) fn clear_credentials(server: &mut Server) {
    server.password_cred = None;
    server.remember_password = false;
}

fn error(code: &str, reason: &str) -> AppError {
    AppError::new(code).with("reason", reason)
}

#[cfg(test)]
mod tests;
