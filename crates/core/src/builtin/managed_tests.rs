//! 连接池与通用命令的协议级测试，不监听本机端口。
use super::*;
use std::{
    io::{Read, Write},
    sync::Barrier,
};

fn memory(f: &Fixture) -> Arc<AtomicUsize> {
    let engine = Engine::new().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, parent, options, auth| {
        count.fetch_add(1, Ordering::SeqCst);
        match parent {
            Some(parent) => engine.connect_via(parent, options, auth),
            None => pierlink_ssh_engine::test_support::connect_sessions(&engine, options, auth),
        }
    }));
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    calls
}

fn command(value: &str) -> ExecCommand {
    ExecCommand::Escaped(value.into())
}

pub(super) fn wait_for_waiter(f: &Fixture) {
    let slot = f.manager.pool.slot(&f.server.id);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !slot.has_waiter() {
        assert!(Instant::now() < deadline, "请求未进入共享登录等待");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn 交互请求等待后台缺密码或验证码后用自己的回调登录() {
    use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer};
    for two_factor in [false, true] {
        let f = Fixture::new(AuthMethod::Agent);
        let engine = Engine::new().unwrap();
        let peer = LoginPeer::new(if two_factor {
            LoginMode::PasswordAndOtp
        } else {
            LoginMode::Password
        });
        let (entered, started) = std::sync::mpsc::sync_channel(1);
        let (release, ready) = std::sync::mpsc::sync_channel(1);
        let ready = Mutex::new(ready);
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                entered.send(()).unwrap();
                ready
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                let Authentication::Configured(auth) = auth else {
                    panic!()
                };
                if two_factor {
                    assert!((auth.answers)(challenge("Verification code:", false)).is_none());
                } else {
                    assert!((auth.password.unwrap())().is_none());
                }
                Err(Error::Callback)
            } else {
                peer.connect_with_agent(&engine, options, auth, &[])
            }
        }));
        std::thread::scope(|scope| {
            let background = scope.spawn(|| f.manager.get_connection(f.request(false)));
            started.recv_timeout(Duration::from_secs(3)).unwrap();
            let interactive = scope.spawn(|| f.manager.get_connection(f.request(true)));
            wait_for_waiter(&f);
            release.send(()).unwrap();
            assert_eq!(
                background.join().unwrap().err().unwrap().code,
                if two_factor {
                    "ssh.two_factor_required"
                } else {
                    "ssh.password_needed"
                }
            );
            interactive.join().unwrap().unwrap().ping().unwrap();
        });
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 1);
        assert_eq!(
            f.callbacks.challenges.lock().unwrap().len(),
            usize::from(two_factor)
        );
        f.manager.close_all().unwrap();
    }
}

#[test]
fn 并发复用与主批量窗口隔离且批量背压不拖慢回显() {
    let f = Fixture::new(AuthMethod::Password);
    let calls = memory(&f);
    let barrier = Barrier::new(10);
    let connections = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..10)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    f.manager.get_connection(f.request(false)).unwrap()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        connections
            .iter()
            .all(|c| c.same_connection(&connections[0]))
    );
    assert_eq!(connections[0].window_size(), 256 * 1024);
    let mut request = f.request(false);
    request.class = ConnectionClass::Bulk;
    let bulk = f.manager.get_connection(request).unwrap();
    assert_eq!(bulk.window_size(), 2 * 1024 * 1024);
    assert!(!bulk.same_connection(&connections[0]));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let mut transfer = bulk.exec("mixed-flood", &[]).unwrap();
    let mut echo = connections[0]
        .shell(&pierlink_ssh_engine::Pty::default(), &[])
        .unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| std::io::copy(&mut transfer.stdout, &mut std::io::sink()).unwrap());
        scope.spawn(|| std::io::copy(&mut transfer.stderr, &mut std::io::sink()).unwrap());
        for _ in 0..8 {
            let start = Instant::now();
            echo.stdin.write_all(b"echo").unwrap();
            let mut value = [0; 4];
            echo.stdout.read_exact(&mut value).unwrap();
            assert_eq!(&value, b"echo");
            assert!(
                start.elapsed() < Duration::from_millis(100),
                "批量传输期间回显超过一百毫秒"
            );
        }
    });
    assert_eq!(transfer.wait().unwrap(), Some(0));
    f.manager.close_all().unwrap();
    assert!(connections.iter().all(Connection::is_closed));
    assert!(f.manager.get_connection(f.request(false)).is_err());
}

