use super::*;
use crate::storage::{AuthMethod, ServerSource};
use pierlink_ssh_engine::{
    Authentication, ConfiguredAuthentication, ConnectionOptions, Error, HostKeyCheck, Prompt,
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Callbacks {
    host_keys: Mutex<Vec<HostKey>>,
    passwords: AtomicUsize,
    phrases: AtomicUsize,
    challenges: Mutex<Vec<Challenge>>,
    phrase: Mutex<Option<String>>,
    password: Mutex<Option<String>>,
    otp_secret: Mutex<Option<String>>,
    dedicated: Mutex<Option<(PathBuf, String)>>,
    cancel: bool,
}

impl ConnectionCallbacks for Callbacks {
    fn check_host_key(
        &self,
        _: &str,
        key: &HostKey,
        cancelled: &AtomicBool,
    ) -> Result<bool, AppError> {
        self.host_keys.lock().unwrap().push(key.clone());
        Ok(!cancelled.load(Ordering::Acquire))
    }
    fn password(
        &self,
        _: &str,
        cancelled: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        self.passwords.fetch_add(1, Ordering::SeqCst);
        if self.cancel {
            cancelled.store(true, Ordering::Release);
        }
        Ok(Some(Zeroizing::new(
            self.password
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "测试密码".into()),
        )))
    }
    fn passphrase(
        &self,
        _: &str,
        _: &Path,
        _: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        self.phrases.fetch_add(1, Ordering::SeqCst);
        Ok(self.phrase.lock().unwrap().clone().map(Zeroizing::new))
    }
    fn dedicated_key(
        &self,
        _: &str,
        _: &AtomicBool,
    ) -> Result<Option<DedicatedCredential>, AppError> {
        Ok(self
            .dedicated
            .lock()
            .unwrap()
            .as_ref()
            .map(|(path, phrase)| DedicatedCredential {
                path: path.clone(),
                passphrase: Zeroizing::new(phrase.clone()),
            }))
    }
    fn keyboard_interactive(
        &self,
        _: &str,
        challenge: Challenge,
        _: &AtomicBool,
    ) -> Result<Option<Vec<Zeroizing<String>>>, AppError> {
        let count = challenge.prompts.len();
        self.challenges.lock().unwrap().push(challenge);
        let answer = self
            .otp_secret
            .lock()
            .unwrap()
            .as_ref()
            .map(|secret| {
                let seconds = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                totp::code(secret, seconds, 6)
            })
            .unwrap_or_else(|| "123456".into());
        Ok(Some(
            (0..count).map(|_| Zeroizing::new(answer.clone())).collect(),
        ))
    }
}

struct Fixture {
    server: Server,
    vault: Arc<PasswordVault>,
    callbacks: Arc<Callbacks>,
    cancelled: Arc<AtomicBool>,
    manager: ConnectionManager,
}

impl Fixture {
    fn new(method: AuthMethod) -> Self {
        let mut server = crate::server_input::build_alias_server("test", None, "s", 0).unwrap();
        server.source = ServerSource::Manual;
        server.ssh_alias = None;
        server.host = Some("127.0.0.1".into());
        server.username = Some("tester".into());
        server.auth_method = method;
        server.identity_file = Some("指定私钥".into());
        Self {
            server,
            vault: Arc::new(PasswordVault::default()),
            callbacks: Arc::new(Callbacks::default()),
            cancelled: Arc::new(AtomicBool::new(false)),
            manager: ConnectionManager::default(),
        }
    }

    fn request(&self, interactive: bool) -> ConnectionRequest<'_> {
        ConnectionRequest {
            server: &self.server,
            class: ConnectionClass::Main,
            vault: self.vault.clone(),
            callbacks: self.callbacks.clone(),
            cancelled: self.cancelled.clone(),
            interactive,
        }
    }

    fn run(
        &self,
        interactive: bool,
        connect: impl FnOnce(ConnectionOptions, ConfiguredAuthentication) -> Result<Connection, Error>,
    ) -> AppError {
        self.manager
            .connect_with(self.request(interactive), options(), |opts, auth| {
                let Authentication::Configured(auth) = auth else {
                    panic!("必须使用确定性登录")
                };
                connect(opts, auth)
            })
            .err()
            .unwrap()
    }
}

fn options() -> ConnectionOptions {
    ConnectionOptions::new(
        "memory",
        22,
        "tester",
        HostKeyCheck::Fingerprint(String::new()),
    )
}

