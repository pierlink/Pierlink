//! 内存 SSH 对端验证隧道分派、后台凭据等待及连接生命周期。
use super::*;
use crate::{
    process::SshProgram,
    ssh::{args::CommonOpts, engine::Feature, target::SshTarget},
    tunnel::{Readiness, TunnelForward, TunnelManager, TunnelSpec, TunnelState, builtin_spec},
};

fn spec(f: &Fixture) -> TunnelSpec {
    TunnelSpec {
        builtin: None,
        id: "forward:test".into(),
        server_id: f.server.id.clone(),
        target: SshTarget::Manual {
            host: "127.0.0.1".into(),
            port: 22,
            user: Some("tester".into()),
            identity_file: None,
            jump: None,
        },
        options: CommonOpts::default(),
        forwards: vec![TunnelForward::Remote {
            remote_port: 32123,
            local_port: 80,
        }],
        readiness: Readiness::AuthMarker,
    }
}
fn fixture() -> Fixture {
    let mut f = Fixture::new(AuthMethod::Password);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    let engine = Engine::new().unwrap();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
        pierlink_ssh_engine::test_support::connect_forwarding(&engine, options, auth)
    }));
    f
}
fn start(f: &Fixture, tunnels: &TunnelManager) {
    tunnels
        .start(builtin_spec(&f.manager, f.request(true), Feature::PortForward, spec(f)).unwrap())
        .unwrap();
}
fn tunnels() -> TunnelManager {
    TunnelManager::new(SshProgram {
        path: "/不存在的ssh程序".into(),
    })
    .unwrap()
}
fn wait(tunnels: &TunnelManager, state: TunnelState) -> crate::tunnel::TunnelEvent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = tunnels.control().status("forward:test").unwrap();
        if event.state == state {
            return event;
        }
        assert!(Instant::now() < deadline, "等待隧道状态超时：{event:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn 终端取消共享登录后后台隧道等待密码且可恢复() {
    struct CancelOnDrop(Arc<AtomicBool>);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let f = fixture();
    let tunnels = tunnels();
    let (entered, started) = std::sync::mpsc::sync_channel(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let callbacks = Arc::new(WaitingCallbacks {
        stage: "password",
        entered,
    });
    std::thread::scope(|scope| {
        let _cancel_on_exit = CancelOnDrop(cancelled.clone());
        let terminal = scope.spawn(|| {
            let mut request = f.request(true);
            request.callbacks = callbacks;
            request.cancelled = cancelled.clone();
            f.manager.get_connection(request)
        });
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        start(&f, &tunnels);
        managed::wait_for_waiter(&f);
        cancelled.store(true, Ordering::Release);
        assert_eq!(
            terminal.join().unwrap().err().unwrap().code,
            "ssh.auth_cancelled"
        );
        let waiting = wait(&tunnels, TunnelState::WaitingPassword);
        assert_eq!(waiting.error.unwrap().code, "ssh.password_needed");
        assert!(!f.cancelled.load(Ordering::Acquire));
    });
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    wait(&tunnels, TunnelState::Connected);
    tunnels.stop("forward:test").unwrap();
}

#[test]
fn 转发分派保留系统参数且遵守功能开关和连接类别() {
    let mut f = fixture();
    f.server.connection_engine = crate::storage::Engine::OpenSsh;
    let original = spec(&f);
    let selected = builtin_spec(
        &f.manager,
        f.request(true),
        Feature::PortForward,
        original.clone(),
    )
    .unwrap();
    assert!(selected.builtin.is_none());
    assert_eq!(selected.target, original.target);
    assert_eq!(selected.id, original.id);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    let selected =
        builtin_spec(&f.manager, f.request(true), Feature::SyncTunnel, spec(&f)).unwrap();
    assert!(selected.builtin.is_some());
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let mut request = f.request(true);
    request.class = ConnectionClass::Bulk;
    let selected = builtin_spec(&f.manager, request, Feature::SyncTunnel, spec(&f)).unwrap();
    assert!(selected.builtin.is_some());
    let tunnels = tunnels();
    tunnels.start(selected).unwrap();
    wait(&tunnels, TunnelState::Connected);
    let mut request = f.request(false);
    request.class = ConnectionClass::Bulk;
    let bulk = f.manager.get_connection(request).unwrap();
    assert_eq!(bulk.window_size(), 2 * 1024 * 1024);
    // 已有远程登记在所传的批量连接上，而不是另开主连接。
    assert!(bulk.remote_forward(32123).is_err());
    let mut wrong = spec(&f);
    wrong.server_id = "other".into();
    assert!(builtin_spec(&f.manager, f.request(false), Feature::PortForward, wrong).is_err());
}

#[test]
fn 后台缺少或错误密码时等待且新密码自动恢复不弹框() {
    let f = fixture();
    let tunnels = tunnels();
    start(&f, &tunnels);
    let waiting = wait(&tunnels, TunnelState::WaitingPassword);
    assert_eq!(waiting.attempt, 0);
    assert!(waiting.next_retry_at_ms.is_none());
    tunnels.reconnect_all().unwrap();
    tunnels.network_restored().unwrap();
    assert_eq!(wait(&tunnels, TunnelState::WaitingPassword).attempt, 0);
    f.vault.set("s", Zeroizing::new("错误密码".into())).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.vault.has("s") {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let waiting = wait(&tunnels, TunnelState::WaitingPassword);
    assert_eq!(waiting.error.unwrap().code, "ssh.auth_failed");
    assert_eq!(waiting.attempt, 0);
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    wait(&tunnels, TunnelState::Connected);
    assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 0);
    assert!(f.callbacks.challenges.lock().unwrap().is_empty());
    tunnels.stop("forward:test").unwrap();
}

#[test]
fn 断线重连网络恢复主动重建与停止均不改变共享终端生命周期() {
    use std::io::{Read, Write};
    let f = fixture();
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let tunnels = tunnels();
    start(&f, &tunnels);
    let first = wait(&tunnels, TunnelState::Connected);
    let connection = f.manager.get_connection(f.request(false)).unwrap();
    let mut terminal = connection
        .shell(&pierlink_ssh_engine::Pty::default(), &[])
        .unwrap();
    tunnels.reconnect_all().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = wait(&tunnels, TunnelState::Connected);
        if event.last_connected_at_ms != first.last_connected_at_ms {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(connection.same_connection(&f.manager.get_connection(f.request(false)).unwrap()));
    terminal.stdin.write_all(b"echo").unwrap();
    let mut bytes = [0; 4];
    terminal.stdout.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"echo");
    connection.abort();
    wait(&tunnels, TunnelState::Reconnecting);
    tunnels.network_restored().unwrap();
    wait(&tunnels, TunnelState::Connected);
    let replacement = f.manager.get_connection(f.request(false)).unwrap();
    assert!(!replacement.same_connection(&connection));
    tunnels.stop("forward:test").unwrap();
    assert!(!replacement.is_closed());
    drop(tunnels);
    // 重建管理器后，以同一持久化 spec 自动启用。
    let restarted = self::tunnels();
    start(&f, &restarted);
    wait(&restarted, TunnelState::Connected);
    f.cancelled.store(true, Ordering::Release);
    assert_eq!(
        wait(&restarted, TunnelState::Error).error.unwrap().code,
        "ssh.auth_cancelled"
    );
    assert!(!replacement.is_closed());
}

#[test]
fn 私钥失败不伪装为等待服务器密码() {
    let mut f = fixture();
    f.server.auth_method = AuthMethod::Key;
    f.server.identity_file = Some("/不存在的测试私钥".into());
    let tunnels = tunnels();
    start(&f, &tunnels);
    let error = wait(&tunnels, TunnelState::Error);
    assert_eq!(error.error.unwrap().code, "ssh.auth_failed");
    assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 0);
}

