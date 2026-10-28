//! Pierlink 补丁修订的流量控制回归；内存对端无需监听端口或落盘。
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct RawClient {
    exchanges: Arc<AtomicUsize>,
}
impl russh::client::Handler for RawClient {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        _: &russh::keys::PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(true)
    }
    async fn kex_done(
        &mut self,
        _: Option<&[u8]>,
        _: &russh::Names,
        _: &mut russh::client::Session,
    ) -> std::result::Result<(), Self::Error> {
        self.exchanges.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

async fn raw_pair(
    peer: Peer,
    config: russh::client::Config,
) -> (
    russh::client::Handle<RawClient>,
    tokio::task::JoinHandle<()>,
    Arc<AtomicUsize>,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let config_server = peer_config(Ed25519Keypair::from_seed(&[7; 32]).into());
    let task = tokio::spawn(async move {
        let session = server::run_stream(Arc::new(config_server), server, peer)
            .await
            .unwrap();
        let _ = session.await;
    });
    let handler = RawClient::default();
    let exchanges = handler.exchanges.clone();
    let mut client = russh::client::connect_stream(Arc::new(config), client, handler)
        .await
        .unwrap();
    assert!(
        client
            .authenticate_password("测试", "测试密码")
            .await
            .unwrap()
            .success()
    );
    (client, task, exchanges)
}

async fn finish(client: russh::client::Handle<RawClient>, task: tokio::task::JoinHandle<()>) {
    client
        .disconnect(russh::Disconnect::ByApplication, "测试结束", "")
        .await
        .unwrap();
    drop(client);
    task.await.unwrap();
}

#[test]
fn 小传输缓冲下终端大段粘贴与另一通道下载换钥不互等() {
    let engine = Engine::new().unwrap();
    engine.executor.handle().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(60), async {
            let (client, task, exchanges) =
                raw_pair(Peer::default(), russh::client::Config::default()).await;
            let mut terminal = client.channel_open_session().await.unwrap();
            terminal
                .request_pty(true, "xterm-256color", 80, 24, 0, 0, &[])
                .await
                .unwrap();
            crate::channel::ack(&mut terminal, "终端尺寸")
                .await
                .unwrap();
            terminal.request_shell(true).await.unwrap();
            crate::channel::ack(&mut terminal, "终端登录")
                .await
                .unwrap();
            let mut download = client.channel_open_session().await.unwrap();
            download.exec(true, "flood-large").await.unwrap();
            crate::channel::ack(&mut download, "并行下载")
                .await
                .unwrap();
            let before = exchanges.load(Ordering::Relaxed);
            let (mut reader, writer) = terminal.split();
            // 每块内容不同，回显逐字节比较，覆盖部分写入后的重复、丢失与乱序。
            let pasted: Vec<u8> = (0..8 * 1024 * 1024)
                .map(|index| ((index + index / (64 * 1024)) % 251) as u8)
                .collect();
            let upload = async {
                let mut tx = writer.make_writer();
                for block in pasted.chunks(64 * 1024) {
                    tx.write_all(block).await.unwrap();
                }
            };
            let echo = async {
                let mut actual = vec![0; pasted.len()];
                reader.make_reader().read_exact(&mut actual).await.unwrap();
                assert_eq!(actual, pasted);
            };
            let receive = async {
                let mut actual = Vec::new();
                download
                    .make_reader()
                    .read_to_end(&mut actual)
                    .await
                    .unwrap();
                assert_eq!(actual, vec![b'x'; 8 * 1024 * 1024]);
            };
            tokio::join!(upload, echo, receive);
            assert!(exchanges.load(Ordering::Relaxed) > before + 10);
            writer.data_bytes(b"still-alive".as_slice()).await.unwrap();
            let mut echoed = [0; 11];
            reader.make_reader().read_exact(&mut echoed).await.unwrap();
            assert_eq!(&echoed, b"still-alive");
            finish(client, task).await;
        })
        .await
        .expect("64 KiB 传输缓冲下双向传输和换钥必须在 60 秒内完成");
    });
}

