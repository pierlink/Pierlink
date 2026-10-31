//! 登录回调的秘密仅留在内存，认证错误只作废实际发送的密码代数。
use super::*;
use crate::{security::vault::Password, ssh::known_hosts::KnownHosts, storage::AuthMethod};
use pierlink_ssh_engine::{
    Authentication, ConfiguredAuthentication, ConnectionOptions, HostKeyCheck, PrivateKeyFile,
};
use std::{
    sync::{TryLockError, atomic::Ordering},
    time::Duration,
};

#[derive(Default)]
struct Progress {
    password: Option<Password>,
    sent: bool,
    error: Option<AppError>,
}

struct Login {
    server: String,
    vault: Arc<PasswordVault>,
    callbacks: Arc<dyn ConnectionCallbacks>,
    cancelled: Arc<AtomicBool>,
    interactive: bool,
    progress: Arc<Mutex<Progress>>,
    passphrases: Arc<Mutex<HashMap<PathBuf, Zeroizing<String>>>>,
    public_key_only: bool,
}

fn cancelled() -> AppError {
    AppError::new("ssh.auth_cancelled")
}

impl Login {
    fn check_cancelled(&self) -> Result<(), AppError> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(cancelled())
        } else {
            Ok(())
        }
    }

    fn fail<T>(&self, error: AppError) -> Option<T> {
        self.progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .error = Some(error);
        None
    }

    fn password(&self) -> Result<Password, AppError> {
        self.check_cancelled()?;
        if let Some(password) = &self
            .progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .password
        {
            return Ok(password.clone());
        }
        let password = match self.vault.get(&self.server)? {
            Some(password) => password,
            None if !self.interactive => return Err(AppError::new("ssh.password_needed")),
            None => {
                let secret = match self.callbacks.password(&self.server, &self.cancelled) {
                    Ok(secret) => secret,
                    Err(_) => {
                        self.check_cancelled()?;
                        return Err(AppError::new("ssh.password_load_failed"));
                    }
                };
                // 晚到的有效密码依然存入库，再处理取消。
                let secret = secret.ok_or_else(cancelled)?;
                let generation = self.vault.set(&self.server, secret.clone())?;
                Password { secret, generation }
            }
        };
        self.progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .password = Some(password.clone());
        self.check_cancelled()?;
        Ok(password)
    }

    fn password_answer(&self) -> Option<String> {
        if !self.interactive && self.vault.seen_two_factor(&self.server) {
            return self.fail(AppError::new("ssh.two_factor_required"));
        }
        match self.password() {
            Ok(password) => {
                self.progress.lock().unwrap_or_else(|p| p.into_inner()).sent = true;
                Some(password.secret.to_string())
            }
            Err(error) => self.fail(error),
        }
    }

    fn answers(&self, challenge: Challenge) -> Option<Vec<String>> {
        if self.check_cancelled().is_err() {
            return self.fail(cancelled());
        }
        let sent = self.progress.lock().unwrap_or_else(|p| p.into_inner()).sent;
        if !sent
            && challenge.prompts.len() == 1
            && !challenge.prompts[0].echo
            && challenge.prompts[0]
                .text
                .to_ascii_lowercase()
                .contains("password")
        {
            return self.password_answer().map(|password| vec![password]);
        }
        if challenge.prompts.is_empty() {
            return Some(Vec::new());
        }
        self.vault.mark_two_factor(&self.server);
        if !self.interactive {
            return self.fail(AppError::new("ssh.two_factor_required"));
        }
        let count = challenge.prompts.len();
        let answers = self
            .callbacks
            .keyboard_interactive(&self.server, challenge, &self.cancelled);
        if self.check_cancelled().is_err() {
            return self.fail(cancelled());
        }
        match answers {
            Ok(Some(answers)) if answers.len() == count => {
                Some(answers.iter().map(|s| s.to_string()).collect())
            }
            _ => self.fail(cancelled()),
        }
    }

    fn phrase(&self, path: &Path, remember: bool) -> Option<String> {
        if self.check_cancelled().is_err() {
            return self.fail(cancelled());
        }
        if remember
            && let Some(phrase) = self
                .passphrases
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(path)
        {
            return Some(phrase.to_string());
        }
        if !self.interactive {
            return self.fail(AppError::new("ssh.password_needed"));
        }
        let phrase = self
            .callbacks
            .passphrase(&self.server, path, &self.cancelled);
        if self.check_cancelled().is_err() {
            return self.fail(cancelled());
        }
        match phrase {
            Ok(Some(phrase)) => {
                if remember {
                    let mut cache = self.passphrases.lock().unwrap_or_else(|p| p.into_inner());
                    if cache.len() >= 128 {
                        cache.clear();
                    }
                    cache.insert(path.to_owned(), phrase.clone());
                }
                Some(phrase.to_string())
            }
            _ => self.fail(cancelled()),
        }
    }

    fn file(self: &Arc<Self>, path: PathBuf, remember: bool) -> PrivateKeyFile {
        let login = self.clone();
        let key_path = path.clone();
        let cached = remember
            && self
                .passphrases
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains_key(&path);
        PrivateKeyFile {
            path,
            passphrase: (self.interactive || cached).then(|| {
                Arc::new(move || login.phrase(&key_path, remember))
                    as Arc<dyn Fn() -> Option<String> + Send + Sync>
            }),
        }
    }

    fn authentication(self: &Arc<Self>, server: &Server) -> Result<Authentication, AppError> {
        let private_keys = match server.auth_method {
            AuthMethod::Password => vec![],
            AuthMethod::Agent => (if server.identity_files.is_empty() {
                default_keys()
            } else {
                server
                    .identity_files
                    .iter()
                    .filter(|p| p.as_str() != "none")
                    .map(PathBuf::from)
                    .collect()
            })
            .into_iter()
            .map(|path| self.file(path, false))
            .collect(),
            AuthMethod::Key => {
                let path = server
                    .identity_file
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        AppError::new("server.invalid_field").with("field", "identity_file")
                    })?;
                vec![self.file(PathBuf::from(path), true)]
            }
            AuthMethod::DedicatedKey => {
                let credential = self.callbacks.dedicated_key(&self.server, &self.cancelled);
                self.check_cancelled()?;
                let credential = credential.map_err(|_| {
                    AppError::new("keys.local_failed").with_reason(
                        "builtin.couldn_t_read_local_key_credentials",
                        "读取本机密钥凭据失败",
                    )
                })?;
                let credential = credential.filter(|key| key.path.is_file()).ok_or_else(|| {
                    AppError::new("keys.local_missing").with("server", &server.name)
                })?;
                vec![PrivateKeyFile {
                    path: credential.path,
                    passphrase: Some(Arc::new(move || Some(credential.passphrase.to_string()))),
                }]
            }
        };

        let password_login = self.clone();
        let answers_login = self.clone();
        if self.public_key_only {
            return Ok(Authentication::Configured(ConfiguredAuthentication {
                agent: false,
                agent_identities_only: true,
                private_keys,
                skip_unusable_keys: false,
                password: None,
                answers: Arc::new(|_| None),
            }));
        }
        Ok(Authentication::Configured(ConfiguredAuthentication {
            agent: server.auth_method == AuthMethod::Agent,
            agent_identities_only: server.identities_only,
            private_keys,
            skip_unusable_keys: server.auth_method == AuthMethod::Agent,
            password: matches!(server.auth_method, AuthMethod::Password | AuthMethod::Agent).then(
                || {
                    Arc::new(move || password_login.password_answer())
                        as Arc<dyn Fn() -> Option<String> + Send + Sync>
                },
            ),
            answers: Arc::new(move |challenge| answers_login.answers(challenge)),
        }))
    }

    fn finish(
        &self,
        server: &Server,
        result: pierlink_ssh_engine::Result<Connection>,
    ) -> Result<Connection, AppError> {
        let mut progress = self.progress.lock().unwrap_or_else(|p| p.into_inner());
        let password = progress.password.take();
        self.check_cancelled()?;
        if let Some(error) = progress.error.take() {
            return Err(error);
        }
        if let Err(error) = &result {
            if matches!(error, pierlink_ssh_engine::Error::Callback)
                && server.auth_method == AuthMethod::Key
                && !self.interactive
            {
                return Err(AppError::new("ssh.password_needed").with_reason(
                    "builtin.the_private_key_requires_a_passphrase",
                    "私钥需要口令",
                ));
            }
            if matches!(error, pierlink_ssh_engine::Error::PrivateKey)
                && let Some(path) = server.identity_file.as_ref()
            {
                self.passphrases
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(Path::new(path));
            }
            if matches!(error, pierlink_ssh_engine::Error::Authentication)
                && progress.sent
                && let Some(password) = password
            {
                // 作废回调不在本次登录状态的锁内执行。
                drop(progress);
                self.vault.invalidate(&self.server, password.generation);
                return Err(error::map_engine_error(error, error::Operation::Other)
                    .with("password_generation", password.generation));
            }
        }
        result.map_err(|error| match error {
            pierlink_ssh_engine::Error::Disconnected => error::login_disconnected(),
            error => error::map_engine_error(&error, error::Operation::Other),
        })
    }
}

