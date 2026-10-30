//! 后台分派、限额、密码恢复与采样取消的内存协议测试。
use super::*;
use crate::{
    monitor::{Monitor, MonitorEvent},
    process::SshProgram,
    remote::{
        command::background,
        run::{RunLimits, run_script},
    },
};
use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer, ScriptReply};
use std::sync::mpsc;

fn program() -> SshProgram {
    SshProgram {
        path: "/不存在的ssh程序".into(),
    }
}
fn command(f: &Fixture) -> crate::ssh::args::SshCommand {
    background(&f.manager, f.request(false), || {
        panic!("内置模式不能构造 askpass 或 OpenSSH 参数")
    })
    .unwrap()
}
fn peer(
    f: &Fixture,
    mode: LoginMode,
    handler: impl Fn(&[u8]) -> ScriptReply + Send + Sync + 'static,
) -> LoginPeer {
    let peer = LoginPeer::new(mode).with_scripts(handler);
    attach(f, peer)
}
fn attach(f: &Fixture, peer: LoginPeer) -> LoginPeer {
    let copy = peer.clone();
    let engine = Engine::new().unwrap();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
        copy.connect(&engine, options, auth)
    }));
    peer
}
fn reply(script: &[u8], value: &str) -> ScriptReply {
    let script = std::str::from_utf8(script).unwrap();
    assert!(script.contains(") </dev/null"));
    let marker = |prefix: &str| {
        let start = script.find(prefix).unwrap();
        script[start..].split('\'').next().unwrap()
    };
    ScriptReply {
        stdout: format!(
            "启动噪声\n{}\n{value}\n{}\n尾部噪声",
            marker("__PIERLINK_BEGIN_"),
            marker("__PIERLINK_END_")
        )
        .into_bytes(),
        stderr: Vec::new(),
        exit_code: Some(0),
    }
}

#[test]
fn 内置部署分块上传复用批量连接_取消续传与失败重试() {
    use crate::deploy::{Arch, set_computer_short_id, upload_binary};
    let mut f = Fixture::new(AuthMethod::Password);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    set_computer_short_id("test0001").unwrap();
    let root = crate::test_support::tempdir();
    let path = root.path().join("upload.gz");
    let total = 2 * 1024 * 1024 + 17;
    std::fs::write(&path, vec![0x5a; total]).unwrap();
    let offset = Arc::new(AtomicUsize::new(0));
    let uploaded = offset.clone();
    let fail = Arc::new(AtomicBool::new(false));
    let failure = fail.clone();
    let peer = LoginPeer::new(LoginMode::Password).with_scripts(move |script| {
        let text = std::str::from_utf8(script).unwrap();
        if text.contains("gzip -dc") {
            assert_eq!(uploaded.load(Ordering::SeqCst), total);
            return reply(script, r#"{"path":"/home/test/syncthing"}"#);
        }
        if text.contains("base64 -d >>") {
            let data = text
                .split("<<'PIERLINK_DATA'\n")
                .nth(1)
                .unwrap()
                .split('\n')
                .next()
                .unwrap();
            let length = data.len() / 4 * 3 - data.bytes().rev().take_while(|c| *c == b'=').count();
            uploaded.fetch_add(length, Ordering::SeqCst);
            if failure.swap(false, Ordering::SeqCst) {
                // 远端已写入，但应答丢失；下一次调用必须查询偏移，不能重复追加。
                return ScriptReply {
                    stdout: vec![],
                    stderr: vec![],
                    exit_code: Some(255),
                };
            }
            return reply(script, r#"{"ok":true}"#);
        }
        reply(
            script,
            &format!(
                r#"{{"complete":false,"size":{}}}"#,
                uploaded.load(Ordering::SeqCst)
            ),
        )
    });
    let connections = Arc::new(AtomicUsize::new(0));
    let count = connections.clone();
    let engine = Engine::new().unwrap();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
        let connection = peer.connect(&engine, options, auth)?;
        assert_eq!(connection.window_size(), 2 * 1024 * 1024);
        count.fetch_add(1, Ordering::SeqCst);
        Ok(connection)
    }));
    let cmd = command(&f);
    let cancel = AtomicBool::new(false);
    let progress = Mutex::new(Vec::new());
    let report = |n, size| {
        assert_eq!(size, total as u64);
        progress.lock().unwrap().push(n);
        if n >= 1024 * 1024 {
            cancel.store(true, Ordering::Release);
        }
    };
    assert_eq!(
        upload_binary(
            &program(),
            &cmd,
            &path,
            &"a".repeat(64),
            "2.1.7",
            Arch::X86_64,
            &report,
            &cancel
        )
        .unwrap_err()
        .code,
        "deploy.cancelled"
    );
    assert_eq!(offset.load(Ordering::SeqCst), 1024 * 1024);
    assert_eq!(*progress.lock().unwrap(), [0, 1024 * 1024]);
    cancel.store(false, Ordering::Release);
    fail.store(true, Ordering::Release);
    assert_eq!(
        upload_binary(
            &program(),
            &cmd,
            &path,
            &"a".repeat(64),
            "2.1.7",
            Arch::X86_64,
            &|_, _| {},
            &cancel
        )
        .unwrap_err()
        .code,
        "deploy.remote_failed"
    );
    assert_eq!(offset.load(Ordering::SeqCst), 2 * 1024 * 1024);
    let resumed = Mutex::new(Vec::new());
    assert_eq!(
        upload_binary(
            &program(),
            &cmd,
            &path,
            &"a".repeat(64),
            "2.1.7",
            Arch::X86_64,
            &|n, _| resumed.lock().unwrap().push(n),
            &cancel
        )
        .unwrap(),
        "/home/test/syncthing"
    );
    assert_eq!(resumed.lock().unwrap()[0], 2 * 1024 * 1024);
    assert_eq!(resumed.lock().unwrap().last().copied(), Some(total as u64));
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    assert_eq!(
        cmd.builtin.as_ref().unwrap().request().class,
        ConnectionClass::Main
    );
    assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 0);
}