#[test]
fn 标准输入结束不能截断仍在输出的命令() {
    let engine = Engine::new().unwrap();
    engine.executor.handle().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(30), async {
            let (client, task, _) = raw_pair(
                Peer::default(),
                russh::client::Config {
                    window_size: 256 * 1024,
                    ..Default::default()
                },
            )
            .await;
            let mut channel = client.channel_open_session().await.unwrap();
            channel.exec(true, "stderr-first").await.unwrap();
            crate::channel::ack(&mut channel, "双输出").await.unwrap();
            let (mut reader, writer) = channel.split();
            let consumption = reader.consumption();
            // 暂停消费直到填满窗口，保证命令收到 stdin EOF 时还没发完输出。
            while consumption.buffered_bytes() < 256 * 1024 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            writer.eof().await.unwrap();
            client.send_ping().await.unwrap();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut status = None;
            loop {
                match reader.wait().await {
                    Some(russh::ChannelMsg::Data { data }) => stdout.extend_from_slice(&data),
                    Some(russh::ChannelMsg::ExtendedData { data, ext: 1 }) => {
                        stderr.extend_from_slice(&data)
                    }
                    Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                        status = Some(exit_status)
                    }
                    Some(russh::ChannelMsg::Close) => break,
                    Some(_) => {}
                    None => panic!("命令输出结束前连接已断开"),
                }
            }
            assert_eq!(stderr.len(), 4 * 1024 * 1024);
            assert!(stderr.iter().all(|byte| *byte == b'x'));
            assert_eq!(stdout, b"stdout");
            assert_eq!(status, Some(0));
            assert_eq!(consumption.buffered_bytes(), 0);
            finish(client, task).await;
        })
        .await
        .expect("命令未能在标准输入结束后继续输出");
    });
}

#[test]
fn memory_download_adjustments_bounded_with_8k_and_4_byte_reads() {
    let engine = Engine::new().unwrap();
    engine.executor.handle().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(30), async {
            // 同时覆盖半窗阈值和最大包长主导的阈值。
            for (window, packet) in [(256 * 1024, 32 * 1024), (32 * 1024, 32 * 1024)] {
                for size in [8192, 4] {
                    let adjustments = Arc::new(AtomicUsize::new(0));
                    let (client, task, _) = raw_pair(
                        Peer {
                            adjustments: adjustments.clone(),
                            ..Default::default()
                        },
                        russh::client::Config {
                            window_size: window,
                            maximum_packet_size: packet,
                            ..Default::default()
                        },
                    )
                    .await;
                    let mut channel = client.channel_open_session().await.unwrap();
                    channel.exec(true, "flood").await.unwrap();
                    crate::channel::ack(&mut channel, "下载").await.unwrap();
                    let mut reader = channel.make_reader();
                    let mut buffer = vec![0; size];
                    let mut bytes = 0;
                    loop {
                        let count = reader.read(&mut buffer).await.unwrap();
                        if count == 0 {
                            break;
                        }
                        assert!(buffer[..count].iter().all(|byte| *byte == b'x'));
                        bytes += count;
                    }
                    assert_eq!(bytes, 4 * 1024 * 1024);
                    client.send_ping().await.unwrap();
                    let count = adjustments.load(Ordering::Relaxed);
                    let per_mib = (1024 * 1024) / (window / 2).max(packet) as usize;
                    assert!(
                        count > 0 && count <= 4 * per_mib,
                        "补窗过细：缓冲 {size}，{count} 条"
                    );
                    finish(client, task).await;
                }
            }
        })
        .await
        .expect("小缓冲下载未能持续补窗");
    });
}

