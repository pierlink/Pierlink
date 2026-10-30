use super::{
    Tools, VncStream, VncTarget, parse, relay,
    stream::{Backend, closed},
};
use crate::{
    builtin::error::{Operation, map_engine_error},
    error::AppError,
    process::{Reader, SshProcess, SshProgram, collect_tree, spawn_tree},
    ssh::args::SshCommand,
};
use pierlink_ssh_engine::{ChannelFailure, Error, Session, Tunnel};
use std::{
    thread,
    time::{Duration, Instant},
};

enum Transport {
    Tunnel(Tunnel),
    Exec(Session),
    Process(SshProcess, Reader),
}
impl Backend for Transport {
    fn close(&mut self) {
        match self {
            Self::Tunnel(t) => t.closer().cancel(),
            Self::Exec(s) => s.closer().cancel(),
            Self::Process(p, stderr) => {
                let _ = p.terminate();
                stderr.join();
            }
        }
    }
    fn failure(&mut self) -> AppError {
        match self {
            Self::Process(process, stderr) => {
                let start = Instant::now();
                loop {
                    let data = stderr.state.lock().unwrap_or_else(|p| p.into_inner());
                    if data.done || start.elapsed() >= Duration::from_millis(100) {
                        let code = crate::remote::run::matched_ssh_error(&data.bytes);
                        return process.finish_error(code.map_or_else(closed, AppError::new));
                    }
                    drop(data);
                    thread::sleep(Duration::from_millis(5));
                }
            }
            _ => closed(),
        }
    }
}

/// 用 program、后台用途 cmd 和检测到的 tools 连接 target；只允许远端 Unix 路径或 IPv4 回环端口。
/// 复用 cmd 内置连接上下文；OpenSSH 保留原认证配置。直连被禁止时自动使用 exec 中继。
pub fn open_stream(
    program: &SshProgram,
    cmd: &SshCommand,
    tools: &Tools,
    target: VncTarget,
) -> Result<VncStream, AppError> {
    validate(&target)?;
    if let Some(context) = &cmd.builtin {
        let connection = context.manager.get_connection(context.request())?;
        let (opened, operation) = match &target {
            VncTarget::UnixPath(path) => (
                connection.direct_streamlocal(path),
                Operation::OpenStreamLocal,
            ),
            VncTarget::Loopback(port) => (
                connection.direct_tcpip("127.0.0.1", *port),
                Operation::OpenForward,
            ),
        };
        match opened {
            Ok(mut tunnel) => {
                let (input, output) = tunnel
                    .take_io()
                    .map_err(|e| map_engine_error(&e, operation))?;
                VncStream::from_io(Transport::Tunnel(tunnel), input, output)
            }
            Err(error) => {
                let mapped = map_engine_error(&error, operation);
                if matches!(error, Error::ChannelRejected(ChannelFailure::Prohibited))
                    || mapped.code == "ssh.streamlocal_open_failed"
                {
                    open_relay(program, cmd, tools, &target).map_err(fallback_error)
                } else {
                    Err(mapped)
                }
            }
        }
    } else {
        let version = openssh_version(program)?;
        if version < (9, 4) && matches!(target, VncTarget::UnixPath(_)) {
            return open_relay(program, cmd, tools, &target);
        }
        let command = forward_command(cmd, &target)?;
        let result = process_stream(program, &command).and_then(|mut stream| {
            // 超时照实报超时：走到这里的版本都支持这种转发，慢多半是登录慢（跳板、验证码）
            match banner(&mut stream, Duration::from_secs(15)) {
                Ok(()) => Ok(stream),
                Err(error) => {
                    stream.close();
                    Err(error)
                }
            }
        });
        match result {
            Err(error) if error.code == "ssh.forwarding_prohibited" => {
                open_relay(program, cmd, tools, &target).map_err(fallback_error)
            }
            other => other,
        }
    }
}

fn validate(target: &VncTarget) -> Result<(), AppError> {
    match target {
        VncTarget::Loopback(0) => {
            Err(AppError::new("vnc.invalid_target").with_reason("vnc.port_zero", "端口必须大于零"))
        }
        VncTarget::UnixPath(path)
            if !path.starts_with('/') || path.len() > 107 || path.contains('\0') =>
        {
            Err(AppError::new("vnc.invalid_target").with_reason(
                "vnc.socket_must_be_an_absolute_path_without_null_characters_and",
                "套接字必须为不含空字符的绝对路径，且不超过 107 字节",
            ))
        }
        _ => Ok(()),
    }
}