#[test]
fn 等待密码期间取消后新凭据不会重启隧道() {
    let f = fixture();
    let tunnels = tunnels();
    start(&f, &tunnels);
    wait(&tunnels, TunnelState::WaitingPassword);
    f.cancelled.store(true, Ordering::Release);
    wait(&tunnels, TunnelState::Error);
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let event = tunnels.control().status("forward:test").unwrap();
    assert_eq!(event.state, TunnelState::Error);
    assert!(event.next_retry_at_ms.is_none());
}

#[test]
fn 验证码后台转发和同步等待主连接登录并在断网后再次等待() {
    use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer};
    for feature in [Feature::PortForward, Feature::SyncTunnel] {
        let f = fixture();
        let peer = LoginPeer::new(LoginMode::PasswordAndOtp).with_forwarding();
        let connected = peer.clone();
        let engine = Engine::new().unwrap();
        *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
            connected.connect(&engine, options, auth)
        }));
        f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
        let tunnels = tunnels();
        let mut request = f.request(false);
        if feature == Feature::SyncTunnel {
            request.class = ConnectionClass::Bulk;
        }
        tunnels
            .start(builtin_spec(&f.manager, request, feature, spec(&f)).unwrap())
            .unwrap();
        let mut previous = None;
        for _ in 0..2 {
            let waiting = wait(&tunnels, TunnelState::WaitingPassword);
            assert_eq!(
                waiting.error.as_ref().unwrap().code,
                "ssh.two_factor_required"
            );
            assert!(waiting.next_retry_at_ms.is_none());
            let attempts = peer.attempts().len();
            // 有新密码、网络恢复或主动重连都不能代替验证码登录。
            f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
            tunnels.network_restored().unwrap();
            tunnels.reconnect_all().unwrap();
            std::thread::sleep(Duration::from_millis(150));
            let still_waiting = tunnels.control().status("forward:test").unwrap();
            assert_eq!(still_waiting.state, TunnelState::WaitingPassword);
            assert_eq!(still_waiting.attempt, waiting.attempt);
            assert_eq!(peer.attempts().len(), attempts);
            let started = Instant::now();
            // 与“登录”和终端入口相同：交互式获取主连接。
            let main = f.manager.get_connection(f.request(true)).unwrap();
            let resumed = wait(&tunnels, TunnelState::Connected);
            assert!(started.elapsed() < Duration::from_secs(2));
            assert_ne!(resumed.last_connected_at_ms, previous);
            previous = resumed.last_connected_at_ms;
            assert!(resumed.error.is_none());
            let mut request = f.request(false);
            request.class = ConnectionClass::Bulk;
            assert!(main.same_connection(&f.manager.get_connection(request).unwrap()));
            assert!(main.remote_forward(32123).is_err());
            main.abort();
            tunnels.network_restored().unwrap();
        }
        wait(&tunnels, TunnelState::WaitingPassword);
        assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 0);
        assert_eq!(f.callbacks.challenges.lock().unwrap().len(), 2);
        tunnels.stop("forward:test").unwrap();
    }
}

