//! 沙箱外用普通用户启动本机 sshd；进程、密钥、配置和日志均在测试结束时清理。
use super::*;
use std::{
    fs,
    io::{ErrorKind, Read, Write},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
};

struct Server {
    child: Option<Child>,
    root: tempfile::TempDir,
    port: u16,
    user: String,
}
impl Server {
    fn new(forwarding: bool) -> Self {
        Self::with_permit_open(forwarding, None)
    }
    fn with_permit_open(forwarding: bool, permit_open: Option<u16>) -> Self {
        let root = crate::test_support::tempdir();
        for name in ["host", "client"] {
            assert!(
                Command::new("/usr/bin/ssh-keygen")
                    .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                    .arg(root.path().join(name))
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned();
        let mut server = Self {
            child: None,
            root,
            port: allocate_port().unwrap(),
            user,
        };
        let path = server.root.path();
        fs::write(path.join("sshd_config"), format!(
            "Port {}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowTcpForwarding {}\nGatewayPorts no\nLogLevel ERROR\nPermitOpen {}\n",
            server.port, path.join("host").display(), path.join("sshd.pid").display(), path.join("client.pub").display(), if forwarding { "yes" } else { "no" }, permit_open.map_or_else(|| "any".into(), |port| format!("127.0.0.1:{port}"))
        )).unwrap();
        fs::write(
            path.join("known_hosts"),
            format!(
                "[127.0.0.1]:{} {}",
                server.port,
                fs::read_to_string(path.join("host.pub")).unwrap()
            ),
        )
        .unwrap();
        server.child = Some(
            Command::new("/usr/sbin/sshd")
                .args(["-D", "-e", "-f"])
                .arg(path.join("sshd_config"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(fs::File::create(path.join("sshd.log")).unwrap())
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                server.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "sshd 启动失败：{}",
                fs::read_to_string(server.root.path().join("sshd.log")).unwrap()
            );
            if TcpStream::connect(("127.0.0.1", server.port)).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "sshd 监听超时");
            thread::sleep(Duration::from_millis(20));
        }
        server
    }
    fn spec(&self, id: &str, forward: TunnelForward) -> TunnelSpec {
        TunnelSpec {
            builtin: None,
            id: id.into(),
            server_id: "本机测试".into(),
            target: SshTarget::Manual {
                host: "127.0.0.1".into(),
                port: self.port,
                user: Some(self.user.clone()),
                identity_file: Some(self.root.path().join("client")),
                jump: None,
            },
            options: CommonOpts {
                known_hosts: Some(self.root.path().join("known_hosts")),
                connect_timeout: Some(2),
                ..Default::default()
            },
            forwards: vec![forward],
            readiness: Readiness::AuthMarker,
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGSTOP);
            }
            kill_descendants(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn children(parent: u32) -> Vec<u32> {
    fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_string_lossy().parse().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let (_, rest) = stat.rsplit_once(") ")?;
            (rest.split_whitespace().nth(1)?.parse::<u32>().ok()? == parent).then_some(pid)
        })
        .collect()
}
fn kill_descendants(parent: u32) {
    for pid in children(parent) {
        unsafe {
            libc::kill(pid as i32, libc::SIGSTOP);
        }
        kill_descendants(pid);
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}
fn wait_connected(manager: &TunnelManager, id: &str, previous: Option<u64>) -> TunnelEvent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = manager.control().status(id).unwrap();
        assert_ne!(event.state, TunnelState::Error, "隧道失败：{event:?}");
        if event.state == TunnelState::Connected && event.last_connected_at_ms != previous {
            return event;
        }
        assert!(Instant::now() < deadline, "隧道五秒内未就绪：{event:?}");
        thread::sleep(Duration::from_millis(20));
    }
}
fn client(port: u16, socks_target: Option<u16>) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    if let Some(target) = socks_target {
        stream.write_all(&[5, 1, 0]).unwrap();
        let mut selected = [0; 2];
        stream.read_exact(&mut selected).unwrap();
        assert_eq!(selected, [5, 0]);
        let [high, low] = target.to_be_bytes();
        stream
            .write_all(&[5, 1, 0, 1, 127, 0, 0, 1, high, low])
            .unwrap();
        let mut reply = [0; 10];
        stream.read_exact(&mut reply).unwrap();
        assert_eq!(&reply[..4], [5, 0, 0, 1]);
    }
    stream
}
fn exchange(listener: &TcpListener, mut client: TcpStream) {
    client.write_all(b"request").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut service = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "转发目标未收到连接");
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("接收转发连接失败：{e}"),
        }
    };
    service
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut request = [0; 7];
    service.read_exact(&mut request).unwrap();
    assert_eq!(&request, b"request");
    service.write_all(b"reply").unwrap();
    let mut reply = [0; 5];
    client.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"reply");
}

