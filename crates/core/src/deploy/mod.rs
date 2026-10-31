//! 经受限 SSH 脚本管理服务器实例；不执行前端提供的命令。
mod directory;
mod lifecycle;
pub use directory::{DirectoryCheck, check_sync_directory};
#[cfg(all(test, unix))]
mod builtin_tests;
mod model;
#[cfg(all(test, unix))]
mod tests;
mod upload;
use crate::{
    error::AppError,
    process::SshProgram,
    remote::run::{RunLimits, run_script},
    ssh::{args::SshCommand, quote::sh_quote},
};
pub use lifecycle::{create_instance, read_api_key, start_instance, stop_instance, uninstall};
pub use model::*;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::time::Duration;
pub use upload::upload_binary;
pub(crate) use upload::{upload_binary_to, upload_command};

static COMPUTER_SHORT_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
/// 应用启动时传入 settings 中的 device.short_id（八位小写字母数字）；重复设置同值成功，换值报错。
pub fn set_computer_short_id(short_id: &str) -> Result<(), AppError> {
    if short_id.len() != 8
        || !short_id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(err("invalid_input"));
    }
    let current = COMPUTER_SHORT_ID.get_or_init(|| short_id.to_owned());
    if current != short_id {
        return Err(err("identity_conflict"));
    }
    Ok(())
}

fn err(code: &str) -> AppError {
    AppError::new(format!("deploy.{code}"))
}
fn quote(s: &str) -> Result<String, AppError> {
    sh_quote(s).map_err(|_| err("invalid_input"))
}
fn run(program: &SshProgram, cmd: &SshCommand, body: &str, write: bool) -> Result<Value, AppError> {
    let short_id = COMPUTER_SHORT_ID
        .get()
        .ok_or_else(|| err("identity_required"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| err("clock_invalid"))?
        .as_secs();
    let owner = format!("{short_id}-{now}-{}", uuid::Uuid::new_v4());
    let script = format!(
        "{}\nowner={}\n{}\n{}",
        include_str!("common.sh"),
        quote(&owner)?,
        if write { "acquire" } else { "" },
        body
    );
    let value = run_script(
        program,
        cmd,
        &script,
        RunLimits {
            timeout: Duration::from_secs(240),
            stdout_bytes: 20 * 1024 * 1024,
            stderr_bytes: 4096,
        },
    )
    .map_err(|e| err("remote_failed").with("cause", e.code))?;
    if let Some(code) = value.get("error").and_then(Value::as_str) {
        let allowed = [
            "no_sha256_tool",
            "binary_missing",
            "invalid_api_key",
            "unsafe_path",
            "permission_denied",
            "locked",
            "cannot_probe_ports",
            "missing_base64",
            "missing_gzip",
            "checksum_mismatch",
            "upload_conflict",
            "generate_failed",
            "config_changed",
            "instance_changed",
            "not_found",
            "start_failed",
            "stop_failed",
            "missing_setsid",
            "missing_nohup",
            "invalid_pid",
            "shared_binary",
            "instance_running",
        ];
        let suffix = code
            .strip_prefix("deploy.")
            .filter(|s| allowed.contains(s))
            .ok_or_else(|| err("invalid_report"))?;
        if let Some(tool) = suffix.strip_prefix("missing_") {
            return Err(err("missing_tool").with("tool", tool));
        }
        return Err(err(suffix));
    }
    Ok(value)
}
fn decode<T: DeserializeOwned>(value: Value) -> Result<T, AppError> {
    serde_json::from_value(value).map_err(|_| err("invalid_report"))
}

/// 使用 program 和后台 cmd 只读检查服务器；noexec 探针立即删除，缺 machine-id 时保存后备身份。
pub fn inspect(program: &SshProgram, cmd: &SshCommand) -> Result<ServerReport, AppError> {
    let value = run(program, cmd, include_str!("inspect.sh"), false)?;
    let mut report: ServerReport = decode(value.clone())?;
    report.os_name = report.os_name.map(|name| name.chars().take(80).collect());
    if let Some(instance) = &mut report.instance {
        instance.devices = lifecycle::devices(
            value["config"]
                .as_str()
                .ok_or_else(|| err("invalid_report"))?,
        )
        .unwrap_or_default();
        instance.devices.retain(|d| d != &instance.info.device_id);
    }
    report.validate()?;
    Ok(report)
}
