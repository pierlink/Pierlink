use super::*;
use std::sync::atomic::AtomicUsize;
#[derive(Default)]
struct Handler {
    passwords: AtomicUsize,
    others: AtomicUsize,
    cancel: AtomicBool,
    blocked: Mutex<bool>,
    wake: Condvar,
}
impl AskHandler for Handler {
    fn ask_password(&self, _: &str, _: &str) -> Result<Option<Zeroizing<String>>, AppError> {
        self.passwords.fetch_add(1, Ordering::SeqCst);
        let lock = self.blocked.lock().unwrap();
        let _lock = self.wake.wait_while(lock, |blocked| *blocked).unwrap();
        Ok((!self.cancel.load(Ordering::SeqCst)).then(|| Zeroizing::new("测试密码".into())))
    }
    fn ask_other(
        &self,
        _: &str,
        _: &str,
        _: PromptKind,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        self.others.fetch_add(1, Ordering::SeqCst);
        Ok((!self.cancel.load(Ordering::SeqCst)).then(|| Zeroizing::new("验证码".into())))
    }
    fn cancel_all(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        *self.blocked.lock().unwrap() = false;
        self.wake.notify_all();
    }
}
fn fixture() -> (
    tempfile::TempDir,
    Arc<PasswordVault>,
    Arc<Handler>,
    AskpassServer,
) {
    let root = crate::test_support::tempdir();
    let vault = Arc::new(PasswordVault::default());
    let handler = Arc::new(Handler::default());
    let inner = Arc::new(Inner {
        endpoint: "memory".into(),
        vault: vault.clone(),
        handler: handler.clone(),
        bindings: Mutex::new(HashMap::new()),
        flights: Mutex::new(HashMap::new()),
        stopped: Arc::new(AtomicBool::new(false)),
    });
    let server = AskpassServer {
        inner,
        worker: None,
    };
    (root, vault, handler, server)
}
fn binding(server: &AskpassServer, interactive: bool) -> Arc<Registration> {
    server
        .register(RegistrationSpec {
            server_id: "服务器".into(),
            expected_prompt: "Alice@HOST".into(),
            interactive,
            purpose: "终端".into(),
            password_only: true,
        })
        .unwrap()
}
fn answer(
    guard: &Registration,
    prompt: &str,
    kind: PromptKind,
) -> Result<Zeroizing<String>, AppError> {
    memory_answer(&guard.inner, guard.nonce(), prompt, kind)
}
struct BytesStream {
    input: std::io::Cursor<Vec<u8>>,
    output: Vec<u8>,
}
impl Read for BytesStream {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(bytes)
    }
}
impl Write for BytesStream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.output.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct Loopback {
    inner: Arc<Inner>,
    outgoing: Vec<u8>,
    incoming: Option<std::io::Cursor<Vec<u8>>>,
}
impl Write for Loopback {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.outgoing.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Read for Loopback {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.incoming.is_none() {
            let mut stream = BytesStream {
                input: std::io::Cursor::new(std::mem::take(&mut self.outgoing)),
                output: Vec::new(),
            };
            let _ = handle_connection(&mut stream, &self.inner);
            self.incoming = Some(std::io::Cursor::new(stream.output));
        }
        self.incoming.as_mut().unwrap().read(bytes)
    }
}
fn memory_answer(
    inner: &Arc<Inner>,
    nonce: &str,
    prompt: &str,
    kind: PromptKind,
) -> Result<Zeroizing<String>, AppError> {
    exchange(
        &mut Loopback {
            inner: inner.clone(),
            outgoing: Vec::new(),
            incoming: None,
        },
        nonce,
        prompt,
        kind,
    )
}
#[test]
fn 消息长度有界且不接受未完成消息() {
    for bytes in [vec![b'x'; MAX_MESSAGE + 1], vec![b'x'; 5]] {
        assert!(read_line(&mut std::io::Cursor::new(bytes)).is_err());
    }
    assert!(write_json(&mut Vec::new(), &"秘密".repeat(MAX_MESSAGE)).is_err());
}
#[test]
fn s23_错_nonce_注销_错提示_版本_额外身份字段_全部拒绝() {
    let (_root, vault, handler, server) = fixture();
    vault
        .set("服务器", Zeroizing::new("库密码".into()))
        .unwrap();
    let guard = binding(&server, false);
    assert!(
        memory_answer(
            &guard.inner,
            "wrong",
            "Alice@host's password: ",
            PromptKind::Password
        )
        .is_err()
    );
    for prompt in [
        "alice@host's password: ",
        "Alice@jump's password: ",
        "Alice@host's password:  ",
        "Password: ",
    ] {
        assert!(answer(&guard, prompt, PromptKind::Password).is_err());
    }
    assert_eq!(
        guard.finish_error(AppError::new("ssh.auth_failed")).code,
        "ssh.two_factor_required"
    );
    assert!(vault.has("服务器"));
    assert_eq!(handler.passwords.load(Ordering::SeqCst), 0);
    for request in [
        serde_json::json!({"version":2,"nonce":guard.nonce(),"prompt":"Alice@host's password: ","kind":"password"}),
        serde_json::json!({"version":1,"nonce":guard.nonce(),"prompt":"Alice@host's password: ","kind":"password","server_id":"其他服务器"}),
    ] {
        let mut stream = Loopback {
            inner: guard.inner.clone(),
            outgoing: Vec::new(),
            incoming: None,
        };
        write_json(&mut stream, &request).unwrap();
        let response = read_line(&mut stream)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Response>(&bytes).ok());
        assert!(response.is_none_or(|response| response.answer.is_none()));
    }
    let nonce = guard.nonce().to_owned();
    let inner = guard.inner.clone();
    drop(guard);
    assert!(
        memory_answer(
            &inner,
            &nonce,
            "Alice@host's password: ",
            PromptKind::Password
        )
        .is_err()
    );
}
#[test]
fn 库密码可用_只发送一次_认证失败只作废实际代数() {
    let (_root, vault, _handler, server) = fixture();
    let old = vault
        .set("服务器", Zeroizing::new("旧密码".into()))
        .unwrap();
    let template = binding(&server, false);
    let guard = template.prepare().unwrap();
    assert_ne!(guard.nonce(), template.nonce());
    assert_eq!(guard.expected_prompt(), "Alice@host");
    assert_eq!(
        &**answer(&guard, "(Alice@host) Password: ", PromptKind::Password).unwrap(),
        "旧密码"
    );
    assert_eq!(guard.status().used_generation, Some(old));
    assert!(answer(&guard, "(Alice@host) Password: ", PromptKind::Password).is_err());
    vault
        .set("服务器", Zeroizing::new("新密码".into()))
        .unwrap();
    let error = guard.finish_error(AppError::new("ssh.auth_failed").with("stderr_tail", "旧密码"));
    assert_eq!(error.code, "ssh.auth_failed");
    assert!(!format!("{error:?}").contains("旧密码"));
    assert!(vault.has("服务器"));
    let guard = template.prepare().unwrap();
    answer(&guard, "Alice@host's password: ", PromptKind::Password).unwrap();
    guard.finish_error(AppError::new("ssh.auth_failed"));
    assert!(!vault.has("服务器"));
    assert_eq!(template.prepare().unwrap_err().code, "ssh.password_needed");
}
#[test]
fn 非交互缺密码不启动_交互取消后仍缺密码() {
    let (_root, vault, handler, server) = fixture();
    assert_eq!(
        binding(&server, false).prepare().unwrap_err().code,
        "ssh.password_needed"
    );
    handler.cancel.store(true, Ordering::SeqCst);
    let guard = binding(&server, true);
    assert!(answer(&guard, "Alice@host's password: ", PromptKind::Password).is_err());
    assert!(guard.status().password_needed);
    assert_eq!(
        guard.finish_error(AppError::new("ssh.failed")).code,
        "ssh.password_needed"
    );
    assert!(!vault.has("服务器"));
    assert_eq!(handler.passwords.load(Ordering::SeqCst), 1);
}
#[test]
fn 二次验证标记_其他交互不入库_confirm_none_不自动给密码() {
    let (_root, vault, handler, server) = fixture();
    let guard = binding(&server, true);
    assert_eq!(
        &**answer(&guard, "Alice@host's password: ", PromptKind::Password).unwrap(),
        "测试密码"
    );
    for kind in [PromptKind::Password, PromptKind::Confirm, PromptKind::None] {
        assert_eq!(&**answer(&guard, "不可信提示原文", kind).unwrap(), "验证码");
    }
    assert!(vault.seen_two_factor("服务器"));
    assert_eq!(&**vault.get("服务器").unwrap().unwrap().secret, "测试密码");
    assert_eq!(
        binding(&server, false).prepare().unwrap_err().code,
        "ssh.two_factor_required"
    );
    assert_eq!(handler.others.load(Ordering::SeqCst), 3);
    vault.clear_all();
    let background = binding(&server, false);
    for kind in [PromptKind::Confirm, PromptKind::None] {
        assert!(answer(&background, "Alice@host's password: ", kind).is_err());
    }
    assert!(background.status().other_required);
}
#[test]
fn 默认密钥方式_后台缺密码照常启动_私钥口令不算二次验证() {
    let (_root, vault, handler, server) = fixture();
    let agent = server
        .register(RegistrationSpec {
            server_id: "服务器".into(),
            expected_prompt: "Alice@HOST".into(),
            interactive: false,
            purpose: "资源概览".into(),
            password_only: false,
        })
        .unwrap();
    // 密钥可能就能登录：库里没密码也允许启动
    let guard = agent.prepare().unwrap();
    // 加密私钥的口令：后台不弹框也不记成二次验证
    assert!(
        answer(
            &guard,
            "Enter passphrase for key 'C:\\Users\\a\\.ssh\\id_ed25519': ",
            PromptKind::Password
        )
        .is_err()
    );
    assert_eq!(guard.status(), RegistrationStatus::default());
    assert_eq!(handler.others.load(Ordering::SeqCst), 0);
    // 服务器真来要密码：拒绝并转成“需要密码”，交给调用方进入等待
    assert!(answer(&guard, "Alice@host's password: ", PromptKind::Password).is_err());
    assert_eq!(
        guard.finish_error(AppError::new("ssh.auth_failed")).code,
        "ssh.password_needed"
    );
    assert_eq!(handler.passwords.load(Ordering::SeqCst), 0);
    // 用过密码之后再来私钥口令提示，也不标记二次验证
    vault.set("服务器", Zeroizing::new("密码".into())).unwrap();
    let guard = agent.prepare().unwrap();
    answer(&guard, "Alice@host's password: ", PromptKind::Password).unwrap();
    let _ = answer(
        &guard,
        "Enter passphrase for key '/k': ",
        PromptKind::Password,
    );
    assert!(!vault.seen_two_factor("服务器"));
    // 交互连接（终端）里私钥口令照常弹框，不入库
    let terminal = binding(&server, true);
    assert_eq!(
        &**answer(
            &terminal,
            "Enter passphrase for key '/k': ",
            PromptKind::Password
        )
        .unwrap(),
        "验证码"
    );
    assert_eq!(handler.others.load(Ordering::SeqCst), 1);
    assert!(!vault.seen_two_factor("服务器"));
}
#[test]
fn 并发同服务器只弹一次且取消共享结果() {
    for cancel in [false, true] {
        let (_root, vault, handler, server) = fixture();
        *handler.blocked.lock().unwrap() = true;
        handler.cancel.store(cancel, Ordering::SeqCst);
        let mut workers = Vec::new();
        for _ in 0..6 {
            let guard = binding(&server, true);
            workers.push(thread::spawn(move || {
                answer(&guard, "Alice@host's password: ", PromptKind::Password).is_ok()
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let count = server
                .inner
                .flights
                .lock()
                .unwrap()
                .get("服务器")
                .map_or(0, Weak::strong_count);
            if count == 6 {
                break;
            }
            assert!(Instant::now() < deadline, "并发请求未全部进入等待");
            thread::sleep(Duration::from_millis(5));
        }
        *handler.blocked.lock().unwrap() = false;
        handler.wake.notify_all();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), !cancel);
        }
        assert_eq!(handler.passwords.load(Ordering::SeqCst), 1);
        assert_eq!(vault.has("服务器"), !cancel);
    }
}
#[cfg(unix)]
#[test]
#[ignore = "需要沙箱外运行：创建真实 Unix 套接字，验证权限和协议"]
fn 私有目录权限_服务退出清理_超长协议实际被拒绝() {
    use std::os::unix::fs::PermissionsExt;
    let (root, vault, handler, _server) = fixture();
    let server = AskpassServer::start(vault, handler, root.path()).unwrap();
    let guard = binding(&server, false);
    let path = std::path::PathBuf::from(guard.pipe());
    let parent = path.parent().unwrap().to_owned();
    assert_eq!(
        std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let mut stream = platform::connect(guard.pipe()).unwrap();
    let _ = stream.write_all(&[b'x'; MAX_MESSAGE + 1]);
    assert!(read_line(&mut stream).is_err());
    drop(stream);
    drop(server);
    assert!(!parent.exists());
    assert!(guard.prepare().is_err());
}

#[cfg(windows)]
#[test]
fn windows_真实管道只接受本程序_首次实例及答案往返() {
    let (root, vault, handler, _memory) = fixture();
    let server = AskpassServer::start(vault.clone(), handler, root.path()).unwrap();
    let guard = binding(&server, false);
    vault
        .set("服务器", Zeroizing::new("测试密码".into()))
        .unwrap();
    assert!(guard.pipe().starts_with(r"\\.\pipe\pierlink-askpass-"));
    let expected_len = r"\\.\pipe\pierlink-askpass-".len() + 32;
    assert_eq!(guard.pipe().len(), expected_len);
    assert_eq!(
        &**request_answer(
            guard.pipe(),
            guard.nonce(),
            "Alice@host's password: ",
            PromptKind::Password
        )
        .unwrap(),
        "测试密码"
    );
    let unconsumed = binding(&server, false);
    let copy = root.path().join("其他程序.exe");
    std::fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
    let output = std::process::Command::new(&copy)
        .args([
            "--exact",
            "ssh::askpass_pipe::tests::windows_不同exe客户端被拒绝",
        ])
        .env("PIERLINK_TEST_PIPE", guard.pipe())
        .env("PIERLINK_TEST_NONCE", unconsumed.nonce())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("测试密码"));
}
#[cfg(windows)]
#[test]
fn windows_不同exe客户端被拒绝() {
    let (Ok(pipe), Ok(nonce)) = (
        std::env::var("PIERLINK_TEST_PIPE"),
        std::env::var("PIERLINK_TEST_NONCE"),
    ) else {
        return;
    };
    assert!(
        request_answer(
            &pipe,
            &nonce,
            "Alice@host's password: ",
            PromptKind::Password
        )
        .is_err()
    );
}

