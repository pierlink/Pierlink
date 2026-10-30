use super::*;
use crate::{
    Engine,
    memory_peer::{self, Peer},
};

#[test]
fn socks握手和请求共用十秒期限且不影响已连通客户端() {
    let engine = Engine::new().unwrap();
    let connection =
        memory_peer::start(&engine, Peer::default(), memory_peer::password(), None).unwrap();
    assert_eq!(OPEN_TIMEOUT, Duration::from_secs(10));
    engine.executor.handle().unwrap().block_on(async {
        let mut stalled = JoinSet::new();
        for prefix in [
            vec![],
            vec![5],
            vec![5, 2, 0],
            vec![5, 1, 0],
            vec![5, 1, 0, 5, 1],
            vec![5, 1, 0, 5, 1, 0, 3, 4, b't'],
            vec![5, 1, 0, 5, 1, 0, 1, 127, 0, 0, 1, 0],
        ] {
            let connection = connection.clone();
            stalled.spawn(async move {
                let (mut client, mut socket) = tokio::io::duplex(512);
                let state = Shared::default();
                let started = tokio::time::Instant::now();
                let exchange = async {
                    // 延迟发送完整握手，确认开始等请求时没有重新计算十秒。
                    if prefix == [5, 1, 0] {
                        tokio::time::sleep(Duration::from_secs(6)).await;
                    }
                    client.write_all(&prefix).await.unwrap();
                    let mut response = Vec::new();
                    client.read_to_end(&mut response).await.unwrap();
                    assert!(response.is_empty() || response == [5, 0]);
                };
                let forward = async {
                    client_connection(&connection, &mut socket, None, &state).await;
                    drop(socket);
                };
                tokio::time::timeout(OPEN_TIMEOUT + Duration::from_secs(1), async {
                    tokio::join!(exchange, forward);
                })
                .await
                .expect("不完整的 SOCKS 客户端必须在十秒期限后释放");
                assert!(started.elapsed() >= OPEN_TIMEOUT);
                assert!(state.lock().unwrap().client_error.is_none());
            });
        }
        let (mut client, mut socket) = tokio::io::duplex(512);
        let state = Shared::default();
        let forward = async {
            client_connection(&connection, &mut socket, None, &state).await;
        };
        let healthy = async {
            client
                .write_all(b"\x05\x01\x00\x05\x01\x00\x03\x04echo\x00\x50")
                .await
                .unwrap();
            let mut reply = [0; 12];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, [5, 0, 5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
            while let Some(result) = stalled.join_next().await {
                result.unwrap();
            }
            client.write_all(b"still-alive").await.unwrap();
            let mut reply = [0; 11];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"still-alive");
            client.shutdown().await.unwrap();
        };
        tokio::time::timeout(OPEN_TIMEOUT + Duration::from_secs(2), async {
            tokio::join!(healthy, forward);
        })
        .await
        .unwrap();
    });
    connection.ping().unwrap();
}

#[test]
fn 内存套接字转发双向半关闭及拒绝保留目标类型() {
    let engine = Engine::new().unwrap();
    for failure in [
        None,
        Some(russh::ChannelOpenFailure::AdministrativelyProhibited),
        Some(russh::ChannelOpenFailure::ConnectFailed),
    ] {
        let connection = memory_peer::start(
            &engine,
            Peer {
                streamlocal_failure: failure.clone(),
                ..Default::default()
            },
            memory_peer::password(),
            None,
        )
        .unwrap();
        let state = Shared::default();
        engine.executor.handle().unwrap().block_on(async {
            let (mut client, mut socket) = tokio::io::duplex(512);
            let exchange = async {
                client.write_all(b"socket-data").await.unwrap();
                client.shutdown().await.unwrap();
                let mut bytes = Vec::new();
                client.read_to_end(&mut bytes).await.unwrap();
                if failure.is_none() {
                    assert_eq!(bytes, b"socket-data");
                } else {
                    assert!(bytes.is_empty());
                }
            };
            let forward = async {
                client_connection(
                    &connection,
                    &mut socket,
                    Some(ForwardingTarget::Unix {
                        path: "/private/api.sock".into(),
                    }),
                    &state,
                )
                .await;
                drop(socket);
            };
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(exchange, forward);
            })
            .await
            .unwrap();
        });
        let expected =
            failure.map(|reason| (Error::from(russh::Error::ChannelOpenFailure(reason)), true));
        assert_eq!(state.lock().unwrap().client_error, expected);
        assert!(!connection.is_closed());
    }
}

