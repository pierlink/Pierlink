use std::io;

/// 引擎的结果；错误只保存受控类型，不保存服务端文本或秘密。
pub type Result<T> = std::result::Result<T, Error>;

/// SSH 通道拒绝原因；MaxSessions 在 OpenSSH 上通常为 ConnectFailed。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelFailure {
    /// 服务器禁止转发或禁止该类通道。
    Prohibited,
    /// 会话限制或目标不可达；SSH 协议不能进一步区分。
    ConnectFailed,
    /// 服务器不支持通道类型。
    UnknownType,
    /// 服务器资源不足。
    ResourceShortage,
    /// 未知的数值拒绝码。
    Other(u32),
}

/// 可由 core 确定性映射为 AppError 的错误分类。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("参数无效")]
    InvalidInput,
    #[error("同步接口不能在异步运行时内直接调用")]
    AsyncContext,
    #[error("运行时或回调任务失败")]
    Runtime,
    #[error("域名解析失败")]
    Dns,
    #[error("网络操作失败：{0:?}")]
    Network(io::ErrorKind),
    #[error("操作超时：{0}")]
    Timeout(&'static str),
    #[error("主机密钥不匹配或被回调拒绝")]
    HostKeyRejected,
    #[error("认证被拒绝")]
    Authentication,
    #[error("登录尝试次数达到服务器上限")]
    TooManyAuthAttempts,
    #[error("私钥读取或解密失败")]
    PrivateKey,
    #[error("认证回调取消或回答数量无效")]
    Callback,
    #[error("SSH agent 不可用或拒绝签名")]
    Agent,
    #[error("服务器没有允许的签名算法")]
    Algorithm,
    #[error("通道被拒绝：{0:?}")]
    ChannelRejected(ChannelFailure),
    #[error("通道请求被拒绝：{0}")]
    RequestRejected(&'static str),
    #[error("连接或通道已关闭")]
    Disconnected,
    #[error("SSH 或子系统协议错误")]
    Protocol,
}

impl From<russh::Error> for Error {
    fn from(value: russh::Error) -> Self {
        use russh::{ChannelOpenFailure as C, Error as E};
        match value {
            E::UnknownKey | E::KeyChanged { .. } => Self::HostKeyRejected,
            E::ChannelOpenFailure(reason) => Self::ChannelRejected(match reason {
                C::AdministrativelyProhibited => ChannelFailure::Prohibited,
                C::ConnectFailed => ChannelFailure::ConnectFailed,
                C::UnknownChannelType => ChannelFailure::UnknownType,
                C::ResourceShortage => ChannelFailure::ResourceShortage,
                C::Other { code, .. } => ChannelFailure::Other(code),
            }),
            E::RequestDenied => Self::RequestRejected("全局请求"),
            E::ConnectionTimeout | E::KeepaliveTimeout | E::InactivityTimeout => {
                Self::Timeout("SSH")
            }
            E::IO(e) => Self::Network(e.kind()),
            E::Disconnect | E::HUP | E::SendError | E::RecvError => Self::Disconnected,
            _ => Self::Protocol,
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        if let Some(error) = value
            .get_ref()
            .and_then(|error| error.downcast_ref::<Self>())
        {
            return error.clone();
        }
        Self::Network(value.kind())
    }
}

pub(crate) fn io_error(value: Error) -> io::Error {
    let kind = match value {
        Error::Timeout(_) => io::ErrorKind::TimedOut,
        Error::Disconnected => io::ErrorKind::BrokenPipe,
        Error::TooManyAuthAttempts => io::ErrorKind::PermissionDenied,
        Error::InvalidInput | Error::AsyncContext => io::ErrorKind::InvalidInput,
        Error::Network(kind) => kind,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_discard_remote_secrets_and_classify_failures() {
        let error = Error::from(russh::Error::ChannelOpenFailure(
            russh::ChannelOpenFailure::Other {
                code: 123,
                reason: "密码口令私钥".into(),
            },
        ));
        assert_eq!(error, Error::ChannelRejected(ChannelFailure::Other(123)));
        assert!(!format!("{error:?} {error}").contains("密码"));
        assert_eq!(
            Error::from(russh::Error::UnknownKey),
            Error::HostKeyRejected
        );
        assert_eq!(
            Error::from(russh::Error::KeepaliveTimeout),
            Error::Timeout("SSH")
        );
        for (reason, expected) in [
            (
                russh::ChannelOpenFailure::AdministrativelyProhibited,
                ChannelFailure::Prohibited,
            ),
            (
                russh::ChannelOpenFailure::ConnectFailed,
                ChannelFailure::ConnectFailed,
            ),
            (
                russh::ChannelOpenFailure::UnknownChannelType,
                ChannelFailure::UnknownType,
            ),
            (
                russh::ChannelOpenFailure::ResourceShortage,
                ChannelFailure::ResourceShortage,
            ),
        ] {
            assert_eq!(
                Error::from(russh::Error::ChannelOpenFailure(reason)),
                Error::ChannelRejected(expected)
            );
        }
        assert_eq!(
            io_error(Error::Timeout("读取")).kind(),
            io::ErrorKind::TimedOut
        );
    }
}
