//! 内置终端直接使用主连接的 PTY 通道；不启动本机 SSH 或伪终端。
use super::*;
use crate::{
    builtin::{ConnectionCallbacks, ConnectionClass, ConnectionManager, ConnectionRequest},
    security::vault::PasswordVault,
    ssh::engine::{Engine, Feature, engine_for_feature},
    storage::Server,
};
use std::sync::atomic::AtomicBool;

/// 延迟到打开终端时取主连接的上下文；不保存明文凭据。
pub struct BuiltinTerminal {
    progress: Option<ConnectProgress>,
    manager: ConnectionManager,
    server: Server,
    vault: Arc<PasswordVault>,
    callbacks: Arc<dyn ConnectionCallbacks>,
    cancelled: Arc<AtomicBool>,
    integration: bool,
    marker: Option<String>,
}

/// 按 request.server 的终端功能选择引擎，返回供 TerminalSpec 使用的终端类型。
/// cwd_marker 为可选目录跟随标记，integration 控制 shell 集成；内置连接固定使用 Main 并允许交互认证。
/// 内置阶段优先发给 request.progress；未提供时通过 TerminalEvent::Stage 发出，二者不会重复通知。
/// 仅系统模式调用 openssh，完整保留原有参数、askpass 和配置行为；此函数不登录。
pub fn remote(
    manager: &ConnectionManager,
    request: ConnectionRequest<'_>,
    cwd_marker: Option<String>,
    integration: bool,
    openssh: impl FnOnce() -> Result<TerminalKind, AppError>,
) -> Result<TerminalKind, AppError> {
    // 依赖 ssh config 的服务器始终走 OpenSSH；转换入口会拒绝带 RemoteCommand 的配置。
    if engine_for_feature(request.server, Feature::Terminal).engine == Engine::OpenSsh {
        return openssh();
    }
    if let Some(marker) = &cwd_marker {
        crate::cwd::validate_marker(marker)?;
    }
    // 先校验登录 shell；集成命令在握手后按服务器类型生成。
    crate::ssh::args::terminal_command(&request.server.login_shell, false)?;
    Ok(TerminalKind::BuiltinRemote(Box::new(BuiltinTerminal {
        progress: request.progress,
        manager: manager.clone(),
        server: request.server.clone(),
        vault: request.vault,
        callbacks: request.callbacks,
        cancelled: request.cancelled,
        integration,
        marker: cwd_marker,
    })))
}

impl BuiltinTerminal {
    pub(super) fn progress(&self, callback: Callback) -> ConnectProgress {
        progress::ordered(
            self.progress.clone().unwrap_or_else(|| {
                Arc::new(move |stage| emit(&callback, TerminalEvent::Stage(stage)))
            }),
        )
    }

    pub(super) fn open(
        self,
        cols: u16,
        rows: u16,
        progress: ConnectProgress,
    ) -> Result<(pierlink_ssh_engine::Session, bool), AppError> {
        let env: Vec<_> = self
            .marker
            .as_deref()
            .map(|m| ("LC_PIERLINK_TERM", m))
            .into_iter()
            .collect();
        self.manager.open_session(
            ConnectionRequest {
                progress: Some(progress.clone()),
                server: &self.server,
                class: ConnectionClass::Main,
                vault: self.vault,
                callbacks: self.callbacks,
                interactive: true,
                cancelled: self.cancelled.clone(),
            },
            |connection| {
                progress(ConnectStage::Session);
                let integration = self.integration
                    && !windows_server(connection.remote_sshid().unwrap_or_default());
                let command =
                    match crate::ssh::args::terminal_command(&self.server.login_shell, integration)
                    {
                        Ok(command) => command,
                        // 保留参数错误；只让 SSH 通道错误触发连接池重试。
                        Err(error) => return Ok(Err(error)),
                    };
                terminal_result(
                    connection.terminal(
                        &pty(cols, rows),
                        &env,
                        command.as_deref(),
                        self.cancelled.clone(),
                    ),
                    integration,
                )
            },
        )?
    }
}

fn terminal_result(
    result: Result<pierlink_ssh_engine::Session, pierlink_ssh_engine::Error>,
    integration: bool,
) -> Result<Result<(pierlink_ssh_engine::Session, bool), AppError>, pierlink_ssh_engine::Error> {
    match result {
        Err(pierlink_ssh_engine::Error::RequestRejected("启动会话")) if integration => {
            Ok(Err(crate::builtin::error::hook_rejected()))
        }
        result => result.map(|session| Ok((session, integration))),
    }
}

fn windows_server(sshid: &str) -> bool {
    let sshid = sshid.to_ascii_lowercase();
    ["openssh_for_windows", "winsshd", "windows"]
        .iter()
        .any(|name| sshid.contains(name))
}

