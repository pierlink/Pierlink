//! 回环转发服务；每个客户端独立异步搬运，队列与并发数有界。
use crate::{ChannelFailure, Connection, Error, Result};
use russh::{ChannelStream, client};
use std::{
    net::{Ipv4Addr, Ipv6Addr, TcpListener as StdListener},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc, watch},
    task::JoinSet,
};

const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CLIENTS: usize = 128;

/// 服务器可见的转发目标；域名交给 SSH 服务器解析。
#[derive(Clone)]
pub enum ForwardingTarget {
    Tcp { host: String, port: u16 },
    Unix { path: String },
}

/// 一组已经由调用方分配端口的转发；本地监听必须是 IPv4 回环地址。
pub enum ForwardingRule {
    Local {
        listener: StdListener,
        target: ForwardingTarget,
    },
    Dynamic {
        listener: StdListener,
    },
    Remote {
        remote_port: u16,
        local_port: u16,
    },
}

#[derive(Default)]
struct State {
    ready: bool,
    failure: Option<Error>,
    client_error: Option<(Error, bool)>,
}
type Shared = Arc<Mutex<State>>;

/// 单组转发的生命周期；释放时关闭客户端并撤销自己的远程监听，不断开共享 SSH。
pub struct Forwarding {
    state: Shared,
    cancel: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
    connection: Connection,
}

impl Forwarding {
    /// 无参数；返回监听是否全部就绪，建立失败或 SSH 断线时返回错误。
    pub fn status(&self) -> Result<bool> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(error) = &state.failure {
            return Err(error.clone());
        }
        if self.connection.is_closed() {
            return Err(Error::Disconnected);
        }
        Ok(state.ready)
    }

    /// 无参数；取走最近一次客户端通道拒绝原因及是否为 Unix 套接字，其他客户端继续工作。
    pub fn take_client_error(&self) -> Option<(Error, bool)> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .client_error
            .take()
    }
}

impl Drop for Forwarding {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
        if let Some(task) = self.task.take() {
            // 撤销请求有界等待；即使调用方误在异步上下文释放，后台任务仍自行清理。
            let _ = self
                .connection
                .executor()
                .run(Duration::from_secs(1), "停止转发", async {
                    task.await.map_err(|_| Error::Runtime)
                });
        }
    }
}

impl Connection {
    /// 按 rules 启动本地、远程或 SOCKS5 转发，返回可查询就绪状态的独立句柄。
    /// 每组最多 64 个监听、128 个活跃客户端；释放句柄回收监听、通道及任务。
    pub fn forwarding(&self, rules: Vec<ForwardingRule>) -> Result<Forwarding> {
        if rules.is_empty() || rules.len() > 64 {
            return Err(Error::InvalidInput);
        }
        for rule in &rules {
            match rule {
                ForwardingRule::Local { listener, target } => {
                    validate_listener(listener)?;
                    match target {
                        ForwardingTarget::Tcp { host, port }
                            if host.is_empty() || host.contains('\0') || *port == 0 =>
                        {
                            return Err(Error::InvalidInput);
                        }
                        ForwardingTarget::Unix { path }
                            if path.is_empty() || path.contains('\0') =>
                        {
                            return Err(Error::InvalidInput);
                        }
                        _ => {}
                    }
                }
                ForwardingRule::Dynamic { listener } => validate_listener(listener)?,
                ForwardingRule::Remote {
                    remote_port,
                    local_port,
                } if *remote_port == 0 || *local_port == 0 => return Err(Error::InvalidInput),
                _ => {}
            }
        }
        let state = Arc::new(Mutex::new(State::default()));
        let (cancel, mut cancelled) = watch::channel(false);
        let shared = state.clone();
        let stop_listeners = cancel.clone();
        let connection = self.clone();
        let task = self.executor().handle()?.spawn(async move {
            let mut ports = RemotePorts {
                connection: connection.clone(),
                ports: Vec::new(),
            };
            let mut listeners = JoinSet::new();
            let listener_cancel = cancelled.clone();
            let result = tokio::select! {
                biased;
                _ = cancelled.wait_for(|v| *v) => Ok(()),
                result = serve(&connection, rules, &shared, &mut ports, &mut listeners, listener_cancel) => result,
            };
            if let Err(error) = result {
                shared.lock().unwrap_or_else(|p| p.into_inner()).failure = Some(error);
            }
            // 先等待监听与客户端任务退出，重建时才不会撞上自己的旧监听端口。
            stop_listeners.send_replace(true);
            while listeners.join_next().await.is_some() {}
            ports.cleanup().await;
        });
        Ok(Forwarding {
            state,
            cancel,
            task: Some(task),
            connection: self.clone(),
        })
    }
}

