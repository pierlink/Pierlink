// Pierlink 修改：关闭请求等待入队完成，关闭句柄弱持有连接；明确 stdout/stderr 共享窗口。
use crate::{Connection, Error, Pty, Result, connection::WeakConnection, error::io_error};
use russh::{Channel, ChannelMsg, ChannelWriteHalf, client};
use std::{
    io::{self, Read, Write},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{mpsc, watch},
};

// 队列无需按消息条数限流：所有队列共享同一通道的 SSH 字节窗口。
struct QueuedOutput {
    receiver: mpsc::UnboundedReceiver<ChannelMsg>,
    pending: Option<(ChannelMsg, usize)>,
    consumption: russh::ChannelConsumption,
}

fn output_data(msg: &ChannelMsg) -> &[u8] {
    match msg {
        ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => data,
        _ => &[],
    }
}

impl AsyncRead for QueuedOutput {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let (msg, offset) = match self.pending.take() {
            Some(pending) => pending,
            None => match ready!(self.receiver.poll_recv(cx)) {
                Some(msg) => (msg, 0),
                None => return Poll::Ready(Ok(())),
            },
        };
        let data = output_data(&msg);
        let count = buf.remaining().min(data.len() - offset);
        buf.put_slice(&data[offset..offset + count]);
        self.consumption.consume(count);
        if offset + count < data.len() {
            self.pending = Some((msg, offset + count));
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for QueuedOutput {
    fn drop(&mut self) {
        self.receiver.close();
        if let Some((msg, offset)) = self.pending.take() {
            self.consumption.consume(output_data(&msg).len() - offset);
        }
        while let Ok(msg) = self.receiver.try_recv() {
            self.consumption.consume(output_data(&msg).len());
        }
    }
}

async fn cancellable<T>(
    mut cancelled: watch::Receiver<bool>,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        _ = cancelled.wait_for(|closed| *closed) => Err(Error::Disconnected),
        result = future => result,
    }
}

pub(crate) struct Opening {
    pub(crate) channel: Option<Channel<client::Msg>>,
}
impl Opening {
    pub(crate) fn new(channel: Channel<client::Msg>) -> Self {
        Self {
            channel: Some(channel),
        }
    }
}
impl Drop for Opening {
    fn drop(&mut self) {
        // 握手请求被拒或超时取消时，也必须发送关闭，释放服务器的 MaxSessions 配额。
        if let Some(channel) = self.channel.take() {
            drop(channel.into_stream());
        }
    }
}

pub(crate) async fn ack(channel: &mut Channel<client::Msg>, request: &'static str) -> Result<()> {
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => return Ok(()),
            Some(ChannelMsg::Failure) => return Err(Error::RequestRejected(request)),
            Some(ChannelMsg::WindowAdjusted { .. }) => {}
            Some(ChannelMsg::Close | ChannelMsg::Eof) | None => return Err(Error::Disconnected),
            _ => return Err(Error::Protocol),
        }
    }
}

/// 同步输出流；stdout 和 stderr 必须都读，共享窗口用尽时两者都会停发；不需要的流可丢弃。
pub struct Output {
    inner: Box<dyn AsyncRead + Unpin + Send>,
    connection: Connection,
    cancelled: watch::Receiver<bool>,
}

impl Read for Output {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let stream = &mut self.inner;
        self.connection
            .wait(cancellable(self.cancelled.clone(), async {
                stream.read(buffer).await.map_err(Error::from)
            }))
            .map_err(io_error)
    }
}

/// 同步标准输入流；shutdown 显式发送 EOF，不关闭标准输出。
pub struct Input {
    inner: Box<dyn AsyncWrite + Unpin + Send>,
    connection: Connection,
    cancelled: watch::Receiver<bool>,
}

impl Input {
    /// 发送通道 EOF；之后仍可读取 stdout、stderr 和退出状态。
    pub fn shutdown(&mut self) -> Result<()> {
        let stream = &mut self.inner;
        self.connection.run("发送 EOF", async {
            stream.shutdown().await.map_err(Error::from)
        })
    }
}

