//! HTTP CONNECT 和 SOCKS5 出站连接；只解析代理地址，目标域名交给代理。
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 代理协议；序列化名称为 http / socks5。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyKind {
    Http,
    Socks5,
}

/// 无认证的代理地址。
#[derive(Clone, Debug)]
pub struct ProxyServer {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
}

/// 代理拒绝连接的固定原因；显示为中文原文，状态码仅保留经过校验的数字。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RefuseReason {
    #[error("无法取得代理连接")]
    ConnectionUnavailable,
    #[error("无法设置代理连接")]
    ConnectionSetupFailed,
    #[error("写入代理请求失败")]
    RequestWriteFailed,
    #[error("代理响应不完整")]
    ResponseIncomplete,
    #[error("代理响应头过大")]
    ResponseHeaderTooLarge,
    #[error("代理 HTTP 状态行无效")]
    InvalidHttpStatusLine,
    #[error("代理 HTTP 状态码无效")]
    InvalidHttpStatus,
    #[error("HTTP 状态码 {0}")]
    HttpStatus(u16),
    #[error("SOCKS5 握手中断")]
    SocksHandshakeInterrupted,
    #[error("SOCKS5 协议版本无效")]
    InvalidSocksVersion,
    #[error("SOCKS5 认证方式无效")]
    InvalidSocksAuthMethod,
    #[error("SOCKS5 响应格式无效")]
    InvalidSocksResponse,
    #[error("SOCKS5 回复码 {0}")]
    SocksReply(u8),
    #[error("SOCKS5 地址类型无效")]
    InvalidSocksAddressType,
}

impl RefuseReason {
    /// 无参数；返回当前原因在 errors.detail 下的翻译键，中文原文由 Display 提供。
    pub fn key(&self) -> &'static str {
        match self {
            Self::ConnectionUnavailable => "proxy.connection_unavailable",
            Self::ConnectionSetupFailed => "proxy.connection_setup_failed",
            Self::RequestWriteFailed => "proxy.request_write_failed",
            Self::ResponseIncomplete => "proxy.response_incomplete",
            Self::ResponseHeaderTooLarge => "proxy.response_header_too_large",
            Self::InvalidHttpStatusLine => "proxy.invalid_http_status_line",
            Self::InvalidHttpStatus => "proxy.invalid_http_status",
            Self::HttpStatus(_) => "proxy.http_status",
            Self::SocksHandshakeInterrupted => "proxy.socks_handshake_interrupted",
            Self::InvalidSocksVersion => "proxy.invalid_socks_version",
            Self::InvalidSocksAuthMethod => "proxy.invalid_socks_auth_method",
            Self::InvalidSocksResponse => "proxy.invalid_socks_response",
            Self::SocksReply(_) => "proxy.socks_reply",
            Self::InvalidSocksAddressType => "proxy.invalid_socks_address_type",
        }
    }
}

/// 代理失败原因；不存响应正文、认证值或未经检查的状态行。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProxyError {
    #[error("代理标准流转发中断")]
    Io,
    #[error("代理参数无效")]
    InvalidInput,
    #[error("连不上代理：{proxy}")]
    Unreachable { proxy: String },
    #[error("代理要求用户名和密码，Pierlink 不支持带密码的代理")]
    AuthRequired,
    #[error("代理拒绝连接目标 {target}：{reason}")]
    Refused {
        target: String,
        reason: RefuseReason,
    },
    #[error("代理握手超时")]
    Timeout,
}

/// 校验代理和目标地址及端口，返回无秘密的参数错误；不执行 DNS 查询。
pub fn validate(proxy: &ProxyServer, host: &str, port: u16) -> Result<(), ProxyError> {
    if !valid_host(&proxy.host) || !valid_host(host) || proxy.port == 0 || port == 0 {
        return Err(ProxyError::InvalidInput);
    }
    Ok(())
}

fn valid_host(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    !host.is_empty()
        && host.len() <= 255
        && host
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// 将主机和端口格式化为目标地址；IPv6 字面量加方括号。
pub fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn refused(host: &str, port: u16, reason: RefuseReason) -> ProxyError {
    ProxyError::Refused {
        target: authority(host, port),
        reason,
    }
}

/// 同步经 proxy 连到 host:port；timeout 覆盖代理解析、TCP 和握手，返回阻塞 TCP 流。
/// 在线程式调用方使用；异步调用方请使用 connect_async。
pub fn connect(
    proxy: &ProxyServer,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<std::net::TcpStream, ProxyError> {
    validate(proxy, host, port)?;
    if tokio::runtime::Handle::try_current().is_ok() {
        // Tauri 的阻塞线程也带 tokio 上下文；独立线程驱动临时运行时，避免嵌套运行时 panic。
        return std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("代理连接".into())
                .spawn_scoped(scope, || connect(proxy, host, port, timeout))
                .map_err(|_| ProxyError::Io)?
                .join()
                .unwrap_or(Err(ProxyError::Io))
        });
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ProxyError::Unreachable {
            proxy: authority(&proxy.host, proxy.port),
        })?;
    let result = runtime
        .block_on(connect_async(proxy, host, port, timeout))
        .and_then(|stream| {
            stream
                .into_std()
                .map_err(|_| refused(host, port, RefuseReason::ConnectionUnavailable))
        });
    // 系统 DNS 调用不能取消；超时后不让运行时析构继续阻塞调用方。
    runtime.shutdown_background();
    let stream = result?;
    stream
        .set_nonblocking(false)
        .map_err(|_| refused(host, port, RefuseReason::ConnectionSetupFailed))?;
    Ok(stream)
}

