//! 普通用户本机 sshd 验证内置转发；测试数据随公共临时根目录自动清理。
use super::*;
use crate::{
    builtin_test_support::{Fixture, sshd},
    ssh::engine::Feature,
};
use std::io::{Read, Write};

fn spec(f: &Fixture, id: &str, forward: TunnelForward) -> TunnelSpec {
    let command = f.command();
    builtin_spec(
        &f.manager,
        command.builtin.as_ref().unwrap().request(),
        Feature::PortForward,
        TunnelSpec {
            builtin: None,
            id: id.into(),
            server_id: f.server.id.clone(),
            target: SshTarget::Manual {
                host: "127.0.0.1".into(),
                port: f.sshd.port,
                user: Some(f.sshd.user.clone()),
                identity_file: Some(f.sshd.root.path().join("client")),
                jump: None,
            },
            options: CommonOpts::default(),
            forwards: vec![forward],
            readiness: Readiness::AuthMarker,
        },
    )
    .unwrap()
}
fn wait(manager: &TunnelManager, id: &str, previous: Option<u64>) -> TunnelEvent {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let event = manager.control().status(id).unwrap();
        if event.state == TunnelState::Connected && event.last_connected_at_ms != previous {
            return event;
        }
        assert!(Instant::now() < deadline, "隧道没有就绪：{event:?}");
        thread::sleep(Duration::from_millis(20));
    }
}
fn socket(port: u16) -> TcpStream {
    let client = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(3),
    )
    .unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    client
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    client
}
fn socks(port: u16, target: u16, address: &[u8], expected: u8) -> TcpStream {
    let mut client = socket(port);
    client.write_all(&[5, 1, 0]).unwrap();
    let mut auth = [0; 2];
    client.read_exact(&mut auth).unwrap();
    assert_eq!(auth, [5, 0]);
    client
        .write_all(&[&[5, 1, 0][..], address, &target.to_be_bytes()].concat())
        .unwrap();
    let mut reply = [0; 10];
    client.read_exact(&mut reply).unwrap();
    assert_eq!(reply[1], expected);
    client
}
fn exchange(target: &TcpListener, mut client: TcpStream) {
    let deadline = Instant::now() + Duration::from_secs(3);
    target.set_nonblocking(true).unwrap();
    let mut server = loop {
        match target.accept() {
            Ok((server, _)) => break server,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("目标接受连接失败：{error}"),
        }
    };
    server
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    server
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    client.write_all("请求".as_bytes()).unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let mut request = Vec::new();
    server.read_to_end(&mut request).unwrap();
    assert_eq!(request, "请求".as_bytes());
    server.write_all(b"response").unwrap();
    server.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    assert_eq!(response, b"response");
}
fn forward(kind: usize, port: u16, target: u16) -> TunnelForward {
    match kind {
        0 => TunnelForward::Local {
            local_port: Some(port),
            remote: ForwardTarget::Tcp { port: target },
            replace_on_conflict: false,
        },
        1 => TunnelForward::Remote {
            remote_port: port,
            local_port: target,
        },
        _ => TunnelForward::Dynamic { local_port: port },
    }
}
fn connect(kind: usize, port: u16, target: u16) -> TcpStream {
    if kind == 2 {
        socks(port, target, &[1, 127, 0, 0, 1], 0)
    } else {
        socket(port)
    }
}

