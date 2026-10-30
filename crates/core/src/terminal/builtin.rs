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
    manager: ConnectionManager,
    server: Server,
    vault: Arc<PasswordVault>,
    callbacks: Arc<dyn ConnectionCallbacks>,
    cancelled: Arc<AtomicBool>,
    command: Option<String>,
    marker: Option<String>,
}

/// 按 request.server 的终端功能选择引擎，返回供 TerminalSpec 使用的终端类型。
/// cwd_marker 为可选目录跟随标记；内置连接固定使用 Main 并允许交互认证。
/// 仅系统模式调用 openssh，完整保留原有参数、askpass 和配置行为；此函数不登录。
pub fn remote(
    manager: &ConnectionManager,
    request: ConnectionRequest<'_>,
    cwd_marker: Option<String>,
    openssh: impl FnOnce() -> Result<TerminalKind, AppError>,
) -> Result<TerminalKind, AppError> {
    if engine_for_feature(request.server, Feature::Terminal).engine == Engine::OpenSsh {
        return openssh();
    }
    if let Some(marker) = &cwd_marker {
        crate::cwd::validate_marker(marker)?;
    }
    let command = crate::ssh::args::terminal_command(&request.server.login_shell)?;
    Ok(TerminalKind::BuiltinRemote(Box::new(BuiltinTerminal {
        manager: manager.clone(),
        server: request.server.clone(),
        vault: request.vault,
        callbacks: request.callbacks,
        cancelled: request.cancelled,
        command,
        marker: cwd_marker,
    })))
}

impl BuiltinTerminal {
    pub(super) fn open(
        self,
        cols: u16,
        rows: u16,
    ) -> Result<pierlink_ssh_engine::Session, AppError> {
        let env: Vec<_> = self
            .marker
            .as_deref()
            .map(|m| ("LC_PIERLINK_TERM", m))
            .into_iter()
            .collect();
        self.manager.open_session(
            ConnectionRequest {
                server: &self.server,
                class: ConnectionClass::Main,
                vault: self.vault,
                callbacks: self.callbacks,
                interactive: true,
                cancelled: self.cancelled.clone(),
            },
            |connection| {
                connection.terminal(
                    &pty(cols, rows),
                    &env,
                    self.command.as_deref(),
                    self.cancelled.clone(),
                )
            },
        )
    }
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