fn challenge(text: &str, echo: bool) -> Challenge {
    Challenge {
        name: "认证".into(),
        instructions: "不可信提示".into(),
        prompts: vec![Prompt {
            text: text.into(),
            echo,
        }],
    }
}

#[test]
fn 密码只取一次且被拒作废对应代数和保存凭据() {
    for newer in [false, true] {
        let mut f = Fixture::new(AuthMethod::Password);
        let invalidations = Arc::new(AtomicUsize::new(0));
        let calls = invalidations.clone();
        f.vault = Arc::new(
            PasswordVault::new(Some(Arc::new(|_| {
                Ok(Some(Zeroizing::new("旧密码".into())))
            })))
            .with_on_invalidate(Arc::new(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
            })),
        );
        let error = f.run(false, |_, auth| {
            assert!(!auth.agent && auth.private_keys.is_empty());
            assert_eq!(
                (auth.answers)(challenge("PaSsWoRd:", false)).unwrap(),
                vec!["旧密码"]
            );
            if newer {
                f.vault.set("s", Zeroizing::new("新密码".into())).unwrap();
            }
            Err(Error::Authentication)
        });
        assert_eq!(error.code, "ssh.auth_failed");
        assert_eq!(f.vault.has("s"), newer);
        assert_eq!(invalidations.load(Ordering::SeqCst), usize::from(!newer));
        assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 0);
        assert!(!format!("{error:?}").contains("密码"));
    }
}

