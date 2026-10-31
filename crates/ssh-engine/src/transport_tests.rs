//! 可暂停、限速的内存传输，覆盖密文积压时的断开、保活与控制消息。
use crate::Error;
use russh::{Channel, ChannelId, client, keys::ssh_key::private::Ed25519Keypair, server};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf},
    sync::mpsc,
    time::{Instant, Sleep, timeout},
};

const INTERVAL: Duration = Duration::from_millis(150);
const KEEPALIVE_MAX: usize = 3;

#[derive(Clone, Copy, Default)]
enum ReadMode {
    #[default]
    Open,
    Paused,
    Slow,
    Closed,
}

#[derive(Default)]
struct Control {
    read: Mutex<(ReadMode, Option<Waker>)>,
    written: AtomicUsize,
    blocked: AtomicUsize,
}

impl Control {
    fn set(&self, mode: ReadMode) {
        let wake = {
            let mut state = self.read.lock().unwrap();
            state.0 = mode;
            state.1.take()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }
}

struct Transport {
    stream: DuplexStream,
    control: Arc<Control>,
    delay: Option<Pin<Box<Sleep>>>,
}

impl AsyncRead for Transport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mode = {
            let mut state = self.control.read.lock().unwrap();
            state.1 = Some(cx.waker().clone());
            state.0
        };
        match mode {
            ReadMode::Paused => Poll::Pending,
            ReadMode::Closed => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "测试传输已关闭",
            ))),
            ReadMode::Open => Pin::new(&mut self.stream).poll_read(cx, buf),
            ReadMode::Slow => {
                if let Some(delay) = &mut self.delay
                    && delay.as_mut().poll(cx).is_pending()
                {
                    return Poll::Pending;
                }
                let mut bytes = [0; 4096];
                let length = buf.remaining().min(bytes.len());
                let mut limited = ReadBuf::new(&mut bytes[..length]);
                let result = Pin::new(&mut self.stream).poll_read(cx, &mut limited);
                let count = limited.filled().len();
                buf.put_slice(limited.filled());
                if count > 0 {
                    self.delay = Some(Box::pin(tokio::time::sleep(Duration::from_millis(20))));
                }
                result
            }
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, buf);
        match &result {
            Poll::Ready(Ok(count)) => {
                self.control.written.fetch_add(*count, Ordering::SeqCst);
            }
            Poll::Pending => {
                self.control.blocked.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum End {
    Remote(u32, String),
    Failure(Error),
}

#[derive(Default)]
struct Client {
    end: Arc<Mutex<Option<End>>>,
    received: Arc<Mutex<Vec<u8>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn data(
        &mut self,
        _: ChannelId,
        data: &[u8],
        _: &mut client::Session,
    ) -> Result<(), Self::Error> {
        self.received.lock().unwrap().extend_from_slice(data);
        Ok(())
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        match reason {
            client::DisconnectReason::ReceivedDisconnect(info) => {
                *self.end.lock().unwrap() =
                    Some(End::Remote(info.reason_code as u32, info.message));
                Ok(())
            }
            client::DisconnectReason::Error(error) => {
                *self.end.lock().unwrap() = Some(End::Failure(Error::from(error)));
                Err(russh::Error::Disconnect)
            }
        }
    }
}

struct Peer {
    opened: mpsc::UnboundedSender<Channel<server::Msg>>,
    received: Arc<Mutex<Vec<u8>>>,
}

fn payload() -> Vec<u8> {
    (0..256 * 1024).map(|index| (index % 251) as u8).collect()
}

impl server::Handler for Peer {
    type Error = russh::Error;

    async fn auth_none(&mut self, _: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.opened.send(channel).unwrap();
        reply.accept().await;
        Ok(())
    }

    async fn data(
        &mut self,
        _: ChannelId,
        data: &[u8],
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.received.lock().unwrap().extend_from_slice(data);
        Ok(())
    }

    async fn exec_request(
        &mut self,
        id: ChannelId,
        _: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        // 同一次回调入队数据和断开，确保进入主动断开状态时仍有密文积压。
        session.data(id, payload())?;
        session.disconnect(russh::Disconnect::ByApplication, "主动结束", "")
    }
}

struct Pair {
    client: client::Handle<Client>,
    server: server::Handle,
    server_task: tokio::task::JoinHandle<Result<(), russh::Error>>,
    client_channel: Channel<client::Msg>,
    server_channel: Channel<server::Msg>,
    client_io: Arc<Control>,
    server_io: Arc<Control>,
    end: Arc<Mutex<Option<End>>>,
    uploaded: Arc<Mutex<Vec<u8>>>,
    downloaded: Arc<Mutex<Vec<u8>>>,
}

impl Drop for Pair {
    fn drop(&mut self) {
        // 断言失败时也唤醒协议读取，使后台连接任务释放传输。
        self.client_io.set(ReadMode::Closed);
        self.server_io.set(ReadMode::Closed);
        self.server_task.abort();
    }
}

impl Pair {
    async fn new(client_alive: bool, server_alive: bool) -> Self {
        let config = server::Config {
            keys: vec![Ed25519Keypair::from_seed(&[31; 32]).into()],
            window_size: 4 * 1024 * 1024,
            keepalive_interval: server_alive.then_some(INTERVAL),
            keepalive_max: KEEPALIVE_MAX,
            ..Default::default()
        };
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let client_io = Arc::new(Control::default());
        let server_io = Arc::new(Control::default());
        let (opened, mut channels) = mpsc::unbounded_channel();
        let uploaded = Arc::new(Mutex::new(Vec::new()));
        let peer = Peer {
            opened,
            received: uploaded.clone(),
        };
        let server = server::run_stream(
            Arc::new(config),
            Transport {
                stream: server_stream,
                control: server_io.clone(),
                delay: None,
            },
            peer,
        );
        let handler = Client::default();
        let end = handler.end.clone();
        let downloaded = handler.received.clone();
        let client = client::connect_stream(
            Arc::new(client::Config {
                window_size: 4 * 1024 * 1024,
                keepalive_interval: client_alive.then_some(INTERVAL),
                keepalive_max: KEEPALIVE_MAX,
                ..Default::default()
            }),
            Transport {
                stream: client_stream,
                control: client_io.clone(),
                delay: None,
            },
            handler,
        );
        let (server, client) = tokio::join!(server, client);
        let server = server.unwrap();
        let server_handle = server.handle();
        let server_task = tokio::spawn(server);
        let mut client = client.unwrap();
        assert!(client.authenticate_none("测试").await.unwrap().success());
        let client_channel = client.channel_open_session().await.unwrap();
        let server_channel = channels.recv().await.unwrap();
        Self {
            client,
            server: server_handle,
            server_task,
            client_channel,
            server_channel,
            client_io,
            server_io,
            end,
            uploaded,
            downloaded,
        }
    }

    async fn queue(&self, from_client: bool) {
        if from_client {
            self.client
                .data(self.client_channel.id(), payload())
                .await
                .unwrap();
        } else {
            self.server
                .data(self.server_channel.id(), payload())
                .await
                .unwrap();
        }
    }

    async fn finish(&mut self) {
        self.client_io.set(ReadMode::Open);
        self.server_io.set(ReadMode::Open);
        self.server
            .disconnect(
                russh::Disconnect::ByApplication,
                "测试结束".into(),
                "".into(),
            )
            .await
            .unwrap();
        timeout(Duration::from_secs(2), &mut self.client)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(2), &mut self.server_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

async fn until(mut condition: impl FnMut() -> bool) {
    timeout(Duration::from_secs(8), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("内存传输未达到预期状态");
}

#[tokio::test]
async fn 写满收到对端断开立即退出且后续写入失败() {
    let mut pair = Pair::new(false, false).await;
    pair.server_io.set(ReadMode::Paused);
    let blocked = pair.client_io.blocked.load(Ordering::SeqCst);
    pair.queue(true).await;
    until(|| pair.client_io.blocked.load(Ordering::SeqCst) > blocked).await;
    let started = Instant::now();
    pair.server
        .disconnect(
            russh::Disconnect::ByApplication,
            "对端结束".into(),
            "".into(),
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(2), &mut pair.client)
        .await
        .unwrap()
        .unwrap();
    assert!(pair.client.is_closed());
    assert_eq!(
        *pair.end.lock().unwrap(),
        Some(End::Remote(
            russh::Disconnect::ByApplication as u32,
            "对端结束".into()
        ))
    );
    let error = timeout(
        Duration::from_millis(200),
        pair.client_channel.data_bytes("再次写入".as_bytes()),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(Error::from(error), Error::Disconnected);
    eprintln!("收到断开后退出并拒绝写入：{:?}", started.elapsed());
    pair.server_io.set(ReadMode::Open);
    timeout(Duration::from_secs(2), &mut pair.server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

async fn slow_peer(from_client: bool) {
    let mut pair = Pair::new(from_client, !from_client).await;
    let (sender, receiver, received) = if from_client {
        (
            pair.client_io.clone(),
            pair.server_io.clone(),
            pair.uploaded.clone(),
        )
    } else {
        (
            pair.server_io.clone(),
            pair.client_io.clone(),
            pair.downloaded.clone(),
        )
    };
    // 阻止所有入站回复，仅凭实际出站进展维持存活。
    sender.set(ReadMode::Paused);
    receiver.set(ReadMode::Slow);
    let blocked = sender.blocked.load(Ordering::SeqCst);
    let started = Instant::now();
    pair.queue(from_client).await;
    until(|| received.lock().unwrap().len() == 256 * 1024).await;
    assert!(started.elapsed() > INTERVAL * KEEPALIVE_MAX as u32 * 2);
    assert!(sender.blocked.load(Ordering::SeqCst) > blocked);
    assert!(!pair.client.is_closed());
    assert!(!pair.server_task.is_finished());
    assert_eq!(*received.lock().unwrap(), payload());
    eprintln!(
        "限速积压期间保持存活，客户端发送={from_client}：{:?}",
        started.elapsed()
    );
    pair.finish().await;
}

#[tokio::test]
async fn 客户端持续慢写超过保活期限仍完整传输() {
    slow_peer(true).await;
}

#[tokio::test]
async fn 服务端持续慢写超过保活期限仍完整传输() {
    slow_peer(false).await;
}

async fn stalled_peer(from_client: bool) {
    let mut pair = Pair::new(from_client, !from_client).await;
    pair.client_io.set(ReadMode::Paused);
    pair.server_io.set(ReadMode::Paused);
    let sender = if from_client {
        pair.client_io.clone()
    } else {
        pair.server_io.clone()
    };
    let blocked = sender.blocked.load(Ordering::SeqCst);
    pair.queue(from_client).await;
    until(|| sender.blocked.load(Ordering::SeqCst) > blocked).await;
    let written = sender.written.load(Ordering::SeqCst);
    let started = Instant::now();
    if from_client {
        assert!(
            timeout(Duration::from_secs(3), &mut pair.client)
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(
            *pair.end.lock().unwrap(),
            Some(End::Failure(Error::Timeout("SSH")))
        );
        assert!(pair.client.is_closed());
    } else {
        let result = timeout(Duration::from_secs(3), &mut pair.server_task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(russh::Error::KeepaliveTimeout)));
    }
    assert!(started.elapsed() >= INTERVAL * (KEEPALIVE_MAX as u32 - 1));
    assert_eq!(sender.written.load(Ordering::SeqCst), written);
    eprintln!(
        "完全停读的保活超时，客户端发送={from_client}：{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn 客户端写满且无入站仍按保活超时退出() {
    stalled_peer(true).await;
}

#[tokio::test]
async fn 服务端写满且无入站仍按保活超时退出() {
    stalled_peer(false).await;
}

#[tokio::test]
async fn 服务端主动断开先排空积压且客户端保留断开原因() {
    let mut pair = Pair::new(false, false).await;
    pair.client_io.set(ReadMode::Paused);
    let blocked = pair.server_io.blocked.load(Ordering::SeqCst);
    pair.client_channel.exec(false, "断开前输出").await.unwrap();
    until(|| pair.server_io.blocked.load(Ordering::SeqCst) > blocked).await;
    assert!(!pair.server_task.is_finished());
    pair.client_io.set(ReadMode::Open);
    timeout(Duration::from_secs(2), &mut pair.client)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*pair.downloaded.lock().unwrap(), payload());
    assert_eq!(
        *pair.end.lock().unwrap(),
        Some(End::Remote(
            russh::Disconnect::ByApplication as u32,
            "主动结束".into()
        ))
    );
    timeout(Duration::from_secs(2), &mut pair.server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn 写满队列时断开关闭和结束输入均随保活退出解除等待() {
    for action in ["disconnect", "close", "eof"] {
        let mut pair = Pair::new(true, false).await;
        pair.server_io.set(ReadMode::Paused);
        let blocked = pair.client_io.blocked.load(Ordering::SeqCst);
        pair.queue(true).await;
        until(|| pair.client_io.blocked.load(Ordering::SeqCst) > blocked).await;
        // 密文写满后，填满容量为十的应用消息队列，确保控制消息必须等待。
        for _ in 0..10 {
            pair.client_channel
                .window_change(80, 24, 0, 0)
                .await
                .unwrap();
        }
        let started = Instant::now();
        let operation = async {
            match action {
                "disconnect" => {
                    pair.client
                        .disconnect(russh::Disconnect::ByApplication, "关闭", "")
                        .await
                }
                "close" => pair.client_channel.close().await,
                _ => pair.client_channel.eof().await,
            }
        };
        assert!(
            timeout(Duration::from_secs(5), operation)
                .await
                .unwrap()
                .is_err()
        );
        assert!(
            timeout(Duration::from_secs(1), &mut pair.client)
                .await
                .unwrap()
                .is_err()
        );
        assert!(pair.client.is_closed());
        let mut writer = pair.client_channel.make_writer();
        assert!(
            timeout(
                Duration::from_millis(200),
                writer.write_all("结束后写入".as_bytes())
            )
            .await
            .unwrap()
            .is_err()
        );
        assert_eq!(
            *pair.end.lock().unwrap(),
            Some(End::Failure(Error::Timeout("SSH")))
        );
        eprintln!("写满后 {action} 返回并退出：{:?}", started.elapsed());
    }
}