fn validate_listener(listener: &StdListener) -> Result<()> {
    if listener.local_addr()?.ip() != std::net::IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return Err(Error::InvalidInput);
    }
    listener.set_nonblocking(true)?;
    Ok(())
}

struct RemotePorts {
    connection: Connection,
    ports: Vec<u16>,
}
impl RemotePorts {
    async fn register(&mut self, port: u16) -> Result<mpsc::Receiver<ChannelStream<client::Msg>>> {
        let (tx, rx) = mpsc::channel(8);
        {
            let mut routes = self
                .connection
                .inner
                .routed
                .lock()
                .map_err(|_| Error::Runtime)?;
            if routes.contains_key(&port)
                || self
                    .connection
                    .inner
                    .allowed_ports
                    .lock()
                    .map_err(|_| Error::Runtime)?
                    .contains(&u32::from(port))
            {
                return Err(Error::RequestRejected("远程转发"));
            }
            // 在请求前登记，允许服务器紧接成功应答发来首条连接。
            routes.insert(port, tx);
            self.ports.push(port);
        }
        tokio::time::timeout(OPEN_TIMEOUT, async {
            self.connection
                .inner
                .handle
                .read()
                .await
                .tcpip_forward("127.0.0.1", u32::from(port))
                .await
        })
        .await
        .map_err(|_| Error::Timeout("远程转发"))?
        .map_err(|e| match Error::from(e) {
            Error::RequestRejected(_) => Error::RequestRejected("远程转发"),
            error => error,
        })?;
        Ok(rx)
    }

    async fn cleanup(&mut self) {
        let mut tasks = JoinSet::new();
        for port in std::mem::take(&mut self.ports) {
            let connection = self.connection.clone();
            tasks.spawn(async move {
                // 同一端口在撤销应答前继续保留登记，防止旧清理误撤销新的转发。
                let _ = tokio::time::timeout(Duration::from_millis(500), async {
                    connection
                        .inner
                        .handle
                        .read()
                        .await
                        .cancel_tcpip_forward("127.0.0.1", u32::from(port))
                        .await
                })
                .await;
                connection
                    .inner
                    .routed
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&port);
            });
        }
        while tasks.join_next().await.is_some() {}
    }
}

