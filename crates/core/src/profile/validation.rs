//! 不可信配置的容量和字段校验；错误只包含固定字段名、数量及原因。
use super::*;
use crate::storage::ServerSource;
use std::{collections::HashSet, io::Write};

const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 500_000;
const MAX_IDENTITY_FILES: usize = 128;

pub(super) fn limit(field: &str, actual: usize, maximum: usize) -> Result<(), AppError> {
    if actual > maximum {
        return Err(error("profile.limit_exceeded", "配置文件超过容量限制")
            .with("field", field)
            .with("limit", maximum)
            .with("actual", actual));
    }
    Ok(())
}

pub(super) fn version_error(version: u64) -> AppError {
    error(
        "profile.unsupported_version",
        "配置版本不受支持，请使用兼容的应用版本",
    )
    .with("version", version)
    .with("supported_version", PROFILE_VERSION)
}

fn invalid(field: &str) -> AppError {
    error("profile.invalid_field", "配置字段缺失或内容不合法").with("field", field)
}

pub(super) fn validate(profile: &Profile) -> Result<(), AppError> {
    if profile.format != PROFILE_FORMAT {
        return Err(error("profile.invalid_format", "不是 Pierlink 配置文件"));
    }
    if profile.version != PROFILE_VERSION {
        return Err(version_error(u64::from(profile.version)));
    }
    for (field, count) in [
        ("servers", profile.servers.len()),
        ("port_forwards", profile.port_forwards.len()),
        ("snippets", profile.snippets.len()),
    ] {
        limit(field, count, MAX_PROFILE_ITEMS)?;
    }
    // 计入根对象、四个头部值及三个实体数组，与完整 JSON 的节点和深度限制一致。
    let mut budget = Budget {
        nodes: 8,
        string_bytes: 0,
    };
    budget.string(&profile.app_version)?;
    // 先遍历不透明 JSON，避免对调用方构造的深层 Value 递归序列化。
    budget.visit(&profile.extras, 1)?;
    let mut ids = HashSet::new();
    for server in &profile.servers {
        check_id(&server.id, "servers.id", &mut ids)?;
        limit(
            "identity_files",
            server.identity_files.len(),
            MAX_IDENTITY_FILES,
        )?;
        budget.record(server)?;
        if server.name.trim().is_empty() || server.name.contains('\0') {
            return Err(invalid("servers.name"));
        }
        match server.source {
            ServerSource::Manual => {
                if server.host.as_deref().is_none_or(|s| {
                    s.is_empty()
                        || s.starts_with('-')
                        || s.chars().any(|c| c.is_control() || c.is_whitespace())
                }) {
                    return Err(invalid("servers.host"));
                }
            }
            ServerSource::SshConfig => {
                let alias = server
                    .ssh_alias
                    .as_deref()
                    .ok_or_else(|| invalid("servers.ssh_alias"))?;
                crate::ssh::config_file::validate_alias(alias)
                    .map_err(|_| invalid("servers.ssh_alias"))?;
            }
        }
        if server.port == Some(0) {
            return Err(invalid("servers.port"));
        }
    }
    ids.clear();
    for forward in &profile.port_forwards {
        check_id(&forward.id, "port_forwards.id", &mut ids)?;
        budget.record(forward)?;
        crate::tunnel::forward::validate(forward)?;
    }
    ids.clear();
    for snippet in &profile.snippets {
        check_id(&snippet.id, "snippets.id", &mut ids)?;
        budget.record(snippet)?;
    }
    // 使用计数写入器，不为容量检查再分配一份完整 JSON；转义后的字节也计入上限。
    serde_json::to_writer(ByteLimit(0), profile).map_err(|_| {
        error("profile.limit_exceeded", "配置序列化后超过文件大小限制")
            .with("field", "file_bytes")
            .with("limit", MAX_PROFILE_BYTES)
    })?;
    Ok(())
}

fn check_id<'a>(id: &'a str, field: &str, ids: &mut HashSet<&'a str>) -> Result<(), AppError> {
    if id.trim().is_empty() || id.contains('\0') {
        return Err(invalid(field));
    }
    if !ids.insert(id) {
        return Err(error("profile.duplicate_id", "同类条目含有重复 ID").with("field", field));
    }
    Ok(())
}

pub(super) fn validate_json(value: &Value) -> Result<(), AppError> {
    Budget::default().visit(value, 0)
}

#[derive(Default)]
struct Budget {
    nodes: usize,
    string_bytes: usize,
}

impl Budget {
    fn string(&mut self, value: &str) -> Result<(), AppError> {
        limit("string_bytes", value.len(), MAX_STRING_BYTES)?;
        self.string_bytes = self.string_bytes.saturating_add(value.len());
        limit("total_string_bytes", self.string_bytes, MAX_PROFILE_BYTES)
    }

    fn record(&mut self, record: &impl Serialize) -> Result<(), AppError> {
        let value = serde_json::to_value(record)
            .map_err(|_| error("profile.invalid_json", "配置条目无法序列化"))?;
        self.visit(&value, 2)
    }

    fn visit(&mut self, value: &Value, depth: usize) -> Result<(), AppError> {
        limit("json_depth", depth, MAX_DEPTH)?;
        self.nodes += 1;
        limit("json_nodes", self.nodes, MAX_NODES)?;
        match value {
            Value::String(s) => self.string(s)?,
            Value::Array(values) => {
                for value in values {
                    self.visit(value, depth + 1)?;
                }
            }
            Value::Object(values) => {
                for (key, value) in values {
                    self.string(key)?;
                    self.visit(value, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

struct ByteLimit(usize);

impl Write for ByteLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_PROFILE_BYTES {
            return Err(std::io::Error::other("配置文件超过大小限制"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