#[test]
#[ignore = "需要沙箱外运行：普通用户启动本机 sshd 并监听随机高位端口"]
fn 真实sshd_三种转发_零探测连接_杀ssh自动恢复且端口不变() {
    let server = Server::new(true);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let target_port = listener.local_addr().unwrap().port();
    for kind in ["local", "remote", "dynamic"] {
        let port = allocate_port().unwrap();
        let forward = match kind {
            "local" => TunnelForward::Local {
                local_port: Some(port),
                remote: ForwardTarget::Host {
                    host: "127.0.0.1".into(),
                    port: target_port,
                },
                replace_on_conflict: false,
            },
            "remote" => TunnelForward::Remote {
                remote_port: port,
                local_port: target_port,
            },
            _ => TunnelForward::Dynamic { local_port: port },
        };
        let spec = server.spec(kind, forward);
        let expected_arg = build_args(
            &spec.target,
            &Purpose::Tunnel {
                forwards: vec![match kind {
                    "local" => Forward::Local {
                        local_port: port,
                        remote: ForwardTarget::Host {
                            host: "127.0.0.1".into(),
                            port: target_port,
                        },
                    },
                    "remote" => Forward::Remote {
                        remote_port: port,
                        local_port: target_port,
                    },
                    _ => Forward::Dynamic { local_port: port },
                }],
                readiness: Readiness::AuthMarker,
            },
            &spec.options,
        )
        .unwrap()
        .args;
        let marker = &expected_arg[expected_arg.len() - 2];
        let manager = TunnelManager::new(SshProgram {
            path: "/usr/bin/ssh".into(),
        })
        .unwrap();
        let ports = manager.start(spec).unwrap();
        assert_eq!(ports, if kind == "remote" { vec![] } else { vec![port] });
        let event = wait_connected(&manager, kind, None);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            ErrorKind::WouldBlock,
            "建立隧道不能探测用户服务"
        );
        exchange(
            &listener,
            client(port, (kind == "dynamic").then_some(target_port)),
        );
        let pid = children(std::process::id())
            .into_iter()
            .find(|pid| {
                fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|cmd| cmd.split(|b| *b == 0).any(|arg| arg == marker.as_bytes()))
            })
            .expect("必须找到此测试的 SSH 子进程");
        let started = Instant::now();
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
        let restored = wait_connected(&manager, kind, event.last_connected_at_ms);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(restored.local_ports, ports);
        assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
        exchange(
            &listener,
            client(port, (kind == "dynamic").then_some(target_port)),
        );
        manager.network_restored().unwrap();
        // 请求状态排在网络通知之后；健康连接不应更换时间戳或 SSH 进程。
        let current = manager.control().status(kind).unwrap();
        assert_eq!(current.state, TunnelState::Connected);
        assert_eq!(current.last_connected_at_ms, restored.last_connected_at_ms);
        assert_eq!(current.local_ports, ports);
        exchange(
            &listener,
            client(port, (kind == "dynamic").then_some(target_port)),
        );
        manager.stop(kind).unwrap();
    }
}