impl Write for Input {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        // 单次写入不把调用方的大缓冲复制到无界队列。
        let buffer = &buffer[..buffer.len().min(32 * 1024)];
        let stream = &mut self.inner;
        self.connection
            .wait(cancellable(self.cancelled.clone(), async {
                stream.write(buffer).await.map_err(Error::from)
            }))
            .map_err(io_error)
    }

    fn flush(&mut self) -> io::Result<()> {
        let stream = &mut self.inner;
        self.connection
            .run("刷新通道", async {
                stream.flush().await.map_err(Error::from)
            })
            .map_err(io_error)
    }
}

/// 可克隆的通道关闭句柄；可在线程阻塞读、写或等待退出时从另一线程取消。
/// 使用连接弱引用，单独保存不会延长连接和运行时寿命。
#[derive(Clone)]
pub struct ChannelCloser {
    control: Arc<ChannelWriteHalf<client::Msg>>,
    cancelled: watch::Sender<bool>,
    connection: WeakConnection,
}

impl ChannelCloser {
    /// 无参数；立即取消本地读写，并在引擎运行时投递关闭请求，不等待队列。
    pub fn cancel(&self) {
        self.cancelled.send_replace(true);
        if let Some(connection) = self.connection.upgrade()
            && let Ok(runtime) = connection.executor().handle()
        {
            let control = self.control.clone();
            runtime.spawn(async move {
                // 先关闭远端输入，让依赖输入存活的后台脚本有机会清理子进程。
                let _ = control.eof().await;
                let _ = control.close().await;
            });
        }
    }

    /// 无参数；立即取消本地等待并等待关闭请求入队，连接已释放时返回 Disconnected。
    pub fn close(&self) -> Result<()> {
        self.cancelled.send_replace(true);
        let connection = self.connection.upgrade().ok_or(Error::Disconnected)?;
        connection.wait(async { self.control.close().await.map_err(Error::from) })
    }
}

/// 服务器提供的退出状态；信号不包含服务端自由文本。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: Option<u32>,
    pub signal: Option<String>,
}

/// exec 或 shell 会话；stdout 和 stderr 必须都读，共享窗口占满时另一流也会停发。
/// 不需要的输出流可丢弃，其已缓存和后续数据会归还额度。
pub struct Session {
    pub stdin: Input,
    pub stdout: Output,
    pub stderr: Output,
    control: Arc<ChannelWriteHalf<client::Msg>>,
    status: watch::Receiver<Option<Result<ExitStatus>>>,
    cancelled: watch::Sender<bool>,
    consumption: russh::ChannelConsumption,
    pump: tokio::task::JoinHandle<()>,
    connection: Connection,
}

impl Session {
    pub(crate) fn new(channel: Channel<client::Msg>, connection: Connection) -> Result<Self> {
        Self::with_output(channel, connection, false)
    }