#[test]
fn 后台按功能分派_密码从库读取_脚本经标准输入并保留帧隔离() {
    let mut f = Fixture::new(AuthMethod::Password);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    let peer = peer(&f, LoginMode::Password, |script| reply(script, "true"));
    let cmd = command(&f);
    assert!(cmd.askpass.is_none() && cmd.env.is_empty() && cmd.args.is_empty());
    assert_eq!(
        run_script(&program(), &cmd, "cat\nprintf true", RunLimits::default())
            .unwrap_err()
            .code,
        "ssh.password_needed"
    );
    assert!(peer.attempts().is_empty());
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    assert_eq!(
        run_script(&program(), &cmd, "cat\nprintf true", RunLimits::default()).unwrap(),
        true
    );
    assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 0);
    assert_eq!(
        program().spawn(&cmd).err().unwrap().code,
        "ssh.invalid_argument"
    );
    f.server.connection_engine = crate::storage::Engine::OpenSsh;
    let original = crate::ssh::args::SshCommand {
        args: vec!["原参数".into()],
        env: vec![],
        config_text: None,
        askpass: None,
        builtin: None,
    };
    assert_eq!(
        background(&f.manager, f.request(false), || Ok(original.clone())).unwrap(),
        original
    );
    f.server.connection_engine = crate::storage::Engine::Builtin;
    f.server.ssh_alias = Some("仍依赖别名".into());
    assert_eq!(
        background(&f.manager, f.request(false), || Ok(original.clone())).unwrap(),
        original
    );
}

#[test]
fn 内置脚本独立限额_超时_退出分类与密码脱敏() {
    for (case, expected) in [
        (0, "remote.output_too_large"),
        (1, "remote.output_too_large"),
        (2, "remote.timeout"),
        (3, "remote.script_failed"),
        (4, "ssh.auth_failed"),
        (5, "remote.invalid_utf8"),
        (6, "remote.frame_missing"),
    ] {
        let mut f = Fixture::new(AuthMethod::Password);
        f.server.connection_engine = crate::storage::Engine::Builtin;
        f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
        peer(&f, LoginMode::Password, move |_| ScriptReply {
            stdout: match case {
                0 => vec![b'x'; 65536],
                5 => vec![255],
                _ => Vec::new(),
            },
            stderr: match case {
                1 => vec![b'x'; 65536],
                3 | 4 => b"Permission denied (password)."
                    .iter()
                    .copied()
                    .chain("测试密码".as_bytes().iter().copied())
                    .collect(),
                _ => Vec::new(),
            },
            exit_code: match case {
                2 => None,
                3 => Some(7),
                4 => Some(255),
                _ => Some(0),
            },
        });
        let error = run_script(
            &program(),
            &command(&f),
            "printf true",
            RunLimits {
                stdout_bytes: 4096,
                stderr_bytes: 512,
                timeout: Duration::from_millis(if case == 2 { 200 } else { 5000 }),
            },
        )
        .unwrap_err();
        assert_eq!(error.code, expected, "用例 {case}");
        assert!(!format!("{error:?}").contains("测试密码"));
        if case <= 1 {
            assert_eq!(
                error.params["stream"],
                if case == 0 { "stdout" } else { "stderr" }
            );
        }
    }
}

