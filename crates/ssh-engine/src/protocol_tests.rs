//! 内存双工流上的真实 SSH 协议测试，沙箱内不监听端口。
use crate::{Authentication, ChannelFailure, ConnectionOptions, Engine, Error, HostKeyCheck, Pty};
use russh::{
    ChannelId,
    keys::{HashAlg, PrivateKey, ssh_key::private::Ed25519Keypair},
    server,
};
use std::{
    borrow::Cow,
    collections::HashMap,
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::memory_peer::{Peer, password, peer_config, start, start_options};
#[path = "flow_control_tests.rs"]
mod flow_control_tests;

#[test]
fn 连接与克隆保留服务器版本字符串且换钥后不变() {
    let engine = Engine::new().unwrap();
    for sshid in [
        "SSH-2.0-OpenSSH_for_Windows_9.5",
        "SSH-2.0-OpenSSH_9.6p1 Ubuntu-3ubuntu13",
    ] {
        let connection = start(
            &engine,
            Peer {
                server_id: Some(sshid.into()),
                ..Default::default()
            },
            password(),
            None,
        )
        .unwrap();
        assert_eq!(connection.remote_sshid(), Some(sshid));
        let cloned = connection.clone();
        assert_eq!(cloned.remote_sshid(), Some(sshid));
        let before = connection.statistics().key_exchanges;
        let mut session = connection.shell(&Pty::default(), &[]).unwrap();
        let data = vec![b'x'; 256 * 1024];
        let mut received = vec![0; data.len()];
        std::thread::scope(|scope| {
            scope.spawn(|| session.stdin.write_all(&data).unwrap());
            session.stdout.read_exact(&mut received).unwrap();
        });
        assert_eq!(received, data);
        assert!(connection.statistics().key_exchanges > before);
        assert_eq!(connection.remote_sshid(), Some(sshid));
        assert_eq!(cloned.remote_sshid(), Some(sshid));
        session.close().unwrap();
    }
}

#[test]
fn 终端合并输出可拆流并非阻塞查询状态() {
    let engine = Engine::new().unwrap();
    let connection = start(&engine, Peer::default(), password(), None).unwrap();
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut session = connection
        .terminal(
            &Pty::default(),
            &[("IGNORED", "value")],
            Some("output"),
            cancelled.clone(),
        )
        .unwrap();
    let (_, mut stdout, mut stderr) = session.take_io();
    let mut bytes = Vec::new();
    stdout.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"stdoutstderr");
    assert_eq!(stderr.read(&mut [0; 1]).unwrap(), 0);
    assert_eq!(session.wait().unwrap(), Some(7));
    assert_eq!(session.try_wait_status().unwrap().unwrap().code, Some(7));
    let (_, mut empty, _) = session.take_io();
    assert_eq!(empty.read(&mut [0; 1]).unwrap(), 0);
    drop(session);
    let mut session = connection
        .terminal(&Pty::default(), &[], None, cancelled.clone())
        .unwrap();
    assert!(session.try_wait_status().is_none());
    let (mut stdin, mut stdout, _) = session.take_io();
    stdin.write_all(b"echo").unwrap();
    let mut bytes = [0; 4];
    stdout.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"echo");
    session.close().unwrap();
    assert!(session.try_wait_status().unwrap().is_err());
    assert!(!connection.is_closed());
    assert!(
        connection
            .terminal(
                &Pty {
                    columns: 0,
                    ..Pty::default()
                },
                &[],
                None,
                cancelled.clone()
            )
            .is_err()
    );
    assert!(
        connection
            .terminal(
                &Pty::default(),
                &[("BAD=ENV", "x")],
                None,
                cancelled.clone()
            )
            .is_err()
    );
    assert!(
        connection
            .terminal(&Pty::default(), &[], Some("secret\0"), cancelled.clone())
            .is_err()
    );
    assert!(
        connection
            .terminal(&Pty::default(), &[], Some("reject"), cancelled.clone())
            .is_err()
    );
    cancelled.store(true, std::sync::atomic::Ordering::Release);
    assert!(
        connection
            .terminal(&Pty::default(), &[], None, cancelled)
            .is_err()
    );
}

