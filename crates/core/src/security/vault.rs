//! 按服务器保存本次运行的密码；代数在清空后也不复用，订阅队列只保留一次唤醒。
use crate::error::AppError;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Condvar, Mutex, Weak, mpsc},
    time::Duration,
};
pub use zeroize::Zeroizing;

/// 调用方提供的凭据载入器；参数为服务器 ID，返回密码、未保存或脱敏错误。
pub type PasswordLoader = dyn Fn(&str) -> Result<Option<Zeroizing<String>>, AppError> + Send + Sync;

/// 密码快照不实现 Debug；使用后自动擦除其字符串内存。
#[derive(Clone)]
pub struct Password {
    pub secret: Zeroizing<String>,
    pub generation: u64,
}
#[derive(Default)]
struct State {
    passwords: HashMap<String, Password>,
    revisions: HashMap<String, u64>,
    attempted: HashSet<String>,
    loading: HashSet<String>,
    epoch: u64,
    two_factor: HashSet<String>,
    generation: u64,
    next_subscription: u64,
    subscriptions: HashMap<u64, (String, mpsc::SyncSender<()>)>,
}
/// 密码库仅保留当前值和本次运行的服务器标记；退出时调用 clear_all。
#[derive(Default)]
pub struct PasswordVault {
    state: Mutex<State>,
    changed: Condvar,
    loader: Option<Arc<PasswordLoader>>,
    on_invalidate: Option<Arc<InvalidateHook>>,
}
/// 密码因登录被拒而作废后的回调；参数为服务器 ID（例如删掉保存的错误密码）。不在库的锁内调用。
pub type InvalidateHook = dyn Fn(&str) + Send + Sync;
impl PasswordVault {
    /// 按 server 返回已有内存密码快照，不触发凭据读取，用于执行前后错误脱敏。
    pub(crate) fn peek(&self, server: &str) -> Option<Password> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .passwords
            .get(server)
            .cloned()
    }
    /// 参数为可选凭据载入器；返回空密码库。载入器每台服务器最多尝试一次，作废后不再加载旧密码。
    pub fn new(loader: Option<Arc<PasswordLoader>>) -> Self {
        Self {
            loader,
            ..Self::default()
        }
    }
    /// 设置作废回调并返回自身；登录被拒作废密码时调用，主动 forget 和 clear_all 不调用。
    pub fn with_on_invalidate(mut self, hook: Arc<InvalidateHook>) -> Self {
        self.on_invalidate = Some(hook);
        self
    }
    /// 按服务器返回密码快照；缺失时合并并发载入。载入失败只返回固定错误，不转发回调错误内容。
    pub fn get(&self, server: &str) -> Result<Option<Password>, AppError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(password) = state.passwords.get(server) {
                return Ok(Some(password.clone()));
            }
            if state.loading.contains(server) {
                state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
                continue;
            }
            if state.attempted.contains(server) || self.loader.is_none() || state.epoch != 0 {
                return Ok(None);
            }
            state.attempted.insert(server.into());
            state.loading.insert(server.into());
            let epoch = state.epoch;
            let revision = state.revisions.get(server).copied();
            drop(state);
            let loaded = self
                .loader
                .as_ref()
                .map(|loader| loader(server))
                .transpose();
            state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.loading.remove(server);
            self.changed.notify_all();
            if state.revisions.get(server).copied() != revision {
                return Ok(state.passwords.get(server).cloned());
            }
            let loaded = loaded
                .map_err(|_| AppError::new("ssh.password_load_failed"))?
                .flatten();
            if epoch == state.epoch
                && !state.passwords.contains_key(server)
                && let Some(secret) = loaded
            {
                validate(&secret)?;
                put(&mut state, server, secret)?;
            }
            return Ok(state.passwords.get(server).cloned());
        }
    }
    /// 保存 server 的密码并返回新代数，通知该服务器所有订阅者；非法密码不写入。
    pub fn set(&self, server: &str, secret: Zeroizing<String>) -> Result<u64, AppError> {
        validate(&secret)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.attempted.insert(server.into());
        let generation = put(&mut state, server, secret)?;
        self.changed.notify_all();
        Ok(generation)
    }
    /// 仅在 server 的当前代数等于 generation 时擦除密码，返回是否作废；不会重新加载失效凭据。
    pub fn invalidate(&self, server: &str, generation: u64) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state
            .passwords
            .get(server)
            .is_some_and(|p| p.generation == generation)
        {
            return false;
        }
        state.passwords.remove(server);
        for (target, sender) in state.subscriptions.values() {
            if target == server {
                let _ = sender.try_send(());
            }
        }
        drop(state);
        if let Some(hook) = &self.on_invalidate {
            hook(server);
        }
        true
    }
    /// 忘掉 server 的密码（删除服务器、改密码、关掉记住密码时调用），返回之前是否有密码。
    /// 同时允许下次需要时重新从凭据载入器读取（例如用户刚保存了新密码）。
    pub fn forget(&self, server: &str) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let had = state.passwords.remove(server).is_some();
        state.attempted.remove(server);
        // 换一个修订号，让正在进行的载入作废，不把忘掉之前读到的旧密码放回来
        state.generation = state.generation.saturating_add(1);
        let revision = state.generation;
        state.revisions.insert(server.into(), revision);
        state.two_factor.remove(server);
        for (target, sender) in state.subscriptions.values() {
            if target == server {
                let _ = sender.try_send(());
            }
        }
        self.changed.notify_all();
        had
    }
    /// 查询 server 当前是否已有密码；不触发凭据载入。
    pub fn has(&self, server: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .passwords
            .contains_key(server)
    }
    /// 标记 server 曾在密码后要求其他验证；无返回值，标记只保存在内存。
    pub fn mark_two_factor(&self, server: &str) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .two_factor
            .insert(server.into());
    }
    /// 查询 server 是否曾要求二次验证；返回本次运行的内存标记。
    pub fn seen_two_factor(&self, server: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .two_factor
            .contains(server)
    }
    /// 为 server 创建容量为一的变化订阅；设置、作废和清空时通知，订阅释放时立即注销。
    pub fn subscribe(self: &Arc<Self>, server: &str) -> VaultSubscription {
        let (tx, rx) = mpsc::sync_channel(1);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let id = state.next_subscription;
        state.next_subscription = state.next_subscription.wrapping_add(1);
        state.subscriptions.insert(id, (server.into(), tx));
        VaultSubscription {
            vault: Arc::downgrade(self),
            id,
            receiver: rx,
        }
    }
    /// 擦除全部密码及二次验证标记；无参数和返回值，保留代数避免旧连接作废新密码。
    pub fn clear_all(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.passwords.clear();
        state.two_factor.clear();
        // 不重新启用载入器，防止退出清理后再次取回凭据；同时使正在进行的载入过期。
        state.epoch = state.epoch.wrapping_add(1);
        for (_, sender) in state.subscriptions.values() {
            let _ = sender.try_send(());
        }
        self.changed.notify_all();
    }
}
fn validate(secret: &str) -> Result<(), AppError> {
    if secret.is_empty() || secret.contains(['\0', '\n', '\r']) || secret.len() > 1024 {
        return Err(AppError::new("ssh.invalid_password"));
    }
    Ok(())
}
fn put(state: &mut State, server: &str, secret: Zeroizing<String>) -> Result<u64, AppError> {
    state.generation = state
        .generation
        .checked_add(1)
        .ok_or_else(|| AppError::new("ssh.password_generation_exhausted"))?;
    let generation = state.generation;
    state.revisions.insert(server.into(), generation);
    state
        .passwords
        .insert(server.into(), Password { secret, generation });
    for (target, sender) in state.subscriptions.values() {
        if target == server {
            let _ = sender.try_send(());
        }
    }
    Ok(generation)
}
/// 有界变化通知的所有者；不含密码，丢弃后注销。
pub struct VaultSubscription {
    vault: Weak<PasswordVault>,
    id: u64,
    receiver: mpsc::Receiver<()>,
}
impl VaultSubscription {
    /// 等待至多 timeout；返回期间是否收到该服务器的新密码通知。
    pub fn wait(&self, timeout: Duration) -> bool {
        self.receiver.recv_timeout(timeout).is_ok()
    }
}
impl Drop for VaultSubscription {
    fn drop(&mut self) {
        if let Some(vault) = self.vault.upgrade() {
            vault
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .subscriptions
                .remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn 代数作废订阅隔离与退出清空() {
        let vault = Arc::new(PasswordVault::new(None));
        let subscription = vault.subscribe("a");
        let other = vault.subscribe("b");
        let first = vault.set("a", Zeroizing::new("旧密码".into())).unwrap();
        let second = vault.set("a", Zeroizing::new("新密码".into())).unwrap();
        assert!(subscription.wait(Duration::ZERO));
        assert!(!subscription.wait(Duration::ZERO));
        assert!(!other.wait(Duration::ZERO));
        assert!(!vault.invalidate("a", first));
        assert!(vault.has("a"));
        assert_eq!(&**vault.get("a").unwrap().unwrap().secret, "新密码");
        assert!(vault.invalidate("a", second));
        assert!(!vault.has("a"));
        vault.mark_two_factor("a");
        assert!(vault.seen_two_factor("a"));
        vault.set("a", Zeroizing::new("待忘记".into())).unwrap();
        let watcher = vault.subscribe("a");
        assert!(vault.forget("a"));
        assert!(watcher.wait(Duration::ZERO));
        assert!(!vault.has("a") && !vault.seen_two_factor("a"));
        assert!(!vault.forget("a"));
        drop(watcher);
        vault.mark_two_factor("a");
        vault.clear_all();
        assert!(!vault.seen_two_factor("a"));
        assert!(vault.get("a").unwrap().is_none());
        assert!(vault.set("a", Zeroizing::new("再输入".into())).unwrap() > second);
        drop(subscription);
        drop(other);
        assert!(vault.state.lock().unwrap().subscriptions.is_empty());
        for secret in ["", "\n", "\r", "\0"] {
            assert!(vault.set("a", Zeroizing::new(secret.into())).is_err());
        }
    }
    #[test]
    fn 作废回调只在登录被拒作废时调用() {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = calls.clone();
        let vault = PasswordVault::new(None).with_on_invalidate(Arc::new(move |server| {
            seen.lock().unwrap().push(server.into());
        }));
        let generation = vault.set("a", Zeroizing::new("密码".into())).unwrap();
        assert!(!vault.invalidate("a", generation + 1));
        assert!(vault.invalidate("a", generation));
        vault.set("b", Zeroizing::new("密码".into())).unwrap();
        vault.forget("b");
        vault.set("c", Zeroizing::new("密码".into())).unwrap();
        vault.clear_all();
        assert_eq!(*calls.lock().unwrap(), vec!["a".to_string()]);
    }
    #[test]
    fn 凭据只加载一次且作废后不再取旧密码() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let vault = PasswordVault::new(Some(Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(Some(Zeroizing::new("保存的密码".into())))
        })));
        let password = vault.get("a").unwrap().unwrap();
        assert!(vault.invalidate("a", password.generation));
        assert!(vault.get("a").unwrap().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod loading_tests {
    use super::*;
    use std::{sync::mpsc, thread};
    #[test]
    fn 载入期间设置其他服务器不丢密码_同服务器新值及退出优先() {
        for operation in ["other", "same", "clear"] {
            let (started, entered) = mpsc::channel();
            let (release, wait) = mpsc::channel();
            let wait = Mutex::new(wait);
            let vault = Arc::new(PasswordVault::new(Some(Arc::new(move |_| {
                started.send(()).unwrap();
                wait.lock().unwrap().recv().unwrap();
                Ok(Some(Zeroizing::new("保存的旧密码".into())))
            }))));
            let loading = vault.clone();
            let worker = thread::spawn(move || loading.get("a").unwrap());
            entered.recv_timeout(Duration::from_secs(2)).unwrap();
            match operation {
                "other" => {
                    vault.set("b", Zeroizing::new("其他密码".into())).unwrap();
                }
                "same" => {
                    vault.set("a", Zeroizing::new("用户新密码".into())).unwrap();
                }
                _ => vault.clear_all(),
            }
            release.send(()).unwrap();
            let result = worker.join().unwrap();
            match operation {
                "other" => assert_eq!(&**result.unwrap().secret, "保存的旧密码"),
                "same" => assert_eq!(&**result.unwrap().secret, "用户新密码"),
                _ => assert!(result.is_none()),
            }
        }
        let vault = PasswordVault::new(Some(Arc::new(|_| {
            Err(AppError::new("secret-error").with("secret", "秘密"))
        })));
        let error = vault.get("a").err().unwrap();
        assert!(!format!("{error:?}").contains("秘密"));
        assert_eq!(error.code, "ssh.password_load_failed");
        assert!(vault.get("a").unwrap().is_none());
    }
}
