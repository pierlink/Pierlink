//! 有总时限和输出上限的后台脚本执行。
use crate::{
    error::AppError,
    process::{Reader, SshProgram, io_error},
    security::redact,
    ssh::args::SshCommand,
};
use std::{
    io::Write,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy)]
pub struct RunLimits {
    pub timeout: Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}
impl Default for RunLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            stdout_bytes: 1024 * 1024,
            stderr_bytes: 64 * 1024,
        }
    }
}

const RULES: &[(&[&str], &str)] = &[
    (
        &["Too many authentication failures"],
        "ssh.too_many_auth_attempts",
    ),
    (
        &[
            "REMOTE HOST IDENTIFICATION HAS CHANGED",
            "REVOKED HOST KEY DETECTED",
        ],
        "ssh.host_key_changed",
    ),
    (
        &[
            "Host key verification failed",
            "No ED25519 host key is known",
            "No RSA host key is known",
            "No ECDSA host key is known",
        ],
        "ssh.host_key_unknown",
    ),
    (
        &[
            "Bad local forwarding specification",
            "Bad remote forwarding specification",
            "Bad dynamic forwarding specification",
        ],
        "ssh.invalid_argument",
    ),
    (
        &["administratively prohibited", "port forwarding is disabled"],
        "ssh.forwarding_prohibited",
    ),
    (
        &[
            "Permission denied (",
            "Permission denied, please try again.",
            "No more authentication methods to try",
        ],
        "ssh.auth_failed",
    ),
    (
        &[
            "Connection refused",
            "Connection timed out",
            "Connection reset",
            "Connection closed",
            "No route to host",
            "Network is unreachable",
            "Could not resolve hostname",
            "Operation timed out",
        ],
        "ssh.unreachable",
    ),
];

pub(crate) fn matched_ssh_error(stderr: &[u8]) -> Option<&'static str> {
    RULES
        .iter()
        .find(|(patterns, _)| {
            patterns
                .iter()
                .any(|p| stderr.windows(p.len()).any(|w| w == p.as_bytes()))
        })
        .map(|(_, code)| *code)
}

/// 按 OpenSSH 固定提示将退出码和 stderr 转为错误；secrets 是调用方已知秘密，尾部先脱敏再截断到 2048 字符。
pub fn classify_error(exit_code: Option<i32>, stderr: &str, secrets: &[&str]) -> AppError {
    if let Some((_, rest)) = stderr.split_once("remote port forwarding failed for listen port ") {
        let port = rest
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|p| p.parse::<u16>().ok());
        let mut error = error_with_tail("tunnel.remote_forward_failed", exit_code, stderr, secrets)
            .with_reason(
                "remote.server_listening_port_is_in_use_or_remote_forwarding_is",
                "服务器监听端口被占用，或服务器禁止远程转发",
            );
        if let Some(port) = port {
            error = error.with("port", port);
        }
        return error;
    }
    if stderr.contains("bind: Permission denied")
        || stderr
            .lines()
            .any(|line| line.contains("bind [") && line.contains(": Permission denied"))
    {
        return error_with_tail("tunnel.port_reserved", exit_code, stderr, secrets);
    }
    let code = RULES
        .iter()
        .find(|(patterns, _)| patterns.iter().any(|p| stderr.contains(p)))
        .map_or("ssh.failed", |(_, code)| *code);
    error_with_tail(code, exit_code, stderr, secrets)
}
fn error_with_tail(code: &str, exit_code: Option<i32>, stderr: &str, secrets: &[&str]) -> AppError {
    let safe = redact(stderr, secrets);
    let tail: String = safe
        .chars()
        .rev()
        .take(2048)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut e = AppError::new(code).with("stderr_tail", tail);
    if let Some(code) = exit_code {
        e = e.with("exit_code", code);
    }
    e
}