#[test]
fn 后台缺密码不建连接_并发错误密码最多一次() {
    let f = Fixture::new(AuthMethod::Password);
    assert_eq!(
        f.run(false, |_, _| panic!("后台缺密码不能建连")).code,
        "ssh.password_needed"
    );
    f.vault.set("s", Zeroizing::new("旧密码".into())).unwrap();
    let connections = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..6)
            .map(|_| {
                scope.spawn(|| {
                    let error = f.run(false, |_, auth| {
                        connections.fetch_add(1, Ordering::SeqCst);
                        assert!((auth.password.unwrap())().is_some());
                        std::thread::sleep(Duration::from_millis(25));
                        Err(Error::Authentication)
                    });
                    assert!(matches!(
                        error.code.as_str(),
                        "ssh.auth_failed" | "ssh.password_needed"
                    ));
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    });
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[test]
fn 建连前询问并缓存密码_取消保留晚到密码() {
    for cancel in [false, true] {
        let mut f = Fixture::new(AuthMethod::Password);
        f.callbacks = Arc::new(Callbacks {
            cancel,
            ..Default::default()
        });
        let error = f.run(true, |_, auth| {
            assert!(!cancel);
            assert!(f.vault.has("s"));
            assert_eq!((auth.password.unwrap())().as_deref(), Some("测试密码"));
            f.cancelled.store(true, Ordering::Release);
            Err(Error::Authentication)
        });
        assert_eq!(error.code, "ssh.auth_cancelled");
        assert!(f.vault.has("s"));
        assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn 验证码交回调且认证最终失败作废已发送密码_后台等待不作废() {
    for interactive in [false, true] {
        let f = Fixture::new(AuthMethod::Password);
        f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
        let error = f.run(interactive, |_, auth| {
            assert_eq!(
                (auth.answers)(challenge("Password:", false)).unwrap(),
                vec!["测试密码"]
            );
            let answer = (auth.answers)(challenge("Verification code:", false));
            assert_eq!(answer, interactive.then(|| vec!["123456".to_owned()]));
            Err(if interactive {
                Error::Authentication
            } else {
                Error::Callback
            })
        });
        assert_eq!(
            error.code,
            if interactive {
                "ssh.auth_failed"
            } else {
                "ssh.two_factor_required"
            }
        );
        assert_eq!(f.vault.has("s"), !interactive);
        assert!(f.vault.seen_two_factor("s"));
        assert_eq!(
            error.params.contains_key("password_generation"),
            interactive
        );
        assert_eq!(
            f.callbacks.challenges.lock().unwrap().len(),
            usize::from(interactive)
        );
        assert_eq!(
            f.run(false, |_, _| panic!("已知验证码后台不重试")).code,
            "ssh.two_factor_required"
        );
    }
    // 带回显的提示不作为密码，密码只自动发送一次。
    let f = Fixture::new(AuthMethod::Password);
    f.run(true, |_, auth| {
        assert_eq!(
            (auth.answers)(challenge("Password:", true)).unwrap(),
            vec!["123456"]
        );
        assert_eq!(
            (auth.answers)(challenge("Password:", false)).unwrap(),
            vec!["测试密码"]
        );
        assert_eq!(
            (auth.answers)(challenge("Password:", false)).unwrap(),
            vec!["123456"]
        );
        Err(Error::Authentication)
    });
    assert_eq!(f.callbacks.challenges.lock().unwrap().len(), 2);
}

#[test]
fn 验证码先于密码时仍使用记住的密码() {
    keyboard_sequence(
        pierlink_ssh_engine::test_support::LoginMode::OtpAndPassword,
        1,
    );
}

#[test]
fn 空信息轮之后的密码提示仍使用记住的密码() {
    keyboard_sequence(
        pierlink_ssh_engine::test_support::LoginMode::InformationAndPassword,
        0,
    );
}

fn keyboard_sequence(mode: pierlink_ssh_engine::test_support::LoginMode, challenges: usize) {
    let f = Fixture::new(AuthMethod::Password);
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let engine = Engine::new().unwrap();
    let peer = pierlink_ssh_engine::test_support::LoginPeer::new(mode);
    let connection = f
        .manager
        .connect_with(f.request(true), options(), |opts, auth| {
            peer.connect(&engine, opts, auth)
        })
        .unwrap();
    connection.ping().unwrap();
    assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 0);
    assert_eq!(f.callbacks.challenges.lock().unwrap().len(), challenges);
    assert_eq!(f.vault.seen_two_factor("s"), challenges > 0);
    assert!(f.vault.has("s"));
}

#[test]
fn 密码错误后仍询问验证码的认证失败作废该代密码并保留新代() {
    use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer};
    for newer in [false, true] {
        let f = Fixture::new(AuthMethod::Password);
        let generation = f.vault.set("s", Zeroizing::new("错误密码".into())).unwrap();
        let engine = Engine::new().unwrap();
        let peer = LoginPeer::new(LoginMode::PasswordAndOtp);
        let error = f
            .manager
            .connect_with(f.request(true), options(), |opts, auth| {
                let result = peer.connect(&engine, opts, auth);
                if newer {
                    f.vault.set("s", Zeroizing::new("新密码".into())).unwrap();
                }
                result
            })
            .err()
            .unwrap();
        assert_eq!(error.code, "ssh.auth_failed");
        assert_eq!(error.params["password_generation"], generation.to_string());
        assert_eq!(f.vault.has("s"), newer);
        assert_eq!(f.callbacks.challenges.lock().unwrap().len(), 1);
        assert!(!format!("{error:?}").contains("错误密码"));
    }
}

#[test]
fn agent_默认密钥与密码回退共享密码库() {
    let f = Fixture::new(AuthMethod::Agent);
    let error = f.run(false, |_, auth| {
        assert!(auth.agent && auth.skip_unusable_keys);
        assert_eq!(auth.private_keys.len(), 3);
        assert!(auth.private_keys.iter().all(|key| key.passphrase.is_none()));
        assert_eq!(auth.private_keys[0].path.file_name().unwrap(), "id_ed25519");
        assert!((auth.password.unwrap())().is_none());
        Err(Error::Callback)
    });
    assert_eq!(error.code, "ssh.password_needed");
    f.run(true, |_, auth| {
        assert_eq!((auth.password.unwrap())().as_deref(), Some("测试密码"));
        Err(Error::Disconnected)
    });
    assert!(f.vault.has("s"));
    *f.callbacks.phrase.lock().unwrap() = Some("默认密钥口令".into());
    for _ in 0..2 {
        f.run(true, |_, auth| {
            assert_eq!(
                (auth.private_keys[0].passphrase.as_ref().unwrap())().as_deref(),
                Some("默认密钥口令")
            );
            Err(Error::Disconnected)
        });
    }
    assert_eq!(f.callbacks.phrases.load(Ordering::SeqCst), 2);
    assert!(f.manager.passphrases.lock().unwrap().is_empty());
}

#[test]
fn 指定密钥只用一把_口令按路径缓存_错误只试一次并清除() {
    let f = Fixture::new(AuthMethod::Key);
    *f.callbacks.phrase.lock().unwrap() = Some("密钥口令".into());
    for error in [Error::Disconnected, Error::PrivateKey, Error::Disconnected] {
        f.run(true, |_, auth| {
            assert!(!auth.agent && auth.password.is_none() && !auth.skip_unusable_keys);
            assert_eq!(auth.private_keys.len(), 1);
            assert_eq!(auth.private_keys[0].path, Path::new("指定私钥"));
            assert_eq!(
                (auth.private_keys[0].passphrase.as_ref().unwrap())().as_deref(),
                Some("密钥口令")
            );
            Err(error)
        });
    }
    assert_eq!(f.callbacks.phrases.load(Ordering::SeqCst), 2);
    f.manager.close_server("s").unwrap();
    f.manager.close_all().unwrap();
    assert!(f.manager.passphrases.lock().unwrap().is_empty());
}

#[test]
fn 专用密钥读取凭据_本机丢失沿用错误码() {
    let f = Fixture::new(AuthMethod::DedicatedKey);
    assert_eq!(
        f.run(false, |_, _| panic!("丢失密钥不能连接")).code,
        "keys.local_missing"
    );
    let root = crate::test_support::tempdir();
    let path = root.path().join("pierlink_ed25519");
    std::fs::write(&path, "测试占位").unwrap();
    *f.callbacks.dedicated.lock().unwrap() = Some((path.clone(), "专用口令".into()));
    f.run(false, |_, auth| {
        assert!(!auth.agent && auth.password.is_none());
        assert_eq!(auth.private_keys[0].path, path);
        assert_eq!(
            (auth.private_keys[0].passphrase.as_ref().unwrap())().as_deref(),
            Some("专用口令")
        );
        Err(Error::Disconnected)
    });
}

#[test]
fn 主机校验取消_连接参数与公共入口() {
    let f = Fixture::new(AuthMethod::Agent);
    let mut request = f.request(false);
    request.class = ConnectionClass::Bulk;
    let error = f
        .manager
        .connect_with(request, options(), |opts, _| {
            assert_eq!(opts.window_size, 2 * 1024 * 1024);
            let HostKeyCheck::Callback(check) = opts.host_key else {
                panic!("必须核对主机密钥")
            };
            assert!(check(HostKey {
                host: "memory".into(),
                port: 22,
                algorithm: "ssh-ed25519".into(),
                fingerprint: "SHA256:test".into(),
                public_key: String::new()
            }));
            f.cancelled.store(true, Ordering::Release);
            Err(Error::Authentication)
        })
        .err()
        .unwrap();
    assert_eq!(error.code, "ssh.auth_cancelled");
    let f = Fixture::new(AuthMethod::Password);
    assert_eq!(
        f.manager
            .get_connection(f.request(false))
            .err()
            .unwrap()
            .code,
        "ssh.password_needed"
    );
    assert_eq!(
        f.manager
            .connect(f.request(false), options())
            .err()
            .unwrap()
            .code,
        "ssh.password_needed"
    );
}

#[test]
fn 排队期间取消一秒内返回且不作废密码() {
    let f = Fixture::new(AuthMethod::Password);
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let gate = f.manager.gate("s");
    let _guard = gate.lock().unwrap();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let started = Instant::now();
            assert_eq!(
                f.run(false, |_, _| panic!("排队取消不能建连")).code,
                "ssh.auth_cancelled"
            );
            assert!(started.elapsed() < Duration::from_secs(1));
        });
        std::thread::sleep(Duration::from_millis(50));
        f.cancelled.store(true, Ordering::Release);
        worker.join().unwrap();
    });
    assert!(f.vault.has("s"));
}

