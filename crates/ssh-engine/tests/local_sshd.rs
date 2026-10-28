//! 沙箱外执行：cargo test --offline -p pierlink-ssh-engine --test local_sshd -- --ignored
#![cfg(unix)]
mod support;
use pierlink_ssh_engine::{
    Authentication, ChannelFailure, ConfiguredAuthentication, Engine, Error, HostKeyCheck,
    PrivateKeyFile, Pty, sh_quote,
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};

#[test]
#[ignore = "需要沙箱外监听本机端口"]
fn local_exec_env_stdio_exit_pty_resize_sftp() {
    let server = support::Server::new("");
    let engine = Engine::new().unwrap();
    let connection = server.connect(&engine);
    let mut exec = connection
        .exec(
            "read value; printf '%s/%s' \"$PIERLINK_TEST\" \"$value\"; printf error >&2; exit 7",
            &[("PIERLINK_TEST", "环境变量")],
        )
        .unwrap();
    exec.stdin.write_all(b"input\n").unwrap();
    exec.stdin.shutdown().unwrap();
    let mut out = String::new();
    let mut err = String::new();
    exec.stdout.read_to_string(&mut out).unwrap();
    exec.stderr.read_to_string(&mut err).unwrap();
    assert_eq!(out, "环境变量/input");
    assert_eq!(err, "error");
    assert_eq!(exec.wait().unwrap(), Some(7));
    let mut shell = connection.shell(&Pty::default(), &[]).unwrap();
    shell
        .resize(&Pty {
            columns: 101,
            rows: 37,
            ..Pty::default()
        })
        .unwrap();
    shell
        .stdin
        .write_all(b"stty -echo; stty size; exit\n")
        .unwrap();
    let mut text = String::new();
    shell.stdout.read_to_string(&mut text).unwrap();
    assert!(text.contains("37 101"), "未收到调整后的窗口尺寸");
    assert_eq!(shell.wait().unwrap(), Some(0));
    let sftp = connection.sftp_handshake().unwrap();
    assert_eq!(sftp.version, 3);
    drop(sftp);
    let running = connection.exec("sleep 60", &[]).unwrap();
    running.close().unwrap();
    drop(running);
    connection.ping().unwrap();
    connection.disconnect().unwrap();
}

#[test]
#[ignore = "需要沙箱外监听本机端口"]
fn local_direct_streamlocal_and_tcpip_bidirectional() {
    use std::os::unix::net::UnixListener;
    let server = support::Server::new("AllowStreamLocalForwarding yes\nAllowTcpForwarding yes");
    let connection = server.connect(&Engine::new().unwrap());
    let path = server.root.path().join("echo.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::thread::scope(|scope| {
        let echo = scope.spawn(|| {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data = [0; 8];
            stream.read_exact(&mut data).unwrap();
            assert_eq!(&data, b"unixping");
            stream.write_all(b"unixpong").unwrap();
        });
        let mut tunnel = connection
            .direct_streamlocal(path.to_str().unwrap())
            .unwrap();
        tunnel.write_all(b"unixping").unwrap();
        tunnel.flush().unwrap();
        let mut data = [0; 8];
        tunnel.read_exact(&mut data).unwrap();
        assert_eq!(&data, b"unixpong");
        tunnel.shutdown().unwrap();
        echo.join().unwrap();
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::scope(|scope| {
        let echo = scope.spawn(|| {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data = [0; 7];
            stream.read_exact(&mut data).unwrap();
            stream.write_all(&data).unwrap();
        });
        let mut stream = connection.direct_tcpip("127.0.0.1", port).unwrap();
        stream.write_all(b"tcpping").unwrap();
        let mut data = [0; 7];
        stream.read_exact(&mut data).unwrap();
        assert_eq!(&data, b"tcpping");
        echo.join().unwrap();
    });
}

#[test]
#[ignore = "需要沙箱外监听本机端口"]
fn local_remote_forward_accept_cancel() {
    let server = support::Server::new("AllowTcpForwarding yes");
    let connection = server.connect(&Engine::new().unwrap());
    let port = connection.remote_forward(0).unwrap();
    std::thread::scope(|scope| {
        let peer = scope.spawn(|| {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream.write_all(b"request").unwrap();
            let mut response = [0; 8];
            stream.read_exact(&mut response).unwrap();
            assert_eq!(&response, b"response");
        });
        let forwarded = connection.accept_forward().unwrap();
        assert_eq!(forwarded.connected_port, port);
        let mut tunnel = forwarded.stream;
        let mut request = [0; 7];
        tunnel.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"request");
        tunnel.write_all(b"response").unwrap();
        peer.join().unwrap();
    });
    connection.cancel_forward(port).unwrap();
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
}