#[test]
fn 会话上限自动扩容且新连接仍遵循流量类别() {
    let f = Fixture::new(AuthMethod::Password);
    let calls = memory(&f);
    let mut sessions = Vec::new();
    for _ in 0..5 {
        sessions.push(
            f.manager
                .shell(f.request(false), &pierlink_ssh_engine::Pty::default(), &[])
                .unwrap(),
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let main = f.manager.get_connection(f.request(false)).unwrap();
    assert_eq!(main.window_size(), 256 * 1024);
    let mut request = f.request(false);
    request.class = ConnectionClass::Bulk;
    let bulk = f
        .manager
        .open_session(request, |c| {
            c.shell(&pierlink_ssh_engine::Pty::default(), &[])
        })
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    drop(bulk);
    drop(sessions);
    f.manager.close_all().unwrap();
}

#[test]
fn 已知验证码只保留一条大窗口连接且会话满不再次登录() {
    use pierlink_ssh_engine::{
        ChannelFailure,
        test_support::{LoginMode, LoginPeer},
    };
    let f = Fixture::new(AuthMethod::Password);
    f.vault.mark_two_factor("s");
    let engine = Engine::new().unwrap();
    let peer = LoginPeer::new(LoginMode::PasswordAndOtp);
    let connected = peer.clone();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
        connected.connect(&engine, options, auth)
    }));
    let main = f.manager.get_connection(f.request(true)).unwrap();
    assert_eq!(main.window_size(), 2 * 1024 * 1024);
    let mut request = f.request(false);
    request.class = ConnectionClass::Bulk;
    let bulk = f.manager.get_connection(request).unwrap();
    assert!(main.same_connection(&bulk));
    assert_eq!(f.callbacks.challenges.lock().unwrap().len(), 1);
    let attempts = peer.attempts().len();
    let error = f
        .manager
        .open_session::<()>(f.request(false), |_| {
            Err(Error::ChannelRejected(ChannelFailure::ConnectFailed))
        })
        .unwrap_err();
    assert_eq!(error.code, "ssh.session_open_failed");
    assert_eq!(peer.attempts().len(), attempts);
    f.manager.close_all().unwrap();
}

