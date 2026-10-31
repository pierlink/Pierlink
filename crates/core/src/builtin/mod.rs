//! 内置 SSH 登录与凭据编排。
mod background;
pub mod error;
mod exec;
mod login;
mod pool;
mod state;
pub use background::BackgroundConnection;
pub use exec::{ExecCommand, ExecOptions, ExecResult, OutputStream};
pub use pool::ServerResolver;
pub use state::{ConnectionEvent, ConnectionState, StateSubscription};

use pierlink_ssh_engine::Engine;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

use crate::{error::AppError, security::vault::PasswordVault, storage::Server};
use pierlink_ssh_engine::{Challenge, Connection, HostKey};
use std::sync::atomic::AtomicBool;
use zeroize::Zeroizing;

/// 同一服务器分离交互与大流量，避免批量传输阻塞终端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionClass {
    Main,
    Bulk,
}

/// 连接时的界面交互；不实现 Debug，不保存回答，不在连接池锁内调用。
/// 所有回调均须在 cancelled 置位后及时结束；取消不等于认证失败，不应作废密码。
pub trait ConnectionCallbacks: Send + Sync {
    /// 按 server_id 核对 key；每级跳板单独核对，返回是否信任或脱敏错误。
    /// key 来自服务器，须按不可信数据处理；cancelled 置位时停止等待界面。
    /// 实现可调用 ssh::known_hosts::KnownHosts::verify；别名由 server_id 对应的配置提供。
    fn check_host_key(
        &self,
        server_id: &str,
        key: &HostKey,
        cancelled: &AtomicBool,
    ) -> Result<bool, AppError>;

    /// 为 server_id 的 path 私钥取得一次口令；返回自动擦除的口令，None 表示取消。
    /// cancelled 置位时停止等待，口令不得写入日志或密码库。
    fn passphrase(
        &self,
        server_id: &str,
        path: &Path,
        cancelled: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError>;

    /// 为 server_id 询问服务器密码；None 表示取消。记住密码由界面层处理。
    /// 必须合并同一服务器的密码框；取消须在一秒内结束等待。
    fn password(
        &self,
        server_id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError>;

    /// 读取本机专用私钥路径及凭据管理器里的口令；None 表示本机密钥丢失。
    /// 不启动 ssh-keygen 或 askpass，读取错误须脱敏，取消须及时返回。
    fn dedicated_key(
        &self,
        server_id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Option<DedicatedCredential>, AppError>;

    /// 按 server_id 展示一轮不可信 challenge，返回与提示一一对应的自动擦除回答。
    /// None 或 cancelled 置位表示取消；验证码不缓存，允许多轮回调。
    fn keyboard_interactive(
        &self,
        server_id: &str,
        challenge: Challenge,
        cancelled: &AtomicBool,
    ) -> Result<Option<Vec<Zeroizing<String>>>, AppError>;
}

/// 本机专用密钥材料；口令自动擦除，不实现 Debug。
pub struct DedicatedCredential {
    pub path: PathBuf,
    pub passphrase: Zeroizing<String>,
}

/// 一次取连接请求；共享凭据和回调的所有权，取消后不留下借用工作线程。
/// interactive 与连接类别无关，文件面板的 Bulk 连接也可以由用户主动发起。
#[derive(Clone)]
pub struct ConnectionRequest<'a> {
    /// 可选的终端连接进度；后台调用传 None，回调必须快速返回。
    pub progress: Option<crate::terminal::ConnectProgress>,
    pub server: &'a Server,
    pub class: ConnectionClass,
    pub vault: Arc<PasswordVault>,
    pub callbacks: Arc<dyn ConnectionCallbacks>,
    pub interactive: bool,
    /// 调用方持有并在窗口关闭或退出时置位，覆盖连接与回调等待。
    pub cancelled: Arc<AtomicBool>,
}

/// 连接管理器；同服务器的登录串行，主连接和批量连接按需共享。
/// 指定密钥的口令只在本对象存活期间缓存，最多 128 个路径；达到上限时擦除旧缓存。
#[derive(Clone)]
pub struct ConnectionManager {
    engine: Arc<OnceLock<Result<Engine, pierlink_ssh_engine::Error>>>,
    gates: Arc<Mutex<HashMap<String, Weak<Mutex<()>>>>>,
    pool: Arc<pool::Pool>,
    passphrases: Arc<Mutex<HashMap<PathBuf, Zeroizing<String>>>>,
    verify_server: Option<String>,
}

impl ConnectionManager {
    pub(crate) fn key_verification_manager(&self, server: &str) -> Self {
        let manager = Self {
            verify_server: Some(server.into()),
            passphrases: Arc::new(Mutex::new(
                self.passphrases
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone(),
            )),
            ..Self::default()
        };
        manager.copy_resolver_from(self);
        manager
    }
    /// 按 request 取得共享连接；手工服务器支持最多四级链，缺失跳板返回错误。
    /// 通道和连接 clone 都算使用者；全部释放五分钟后自动断开。
    pub fn get_connection(&self, request: ConnectionRequest<'_>) -> Result<Connection, AppError> {
        let id = request.server.id.clone();
        let result = self.pooled(request, None);
        if let Err(error) = &result {
            self.publish(&id, pool::error_state(error), Some(error.clone()));
        }
        result
    }

    /// 用已解析的 options 为 request 建立单条已登录连接，返回连接或脱敏错误。
    /// 主机密钥始终交给 request.callbacks 核对，取消标记由 request 提供。
    /// 此入口把已解析的直连参数纳入连接池；有跳板时请使用 get_connection。
    pub fn connect(
        &self,
        request: ConnectionRequest<'_>,
        options: pierlink_ssh_engine::ConnectionOptions,
    ) -> Result<Connection, AppError> {
        self.connect_direct(request, options)
    }
}

impl Default for ConnectionManager {
    fn default() -> Self {
        Self {
            engine: Arc::new(OnceLock::new()),
            gates: Arc::new(Mutex::new(HashMap::new())),
            passphrases: Arc::new(Mutex::new(HashMap::new())),
            verify_server: None,
            pool: pool::Pool::new(),
        }
    }
}

#[cfg(test)]
mod tests;