#[test]
#[ignore = "需要沙箱外监听本机端口"]
fn local_auth_rejected_hostkey_mismatch_forwarding_prohibited() {
    let server = support::Server::new("DisableForwarding yes");
    let engine = Engine::new().unwrap();
    let mut changed = server.options();
    changed.host_key = HostKeyCheck::Fingerprint("SHA256:不匹配".into());
    assert!(matches!(
        engine.connect(changed, server.auth()),
        Err(Error::HostKeyRejected)
    ));
    assert!(matches!(
        engine.connect(
            server.options(),
            Authentication::Password("不可显示的测试密码".into())
        ),
        Err(Error::Authentication)
    ));
    let connection = server.connect(&engine);
    assert!(matches!(
        connection.direct_tcpip("127.0.0.1", 22),
        Err(Error::ChannelRejected(ChannelFailure::Prohibited))
    ));
    // OpenSSH 禁止转发时，套接字转发报的是“连接失败”而不是“被禁止”，两种都算拒绝
    assert!(matches!(
        connection.direct_streamlocal("/不存在.sock"),
        Err(Error::ChannelRejected(
            ChannelFailure::Prohibited | ChannelFailure::ConnectFailed
        ))
    ));
    assert!(matches!(
        connection.remote_forward(0),
        Err(Error::RequestRejected(_))
    ));
    assert!(connection.exec("true", &[("FORBIDDEN", "value")]).is_err());
    assert!(matches!(
        connection.exec("\0", &[]),
        Err(Error::InvalidInput)
    ));
    assert!(matches!(
        connection.direct_tcpip("", 0),
        Err(Error::InvalidInput)
    ));
}

#[test]
#[ignore = "需要沙箱外监听本机端口"]
fn local_three_jump_levels_verify_each_host() {
    let servers: Vec<_> = (0..4)
        .map(|_| support::Server::new("AllowTcpForwarding yes"))
        .collect();
    let engine = Engine::new().unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut chain = None;
    for server in &servers {
        let expected = match server.options().host_key {
            HostKeyCheck::Fingerprint(value) => value,
            _ => unreachable!(),
        };
        let mut options = server.options();
        let observed = observed.clone();
        options.host_key = HostKeyCheck::Callback(Arc::new(move |key| {
            let trusted = key.fingerprint == expected;
            observed.lock().unwrap().push(key.port);
            trusted
        }));
        chain = Some(match &chain {
            None => engine.connect(options, server.auth()).unwrap(),
            Some(parent) => engine.connect_via(parent, options, server.auth()).unwrap(),
        });
    }
    assert_eq!(
        *observed.lock().unwrap(),
        servers.iter().map(|server| server.port).collect::<Vec<_>>()
    );
    let last = chain.unwrap();
    assert_eq!(support::command(&last, "printf nested"), b"nested");
    let mut rejected = servers[0].options();
    rejected.host_key = HostKeyCheck::Fingerprint("SHA256:错误的下级指纹".into());
    assert!(matches!(
        engine.connect_via(&last, rejected, servers[0].auth()),
        Err(Error::HostKeyRejected)
    ));
}

#[test]
#[ignore = "需要沙箱外监听本机端口"]
fn local_openssh_key_algorithms_and_passphrase() {
    let server = support::Server::new("");
    let engine = Engine::new().unwrap();
    for algorithm in ["ed25519", "ecdsa", "rsa"] {
        let path = server.root.path().join(algorithm);
        support::keygen(&path, algorithm, "测试口令");
        let public = std::fs::read(path.with_extension("pub")).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(server.root.path().join("client.pub"))
            .unwrap()
            .write_all(&public)
            .unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        let auth = Authentication::PrivateKey {
            path,
            passphrase: Some(Arc::new(move || {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Some("测试口令".into())
            })),
        };
        let connection = engine.connect(server.options(), auth).unwrap();
        assert_eq!(support::command(&connection, "printf key-ok"), b"key-ok");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        let configured = Authentication::Configured(ConfiguredAuthentication {
            agent: false,
            agent_identities_only: false,
            private_keys: vec![PrivateKeyFile {
                path: server.root.path().join(algorithm),
                passphrase: Some(Arc::new(|| Some("测试口令".into()))),
            }],
            skip_unusable_keys: false,
            password: None,
            answers: Arc::new(|_| panic!("密钥不能回退密码")),
        });
        let connection = engine.connect(server.options(), configured).unwrap();
        assert_eq!(
            support::command(&connection, "printf configured-key-ok"),
            b"configured-key-ok"
        );
    }
    let path = sh_quote(server.root.path().to_str().unwrap()).unwrap();
    assert!(path.starts_with('\''));
}