fn pty(cols: u16, rows: u16) -> pierlink_ssh_engine::Pty {
    pierlink_ssh_engine::Pty {
        term: "xterm-256color".into(),
        columns: cols.into(),
        rows: rows.into(),
        pixel_width: 0,
        pixel_height: 0,
    }
}

pub(super) enum Process {
    Local(child::Process),
    Builtin(Box<pierlink_ssh_engine::Session>),
}
impl Process {
    pub(super) fn poll(&mut self) -> Option<Option<i32>> {
        match self {
            Self::Local(process) => process.poll(),
            Self::Builtin(channel) => channel.try_wait_status().map(|result| match result {
                // 与系统 SSH 一致，用 255 表示连接断开，供界面提示重新连接。
                Err(pierlink_ssh_engine::Error::Disconnected) => Some(255),
                Ok(status) if status.signal.is_none() => {
                    status.code.and_then(|c| i32::try_from(c).ok())
                }
                _ => None,
            }),
        }
    }
    pub(super) fn busy(&mut self) -> Option<bool> {
        match self {
            Self::Local(process) => process.busy(),
            Self::Builtin(_) => None,
        }
    }
    pub(super) fn terminate(&mut self) {
        match self {
            Self::Local(process) => process.terminate(),
            Self::Builtin(channel) => channel.closer().cancel(),
        }
    }
    pub(super) fn finish(&mut self) {
        match self {
            Self::Local(process) => process.finish(),
            Self::Builtin(channel) => channel.closer().cancel(),
        }
    }
    pub(super) fn resize(&self, cols: u16, rows: u16) -> Result<(), AppError> {
        if let Self::Builtin(channel) = self {
            channel.resize(&pty(cols, rows)).map_err(|e| {
                crate::builtin::error::map_engine_error(&e, crate::builtin::error::Operation::Other)
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::windows_server;

    #[test]
    fn 内存对端拒绝启动时只为带钩子的请求提供专用说明() {
        use super::*;
        use pierlink_ssh_engine::{
            Authentication, ConnectionOptions, Engine, HostKeyCheck, test_support::connect_terminal,
        };
        let engine = Engine::new().unwrap();
        let options = ConnectionOptions::new(
            "内存终端",
            22,
            "测试",
            HostKeyCheck::Callback(Arc::new(|_| true)),
        );
        let connection = connect_terminal(
            &engine,
            options,
            Authentication::Password("测试密码".into()),
            Arc::default(),
            None,
        )
        .unwrap();
        for hooked in [true, false] {
            let result = connection.terminal(
                &pty(80, 24),
                &[],
                Some("reject"),
                Arc::new(AtomicBool::new(false)),
            );
            assert!(matches!(
                result,
                Err(pierlink_ssh_engine::Error::RequestRejected("启动会话"))
            ));
            let error = match terminal_result(result, hooked) {
                Ok(Err(error)) => error,
                Err(error) => crate::builtin::error::map_engine_error(
                    &error,
                    crate::builtin::error::Operation::OpenSession,
                ),
                Ok(Ok(_)) => panic!("服务器拒绝后不应打开终端"),
            };
            assert_eq!(error.code, "ssh.failed");
            assert_eq!(
                error.params["reason_key"],
                if hooked {
                    "hook_rejected"
                } else {
                    "request_rejected"
                }
            );
            assert_eq!(error.params["reason_key"], error.params["stderr_tail_key"]);
        }
    }

    #[test]
    fn 拒绝pty不误报提示符钩子被拒绝() {
        use super::*;
        for integration in [true, false] {
            let result = terminal_result(
                Err(pierlink_ssh_engine::Error::RequestRejected("PTY")),
                integration,
            );
            let error = result.err().unwrap();
            let error = crate::builtin::error::map_engine_error(
                &error,
                crate::builtin::error::Operation::OpenSession,
            );
            assert_eq!(error.params["reason_key"], "request_rejected");
        }
    }

    #[test]
    fn 服务器版本识别不区分大小写且未知版本保持集成() {
        for sshid in [
            "SSH-2.0-OpenSSH_for_Windows_9.5",
            "SSH-2.0-OpenSSH_for_Windows_8.1p1",
            "SSH-2.0-7.45 FlowSsh: Bitvise SSH Server (WinSSHD) 9.31",
            "ssh-2.0-openssh_FOR_windows_9.5",
            "SSH-2.0-WiNsShD",
            "SSH-2.0-Custom WINDOWS Server",
        ] {
            assert!(windows_server(sshid), "{sshid}");
        }
        for sshid in [
            "SSH-2.0-OpenSSH_9.6p1 Ubuntu-3ubuntu13",
            "SSH-2.0-dropbear_2022.83",
            "",
        ] {
            assert!(!windows_server(sshid), "{sshid}");
        }
    }
}