#[test]
#[ignore = "需要沙箱外运行：普通用户启动本机 sshd 并监听随机高位端口"]
fn 真实sshd禁止转发_远程可重试_本地就绪后拒绝目标不终止() {
    let server = Server::new(false);
    let manager = TunnelManager::new(SshProgram {
        path: "/usr/bin/ssh".into(),
    })
    .unwrap();
    manager
        .start(server.spec(
            "remote",
            TunnelForward::Remote {
                remote_port: allocate_port().unwrap(),
                local_port: 80,
            },
        ))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = manager.control().status("remote").unwrap();
        if let Some(error) = event.error {
            assert_eq!(error.code, "tunnel.remote_forward_failed");
            assert!(event.next_retry_at_ms.is_some());
            assert_ne!(event.state, TunnelState::Error);
            break;
        }
        assert!(Instant::now() < deadline, "未识别远程转发失败");
        thread::sleep(Duration::from_millis(20));
    }
    manager.stop("remote").unwrap();
    let local = allocate_port().unwrap();
    manager
        .start(server.spec(
            "local",
            TunnelForward::Local {
                local_port: Some(local),
                remote: ForwardTarget::Tcp { port: 80 },
                replace_on_conflict: false,
            },
        ))
        .unwrap();
    let connected = wait_connected(&manager, "local", None);
    let mut blocked = client(local, None);
    let mut byte = [0];
    assert_eq!(blocked.read(&mut byte).unwrap(), 0);
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        let event = manager.control().status("local").unwrap();
        assert_eq!(event.state, TunnelState::Connected);
        assert_eq!(event.last_connected_at_ms, connected.last_connected_at_ms);
        assert!(event.error.is_none());
        thread::sleep(Duration::from_millis(20));
    }
    manager.stop("local").unwrap();
}

#[test]
#[ignore = "需要沙箱外运行：普通用户启动本机 sshd 并监听随机高位端口"]
fn 真实sshd_permitopen_拒绝目标后本地和socks转发仍可访问允许目标() {
    let allowed = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    allowed.set_nonblocking(true).unwrap();
    let allowed_port = allowed.local_addr().unwrap().port();
    let denied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    denied.set_nonblocking(true).unwrap();
    let denied_port = denied.local_addr().unwrap().port();
    let server = Server::with_permit_open(true, Some(allowed_port));
    let manager = TunnelManager::new(SshProgram {
        path: "/usr/bin/ssh".into(),
    })
    .unwrap();
    for dynamic in [false, true] {
        let port = allocate_port().unwrap();
        let forward = if dynamic {
            TunnelForward::Dynamic { local_port: port }
        } else {
            TunnelForward::Local {
                local_port: Some(port),
                remote: ForwardTarget::Tcp { port: allowed_port },
                replace_on_conflict: false,
            }
        };
        let mut spec = server.spec("permitopen", forward);
        let denied_local = if dynamic {
            port
        } else {
            let denied_local = unique_port(&[port], allocate_port).unwrap();
            spec.forwards.push(TunnelForward::Local {
                local_port: Some(denied_local),
                remote: ForwardTarget::Tcp { port: denied_port },
                replace_on_conflict: false,
            });
            denied_local
        };
        manager.start(spec).unwrap();
        let connected = wait_connected(&manager, "permitopen", None);
        let mut blocked = client(denied_local, None);
        if dynamic {
            blocked.write_all(&[5, 1, 0]).unwrap();
            let mut selected = [0; 2];
            blocked.read_exact(&mut selected).unwrap();
            assert_eq!(selected, [5, 0]);
            let [high, low] = denied_port.to_be_bytes();
            blocked
                .write_all(&[5, 1, 0, 1, 127, 0, 0, 1, high, low])
                .unwrap();
        }
        let mut reply = [0; 10];
        assert_eq!(blocked.read(&mut reply).unwrap(), 0);
        assert_eq!(denied.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
        exchange(&allowed, client(port, dynamic.then_some(allowed_port)));
        let current = manager.control().status("permitopen").unwrap();
        assert_eq!(current.state, TunnelState::Connected);
        assert_eq!(current.last_connected_at_ms, connected.last_connected_at_ms);
        assert!(current.error.is_none());
        manager.stop("permitopen").unwrap();
    }
}