async fn serve(
    connection: &Connection,
    rules: Vec<ForwardingRule>,
    state: &Shared,
    ports: &mut RemotePorts,
    listeners: &mut JoinSet<Result<()>>,
    cancelled: watch::Receiver<bool>,
) -> Result<()> {
    let limit = Arc::new(Semaphore::new(MAX_CLIENTS));
    for rule in rules {
        let connection = connection.clone();
        let limit = limit.clone();
        let state = state.clone();
        match rule {
            ForwardingRule::Local { listener, target } => {
                let listener = TcpListener::from_std(listener)?;
                listeners.spawn(local_connections(
                    connection,
                    listener,
                    Some(target),
                    limit,
                    state,
                    cancelled.clone(),
                ));
            }
            ForwardingRule::Dynamic { listener } => {
                let listener = TcpListener::from_std(listener)?;
                listeners.spawn(local_connections(
                    connection,
                    listener,
                    None,
                    limit,
                    state,
                    cancelled.clone(),
                ));
            }
            ForwardingRule::Remote {
                remote_port,
                local_port,
            } => {
                let incoming = ports.register(remote_port).await?;
                listeners.spawn(remote_connections(
                    incoming,
                    local_port,
                    limit,
                    cancelled.clone(),
                ));
            }
        }
    }
    state.lock().unwrap_or_else(|p| p.into_inner()).ready = true;
    loop {
        tokio::select! {
            result = listeners.join_next() => return result.ok_or(Error::Disconnected)?.map_err(|_| Error::Runtime)?,
            _ = tokio::time::sleep(Duration::from_millis(25)) => {
                if connection.is_closed() { return Err(Error::Disconnected); }
            }
        }
    }
}

async fn local_connections(
    connection: Connection,
    listener: TcpListener,
    target: Option<ForwardingTarget>,
    limit: Arc<Semaphore>,
    state: Shared,
    mut cancelled: watch::Receiver<bool>,
) -> Result<()> {
    let mut clients = JoinSet::new();
    let result = async {
        loop {
            // 及时回收已结束任务，连接反复建立时也不积累完成句柄。
            while clients.try_join_next().is_some() {}
            tokio::select! {
                biased;
                _ = cancelled.wait_for(|v| *v) => return Ok(()),
                _ = clients.join_next(), if !clients.is_empty() => {},
                accepted = listener.accept() => {
                    let (mut socket, _) = accepted?;
                    let Ok(permit) = limit.clone().try_acquire_owned() else { continue };
                    let connection = connection.clone();
                    let target = target.clone();
                    let state = state.clone();
                    clients.spawn(async move {
                        let _permit = permit;
                        client_connection(&connection, &mut socket, target, &state).await;
                    });
                }
            }
        }
    }
    .await;
    clients.shutdown().await;
    result
}

async fn client_connection<S: AsyncRead + AsyncWrite + Unpin>(
    connection: &Connection,
    socket: &mut S,
    target: Option<ForwardingTarget>,
    state: &Shared,
) {
    let streamlocal = matches!(target, Some(ForwardingTarget::Unix { .. }));
    let result = tokio::time::timeout(OPEN_TIMEOUT, async {
        match target {
            Some(target) => open(connection, &target).await,
            None => socks_connect(socket, connection).await,
        }
    })
    .await
    .unwrap_or(Err(Error::Timeout("客户端转发")));
    match result {
        Ok(mut stream) => {
            let _ = tokio::io::copy_bidirectional(socket, &mut stream).await;
        }
        Err(error @ Error::ChannelRejected(_)) => {
            state.lock().unwrap_or_else(|p| p.into_inner()).client_error =
                Some((error, streamlocal));
        }
        Err(_) => {}
    }
}

async fn remote_connections(
    mut incoming: mpsc::Receiver<ChannelStream<client::Msg>>,
    port: u16,
    limit: Arc<Semaphore>,
    mut cancelled: watch::Receiver<bool>,
) -> Result<()> {
    let mut clients = JoinSet::new();
    let result = async {
    loop {
        // 及时回收已结束任务，连接反复建立时也不积累完成句柄。
        while clients.try_join_next().is_some() {}
        tokio::select! {
            biased;
            _ = cancelled.wait_for(|v| *v) => return Ok(()),
            _ = clients.join_next(), if !clients.is_empty() => {},
            incoming = incoming.recv() => {
                let mut stream = incoming.ok_or(Error::Disconnected)?;
                let Ok(permit) = limit.clone().try_acquire_owned() else { continue };
                clients.spawn(async move {
                    let _permit = permit;
                    if let Ok(Ok(mut socket)) = tokio::time::timeout(OPEN_TIMEOUT, TcpStream::connect((Ipv4Addr::LOCALHOST, port))).await {
                        let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                    }
                });
            }
        }
    }
    }.await;
    clients.shutdown().await;
    result
}