#[test]
fn 认证途中连接断开报连不上且不作废记住的密码() {
    use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer};
    let engine = Engine::new().unwrap();
    let peer = LoginPeer::new(LoginMode::DisconnectOnPassword);
    let f = Fixture::new(AuthMethod::Password);
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let error = f
        .manager
        .connect_with(f.request(false), options(), |opts, auth| {
            peer.connect(&engine, opts, auth)
        })
        .err()
        .unwrap();
    assert_eq!(error.code, "ssh.unreachable");
    // 界面提示“登录过程中连接断开”，不是“检查地址和网络”
    assert_eq!(error.params["reason_key"], "disconnected_login");
    assert!(!error.params.contains_key("password_generation"));
    assert!(f.vault.has("s"));
    assert!(peer.attempts().contains(&"password"));
}

#[test]
fn 内存对端_密码与多轮验证_错误一次和后台行为() {
    use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer};
    let engine = Engine::new().unwrap();
    for mode in [
        LoginMode::Password,
        LoginMode::UnavailableKeyboard,
        LoginMode::KeyboardPassword,
        LoginMode::PasswordAndOtp,
    ] {
        let peer = LoginPeer::new(mode);
        let f = Fixture::new(AuthMethod::Password);
        let connection = f
            .manager
            .connect_with(f.request(true), options(), |opts, auth| {
                peer.connect(&engine, opts, auth)
            })
            .unwrap();
        connection.ping().unwrap();
        drop(connection);
        assert!(f.vault.has("s"));
        assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 1);
        if f.vault.seen_two_factor("s") {
            assert_eq!(f.callbacks.challenges.lock().unwrap().len(), 1);
        }
    }
    for mode in [LoginMode::Password, LoginMode::KeyboardPassword] {
        let peer = LoginPeer::new(mode);
        let f = Fixture::new(AuthMethod::Password);
        f.vault.set("s", Zeroizing::new("错误密码".into())).unwrap();
        let error = f
            .manager
            .connect_with(f.request(false), options(), |opts, auth| {
                peer.connect(&engine, opts, auth)
            })
            .err()
            .unwrap();
        assert_eq!(error.code, "ssh.auth_failed");
        assert!(!f.vault.has("s"));
        let attempts = peer.attempts();
        assert_eq!(
            attempts
                .iter()
                .filter(|method| **method == "password")
                .count()
                + usize::from(attempts.contains(&"keyboard-interactive")),
            1
        );
        assert_eq!(
            f.manager
                .connect_with(f.request(false), options(), |_, _| panic!(
                    "错误密码不能再试"
                ))
                .err()
                .unwrap()
                .code,
            "ssh.password_needed"
        );
    }
    let peer = LoginPeer::new(LoginMode::PasswordAndOtp);
    let f = Fixture::new(AuthMethod::Password);
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let error = f
        .manager
        .connect_with(f.request(false), options(), |opts, auth| {
            peer.connect(&engine, opts, auth)
        })
        .err()
        .unwrap();
    assert_eq!(error.code, "ssh.two_factor_required");
    assert!(f.vault.has("s") && f.vault.seen_two_factor("s"));
    assert!(!peer.attempts().contains(&"password"));
    let peer = LoginPeer::new(LoginMode::Password);
    let f = Fixture::new(AuthMethod::Agent);
    let connection = f
        .manager
        .connect_with(f.request(true), options(), |opts, auth| {
            let Authentication::Configured(mut auth) = auth else {
                panic!()
            };
            auth.private_keys.clear();
            peer.connect_with_agent(&engine, opts, Authentication::Configured(auth), &[])
        })
        .unwrap();
    connection.ping().unwrap();
    assert_eq!(
        peer.attempts()
            .iter()
            .filter(|method| **method == "password")
            .count(),
        1
    );
}