impl ConnectionManager {
    pub(super) fn gate(&self, server: &str) -> Arc<Mutex<()>> {
        let mut gates = self.gates.lock().unwrap_or_else(|p| p.into_inner());
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(server).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(Mutex::new(()));
        gates.insert(server.into(), Arc::downgrade(&gate));
        gate
    }

    pub(super) fn connect_with(
        &self,
        request: ConnectionRequest<'_>,
        mut options: ConnectionOptions,
        connect: impl FnOnce(
            ConnectionOptions,
            Authentication,
        ) -> pierlink_ssh_engine::Result<Connection>,
    ) -> Result<Connection, AppError> {
        if let Some(progress) = request.progress.clone() {
            options.progress = Some(Arc::new(move |stage| {
                use pierlink_ssh_engine::ConnectStage as Stage;
                progress(match stage {
                    Stage::Resolve => crate::terminal::ConnectStage::Resolve,
                    Stage::Connect => crate::terminal::ConnectStage::Connect,
                    Stage::Auth => crate::terminal::ConnectStage::Auth,
                });
            }));
        }
        let gate = self.gate(&request.server.id);
        let _guard = loop {
            if request.cancelled.load(Ordering::Acquire) {
                return Err(cancelled());
            }
            match gate.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::Poisoned(p)) => break p.into_inner(),
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(25)),
            }
        };
        let method = request.server.auth_method;
        let login = Arc::new(Login {
            server: request.server.id.clone(),
            vault: request.vault,
            callbacks: request.callbacks,
            cancelled: request.cancelled,
            interactive: request.interactive,
            progress: Arc::new(Mutex::new(Progress::default())),
            passphrases: self.passphrases.clone(),
            public_key_only: self.verify_server.as_deref() == Some(request.server.id.as_str()),
        });
        if !login.interactive
            && login.vault.seen_two_factor(&login.server)
            && matches!(method, AuthMethod::Password)
        {
            return Err(AppError::new("ssh.two_factor_required"));
        }
        if method == AuthMethod::Password && !(request.interactive && request.progress.is_some()) {
            login.password()?;
        }
        let callbacks = login.callbacks.clone();
        let server_id = login.server.clone();
        let cancelled = login.cancelled.clone();
        let progress = login.progress.clone();
        let host_key_alias = request.server.host_key_alias.clone();
        // 读取失败只影响协商顺序，实际主机密钥仍必须通过回调核对。
        options.preferred_host_key_algorithms = KnownHosts::for_user()
            .and_then(|hosts| {
                hosts.preferred_algorithms(&options.host, options.port, host_key_alias.as_deref())
            })
            .unwrap_or_default();
        options.host_key = HostKeyCheck::Callback(Arc::new(move |mut key| {
            if let Some(alias) = &host_key_alias {
                key.host = alias.clone();
                // HostKeyAlias 是 known_hosts 的完整查询名，不附加目标端口。
                key.port = 22;
            }
            if cancelled.load(Ordering::Acquire) {
                return false;
            }
            match callbacks.check_host_key(&server_id, &key, &cancelled) {
                Ok(trusted) => trusted && !cancelled.load(Ordering::Acquire),
                Err(error) => {
                    progress.lock().unwrap_or_else(|p| p.into_inner()).error =
                        Some(safe_host_error(&error));
                    false
                }
            }
        }));
        options.cancelled = Some(login.cancelled.clone());
        // 窗口在握手前固定；首次认证中才识别的验证码只能从下一条新连接起采用大窗口。
        if request.class == ConnectionClass::Bulk || login.vault.seen_two_factor(&login.server) {
            options.window_size = 2 * 1024 * 1024;
        }
        let auth = login.authentication(request.server)?;
        let result = connect(options, auth);
        login.finish(request.server, result)
    }
}