#[test]
fn 认证途中服务器断开时报断开而不是认证失败() {
    use crate::test_support::{LoginMode, LoginPeer};
    let engine = Engine::new().unwrap();
    let peer = LoginPeer::new(LoginMode::DisconnectOnPassword);
    let options = ConnectionOptions::new(
        "内存",
        22,
        "测试",
        HostKeyCheck::Callback(Arc::new(|_| true)),
    );
    let error = peer.connect(&engine, options, password()).err().unwrap();
    assert!(matches!(error, Error::Disconnected), "{error:?}");
    assert!(peer.attempts().contains(&"password"));
}

#[test]
fn 远端关闭后仍可分块读完已缓冲的双输出() {
    use crate::test_support::{LoginMode, LoginPeer, ScriptReply};
    let engine = Engine::new().unwrap();
    let peer = LoginPeer::new(LoginMode::Password).with_scripts(|_| ScriptReply {
        stdout: (0..90001).map(|i| (i % 251) as u8).collect(),
        stderr: (0..70003).map(|i| ((i + 1) % 251) as u8).collect(),
        exit_code: Some(17),
    });
    let mut options = ConnectionOptions::new(
        "内存",
        22,
        "测试",
        HostKeyCheck::Callback(Arc::new(|_| true)),
    );
    options.window_size = 256 * 1024;
    let connection = peer.connect(&engine, options, password()).unwrap();
    let mut session = connection
        .exec(r#"/bin/sh -c ''"'"'/bin/sh'"'"' '"'"'-s'"'"''"#, &[])
        .unwrap();
    session.stdin.shutdown().unwrap();
    // 输出小于窗口，先等远端 Close，确保后续读取完全来自关闭后的缓冲。
    assert_eq!(session.wait_status().unwrap().code, Some(17));
    assert_eq!(session.buffered_bytes(), 160004);
    for (stream, length, offset) in [
        (&mut session.stdout, 90001, 0),
        (&mut session.stderr, 70003, 1),
    ] {
        let mut buffer = [0; 997];
        let mut received = Vec::new();
        loop {
            let count = stream.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..count]);
        }
        assert_eq!(received.len(), length);
        assert!(
            received
                .iter()
                .enumerate()
                .all(|(i, byte)| *byte == ((i + offset) % 251) as u8)
        );
    }
    assert_eq!(session.buffered_bytes(), 0);
    assert_eq!(session.wait().unwrap(), Some(17));
    connection.ping().unwrap();
}

#[test]
fn memory_exec_env_exit_rejection_and_maxsessions() {
    let engine = Engine::new().unwrap();
    let connection = start(&engine, Peer::default(), password(), None).unwrap();
    let mut session = connection.exec("output", &[("ACCEPTED", "值")]).unwrap();
    assert_eq!(connection.statistics().started_sessions, 1);
    let mut out = String::new();
    let mut err = String::new();
    session.stdout.read_to_string(&mut out).unwrap();
    session.stderr.read_to_string(&mut err).unwrap();
    assert_eq!((out.as_str(), err.as_str()), ("stdout", "stderr"));
    assert_eq!(session.wait().unwrap(), Some(7));
    drop(session);
    assert!(matches!(
        connection.exec("echo", &[("REJECTED", "值")]),
        Err(Error::RequestRejected("环境变量"))
    ));
    std::thread::sleep(Duration::from_millis(50));
    assert!(matches!(
        connection.exec("reject", &[]),
        Err(Error::RequestRejected(_))
    ));
    std::thread::sleep(Duration::from_millis(50));
    let first = connection.exec("echo", &[]).unwrap();
    let second = connection.exec("echo", &[]).unwrap();
    assert!(matches!(
        connection.exec("echo", &[]),
        Err(Error::ChannelRejected(ChannelFailure::ConnectFailed))
    ));
    assert_eq!(connection.statistics().started_sessions, 6);
    assert_eq!(connection.statistics().opened_sessions, 5);
    first.close().unwrap();
    second.close().unwrap();
    connection.disconnect().unwrap();
}