#[test]
#[ignore = "需要沙箱外监听本机 Unix 套接字和 TCP 端口"]
fn local_linux_agent_lists_signs_and_authenticates_selected_keys() {
    use russh::keys::{HashAlg, PublicKey, agent::client::AgentClient, signature::Verifier};
    if let Ok(root) = std::env::var("PL_ENGINE_AGENT_CHILD_ROOT") {
        let engine = Engine::new().unwrap();
        let listed = engine.agent_keys().unwrap();
        let port: u16 = std::env::var("PL_ENGINE_AGENT_CHILD_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let user = std::env::var("PL_ENGINE_AGENT_CHILD_USER").unwrap();
        let host =
            PublicKey::from_openssh(&std::fs::read_to_string(format!("{root}/host.pub")).unwrap())
                .unwrap();
        for algorithm in ["ed25519", "ecdsa", "rsa"] {
            let public = PublicKey::from_openssh(
                &std::fs::read_to_string(format!("{root}/agent-{algorithm}.pub")).unwrap(),
            )
            .unwrap();
            assert!(listed.iter().any(|key| key.key_data() == public.key_data()));
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                let mut agent = AgentClient::connect_env().await.unwrap();
                for hash in if algorithm == "rsa" {
                    vec![Some(HashAlg::Sha256), Some(HashAlg::Sha512)]
                } else {
                    vec![None]
                } {
                    let signature = agent
                        .sign_request_signature(&public, hash, b"agent-test")
                        .await
                        .unwrap();
                    Verifier::verify(&public, b"agent-test", &signature).unwrap();
                    if let Some(hash) = hash {
                        assert_eq!(
                            signature.algorithm(),
                            russh::keys::Algorithm::Rsa { hash: Some(hash) }
                        );
                    }
                }
            });
            let options = pierlink_ssh_engine::ConnectionOptions::new(
                "127.0.0.1",
                port,
                &user,
                HostKeyCheck::Fingerprint(host.fingerprint(HashAlg::Sha256).to_string()),
            );
            let connection = engine
                .connect(
                    options.clone(),
                    Authentication::Agent { public_key: public },
                )
                .unwrap();
            assert_eq!(
                support::command(&connection, "printf agent-ok"),
                b"agent-ok"
            );
            let connection = engine
                .connect(
                    options,
                    Authentication::Configured(ConfiguredAuthentication {
                        agent: true,
                        agent_identities_only: false,
                        private_keys: vec![],
                        skip_unusable_keys: true,
                        password: None,
                        answers: Arc::new(|_| panic!("agent 不能询问密码")),
                    }),
                )
                .unwrap();
            assert_eq!(
                support::command(&connection, "printf configured-agent-ok"),
                b"configured-agent-ok"
            );
        }
        return;
    }
    let server = support::Server::new("");
    let socket = server.root.path().join("agent.sock");
    struct Agent(std::process::Child);
    impl Drop for Agent {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut agent = Agent(
        std::process::Command::new("/usr/bin/ssh-agent")
            .args(["-D", "-a"])
            .arg(&socket)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let started = std::time::Instant::now();
    while !socket.exists() {
        assert!(agent.0.try_wait().unwrap().is_none(), "agent 提前退出");
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    for algorithm in ["ed25519", "ecdsa", "rsa"] {
        let key = server.root.path().join(format!("agent-{algorithm}"));
        support::keygen(&key, algorithm, "");
        let public = std::fs::read(key.with_extension("pub")).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(server.root.path().join("client.pub"))
            .unwrap()
            .write_all(&public)
            .unwrap();
        let status = std::process::Command::new("/usr/bin/ssh-add")
            .arg(&key)
            .env("SSH_AUTH_SOCK", &socket)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "local_linux_agent_lists_signs_and_authenticates_selected_keys",
        ])
        .env("SSH_AUTH_SOCK", &socket)
        .env("PL_ENGINE_AGENT_CHILD_ROOT", server.root.path())
        .env("PL_ENGINE_AGENT_CHILD_PORT", server.port.to_string())
        .env("PL_ENGINE_AGENT_CHILD_USER", &server.user)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "agent 子进程测试失败：{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
#[ignore = "需要沙箱外监听 IPv4/IPv6 本机端口"]
fn local_ipv6_literal_dns_network_and_handshake_timeout() {
    let server = support::Server::new("ListenAddress ::1");
    let engine = Engine::new().unwrap();
    let mut options = server.options();
    options.host = "::1".into();
    let connection = engine.connect(options, server.auth()).unwrap();
    connection.ping().unwrap();
    let mut options = server.options();
    options.host = "pierlink-test-no-such-host.invalid".into();
    assert!(matches!(
        engine.connect(options, server.auth()),
        Err(Error::Dns)
    ));
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let unused = listener.local_addr().unwrap().port();
    drop(listener);
    let mut options = server.options();
    options.port = unused;
    assert!(matches!(
        engine.connect(options, server.auth()),
        Err(Error::Network(std::io::ErrorKind::ConnectionRefused))
    ));
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let mut options = server.options();
    options.port = listener.local_addr().unwrap().port();
    options.connect_timeout = Duration::from_millis(50);
    std::thread::scope(|scope| {
        let peer = scope.spawn(|| {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut byte = [0];
            let _ = stream.read(&mut byte);
            std::thread::sleep(Duration::from_millis(200));
        });
        assert!(matches!(
            engine.connect(options, server.auth()),
            Err(Error::Timeout("SSH 握手"))
        ));
        peer.join().unwrap();
    });
}

#[test]
#[ignore = "需要沙箱外监听本机端口，验收耗时至少 60 秒"]
fn local_unread_channel_60s_isolated_bounded_complete() {
    use std::time::Instant;
    let server =
        support::Server::new("ClientAliveInterval 1\nClientAliveCountMax 3\nRekeyLimit 64K");
    let engine = Engine::new().unwrap();
    let mut options = server.options();
    options.window_size = 2 * 1024 * 1024;
    let window = options.window_size as usize;
    let connection = engine.connect(options, server.auth()).unwrap();
    let mut flood = connection.exec("head -c 8388608 /dev/zero", &[]).unwrap();
    let mut echo = connection.exec("exec cat", &[]).unwrap();
    let before = connection.statistics().key_exchanges;
    let started = Instant::now();
    for second in 0..60 {
        let round = Instant::now();
        echo.stdin.write_all(&[b'b'; 4096]).unwrap();
        let mut reply = [0; 4096];
        echo.stdout.read_exact(&mut reply).unwrap();
        assert_eq!(reply, [b'b'; 4096]);
        assert!(
            round.elapsed() < Duration::from_millis(100),
            "通道 B 往返超出 100ms"
        );
        let ping = Instant::now();
        connection.ping().unwrap();
        assert!(
            ping.elapsed() < Duration::from_millis(100),
            "SSH ping 超出 100ms"
        );
        assert!(flood.buffered_bytes() <= window);
        std::thread::sleep(
            (started + Duration::from_secs(second + 1)).saturating_duration_since(Instant::now()),
        );
    }
    assert_eq!(flood.buffered_bytes(), window);
    assert!(connection.statistics().key_exchanges > before);
    let mut count = 0;
    let mut block = [0; 32768];
    loop {
        let size = flood.stdout.read(&mut block).unwrap();
        if size == 0 {
            break;
        }
        assert!(block[..size].iter().all(|byte| *byte == 0));
        count += size;
        assert!(flood.buffered_bytes() <= window);
    }
    assert_eq!(count, 8 * 1024 * 1024);
    assert_eq!(flood.buffered_bytes(), 0);
    assert_eq!(flood.wait().unwrap(), Some(0));
    echo.stdin.shutdown().unwrap();
    assert_eq!(echo.wait().unwrap(), Some(0));
    connection.ping().unwrap();
    connection.disconnect().unwrap();
}

#[test]
#[ignore = "需要沙箱外监听本机端口，验收耗时至少 60 秒"]
fn local_blocked_upload_60s_other_channel_and_remote_close() {
    use std::time::Instant;
    let server =
        support::Server::new("ClientAliveInterval 1\nClientAliveCountMax 3\nRekeyLimit 64K");
    let connection = server.connect(&Engine::new().unwrap());
    let mut blocked = connection.exec("printf ready; sleep 60", &[]).unwrap();
    let mut ready = [0; 5];
    blocked.stdout.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready");
    let started = Instant::now();
    let mut echo = connection.exec("exec cat", &[]).unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let result = blocked.stdin.write_all(&vec![0x5a; 8 * 1024 * 1024]);
        // 同一个写入器在报错后再次写入也必须返回错误，不能重新挂起或 panic。
        let retry = blocked.stdin.write(b"retry");
        let _ = done_tx.send((result, retry));
    });
    let checks = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for second in 0..60 {
            if second < 59 {
                assert!(
                    matches!(
                        done_rx.try_recv(),
                        Err(std::sync::mpsc::TryRecvError::Empty)
                    ),
                    "8 MiB 写入应等窗口至远端关闭"
                );
            }
            let round = Instant::now();
            echo.stdin.write_all(b"echo\n").unwrap();
            let mut reply = [0; 5];
            echo.stdout.read_exact(&mut reply).unwrap();
            assert_eq!(&reply, b"echo\n");
            assert!(
                round.elapsed() < Duration::from_millis(100),
                "通道 B 往返超出 100ms"
            );
            let ping = Instant::now();
            connection.ping().unwrap();
            assert!(ping.elapsed() < Duration::from_millis(100));
            std::thread::sleep(
                (started + Duration::from_secs(second + 1))
                    .saturating_duration_since(Instant::now()),
            );
        }
        let (result, retry) = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_err());
        assert!(retry.is_err());
        assert_ne!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(62),
            "远端关闭后写入方未立即返回"
        );
        echo.stdin.write_all(b"alive\n").unwrap();
        let mut reply = [0; 6];
        echo.stdout.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"alive\n");
        echo.stdin.shutdown().unwrap();
        assert_eq!(echo.wait().unwrap(), Some(0));
        connection.ping().unwrap();
    }));
    // 断言失败时也先取消阻塞写入，再回收线程，避免失败测试永久挂住。
    let _ = connection.disconnect();
    writer.join().unwrap();
    if let Err(panic) = checks {
        std::panic::resume_unwind(panic);
    }
}