#[cfg(unix)]
#[test]
fn 内存对端_指定及专用密钥_口令错误一次_后台跳过加密默认密钥() {
    use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer};
    let root = crate::test_support::tempdir();
    let engine = Engine::new().unwrap();
    for encrypted in [false, true] {
        let path = root
            .path()
            .join(if encrypted { "encrypted" } else { "plain" });
        let phrase = if encrypted { "密钥口令" } else { "" };
        assert!(
            std::process::Command::new("/usr/bin/ssh-keygen")
                .args(["-q", "-t", "ed25519", "-a", "1", "-N", phrase, "-f"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let peer = LoginPeer::key(&path, encrypted.then_some(phrase)).unwrap();
        if !encrypted {
            let f = Fixture::new(AuthMethod::Agent);
            let rejected = root.path().join("rejected-agent-key");
            assert!(
                std::process::Command::new("/usr/bin/ssh-keygen")
                    .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                    .arg(&rejected)
                    .status()
                    .unwrap()
                    .success()
            );
            let connection = f
                .manager
                .connect_with(f.request(false), options(), |opts, auth| {
                    peer.connect_with_agent(&engine, opts, auth, &[rejected, path.clone()])
                })
                .unwrap();
            connection.ping().unwrap();
            assert_eq!(
                peer.attempts()
                    .iter()
                    .filter(|method| **method == "publickey")
                    .count(),
                2
            );
            assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 0);
        }
        for method in [AuthMethod::Key, AuthMethod::DedicatedKey] {
            let mut f = Fixture::new(method);
            f.server.identity_file = Some(path.to_str().unwrap().into());
            *f.callbacks.phrase.lock().unwrap() = Some(phrase.into());
            *f.callbacks.dedicated.lock().unwrap() = Some((path.clone(), phrase.into()));
            let connection = f
                .manager
                .connect_with(f.request(true), options(), |opts, auth| {
                    peer.connect(&engine, opts, auth)
                })
                .unwrap();
            connection.ping().unwrap();
            assert!(!peer.attempts().contains(&"password"));
            assert_eq!(
                f.callbacks.phrases.load(Ordering::SeqCst),
                usize::from(encrypted && method == AuthMethod::Key)
            );
        }
        if encrypted {
            let mut f = Fixture::new(AuthMethod::Key);
            f.server.identity_file = Some(path.to_str().unwrap().into());
            *f.callbacks.phrase.lock().unwrap() = Some("错误口令".into());
            let error = f
                .manager
                .connect_with(f.request(true), options(), |opts, auth| {
                    peer.connect(&engine, opts, auth)
                })
                .err()
                .unwrap();
            assert_eq!(error.code, "ssh.auth_failed");
            assert_eq!(f.callbacks.phrases.load(Ordering::SeqCst), 1);
            let f = Fixture::new(AuthMethod::Agent);
            let password_peer = LoginPeer::new(LoginMode::Password);
            f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
            let connection = f
                .manager
                .connect_with(f.request(false), options(), |opts, auth| {
                    let Authentication::Configured(mut auth) = auth else {
                        panic!()
                    };
                    // 固定默认文件来源，避免测试依赖或改变进程 HOME 与真实用户密钥。
                    auth.private_keys = vec![pierlink_ssh_engine::PrivateKeyFile {
                        path: path.clone(),
                        passphrase: None,
                    }];
                    password_peer.connect_with_agent(
                        &engine,
                        opts,
                        Authentication::Configured(auth),
                        &[],
                    )
                })
                .unwrap();
            connection.ping().unwrap();
            assert!(!password_peer.attempts().contains(&"publickey"));
            assert_eq!(f.callbacks.phrases.load(Ordering::SeqCst), 0);
        }
    }
}

struct WaitingCallbacks {
    stage: &'static str,
    entered: std::sync::mpsc::SyncSender<()>,
}

impl WaitingCallbacks {
    fn wait(&self, stage: &str, cancelled: &AtomicBool) {
        if self.stage == stage {
            self.entered.send(()).unwrap();
            while !cancelled.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

impl ConnectionCallbacks for WaitingCallbacks {
    fn check_host_key(
        &self,
        _: &str,
        _: &HostKey,
        cancelled: &AtomicBool,
    ) -> Result<bool, AppError> {
        self.wait("host", cancelled);
        Ok(true)
    }
    fn password(
        &self,
        _: &str,
        cancelled: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        self.wait("password", cancelled);
        Err(AppError::new("不应泄露的回调错误").with("reason", "秘密密码"))
    }
    fn passphrase(
        &self,
        _: &str,
        _: &Path,
        cancelled: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        self.wait("phrase", cancelled);
        Ok(None)
    }
    fn dedicated_key(
        &self,
        _: &str,
        cancelled: &AtomicBool,
    ) -> Result<Option<DedicatedCredential>, AppError> {
        self.wait("dedicated", cancelled);
        Err(AppError::new("不应泄露的回调错误").with("reason", "秘密口令"))
    }
    fn keyboard_interactive(
        &self,
        _: &str,
        _: Challenge,
        cancelled: &AtomicBool,
    ) -> Result<Option<Vec<Zeroizing<String>>>, AppError> {
        self.wait("otp", cancelled);
        Ok(None)
    }
}

#[cfg(unix)]
#[test]
fn 所有界面回调取消后均在一秒内返回且不作废密码() {
    use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer};
    let engine = Engine::new().unwrap();
    let root = crate::test_support::tempdir();
    let path = root.path().join("encrypted");
    assert!(
        std::process::Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-a", "1", "-N", "口令", "-f"])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    for (stage, method) in [
        ("host", AuthMethod::Password),
        ("password", AuthMethod::Password),
        ("phrase", AuthMethod::Key),
        ("dedicated", AuthMethod::DedicatedKey),
        ("otp", AuthMethod::Password),
    ] {
        let mut f = Fixture::new(method);
        f.server.identity_file = Some(path.to_str().unwrap().into());
        let had_password = stage != "password";
        if had_password {
            f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let callbacks = Arc::new(WaitingCallbacks { stage, entered: tx });
        let peer = if stage == "phrase" {
            LoginPeer::key(&path, Some("口令")).unwrap()
        } else {
            LoginPeer::new(LoginMode::PasswordAndOtp)
        };
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let mut request = f.request(true);
                request.callbacks = callbacks;
                f.manager.connect_with(request, options(), |opts, auth| {
                    peer.connect(&engine, opts, auth)
                })
            });
            rx.recv_timeout(Duration::from_secs(3)).unwrap();
            let started = Instant::now();
            f.cancelled.store(true, Ordering::Release);
            assert_eq!(
                worker.join().unwrap().err().unwrap().code,
                "ssh.auth_cancelled"
            );
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "取消回调超时：{stage}"
            );
        });
        assert_eq!(f.vault.has("s"), had_password);
    }
}