#[test]
fn memory_password_and_multiround_callback_failures() {
    let engine = Engine::new().unwrap();
    assert!(matches!(
        start(
            &engine,
            Peer::default(),
            Authentication::Password("错误密码".into()),
            None
        ),
        Err(Error::Authentication)
    ));
    assert!(matches!(
        start(
            &engine,
            Peer::default(),
            password(),
            Some(HostKeyCheck::Fingerprint("SHA256:错误".into()))
        ),
        Err(Error::HostKeyRejected)
    ));
    let rounds = Arc::new(Mutex::new(0));
    let count = rounds.clone();
    let auth = Authentication::KeyboardInteractive(Arc::new(move |challenge| {
        *count.lock().unwrap() += 1;
        assert_eq!(challenge.prompts.len(), 1);
        assert!(!challenge.prompts[0].echo);
        Some(vec![
            if challenge.prompts[0].text == "Password:" {
                "password"
            } else {
                "123456"
            }
            .into(),
        ])
    }));
    let connection = start(&engine, Peer::default(), auth, None).unwrap();
    assert_eq!(*rounds.lock().unwrap(), 2);
    std::thread::sleep(Duration::from_millis(250));
    connection.ping().unwrap();
    for callback in [
        Arc::new(|_| None) as crate::auth::Answers,
        Arc::new(|_| Some(vec![])),
    ] {
        assert!(matches!(
            start(
                &engine,
                Peer::default(),
                Authentication::KeyboardInteractive(callback),
                None
            ),
            Err(Error::Callback)
        ));
    }
}

#[test]
fn memory_pty_streamlocal_sftp_and_rekey_content() {
    let engine = Engine::new().unwrap();
    let sizes = Arc::new(Mutex::new(Vec::new()));
    let connection = start(
        &engine,
        Peer {
            received_sizes: sizes.clone(),
            ..Default::default()
        },
        password(),
        None,
    )
    .unwrap();
    let mut shell = connection.shell(&Pty::default(), &[]).unwrap();
    shell
        .resize(&Pty {
            rows: 50,
            columns: 120,
            ..Pty::default()
        })
        .unwrap();
    shell.stdin.write_all(b"echo").unwrap();
    let mut echoed = [0; 4];
    shell.stdout.read_exact(&mut echoed).unwrap();
    assert_eq!(&echoed, b"echo");
    shell.stdin.shutdown().unwrap();
    assert_eq!(shell.wait().unwrap(), Some(0));
    assert_eq!(*sizes.lock().unwrap(), vec![(80, 24), (120, 50)]);
    drop(shell);
    let sftp = connection.sftp_handshake().unwrap();
    assert_eq!(sftp.version, 3);
    drop(sftp);
    let mut stream = connection.direct_streamlocal("/测试.sock").unwrap();
    let mut buffer = vec![0; 16 * 1024];
    for index in 0..128 {
        let data = vec![index as u8; buffer.len()];
        stream.write_all(&data).unwrap();
        stream.flush().unwrap();
        stream.read_exact(&mut buffer).unwrap();
        assert_eq!(buffer, data);
    }
    stream.shutdown().unwrap();
    assert!(connection.statistics().key_exchanges > 2);
    eprintln!(
        "内存通道：双向各 2097152 字节校验通过，累计换钥 {} 次（含初次握手）",
        connection.statistics().key_exchanges
    );
    let connection = start(
        &engine,
        Peer {
            malformed_sftp: true,
            ..Default::default()
        },
        password(),
        None,
    )
    .unwrap();
    assert!(matches!(connection.sftp_handshake(), Err(Error::Protocol)));
}