#[test]
fn 并发错误只尝试一次密码且所有等待者收到同一次失败() {
    let f = Fixture::new(AuthMethod::Password);
    f.vault.set("s", Zeroizing::new("错误密码".into())).unwrap();
    let engine = Engine::new().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
        count.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(150));
        pierlink_ssh_engine::test_support::connect_sessions(&engine, options, auth)
    }));
    let barrier = Barrier::new(8);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    assert_eq!(
                        f.manager
                            .get_connection(f.request(false))
                            .err()
                            .unwrap()
                            .code,
                        "ssh.auth_failed"
                    );
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!f.vault.has("s"));
    assert_eq!(
        f.manager
            .get_connection(f.request(false))
            .err()
            .unwrap()
            .code,
        "ssh.password_needed"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn 后台等待密码后自动连接_断线退避_网络恢复立即重试且终端不重连() {
    let f = Fixture::new(AuthMethod::Password);
    let calls = memory(&f);
    f.vault.forget("s");
    let states = f.manager.subscribe_states();
    let background = f.manager.background(f.request(true)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(event) = states.wait(Duration::from_millis(100))
            && event.state == ConnectionState::WaitingPassword
        {
            break;
        }
        assert!(Instant::now() < deadline);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(background.connection().is_none());
    assert!(
        background
            .wait_connected(Duration::from_millis(10))
            .is_err()
    );
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let old = background.wait_connected(Duration::from_secs(3)).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let mut terminal = old
        .shell(&pierlink_ssh_engine::Pty::default(), &[])
        .unwrap();
    old.abort();
    let start = Instant::now();
    assert!(terminal.stdout.read(&mut [0]).is_err());
    let new = background.wait_connected(Duration::from_secs(4)).unwrap();
    assert!(!new.same_connection(&old));
    assert!(start.elapsed() >= Duration::from_millis(900));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(terminal.stdout.read(&mut [0]).is_err());
    new.abort();
    f.manager.network_restored();
    let start = Instant::now();
    let last = background.wait_connected(Duration::from_secs(2)).unwrap();
    assert!(!last.same_connection(&new));
    assert!(start.elapsed() < Duration::from_secs(1));
    f.manager.close_server("s").unwrap();
    assert!(last.is_closed());
    assert!(
        background
            .wait_connected(Duration::from_millis(100))
            .is_err()
    );
    drop(background);
}

#[test]
fn 后台验证码等待不会重复认证_交互登录完成后自动复用() {
    use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer};
    let f = Fixture::new(AuthMethod::Password);
    let peer = LoginPeer::new(LoginMode::PasswordAndOtp);
    let engine = Engine::new().unwrap();
    let connected = peer.clone();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
        connected.connect(&engine, options, auth)
    }));
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let background = f.manager.background(f.request(false)).unwrap();
    assert_eq!(
        background
            .wait_connected(Duration::from_millis(500))
            .err()
            .unwrap()
            .code,
        "ssh.two_factor_required"
    );
    let attempts = peer.attempts().len();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(peer.attempts().len(), attempts);
    let connection = f.manager.get_connection(f.request(true)).unwrap();
    assert_eq!(connection.window_size(), 2 * 1024 * 1024);
    let resumed = background.wait_connected(Duration::from_secs(2)).unwrap();
    assert!(connection.same_connection(&resumed));
    f.manager.close_all().unwrap();
}

#[test]
fn 三级跳板各自核对主机与凭据且共用入口_缺失环路与超深拒绝() {
    let mut f = Fixture::new(AuthMethod::Password);
    let calls = memory(&f);
    let mut first = f.server.clone();
    first.id = "jump1".into();
    first.host = Some("hop1".into());
    let mut second = first.clone();
    second.id = "jump2".into();
    second.host = Some("hop2".into());
    second.jump_server_id = Some(first.id.clone());
    f.server.host = Some("hop3".into());
    f.server.jump_server_id = Some(second.id.clone());
    for server in [&first, &second] {
        f.vault
            .set(&server.id, Zeroizing::new("测试密码".into()))
            .unwrap();
    }
    let records = Arc::new(Mutex::new(HashMap::from([
        (first.id.clone(), first.clone()),
        (second.id.clone(), second.clone()),
    ])));
    let read = records.clone();
    f.manager.set_server_resolver(Arc::new(move |id| {
        Ok(read.lock().unwrap().get(id).cloned())
    }));
    let keys = Arc::new(Mutex::new(Vec::new()));
    let callbacks = Arc::new(HostCallbacks {
        keys: keys.clone(),
        base: f.callbacks.clone(),
        denied: None,
    });
    let mut request = f.request(false);
    request.callbacks = callbacks.clone();
    let connection = f.manager.get_connection(request).unwrap();
    assert_eq!(
        keys.lock()
            .unwrap()
            .iter()
            .map(|(id, key): &(String, HostKey)| (id.as_str(), key.host.as_str()))
            .collect::<Vec<_>>(),
        vec![("jump1", "hop1"), ("jump2", "hop2"), ("s", "hop3")]
    );
    let seen = keys.lock().unwrap();
    assert_ne!(seen[0].1.fingerprint, seen[1].1.fingerprint);
    assert_ne!(seen[1].1.fingerprint, seen[2].1.fingerprint);
    drop(seen);
    let mut other = f.server.clone();
    other.id = "other".into();
    other.host = Some("hop4".into());
    f.vault
        .set("other", Zeroizing::new("测试密码".into()))
        .unwrap();
    let mut request = f.request(false);
    request.server = &other;
    request.callbacks = callbacks;
    f.manager.get_connection(request).unwrap().ping().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    records
        .lock()
        .unwrap()
        .get_mut("jump1")
        .unwrap()
        .jump_server_id = Some("s".into());
    records.lock().unwrap().insert("s".into(), f.server.clone());
    assert_eq!(
        f.manager
            .get_connection(f.request(false))
            .err()
            .unwrap()
            .code,
        "ssh.jump_cycle"
    );
    records.lock().unwrap().remove("jump1");
    assert_eq!(
        f.manager
            .get_connection(f.request(false))
            .err()
            .unwrap()
            .code,
        "ssh.jump_not_found"
    );
    f.manager.close_server("jump2").unwrap();
    assert!(connection.is_closed());
    f.manager.close_all().unwrap();
    let mut records = HashMap::new();
    for n in 0..5 {
        let mut server = first.clone();
        server.id = n.to_string();
        server.jump_server_id = (n < 4).then(|| (n + 1).to_string());
        records.insert(server.id.clone(), server);
    }
    let target = records["0"].clone();
    let manager = ConnectionManager::default();
    manager.set_server_resolver(Arc::new(move |id| Ok(records.get(id).cloned())));
    assert_eq!(
        manager.chain(&target).unwrap_err().code,
        "ssh.jump_too_deep"
    );
}

