//! Windows 原生凭据管理器适配；不改变 keyring 的全局默认存储。
use super::{CredentialStore, store_error};
use crate::error::AppError;
use keyring_core::{Entry, Error, api::CredentialStoreApi};
use std::sync::Arc;

pub struct WindowsCredentialStore {
    store: Arc<windows_native_keyring_store::Store>,
}

impl WindowsCredentialStore {
    /// 创建 Windows 原生凭据存储；初始化失败时返回不含底层敏感信息的错误。
    pub fn new() -> Result<Self, AppError> {
        Ok(Self {
            store: windows_native_keyring_store::Store::new().map_err(|_| store_error("init"))?,
        })
    }

    fn entry(&self, key: &str) -> Result<Entry, AppError> {
        self.store
            .build("pierlink", key, None)
            .map_err(|_| store_error("entry"))
    }
}

impl CredentialStore for WindowsCredentialStore {
    fn get(&self, key: &str) -> Result<Option<String>, AppError> {
        match self.entry(key)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(Error::NoEntry) => Ok(None),
            Err(_) => Err(store_error("get")),
        }
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), AppError> {
        self.entry(key)?
            .set_password(secret)
            .map_err(|_| store_error("set"))
    }
    fn delete(&self, key: &str) -> Result<(), AppError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(_) => Err(store_error("delete")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 原生凭据完整生命周期() {
        let store = WindowsCredentialStore::new().unwrap();
        let key = format!("pierlink/test/{}", uuid::Uuid::new_v4());
        struct Cleanup<'a>(&'a WindowsCredentialStore, &'a str);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = self.0.delete(self.1);
            }
        }
        let _cleanup = Cleanup(&store, &key);
        super::super::tests::check_store(&store, &key);
    }
}