#[path = "../../../ssh-engine/tests/support/totp.rs"]
mod totp;

#[test]
fn 环境服务器密码与密码加验证码() {
    for prefix in ["PW", "OTP"] {
        let names = if prefix == "PW" {
            vec![
                "PIERLINK_ENGINE_SSHD",
                "PIERLINK_ENGINE_PW_USER",
                "PIERLINK_ENGINE_PW",
            ]
        } else {
            vec![
                "PIERLINK_ENGINE_SSHD",
                "PIERLINK_ENGINE_OTP_USER",
                "PIERLINK_ENGINE_OTP_PW",
                "PIERLINK_ENGINE_OTP_SECRET",
            ]
        };
        let mut values = Vec::new();
        for name in &names {
            match std::env::var(name) {
                Ok(value) if !value.is_empty() => values.push(value),
                _ => {
                    eprintln!("跳过 {prefix} 认证测试：未设置 {name}");
                    break;
                }
            }
        }
        if values.len() != names.len() {
            continue;
        }
        let f = Fixture::new(AuthMethod::Password);
        *f.callbacks.password.lock().unwrap() = Some(values[2].clone());
        if prefix == "OTP" {
            *f.callbacks.otp_secret.lock().unwrap() = Some(values[3].clone());
        }
        let (host, port) = values[0].rsplit_once(':').expect("测试地址须为 host:port");
        let options = ConnectionOptions::new(
            host.trim_start_matches('[').trim_end_matches(']'),
            port.parse().unwrap(),
            &values[1],
            HostKeyCheck::Callback(Arc::new(|_| true)),
        );
        let connection = f.manager.connect(f.request(true), options).unwrap();
        connection.ping().unwrap();
        assert!(f.vault.has("s"));
        if prefix == "OTP" {
            assert!(f.vault.seen_two_factor("s"));
        }
    }
}