struct HostCallbacks {
    keys: Arc<Mutex<Vec<(String, HostKey)>>>,
    base: Arc<Callbacks>,
    denied: Option<&'static str>,
}
impl ConnectionCallbacks for HostCallbacks {
    fn check_host_key(&self, id: &str, key: &HostKey, _: &AtomicBool) -> Result<bool, AppError> {
        self.keys.lock().unwrap().push((id.into(), key.clone()));
        if self.denied == Some(id) {
            return Err(AppError::new("ssh.host_key_changed"));
        }
        Ok(true)
    }
    fn password(
        &self,
        id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        self.base.password(id, cancelled)
    }
    fn passphrase(
        &self,
        id: &str,
        path: &Path,
        cancelled: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        self.base.passphrase(id, path, cancelled)
    }
    fn dedicated_key(
        &self,
        id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Option<DedicatedCredential>, AppError> {
        self.base.dedicated_key(id, cancelled)
    }
    fn keyboard_interactive(
        &self,
        id: &str,
        challenge: Challenge,
        cancelled: &AtomicBool,
    ) -> Result<Option<Vec<Zeroizing<String>>>, AppError> {
        self.base.keyboard_interactive(id, challenge, cancelled)
    }
}

#[test]
fn 命令同时读取双输出_退出码_信号_截断与流式回调() {
    let f = Fixture::new(AuthMethod::Password);
    memory(&f);
    let output = f
        .manager
        .exec(f.request(false), command("output"), ExecOptions::default())
        .unwrap();
    assert_eq!(output.stdout, b"stdout");
    assert_eq!(output.stderr, b"stderr");
    assert_eq!(output.exit_code, Some(7));
    assert!(!output.timed_out && !output.cancelled && !output.truncated);
    let output = f
        .manager
        .exec(f.request(false), command("signal"), ExecOptions::default())
        .unwrap();
    assert_eq!(output.signal.as_deref(), Some("TERM"));
    assert_eq!(output.exit_code, None);
    let output = f
        .manager
        .exec(
            f.request(false),
            command("stderr-first"),
            ExecOptions {
                output_limit: 12000,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(output.stdout.len() + output.stderr.len(), 12000);
    assert_eq!(output.exit_code, Some(0));
    assert!(output.truncated);
    let mut seen = Vec::new();
    let output = f
        .manager
        .exec_streaming(
            f.request(false),
            command("output"),
            ExecOptions::default(),
            |kind, bytes| {
                seen.push((kind, bytes.to_vec()));
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen.contains(&(OutputStream::Stdout, b"stdout".to_vec())));
    assert!(seen.contains(&(OutputStream::Stderr, b"stderr".to_vec())));
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    assert_eq!(output.exit_code, Some(7));
    let error = f
        .manager
        .exec_streaming(
            f.request(false),
            command("output"),
            ExecOptions::default(),
            |_, _| Err(AppError::new("秘密")),
        )
        .unwrap_err();
    assert!(!format!("{error:?}").contains("秘密"));
    f.manager.close_all().unwrap();
}

fn burst_reply(input: &[u8]) -> pierlink_ssh_engine::test_support::ScriptReply {
    let seed = input[0] as usize;
    // 两种总量分别低于和超过主连接窗口，内容随命令及字节位置变化以检查尾部和串流。
    let size = if seed.is_multiple_of(2) { 80 } else { 160 } * 1024;
    pierlink_ssh_engine::test_support::ScriptReply {
        stdout: (0..size).map(|i| ((i + seed) % 251) as u8).collect(),
        stderr: (0..size + 173)
            .map(|i| ((i * 3 + seed + 1) % 251) as u8)
            .collect(),
        exit_code: Some((seed % 13 + 1) as u32),
    }
}

#[test]
fn 同一连接并发快速退出保留双输出内容和合计上限() {
    use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer};
    let f = Fixture::new(AuthMethod::Password);
    let engine = Engine::new().unwrap();
    let peer = LoginPeer::new(LoginMode::Password).with_scripts(burst_reply);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, parent, options, auth| {
        assert!(parent.is_none());
        count.fetch_add(1, Ordering::SeqCst);
        peer.connect(&engine, options, auth)
    }));
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let connection = f.manager.get_connection(f.request(false)).unwrap();
    for round in 0..6 {
        let barrier = Barrier::new(24);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..24)
                .map(|index| {
                    let f = &f;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let input = vec![(round * 24 + index) as u8];
                        let expected = burst_reply(&input);
                        let total = expected.stdout.len() + expected.stderr.len();
                        let limit = match (index / 2) % 4 {
                            0 => 12000,
                            1 => total + 4096,
                            2 => total,
                            _ => 0,
                        };
                        let options = ExecOptions {
                            stdin: input,
                            output_limit: limit,
                            ..Default::default()
                        };
                        let command = ExecCommand::Argv(vec!["/bin/sh".into(), "-s".into()]);
                        barrier.wait();
                        let (result, stdout, stderr) = if round % 2 == 0 {
                            let mut stdout = Vec::new();
                            let mut stderr = Vec::new();
                            let result = f
                                .manager
                                .exec_streaming(
                                    f.request(false),
                                    command,
                                    options,
                                    |kind, bytes| {
                                        match kind {
                                            OutputStream::Stdout => &mut stdout,
                                            OutputStream::Stderr => &mut stderr,
                                        }
                                        .extend_from_slice(bytes);
                                        Ok(())
                                    },
                                )
                                .unwrap();
                            assert!(result.stdout.is_empty() && result.stderr.is_empty());
                            (result, stdout, stderr)
                        } else {
                            let mut result =
                                f.manager.exec(f.request(false), command, options).unwrap();
                            let stdout = std::mem::take(&mut result.stdout);
                            let stderr = std::mem::take(&mut result.stderr);
                            (result, stdout, stderr)
                        };
                        assert!(!result.timed_out && !result.cancelled);
                        assert_eq!(result.exit_code, expected.exit_code);
                        assert_eq!(result.signal, None);
                        assert_eq!(result.truncated, total > limit);
                        assert_eq!(stdout.len() + stderr.len(), total.min(limit));
                        assert!(expected.stdout.starts_with(&stdout));
                        assert!(expected.stderr.starts_with(&stderr));
                        if total <= limit {
                            assert_eq!(stdout, expected.stdout);
                            assert_eq!(stderr, expected.stderr);
                        }
                    })
                })
                .collect();
            for worker in workers {
                worker.join().unwrap();
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(connection.same_connection(&f.manager.get_connection(f.request(false)).unwrap()));
        connection.ping().unwrap();
    }
    f.manager.close_all().unwrap();
}

