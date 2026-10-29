//! 原样执行用户命令；非零退出码保留，不使用远程脚本 JSON 包装。
use crate::{
    builtin::{
        ConnectionClass, ConnectionManager, ConnectionRequest, ExecCommand, ExecOptions,
        OutputStream,
    },
    error::AppError,
    mcp::{RunLimits, RunOutput},
    process::{HeadTail, SshProgram, collect_tree},
    ssh::{
        args::SshCommand,
        engine::{Engine, Feature, engine_for_feature},
        quote::sh_quote,
    },
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

/// 按 request 的后台连接方式在服务器执行 command，cwd 为远程目录，limits 限制时间及各流输出。
/// 内置在主连接打开 exec 通道；openssh 必须构造后台用途的参数和非交互密码登记，命令替换其末尾的 /bin/sh -s。
/// 不弹密码或验证码框；缺凭据返回 ssh.password_needed / ssh.two_factor_required，非零退出码原样返回。
/// 取消或超时关闭通道或本地 SSH 进程树，设置 timed_out；OpenSSH 的远端进程可能继续运行。
#[allow(clippy::too_many_arguments)]
pub fn run_raw(
    manager: &ConnectionManager,
    mut request: ConnectionRequest<'_>,
    program: &SshProgram,
    openssh: impl FnOnce() -> Result<SshCommand, AppError>,
    command: &str,
    cwd: Option<&str>,
    limits: RunLimits,
    cancelled: Arc<AtomicBool>,
) -> Result<RunOutput, AppError> {
    let start = Instant::now();
    if cancelled.load(Ordering::Acquire)
        || request.cancelled.load(Ordering::Acquire)
        || limits.timeout.is_zero()
    {
        return Ok(RunOutput {
            timed_out: true,
            ..Default::default()
        });
    }
    if command.is_empty() || command.contains('\0') {
        return Err(AppError::new("ssh.invalid_argument").with("reason", "命令为空或含空字符"));
    }
    request.interactive = false;
    request.class = ConnectionClass::Main;
    let mut output =
        if engine_for_feature(request.server, Feature::Background).engine == Engine::Builtin {
            let mut stdout = HeadTail::new(limits.output_bytes);
            let mut stderr = HeadTail::new(limits.output_bytes);
            let result = manager.exec_streaming(
                request,
                ExecCommand::Escaped(command.into()),
                ExecOptions {
                    directory: cwd.map(str::to_owned),
                    timeout: Some(limits.timeout.saturating_sub(start.elapsed())),
                    // 回调自己分别保留头尾；必须交付所有块，不能在这里消耗合计输出额度。
                    output_limit: usize::MAX,
                    cancelled,
                    ..Default::default()
                },
                |stream, bytes| {
                    match stream {
                        OutputStream::Stdout => stdout.push(bytes),
                        OutputStream::Stderr => stderr.push(bytes),
                    }
                    Ok(())
                },
            )?;
            let (stdout, out_truncated) = stdout.snapshot();
            let (stderr, err_truncated) = stderr.snapshot();
            RunOutput {
                exit_code: result.exit_code.map(|v| v as i32),
                stdout,
                stderr,
                truncated: out_truncated || err_truncated,
                timed_out: result.timed_out || result.cancelled,
                duration_ms: 0,
            }
        } else {
            let script = match cwd {
                Some(cwd) => format!("cd {} && {command}", sh_quote(cwd)?),
                None => command.into(),
            };
            let remote = format!("/bin/sh -c {}", sh_quote(&script)?);
            let mut cmd = openssh()?;
            if cmd.args.last().map(String::as_str) != Some("/bin/sh -s") || cmd.builtin.is_some() {
                return Err(AppError::new("ssh.invalid_argument")
                    .with("reason", "原样执行需要后台用途的 OpenSSH 参数"));
            }
            cmd.args.pop();
            cmd.args.push(remote);
            if cancelled.load(Ordering::Acquire) || request.cancelled.load(Ordering::Acquire) {
                return Ok(RunOutput {
                    timed_out: true,
                    ..Default::default()
                });
            }
            let mut process = program.spawn(&cmd)?;
            let (output, ssh_error) = collect_tree(
                &mut process,
                RunLimits {
                    timeout: limits.timeout.saturating_sub(start.elapsed()),
                    ..limits
                },
                || cancelled.load(Ordering::Acquire) || request.cancelled.load(Ordering::Acquire),
                Some(super::run::matched_ssh_error),
            )?;
            if output.exit_code == Some(255)
                && !output.timed_out
                && let Some(code) = ssh_error
            {
                // 不附 stderr：远程输出可能含未登记秘密，错误只能带固定原因与退出码。
                return Err(process.finish_error(
                    AppError::new(code)
                        .with("exit_code", 255)
                        .with("reason", "SSH 连接或认证失败"),
                ));
            }
            output
        };
    output.duration_ms = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
    Ok(output)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        builtin_test_support::Fixture,
        ssh::{
            args::{CommonOpts, Purpose, build_args},
            target::SshTarget,
        },
    };
    use std::{fs, time::Duration};
    #[test]
    #[ignore = "需要本机 sshd 和随机监听端口"]
    fn 系统原样执行真实退出码转义认证分类及超时清理() {
        let f = Fixture::new();
        let context = f.command().builtin.unwrap();
        let mut server = f.server.clone();
        server.connection_engine = crate::storage::Engine::OpenSsh;
        let request = || ConnectionRequest {
            server: &server,
            ..context.request()
        };
        let known = f.sshd.root.path().join("known_hosts");
        let key = fs::read_to_string(f.sshd.root.path().join("host.pub")).unwrap();
        fs::write(&known, format!("[127.0.0.1]:{} {}", f.sshd.port, key)).unwrap();
        let target = SshTarget::Manual {
            host: "127.0.0.1".into(),
            port: f.sshd.port,
            user: Some(f.sshd.user.clone()),
            identity_file: Some(f.sshd.root.path().join("client")),
            jump: None,
        };
        let openssh = || {
            build_args(
                &target,
                &Purpose::Background,
                &CommonOpts {
                    known_hosts: Some(known.clone()),
                    ..Default::default()
                },
            )
        };
        let program = SshProgram {
            path: "/usr/bin/ssh".into(),
        };
        let limits = RunLimits {
            timeout: Duration::from_secs(5),
            output_bytes: 128,
        };
        let cancelled = || Arc::new(AtomicBool::new(false));
        for code in [0, 7, 255] {
            let out = run_raw(
                &f.manager,
                request(),
                &program,
                openssh,
                &format!("printf out; printf err >&2; exit {code}"),
                None,
                limits,
                cancelled(),
            )
            .unwrap();
            assert_eq!(out.exit_code, Some(code));
            assert_eq!(out.stdout, b"out");
            assert_eq!(out.stderr, b"err");
        }
        let directory = f.sshd.root.path().join("中文 ' $() 目录");
        fs::create_dir(&directory).unwrap();
        let out = run_raw(
            &f.manager,
            request(),
            &program,
            openssh,
            "pwd",
            directory.to_str(),
            RunLimits {
                output_bytes: 4096,
                ..limits
            },
            cancelled(),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            directory.to_str().unwrap()
        );
        let error = run_raw(
            &f.manager,
            request(),
            &program,
            openssh,
            "printf 'Permission denied (publickey). 未登记秘密' >&2; exit 255",
            None,
            limits,
            cancelled(),
        )
        .unwrap_err();
        assert_eq!(error.code, "ssh.auth_failed");
        assert!(!format!("{error:?}").contains("未登记秘密"));
        let out = run_raw(
            &f.manager,
            request(),
            &program,
            openssh,
            "printf head; head -c 200000 /dev/zero | tr '\\0' x; printf tail",
            None,
            limits,
            cancelled(),
        )
        .unwrap();
        assert!(out.truncated && out.stdout.starts_with(b"head") && out.stdout.ends_with(b"tail"));
        let started = Instant::now();
        let out = run_raw(
            &f.manager,
            request(),
            &program,
            openssh,
            "sleep 30",
            None,
            RunLimits {
                timeout: Duration::from_millis(200),
                ..limits
            },
            cancelled(),
        )
        .unwrap();
        assert!(out.timed_out && started.elapsed() < Duration::from_secs(2));
    }
}