#[test]
fn 跳板验证码只由对应服务器登录唤醒且停止后不再重试() {
    use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer};
    let mut f = fixture();
    let mut jump = f.server.clone();
    jump.id = "jump".into();
    f.server.jump_server_id = Some(jump.id.clone());
    let record = jump.clone();
    f.manager.set_server_resolver(Arc::new(move |id| {
        Ok((id == record.id).then(|| record.clone()))
    }));
    let peer = LoginPeer::new(LoginMode::PasswordAndOtp).with_forwarding();
    let connected = peer.clone();
    let engine = Engine::new().unwrap();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |server, _, options, auth| {
        if server.id == "jump" {
            connected.connect(&engine, options, auth)
        } else {
            pierlink_ssh_engine::test_support::connect_forwarding(&engine, options, auth)
        }
    }));
    for id in ["s", "jump"] {
        f.vault.set(id, Zeroizing::new("测试密码".into())).unwrap();
    }
    let tunnels = tunnels();
    start(&f, &tunnels);
    let waiting = wait(&tunnels, TunnelState::WaitingPassword);
    assert_eq!(waiting.error.unwrap().params["server_id"], "jump");
    let attempts = peer.attempts().len();
    f.manager.publish("s", ConnectionState::Connected, None);
    f.vault
        .set("jump", Zeroizing::new("测试密码".into()))
        .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        tunnels.control().status("forward:test").unwrap().state,
        TunnelState::WaitingPassword
    );
    assert_eq!(peer.attempts().len(), attempts);
    let mut request = f.request(true);
    request.server = &jump;
    let main = f.manager.get_connection(request).unwrap();
    wait(&tunnels, TunnelState::Connected);
    tunnels.stop("forward:test").unwrap();
    let attempts = peer.attempts().len();
    main.abort();
    tunnels.network_restored().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(peer.attempts().len(), attempts);
    assert_eq!(
        tunnels.control().status("forward:test").unwrap_err().code,
        "tunnel.not_found"
    );
}

