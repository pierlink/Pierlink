//! Pierlink 关闭回归：可控阻塞换钥读方向，使出站队列确定性占满超过一秒。
use super::*;
use crate::memory_peer::{Peer, password, peer_config, start};
use russh::{
    keys::{PrivateKey, ssh_key::private::Ed25519Keypair},
    server,
};
use std::{
    pin::Pin,
    task::{Context, Poll, Waker},
};
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Default)]
struct Gate {
    state: Mutex<(bool, Option<Waker>)>,
    blocked_writes: AtomicUsize,
}
struct Paused(Arc<Gate>);
impl Drop for Paused {
    fn drop(&mut self) {
        self.0.resume();
    }
}
impl Gate {
    fn pause(&self) {
        self.state.lock().unwrap().0 = true;
    }
    fn resume(&self) {
        let wake = {
            let mut state = self.state.lock().unwrap();
            state.0 = false;
            state.1.take()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }
}
struct Gated<S> {
    stream: S,
    gate: Arc<Gate>,
}
impl<S: AsyncRead + Unpin> AsyncRead for Gated<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        {
            let mut state = self.gate.state.lock().unwrap();
            if state.0 {
                state.1 = Some(cx.waker().clone());
                return Poll::Pending;
            }
        }
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for Gated<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, buf);
        if result.is_pending() {
            self.gate.blocked_writes.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[test]
fn memory_drop_and_closer_during_rekey_full_queue_release_maxsessions_ten_times() {
    let engine = Engine::new().unwrap();
    let key: PrivateKey = Ed25519Keypair::from_seed(&[7; 32]).into();
    let mut config = peer_config(key);
    config.keepalive_interval = None;
    let (client, server) = tokio::io::duplex(64 * 1024);
    engine.executor.handle().unwrap().spawn(async move {
        let session = server::run_stream(Arc::new(config), server, Peer::default())
            .await
            .unwrap();
        let _ = session.await;
    });
    let gate = Arc::new(Gate::default());
    let mut options = ConnectionOptions::new(
        "内存",
        22,
        "测试",
        HostKeyCheck::Callback(Arc::new(|_| true)),
    );
    // close() 也必须跨过请求超时，等待真正入队。
    options.io_timeout = Duration::from_secs(1);
    let connection = engine
        .connect_stream(
            options,
            password(),
            Gated {
                stream: client,
                gate: gate.clone(),
            },
            None,
        )
        .unwrap();
    let anchor = connection.exec("echo", &[]).unwrap();
    for index in 0..10 {
        let channel = connection
            .session_start(&[], None, Some("echo"), None)
            .unwrap();
        let control = channel.write_handle();
        let session = crate::Session::new(channel, connection.clone()).unwrap();
        gate.pause();
        let paused = Paused(gate.clone());
        connection
            .run("发起换钥", async {
                connection
                    .inner
                    .handle
                    .read()
                    .await
                    .rekey_soon()
                    .await
                    .map_err(Error::from)
            })
            .unwrap();
        let sent = Arc::new(AtomicUsize::new(0));
        let count = sent.clone();
        let filler = engine.executor.handle().unwrap().spawn(async move {
            for _ in 0..64 {
                control.window_change(80, 24, 0, 0).await.unwrap();
                count.fetch_add(1, Ordering::SeqCst);
            }
        });
        let started = std::time::Instant::now();
        // 等 filler 被满队列挡住：计数连续 300 毫秒不再增长。队列里还可能有换钥请求等别的消息，
        // filler 能排进几条取决于事件循环是否已经取走它们（各平台时机不同，Windows 上见过 8 条），
        // 所以只要求停在队列容量以内；换钥期间如果用户队列仍在出队，64 条会很快全部排完。
        let mut last = (sent.load(Ordering::SeqCst), std::time::Instant::now());
        loop {
            std::thread::sleep(Duration::from_millis(20));
            let now = sent.load(Ordering::SeqCst);
            if now != last.0 {
                last = (now, std::time::Instant::now());
            } else if last.1.elapsed() >= Duration::from_millis(300) {
                break;
            }
            // 只是防止测试卡死的上限
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "出站队列一直没有稳定，已排入 {now} 条消息"
            );
        }
        let queued = sent.load(Ordering::SeqCst);
        assert!(
            (1..=10).contains(&queued),
            "换钥期间用户队列仍在出队：已排入 {queued} 条"
        );
        let worker = if index % 2 == 0 {
            drop(session);
            None
        } else {
            let closer = session.closer();
            Some(std::thread::spawn(move || {
                let result = closer.close();
                drop(session);
                result
            }))
        };
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            sent.load(Ordering::SeqCst),
            queued,
            "关闭等待期间队列仍在出队"
        );
        drop(paused);
        connection
            .run("等待队列恢复", async {
                filler.await.map_err(|_| Error::Runtime)
            })
            .unwrap();
        if let Some(worker) = worker {
            worker.join().unwrap().unwrap();
        }
        connection.ping().unwrap();
        // anchor 固定占一个名额，每轮能再打开一个即证明 Close 到达了 MaxSessions=2 对端。
        let probe = connection.exec("echo", &[]).unwrap();
        probe.close().unwrap();
        drop(probe);
        connection.ping().unwrap();
    }
    assert!(connection.statistics().key_exchanges >= 11);
    anchor.close().unwrap();
}