#[test]
fn memory_unread_channel_isolated_bounded_and_complete_with_rekeys() {
    for window in [256 * 1024, 2 * 1024 * 1024] {
        let engine = Engine::new().unwrap();
        let connection = start_options(
            &engine,
            Peer::default(),
            password(),
            None,
            Duration::from_secs(5),
            window,
        )
        .unwrap();
        let mut flood = connection.exec("mixed-flood", &[]).unwrap();
        let mut echo = connection.exec("echo", &[]).unwrap();
        let before = connection.statistics().key_exchanges;
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(flood.buffered_bytes(), window as usize);
        for _ in 0..16 {
            echo.stdin.write_all(&[b'b'; 8192]).unwrap();
            let mut reply = [0; 8192];
            echo.stdout.read_exact(&mut reply).unwrap();
            assert_eq!(reply, [b'b'; 8192]);
            connection.ping().unwrap();
            assert!(flood.buffered_bytes() <= window as usize);
        }
        assert!(connection.statistics().key_exchanges > before);
        assert_eq!(flood.buffered_bytes(), window as usize);
        let mut out = Vec::new();
        let mut err = Vec::new();
        std::thread::scope(|scope| {
            scope.spawn(|| flood.stderr.read_to_end(&mut err).unwrap());
            flood.stdout.read_to_end(&mut out).unwrap();
        });
        assert_eq!(out, vec![b'x'; 2 * 1024 * 1024]);
        assert_eq!(err, vec![b'x'; 2 * 1024 * 1024]);
        assert_eq!(flood.buffered_bytes(), 0);
        assert_eq!(flood.wait().unwrap(), Some(0));
        echo.close().unwrap();
    }
}

#[test]
fn memory_blocked_writer_errors_on_remote_close_while_other_channel_works() {
    let engine = Engine::new().unwrap();
    let connection = start(
        &engine,
        Peer {
            stop_replenishing: true,
            ..Default::default()
        },
        password(),
        None,
    )
    .unwrap();
    let mut blocked = connection.exec("blocked", &[]).unwrap();
    let mut echo = connection.exec("echo", &[]).unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let result = blocked.stdin.write_all(&vec![0x5a; 8 * 1024 * 1024]);
        done_tx.send(result).unwrap();
    });
    assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
    echo.stdin.write_all(b"ping").unwrap();
    let mut reply = [0; 4];
    echo.stdout.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"ping");
    connection.ping().unwrap();
    let result = done_rx.recv_timeout(Duration::from_secs(3));
    if result.is_err() {
        connection.disconnect().unwrap();
    }
    assert!(result.unwrap().is_err());
    writer.join().unwrap();
    echo.stdin.shutdown().unwrap();
    assert_eq!(echo.wait().unwrap(), Some(0));
    connection.ping().unwrap();
}

#[test]
fn memory_idle_reads_tunnel_and_exit_wait_outlive_request_timeout() {
    let engine = Engine::new().unwrap();
    let connection = start_options(
        &engine,
        Peer::default(),
        password(),
        None,
        Duration::from_millis(100),
        256 * 1024,
    )
    .unwrap();
    let mut session = connection.exec("delayed", &[]).unwrap();
    let mut out = Vec::new();
    session.stdout.read_to_end(&mut out).unwrap();
    assert_eq!(out, b"late");
    assert_eq!(session.wait().unwrap(), Some(9));
    let mut session = connection.exec("delayed", &[]).unwrap();
    assert_eq!(session.wait().unwrap(), Some(9));
    let mut tunnel = connection.direct_streamlocal("delayed").unwrap();
    let mut out = Vec::new();
    tunnel.read_to_end(&mut out).unwrap();
    assert_eq!(out, b"late");
}

