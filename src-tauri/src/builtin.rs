//! 内置 SSH 的 Tauri 接入：连接管理器、连接时要用户回答的界面回调、连接状态推给界面。
//!
//! 指纹：内置 SSH 和系统 ssh 读写同一个 `~/.ssh/known_hosts`。打开终端前的“核对指纹”流程（`host_key.rs`）
//! 照旧确认并写入；这里只认已经确认过的记录，没确认过的返回 `ssh.host_key_unknown`，指纹变了返回 `ssh.host_key_changed`。

use crate::data::DbState;
use crate::passwords::PasswordState;
use pierlink_core::builtin::{
    ConnectionCallbacks, ConnectionClass, ConnectionManager, ConnectionRequest, DedicatedCredential,
};
use pierlink_core::error::AppError;
use pierlink_core::security::vault::Zeroizing;
use pierlink_core::ssh::args::SshCommand;
use pierlink_core::ssh::known_hosts::{HostKeyStatus, KnownHosts};
use pierlink_core::storage::Server;
use pierlink_ssh_engine::{Challenge, HostKey};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// 推给界面的每台服务器的内置连接状态
pub const STATE_EVENT: &str = "pierlink://connection-state";

/// 内置 SSH 的应用状态
pub struct BuiltinState {
    pub manager: Arc<ConnectionManager>,
    pub callbacks: Arc<UiCallbacks>,
    stop: Arc<AtomicBool>,
    /// 每台服务器最近一次的连接状态（AI 接入列目标时显示在不在线）
    states: Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, pierlink_core::builtin::ConnectionState>,
        >,
    >,
}