#[test]
#[ignore = "需要沙箱外监听本机端口"]
fn 本机终端粘贴八兆同时下载三十二兆并频繁换钥() {
    let server = support::Server::new("RekeyLimit 64K\nMaxSessions 2");
    let engine = Engine::new().unwrap();
    let connection = server.connect(&engine);
    let (done, completed) = std::sync::mpsc::channel();
    // 所有读写共用总期限；失败时先断开连接，确保阻塞读写和线程都能释放。
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let mut terminal = connection.shell(&Pty::default(), &[]).unwrap();
            terminal.stdin.write_all(
                b"stty raw -echo; printf 'p%sl-ready' ''; head -c 8388608 | cat >/dev/null; stty sane; printf 'p%sl-pasted\\n' ''\n",
            ).unwrap();
            wait_terminal_marker(&mut terminal.stdout, b"pl-ready");
            let mut download = connection.exec("head -c 33554432 /dev/zero", &[]).unwrap();
            let before = connection.statistics().key_exchanges;
            std::thread::scope(|transfer| {
                let upload = transfer.spawn(|| {
                    let block = [b'p'; 64 * 1024];
                    for _ in 0..128 {
                        terminal.stdin.write_all(&block).unwrap();
                    }
                });
                let mut block = [0xff; 64 * 1024];
                let mut received = 0;
                loop {
                    let count = download.stdout.read(&mut block).unwrap();
                    if count == 0 { break; }
                    assert!(block[..count].iter().all(|byte| *byte == 0));
                    received += count;
                }
                assert_eq!(received, 32 * 1024 * 1024);
                assert_eq!(download.wait().unwrap(), Some(0));
                upload.join().unwrap();
            });
            wait_terminal_marker(&mut terminal.stdout, b"pl-pasted");
            terminal.stdin.write_all(b"printf 'p%sl-alive\\n' ''\n").unwrap();
            wait_terminal_marker(&mut terminal.stdout, b"pl-alive");
            assert!(connection.statistics().key_exchanges > before + 10);
            terminal.stdin.write_all(b"exit\n").unwrap();
            assert_eq!(terminal.wait().unwrap(), Some(0));
            done.send(()).unwrap();
        });
        let result = completed.recv_timeout(Duration::from_secs(60));
        let _ = connection.disconnect();
        worker.join().unwrap();
        result.expect("终端粘贴、并行下载及后续回显必须在 60 秒内完成");
    });
}

