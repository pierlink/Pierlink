//! 仅测试中协商明文密码套件，修改内存报文以模拟违规对端；生产配置不变。
use super::*;
use std::collections::VecDeque;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};

#[derive(Clone, Copy)]
struct Fault {
    length: usize,
    extended: bool,
}

async fn relay<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    source: R,
    mut target: W,
    faults: Arc<Mutex<VecDeque<Fault>>>,
) -> std::io::Result<()> {
    let mut source = BufReader::new(source);
    let mut banner = Vec::new();
    source.read_until(b'\n', &mut banner).await?;
    target.write_all(&banner).await?;
    loop {
        let length = source.read_u32().await? as usize;
        assert!(length < 1024 * 1024, "测试报文长度异常");
        let mut packet = vec![0; length];
        source.read_exact(&mut packet).await?;
        // 包体首字节为填充长度，第二字节为 SSH 消息类型。
        let fault = if packet[1] == 94 {
            faults.lock().unwrap().pop_front()
        } else {
            None
        };
        if let Some(fault) = fault {
            let mut payload = vec![if fault.extended { 95 } else { 94 }];
            payload.extend_from_slice(&packet[2..6]);
            if fault.extended {
                payload.extend_from_slice(&1_u32.to_be_bytes());
            }
            payload.extend_from_slice(&(fault.length as u32).to_be_bytes());
            payload.resize(payload.len() + fault.length, b'x');
            let mut padding = 8 - (5 + payload.len()) % 8;
            if padding < 4 {
                padding += 8;
            }
            packet.clear();
            packet.push(padding as u8);
            packet.extend_from_slice(&payload);
            packet.resize(packet.len() + padding, 0);
        }
        target.write_u32(packet.len() as u32).await?;
        target.write_all(&packet).await?;
    }
}

async fn faulty_pair(
    window: u32,
    packet: u32,
    faults: Vec<Fault>,
) -> (
    russh::client::Handle<RawClient>,
    Vec<tokio::task::JoinHandle<()>>,
) {
    let (client_stream, proxy_client) = tokio::io::duplex(64 * 1024);
    let (proxy_server, server_stream) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(proxy_client);
    let (server_read, server_write) = tokio::io::split(proxy_server);
    let outbound = tokio::spawn(async move {
        let _ = relay(client_read, server_write, Arc::default()).await;
    });
    let inbound = tokio::spawn(async move {
        let _ = relay(
            server_read,
            client_write,
            Arc::new(Mutex::new(faults.into())),
        )
        .await;
    });
    let mut config = peer_config(Ed25519Keypair::from_seed(&[7; 32]).into());
    config.preferred.cipher = Cow::Owned(vec![russh::cipher::NONE]);
    config.preferred.mac = Cow::Owned(vec![russh::mac::NONE]);
    config.keepalive_interval = None;
    let task = tokio::spawn(async move {
        let session = server::run_stream(Arc::new(config), server_stream, Peer::default())
            .await
            .unwrap();
        let _ = session.await;
    });
    let mut config = russh::client::Config {
        window_size: window,
        maximum_packet_size: packet,
        ..Default::default()
    };
    config.preferred.cipher = Cow::Owned(vec![russh::cipher::NONE]);
    config.preferred.mac = Cow::Owned(vec![russh::mac::NONE]);
    let mut client =
        russh::client::connect_stream(Arc::new(config), client_stream, RawClient::default())
            .await
            .unwrap();
    assert!(
        client
            .authenticate_password("测试", "测试密码")
            .await
            .unwrap()
            .success()
    );
    (client, vec![outbound, inbound, task])
}

async fn stop(tasks: Vec<tokio::task::JoinHandle<()>>) {
    for task in tasks {
        task.abort();
        let _ = task.await;
    }
}

#[test]
fn memory_oversized_normal_and_extended_packets_keep_connection_alive() {
    let engine = Engine::new().unwrap();
    engine.executor.handle().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let faults = [false, true].map(|extended| Fault {
                length: 1025,
                extended,
            });
            let (client, tasks) = faulty_pair(4096, 1024, faults.to_vec()).await;
            let mut channel = client.channel_open_session().await.unwrap();
            channel.exec(true, "echo").await.unwrap();
            crate::channel::ack(&mut channel, "超包长").await.unwrap();
            let (mut reader, writer) = channel.split();
            for extended in [false, true] {
                writer.data_bytes(b"a".as_slice()).await.unwrap();
                let msg = reader.wait().await.unwrap();
                match msg {
                    russh::ChannelMsg::Data { data } if !extended => assert_eq!(data.len(), 1025),
                    russh::ChannelMsg::ExtendedData { ext: 1, data } if extended => {
                        assert_eq!(data.len(), 1025)
                    }
                    other => panic!("未收到预期数据：{other:?}"),
                }
                client.send_ping().await.unwrap();
            }
            assert_eq!(reader.consumption().buffered_bytes(), 0);
            drop(client);
            stop(tasks).await;
        })
        .await
        .expect("超包长错误地断开了连接");
    });
}

#[test]
fn memory_window_overrun_saturates_and_disconnects_only_above_cumulative_ten_percent() {
    let engine = Engine::new().unwrap();
    engine.executor.handle().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let faults = [
                Fault {
                    length: 100,
                    extended: false,
                },
                Fault {
                    length: 5,
                    extended: true,
                },
                Fault {
                    length: 5,
                    extended: false,
                },
                Fault {
                    length: 1,
                    extended: true,
                },
            ];
            let (client, tasks) = faulty_pair(100, 100, faults.to_vec()).await;
            let mut channel = client.channel_open_session().await.unwrap();
            channel.exec(true, "echo").await.unwrap();
            crate::channel::ack(&mut channel, "越窗").await.unwrap();
            let (mut reader, writer) = channel.split();
            for (length, extended) in [(100, false), (5, true), (5, false)] {
                writer.data_bytes(b"a".as_slice()).await.unwrap();
                let msg = reader.wait_unconsumed().await.unwrap();
                match msg {
                    russh::ChannelMsg::Data { data } if !extended => assert_eq!(data.len(), length),
                    russh::ChannelMsg::ExtendedData { data, .. } if extended => {
                        assert_eq!(data.len(), length)
                    }
                    other => panic!("宽限内未收到数据：{other:?}"),
                }
                client.send_ping().await.unwrap();
            }
            assert_eq!(reader.consumption().buffered_bytes(), 110);
            writer.data_bytes(b"a".as_slice()).await.unwrap();
            assert!(reader.wait_unconsumed().await.is_none());
            assert!(matches!(client.await, Err(russh::Error::Inconsistent)));
            stop(tasks).await;
        })
        .await
        .expect("累计越窗未按十分之一窗口限制断开");
    });
}