#[test]
fn 采样等待密码后立即恢复_掉线重连_停止不关闭共享连接() {
    let mut f = Fixture::new(AuthMethod::Password);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    peer(&f, LoginMode::Password, |_| {
        ScriptReply {
        stdout: b"PLMON1 {\"at_ms\":1,\"uptime_s\":1,\"load\":[0,0,0],\"cpu_percent\":null,\"cpu_cores\":1,\"mem_total\":1,\"mem_available\":1,\"disks\":[],\"gpus\":[],\"gpu_error\":null,\"os_name\":null} PLMON1\n".to_vec(),
        stderr: Vec::new(), exit_code: None,
    }
    });
    let (tx, rx) = mpsc::channel();
    let monitor = Monitor::start(
        program(),
        command(&f),
        Duration::from_secs(15),
        Arc::new(move |event| {
            tx.send(event).unwrap();
        }),
    )
    .unwrap();
    let wait = |matches: &dyn Fn(&MonitorEvent) -> bool| {
        let start = Instant::now();
        loop {
            let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if matches(&event) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "未收到预期采样状态：{event:?}"
            );
        }
    };
    wait(&|event| matches!(event, MonitorEvent::WaitingPassword { .. }));
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    wait(&|event| matches!(event, MonitorEvent::Sample(_)));
    let connection = f.manager.get_connection(f.request(false)).unwrap();
    connection.disconnect().unwrap();
    monitor.network_restored();
    wait(&|event| matches!(event, MonitorEvent::Sample(_)));
    let connection = f.manager.get_connection(f.request(false)).unwrap();
    let start = Instant::now();
    monitor.stop();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(!connection.is_closed());
    wait(&|event| matches!(event, MonitorEvent::Stopped));
}

#[test]
fn 采样二次验证停止重试且不调用验证码回调() {
    let mut f = Fixture::new(AuthMethod::Password);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    peer(&f, LoginMode::PasswordAndOtp, |_| unreachable!());
    let (tx, rx) = mpsc::channel();
    let monitor = Monitor::start(
        program(),
        command(&f),
        Duration::from_secs(15),
        Arc::new(move |event| {
            tx.send(event).unwrap();
        }),
    )
    .unwrap();
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        MonitorEvent::Connecting
    ));
    assert!(
        matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), MonitorEvent::Failed { error } if error.code == "ssh.two_factor_required")
    );
    assert!(f.callbacks.challenges.lock().unwrap().is_empty());
    monitor.stop();
}

#[cfg(unix)]
#[test]
fn 内置密钥验证每次重新认证_已登录连接不能掩盖密钥被撤销() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("key");
    crate::builtin_test_support::sshd::keygen(&path, "ed25519", "");
    let mut f = Fixture::new(AuthMethod::Key);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    f.server.identity_file = Some(path.to_str().unwrap().into());
    let key_peer = LoginPeer::key(&path, None)
        .unwrap()
        .with_scripts(|script| reply(script, "{\"verified\":true}"));
    let key_peer = attach(&f, key_peer);
    let cmd = command(&f);
    run_script(&program(), &cmd, "printf true", RunLimits::default()).unwrap();
    let connection = f.manager.get_connection(f.request(false)).unwrap();
    crate::keys::remote::verify(&program(), &cmd).unwrap();
    assert_eq!(
        key_peer
            .attempts()
            .iter()
            .filter(|method| **method == "publickey")
            .count(),
        2
    );
    assert!(!connection.is_closed());
    // 服务端撤销公钥，但原连接仍可用；验证必须失败，不能回退到密码或验证码。
    let rejecting = peer(&f, LoginMode::Password, |_| unreachable!());
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    assert!(crate::keys::remote::verify(&program(), &cmd).is_err());
    assert!(!rejecting.attempts().contains(&"password"));
    assert!(!connection.is_closed());
    assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 0);
    assert!(f.callbacks.challenges.lock().unwrap().is_empty());
}