    pub(crate) fn with_output(
        channel: Channel<client::Msg>,
        connection: Connection,
        merge_output: bool,
    ) -> Result<Self> {
        let (mut reader, writer) = channel.split();
        let control = Arc::new(writer);
        let input = control.make_writer();
        let consumption = reader.consumption();
        let (out_sink, stdout) = mpsc::unbounded_channel();
        let (err_sink, stderr) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(None);
        let (cancelled, cancel_rx) = watch::channel(false);
        let pump_consumption = consumption.clone();
        let pump_cancel = cancel_rx.clone();
        let pump = connection.executor().handle()?.spawn(async move {
            let mut exit = ExitStatus::default();
            let mut out_sink = Some(out_sink);
            let mut err_sink = Some(err_sink);
            let result = cancellable(pump_cancel, async {
                loop {
                    match reader.wait_unconsumed().await {
                        Some(msg @ ChannelMsg::Data { .. }) => {
                            if let Some(sink) = &out_sink {
                                if let Err(error) = sink.send(msg) {
                                    pump_consumption.consume(output_data(&error.0).len());
                                }
                            } else {
                                pump_consumption.consume(output_data(&msg).len());
                            }
                        }
                        Some(msg @ ChannelMsg::ExtendedData { ext: 1, .. }) => {
                            let sink = if merge_output { &out_sink } else { &err_sink };
                            if let Some(sink) = sink {
                                if let Err(error) = sink.send(msg) {
                                    pump_consumption.consume(output_data(&error.0).len());
                                }
                            } else {
                                pump_consumption.consume(output_data(&msg).len());
                            }
                        }
                        Some(ChannelMsg::ExtendedData { data, .. }) => {
                            pump_consumption.consume(data.len())
                        }
                        Some(ChannelMsg::ExitStatus { exit_status }) => {
                            exit.code = Some(exit_status)
                        }
                        Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                            exit.signal = Some(match signal_name {
                                russh::Sig::Custom(_) => "UNKNOWN".to_owned(),
                                signal => format!("{signal:?}"),
                            });
                        }
                        Some(ChannelMsg::Eof) => {
                            out_sink.take();
                            err_sink.take();
                        }
                        Some(ChannelMsg::Close) => return Ok(exit),
                        None => return Err(Error::Disconnected),
                        _ => {}
                    }
                }
            })
            .await;
            status_tx.send_replace(Some(result));
        });
        Ok(Self {
            stdin: Input {
                inner: Box::new(input),
                connection: connection.clone(),
                cancelled: cancel_rx.clone(),
            },
            stdout: Output {
                inner: Box::new(QueuedOutput {
                    receiver: stdout,
                    pending: None,
                    consumption: consumption.clone(),
                }),
                connection: connection.clone(),
                cancelled: cancel_rx.clone(),
            },
            stderr: Output {
                inner: Box::new(QueuedOutput {
                    receiver: stderr,
                    pending: None,
                    consumption: consumption.clone(),
                }),
                connection: connection.clone(),
                cancelled: cancel_rx.clone(),
            },
            control,
            cancelled,
            consumption,
            status,
            pump,
            connection,
        })
    }

    /// 等待通道关闭并返回 exit-status；服务器未提供退出码时返回 None。
    pub fn wait(&mut self) -> Result<Option<u32>> {
        self.wait_status().map(|status| status.code)
    }

    /// 无参数；等待通道关闭，返回退出码及信号；未发送的字段为 None。
    pub fn wait_status(&mut self) -> Result<ExitStatus> {
        let status = &mut self.status;
        self.connection
            .wait(cancellable(self.cancelled.subscribe(), async {
                loop {
                    if let Some(result) = status.borrow().clone() {
                        return result;
                    }
                    status.changed().await.map_err(|_| Error::Disconnected)?;
                }
            }))
    }

    /// 无参数；返回本通道已收到但尚未被 stdout/stderr 消费的字节数。
    pub fn buffered_bytes(&self) -> usize {
        self.consumption.buffered_bytes()
    }

    /// 无参数；取走三个独立读写流，原位置替换为空流，可交给不同线程持有。
    /// 会话仍拥有通道生命周期，必须保留至输出排空；重复调用只返回空流。
    pub fn take_io(&mut self) -> (Input, Output, Output) {
        let input = Input {
            inner: std::mem::replace(&mut self.stdin.inner, Box::new(tokio::io::sink())),
            connection: self.connection.clone(),
            cancelled: self.cancelled.subscribe(),
        };
        let take = |output: &mut Output| Output {
            inner: std::mem::replace(&mut output.inner, Box::new(tokio::io::empty())),
            connection: self.connection.clone(),
            cancelled: self.cancelled.subscribe(),
        };
        (input, take(&mut self.stdout), take(&mut self.stderr))
    }

    /// 无参数；立即返回已结束的状态或错误，通道仍在运行时返回 None，不消费输出。
    pub fn try_wait_status(&self) -> Option<Result<ExitStatus>> {
        self.status.borrow().clone().or_else(|| {
            (self.connection.is_closed() || *self.cancelled.borrow())
                .then_some(Err(Error::Disconnected))
        })
    }

    /// 按给定字符和像素尺寸发送 window-change；字符尺寸为 0 时拒绝。
    pub fn resize(&self, size: &Pty) -> Result<()> {
        if size.columns == 0 || size.rows == 0 {
            return Err(Error::InvalidInput);
        }
        // 只把写句柄交给等待的线程（会话本身不能跨线程共享）
        let control = self.control.clone();
        let (columns, rows, width, height) =
            (size.columns, size.rows, size.pixel_width, size.pixel_height);
        self.connection.run("调整窗口", async move {
            control
                .window_change(columns, rows, width, height)
                .await
                .map_err(Error::from)
        })
    }

    /// 无参数；返回可在其他线程关闭本会话的共享句柄。
    pub fn closer(&self) -> ChannelCloser {
        ChannelCloser {
            control: self.control.clone(),
            cancelled: self.cancelled.clone(),
            connection: self.connection.downgrade(),
        }
    }

    /// 关闭该通道；其他通道及共享连接继续由调用方管理。
    pub fn close(&self) -> Result<()> {
        self.closer().close()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancelled.send_replace(true);
        self.pump.abort();
        let control = self.control.clone();
        if let Ok(runtime) = self.connection.executor().handle() {
            runtime.spawn(async move {
                let _ = control.close().await;
            });
        }
    }
}