#[path = "managed_tests.rs"]
mod managed;

#[cfg(unix)]
#[path = "host_key_tests.rs"]
mod host_keys;

#[path = "background_tests.rs"]
mod background;

#[path = "terminal_tests.rs"]
mod terminal;

#[path = "sftp_tests.rs"]
mod sftp;

#[path = "tunnel_tests.rs"]
mod tunnel;

#[test]
fn 进程内生成的专用密钥通过内存对端登录() {
    use crate::security::{CredentialStore, MemoryCredentialStore};
    use pierlink_ssh_engine::test_support::LoginPeer;
    let root = crate::test_support::tempdir();
    let store = MemoryCredentialStore::default();
    let key = crate::keys::local::ensure_local_key(root.path(), "pierlink-test-abcd1234", &store)
        .unwrap();
    let phrase = store
        .get(crate::keys::local::CREDENTIAL_ID)
        .unwrap()
        .unwrap();
    let peer = LoginPeer::key(&key.private_path, Some(&phrase)).unwrap();
    let engine = Engine::new().unwrap();
    let f = Fixture::new(AuthMethod::DedicatedKey);
    *f.callbacks.dedicated.lock().unwrap() = Some((key.private_path, phrase));
    let connection = f
        .manager
        .connect_with(f.request(false), options(), |opts, auth| {
            peer.connect(&engine, opts, auth)
        })
        .unwrap();
    connection.ping().unwrap();
    assert!(peer.attempts().contains(&"publickey"));
    assert!(!peer.attempts().contains(&"password"));
    assert_eq!(f.callbacks.phrases.load(Ordering::SeqCst), 0);
}