#[test]
fn memory_disconnect_cancels_reads_tunnels_and_exit_wait() {
    let engine = Engine::new().unwrap();
    let connection = start(&engine, Peer::default(), password(), None).unwrap();
    let mut session = connection.exec("echo", &[]).unwrap();
    let mut tunnel = connection.direct_streamlocal("echo").unwrap();
    std::thread::scope(|scope| {
        let read = scope.spawn(|| session.stdout.read(&mut [0; 1]));
        let tunnel_read = scope.spawn(|| tunnel.read(&mut [0; 1]));
        std::thread::sleep(Duration::from_millis(50));
        connection.disconnect().unwrap();
        assert!(read.join().unwrap().is_err());
        assert!(tunnel_read.join().unwrap().is_err());
    });
    assert_eq!(session.wait(), Err(Error::Disconnected));
}

#[test]
fn memory_local_close_cancels_output_and_wait_without_closing_connection() {
    let engine = Engine::new().unwrap();
    let connection = start(&engine, Peer::default(), password(), None).unwrap();
    let mut session = connection.exec("echo", &[]).unwrap();
    session.close().unwrap();
    assert!(session.stdout.read(&mut [0; 1]).is_err());
    assert_eq!(session.wait(), Err(Error::Disconnected));
    connection.ping().unwrap();
}

#[test]
fn memory_handshake_timeout_and_last_owner_closes_transport() {
    let engine = Engine::new().unwrap();
    let (client, _peer) = tokio::io::duplex(1024);
    let mut options = ConnectionOptions::new(
        "内存",
        22,
        "测试",
        HostKeyCheck::Callback(Arc::new(|_| true)),
    );
    options.connect_timeout = Duration::from_millis(20);
    assert!(matches!(
        engine.connect_stream(options, password(), client, None),
        Err(Error::Timeout("SSH 握手"))
    ));
    let (guard, closed) = tokio::sync::oneshot::channel();
    let (stream, mut peer) = tokio::io::duplex(1024);
    let mut transport = super::connection::Transport { stream, closed };
    drop(guard);
    engine
        .executor
        .run(Duration::from_secs(1), "关闭传输", async {
            use tokio::io::AsyncReadExt;
            assert!(transport.read_u8().await.is_err());
            drop(transport);
            assert!(peer.read_u8().await.is_err());
            Ok(())
        })
        .unwrap();
}

#[test]
fn memory_three_jump_levels_verify_independent_hostkeys() {
    let engine = Engine::new().unwrap();
    let root = start(&engine, Peer::default(), password(), None).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut parent = root;
    for (host, seed) in [("hop2", 2), ("hop3", 3), ("hop4", 4)] {
        let key: PrivateKey = Ed25519Keypair::from_seed(&[seed; 32]).into();
        let expected = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        let observed = seen.clone();
        let options = ConnectionOptions::new(
            host,
            22,
            "测试",
            HostKeyCheck::Callback(Arc::new(move |key| {
                observed.lock().unwrap().push(key.host);
                key.fingerprint == expected
            })),
        );
        parent = engine.connect_via(&parent, options, password()).unwrap();
    }
    assert_eq!(*seen.lock().unwrap(), ["hop2", "hop3", "hop4"]);
    parent.ping().unwrap();
    let bad = ConnectionOptions::new(
        "hop2",
        22,
        "测试",
        HostKeyCheck::Fingerprint("SHA256:错误".into()),
    );
    assert!(matches!(
        engine.connect_via(&parent, bad, password()),
        Err(Error::HostKeyRejected)
    ));
}