#[test]
#[ignore = "需要沙箱外运行：普通用户启动本机 sshd 并监听随机高位端口"]
fn 三种内置转发连通半关闭断线恢复端口不变及重新启动() {
    let f = Fixture::new();
    let manager = TunnelManager::new(f.program()).unwrap();
    for kind in 0..3 {
        let target = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let target_port = target.local_addr().unwrap().port();
        let port = allocate_port().unwrap();
        let original = spec(&f, "forward", forward(kind, port, target_port));
        manager.start(original.clone()).unwrap();
        let connected = wait(&manager, "forward", None);
        exchange(&target, connect(kind, port, target_port));
        let command = f.command();
        let connection = f
            .manager
            .get_connection(command.builtin.as_ref().unwrap().request())
            .unwrap();
        connection.abort();
        let recovered = wait(&manager, "forward", connected.last_connected_at_ms);
        assert_eq!(recovered.local_ports, connected.local_ports);
        exchange(&target, connect(kind, port, target_port));
        manager.reconnect_all().unwrap();
        let rebuilt = wait(&manager, "forward", recovered.last_connected_at_ms);
        assert_eq!(rebuilt.local_ports, connected.local_ports);
        exchange(&target, connect(kind, port, target_port));
        manager.stop("forward").unwrap();
        // 相同保存模型在新的管理器中重新启用，不需要前端更换端口。
        let restarted = TunnelManager::new(f.program()).unwrap();
        restarted.start(original).unwrap();
        wait(&restarted, "forward", None);
        exchange(&target, connect(kind, port, target_port));
    }
}

