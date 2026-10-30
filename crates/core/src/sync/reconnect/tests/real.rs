use super::*;
use crate::{
    ssh::{args::CommonOpts, target::SshTarget},
    syncthing::{folder::FolderSpec, process::SyncthingBin},
    test_support::tempdir,
    tunnel::{TunnelManager, allocate_port},
};
use std::{
    fs, io,
    net::{Shutdown, TcpListener, TcpStream},
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    sync::atomic::AtomicUsize,
};

struct Relay {
    port: u16,
    enabled: Arc<AtomicBool>,
    attempts: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    sockets: Arc<Mutex<Vec<TcpStream>>>,
    worker: Option<JoinHandle<()>>,
}
impl Relay {
    fn start(target: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let enabled = Arc::new(AtomicBool::new(true));
        let attempts = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let sockets = Arc::new(Mutex::new(Vec::<TcpStream>::new()));
        let (open, count, stop, active) = (
            enabled.clone(),
            attempts.clone(),
            stopped.clone(),
            sockets.clone(),
        );
        let worker = thread::spawn(move || {
            let mut copies = vec![];
            while !stop.load(Ordering::Acquire) {
                let Ok((mut incoming, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                count.fetch_add(1, Ordering::AcqRel);
                if !open.load(Ordering::Acquire) {
                    continue;
                }
                let mut outgoing = TcpStream::connect(("127.0.0.1", target)).unwrap();
                let (mut source, mut sink) =
                    (incoming.try_clone().unwrap(), outgoing.try_clone().unwrap());
                active
                    .lock()
                    .unwrap()
                    .extend([incoming.try_clone().unwrap(), outgoing.try_clone().unwrap()]);
                copies.push(thread::spawn(move || {
                    let _ = io::copy(&mut incoming, &mut outgoing);
                    let _ = outgoing.shutdown(Shutdown::Write);
                }));
                copies.push(thread::spawn(move || {
                    let _ = io::copy(&mut sink, &mut source);
                    let _ = source.shutdown(Shutdown::Write);
                }));
            }
            for socket in active.lock().unwrap().drain(..) {
                let _ = socket.shutdown(Shutdown::Both);
            }
            for copy in copies {
                copy.join().unwrap();
            }
        });
        Self {
            port,
            enabled,
            attempts,
            stopped,
            sockets,
            worker: Some(worker),
        }
    }
    fn interrupt(&self, local: &LocalSync, remote: &LocalSync) {
        self.enabled.store(false, Ordering::Release);
        let before = self.attempts.load(Ordering::Acquire);
        for socket in self.sockets.lock().unwrap().drain(..) {
            let _ = socket.shutdown(Shutdown::Both);
        }
        wait_for(Duration::from_secs(70), || {
            !connected(local, remote) && self.attempts.load(Ordering::Acquire) > before
        });
        // 让失败拨号完成并进入 Syncthing 的重试等待，再恢复转发。
        thread::sleep(Duration::from_millis(300));
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "设备连接未在时限内恢复");
        thread::sleep(Duration::from_millis(50));
    }
}
fn connected(local: &LocalSync, remote: &LocalSync) -> bool {
    local.client().connections().unwrap()["connections"][remote.device_id()]["connected"] == true
        && remote.client().connections().unwrap()["connections"][local.device_id()]["connected"]
            == true
}
fn local(bin: &SyncthingBin, path: &Path) -> (LocalSync, u16) {
    let gui = allocate_port().unwrap();
    let mut port = allocate_port().unwrap();
    while port == gui {
        port = allocate_port().unwrap();
    }
    (
        LocalSync::start(bin, path, gui, port, "test-key", "test-password").unwrap(),
        port,
    )
}
fn info(remote: &LocalSync, port: u16, root: &Path) -> ServerEndpointInfo {
    ServerEndpointInfo {
        server_id: "reconnect-server".into(),
        name: "测试服务器".into(),
        device_id: remote.device_id().into(),
        socket_path: "/unused".into(),
        sync_port: port,
        api_key: "test-key".into(),
        state_dir: root.join("state").display().to_string(),
    }
}
fn bin() -> Option<SyncthingBin> {
    let bin = std::env::var_os("PIERLINK_TEST_SYNCTHING");
    if bin.is_none() {
        eprintln!("跳过真实设备重连测试：未设置 PIERLINK_TEST_SYNCTHING");
    }
    bin.map(|path| SyncthingBin { path: path.into() })
}

#[test]
fn 真实设备重拨前后耗时与同步安全() {
    let Some(bin) = bin() else {
        return;
    };
    let root = tempdir();
    let (a, _) = local(&bin, &root.path().join("a"));
    let (b, port) = local(&bin, &root.path().join("b"));
    let relay = Relay::start(port);
    let endpoint = info(&b, port, root.path());
    let _link = crate::sync::ServerLink::from_parts(
        b.client().clone(),
        format!("tcp://127.0.0.1:{}", relay.port),
        &endpoint,
        &a,
        "电脑",
    )
    .unwrap();
    // Syncthing 2.1.7 实测默认重拨间隔是 20 秒（文档写的 60 秒是旧版本），只记录，不写死
    let interval = a.client().get_options().unwrap()["reconnectionIntervalS"].clone();
    eprintln!("Syncthing 默认重拨间隔 = {interval} 秒");
    assert!(
        interval.as_u64().is_some_and(|s| s >= 10),
        "重拨间隔异常：{interval}"
    );
    let folder_id = "pl-reconnect1234567";
    let a_files = root.path().join("a-files");
    let b_files = root.path().join("b-files");
    fs::create_dir(&a_files).unwrap();
    fs::create_dir(&b_files).unwrap();
    for (instance, peer, path) in [(&a, &b, &a_files), (&b, &a, &b_files)] {
        instance
            .client()
            .put_folder(&FolderSpec {
                id: folder_id.into(),
                path: path.clone(),
                devices: vec![peer.device_id().into()],
                paused: false,
                watch_delay_s: 1,
                ..Default::default()
            })
            .unwrap();
    }
    fs::write(a_files.join("原有文件"), "原有内容").unwrap();
    wait_for(Duration::from_secs(30), || {
        connected(&a, &b) && b_files.join("原有文件").exists()
    });
    let folders = [
        a.client().list_folders().unwrap(),
        b.client().list_folders().unwrap(),
    ];
    let before_sequence = [
        a.client().db_status(folder_id).unwrap()["sequence"].clone(),
        b.client().db_status(folder_id).unwrap()["sequence"].clone(),
    ];
    relay.interrupt(&a, &b);
    let start = Instant::now();
    relay.enabled.store(true, Ordering::Release);
    wait_for(Duration::from_secs(75), || connected(&a, &b));
    let baseline = start.elapsed();
    // 超过短连接阈值，避免刚连上就断开引入额外冷却。
    thread::sleep(Duration::from_secs(6));
    relay.interrupt(&a, &b);
    let mut events = crate::sync::EventPump::new(&a);
    events.poll().unwrap();
    let start = Instant::now();
    relay.enabled.store(true, Ordering::Release);
    let mut state = ReconnectState {
        local: a.client().clone(),
        address_updates: a.address_updates.clone(),
        local_id: a.device_id().into(),
        remote_id: b.device_id().into(),
        api_key: "test-key".into(),
        resume_remote: false,
        remote: None,
    };
    state.recover(b.client(), relay.port).unwrap();
    wait_for(Duration::from_secs(8), || connected(&a, &b));
    let improved = start.elapsed();
    assert!(
        events.poll().unwrap().iter().any(|event| {
            event.device.as_deref() == Some(b.device_id())
                && event.kind == crate::sync::SyncEventKind::DeviceConnected
        }),
        "地址重拨后的真实连接恢复应保留"
    );
    eprintln!("Syncthing 2.1.7 默认重拨：{baseline:?}；主动重拨：{improved:?}");
    assert!(improved < Duration::from_secs(8));
    assert!(improved < baseline, "主动重拨应快于默认等待");
    assert_eq!(a.client().list_folders().unwrap(), folders[0]);
    assert_eq!(b.client().list_folders().unwrap(), folders[1]);
    assert_eq!(
        a.client().db_status(folder_id).unwrap()["sequence"],
        before_sequence[0]
    );
    assert_eq!(
        b.client().db_status(folder_id).unwrap()["sequence"],
        before_sequence[1]
    );
    assert_eq!(
        fs::read_to_string(b_files.join("原有文件")).unwrap(),
        "原有内容"
    );
    fs::write(a_files.join("重连后新文件"), "无需重新扫描").unwrap();
    wait_for(Duration::from_secs(10), || {
        b_files.join("重连后新文件").exists()
    });
    assert_eq!(
        fs::read_to_string(b_files.join("重连后新文件")).unwrap(),
        "无需重新扫描"
    );
}

#[test]
fn 连续四次断开恢复后第五次在五秒内重连() {
    let Some(bin) = bin() else {
        return;
    };
    let before = cooled_reconnect(&bin, false);
    let after = cooled_reconnect(&bin, true);
    eprintln!("耗尽冷却后第五次重连：旧方案 {before:?}；地址切换 {after:?}");
    assert!(after < Duration::from_secs(5));
}

fn cooled_reconnect(bin: &SyncthingBin, change_address: bool) -> Duration {
    let root = tempdir();
    let (a, _) = local(bin, &root.path().join("a"));
    let (b, port) = local(bin, &root.path().join("b"));
    let relay = Relay::start(port);
    let endpoint = info(&b, port, root.path());
    let _link = crate::sync::ServerLink::from_parts(
        b.client().clone(),
        format!("tcp://127.0.0.1:{}", relay.port),
        &endpoint,
        &a,
        "电脑",
    )
    .unwrap();
    wait_for(Duration::from_secs(30), || connected(&a, &b));
    let mut state = ReconnectState {
        local: a.client().clone(),
        address_updates: a.address_updates.clone(),
        local_id: a.device_id().into(),
        remote_id: b.device_id().into(),
        api_key: "test-key".into(),
        resume_remote: false,
        remote: None,
    };
    // 两组各用独立实例；前四轮也走待测方案，耗尽设备强制重拨额度并覆盖地址复用。
    for round in 1..=4 {
        relay.interrupt(&a, &b);
        relay.enabled.store(true, Ordering::Release);
        if change_address {
            state.recover(b.client(), relay.port).unwrap();
        } else {
            set_paused(a.client(), b.device_id(), true).unwrap();
            set_paused(a.client(), b.device_id(), false).unwrap();
        }
        wait_for(Duration::from_secs(75), || connected(&a, &b));
        eprintln!("冷却准备第 {round} 轮完成（切换地址={change_address}）");
    }
    relay.interrupt(&a, &b);
    let started = Instant::now();
    relay.enabled.store(true, Ordering::Release);
    if change_address {
        state.recover(b.client(), relay.port).unwrap();
    } else {
        set_paused(a.client(), b.device_id(), true).unwrap();
        set_paused(a.client(), b.device_id(), false).unwrap();
    }
    wait_for(
        Duration::from_secs(if change_address { 5 } else { 75 }),
        || connected(&a, &b),
    );
    let elapsed = started.elapsed();
    eprintln!("耗尽冷却后第五次重连（切换地址={change_address}）：{elapsed:?}");
    for (instance, peer) in [(&a, &b), (&b, &a)] {
        let device = instance
            .client()
            .call(
                "GET",
                &format!("/rest/config/devices/{}", peer.device_id()),
                &[],
                None,
            )
            .unwrap();
        assert_eq!(device["paused"], false);
    }
    elapsed
}

struct Sshd(Child);
impl Drop for Sshd {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn with_ssh_pair(
    check: impl FnOnce(&Path, &LocalSync, &LocalSync, &TunnelManager, &mut crate::sync::ServerLink),
) {
    let Some(bin) = bin() else {
        return;
    };
    let root = tempdir();
    let (a, _) = local(&bin, &root.path().join("a"));
    let port = allocate_port().unwrap();
    let remote_spec = crate::syncthing::home::HomeSpec {
        home: root.path().join("b"),
        gui: crate::syncthing::home::GuiListen::Unix {
            path: root.path().join("s/api"),
        },
        gui_user: "pierlink".into(),
        listen_port: port,
    };
    let remote_info =
        crate::syncthing::home::init_home(&bin, &remote_spec, "test-password").unwrap();
    let instance =
        crate::syncthing::process::start(&bin, &remote_spec.home, &remote_spec.gui, "test-key")
            .unwrap();
    let b = LocalSync {
        address_updates: Default::default(),
        client: instance.client().clone(),
        instance: Some(instance),
        id: remote_info.device_id,
    };
    for name in ["host", "client"] {
        assert!(
            Command::new("/usr/bin/ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(root.path().join(name))
                .status()
                .unwrap()
                .success()
        );
    }
    let ssh_port = allocate_port().unwrap();
    let config = root.path().join("sshd_config");
    fs::write(&config, format!("Port {ssh_port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowTcpForwarding yes\nAllowStreamLocalForwarding yes\nLogLevel ERROR\n", root.path().join("host").display(), root.path().join("sshd.pid").display(), root.path().join("client.pub").display())).unwrap();
    let log = fs::File::create(root.path().join("sshd.log")).unwrap();
    let mut sshd = Sshd(
        Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    wait_for(Duration::from_secs(5), || {
        assert!(
            sshd.0.try_wait().unwrap().is_none(),
            "sshd 退出：{}",
            fs::read_to_string(root.path().join("sshd.log")).unwrap()
        );
        TcpStream::connect(("127.0.0.1", ssh_port)).is_ok()
    });
    let known_hosts = root.path().join("known_hosts");
    fs::write(
        &known_hosts,
        format!(
            "[127.0.0.1]:{ssh_port} {}",
            fs::read_to_string(root.path().join("host.pub")).unwrap()
        ),
    )
    .unwrap();
    let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned();
    let control = TunnelManager::new(crate::process::SshProgram {
        path: "/usr/bin/ssh".into(),
    })
    .unwrap();
    let mut endpoint = info(&b, port, root.path());
    endpoint.socket_path = root.path().join("s/api").display().to_string();
    let mut link = crate::sync::ServerLink::connect(
        &control,
        SshTarget::Manual {
            host: "127.0.0.1".into(),
            port: ssh_port,
            user: Some(user),
            identity_file: Some(root.path().join("client")),
            jump: None,
        },
        CommonOpts {
            known_hosts: Some(known_hosts),
            ..Default::default()
        },
        &endpoint,
        &a,
        "电脑",
    )
    .unwrap();
    wait_for(Duration::from_secs(8), || connected(&a, &b));
    check(root.path(), &a, &b, &control, &mut link);
    // 有的测试故意让管理接口一直连不上，收尾时可能还没恢复：只接受这一种收尾失败
    if let Err(e) = link.disconnect(&control) {
        assert_eq!(
            e.params.get("cause").map(String::as_str),
            Some("syncthing.unreachable"),
            "{e:?}"
        );
    }
}

#[test]
fn 真实隧道重连_网络恢复保留连接_探活失败重建() {
    with_ssh_pair(|root, a, b, control, link| {
        let original_port = link.local_sync_port();
        for network in [false, true] {
            let previous = control
                .control()
                .status("sync:reconnect-server")
                .unwrap()
                .last_connected_at_ms;
            let started = Instant::now();
            if network {
                control.network_restored().unwrap();
            } else {
                control.reconnect_all().unwrap();
            }
            if network {
                // 网络等级短暂变化时 SSH 仍存活，探活成功必须保留原连接。
                thread::sleep(Duration::from_millis(500));
                let status = control.control().status("sync:reconnect-server").unwrap();
                assert_eq!(status.state, TunnelState::Connected);
                assert_eq!(status.last_connected_at_ms, previous);
                assert!(connected(a, b));
            } else {
                wait_for(Duration::from_secs(8), || {
                    let status = control.control().status("sync:reconnect-server").unwrap();
                    status.state == TunnelState::Connected
                        && status.last_connected_at_ms != previous
                        && connected(a, b)
                });
            }
            link.refresh_address(a).unwrap();
            assert_eq!(
                control
                    .control()
                    .status("sync:reconnect-server")
                    .unwrap()
                    .local_ports[0],
                original_port
            );
            eprintln!(
                "实际 SSH 重建（网络恢复={network}）至双端连接：{:?}",
                started.elapsed()
            );
        }
        let previous = control
            .control()
            .status("sync:reconnect-server")
            .unwrap()
            .last_connected_at_ms;
        let socket = root.join("s/api");
        let moved = root.join("s/api-moved");
        fs::rename(&socket, &moved).unwrap();
        control.network_restored().unwrap();
        wait_for(Duration::from_secs(8), || {
            let status = control.control().status("sync:reconnect-server").unwrap();
            status.state == TunnelState::Reconnecting || status.last_connected_at_ms != previous
        });
        fs::rename(&moved, &socket).unwrap();
        wait_for(Duration::from_secs(8), || {
            let status = control.control().status("sync:reconnect-server").unwrap();
            status.state == TunnelState::Connected
                && status.last_connected_at_ms != previous
                && connected(a, b)
        });
        assert_eq!(
            control
                .control()
                .status("sync:reconnect-server")
                .unwrap()
                .local_ports[0],
            original_port
        );
    });
}

#[test]
fn 真实双实例管理接口一直不恢复_重建次数与间隔受限() {
    with_ssh_pair(|root, _a, _b, tunnels, link| {
        let control = tunnels.control();
        let before = control.status("sync:reconnect-server").unwrap();
        let original_port = link.local_sync_port();
        // 两个真实实例继续运行；只把远端管理套接字移开，直到测试结束都不恢复。
        fs::rename(root.join("s/api"), root.join("s/api-offline")).unwrap();
        let started = Instant::now();
        tunnels.network_restored().unwrap();
        let mut rebuilds = vec![];
        let mut schedules = vec![];
        let mut connected_at = before.last_connected_at_ms;
        wait_for(Duration::from_secs(25), || {
            let status = control.status("sync:reconnect-server").unwrap();
            assert_eq!(status.local_ports[0], original_port);
            if status.last_connected_at_ms != connected_at {
                connected_at = status.last_connected_at_ms;
                rebuilds.push(started.elapsed());
            }
            if status.state == TunnelState::Reconnecting
                && let Some(next) = status.next_retry_at_ms
                && schedules
                    .last()
                    .is_none_or(|(generation, _)| *generation != connected_at)
            {
                schedules.push((connected_at, next.saturating_sub(status.timestamp_ms)));
            }
            schedules.len() == 3
        });
        assert_eq!(rebuilds.len(), 2, "三次探活失败中第三次必须仍在等待退避");
        for ((_, delay_ms), expected) in schedules.iter().zip([1000, 2000, 5000]) {
            assert!(
                (expected - 100..=expected).contains(delay_ms),
                "探活重建退避应递增：{schedules:?}"
            );
        }
        assert!(rebuilds[0] >= UNREACHABLE_REBUILD_AFTER + Duration::from_secs(1));
        assert!(rebuilds[1] - rebuilds[0] >= UNREACHABLE_REBUILD_AFTER + Duration::from_secs(2));
        let generation = control
            .status("sync:reconnect-server")
            .unwrap()
            .last_connected_at_ms;
        thread::sleep(Duration::from_millis(500));
        let waiting = control.status("sync:reconnect-server").unwrap();
        assert_eq!(waiting.state, TunnelState::Reconnecting);
        assert_eq!(waiting.last_connected_at_ms, generation);
        assert!(started.elapsed() < Duration::from_secs(26));
        eprintln!(
            "管理接口持续不可达：完成重建 {} 次，重建耗时 {rebuilds:?}，退避 {schedules:?}",
            rebuilds.len()
        );
    });
}