#[test]
fn memory_remote_forward_receive_and_cancel() {
    let engine = Engine::new().unwrap();
    let connection = start(&engine, Peer::default(), password(), None).unwrap();
    let port = connection.remote_forward(0).unwrap();
    assert_eq!(port, 32123);
    let event = connection.accept_forward().unwrap();
    assert_eq!(event.connected_port, port);
    assert_eq!(
        (event.originator_address.as_str(), event.originator_port),
        ("127.0.0.1", 12345)
    );
    let mut incoming = event.stream;
    let mut data = [0; 9];
    incoming.read_exact(&mut data).unwrap();
    assert_eq!(&data, b"forwarded");
    incoming.write_all(b"pong").unwrap();
    connection.cancel_forward(port).unwrap();
}

#[test]
fn memory_raw_pending_data_does_not_block_controls_and_is_dropped_on_close() {
    struct RawClient;
    impl russh::client::Handler for RawClient {
        type Error = russh::Error;
        async fn check_server_key(
            &mut self,
            _: &russh::keys::PublicKeyOrCertificate,
        ) -> std::result::Result<bool, Self::Error> {
            Ok(true)
        }
    }
    let engine = Engine::new().unwrap();
    engine.executor.handle().unwrap().block_on(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::time::timeout(Duration::from_secs(5), async {
            let key: PrivateKey = Ed25519Keypair::from_seed(&[7; 32]).into();
            let mut config = peer_config(key);
            config.window_size = 32 * 1024;
            let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
            let peer = Peer {
                stop_replenishing: true,
                ..Default::default()
            };
            let server = tokio::spawn(async move {
                let session = server::run_stream(Arc::new(config), server_stream, peer)
                    .await
                    .unwrap();
                let _ = session.await;
            });
            let mut client = russh::client::connect_stream(
                Arc::new(russh::client::Config::default()),
                client_stream,
                RawClient,
            )
            .await
            .unwrap();
            assert!(
                client
                    .authenticate_password("测试", "测试密码")
                    .await
                    .unwrap()
                    .success()
            );
            let mut blocked = client.channel_open_session().await.unwrap();
            blocked.exec(true, "blocked").await.unwrap();
            crate::channel::ack(&mut blocked, "测试").await.unwrap();
            let mut echo = client.channel_open_session().await.unwrap();
            echo.exec(true, "echo").await.unwrap();
            crate::channel::ack(&mut echo, "测试").await.unwrap();
            // 低层 Handle::data 绕过写入器的预留额度，确保实际进入 russh 的 pending_data。
            client
                .data(blocked.id(), vec![0x5a; 8 * 1024 * 1024])
                .await
                .unwrap();
            let mut blocked_writer = blocked.make_writer();
            let mut blocked_write = Box::pin(blocked_writer.write_all(&[0x5a; 64 * 1024]));
            assert!(
                tokio::time::timeout(Duration::from_millis(80), &mut blocked_write)
                    .await
                    .is_err()
            );
            let mut stream = echo.into_stream();
            stream.write_all(b"ping").await.unwrap();
            let mut reply = [0; 4];
            stream.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"ping");
            client.send_ping().await.unwrap();
            // 全局请求及其他通道的 EOF 在 A 积压期间也必须发出。
            assert_eq!(client.tcpip_forward("127.0.0.1", 0).await.unwrap(), 32123);
            client
                .cancel_tcpip_forward("127.0.0.1", 32123)
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
            assert_eq!(stream.read(&mut [0; 1]).await.unwrap(), 0);
            assert!(blocked_write.await.is_err());
            assert!(blocked_writer.write(b"retry").await.is_err());
            client.send_ping().await.unwrap();
            client
                .disconnect(russh::Disconnect::ByApplication, "测试结束", "")
                .await
                .unwrap();
            drop(client);
            server.await.unwrap();
        })
        .await
        .expect("积压通道阻塞了连接或控制消息");
    });
}