/// 用 program 和后台用途 target_cmd 执行可信 body，limits 限制总时间与两个输出流；返回帧内 JSON 或脱敏错误。
/// 退出码 255 或信号结束才归类为 SSH 错误；其他非零码返回 remote.script_failed，附退出码及脱敏尾部。
pub fn run_script(
    program: &SshProgram,
    target_cmd: &SshCommand,
    body: &str,
    limits: RunLimits,
) -> Result<serde_json::Value, AppError> {
    if let Some(command) = &target_cmd.builtin {
        return run_builtin(command, body, limits);
    }
    let start = Instant::now();
    let nonce = super::framing::new_nonce();
    let script = super::framing::wrap_script(body, &nonce);
    let mut process = program.spawn(target_cmd)?;
    let mut stdout = Reader::start(
        process
            .child
            .stdout
            .take()
            .ok_or_else(|| io_error("remote.get_standard_output", "获取标准输出"))?,
        limits.stdout_bytes,
    )?;
    let mut stderr = match Reader::start(
        process
            .child
            .stderr
            .take()
            .ok_or_else(|| io_error("remote.get_error_output", "获取错误输出"))?,
        limits.stderr_bytes,
    ) {
        Ok(r) => r,
        Err(e) => {
            let _ = process.terminate();
            stdout.join();
            return Err(e);
        }
    };
    let mut stdin = process
        .child
        .stdin
        .take()
        .ok_or_else(|| io_error("remote.get_standard_input", "获取标准输入"))?;
    let writer = thread::Builder::new()
        .name("ssh写入".into())
        .spawn(move || stdin.write_all(script.as_bytes()));
    let result = (|| {
        let writer = writer
            .as_ref()
            .map_err(|_| io_error("remote.create_writer_thread", "创建写入线程"))?;
        let mut exited = None;
        loop {
            if start.elapsed() >= limits.timeout {
                return Err(AppError::new("remote.timeout"));
            }
            let out = stdout
                .state
                .lock()
                .map_err(|_| io_error("remote.read_standard_output", "读取标准输出"))?;
            let err = stderr
                .state
                .lock()
                .map_err(|_| io_error("remote.read_error_output", "读取错误输出"))?;
            if out.exceeded || err.exceeded {
                return Err(AppError::new("remote.output_too_large")
                    .with("stream", if out.exceeded { "stdout" } else { "stderr" }));
            }
            if out.failed || err.failed {
                return Err(io_error("remote.read_pipe", "读取管道"));
            }
            if let Some(status) = process.try_wait()?
                && ((out.done && err.done && writer.is_finished())
                    || exited.get_or_insert_with(Instant::now).elapsed()
                        >= Duration::from_millis(500))
            {
                if !status.success() {
                    let secrets: Vec<_> = target_cmd.env.iter().map(|(_, v)| v.as_str()).collect();
                    let text = String::from_utf8_lossy(&err.bytes);
                    let error = if status.code().is_some_and(|code| code != 255) {
                        error_with_tail("remote.script_failed", status.code(), &text, &secrets)
                    } else {
                        classify_error(status.code(), &text, &secrets)
                    };
                    return Err(process.finish_error(error));
                }
                let text = std::str::from_utf8(&out.bytes)
                    .map_err(|_| AppError::new("remote.invalid_utf8"))?;
                return super::framing::parse_framed(text, &nonce, limits.stdout_bytes);
            }
            drop(out);
            drop(err);
            thread::sleep(Duration::from_millis(10));
        }
    })();
    let _ = process.terminate();
    stdout.join();
    stderr.join();
    let wrote = writer
        .ok()
        .and_then(|w| if w.is_finished() { w.join().ok() } else { None })
        .is_some_and(|r| r.is_ok());
    if result.is_ok() && !wrote {
        return Err(io_error("remote.write_script", "写入脚本"));
    }
    result
}