#[test]
fn 等待验证码期间取消后主连接登录也不能重启隧道() {
    use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer};
    let f = fixture();
    let peer = LoginPeer::new(LoginMode::PasswordAndOtp).with_forwarding();
    let engine = Engine::new().unwrap();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
        peer.connect(&engine, options, auth)
    }));
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let tunnels = tunnels();
    start(&f, &tunnels);
    wait(&tunnels, TunnelState::WaitingPassword);
    f.cancelled.store(true, Ordering::Release);
    assert_eq!(
        wait(&tunnels, TunnelState::Error).error.unwrap().code,
        "ssh.auth_cancelled"
    );
    let mut request = f.request(true);
    request.cancelled = Arc::new(AtomicBool::new(false));
    let main = f.manager.get_connection(request).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let event = tunnels.control().status("forward:test").unwrap();
    assert_eq!(event.state, TunnelState::Error);
    assert!(event.next_retry_at_ms.is_none());
    main.remote_forward(32123).unwrap();
    main.cancel_forward(32123).unwrap();
}

#[test]
fn 切换前关闭服务器回收两类连接且重新分派不影响其他服务器() {
    use crate::ssh::engine::{Engine as Selection, FEATURE_SUPPORT, engine_for_feature};
    let mut f = fixture();
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let main = f.manager.get_connection(f.request(false)).unwrap();
    let mut request = f.request(false);
    request.class = ConnectionClass::Bulk;
    let bulk = f.manager.get_connection(request).unwrap();
    let mut other = f.server.clone();
    other.id = "other".into();
    f.vault
        .set("other", Zeroizing::new("测试密码".into()))
        .unwrap();
    let mut request = f.request(false);
    request.server = &other;
    let unaffected = f.manager.get_connection(request).unwrap();
    let tunnels = tunnels();
    start(&f, &tunnels);
    wait(&tunnels, TunnelState::Connected);
    // 上层切换前需先停止旧功能，再关闭池，防止旧配置的后台持有者抢先重连。
    tunnels.stop("forward:test").unwrap();
    f.manager.close_server(&f.server.id).unwrap();
    assert!(main.is_closed() && bulk.is_closed());
    unaffected.ping().unwrap();
    f.server.connection_engine = Selection::OpenSsh;
    for &(feature, _) in FEATURE_SUPPORT {
        assert_eq!(
            engine_for_feature(&f.server, feature).engine,
            Selection::OpenSsh
        );
    }
    assert!(
        builtin_spec(&f.manager, f.request(false), Feature::PortForward, spec(&f))
            .unwrap()
            .builtin
            .is_none()
    );
    f.server.connection_engine = Selection::Builtin;
    for &(feature, _) in FEATURE_SUPPORT {
        assert_eq!(
            engine_for_feature(&f.server, feature).engine,
            Selection::Builtin
        );
    }
    start(&f, &tunnels);
    wait(&tunnels, TunnelState::Connected);
    let replacement = f.manager.get_connection(f.request(false)).unwrap();
    assert!(!replacement.same_connection(&main));
    assert!(!replacement.same_connection(&bulk));
    unaffected.ping().unwrap();
    tunnels.stop("forward:test").unwrap();
}

#[test]
fn 隧道登录上限直接失败且密码与网络更新不重试() {
    let f = fixture();
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        Err(Error::TooManyAuthAttempts)
    }));
    let tunnels = tunnels();
    start(&f, &tunnels);
    let event = wait(&tunnels, TunnelState::Error);
    assert_eq!(event.error.unwrap().code, "ssh.too_many_auth_attempts");
    assert!(event.next_retry_at_ms.is_none());
    tunnels.network_restored().unwrap();
    f.vault.set("s", Zeroizing::new("新密码".into())).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let event = tunnels.control().status("forward:test").unwrap();
    assert_eq!(event.state, TunnelState::Error);
    assert!(event.next_retry_at_ms.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 0);
}