#[test]
fn memory_handler_only_receiver_keeps_consuming_after_window_exhaustion() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CallbackClient {
        bytes: Arc<AtomicUsize>,
        done: Option<tokio::sync::oneshot::Sender<()>>,
    }
    impl russh::client::Handler for CallbackClient {
        type Error = russh::Error;
        async fn check_server_key(
            &mut self,
            _: &russh::keys::PublicKeyOrCertificate,
        ) -> std::result::Result<bool, Self::Error> {
            Ok(true)
        }
        async fn data(
            &mut self,
            _: ChannelId,
            data: &[u8],
            _: &mut russh::client::Session,
        ) -> std::result::Result<(), Self::Error> {
            self.bytes.fetch_add(data.len(), Ordering::Relaxed);
            Ok(())
        }
        async fn channel_close(
            &mut self,
            _: ChannelId,
            _: &mut russh::client::Session,
        ) -> std::result::Result<(), Self::Error> {
            if let Some(done) = self.done.take() {
                let _ = done.send(());
            }
            Ok(())
        }
    }
    let engine = Engine::new().unwrap();
    engine.executor.handle().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
            let config = peer_config(Ed25519Keypair::from_seed(&[7; 32]).into());
            let server = tokio::spawn(async move {
                let session = server::run_stream(Arc::new(config), server_stream, Peer::default())
                    .await
                    .unwrap();
                let _ = session.await;
            });
            let bytes = Arc::new(AtomicUsize::new(0));
            let (done, finished) = tokio::sync::oneshot::channel();
            let handler = CallbackClient {
                bytes: bytes.clone(),
                done: Some(done),
            };
            let config = russh::client::Config {
                window_size: 32 * 1024,
                ..Default::default()
            };
            let mut client =
                russh::client::connect_stream(Arc::new(config), client_stream, handler)
                    .await
                    .unwrap();
            assert!(
                client
                    .authenticate_password("测试", "测试密码")
                    .await
                    .unwrap()
                    .success()
            );
            let (reader, writer) = client.channel_open_session().await.unwrap().split();
            drop(reader);
            writer.exec(true, "flood").await.unwrap();
            finished.await.unwrap();
            assert_eq!(bytes.load(Ordering::Relaxed), 4 * 1024 * 1024);
            client
                .disconnect(russh::Disconnect::ByApplication, "测试结束", "")
                .await
                .unwrap();
            drop(client);
            server.await.unwrap();
        })
        .await
        .expect("回调接收数据未补窗口");
    });
}

#[test]
fn memory_close_handles_interrupt_pending_reads_tunnels_and_exit_wait() {
    let engine = Engine::new().unwrap();
    let connection = start(
        &engine,
        Peer {
            stop_replenishing: true,
            ..Default::default()
        },
        password(),
        None,
    )
    .unwrap();
    for read_output in [true, false] {
        let mut session = connection.exec("echo", &[]).unwrap();
        let closer = session.closer();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = if read_output {
                session.stdout.read(&mut [0; 1]).map(|_| ())
            } else {
                session.wait().map(|_| ()).map_err(crate::error::io_error)
            };
            let _ = done_tx.send(result);
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        closer.close().unwrap();
        let result = done_rx.recv_timeout(Duration::from_secs(1));
        if result.is_err() {
            connection.disconnect().unwrap();
        }
        assert!(result.unwrap().is_err());
        worker.join().unwrap();
        connection.ping().unwrap();
    }
    let mut tunnel = connection.direct_streamlocal("echo").unwrap();
    let closer = tunnel.closer();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = done_tx.send(tunnel.read(&mut [0; 1]));
    });
    assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
    closer.close().unwrap();
    let result = done_rx.recv_timeout(Duration::from_secs(1));
    if result.is_err() {
        connection.disconnect().unwrap();
    }
    assert!(result.unwrap().is_err());
    worker.join().unwrap();
    let tunnel = connection.direct_streamlocal("echo").unwrap();
    tunnel.close().unwrap();
    let mut blocked = connection.exec("blocked", &[]).unwrap();
    let closer = blocked.closer();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = done_tx.send(blocked.stdin.write_all(&vec![0x5a; 8 * 1024 * 1024]));
    });
    assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
    closer.close().unwrap();
    let result = done_rx.recv_timeout(Duration::from_millis(100));
    if result.is_err() {
        connection.disconnect().unwrap();
    }
    assert!(result.unwrap().is_err());
    worker.join().unwrap();
    connection.ping().unwrap();
}