#[test]
fn 内置密码安装直接写库_失败一次后作废且不回显密码() {
    let mut f = Fixture::new(AuthMethod::Password);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    let peer = peer(&f, LoginMode::Password, |_| unreachable!());
    let cmd = command(&f);
    let public = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let error =
        crate::keys::remote::install_with_password(&program(), &cmd, &cmd, public, "错误密码")
            .unwrap_err();
    assert_eq!(error.code, "ssh.auth_failed");
    assert!(!format!("{error:?}").contains("错误密码"));
    assert!(!f.vault.has("s"));
    assert_eq!(
        peer.attempts()
            .iter()
            .filter(|method| **method == "password")
            .count(),
        1
    );
    assert_eq!(
        run_script(&program(), &cmd, "printf true", RunLimits::default())
            .unwrap_err()
            .code,
        "ssh.password_needed"
    );
    assert_eq!(
        peer.attempts()
            .iter()
            .filter(|method| **method == "password")
            .count(),
        1
    );
}

#[test]
fn 主动操作本次输入的密码也从脚本错误中移除() {
    let mut f = Fixture::new(AuthMethod::Password);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    peer(&f, LoginMode::Password, |_| ScriptReply {
        stdout: Vec::new(),
        stderr: "测试密码".as_bytes().to_vec(),
        exit_code: Some(7),
    });
    let cmd = background(&f.manager, f.request(true), || unreachable!()).unwrap();
    let error = run_script(&program(), &cmd, "printf true", RunLimits::default()).unwrap_err();
    assert_eq!(error.code, "remote.script_failed");
    assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 1);
    assert!(!format!("{error:?}").contains("测试密码"));
}

#[test]
fn 采样跳板密码被拒只等待跳板的新密码且不反复登录() {
    let mut f = Fixture::new(AuthMethod::Password);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    let mut jump = f.server.clone();
    jump.id = "jump".into();
    f.server.jump_server_id = Some(jump.id.clone());
    f.manager
        .set_server_resolver(Arc::new(move |id| Ok((id == "jump").then(|| jump.clone()))));
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    f.vault
        .set("jump", Zeroizing::new("错误密码".into()))
        .unwrap();
    let peer = peer(&f, LoginMode::Password, |_| ScriptReply {
        stdout: b"PLMON1 {\"unsupported\":\"no_proc\"} PLMON1\n".to_vec(),
        stderr: Vec::new(),
        exit_code: Some(0),
    });
    let (tx, rx) = mpsc::channel();
    let monitor = Monitor::start(
        program(),
        command(&f),
        Duration::from_secs(15),
        Arc::new(move |event| {
            tx.send(event).unwrap();
        }),
    )
    .unwrap();
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        MonitorEvent::Connecting
    ));
    let MonitorEvent::WaitingPassword { error } = rx.recv_timeout(Duration::from_secs(5)).unwrap()
    else {
        panic!("应等待跳板密码")
    };
    assert_eq!(error.params["server_id"], "jump");
    assert!(error.params.contains_key("password_generation"));
    assert!(f.vault.has("s") && !f.vault.has("jump"));
    assert!(rx.recv_timeout(Duration::from_millis(150)).is_err());
    assert_eq!(
        peer.attempts()
            .iter()
            .filter(|method| **method == "password")
            .count(),
        1
    );
    f.vault
        .set("jump", Zeroizing::new("测试密码".into()))
        .unwrap();
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        MonitorEvent::Connecting
    ));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        MonitorEvent::Unsupported { .. }
    ));
    monitor.stop();
}

#[test]
fn 采样登录上限直接失败且网络和密码更新不重试() {
    let mut f = Fixture::new(AuthMethod::Agent);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        Err(Error::TooManyAuthAttempts)
    }));
    let (tx, rx) = mpsc::channel();
    let monitor = Monitor::start(
        program(),
        command(&f),
        Duration::from_secs(15),
        Arc::new(move |event| {
            tx.send(event).unwrap();
        }),
    )
    .unwrap();
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        MonitorEvent::Connecting
    ));
    assert!(
        matches!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), MonitorEvent::Failed { error } if error.code == "ssh.too_many_auth_attempts")
    );
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        MonitorEvent::Stopped
    ));
    monitor.network_restored();
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    monitor.stop();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 0);
}