#[test]
fn memory_upload_64m_rekeys_restores_writable_packet_without_window_events() {
    let engine = Engine::new().unwrap();
    engine.executor.handle().unwrap().block_on(async {
        let uploaded = Arc::new(AtomicUsize::new(0));
        let (client, task, exchanges) = raw_pair(
            Peer {
                uploaded: uploaded.clone(),
                ..Default::default()
            },
            russh::client::Config::default(),
        )
        .await;
        let result = tokio::time::timeout(Duration::from_secs(60), async {
            let mut channel = client.channel_open_session().await.unwrap();
            channel.exec(true, "upload").await.unwrap();
            crate::channel::ack(&mut channel, "上传").await.unwrap();
            let (mut reader, writer) = channel.split();
            // 接收队列保持打开且不读取，固定只写通道不能堆积窗口事件的语义。
            let mut tx = writer.make_writer();
            let block = vec![0x5a; 32 * 1024];
            for _ in 0..2048 {
                tx.write_all(&block).await.unwrap();
            }
            client.send_ping().await.unwrap();
            assert_eq!(uploaded.load(Ordering::Relaxed), 64 * 1024 * 1024);
            assert!(
                exchanges.load(Ordering::Relaxed) > 10,
                "64K 限额必须触发多次换钥"
            );
            assert_eq!(writer.writable_packet_size().await, 32 * 1024);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), reader.wait())
                    .await
                    .is_err(),
                "纯窗口调整不能留下队列消息"
            );
        })
        .await;
        assert!(
            result.is_ok(),
            "换钥上传停在 {} 字节，已换钥 {} 次",
            uploaded.load(Ordering::Relaxed),
            exchanges.load(Ordering::Relaxed)
        );
        finish(client, task).await;
    });
}

#[test]
fn memory_stdout_stalls_until_shared_stderr_window_is_consumed() {
    let engine = Engine::new().unwrap();
    let connection = start(&engine, Peer::default(), password(), None).unwrap();
    let mut session = connection.exec("stderr-first", &[]).unwrap();
    let started = std::time::Instant::now();
    while session.buffered_bytes() < 256 * 1024 {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "stderr 未填满共享窗口"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(session.buffered_bytes(), 256 * 1024);
    let closer = session.closer();
    let (done, received) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut stdout = [0; 6];
        let result = session.stdout.read_exact(&mut stdout);
        let _ = done.send((result, stdout));
        session
    });
    assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
    // 其他通道仍能处理控制消息；共享窗口只限制同一通道。
    connection.ping().unwrap();
    closer.close().unwrap();
    assert!(
        received
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .0
            .is_err()
    );
    worker.join().unwrap();
    // 再次执行并消费 stderr，stdout 才能继续。
    let mut session = connection.exec("stderr-first", &[]).unwrap();
    let mut err = Vec::new();
    session.stderr.read_to_end(&mut err).unwrap();
    assert_eq!(err, vec![b'x'; 4 * 1024 * 1024]);
    let mut out = String::new();
    session.stdout.read_to_string(&mut out).unwrap();
    assert_eq!(out, "stdout");
}

#[test]
fn memory_partial_reader_and_read_half_drop_return_credit() {
    let engine = Engine::new().unwrap();
    engine.executor.handle().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (client, task, _) = raw_pair(
                Peer::default(),
                russh::client::Config {
                    window_size: 32 * 1024,
                    ..Default::default()
                },
            )
            .await;
            let mut channel = client.channel_open_session().await.unwrap();
            channel.exec(true, "flood").await.unwrap();
            crate::channel::ack(&mut channel, "读取").await.unwrap();
            let (mut reader, writer) = channel.split();
            let consumption = reader.consumption();
            {
                let mut partial = reader.make_reader();
                assert_eq!(partial.read(&mut [0; 4]).await.unwrap(), 4);
                assert_eq!(consumption.buffered_bytes(), 32 * 1024 - 4);
            }
            // ChannelRx 丢弃余下包后归还额度，下一包仍能到达。
            let msg = reader.wait().await.unwrap();
            assert!(matches!(msg, russh::ChannelMsg::Data { .. }));
            drop(reader);
            // 读取半边被释放后回调继续消费，不能把发送方卡死。
            writer.closed().await;
            assert_eq!(consumption.buffered_bytes(), 0);
            finish(client, task).await;
        })
        .await
        .expect("丢弃读句柄没有归还额度");
    });
}

