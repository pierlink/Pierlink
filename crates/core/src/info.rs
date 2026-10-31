//! 应用基本信息。

use serde::Serialize;
use specta::Type;

/// 应用版本与运行平台，用于“关于”页面和诊断信息。
#[derive(Debug, Clone, Serialize, Type, PartialEq, Eq)]
pub struct AppInfo {
    /// 应用版本号
    pub version: String,
    /// 操作系统（windows / linux / macos）
    pub os: String,
    /// CPU 架构（x86_64 / aarch64）
    pub arch: String,
}

/// 读取当前应用信息。
///
/// :return: 版本号与运行平台
pub fn app_info() -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn 版本号来自包信息() {
        let info = super::app_info();
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(!info.os.is_empty());
    }
}
