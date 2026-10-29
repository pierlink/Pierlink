//! SSH 连接目标。
use std::path::PathBuf;

/// 链接到用户配置的别名，或应用内配置的连接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshTarget {
    /// 从已解析配置构造，连接时不再读取用户配置。
    Explicit(Box<ExplicitTarget>),
    Alias {
        alias: String,
    },
    Manual {
        host: String,
        port: u16,
        user: Option<String>,
        identity_file: Option<PathBuf>,
        jump: Option<Box<SshTarget>>,
    },
}

/// 经参数校验的显式目标；通过 `ssh::args::explicit_target` 创建。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitTarget {
    pub(crate) host: String,
    pub(crate) config_file: PathBuf,
    pub(crate) hostname: String,
    pub(crate) user: String,
}