/// 异步经 proxy 连到 host:port；timeout 是整个建连期限，成功后连接不再受握手期限限制。
pub async fn connect_async(
    proxy: &ProxyServer,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<tokio::net::TcpStream, ProxyError> {
    validate(proxy, host, port)?;
    if timeout.is_zero() {
        return Err(ProxyError::Timeout);
    }
    tokio::time::timeout(timeout, async {
        let mut stream = tokio::net::TcpStream::connect((proxy.host.as_str(), proxy.port))
            .await
            .map_err(|_| ProxyError::Unreachable {
                proxy: authority(&proxy.host, proxy.port),
            })?;
        stream
            .set_nodelay(true)
            .map_err(|_| refused(host, port, RefuseReason::ConnectionSetupFailed))?;
        match proxy.kind {
            ProxyKind::Http => http(&mut stream, host, port).await?,
            ProxyKind::Socks5 => socks(&mut stream, host, port).await?,
        }
        Ok(stream)
    })
    .await
    .map_err(|_| ProxyError::Timeout)?
}

async fn http<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    host: &str,
    port: u16,
) -> Result<(), ProxyError> {
    let target = authority(host, port);
    let request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|_| refused(host, port, RefuseReason::RequestWriteFailed))?;
    // 逐字节读取有界头部，不吞掉同包到达的 SSH banner，也不读取错误正文。
    let mut header = Vec::new();
    loop {
        let byte = stream
            .read_u8()
            .await
            .map_err(|_| refused(host, port, RefuseReason::ResponseIncomplete))?;
        header.push(byte);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        if header.len() >= 16 * 1024 {
            return Err(refused(host, port, RefuseReason::ResponseHeaderTooLarge));
        }
    }
    let line = header.split(|b| *b == b'\n').next().unwrap_or(&[]);
    let mut parts = line.split(|b| *b == b' ');
    if !matches!(parts.next(), Some(b"HTTP/1.0" | b"HTTP/1.1")) {
        return Err(refused(host, port, RefuseReason::InvalidHttpStatusLine));
    }
    let status = parts
        .next()
        .filter(|s| s.len() == 3 && s.iter().all(u8::is_ascii_digit))
        .and_then(|s| std::str::from_utf8(s).ok())
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| refused(host, port, RefuseReason::InvalidHttpStatus))?;
    match status {
        200..=299 => Ok(()),
        407 => Err(ProxyError::AuthRequired),
        _ => Err(refused(host, port, RefuseReason::HttpStatus(status))),
    }
}

async fn socks<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    host: &str,
    port: u16,
) -> Result<(), ProxyError> {
    let failed = |_| refused(host, port, RefuseReason::SocksHandshakeInterrupted);
    stream.write_all(&[5, 1, 0]).await.map_err(failed)?;
    let mut reply = [0; 2];
    stream.read_exact(&mut reply).await.map_err(failed)?;
    if reply[0] != 5 {
        return Err(refused(host, port, RefuseReason::InvalidSocksVersion));
    }
    match reply[1] {
        0 => {}
        2 | 0xff => return Err(ProxyError::AuthRequired),
        _ => return Err(refused(host, port, RefuseReason::InvalidSocksAuthMethod)),
    }
    let mut request = vec![5, 1, 0];
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            request.extend_from_slice(&[3, host.len() as u8]);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await.map_err(failed)?;
    let mut reply = [0; 4];
    stream.read_exact(&mut reply).await.map_err(failed)?;
    if reply[0] != 5 || reply[2] != 0 {
        return Err(refused(host, port, RefuseReason::InvalidSocksResponse));
    }
    if reply[1] != 0 {
        return Err(refused(host, port, RefuseReason::SocksReply(reply[1])));
    }
    let length = match reply[3] {
        1 => 4,
        4 => 16,
        3 => usize::from(stream.read_u8().await.map_err(failed)?),
        _ => return Err(refused(host, port, RefuseReason::InvalidSocksAddressType)),
    };
    let mut address = [0; 257];
    stream
        .read_exact(&mut address[..length + 2])
        .await
        .map_err(failed)?;
    Ok(())
}

#[cfg(test)]
mod tests;
