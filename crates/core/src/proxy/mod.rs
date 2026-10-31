//! 全局出站代理设置；仅新连接读取，不影响已经建立的流。
mod bypass;
mod env;
pub(crate) mod openssh;
mod stdio;
mod system;

use crate::error::AppError;
pub use env::terminal_env;
use pierlink_ssh_engine::proxy::ProxyError;
pub use pierlink_ssh_engine::proxy::{ProxyKind, ProxyServer};
use std::{
    net::TcpStream,
    sync::{LazyLock, RwLock},
    time::Duration,
};
pub use stdio::pipe_stdio_from_env;
pub use system::system_proxy;

/// 自定义代理的建议默认直连名单；空名单表示只强制直连回环。
pub const DEFAULT_BYPASS: &[&str] = &[
    "localhost",
    "127.0.0.0/8",
    "::1",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "100.64.0.0/10",
    "fc00::/7",
    "fe80::/10",
    "*.local",
    "<local>",
];

/// 不使用代理、每次读取系统设置，或使用指定代理。
#[derive(Clone, Debug, Default)]
pub enum ProxyMode {
    #[default]
    Off,
    System,
    Custom(ProxyServer),
}

/// 内存设置；bypass 仅在自定义模式使用。
#[derive(Clone, Debug)]
pub struct ProxySettings {
    pub mode: ProxyMode,
    pub bypass: Vec<String>,
}
impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            mode: ProxyMode::Off,
            bypass: DEFAULT_BYPASS.iter().map(|s| (*s).into()).collect(),
        }
    }
}

/// 系统当前代理；PAC 仅供界面提示，不执行脚本。
#[derive(Clone, Debug)]
pub enum SystemProxy {
    None,
    Server {
        server: ProxyServer,
        bypass: Vec<String>,
    },
    Pac {
        url: String,
    },
}

static SETTINGS: LazyLock<RwLock<ProxySettings>> =
    LazyLock::new(|| RwLock::new(ProxySettings::default()));

/// 用 settings 替换内存中的代理设置，无返回值；之后新建的连接使用新设置。
pub fn set_settings(settings: ProxySettings) {
    *SETTINGS.write().unwrap_or_else(|p| p.into_inner()) = settings;
}

/// 无参数；返回当前内存设置的独立副本。
pub fn settings() -> ProxySettings {
    SETTINGS.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// 根据 host 返回应使用的代理；回环或名单匹配返回 None，不在本机解析目标域名。
pub fn for_target(host: &str) -> Option<ProxyServer> {
    select(&settings(), host, system_proxy)
}

fn select(
    settings: &ProxySettings,
    host: &str,
    system: impl FnOnce() -> SystemProxy,
) -> Option<ProxyServer> {
    if bypass::loopback(host) {
        return None;
    }
    let (server, bypass) = active(settings, system)?;
    (!bypass::matches(host, &bypass)).then_some(server)
}

fn active(
    settings: &ProxySettings,
    system: impl FnOnce() -> SystemProxy,
) -> Option<(ProxyServer, Vec<String>)> {
    match &settings.mode {
        ProxyMode::Off => None,
        ProxyMode::Custom(server) => Some((server.clone(), settings.bypass.clone())),
        ProxyMode::System => match system() {
            SystemProxy::Server { server, bypass } => Some((server, bypass)),
            SystemProxy::None | SystemProxy::Pac { .. } => None,
        },
    }
}

/// 经 proxy 连接 host:port，timeout 覆盖建连和握手；返回阻塞 TCP 流或前端错误。
pub fn connect(
    proxy: &ProxyServer,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<TcpStream, AppError> {
    pierlink_ssh_engine::proxy::connect(proxy, host, port, timeout).map_err(|e| map_error(&e))
}

pub(crate) fn map_error(error: &ProxyError) -> AppError {
    let code = match error {
        ProxyError::Io => "proxy.io_failed",
        ProxyError::InvalidInput => "proxy.invalid_argument",
        ProxyError::Unreachable { .. } => "proxy.unreachable",
        ProxyError::AuthRequired => "proxy.auth_required",
        ProxyError::Refused { .. } => "proxy.refused",
        ProxyError::Timeout => "proxy.timeout",
    };
    let mut result = AppError::new(code).with("reason", error.to_string());
    match error {
        ProxyError::Unreachable { proxy } => {
            result = result.with("proxy", proxy);
        }
        ProxyError::Refused { target, reason } => {
            result = result.with("target", target).with("reason", reason);
        }
        _ => {}
    }
    result
}

/// 根据 proxy 返回供下载器和终端使用的代理 URL，IPv6 主机加方括号。
pub fn url(proxy: &ProxyServer) -> String {
    let scheme = match proxy.kind {
        ProxyKind::Http => "http",
        ProxyKind::Socks5 => "socks5h",
    };
    format!(
        "{scheme}://{}",
        pierlink_ssh_engine::proxy::authority(&proxy.host, proxy.port)
    )
}

pub(crate) fn environment(proxy: &ProxyServer) -> Vec<(String, String)> {
    vec![("PIERLINK_PROXY".into(), url(proxy))]
}

#[cfg(test)]
mod tests;
