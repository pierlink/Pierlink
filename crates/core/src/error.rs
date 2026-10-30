//! 统一错误类型。
//!
//! 后端返回给前端的错误一律是“错误码 + 参数”，由前端按当前语言翻译（设计 3.8）；
//! 错误信息里绝不能带密码、密钥等敏感内容。

use serde::Serialize;
use specta::Type;
use std::collections::BTreeMap;

/// 返回给前端的错误。
#[derive(Debug, Clone, Serialize, Type, thiserror::Error)]
#[error("{code}")]
pub struct AppError {
    /// 错误码，形如 `ssh.host_key_changed`，前端据此查翻译
    pub code: String,
    /// 翻译时代入的参数（如服务器名、端口），只放可以展示给用户的值
    pub params: BTreeMap<String, String>,
}

impl AppError {
    /// 创建不带参数的错误。
    ///
    /// :param code: 错误码
    /// :return: 错误对象
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            params: BTreeMap::new(),
        }
    }

    /// 追加一个翻译参数，便于链式调用。
    ///
    /// :param key: 参数名
    /// :param value: 参数值（必须可以展示给用户）
    /// :return: 追加参数后的错误对象
    pub fn with(mut self, key: impl Into<String>, value: impl ToString) -> Self {
        self.params.insert(key.into(), value.to_string());
        self
    }

    /// 追加固定说明，返回同时带中文原文和翻译键的错误。
    ///
    /// `name` 是参数名，`key` 对应 `errors.detail` 下的译文；译文可用 `{{参数名}}`
    /// 引用同一错误的其他参数。`text` 是日志、AI 接入和缺少译文时使用的中文原文。
    pub fn with_text(self, name: &str, key: &'static str, text: impl ToString) -> Self {
        self.with(name, text).with(format!("{name}_key"), key)
    }

    /// 追加原因并返回错误；`key` 是翻译键，`text` 是保留给日志和 AI 的中文原文。
    pub fn with_reason(self, key: &'static str, text: impl ToString) -> Self {
        self.with_text("reason", key, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 错误码和参数序列化() {
        let e = AppError::new("tunnel.port_in_use")
            .with("port", 8080)
            .with("process", "node.exe");
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(
            json,
            r#"{"code":"tunnel.port_in_use","params":{"port":"8080","process":"node.exe"}}"#
        );
    }

    #[test]
    fn 固定说明保留原文并按参数名保存翻译键() {
        let error = AppError::new("ssh.io_failed")
            .with("status", 5)
            .with_text("operation", "process.read", "读取输出")
            .with_text("operation", "process.write", String::from("写入输入"));
        let value = serde_json::to_value(error).unwrap();
        assert_eq!(value["params"]["operation"], "写入输入");
        assert_eq!(value["params"]["operation_key"], "process.write");
        assert_eq!(value["params"]["status"], "5");
        assert!(value["params"].get("detail").is_none());
    }

    #[test]
    fn 原因简写保留中文和插值参数() {
        let error = AppError::new("sftp.failure")
            .with_reason("sftp.too_deep", format!("目录层级超过 {} 层", 128))
            .with("limit", 128);
        assert_eq!(error.params["reason"], "目录层级超过 128 层");
        assert_eq!(error.params["reason_key"], "sftp.too_deep");
        assert_eq!(error.params["limit"], "128");
    }
}