#[test]
fn memory_saved_closers_do_not_keep_connection_or_runtime_alive() {
    let engine = Engine::new().unwrap();
    let connection = start(&engine, Peer::default(), password(), None).unwrap();
    let weak = connection.downgrade();
    let runtime = Arc::downgrade(&engine.executor);
    let session = connection.exec("echo", &[]).unwrap();
    let tunnel = connection.direct_streamlocal("echo").unwrap();
    let closers = [session.closer(), tunnel.closer()];
    drop(session);
    drop(tunnel);
    drop(connection);
    drop(engine);
    assert!(weak.upgrade().is_none());
    assert!(runtime.upgrade().is_none());
    for closer in closers {
        assert_eq!(closer.close(), Err(Error::Disconnected));
    }
}

#[test]
fn 引擎写满后对端断开和本地断开均取消应用写入并退出任务() {
    use std::io::Write;
    for remote in [true, false] {
        let engine = Engine::new().unwrap();
        let mut config = peer_config(Ed25519Keypair::from_seed(&[7; 32]).into());
        config.keepalive_interval = None;
        config.limits = russh::Limits::default();
        let (client, server) = tokio::io::duplex(4096);
        let server_gate = Arc::new(Gate::default());
        let paused = Paused(server_gate.clone());
        let client_gate = Arc::new(Gate::default());
        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let gate = server_gate.clone();
        let task = engine.executor.handle().unwrap().spawn(async move {
            let session = server::run_stream(
                Arc::new(config),
                Gated {
                    stream: server,
                    gate,
                },
                Peer::default(),
            )
            .await
            .unwrap();
            handle_tx.send(session.handle()).unwrap();
            session.await
        });
        let options = ConnectionOptions::new(
            "内存",
            22,
            "测试",
            HostKeyCheck::Callback(Arc::new(|_| true)),
        );
        let connection = engine
            .connect_stream(
                options,
                password(),
                Gated {
                    stream: client,
                    gate: client_gate.clone(),
                },
                None,
            )
            .unwrap();
        let server = handle_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut session = connection.exec("echo", &[]).unwrap();
        server_gate.pause();
        let blocked = client_gate.blocked_writes.load(Ordering::SeqCst);
        let (done, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let first = session
                .stdin
                .write_all(&vec![0x5a; 8 * 1024 * 1024])
                .map_err(Error::from);
            let next = session.stdin.write_all(b"again").map_err(Error::from);
            done.send((first, next)).unwrap();
        });
        engine.executor.handle().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(2), async {
                while client_gate.blocked_writes.load(Ordering::SeqCst) == blocked {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        });
        assert!(received.try_recv().is_err(), "应用写入应仍受背压阻塞");
        let started = std::time::Instant::now();
        if remote {
            engine.executor.handle().unwrap().block_on(async {
                server
                    .disconnect(
                        russh::Disconnect::ByApplication,
                        "对端结束".into(),
                        "".into(),
                    )
                    .await
                    .unwrap();
            });
        } else {
            connection.disconnect().unwrap();
        }
        let result = received.recv_timeout(Duration::from_secs(2));
        let closed = connection.is_closed();
        let exited = connection.wait_disconnected(Duration::from_secs(2));
        // 即使断言失败也先取消写入，避免线程继续持有运行时。
        connection.abort();
        drop(paused);
        worker.join().unwrap();
        assert_eq!(
            result.unwrap(),
            (Err(Error::Disconnected), Err(Error::Disconnected))
        );
        assert!(closed);
        exited.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        engine.executor.handle().unwrap().block_on(async {
            let _ = tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap();
        });
        eprintln!(
            "引擎断开取消写入，对端发起={remote}：{:?}",
            started.elapsed()
        );
    }
}