#[test]
fn 转换字段用于身份列表和主机密钥查询但不改变连接地址() {
    let mut f = Fixture::new(AuthMethod::Agent);
    f.server.identity_files = vec!["/first key".into(), "none".into(), "/second key".into()];
    f.server.identities_only = true;
    f.server.host_key_alias = Some("known-alias".into());
    f.run(false, |opts, auth| {
        assert!(auth.agent);
        assert!(auth.agent_identities_only);
        assert_eq!(
            auth.private_keys
                .iter()
                .map(|k| k.path.as_path())
                .collect::<Vec<_>>(),
            [Path::new("/first key"), Path::new("/second key")]
        );
        assert_eq!(opts.host, "memory");
        let HostKeyCheck::Callback(check) = opts.host_key else {
            panic!("缺少主机密钥检查")
        };
        assert!(check(HostKey {
            host: "memory".into(),
            port: 2222,
            algorithm: "ssh-ed25519".into(),
            fingerprint: "SHA256:test".into(),
            public_key: String::new()
        }));
        let keys = f.callbacks.host_keys.lock().unwrap();
        assert_eq!(keys[0].host, "known-alias");
        assert_eq!(keys[0].port, 22);
        Err(Error::Disconnected)
    });
}

#[test]
fn 转换的身份限制只允许匹配文件的代理密钥且加密私钥无需口令() {
    use crate::security::{CredentialStore, MemoryCredentialStore};
    use pierlink_ssh_engine::test_support::LoginPeer;
    let root = crate::test_support::tempdir();
    let store = MemoryCredentialStore::default();
    let key = crate::keys::local::ensure_local_key(
        &root.path().join("allowed"),
        "pierlink-test-abcd1234",
        &store,
    )
    .unwrap();
    let phrase = store
        .get(crate::keys::local::CREDENTIAL_ID)
        .unwrap()
        .unwrap();
    let peer = LoginPeer::key(&key.private_path, Some(&phrase)).unwrap();
    // 内存代理夹具加载无口令私钥；应用读取的身份文件始终为原来的加密文件。
    let decrypted = ssh_key::PrivateKey::read_openssh_file(&key.private_path)
        .unwrap()
        .decrypt(&phrase)
        .unwrap();
    let agent_key = root.path().join("agent-key");
    decrypted
        .write_openssh_file(&agent_key, ssh_key::LineEnding::LF)
        .unwrap();
    let rejected_key = root.path().join("other-key");
    ssh_key::PrivateKey::random(&mut rand::rng(), ssh_key::Algorithm::Ed25519)
        .unwrap()
        .write_openssh_file(&rejected_key, ssh_key::LineEnding::LF)
        .unwrap();
    let mut f = Fixture::new(AuthMethod::Agent);
    f.server.identity_files = vec![key.private_path.to_str().unwrap().into()];
    f.server.identities_only = true;
    let engine = Engine::new().unwrap();
    // 不提供 .pub，仍应从加密文件的公开头部识别允许的身份。
    std::fs::rename(
        key.private_path.with_extension("pub"),
        root.path().join("saved.pub"),
    )
    .unwrap();
    let connection = f
        .manager
        .connect_with(f.request(false), options(), |opts, auth| {
            peer.connect_with_agent(&engine, opts, auth, &[rejected_key, agent_key])
        })
        .unwrap();
    connection.ping().unwrap();
    assert_eq!(
        peer.attempts()
            .iter()
            .filter(|m| **m == "publickey")
            .count(),
        1
    );
    assert_eq!(f.callbacks.phrases.load(Ordering::SeqCst), 0);
}

#[test]
fn 登录次数上限不会作废已取出或尚未发送的密码() {
    for sent in [false, true] {
        let f = Fixture::new(AuthMethod::Agent);
        f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
        let error = f.run(false, |_, auth| {
            if sent {
                assert_eq!((auth.password.unwrap())().unwrap(), "测试密码");
            }
            Err(Error::TooManyAuthAttempts)
        });
        assert_eq!(error.code, "ssh.too_many_auth_attempts");
        assert!(!error.params.contains_key("password_generation"));
        assert!(f.vault.has("s"));
        assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 0);
    }
}