#[test]
fn 客户端读取提示类别_仅成功向stdout写答案_拒绝及环境缺失返回一() {
    let (_root, _vault, handler, server) = fixture();
    let guard = binding(&server, true);
    for (kind, expected) in [
        ("", "测试密码\n"),
        ("confirm", "验证码\n"),
        ("none", "验证码\n"),
    ] {
        let mut output = Vec::new();
        let code = run_client(
            "Alice@host's password: ",
            |key| match key {
                "PIERLINK_ASKPASS_PIPE" => Some("memory".into()),
                "PIERLINK_ASKPASS_NONCE" => Some(guard.nonce().into()),
                "SSH_ASKPASS_PROMPT" => Some(kind.into()),
                _ => None,
            },
            &mut output,
            |_, nonce, prompt, kind| memory_answer(&guard.inner, nonce, prompt, kind),
        );
        assert_eq!(code, 0);
        assert_eq!(output, expected.as_bytes());
    }
    handler.cancel.store(true, Ordering::SeqCst);
    let mut output = Vec::new();
    assert_eq!(
        run_client(
            "验证码",
            |key| Some(if key == "PIERLINK_ASKPASS_NONCE" {
                guard.nonce().into()
            } else {
                "memory".into()
            }),
            &mut output,
            |_, nonce, prompt, kind| memory_answer(&guard.inner, nonce, prompt, kind)
        ),
        1
    );
    assert!(output.is_empty());
    assert_eq!(
        run_client(
            "提示",
            |_| None,
            &mut output,
            |_, _, _, _| panic!("环境缺失不应连接")
        ),
        1
    );
    assert!(output.is_empty());
}

#[test]
fn 登录上限保留密码且不被等待输入状态覆盖() {
    for sent in [false, true] {
        let (_root, vault, handler, server) = fixture();
        vault
            .set("服务器", Zeroizing::new("正确密码".into()))
            .unwrap();
        let guard = binding(&server, true);
        if sent {
            answer(&guard, "Alice@host's password: ", PromptKind::Password).unwrap();
        } else {
            handler.cancel.store(true, Ordering::SeqCst);
            guard.binding.status.lock().unwrap().password_needed = true;
        }
        let error = guard.finish_error(
            AppError::new("ssh.too_many_auth_attempts").with("stderr_tail", "不可信原文"),
        );
        assert_eq!(error.code, "ssh.too_many_auth_attempts");
        assert!(error.params.is_empty());
        assert!(vault.has("服务器"));
    }
}