#[test]
fn 命令超时_取消_阻塞输入_等待密码取消_连接仍可用() {
    let f = Fixture::new(AuthMethod::Password);
    memory(&f);
    for input in [Vec::new(), vec![0; 4 * 1024 * 1024]] {
        let start = Instant::now();
        let output = f
            .manager
            .exec(
                f.request(false),
                command("idle"),
                ExecOptions {
                    stdin: input,
                    timeout: Some(Duration::from_millis(100)),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(output.timed_out && !output.cancelled);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(60));
            cancelled.store(true, Ordering::Release);
        });
        let output = f
            .manager
            .exec(
                f.request(false),
                command("idle"),
                ExecOptions {
                    timeout: None,
                    cancelled: cancelled.clone(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(output.cancelled && !output.timed_out);
    });
    let output = f
        .manager
        .exec(f.request(false), command("output"), ExecOptions::default())
        .unwrap();
    assert_eq!(output.exit_code, Some(7));
    f.manager.close_all().unwrap();
    let f = Fixture::new(AuthMethod::Password);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let mut request = f.request(true);
    request.callbacks = Arc::new(WaitingCallbacks {
        stage: "password",
        entered: tx,
    });
    let cancelled = Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        let cancel = cancelled.clone();
        scope.spawn(move || {
            rx.recv_timeout(Duration::from_secs(2)).unwrap();
            cancel.store(true, Ordering::Release);
        });
        let output = f
            .manager
            .exec(
                request,
                command("output"),
                ExecOptions {
                    timeout: None,
                    cancelled: cancelled.clone(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(output.cancelled);
    });
}

#[test]
fn 关闭服务器取消主机确认等待且只影响该服务器() {
    let f = Fixture::new(AuthMethod::Password);
    memory(&f);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let callbacks = Arc::new(WaitingCallbacks {
        stage: "host",
        entered: tx,
    });
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let mut request = f.request(false);
            request.callbacks = callbacks;
            f.manager.get_connection(request)
        });
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let start = Instant::now();
        f.manager.close_server("s").unwrap();
        assert_eq!(
            worker.join().unwrap().err().unwrap().code,
            "ssh.auth_cancelled"
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    });
}