#[test]
fn memory_dropped_stderr_output_returns_partial_and_queued_credit() {
    let engine = Engine::new().unwrap();
    let connection = start(&engine, Peer::default(), password(), None).unwrap();
    let mut session = connection.exec("stderr-first", &[]).unwrap();
    session.stderr.read_exact(&mut [0; 4]).unwrap();
    // Session 实现 Drop，借另一个会话的已结束空流替换后丢弃 stderr。
    let mut empty = connection.exec("output", &[]).unwrap();
    std::mem::swap(&mut session.stderr, &mut empty.stderr);
    drop(empty);
    let mut out = String::new();
    session.stdout.read_to_string(&mut out).unwrap();
    assert_eq!(out, "stdout");
    assert_eq!(session.wait().unwrap(), Some(0));
    assert_eq!(session.buffered_bytes(), 0);
}

#[test]
fn memory_unread_server_forwarded_channel_does_not_block_other_channels() {
    let engine = Engine::new().unwrap();
    let connection = start(
        &engine,
        Peer {
            forwarded_flood: true,
            ..Default::default()
        },
        password(),
        None,
    )
    .unwrap();
    let port = connection.remote_forward(0).unwrap();
    let mut incoming = connection.accept_forward().unwrap().stream;
    // 转发的接收队列不读满窗后，普通会话和 ping 继续可用。
    let started = std::time::Instant::now();
    while connection.statistics().received_bytes < 256 * 1024 {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "转发通道未填满窗口"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(connection.statistics().received_bytes, 256 * 1024);
    let mut echo = connection.exec("echo", &[]).unwrap();
    echo.stdin.write_all(b"alive").unwrap();
    let mut reply = [0; 5];
    echo.stdout.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"alive");
    connection.ping().unwrap();
    let mut bytes = Vec::new();
    incoming.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, vec![b'f'; 4 * 1024 * 1024]);
    connection.cancel_forward(port).unwrap();
}

#[path = "wire_tests.rs"]
mod wire_tests;

#[test]
fn memory_replenishing_one_channel_keeps_other_below_threshold() {
    let engine = Engine::new().unwrap();
    engine.executor.handle().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            let adjusted_channels = Arc::new(Mutex::new(HashMap::new()));
            let (client, task, _) = raw_pair(
                Peer {
                    adjusted_channels: adjusted_channels.clone(),
                    ..Default::default()
                },
                russh::client::Config {
                    window_size: 256 * 1024,
                    ..Default::default()
                },
            )
            .await;
            let mut echo = client.channel_open_session().await.unwrap();
            echo.exec(true, "echo").await.unwrap();
            crate::channel::ack(&mut echo, "小额消费").await.unwrap();
            echo.data_bytes(b"four".as_slice()).await.unwrap();
            assert!(matches!(
                echo.wait().await,
                Some(russh::ChannelMsg::Data { .. })
            ));
            let mut flood = client.channel_open_session().await.unwrap();
            flood.exec(true, "flood").await.unwrap();
            crate::channel::ack(&mut flood, "下载").await.unwrap();
            let mut bytes = Vec::new();
            flood.make_reader().read_to_end(&mut bytes).await.unwrap();
            assert_eq!(bytes.len(), 4 * 1024 * 1024);
            client.send_ping().await.unwrap();
            assert_eq!(
                adjusted_channels.lock().unwrap().len(),
                1,
                "其他通道通知不能触发不足阈值的四字节补窗"
            );
            finish(client, task).await;
        })
        .await
        .expect("补窗未能按通道隔离");
    });
}

#[test]
#[cfg(debug_assertions)]
fn memory_consumption_debug_assert_rejects_duplicate_confirmation() {
    let engine = Engine::new().unwrap();
    engine.executor.handle().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (client, task, _) =
                raw_pair(Peer::default(), russh::client::Config::default()).await;
            let mut channel = client.channel_open_session().await.unwrap();
            channel.exec(true, "echo").await.unwrap();
            crate::channel::ack(&mut channel, "重复确认").await.unwrap();
            let (mut reader, writer) = channel.split();
            writer.data_bytes(b"a".as_slice()).await.unwrap();
            reader.wait().await.unwrap();
            let consumption = reader.consumption();
            assert_eq!(consumption.buffered_bytes(), 0);
            assert!(std::panic::catch_unwind(|| consumption.consume(1)).is_err());
            finish(client, task).await;
        })
        .await
        .expect("重复确认测试未完成");
    });
}
