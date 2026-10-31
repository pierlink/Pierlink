//! 后台命令携带连接上下文；OpenSSH 参数仍由原调用方按原规则生成。
use crate::{
    builtin::{ConnectionCallbacks, ConnectionClass, ConnectionManager, ConnectionRequest},
    error::AppError,
    security::vault::PasswordVault,
    ssh::{
        args::SshCommand,
        engine::{Engine, Feature, engine_for_feature},
    },
    storage::Server,
};
use std::sync::{Arc, atomic::AtomicBool};

/// 可跨线程持有的内置后台上下文，不存明文凭据，也不创建 askpass 登记。
#[derive(Clone)]
pub struct BuiltinCommand {
    pub(crate) manager: ConnectionManager,
    pub(crate) server: Server,
    pub(crate) vault: Arc<PasswordVault>,
    callbacks: Arc<dyn ConnectionCallbacks>,
    pub(crate) interactive: bool,
    pub(crate) class: ConnectionClass,
    cancelled: Arc<AtomicBool>,
}

impl BuiltinCommand {
    pub(crate) fn request(&self) -> ConnectionRequest<'_> {
        ConnectionRequest {
            progress: None,
            server: &self.server,
            class: self.class,
            vault: self.vault.clone(),
            callbacks: self.callbacks.clone(),
            interactive: self.interactive,
            cancelled: self.cancelled.clone(),
        }
    }

    pub(crate) fn verification(&self) -> Result<Self, AppError> {
        if !matches!(
            self.server.auth_method,
            crate::storage::AuthMethod::Key | crate::storage::AuthMethod::DedicatedKey
        ) {
            return Err(AppError::new("keys.invalid_verify_command"));
        }
        let mut command = self.clone();
        command.manager = self.manager.key_verification_manager(&self.server.id);
        command.interactive = false;
        Ok(command)
    }
}

/// 按 request.server 的后台功能选择引擎，返回所有后台调用方通用的命令上下文。
/// 内置分支复用 manager、密码库与回调；仅 OpenSSH 分支调用 openssh 构造原参数。
/// request.interactive 控制主动操作能否询问密码；常驻监控必须传 false。
pub fn background(
    manager: &ConnectionManager,
    request: ConnectionRequest<'_>,
    openssh: impl FnOnce() -> Result<SshCommand, AppError>,
) -> Result<SshCommand, AppError> {
    for_feature(manager, request, Feature::Background, openssh)
}

pub(crate) fn for_feature(
    manager: &ConnectionManager,
    request: ConnectionRequest<'_>,
    feature: Feature,
    openssh: impl FnOnce() -> Result<SshCommand, AppError>,
) -> Result<SshCommand, AppError> {
    if engine_for_feature(request.server, feature).engine == Engine::OpenSsh {
        return openssh();
    }
    Ok(SshCommand {
        args: Vec::new(),
        env: Vec::new(),
        config_text: None,
        hooked: false,
        askpass: None,
        builtin: Some(Arc::new(BuiltinCommand {
            manager: manager.clone(),
            server: request.server.clone(),
            vault: request.vault,
            callbacks: request.callbacks,
            interactive: request.interactive,
            class: ConnectionClass::Main,
            cancelled: request.cancelled,
        })),
    })
}