#[test]
fn 通道存活不回收_最后使用者释放五分钟后回收_状态订阅有界合并() {
    let f = Fixture::new(AuthMethod::Password);
    let calls = memory(&f);
    let connection = f.manager.get_connection(f.request(false)).unwrap();
    let shell = connection
        .shell(&pierlink_ssh_engine::Pty::default(), &[])
        .unwrap();
    drop(connection);
    let now = Instant::now();
    f.manager.pool.sweep(now + Duration::from_secs(600));
    let connection = f.manager.get_connection(f.request(false)).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(connection);
    drop(shell);
    f.manager.pool.sweep(now);
    f.manager.pool.sweep(now + Duration::from_secs(299));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    f.manager.pool.sweep(now + Duration::from_secs(301));
    f.manager.get_connection(f.request(false)).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let states = f.manager.subscribe_states();
    f.manager.publish("s", ConnectionState::Connecting, None);
    f.manager.publish("s", ConnectionState::Connected, None);
    assert_eq!(
        states.wait(Duration::ZERO).unwrap().state,
        ConnectionState::Connected
    );
    assert!(states.wait(Duration::ZERO).is_none());
    for n in 0..300 {
        let id = n.to_string();
        f.manager.pool.slot(&id);
        f.manager.publish(&id, ConnectionState::Connecting, None);
    }
    let mut received = 0;
    while states.wait(Duration::ZERO).is_some() {
        received += 1;
    }
    assert_eq!(received, 256);
    drop(states);
    f.manager.close_all().unwrap();
}

