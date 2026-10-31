//! Pierlink 内置 SSH 引擎：专用运行时、同步入口与有界通道。
//!
//! 同步接口应在线程式调用方使用；异步调用方请通过 `spawn_blocking` 调用。
//! 本阶段不依赖 core，也不直接向前端暴露错误；接入时由 core 映射为 AppError。

#[cfg(test)]
extern crate self as pierlink_ssh_engine;

mod auth;
#[cfg(test)]
mod auth_tests;
mod channel;
mod connection;
mod error;
mod forwarding;
#[cfg(any(test, feature = "test-support"))]
mod memory_peer;
#[cfg(test)]
mod protocol_tests;
pub mod proxy;
mod runtime;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_files;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod transport_tests;

pub use auth::{Authentication, Challenge, ConfiguredAuthentication, PrivateKeyFile, Prompt};
pub use channel::{
    ChannelCloser, ExitStatus, Forwarded, Input, Output, Session, SftpHandshake, Tunnel,
};
pub use connection::{
    ConnectProgress, ConnectStage, Connection, ConnectionOptions, Engine, HostKey, HostKeyCheck,
    Pty, Statistics,
};
pub use error::{ChannelFailure, Error, Result};
pub use forwarding::{Forwarding, ForwardingRule, ForwardingTarget};

/// 把参数转为 POSIX shell 单引号字面量；含 NUL 时返回参数错误。
pub fn sh_quote(value: &str) -> Result<String> {
    if value.contains('\0') {
        return Err(Error::InvalidInput);
    }
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_handles_untrusted_arguments() {
        assert_eq!(sh_quote("").unwrap(), "''");
        assert_eq!(sh_quote("a'b").unwrap(), "'a'\\''b'");
        for value in ["中文 空格", "$(id)`id`*?[]\n\u{1}"] {
            assert_eq!(sh_quote(value).unwrap(), format!("'{value}'"));
        }
        assert_eq!(sh_quote("\0"), Err(Error::InvalidInput));
    }
}