fn wait_terminal_marker(reader: &mut impl Read, marker: &[u8]) {
    let mut tail = std::collections::VecDeque::with_capacity(marker.len());
    loop {
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        if tail.len() == marker.len() {
            tail.pop_front();
        }
        tail.push_back(byte[0]);
        if tail.iter().copied().eq(marker.iter().copied()) {
            return;
        }
    }
}

#[test]
#[ignore = "需要沙箱外监听本机端口"]
fn local_upload_64m_rekey_64k_restores_writable_packet() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    struct Client {
        fingerprint: String,
        exchanges: Arc<AtomicUsize>,
    }
    impl russh::client::Handler for Client {
        type Error = russh::Error;
        async fn check_server_key(
            &mut self,
            key: &russh::keys::PublicKeyOrCertificate,
        ) -> Result<bool, Self::Error> {
            Ok(key
                .public_key()
                .fingerprint(russh::keys::HashAlg::Sha256)
                .to_string()
                == self.fingerprint)
        }
        async fn kex_done(
            &mut self,
            _: Option<&[u8]>,
            _: &russh::Names,
            _: &mut russh::client::Session,
        ) -> Result<(), Self::Error> {
            self.exchanges.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }
    let server = support::Server::new("RekeyLimit 64K\nMaxSessions 2");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(180), async {
            let HostKeyCheck::Fingerprint(fingerprint) = server.options().host_key else { unreachable!() };
            let exchanges = Arc::new(AtomicUsize::new(0));
            let mut client = russh::client::connect(Arc::new(russh::client::Config::default()),
                ("127.0.0.1", server.port), Client { fingerprint, exchanges: exchanges.clone() }).await.unwrap();
            let key = russh::keys::load_secret_key(server.root.path().join("client"), None).unwrap();
            assert!(client.authenticate_publickey(&server.user,
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), None)).await.unwrap().success());
            let mut channel = client.channel_open_session().await.unwrap();
            channel.exec(true, "dd bs=65536 count=1024 iflag=fullblock of=/dev/null 2>/dev/null; printf done; cat >/dev/null").await.unwrap();
            loop {
                match channel.wait().await.unwrap() {
                    russh::ChannelMsg::Success => break,
                    russh::ChannelMsg::Failure => panic!("上传命令被拒绝"),
                    _ => {}
                }
            }
            let (mut reader, writer) = channel.split();
            let mut tx = writer.make_writer();
            let block = vec![0x5a; 32 * 1024];
            for _ in 0..2048 { tx.write_all(&block).await.unwrap(); }
            let mut done = [0; 4];
            reader.make_reader().read_exact(&mut done).await.unwrap();
            assert_eq!(&done, b"done");
            client.send_ping().await.unwrap();
            assert!(exchanges.load(Ordering::Relaxed) > 10, "64 MiB 上传必须多次换钥");
            assert_eq!(writer.writable_packet_size().await, 32 * 1024, "可写窗口镜像发生漂移");
            writer.close().await.unwrap();
            client.disconnect(russh::Disconnect::ByApplication, "测试结束", "").await.unwrap();
        }).await.expect("sshd 换钥上传超时");
    });
}