#[test]
fn 并发打开会话遇到其他请求抢满新连接时继续扩容_全拒绝时停止() {
    let f = Fixture::new(AuthMethod::Password);
    let calls = memory(&f);
    let barrier = Barrier::new(8);
    let sessions = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    f.manager
                        .shell(f.request(false), &pierlink_ssh_engine::Pty::default(), &[])
                        .unwrap()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(sessions.len(), 8);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    f.manager.close_all().unwrap();
    let f = Fixture::new(AuthMethod::Password);
    let calls = memory(&f);
    let error = f
        .manager
        .open_session::<()>(f.request(false), |_| {
            Err(Error::ChannelRejected(
                pierlink_ssh_engine::ChannelFailure::ConnectFailed,
            ))
        })
        .unwrap_err();
    assert_eq!(error.code, "ssh.session_open_failed");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn 关闭目标服务器也取消正在跳板等待的密码框() {
    let mut f = Fixture::new(AuthMethod::Password);
    let calls = memory(&f);
    let mut jump = f.server.clone();
    jump.id = "jump".into();
    f.server.jump_server_id = Some(jump.id.clone());
    f.manager
        .set_server_resolver(Arc::new(move |_| Ok(Some(jump.clone()))));
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let callbacks = Arc::new(WaitingCallbacks {
        stage: "password",
        entered: tx,
    });
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let mut request = f.request(true);
            request.callbacks = callbacks;
            f.manager.get_connection(request)
        });
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        f.manager.close_server("s").unwrap();
        assert_eq!(
            worker.join().unwrap().err().unwrap().code,
            "ssh.auth_cancelled"
        );
    });
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn 跳板指纹变化阻止下级连接且查询错误不泄露回调内容() {
    let mut f = Fixture::new(AuthMethod::Password);
    let calls = memory(&f);
    let mut jump = f.server.clone();
    jump.id = "jump".into();
    f.server.host = Some("hop2".into());
    f.server.jump_server_id = Some("jump".into());
    f.vault
        .set("jump", Zeroizing::new("测试密码".into()))
        .unwrap();
    f.manager
        .set_server_resolver(Arc::new(move |_| Ok(Some(jump.clone()))));
    let keys = Arc::new(Mutex::new(Vec::new()));
    let callbacks = Arc::new(HostCallbacks {
        keys: keys.clone(),
        base: f.callbacks.clone(),
        denied: Some("jump"),
    });
    let mut request = f.request(false);
    request.callbacks = callbacks;
    let states = f.manager.subscribe_states();
    assert_eq!(
        f.manager.get_connection(request).err().unwrap().code,
        "ssh.host_key_changed"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(keys.lock().unwrap().len(), 1);
    let mut events = Vec::new();
    while let Some(event) = states.wait(Duration::ZERO) {
        events.push(event);
    }
    assert!(
        events
            .iter()
            .any(|e| e.server_id == "s" && e.state == ConnectionState::Failed)
    );
    f.manager.set_server_resolver(Arc::new(|_| {
        Err(AppError::new("秘密口令").with("reason", "秘密私钥"))
    }));
    let error = f.manager.get_connection(f.request(false)).err().unwrap();
    assert_eq!(error.code, "ssh.jump_lookup_failed");
    assert!(!format!("{error:?}").contains("秘密"));
}

#[test]
fn 在别的运行时的线程里调用连接池的同步接口() {
    // 像 Tauri 命令那样：调用方有自己的 tokio 运行时，在它的线程上调用连接池（引擎曾经一律拒绝）
    use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer};
    let f = Fixture::new(AuthMethod::Password);
    let engine = Engine::new().unwrap();
    let peer = LoginPeer::new(LoginMode::Password).with_scripts(burst_reply);
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
        peer.connect(&engine, options, auth)
    }));
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let host = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let input = vec![7u8];
    let expected = burst_reply(&input);
    let result = host
        .block_on(async {
            tokio::task::block_in_place(|| {
                f.manager.exec(
                    f.request(false),
                    ExecCommand::Argv(vec!["/bin/sh".into(), "-s".into()]),
                    ExecOptions {
                        stdin: input.clone(),
                        ..Default::default()
                    },
                )
            })
        })
        .unwrap();
    assert_eq!(result.exit_code, expected.exit_code);
    assert_eq!(result.stdout, expected.stdout);
    let connection = host
        .block_on(async {
            tokio::task::block_in_place(|| f.manager.get_connection(f.request(false)))
        })
        .unwrap();
    connection.ping().unwrap();
}