#[test]
fn memory_sftp_handshake_retains_request_timeout_and_releases_channel() {
    let engine = Engine::new().unwrap();
    let connection = start_options(
        &engine,
        Peer {
            silent_sftp: true,
            ..Default::default()
        },
        password(),
        None,
        Duration::from_millis(100),
        256 * 1024,
    )
    .unwrap();
    assert!(matches!(
        connection.sftp_handshake(),
        Err(Error::Timeout("SFTP 握手"))
    ));
    connection.ping().unwrap();
}

#[test]
fn memory_connection_lifetime_probe_cancelled_open_signal_and_nonblocking_cancel() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let engine = Engine::new().unwrap();
    let connection = start(&engine, Peer::default(), password(), None).unwrap();
    assert!(!connection.is_closed());
    assert!(connection.is_exclusively_owned());
    let duplicate = connection.clone();
    assert!(connection.same_connection(&duplicate));
    assert!(!connection.is_exclusively_owned());
    drop(duplicate);
    assert_eq!(connection.window_size(), 256 * 1024);
    assert_eq!(connection.probe(Duration::ZERO), Err(Error::InvalidInput));
    connection.probe(Duration::from_secs(1)).unwrap();
    let cancelled = Arc::new(AtomicBool::new(true));
    assert!(matches!(
        connection.exec_cancellable("echo", &[], cancelled.clone(), Duration::from_secs(1)),
        Err(Error::Callback)
    ));
    cancelled.store(false, Ordering::Release);
    let mut session = connection
        .exec_cancellable("signal", &[], cancelled, Duration::from_secs(1))
        .unwrap();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    session.stdout.read_to_end(&mut stdout).unwrap();
    session.stderr.read_to_end(&mut stderr).unwrap();
    let status = session.wait_status().unwrap();
    assert_eq!(status.code, None);
    assert_eq!(status.signal.as_deref(), Some("TERM"));
    assert_eq!(connection.statistics().opened_sessions, 1);
    drop(session);
    let mut session = connection.exec("idle", &[]).unwrap();
    session.closer().cancel();
    assert!(session.stdout.read(&mut [0]).is_err());
    assert!(session.wait_status().is_err());
    connection.probe(Duration::from_secs(1)).unwrap();
    connection.abort();
    assert!(connection.is_closed());
    assert_eq!(
        connection.probe(Duration::from_secs(1)),
        Err(Error::Disconnected)
    );
    connection
        .wait_disconnected(Duration::from_secs(1))
        .unwrap();
}

#[test]
fn 子系统仅开流且支持超时参数与取消() {
    let engine = Engine::new().unwrap();
    let connection = start(&engine, Peer::default(), password(), None).unwrap();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(matches!(
        connection.sftp_subsystem(cancel.clone(), Duration::ZERO),
        Err(Error::InvalidInput)
    ));
    let mut session = connection
        .sftp_subsystem(cancel.clone(), Duration::from_secs(1))
        .unwrap();
    assert!(session.try_wait_status().is_none());
    session
        .stdin
        .write_all(&[0, 0, 0, 5, 1, 0, 0, 0, 3])
        .unwrap();
    let mut response = [0; 9];
    session.stdout.read_exact(&mut response).unwrap();
    assert_eq!(response, [0, 0, 0, 5, 2, 0, 0, 0, 3]);
    drop(session);
    cancel.store(true, std::sync::atomic::Ordering::Release);
    assert!(matches!(
        connection.sftp_subsystem(cancel, Duration::from_secs(1)),
        Err(Error::Callback)
    ));
    connection.ping().unwrap();
}