fn run_builtin(
    command: &super::command::BuiltinCommand,
    body: &str,
    limits: RunLimits,
) -> Result<serde_json::Value, AppError> {
    use crate::builtin::{ExecCommand, ExecOptions, OutputStream};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let nonce = super::framing::new_nonce();
    let cancelled = Arc::new(AtomicBool::new(false));
    // 认证失败会作废密码库，先保留本次代数用于错误脱敏；离开函数自动擦除。
    let password = command.vault.peek(&command.server.id);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exceeded = None;
    let result = command.manager.exec_streaming(
        command.request(),
        ExecCommand::Argv(vec!["/bin/sh".into(), "-s".into()]),
        ExecOptions {
            stdin: super::framing::wrap_script(body, &nonce).into_bytes(),
            timeout: Some(limits.timeout),
            // 两个流各自限制，超限时取消而非悄悄截断结果。
            output_limit: usize::MAX,
            cancelled: cancelled.clone(),
            ..Default::default()
        },
        |stream, bytes| {
            let (buffer, limit, name) = match stream {
                OutputStream::Stdout => (&mut stdout, limits.stdout_bytes, "stdout"),
                OutputStream::Stderr => (&mut stderr, limits.stderr_bytes, "stderr"),
            };
            if bytes.len() > limit.saturating_sub(buffer.len()) {
                exceeded.get_or_insert(name);
                cancelled.store(true, Ordering::Release);
            } else if exceeded.is_none() {
                buffer.extend_from_slice(bytes);
            }
            Ok(())
        },
    );
    if let Some(stream) = exceeded {
        return Err(AppError::new("remote.output_too_large").with("stream", stream));
    }
    let result = result?;
    if result.timed_out {
        return Err(AppError::new("remote.timeout"));
    }
    if result.cancelled {
        return Err(AppError::new("ssh.auth_cancelled"));
    }
    if result.exit_code != Some(0) || result.signal.is_some() {
        let code = result.exit_code.and_then(|code| i32::try_from(code).ok());
        let text = String::from_utf8_lossy(&stderr);
        let current_password = command.vault.peek(&command.server.id);
        let secrets: Vec<_> = password
            .iter()
            .chain(current_password.iter())
            .map(|p| p.secret.as_str())
            .collect();
        return Err(
            if result.signal.is_none() && code.is_some_and(|code| code != 255) {
                error_with_tail("remote.script_failed", code, &text, &secrets)
            } else {
                classify_error(code, &text, &secrets)
            },
        );
    }
    let text = std::str::from_utf8(&stdout).map_err(|_| AppError::new("remote.invalid_utf8"))?;
    super::framing::parse_framed(text, &nonce, limits.stdout_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 固定提示表及脱敏() {
        for (patterns, code) in RULES {
            for pattern in *patterns {
                assert_eq!(classify_error(Some(255), pattern, &[]).code, *code);
            }
        }
        let e = classify_error(Some(3), "中文\npassword=秘密\n已知秘密", &["已知秘密"]);
        assert_eq!(e.code, "ssh.failed");
        assert!(!format!("{e:?}").contains("秘密"));
        let e = classify_error(
            None,
            &format!("-----BEGIN PRIVATE KEY-----{}", "secret".repeat(2000)),
            &[],
        );
        assert!(!e.params["stderr_tail"].contains("secret"));
        assert_eq!(RunLimits::default().timeout, Duration::from_secs(30));
    }
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;
    #[test]
    fn s11_s27_真实管道执行及限额() {
        let program = SshProgram {
            path: "/bin/sh".into(),
        };
        let cmd = SshCommand {
            args: vec!["-s".into()],
            env: vec![],
            config_text: None,
            hooked: false,
            builtin: None,
            askpass: None,
        };
        let limits = RunLimits {
            timeout: Duration::from_secs(2),
            ..Default::default()
        };
        assert_eq!(
            run_script(&program, &cmd, "cat\nprintf true", limits).unwrap(),
            true
        );
        // 超时用例给 100 毫秒；输出超限用例给足 10 秒，避免在较慢的 CI 机器上先触发超时。
        for (body, expected, timeout_ms) in [
            ("sleep 2", "remote.timeout", 100),
            ("yes", "remote.output_too_large", 10_000),
            ("yes >&2", "remote.output_too_large", 10_000),
        ] {
            let limits = RunLimits {
                timeout: Duration::from_millis(timeout_ms),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
            };
            assert_eq!(
                run_script(&program, &cmd, body, limits).unwrap_err().code,
                expected
            );
        }
        let error =
            run_script(&program, &cmd, "printf 'password=秘密' >&2; exit 7", limits).unwrap_err();
        assert_eq!(error.code, "remote.script_failed");
        assert!(!format!("{error:?}").contains("秘密"));
        for code in [1, 7, 255] {
            let body =
                format!("printf 'Permission denied (publickey). password=秘密' >&2; exit {code}");
            let error = run_script(&program, &cmd, &body, limits).unwrap_err();
            assert_eq!(
                error.code,
                if code == 255 {
                    "ssh.auth_failed"
                } else {
                    "remote.script_failed"
                }
            );
            assert_eq!(error.params["exit_code"], code.to_string());
            assert!(!format!("{error:?}").contains("秘密"));
        }
        assert_eq!(
            run_script(
                &program,
                &cmd,
                "printf 'Connection closed' >&2; kill -TERM $$",
                limits
            )
            .unwrap_err()
            .code,
            "ssh.unreachable"
        );
        let before = Instant::now();
        let value = run_script(&program, &cmd, "(sleep 30) & printf true", limits).unwrap();
        assert_eq!(value, true);
        assert!(before.elapsed() < Duration::from_secs(2));
        let program = SshProgram {
            path: "/usr/bin/sleep".into(),
        };
        let cmd = SshCommand {
            args: vec!["30".into()],
            env: vec![],
            config_text: None,
            hooked: false,
            builtin: None,
            askpass: None,
        };
        let before = Instant::now();
        assert_eq!(
            run_script(
                &program,
                &cmd,
                &"#数据\n".repeat(100_000),
                RunLimits {
                    timeout: Duration::from_millis(100),
                    ..limits
                }
            )
            .unwrap_err()
            .code,
            "remote.timeout"
        );
        assert!(before.elapsed() < Duration::from_secs(1));
    }
}

#[cfg(test)]
mod forward_error_tests {
    use super::*;
    #[test]
    fn 远程转发失败_保留端口_权限错误和禁止转发区分() {
        let error = classify_error(
            Some(255),
            "Authenticated to host\nError: remote port forwarding failed for listen port 28080\npassword=秘密",
            &[],
        );
        assert_eq!(error.code, "tunnel.remote_forward_failed");
        assert_eq!(error.params["port"], "28080");
        assert!(!format!("{error:?}").contains("秘密"));
        for text in [
            "bind: Permission denied",
            "bind [127.0.0.1]:1080: Permission denied",
        ] {
            assert_eq!(
                classify_error(Some(255), text, &[]).code,
                "tunnel.port_reserved"
            );
        }
        assert_eq!(
            classify_error(
                Some(255),
                "channel 2: open failed: administratively prohibited: open failed",
                &[]
            )
            .code,
            "ssh.forwarding_prohibited"
        );
        assert_eq!(
            classify_error(Some(255), "Permission denied (publickey)", &[]).code,
            "ssh.auth_failed"
        );
    }
}