#[test]
#[ignore = "需要沙箱外监听本机端口"]
fn local_drop_and_closer_rekey_maxsessions_two_ten_cycles() {
    let server = support::Server::new("RekeyLimit 64K\nMaxSessions 2");
    let engine = Engine::new().unwrap();
    let connection = server.connect(&engine);
    let anchor = connection.exec("sleep 60", &[]).unwrap();
    let before = connection.statistics().key_exchanges;
    for index in 0..10 {
        let started = std::time::Instant::now();
        let mut session = loop {
            match connection.exec("cat >/dev/null", &[]) {
                Ok(session) => break session,
                Err(Error::ChannelRejected(ChannelFailure::ConnectFailed))
                    if started.elapsed() < Duration::from_secs(5) =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => panic!("关闭后未释放 MaxSessions 名额"),
            }
        };
        session.stdin.write_all(&vec![0x5a; 256 * 1024]).unwrap();
        if index % 2 == 1 {
            session.closer().close().unwrap();
        }
        drop(session);
        connection.ping().unwrap();
    }
    assert!(connection.statistics().key_exchanges > before + 2);
    // 末轮同样要确认名额归还。
    let started = std::time::Instant::now();
    loop {
        if let Ok(probe) = connection.exec("true", &[]) {
            drop(probe);
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "末轮关闭泄漏会话名额"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    anchor.close().unwrap();
    connection.disconnect().unwrap();
}