/// TCP、Unix 套接字或远程转发的双向同步字节流。
pub struct Tunnel {
    stream: Option<russh::ChannelStream<client::Msg>>,
    connection: Connection,
    closer: ChannelCloser,
}

/// 一条远程转发连接；用监听端口把共享连接上的多个转发路由到各自的本地目标。
pub struct Forwarded {
    pub connected_port: u16,
    pub originator_address: String,
    pub originator_port: u16,
    pub stream: Tunnel,
}

impl Tunnel {
    pub(crate) fn new(channel: Channel<client::Msg>, connection: Connection) -> Self {
        let closer = ChannelCloser {
            control: Arc::new(channel.write_handle()),
            cancelled: watch::channel(false).0,
            connection: connection.downgrade(),
        };
        Self {
            stream: Some(channel.into_stream()),
            connection,
            closer,
        }
    }

    /// 无参数；返回可在其他线程取消本通道读写的关闭句柄。
    pub fn closer(&self) -> ChannelCloser {
        self.closer.clone()
    }

    /// 无参数；取出可跨线程并行的写端和读端，重复取出返回 Disconnected。
    /// 必须保留 Tunnel 至读写结束；其关闭或释放会立即取消两端。
    pub fn take_io(&mut self) -> Result<(Input, Output)> {
        let stream = self.stream.take().ok_or(Error::Disconnected)?;
        let (read, write) = tokio::io::split(SplitStream {
            stream: Some(stream),
            connection: self.connection.clone(),
        });
        Ok((
            Input {
                inner: Box::new(write),
                connection: self.connection.clone(),
                cancelled: self.closer.cancelled.subscribe(),
            },
            Output {
                inner: Box::new(read),
                connection: self.connection.clone(),
                cancelled: self.closer.cancelled.subscribe(),
            },
        ))
    }

    /// 无参数；关闭本通道并取消本地等待，返回关闭请求的结果。
    pub fn close(&self) -> Result<()> {
        self.closer.close()
    }

    /// 半关闭写方向；仍可读取服务器回复。
    pub fn shutdown(&mut self) -> Result<()> {
        let stream = self.stream.as_mut().ok_or(Error::Disconnected)?;
        self.connection.run("转发 EOF", async {
            stream.shutdown().await.map_err(Error::from)
        })
    }
}

// 最后一个拆分端可在任意线程释放，ChannelStream 的析构仍须进入所属运行时。
struct SplitStream {
    stream: Option<russh::ChannelStream<client::Msg>>,
    connection: Connection,
}
impl AsyncRead for SplitStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.stream.as_mut() {
            Some(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            None => std::task::Poll::Ready(Err(io_error(Error::Disconnected))),
        }
    }
}
impl AsyncWrite for SplitStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        match self.stream.as_mut() {
            Some(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            None => std::task::Poll::Ready(Err(io_error(Error::Disconnected))),
        }
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.stream.as_mut() {
            Some(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            None => std::task::Poll::Ready(Err(io_error(Error::Disconnected))),
        }
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.stream.as_mut() {
            Some(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            None => std::task::Poll::Ready(Err(io_error(Error::Disconnected))),
        }
    }
}
impl Drop for SplitStream {
    fn drop(&mut self) {
        if let Ok(runtime) = self.connection.executor().handle() {
            let _enter = runtime.enter();
            drop(self.stream.take());
        }
    }
}

impl Read for Tunnel {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| io_error(Error::Disconnected))?;
        self.connection
            .wait(cancellable(self.closer.cancelled.subscribe(), async {
                stream.read(buffer).await.map_err(Error::from)
            }))
            .map_err(io_error)
    }
}