fn default_keys() -> Vec<PathBuf> {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    home.map(|home| {
        ["id_ed25519", "id_ecdsa", "id_rsa"]
            .into_iter()
            .map(|name| PathBuf::from(&home).join(".ssh").join(name))
            .collect()
    })
    .unwrap_or_default()
}

fn safe_host_error(error: &AppError) -> AppError {
    let code = match error.code.as_str() {
        "ssh.host_key_changed" => "ssh.host_key_changed",
        // 还没确认过的新主机：界面会先走指纹确认流程
        "ssh.host_key_unknown" => "ssh.host_key_unknown",
        "ssh.auth_cancelled" => "ssh.auth_cancelled",
        "ssh.invalid_host" => "ssh.invalid_host",
        "ssh.invalid_host_key" => "ssh.invalid_host_key",
        "ssh.known_hosts_failed" => "ssh.known_hosts_failed",
        _ => "ssh.failed",
    };
    let mut safe = AppError::new(code).with_reason(
        "builtin.host_key_verification_did_not_complete",
        "主机密钥核对未完成",
    );
    // ssh.failed 的提示用 stderr_tail 占位
    if code == "ssh.failed" {
        safe = safe.with_text(
            "stderr_tail",
            "builtin.host_key_verification_did_not_complete",
            "主机密钥核对未完成",
        );
    }
    for name in ["fingerprint", "known_fingerprint", "file", "line"] {
        if let Some(value) = error.params.get(name) {
            safe = safe.with(name, value);
        }
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 主机密钥错误保留指纹文件行号且过滤未知参数() {
        for code in ["ssh.host_key_changed", "ssh.host_key_unknown"] {
            let original = AppError::new(code)
                .with("fingerprint", "SHA256:new")
                .with("known_fingerprint", "SHA256:old")
                .with("file", "/用户/.ssh/known_hosts")
                .with("line", 12)
                .with("reason", "不应泄露的回调原文")
                .with("password", "秘密")
                .with("unknown", "秘密");
            let safe = safe_host_error(&original);
            assert_eq!(safe.code, code);
            for name in ["fingerprint", "known_fingerprint", "file", "line"] {
                assert_eq!(safe.params[name], original.params[name]);
            }
            assert_eq!(safe.params.len(), 6);
            assert_eq!(
                safe.params["reason_key"],
                "builtin.host_key_verification_did_not_complete"
            );
            assert!(!format!("{safe:?}").contains("秘密"));
            assert!(!format!("{safe:?}").contains("回调原文"));
        }
        // 列表外的错误码归为 ssh.failed：它的提示用 stderr_tail，也要带上可翻译的说明
        let other = safe_host_error(&AppError::new("ssh.unreachable").with("reason", "原文"));
        assert_eq!(other.code, "ssh.failed");
        assert_eq!(
            other.params["stderr_tail_key"],
            "builtin.host_key_verification_did_not_complete"
        );
        assert!(!format!("{other:?}").contains("原文"));
    }
}