fn fallback_error(error: AppError) -> AppError {
    if error.code == "vnc.relay_unavailable" {
        error
    } else {
        AppError::new("vnc.forwarding_prohibited")
            .with_reason(
                "vnc.ssh_forwarding_is_unavailable_and_the_relay_could_not_connect",
                "SSH 转发不可用，执行中继也未能连接桌面",
            )
            .with("cause", error.code)
    }
}

pub(super) fn open_relay(
    program: &SshProgram,
    cmd: &SshCommand,
    tools: &Tools,
    target: &VncTarget,
) -> Result<VncStream, AppError> {
    validate(target)?;
    let remote = relay::command(tools, target)?;
    let mut stream = if let Some(context) = &cmd.builtin {
        let cancelled = context.request().cancelled;
        let mut session = context
            .manager
            .open_session(context.request(), |connection| {
                connection.exec_cancellable(
                    &remote,
                    &[],
                    cancelled.clone(),
                    Duration::from_secs(15),
                )
            })?;
        let (input, output, stderr) = session.take_io();
        // 引擎会丢弃并归还未消费的扩展输出，避免 stderr 占满共享接收窗口。
        drop(stderr);
        VncStream::from_io(Transport::Exec(session), input, output)?
    } else {
        let mut command = background_command(cmd)?;
        command.args.push(remote);
        process_stream(program, &command)?
    };
    if let Err(error) = banner(&mut stream, Duration::from_secs(15)) {
        stream.close();
        return Err(error);
    }
    Ok(stream)
}

fn background_command(cmd: &SshCommand) -> Result<SshCommand, AppError> {
    if cmd.builtin.is_some() || cmd.args.last().map(String::as_str) != Some("/bin/sh -s") {
        return Err(AppError::new("ssh.invalid_argument")
            .with_reason(
                "vnc.desktop_connection_requires_a_background_ssh_command_context",
                "桌面连接需要后台用途的 SSH 命令上下文",
            )
            .with_text(
                "field",
                "vnc.desktop_connection_requires_a_background_ssh_command_context",
                "桌面连接需要后台用途的 SSH 命令上下文",
            ));
    }
    let mut cmd = cmd.clone();
    cmd.args.pop();
    Ok(cmd)
}

pub(super) fn forward_command(
    cmd: &SshCommand,
    target: &VncTarget,
) -> Result<SshCommand, AppError> {
    let mut cmd = background_command(cmd)?;
    let address = match target {
        VncTarget::UnixPath(path) => path.clone(),
        VncTarget::Loopback(port) => format!("127.0.0.1:{port}"),
    };
    cmd.args.splice(
        0..0,
        [
            "-W".into(),
            address,
            "-o".into(),
            "ExitOnForwardFailure=yes".into(),
        ],
    );
    Ok(cmd)
}

fn process_stream(program: &SshProgram, command: &SshCommand) -> Result<VncStream, AppError> {
    let mut process = program.spawn(command)?;
    let input = process.child.stdin.take().ok_or_else(closed)?;
    let output = process.child.stdout.take().ok_or_else(closed)?;
    let stderr = Reader::tail(process.child.stderr.take().ok_or_else(closed)?, 8192)?;
    VncStream::from_io(Transport::Process(process, stderr), input, output)
}

fn openssh_version(program: &SshProgram) -> Result<(u16, u16), AppError> {
    let mut process = spawn_tree(&program.path, &["-V".into()], None, &[])?;
    let (output, _) = collect_tree(
        &mut process,
        crate::mcp::RunLimits {
            timeout: Duration::from_secs(3),
            output_bytes: 4096,
        },
        || false,
        None,
    )?;
    let parsed = [output.stderr, output.stdout]
        .into_iter()
        .find_map(|bytes| parse::ssh_version(&String::from_utf8_lossy(&bytes)));
    parsed
        .filter(|_| !output.timed_out && output.exit_code == Some(0))
        .ok_or_else(|| AppError::new("vnc.ssh_too_old").with_text("version", "vnc.unknown", "未知"))
}

pub(super) fn banner(stream: &mut VncStream, timeout: Duration) -> Result<(), AppError> {
    let deadline = Instant::now() + timeout;
    let mut prefix = [0; 4];
    let mut n = 0;
    while n < prefix.len() {
        n += stream.reader.read_until(&mut prefix[n..], Some(deadline))?;
    }
    if prefix != *b"RFB " {
        return Err(AppError::new("vnc.not_rfb").with(
            "banner",
            prefix
                .into_iter()
                .filter(|b| b.is_ascii_graphic() || *b == b' ')
                .map(char::from)
                .collect::<String>(),
        ));
    }
    stream.reader.unread(&prefix);
    Ok(())
}