impl BuiltinState {
    /// 建连接管理器（按需连接，这里不连任何服务器），并开始把连接状态推给界面
    pub fn start(app: &AppHandle) -> Self {
        let manager = Arc::new(ConnectionManager::default());
        let resolver_app = app.clone();
        manager.set_server_resolver(Arc::new(move |id: &str| {
            resolver_app.state::<DbState>().with(|db| db.get_server(id))
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let states = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let subscription = manager.subscribe_states();
        let (emit_app, emit_stop, emit_states) = (app.clone(), stop.clone(), states.clone());
        let spawned = std::thread::Builder::new()
            .name("内置连接状态".into())
            .spawn(move || {
                while !emit_stop.load(Ordering::Acquire) {
                    if let Some(event) = subscription.wait(Duration::from_millis(500)) {
                        emit_states
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(event.server_id.clone(), event.state);
                        // 登录成功（终端或“登录”按钮，包括要验证码的服务器）：首次没连上的同步接着连
                        if event.state == pierlink_core::builtin::ConnectionState::Connected {
                            crate::sync::resume_server(&emit_app, &event.server_id);
                        }
                        let _ = emit_app.emit(STATE_EVENT, &event);
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("启动连接状态推送失败: 错误={e}");
        }
        Self {
            manager,
            callbacks: Arc::new(UiCallbacks { app: app.clone() }),
            stop,
            states,
        }
    }

    /// 这台服务器的内置连接在不在线；这次运行里还没连过时返回 None
    pub fn online(&self, server_id: &str) -> Option<bool> {
        self.states
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(server_id)
            .map(|state| *state == pierlink_core::builtin::ConnectionState::Connected)
    }

    /// 网络恢复：让断开或等待中的连接立即重试
    pub fn network_restored(&self) {
        self.manager.network_restored();
    }

    /// 应用退出：取消所有等待、关闭所有内置连接
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        if let Err(e) = self.manager.close_all() {
            eprintln!("关闭内置连接出错: 错误={}", e.code);
        }
    }
}

/// 后台命令（部署、资源概览、专用密钥、同步检查、文件面板的脚本……）：按服务器的连接方式选内置 SSH 或系统 OpenSSH。
/// 用户刚做的操作（`Access::User`）缺密码时弹框；后台功能不弹框，转为等待输入密码。
/// `openssh` 只在走系统 OpenSSH 时调用，按原来的规则构造参数。
pub(crate) fn background(
    app: &AppHandle,
    server: &Server,
    access: crate::keys::Access,
    openssh: impl FnOnce() -> Result<SshCommand, AppError>,
) -> Result<SshCommand, AppError> {
    let (Some(state), Some(passwords)) = (
        app.try_state::<BuiltinState>(),
        app.try_state::<PasswordState>(),
    ) else {
        return openssh();
    };
    let request = ConnectionRequest {
        server,
        class: ConnectionClass::Main,
        vault: passwords.vault.clone(),
        callbacks: state.callbacks.clone(),
        interactive: matches!(access, crate::keys::Access::User(_)),
        cancelled: Arc::new(AtomicBool::new(false)),
        progress: None,
    };
    pierlink_core::remote::command::background(&state.manager, request, openssh)
}

/// 文件面板（SFTP）：按服务器的连接方式选内置 SSH（批量连接，大文件传输不拖慢终端）或系统 OpenSSH。
/// 文件面板是用户主动打开的，缺密码时弹框。
pub(crate) fn sftp(
    app: &AppHandle,
    server: &Server,
    openssh: impl FnOnce() -> Result<SshCommand, AppError>,
) -> Result<SshCommand, AppError> {
    let (Some(state), Some(passwords)) = (
        app.try_state::<BuiltinState>(),
        app.try_state::<PasswordState>(),
    ) else {
        return openssh();
    };
    let request = ConnectionRequest {
        server,
        class: ConnectionClass::Bulk,
        vault: passwords.vault.clone(),
        callbacks: state.callbacks.clone(),
        interactive: true,
        cancelled: Arc::new(AtomicBool::new(false)),
        progress: None,
    };
    pierlink_core::sftp::command(&state.manager, request, openssh)
}

/// 用户点“登录”：用内置 SSH 交互登录这台服务器（按需问密码、验证码），主连接建好后返回。
/// 要验证码的服务器后台功能自己登不上；登录一次后资源概览、端口转发、同步都用这条连接。
///
/// :param server_id: 服务器 ID
/// :return: 登录成功返回空；用系统 OpenSSH 的服务器返回 `ssh.builtin_only`，用户取消返回 `ssh.auth_cancelled`
#[tauri::command]
#[specta::specta]
pub async fn server_login(app: AppHandle, server_id: String) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let server = crate::servers::load(&app, &server_id)?;
        let engine = pierlink_core::ssh::engine::engine_for_feature(
            &server,
            pierlink_core::ssh::engine::Feature::Background,
        );
        if engine.engine != pierlink_core::storage::Engine::Builtin {
            return Err(AppError::new("ssh.builtin_only"));
        }
        let (Some(state), Some(passwords)) = (
            app.try_state::<BuiltinState>(),
            app.try_state::<PasswordState>(),
        ) else {
            return Err(AppError::new("ssh.builtin_only"));
        };
        let request = ConnectionRequest {
            server: &server,
            class: ConnectionClass::Main,
            vault: passwords.vault.clone(),
            callbacks: state.callbacks.clone(),
            interactive: true,
            cancelled: Arc::new(AtomicBool::new(false)),
            progress: None,
        };
        state.manager.get_connection(request)?;
        eprintln!("内置 SSH 登录成功: 服务器={}", server.name);
        Ok(())
    })
    .await
    .map_err(|e| AppError::new("ssh.task_failed").with("reason", e.to_string()))?
}

/// 关掉一台服务器现有的内置连接（用内置 SSH 打开的终端、文件面板会断开）。
/// 关连接最多要等几秒（等服务器回应断开），放在阻塞线程里等，不占 Tauri 的异步工作线程；
/// 等关完才返回，免得界面紧接着的操作又拿到旧连接。
///
/// :param server_id: 服务器 ID
/// 用户明确要求重试时，清除这台服务器记住的登录上限（agent 密钥太多被服务器断开，见异常清单 K9）
pub(crate) fn clear_auth_limit(app: &AppHandle, server_id: &str) {
    if let Some(state) = app.try_state::<BuiltinState>() {
        state.manager.clear_auth_limit(server_id);
    }
}

pub(crate) async fn close_server(app: &AppHandle, server_id: &str) {
    let (app, id) = (app.clone(), server_id.to_owned());
    let closed = tauri::async_runtime::spawn_blocking(move || {
        if let Some(state) = app.try_state::<BuiltinState>()
            && let Err(e) = state.manager.close_server(&id)
        {
            eprintln!("关闭内置连接失败: 服务器={id}, 错误={}", e.code);
        }
    })
    .await;
    if let Err(e) = closed {
        eprintln!("关闭内置连接的任务失败: 服务器={server_id}, 错误={e}");
    }
}

/// 一台服务器的连接方式（内置 SSH ↔ 系统 OpenSSH）或连接参数（地址、端口、用户、登录方式、密钥）变了：
/// 关掉它现有的内置连接（这台服务器上用内置 SSH 打开的终端会断开，重新连接时按新设置），
/// 正在用的端口转发、同步按新设置重开。
pub(crate) async fn connection_changed(app: &AppHandle, server_id: &str) {
    close_server(app, server_id).await;
    let (app, id) = (app.clone(), server_id.to_owned());
    std::thread::spawn(move || {
        crate::forwards::restart_server(&app, &id);
        crate::sync::reconnect_server(&app, &id);
        eprintln!("已按新的连接设置重开转发和同步: 服务器={id}");
    });
}

/// 隧道（端口转发、同步）：按服务器的连接方式给隧道规格带上内置上下文；用系统 OpenSSH 时原样返回。
/// 隧道是后台功能，不弹框：缺密码时进入“等待输入密码”。
///
/// :param feature: 功能（端口转发用主连接，同步用批量连接，由 class 指定）
/// :return: 交给 TunnelManager 的规格
pub(crate) fn tunnel(
    app: &AppHandle,
    server: &Server,
    feature: pierlink_core::ssh::engine::Feature,
    class: ConnectionClass,
    spec: pierlink_core::tunnel::TunnelSpec,
) -> Result<pierlink_core::tunnel::TunnelSpec, AppError> {
    let (Some(state), Some(passwords)) = (
        app.try_state::<BuiltinState>(),
        app.try_state::<PasswordState>(),
    ) else {
        return Ok(spec);
    };
    let request = ConnectionRequest {
        server,
        class,
        vault: passwords.vault.clone(),
        callbacks: state.callbacks.clone(),
        interactive: false,
        cancelled: Arc::new(AtomicBool::new(false)),
        progress: None,
    };
    pierlink_core::tunnel::builtin_spec(&state.manager, request, feature, spec)
}

/// 连接时要用户回答的界面：复用 0.3.1 的密码框（同一台服务器只弹一个，记住密码照旧）
pub struct UiCallbacks {
    app: AppHandle,
}

impl UiCallbacks {
    fn ask(
        &self,
        server_id: &str,
        kind: &str,
        prompt: Option<&str>,
        cancelled: &AtomicBool,
    ) -> Option<(Zeroizing<String>, bool)> {
        self.app
            .try_state::<PasswordState>()?
            .ask_user(&self.app, server_id, kind, prompt, "", cancelled)
    }
}

impl ConnectionCallbacks for UiCallbacks {
    fn check_host_key(
        &self,
        _server_id: &str,
        key: &HostKey,
        cancelled: &AtomicBool,
    ) -> Result<bool, AppError> {
        if cancelled.load(Ordering::Acquire) {
            return Ok(false);
        }
        // 指纹和记录不一致、被吊销时 check 直接返回 ssh.host_key_changed
        match KnownHosts::for_user()?.check(key, None)?.status {
            HostKeyStatus::Trusted => Ok(true),
            HostKeyStatus::New { .. } => Err(AppError::new("ssh.host_key_unknown")),
        }
    }

    fn passphrase(
        &self,
        server_id: &str,
        path: &Path,
        cancelled: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        let prompt = format!("Enter passphrase for key '{}': ", path.display());
        Ok(self
            .ask(server_id, "other", Some(&prompt), cancelled)
            .map(|(secret, _)| secret))
    }

    fn password(
        &self,
        server_id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        let Some((secret, remember)) = self.ask(server_id, "password", None, cancelled) else {
            return Ok(None);
        };
        if remember && let Err(e) = crate::passwords::remember(&self.app, server_id, &secret) {
            // 没存进凭据管理器不影响这次登录，只是下次还要输
            eprintln!("记住密码失败: 服务器={server_id}, 错误={}", e.code);
        }
        Ok(Some(secret))
    }

    fn dedicated_key(
        &self,
        _server_id: &str,
        _cancelled: &AtomicBool,
    ) -> Result<Option<DedicatedCredential>, AppError> {
        Ok(crate::keys::dedicated_credential(&self.app)?
            .map(|(path, passphrase)| DedicatedCredential { path, passphrase }))
    }

    fn keyboard_interactive(
        &self,
        server_id: &str,
        challenge: Challenge,
        cancelled: &AtomicBool,
    ) -> Result<Option<Vec<Zeroizing<String>>>, AppError> {
        let mut answers = Vec::with_capacity(challenge.prompts.len());
        for prompt in &challenge.prompts {
            // 服务器给的标题、说明和问题一起显示（都只当文字）
            let text = [
                challenge.name.trim(),
                challenge.instructions.trim(),
                prompt.text.trim(),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
            match self.ask(server_id, "other", Some(&text), cancelled) {
                Some((answer, _)) => answers.push(answer),
                None => return Ok(None),
            }
        }
        Ok(Some(answers))
    }
}