#[test]
fn 登录上限只挡后台和自动请求且用户主动连接成功后后台恢复() {
    let f = Fixture::new(AuthMethod::Agent);
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        Err(Error::TooManyAuthAttempts)
    }));
    let events = f.manager.subscribe_states();
    let background = f.manager.background(f.request(false)).unwrap();
    assert_eq!(
        background
            .wait_connected(Duration::from_millis(500))
            .err()
            .unwrap()
            .code,
        "ssh.too_many_auth_attempts"
    );
    // 网络恢复、密码更新都会唤醒后台重试，但只拿记住的结果，不再去服务器登录
    f.manager.network_restored();
    f.vault.set("s", Zeroizing::new("更新密码".into())).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        f.manager
            .get_connection(f.request(false))
            .err()
            .unwrap()
            .code,
        "ssh.too_many_auth_attempts"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // 用户主动连接照常再试一次
    assert_eq!(
        f.manager
            .get_connection(f.request(true))
            .err()
            .unwrap()
            .code,
        "ssh.too_many_auth_attempts"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let states: Vec<_> = std::iter::from_fn(|| events.wait(Duration::ZERO)).collect();
    assert!(
        states
            .iter()
            .any(|event| event.state == ConnectionState::Failed)
    );
    assert!(!states.iter().any(|event| matches!(
        event.state,
        ConnectionState::WaitingPassword | ConnectionState::WaitingTwoFactor
    )));
    // 用户处理好之后（比如从 agent 移除多余的密钥）主动连接成功：清除上限，后台自动连上
    let calls = memory(&f);
    f.manager
        .get_connection(f.request(true))
        .unwrap()
        .ping()
        .unwrap();
    background
        .wait_connected(Duration::from_secs(3))
        .unwrap()
        .ping()
        .unwrap();
    f.manager
        .get_connection(f.request(false))
        .unwrap()
        .ping()
        .unwrap();
    assert!(calls.load(Ordering::SeqCst) >= 1);
    drop(background);
    f.manager.close_all().unwrap();
}

#[test]
fn 用户明确重试时清除登录上限且不影响别的服务器() {
    let f = Fixture::new(AuthMethod::Agent);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        Err(Error::TooManyAuthAttempts)
    }));
    for _ in 0..2 {
        assert_eq!(
            f.manager
                .get_connection(f.request(false))
                .err()
                .unwrap()
                .code,
            "ssh.too_many_auth_attempts"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    f.manager.clear_auth_limit("别的服务器");
    assert_eq!(
        f.manager
            .get_connection(f.request(false))
            .err()
            .unwrap()
            .code,
        "ssh.too_many_auth_attempts"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    f.manager.clear_auth_limit("s");
    let calls = memory(&f);
    f.manager
        .get_connection(f.request(false))
        .unwrap()
        .ping()
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    f.manager.close_all().unwrap();
}

#[test]
fn 登录上限在改了服务器设置关闭旧连接后清除() {
    let mut f = Fixture::new(AuthMethod::Agent);
    *f.manager.pool.connector.lock().unwrap() =
        Some(Arc::new(|_, _, _, _| Err(Error::TooManyAuthAttempts)));
    assert_eq!(
        f.manager
            .get_connection(f.request(false))
            .err()
            .unwrap()
            .code,
        "ssh.too_many_auth_attempts"
    );
    f.manager.close_server("s").unwrap();
    f.cancelled = Arc::new(AtomicBool::new(false));
    f.server.auth_method = AuthMethod::Password;
    f.server.identity_file = None;
    let calls = memory(&f);
    f.manager
        .get_connection(f.request(false))
        .unwrap()
        .ping()
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    f.manager.close_all().unwrap();
}