#[test]
fn socks地址命令和认证协商有界且拒绝畸形输入() {
    let engine = Engine::new().unwrap();
    engine.executor.handle().unwrap().block_on(async {
        for (address, expected) in [
            (vec![1, 127, 0, 0, 1], "127.0.0.1"),
            (
                [vec![3, 12], b"example.test".to_vec()].concat(),
                "example.test",
            ),
            (
                [vec![4], Ipv6Addr::LOCALHOST.octets().to_vec()].concat(),
                "::1",
            ),
        ] {
            let (mut client, mut server) = tokio::io::duplex(512);
            client
                .write_all(&[&[5, 2, 2, 0, 5, 1, 0][..], &address, &[0, 80]].concat())
                .await
                .unwrap();
            let ForwardingTarget::Tcp { host, port } = socks_target(&mut server).await.unwrap()
            else {
                panic!("必须为 TCP")
            };
            assert_eq!((host.as_str(), port), (expected, 80));
            let mut response = [0; 2];
            client.read_exact(&mut response).await.unwrap();
            assert_eq!(response, [5, 0]);
        }
        for (request, response) in [
            (vec![5, 1, 2], vec![5, 255]),
            (vec![5, 0], vec![5, 255]),
            (
                vec![5, 1, 0, 5, 2, 0, 1],
                vec![5, 0, 5, 7, 0, 1, 0, 0, 0, 0, 0, 0],
            ),
            (
                vec![5, 1, 0, 5, 3, 0, 1],
                vec![5, 0, 5, 7, 0, 1, 0, 0, 0, 0, 0, 0],
            ),
            (
                vec![5, 1, 0, 5, 1, 0, 9],
                vec![5, 0, 5, 8, 0, 1, 0, 0, 0, 0, 0, 0],
            ),
            (
                vec![5, 1, 0, 5, 1, 0, 3, 0],
                vec![5, 0, 5, 8, 0, 1, 0, 0, 0, 0, 0, 0],
            ),
            (
                vec![5, 1, 0, 5, 1, 0, 3, 1, 0],
                vec![5, 0, 5, 8, 0, 1, 0, 0, 0, 0, 0, 0],
            ),
            (
                vec![5, 1, 0, 5, 1, 1, 1],
                vec![5, 0, 5, 1, 0, 1, 0, 0, 0, 0, 0, 0],
            ),
            (
                vec![5, 1, 0, 5, 1, 0, 1, 127, 0, 0, 1, 0, 0],
                vec![5, 0, 5, 1, 0, 1, 0, 0, 0, 0, 0, 0],
            ),
        ] {
            let (mut client, mut server) = tokio::io::duplex(512);
            client.write_all(&request).await.unwrap();
            assert!(socks_target(&mut server).await.is_err());
            let mut actual = vec![0; response.len()];
            client.read_exact(&mut actual).await.unwrap();
            assert_eq!(actual, response);
        }
        let (mut client, mut server) = tokio::io::duplex(512);
        client.write_all(&[5, 1]).await.unwrap();
        client.shutdown().await.unwrap();
        assert!(socks_target(&mut server).await.is_err());
    });
}

