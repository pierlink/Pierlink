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
}
