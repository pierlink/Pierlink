//! 凭据存储与日志脱敏；秘密不实现 Debug、不进入错误参数。
use crate::error::AppError;
use std::{collections::HashMap, sync::Mutex};
mod redact;
pub mod vault;
pub use redact::redact;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::WindowsCredentialStore;

/// 凭据接口；键是引用名，秘密仅由调用方持有和凭据存储保存。
pub trait CredentialStore {
    /// 按引用名读取秘密；条目不存在时返回 None，存储失败时返回错误。
    fn get(&self, key: &str) -> Result<Option<String>, AppError>;
    /// 按引用名保存或替换秘密；成功返回空值，失败返回错误。
    fn set(&self, key: &str, secret: &str) -> Result<(), AppError>;
    /// 按引用名删除秘密；不存在时也成功，存储失败时返回错误。
    fn delete(&self, key: &str) -> Result<(), AppError>;
}

/// 仅用于测试的内存存储；随实例释放，不写磁盘，不适用于生产凭据持久化。
#[derive(Default)]
pub struct MemoryCredentialStore {
    secrets: Mutex<HashMap<String, String>>,
}

impl CredentialStore for MemoryCredentialStore {
    fn get(&self, key: &str) -> Result<Option<String>, AppError> {
        Ok(self
            .secrets
            .lock()
            .map_err(|_| store_error("get"))?
            .get(key)
            .cloned())
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), AppError> {
        self.secrets
            .lock()
            .map_err(|_| store_error("set"))?
            .insert(key.to_owned(), secret.to_owned());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), AppError> {
        self.secrets
            .lock()
            .map_err(|_| store_error("delete"))?
            .remove(key);
        Ok(())
    }
}

fn store_error(operation: &str) -> AppError {
    AppError::new("security.credential_store_failed").with("operation", operation)
}

/// 生成随机秘密（API 密钥、口令等），十六进制表示。
/// 使用由操作系统随机源播种的加密安全随机数生成器。
///
/// :param bytes: 随机字节数（结果长度是它的两倍）
/// :return: 小写十六进制字符串
pub fn random_secret(bytes: usize) -> String {
    (0..bytes)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn check_store(store: &dyn CredentialStore, key: &str) {
        assert_eq!(store.get(key).unwrap(), None);
        store.delete(key).unwrap();
        store.set(key, "密码\nAPI秘密").unwrap();
        assert_eq!(store.get(key).unwrap().as_deref(), Some("密码\nAPI秘密"));
        store.set(key, "").unwrap();
        assert_eq!(store.get(key).unwrap(), Some(String::new()));
        store.delete(key).unwrap();
        assert_eq!(store.get(key).unwrap(), None);
        store.delete(key).unwrap();
    }
    #[test]
    fn 随机秘密长度正确且不重复() {
        let a = random_secret(24);
        let b = random_secret(24);
        assert_eq!(a.len(), 48);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn 内存凭据完整生命周期() {
        let store = MemoryCredentialStore::default();
        check_store(&store, "pierlink/server/test/password");
        store.set("a", "甲").unwrap();
        store.set("b", "乙").unwrap();
        store.delete("a").unwrap();
        assert_eq!(store.get("b").unwrap().as_deref(), Some("乙"));
        assert_eq!(MemoryCredentialStore::default().get("b").unwrap(), None);
    }
    #[test]
    fn 锁损坏不会泄露秘密() {
        let store = MemoryCredentialStore::default();
        let _ = std::panic::catch_unwind(|| {
            let _guard = store.secrets.lock().unwrap();
            panic!("测试锁损坏");
        });
        for result in [
            store.get("secret").map(|_| ()),
            store.set("secret", "secret"),
            store.delete("secret"),
        ] {
            let e = result.unwrap_err();
            assert_eq!(e.code, "security.credential_store_failed");
            assert!(!format!("{e:?}").contains("secret"));
        }
    }
}