async fn open(
    connection: &Connection,
    target: &ForwardingTarget,
) -> Result<russh::ChannelStream<russh::client::Msg>> {
    let permit = connection
        .inner
        .pending_channels
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::ChannelRejected(ChannelFailure::ResourceShortage))?;
    let handle = connection.inner.handle.clone();
    let target = target.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    // 取消等待后仍接收迟到的开通道应答并立即关闭，避免远端留下无人持有的 TCP 连接。
    // 等待者最多 128 个，且只持有协议句柄；不延长 Connection 的空闲回收寿命。
    tokio::spawn(async move {
        let _permit = permit;
        let handle = handle.read().await;
        let result = match target {
            ForwardingTarget::Tcp { host, port } => {
                handle
                    .channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", 0)
                    .await
            }
            ForwardingTarget::Unix { path } => handle.channel_open_direct_streamlocal(path).await,
        }
        .map(|channel| channel.into_stream())
        .map_err(Error::from);
        let _ = tx.send(result);
    });
    rx.await.map_err(|_| Error::Disconnected)?
}

async fn socks_target<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut S,
) -> Result<ForwardingTarget> {
    let mut greeting = [0; 2];
    socket.read_exact(&mut greeting).await?;
    if greeting[0] != 5 {
        return Err(Error::Protocol);
    }
    let mut methods = vec![0; usize::from(greeting[1])];
    socket.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        socket.write_all(&[5, 255]).await?;
        return Err(Error::Protocol);
    }
    socket.write_all(&[5, 0]).await?;
    let mut request = [0; 4];
    socket.read_exact(&mut request).await?;
    if request[0] != 5 || request[2] != 0 {
        socks_reply(socket, 1).await?;
        return Err(Error::Protocol);
    }
    if request[1] != 1 {
        socks_reply(socket, 7).await?;
        return Err(Error::Protocol);
    }
    let host = match request[3] {
        1 => {
            let mut bytes = [0; 4];
            socket.read_exact(&mut bytes).await?;
            Ipv4Addr::from(bytes).to_string()
        }
        4 => {
            let mut bytes = [0; 16];
            socket.read_exact(&mut bytes).await?;
            Ipv6Addr::from(bytes).to_string()
        }
        3 => {
            let length = socket.read_u8().await?;
            let mut bytes = vec![0; usize::from(length)];
            socket.read_exact(&mut bytes).await?;
            match String::from_utf8(bytes) {
                Ok(host) if !host.is_empty() && !host.contains('\0') => host,
                _ => {
                    socks_reply(socket, 8).await?;
                    return Err(Error::InvalidInput);
                }
            }
        }
        _ => {
            socks_reply(socket, 8).await?;
            return Err(Error::Protocol);
        }
    };
    let port = socket.read_u16().await?;
    if port == 0 {
        socks_reply(socket, 1).await?;
        return Err(Error::InvalidInput);
    }
    Ok(ForwardingTarget::Tcp { host, port })
}

async fn socks_reply<S: AsyncWrite + Unpin>(socket: &mut S, code: u8) -> Result<()> {
    socket.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    Ok(())
}

async fn socks_connect<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut S,
    connection: &Connection,
) -> Result<russh::ChannelStream<russh::client::Msg>> {
    let target = socks_target(socket).await?;
    let result = open(connection, &target).await;
    let code = match &result {
        Ok(_) => 0,
        Err(Error::ChannelRejected(ChannelFailure::Prohibited)) => 2,
        Err(Error::ChannelRejected(ChannelFailure::ConnectFailed)) => 5,
        Err(_) => 1,
    };
    socks_reply(socket, code).await?;
    result
}

#[cfg(test)]
#[path = "forwarding_tests.rs"]
mod tests;