#[test]
#[ignore = "需要沙箱外运行：普通用户启动本机 sshd 并监听随机高位端口"]
fn 三种内置转发端口占用有原错误码且自动端口可以替换() {
    let f = Fixture::new();
    let manager = TunnelManager::new(f.program()).unwrap();
    let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = occupied.local_addr().unwrap().port();
    for kind in 0..3 {
        manager
            .start(spec(&f, "occupied", forward(kind, port, 80)))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let event = manager.control().status("occupied").unwrap();
            if let Some(error) = event.error {
                assert_eq!(
                    error.code,
                    if kind == 1 {
                        "tunnel.remote_forward_failed"
                    } else {
                        "tunnel.port_in_use"
                    }
                );
                if kind != 1 {
                    assert_eq!(error.params["port"], port.to_string());
                }
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        manager.stop("occupied").unwrap();
    }
    manager
        .start(spec(
            &f,
            "automatic",
            TunnelForward::Local {
                local_port: Some(port),
                remote: ForwardTarget::Tcp { port: 80 },
                replace_on_conflict: true,
            },
        ))
        .unwrap();
    assert_ne!(wait(&manager, "automatic", None).local_ports[0], port);
}

#[test]
#[ignore = "需要沙箱外运行：普通用户启动本机 sshd 并监听随机高位端口"]
fn 三种内置转发服务器拒绝有提示且终端仍正常() {
    let server = sshd::Server::with_root(
        crate::test_support::tempdir(),
        "AllowTcpForwarding no",
        true,
    );
    let f = Fixture::from_sshd(server);
    let manager = TunnelManager::new(f.program()).unwrap();
    for kind in 0..3 {
        let port = allocate_port().unwrap();
        manager
            .start(spec(&f, "denied", forward(kind, port, 80)))
            .unwrap();
        if kind != 1 {
            wait(&manager, "denied", None);
            let mut client = if kind == 2 {
                socks(port, 80, &[1, 127, 0, 0, 1], 2)
            } else {
                socket(port)
            };
            let mut byte = [0];
            assert!(matches!(client.read(&mut byte), Ok(0) | Err(_)));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let event = manager.control().status("denied").unwrap();
            if let Some(error) = event.error {
                assert_eq!(
                    error.code,
                    if kind == 1 {
                        "tunnel.remote_forward_failed"
                    } else {
                        "ssh.forwarding_prohibited"
                    }
                );
                if kind != 1 {
                    assert_eq!(event.state, TunnelState::Connected);
                }
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        let command = f.command();
        let connection = f
            .manager
            .get_connection(command.builtin.as_ref().unwrap().request())
            .unwrap();
        assert_eq!(sshd::command(&connection, "printf terminal"), b"terminal");
        manager.stop("denied").unwrap();
    }
}

#[test]
#[ignore = "需要沙箱外运行：普通用户启动本机 sshd 并监听随机高位端口"]
fn 远程转发按端口隔离且停止一条不影响其他转发() {
    let f = Fixture::new();
    let manager = TunnelManager::new(f.program()).unwrap();
    let a = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let b = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let pa = allocate_port().unwrap();
    manager
        .start(spec(
            &f,
            "a",
            forward(1, pa, a.local_addr().unwrap().port()),
        ))
        .unwrap();
    wait(&manager, "a", None);
    let pb = allocate_port().unwrap();
    manager
        .start(spec(
            &f,
            "b",
            forward(1, pb, b.local_addr().unwrap().port()),
        ))
        .unwrap();
    wait(&manager, "b", None);
    let ca = socket(pa);
    let cb = socket(pb);
    exchange(&b, cb);
    exchange(&a, ca);
    manager.stop("a").unwrap();
    exchange(&b, socket(pb));
    assert!(TcpListener::bind(("127.0.0.1", pa)).is_ok());
}

#[test]
#[ignore = "需要沙箱外运行：普通用户启动本机 sshd 并监听随机高位端口"]
fn socks域名和ipv6由服务器连接且拒绝后仍能访问允许目标() {
    let v4 = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let v6 = TcpListener::bind(("::1", 0)).unwrap();
    let p4 = v4.local_addr().unwrap().port();
    let p6 = v6.local_addr().unwrap().port();
    let server = sshd::Server::with_root(
        crate::test_support::tempdir(),
        &format!("PermitOpen localhost:{p4} [::1]:{p6}"),
        true,
    );
    let f = Fixture::from_sshd(server);
    let manager = TunnelManager::new(f.program()).unwrap();
    let port = allocate_port().unwrap();
    manager
        .start(spec(&f, "socks", forward(2, port, 80)))
        .unwrap();
    wait(&manager, "socks", None);
    drop(socks(port, 80, &[1, 127, 0, 0, 1], 2));
    exchange(&v4, socks(port, p4, b"\x03\x09localhost", 0));
    exchange(
        &v6,
        socks(
            port,
            p6,
            &[vec![4], std::net::Ipv6Addr::LOCALHOST.octets().to_vec()].concat(),
            0,
        ),
    );
}

#[test]
#[ignore = "需要沙箱外运行两分钟：验证真实 TCP 背压下同一 SSH 连接的终端回显"]
fn 客户端暂停读写两分钟时同一主连接终端回显正常且停止释放资源() {
    let f = Fixture::new();
    let manager = TunnelManager::new(f.program()).unwrap();
    let target = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let target_port = target.local_addr().unwrap().port();
    let port = allocate_port().unwrap();
    manager
        .start(spec(&f, "slow", forward(0, port, target_port)))
        .unwrap();
    wait(&manager, "slow", None);
    let client = socket(port);
    let (mut sender, _) = target.accept().unwrap();
    sender
        .set_write_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    // 填满本机 TCP 缓冲和 SSH 窗口，确保测试实际进入背压。
    let block = vec![b'x'; 64 * 1024];
    let start = Instant::now();
    while sender.write_all(&block).is_ok() {
        assert!(start.elapsed() < Duration::from_secs(10));
    }
    let command = f.command();
    let connection = f
        .manager
        .get_connection(command.builtin.as_ref().unwrap().request())
        .unwrap();
    let mut terminal = connection
        .terminal(
            &pierlink_ssh_engine::Pty::default(),
            &[],
            Some("stty -echo; printf 'READY\n'; exec /bin/cat"),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    let (mut input, mut output, _) = terminal.take_io();
    let mut ready = [0; 7];
    output.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"READY\r\n");
    let closer = terminal.closer();
    let (tx, rx) = mpsc::sync_channel(1);
    let echo = thread::spawn(move || {
        for index in 0..120 {
            if input.write_all(format!("{index:04}\n").as_bytes()).is_err() {
                return;
            }
            let mut bytes = [0; 6];
            if output.read_exact(&mut bytes).is_err() {
                return;
            }
            if tx.send(bytes).is_err() {
                return;
            }
        }
    });
    let paused = Instant::now();
    for index in 0..120 {
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap().as_slice(),
            format!("{index:04}\r\n").as_bytes()
        );
        thread::sleep(Duration::from_secs(1));
    }
    assert!(paused.elapsed() >= Duration::from_secs(120));
    closer.cancel();
    drop(rx);
    echo.join().unwrap();
    manager.stop("slow").unwrap();
    assert!(!connection.is_closed());
    drop(client);
    drop(sender);
    assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
}