impl Write for Tunnel {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let buffer = &buffer[..buffer.len().min(32 * 1024)];
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| io_error(Error::Disconnected))?;
        self.connection
            .wait(cancellable(self.closer.cancelled.subscribe(), async {
                stream.write(buffer).await.map_err(Error::from)
            }))
            .map_err(io_error)
    }
    fn flush(&mut self) -> io::Result<()> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| io_error(Error::Disconnected))?;
        self.connection
            .run("刷新转发", async {
                stream.flush().await.map_err(Error::from)
            })
            .map_err(io_error)
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if self.stream.is_none() {
            self.closer.cancel();
        } else {
            self.closer.cancelled.send_replace(true);
        }
        // russh 的 ChannelStream 在 Drop 内启动关闭任务，需要进入它所属的运行时。
        if let Ok(runtime) = self.connection.executor().handle() {
            let _enter = runtime.enter();
            drop(self.stream.take());
        }
    }
}

/// SFTP v3 握手结果；扩展报文保留在有界包内供第二阶段解析。
pub struct SftpHandshake {
    pub version: u32,
    pub extensions: Vec<u8>,
    pub stream: Tunnel,
}

impl Connection {
    /// 启动 sftp 子系统并交换 INIT/VERSION；最大 VERSION 包 64 KiB，畸形或非 v3 回复返回协议错误。
    pub fn sftp_handshake(&self) -> Result<SftpHandshake> {
        let channel = self.session_start(&[], None, None, Some("sftp"))?;
        let mut stream = Tunnel::new(channel, self.clone());
        let packet = self.run("SFTP 握手", async {
            let stream = stream.stream.as_mut().ok_or(Error::Disconnected)?;
            stream.write_all(&[0, 0, 0, 5, 1, 0, 0, 0, 3]).await?;
            let mut length = [0; 4];
            stream.read_exact(&mut length).await?;
            let length = u32::from_be_bytes(length) as usize;
            if !(5..=64 * 1024).contains(&length) {
                return Err(Error::Protocol);
            }
            let mut packet = vec![0; length];
            stream.read_exact(&mut packet).await?;
            if packet[..5] != [2, 0, 0, 0, 3] {
                return Err(Error::Protocol);
            }
            Ok(packet)
        })?;
        Ok(SftpHandshake {
            version: 3,
            extensions: packet[5..].to_vec(),
            stream,
        })
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;
    use crate::{
        Engine,
        memory_peer::{Peer, password, start},
    };
    use std::time::Duration;

    #[test]
    fn 隧道拆分可并行读写且取消后共享连接仍可用() {
        let engine = Engine::new().unwrap();
        let connection = start(&engine, Peer::default(), password(), None).unwrap();
        let mut tunnel = connection.direct_streamlocal("echo").unwrap();
        let (mut input, mut output) = tunnel.take_io().unwrap();
        assert!(matches!(tunnel.take_io(), Err(Error::Disconnected)));
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut bytes = [0; 3];
            output.read_exact(&mut bytes).unwrap();
            tx.send(bytes).unwrap();
            output.read(&mut [0])
        });
        input.write_all(&[1, 2, 3]).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), [1, 2, 3]);
        tunnel.closer().cancel();
        assert!(reader.join().unwrap().is_err());
        assert!(input.write(&[4]).is_err());
        drop(input);
        drop(tunnel);
        connection.ping().unwrap();
        let mut tunnel = connection.direct_streamlocal("echo").unwrap();
        let (mut input, mut output) = tunnel.take_io().unwrap();
        drop(tunnel);
        assert!(input.write(&[1]).is_err());
        assert!(output.read(&mut [0]).is_err());
        drop((input, output));
        connection.ping().unwrap();
        connection.disconnect().unwrap();
    }
}