#[test]
fn 内存对端拒绝目标时socks返回明确错误且终端仍可使用() {
    use std::io::{Read, Write};
    let engine = Engine::new().unwrap();
    let connection =
        memory_peer::start(&engine, Peer::default(), memory_peer::password(), None).unwrap();
    engine.executor.handle().unwrap().block_on(async {
        let (mut client, mut server) = tokio::io::duplex(512);
        client
            .write_all(&[5, 1, 0, 5, 1, 0, 1, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap();
        assert!(matches!(
            socks_connect(&mut server, &connection).await,
            Err(Error::ChannelRejected(ChannelFailure::Prohibited))
        ));
        let mut response = [0; 12];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(response, [5, 0, 5, 2, 0, 1, 0, 0, 0, 0, 0, 0]);
    });
    let mut terminal = connection.shell(&crate::Pty::default(), &[]).unwrap();
    terminal.stdin.write_all(b"ping").unwrap();
    let mut bytes = [0; 4];
    terminal.stdout.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"ping");
}

#[test]
fn 内存远程监听就绪撤销重复保护和断线状态() {
    let engine = Engine::new().unwrap();
    let connection = memory_peer::start(
        &engine,
        Peer {
            stop_replenishing: true,
            ..Default::default()
        },
        memory_peer::password(),
        None,
    )
    .unwrap();
    assert!(matches!(
        connection.forwarding(Vec::new()),
        Err(Error::InvalidInput)
    ));
    assert!(matches!(
        connection.forwarding(vec![ForwardingRule::Remote {
            remote_port: 0,
            local_port: 80
        }]),
        Err(Error::InvalidInput)
    ));
    let service = connection
        .forwarding(vec![ForwardingRule::Remote {
            remote_port: 32123,
            local_port: 80,
        }])
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !service.status().unwrap() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(service.take_client_error().is_none());
    let duplicate = connection
        .forwarding(vec![ForwardingRule::Remote {
            remote_port: 32123,
            local_port: 81,
        }])
        .unwrap();
    while duplicate.status().is_ok() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(duplicate);
    assert!(service.status().unwrap());
    drop(service);
    assert!(connection.inner.routed.lock().unwrap().is_empty());
    let service = connection
        .forwarding(vec![ForwardingRule::Remote {
            remote_port: 32123,
            local_port: 80,
        }])
        .unwrap();
    while !service.status().unwrap() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    connection.abort();
    assert_eq!(service.status(), Err(Error::Disconnected));
}

#[test]
fn 内存本地与动态转发双向传输半关闭和客户端背压隔离() {
    use std::io::{Read, Write};
    let engine = Engine::new().unwrap();
    let connection =
        memory_peer::start(&engine, Peer::default(), memory_peer::password(), None).unwrap();
    let state = Arc::new(Mutex::new(State::default()));
    for dynamic in [false, true] {
        engine.executor.handle().unwrap().block_on(async {
            let (mut client, mut socket) = tokio::io::duplex(1024);
            let task_connection = connection.clone();
            let task_state = state.clone();
            let task = tokio::spawn(async move {
                client_connection(
                    &task_connection,
                    &mut socket,
                    (!dynamic).then(|| ForwardingTarget::Tcp {
                        host: "echo".into(),
                        port: 80,
                    }),
                    &task_state,
                )
                .await;
            });
            if dynamic {
                client
                    .write_all(b"\x05\x01\x00\x05\x01\x00\x03\x04echo\x00\x50")
                    .await
                    .unwrap();
                let mut response = [0; 12];
                client.read_exact(&mut response).await.unwrap();
                assert_eq!(response, [5, 0, 5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
            }
            client.write_all(b"half-close").await.unwrap();
            client.shutdown().await.unwrap();
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(3), client.read_to_end(&mut response))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response, b"half-close");
            task.await.unwrap();
        });
    }
    let (paused, mut socket) = tokio::io::duplex(1024);
    let task_connection = connection.clone();
    let task_state = state.clone();
    let task = engine.executor.handle().unwrap().spawn(async move {
        client_connection(
            &task_connection,
            &mut socket,
            Some(ForwardingTarget::Tcp {
                host: "flood".into(),
                port: 80,
            }),
            &task_state,
        )
        .await;
    });
    std::thread::sleep(Duration::from_millis(100));
    let received = connection.statistics().received_bytes;
    assert!(received >= u64::from(connection.window_size()));
    let mut terminal = connection.shell(&crate::Pty::default(), &[]).unwrap();
    for _ in 0..20 {
        let start = std::time::Instant::now();
        terminal.stdin.write_all(b"ping").unwrap();
        let mut response = [0; 4];
        terminal.stdout.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"ping");
        assert!(start.elapsed() < Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(10));
    }
    // 停读只能占用一个通道窗口和有限的搬运缓冲，不能持续积压服务端输出。
    assert!(connection.statistics().received_bytes - received < 32 * 1024);
    drop(paused);
    task.abort();
    assert!(state.lock().unwrap().client_error.is_none());
    connection.ping().unwrap();
}

#[test]
fn 取消打开转发后迟到通道仍自动关闭且不阻塞终端() {
    let engine = Engine::new().unwrap();
    let closed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connection = memory_peer::start(
        &engine,
        Peer {
            closed_channels: closed.clone(),
            ..Default::default()
        },
        memory_peer::password(),
        None,
    )
    .unwrap();
    engine.executor.handle().unwrap().block_on(async {
        let result = tokio::time::timeout(
            Duration::from_millis(10),
            open(
                &connection,
                &ForwardingTarget::Tcp {
                    host: "late-echo".into(),
                    port: 80,
                },
            ),
        )
        .await;
        assert!(result.is_err());
    });
    connection.ping().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while closed.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(connection.inner.pending_channels.available_permits(), 128);
    assert!(!connection.is_closed());
}

#[test]
fn 远程全局请求拒绝回收登记且不关闭主连接() {
    let engine = Engine::new().unwrap();
    let connection = memory_peer::start(
        &engine,
        Peer {
            reject_forwarding: true,
            ..Default::default()
        },
        memory_peer::password(),
        None,
    )
    .unwrap();
    let service = connection
        .forwarding(vec![ForwardingRule::Remote {
            remote_port: 32123,
            local_port: 80,
        }])
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Err(error) = service.status() {
            assert_eq!(error, Error::RequestRejected("远程转发"));
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(service);
    assert!(connection.inner.routed.lock().unwrap().is_empty());
    connection.ping().unwrap();
}

#[test]
fn 撤销远程监听时尚未消费的来访通道自动关闭() {
    let engine = Engine::new().unwrap();
    let closed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connection = memory_peer::start(
        &engine,
        Peer {
            closed_channels: closed.clone(),
            forwarded_flood: true,
            ..Default::default()
        },
        memory_peer::password(),
        None,
    )
    .unwrap();
    engine.executor.handle().unwrap().block_on(async {
        let mut ports = RemotePorts {
            connection: connection.clone(),
            ports: Vec::new(),
        };
        let incoming = ports.register(32123).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while incoming.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(incoming);
        ports.cleanup().await;
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while closed.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(connection.inner.routed.lock().unwrap().is_empty());
    connection.ping().unwrap();
}
