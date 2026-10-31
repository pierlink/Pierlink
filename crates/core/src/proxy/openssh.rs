//! 在实际启动时读取代理；用户已有的 ProxyCommand / ProxyJump 优先。
use crate::{error::AppError, process::SshProgram, ssh::args::SshCommand};
use std::{path::Path, time::Duration};

pub(crate) struct Prepared {
    pub command: SshCommand,
    pub config: Option<tempfile::TempDir>,
}

pub(crate) fn prepare(program: &SshProgram, command: &SshCommand) -> Result<Prepared, AppError> {
    if matches!(super::settings().mode, super::ProxyMode::Off)
        || command.builtin.is_some()
        || command
            .args
            .iter()
            .any(|s| matches!(s.as_str(), "-G" | "-V"))
    {
        return Ok(Prepared {
            command: command.clone(),
            config: None,
        });
    }
    let executable = std::env::current_exe().map_err(|_| invalid())?;
    prepare_with_exe(program, command, &executable)
}

fn invalid() -> AppError {
    super::map_error(&pierlink_ssh_engine::proxy::ProxyError::InvalidInput)
}

pub(super) fn prepare_with_exe(
    program: &SshProgram,
    command: &SshCommand,
    executable: &Path,
) -> Result<Prepared, AppError> {
    prepare_with_system(
        program,
        command,
        executable,
        &super::settings(),
        super::system_proxy,
    )
}

fn prepare_with_system(
    program: &SshProgram,
    command: &SshCommand,
    executable: &Path,
    settings: &super::ProxySettings,
    system: impl FnOnce() -> super::SystemProxy,
) -> Result<Prepared, AppError> {
    let mut prepared = Prepared {
        command: command.clone(),
        config: None,
    };
    // 先读取一次有效设置；没有静态代理时不生成临时配置，也不启动 ssh -G。
    let Some((proxy, bypass)) = super::active(settings, system) else {
        return Ok(prepared);
    };
    let use_proxy =
        |host: &str| !super::bypass::loopback(host) && !super::bypass::matches(host, &bypass);
    // 只有应用参数构造器生成的手动链在第一跳注入；用户的跳板链完整保留。
    if let Some(text) = command
        .config_text
        .as_ref()
        .filter(|s| s.starts_with("Host pierlink-0\n"))
    {
        let host = text
            .lines()
            .skip(1)
            .take_while(|s| !s.starts_with("Host "))
            .find_map(|s| s.trim().strip_prefix("HostName "))
            .map(|s| s.trim_matches('"'))
            .ok_or_else(invalid)?;
        if use_proxy(host) {
            pierlink_ssh_engine::proxy::validate(&proxy, host, 22)
                .map_err(|e| super::map_error(&e))?;
            let text = text.replacen(
                "Host pierlink-0\n",
                &format!(
                    "Host pierlink-0\n    ProxyCommand {}\n",
                    helper(executable)?
                ),
                1,
            );
            materialize(&mut prepared, &text)?;
            prepared.command.env.extend(super::environment(&proxy));
        }
        return Ok(prepared);
    }
    if let Some(text) = &command.config_text {
        materialize(&mut prepared, text)?;
    }
    let cfg = crate::ssh::config_file::resolve_command(
        program,
        &prepared.command.args,
        Duration::from_secs(5),
    )?;
    if cfg.proxy_command.is_some() || cfg.proxy_jump.is_some() {
        return Ok(prepared);
    }
    if use_proxy(&cfg.hostname) {
        pierlink_ssh_engine::proxy::validate(&proxy, &cfg.hostname, cfg.port)
            .map_err(|e| super::map_error(&e))?;
        prepared.command.args.splice(
            0..0,
            ["-o".into(), format!("ProxyCommand={}", helper(executable)?)],
        );
        prepared.command.env.extend(super::environment(&proxy));
    }
    Ok(prepared)
}

fn materialize(prepared: &mut Prepared, text: &str) -> Result<(), AppError> {
    #[cfg(test)]
    let root = crate::test_support::tempdir();
    #[cfg(not(test))]
    let root = {
        let base = std::env::var_os("PIERLINK_TEST_TMP")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        tempfile::Builder::new()
            .prefix("pierlink-proxy-")
            .tempdir_in(base)
            .map_err(|_| invalid())?
    };
    let path = root.path().join("ssh.conf");
    std::fs::write(&path, text).map_err(|_| invalid())?;
    let path = path.to_str().ok_or_else(invalid)?.to_owned();
    if let Some(index) = prepared.command.args.iter().position(|s| s == "-F") {
        *prepared
            .command
            .args
            .get_mut(index + 1)
            .ok_or_else(invalid)? = path;
    } else {
        prepared.command.args.splice(0..0, ["-F".into(), path]);
    }
    prepared.command.config_text = None;
    prepared.config = Some(root);
    Ok(())
}

fn helper(executable: &Path) -> Result<String, AppError> {
    helper_for_platform(executable, cfg!(windows))
}

fn helper_for_platform(executable: &Path, windows: bool) -> Result<String, AppError> {
    let path = executable.to_str().ok_or_else(invalid)?;
    if path.is_empty() || path.contains(['\0', '\r', '\n']) {
        return Err(invalid());
    }
    // % 在 OpenSSH 的令牌展开之前转义；Windows 的 ProxyCommand 由 CreateProcess 解析双引号。
    let path = path.replace('%', "%%");
    let quoted = if windows {
        if path.contains('"') || path.ends_with('\\') {
            return Err(invalid());
        }
        format!("\"{path}\"")
    } else {
        crate::ssh::quote::sh_quote(&path)?
    };
    Ok(format!("{quoted} --proxy-connect %h %p"))
}

#[cfg(test)]
mod tests;
