// Pierlink 修改：提供关闭句柄使用的连接弱引用，避免延长连接及运行时寿命。
use crate::{
    Authentication, Error, Result, auth,
    channel::{self, Forwarded, Session, Tunnel},
    runtime::Executor,
};
use russh::{
    Channel, ChannelOpenFailure, ChannelStream, client,
    keys::{Algorithm, HashAlg, PublicKey, PublicKeyBase64, PublicKeyOrCertificate},
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    future::Future,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpSocket, lookup_host},
    sync::mpsc,
};

/// 握手实际观察到的主机身份；跳板的每一级分别传给自己的回调。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKey {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub fingerprint: String,
    /// SSH 二进制公钥的 Base64，不含类型、注释或私钥；供 known_hosts 核对和追加。
    pub public_key: String,
}

/// 精确比对已确认指纹，或由回调确认并记录首次指纹。
#[derive(Clone)]
pub enum HostKeyCheck {
    Fingerprint(String),
    Callback(Arc<dyn Fn(HostKey) -> bool + Send + Sync>),
}

/// 建立传输连接时进入的阶段；会话阶段由调用方报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectStage {
    Resolve,
    Connect,
    Auth,
}

/// 阶段通知；回调必须快速返回，不得阻塞引擎线程。
pub type ConnectProgress = Arc<dyn Fn(ConnectStage) + Send + Sync>;

/// 连接参数；host 使用不带方括号的 IPv6 字面量，认证由 connect 单独指定。
#[derive(Clone)]
pub struct ConnectionOptions {
    /// 第一跳使用的出站代理；connect_via 的后续跳忽略此字段。
    pub proxy: Option<crate::proxy::ProxyServer>,
    /// 可选的建连阶段回调；不含主机或认证材料。
    pub progress: Option<ConnectProgress>,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub host_key: HostKeyCheck,
    pub connect_timeout: Duration,
    pub auth_timeout: Duration,
    pub io_timeout: Duration,
    /// 每个通道的接收窗口和未消费数据上限；默认交互连接 256 KiB，批量连接可传 2 MiB。
    pub window_size: u32,
    /// 显式允许旧服务器的 RSA/SHA-1 签名、DH/SHA-1 和 HMAC/SHA-1。
    pub legacy_algorithms: bool,
    /// 优先使用的已知公钥类型（known_hosts 的类型名）；只重排允许列表，不放宽算法策略。
    pub preferred_host_key_algorithms: Vec<String>,
    /// 覆盖 TCP、握手和认证等待的取消标记；不影响已经登录的连接。
    pub cancelled: Option<Arc<AtomicBool>>,
}

impl ConnectionOptions {
    /// 从地址、用户名和主机校验策略建立默认参数；返回的参数仍需交给 connect 校验。
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        host_key: HostKeyCheck,
    ) -> Self {
        Self {
            proxy: None,
            progress: None,
            host: host.into(),
            port,
            username: username.into(),
            host_key,
            connect_timeout: Duration::from_secs(10),
            auth_timeout: Duration::from_secs(120),
            io_timeout: Duration::from_secs(30),
            window_size: 256 * 1024,
            legacy_algorithms: false,
            preferred_host_key_algorithms: Vec::new(),
            cancelled: None,
        }
    }

    fn report(&self, stage: ConnectStage) {
        if !self
            .cancelled
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Acquire))
            && let Some(progress) = &self.progress
        {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| progress(stage)));
        }
    }

    fn validate(&self) -> Result<()> {
        if self.host.is_empty()
            || self.host.contains(['\0', '\n', '\r'])
            || self.window_size == 0
            || self.port == 0
            || self.username.is_empty()
            || self.username.contains('\0')
            || [self.connect_timeout, self.auth_timeout, self.io_timeout].contains(&Duration::ZERO)
        {
            return Err(Error::InvalidInput);
        }
        Ok(())
    }

    fn config(&self) -> client::Config {
        let mut config = client::Config {
            window_size: self.window_size,
            maximum_packet_size: (32 * 1024).min(self.window_size),
            channel_buffer_size: 8,
            keepalive_interval: Some(Duration::from_secs(15)),
            keepalive_max: 3,
            nodelay: true,
            ..Default::default()
        };
        if self.legacy_algorithms {
            config
                .preferred
                .kex
                .to_mut()
                .extend([russh::kex::DH_G14_SHA1, russh::kex::DH_G1_SHA1]);
            config.preferred.mac.to_mut().push(russh::mac::HMAC_SHA1);
        } else {
            config.preferred.key = Cow::Owned(
                config
                    .preferred
                    .key
                    .iter()
                    .filter(|key| !matches!(key, Algorithm::Rsa { hash: None }))
                    .cloned()
                    .collect(),
            );
        }
        // RSA 的 known_hosts 类型是 ssh-rsa，握手仍优先使用允许的 RSA/SHA-2 签名。
        config.preferred.key.to_mut().sort_by_key(|algorithm| {
            let name = if matches!(algorithm, Algorithm::Rsa { .. }) {
                "ssh-rsa".to_owned()
            } else {
                algorithm.to_string()
            };
            !self.preferred_host_key_algorithms.contains(&name)
        });
        config
    }
}

/// PTY 初始大小；像素尺寸交给终端按需填写。
#[derive(Clone, Debug)]
pub struct Pty {
    pub term: String,
    pub columns: u32,
    pub rows: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

impl Default for Pty {
    fn default() -> Self {
        Self {
            term: "xterm-256color".into(),
            columns: 80,
            rows: 24,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// 无秘密的累计测量；key_exchanges 包含首次握手。
#[derive(Clone, Copy, Debug, Default)]
pub struct Statistics {
    pub key_exchanges: u64,
    pub hostkey_announcements: u64,
    pub received_bytes: u64,
    /// 累计成功打开的 session 通道数，含已关闭的会话。
    pub opened_sessions: u64,
    /// 累计发起的 session 通道请求数，包含等待响应和被拒绝的请求。
    pub started_sessions: u64,
}

#[derive(Default)]
struct Counters {
    kex: AtomicU64,
    hostkeys: AtomicU64,
    received: AtomicU64,
    sessions: AtomicU64,
    session_starts: AtomicU64,
}

pub(crate) struct Client {
    options: ConnectionOptions,
    remote_sshid: Arc<OnceLock<String>>,
    counters: Arc<Counters>,
    forwarded: mpsc::Sender<Incoming>,
    routed: Arc<Mutex<HashMap<u16, mpsc::Sender<ChannelStream<client::Msg>>>>>,
    allowed_ports: Arc<Mutex<HashSet<u32>>>,
    pending_forward: Arc<AtomicUsize>,
    closed: tokio::sync::watch::Sender<bool>,
    disconnect: Arc<auth::DisconnectInfo>,
}

struct Incoming {
    channel: Channel<client::Msg>,
    port: u16,
    originator_address: String,
    originator_port: u16,
}

impl client::Handler for Client {
    type Error = Error;

    async fn disconnected(&mut self, reason: client::DisconnectReason<Self::Error>) -> Result<()> {
        if let client::DisconnectReason::ReceivedDisconnect(info) = reason {
            self.disconnect.record(info);
        }
        self.closed.send_replace(true);
        Ok(())
    }

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool> {
        if key.certificate().is_some() {
            return Ok(false);
        }
        let key = key.public_key();
        let info = HostKey {
            host: self.options.host.clone(),
            port: self.options.port,
            algorithm: key.algorithm().to_string(),
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
            public_key: key.public_key_base64(),
        };
        match &self.options.host_key {
            HostKeyCheck::Fingerprint(expected) => Ok(info.fingerprint == *expected),
            HostKeyCheck::Callback(callback) => {
                let callback = callback.clone();
                tokio::task::spawn_blocking(move || callback(info))
                    .await
                    .map_err(|_| Error::Runtime)
            }
        }
    }

    async fn kex_done(
        &mut self,
        _: Option<&[u8]>,
        _: &russh::Names,
        session: &mut client::Session,
    ) -> Result<()> {
        self.remote_sshid
            .get_or_init(|| String::from_utf8_lossy(session.remote_sshid()).into_owned());
        self.counters.kex.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn openssh_ext_host_keys_announced(
        &mut self,
        _: Vec<PublicKey>,
        _: &mut client::Session,
    ) -> Result<()> {
        // 公告只计数，不把未经持有证明的新密钥写入信任记录。
        self.counters.hostkeys.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn data(
        &mut self,
        _: russh::ChannelId,
        data: &[u8],
        _: &mut client::Session,
    ) -> Result<()> {
        self.counters
            .received
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    async fn extended_data(
        &mut self,
        _: russh::ChannelId,
        _: u32,
        data: &[u8],
        _: &mut client::Session,
    ) -> Result<()> {
        self.counters
            .received
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<client::Msg>,
        address: &str,
        port: u32,
        originator_address: &str,
        originator_port: u32,
        reply: client::ChannelOpenHandle,
        _: &mut client::Session,
    ) -> Result<()> {
        let routed = u16::try_from(port).ok().and_then(|port| {
            self.routed
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&port)
                .cloned()
        });
        if address == "127.0.0.1"
            && let Some(sender) = routed
        {
            if let Ok(slot) = sender.try_reserve() {
                reply.accept().await;
                // 先转为自动关闭的流；撤销监听与接受应答并发时，队列丢弃也会发送关闭。
                slot.send(channel.into_stream());
                return Ok(());
            }
            reply.reject(ChannelOpenFailure::ResourceShortage).await;
            return Ok(());
        }
        let allowed = address == "127.0.0.1"
            && (self.pending_forward.load(Ordering::Acquire) > 0
                || self
                    .allowed_ports
                    .lock()
                    .map_err(|_| Error::Runtime)?
                    .contains(&port));
        if allowed {
            // 先预留队列位置再接受；队列满则拒绝，不能阻塞 SSH 事件循环。
            if let Ok(slot) = self.forwarded.try_reserve() {
                reply.accept().await;
                let port = u16::try_from(port).map_err(|_| Error::Protocol)?;
                let originator_port =
                    u16::try_from(originator_port).map_err(|_| Error::Protocol)?;
                slot.send(Incoming {
                    channel,
                    port,
                    originator_address: originator_address.into(),
                    originator_port,
                });
                return Ok(());
            }
        }
        reply
            .reject(ChannelOpenFailure::AdministrativelyProhibited)
            .await;
        Ok(())
    }
}

/// 专用的双工作线程运行时，可创建多台服务器的连接。
#[derive(Clone)]
pub struct Engine {
    pub(crate) executor: Arc<Executor>,
}

impl Engine {
    /// 创建专用 tokio 运行时；失败返回受控运行时错误。
    pub fn new() -> Result<Self> {
        Ok(Self {
            executor: Arc::new(Executor::new()?),
        })
    }

    /// 按参数连接并只执行指定认证；返回共享连接，不读取 ssh config 或 known_hosts。
    pub fn connect(&self, options: ConnectionOptions, auth: Authentication) -> Result<Connection> {
        options.validate()?;
        let stream = self
            .executor
            .run_cancellable(
                options.connect_timeout,
                "TCP 连接",
                options.cancelled.clone(),
                tcp_connect(&options),
            )
            .map_err(|error| match (&options.proxy, error) {
                (Some(_), Error::Timeout(_)) => Error::Proxy(crate::proxy::ProxyError::Timeout),
                (_, error) => error,
            })?;
        self.connect_stream(options, auth, stream, None)
    }

    /// 在已登录跳板的 direct-tcpip 通道里握手；每一级仍使用 options 的独立主机校验。
    pub fn connect_via(
        &self,
        parent: &Connection,
        options: ConnectionOptions,
        auth: Authentication,
    ) -> Result<Connection> {
        options.validate()?;
        options.report(ConnectStage::Connect);
        let channel = parent.inner.executor.run_cancellable(
            options.connect_timeout,
            "跳板通道",
            options.cancelled.clone(),
            async {
                parent
                    .inner
                    .handle
                    .read()
                    .await
                    .channel_open_direct_tcpip(
                        &options.host,
                        u32::from(options.port),
                        "127.0.0.1",
                        0,
                    )
                    .await
                    .map_err(Error::from)
            },
        )?;
        self.connect_stream(options, auth, channel.into_stream(), Some(parent.clone()))
    }

    pub(crate) fn connect_stream<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        &self,
        mut options: ConnectionOptions,
        auth: Authentication,
        stream: S,
        parent: Option<Connection>,
    ) -> Result<Connection> {
        options.report(ConnectStage::Auth);
        // 登录后主连接可能长期复用，不让连接持有已经关闭的终端进度回调。
        options.progress = None;
        // 代理握手已完成，后续通道复用现有连接，不再需要保存出站代理设置。
        options.proxy = None;
        let counters = Arc::new(Counters::default());
        let remote_sshid = Arc::new(OnceLock::new());
        let allowed_ports = Arc::new(Mutex::new(HashSet::new()));
        let pending_forward = Arc::new(AtomicUsize::new(0));
        let routed = Arc::new(Mutex::new(HashMap::new()));
        let (forwarded, incoming) = mpsc::channel(8);
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(false);
        let disconnect = Arc::new(auth::DisconnectInfo::default());
        let client = Client {
            remote_sshid: remote_sshid.clone(),
            disconnect: disconnect.clone(),
            closed: closed_tx.clone(),
            options: options.clone(),
            counters: counters.clone(),
            forwarded,
            routed: routed.clone(),
            allowed_ports: allowed_ports.clone(),
            pending_forward: pending_forward.clone(),
        };
        let (transport_guard, closed) = tokio::sync::oneshot::channel();
        let stream = Transport { stream, closed };
        let mut handle = self.executor.run_cancellable(
            options.connect_timeout,
            "SSH 握手",
            options.cancelled.clone(),
            async { client::connect_stream(Arc::new(options.config()), stream, client).await },
        )?;
        let authenticated = self.executor.run_cancellable(
            options.auth_timeout,
            "认证",
            options.cancelled.clone(),
            auth::authenticate(
                &mut handle,
                &options.username,
                auth,
                options.legacy_algorithms,
                &disconnect,
            ),
        );
        if let Err(error) = authenticated {
            // 直接释放传输层守卫，取消不能再等待一次可能阻塞的网络断开。
            drop(transport_guard);
            return Err(error);
        }
        Ok(Connection {
            inner: Arc::new(Inner {
                handle: Arc::new(tokio::sync::RwLock::new(handle)),
                pending_channels: Arc::new(tokio::sync::Semaphore::new(128)),
                executor: self.executor.clone(),
                timeout: options.io_timeout,
                closed: closed_rx,
                close_signal: closed_tx,
                counters,
                remote_sshid,
                incoming: tokio::sync::Mutex::new(incoming),
                routed,
                allowed_ports,
                pending_forward,
                _parent: parent,
                transport_guard: Mutex::new(Some(transport_guard)),
                window_size: options.window_size,
            }),
        })
    }

    /// 列举当前平台 agent 的普通公钥；不返回注释、私钥或证书。
    pub fn agent_keys(&self) -> Result<Vec<PublicKey>> {
        self.executor
            .run(Duration::from_secs(10), "agent 列举", async {
                let mut agent = auth::agent().await?;
                Ok(agent
                    .request_identities()
                    .await
                    .map_err(|_| Error::Agent)?
                    .into_iter()
                    .filter_map(|identity| match identity {
                        russh::keys::agent::AgentIdentity::PublicKey { key, .. } => Some(key),
                        _ => None,
                    })
                    .collect())
            })
    }
}

async fn tcp_connect(options: &ConnectionOptions) -> Result<tokio::net::TcpStream> {
    options.report(ConnectStage::Resolve);
    if let Some(proxy) = &options.proxy {
        options.report(ConnectStage::Connect);
        return crate::proxy::connect_async(
            proxy,
            &options.host,
            options.port,
            options.connect_timeout,
        )
        .await
        .map_err(Error::from);
    }
    let addresses = lookup_host((options.host.as_str(), options.port))
        .await
        .map_err(|_| Error::Dns)?;
    options.report(ConnectStage::Connect);
    let mut last = Error::Dns;
    for address in addresses {
        let socket = if address.is_ipv6() {
            TcpSocket::new_v6()?
        } else {
            TcpSocket::new_v4()?
        };
        socket.set_keepalive(true)?;
        socket.set_nodelay(true)?;
        match socket.connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Error::from(error),
        }
    }
    Err(last)
}

pub(crate) struct Inner {
    pub(crate) handle: Arc<tokio::sync::RwLock<client::Handle<Client>>>,
    pub(crate) pending_channels: Arc<tokio::sync::Semaphore>,
    pub(crate) routed: Arc<Mutex<HashMap<u16, mpsc::Sender<ChannelStream<client::Msg>>>>>,
    executor: Arc<Executor>,
    timeout: Duration,
    closed: tokio::sync::watch::Receiver<bool>,
    close_signal: tokio::sync::watch::Sender<bool>,
    counters: Arc<Counters>,
    remote_sshid: Arc<OnceLock<String>>,
    incoming: tokio::sync::Mutex<mpsc::Receiver<Incoming>>,
    pub(crate) allowed_ports: Arc<Mutex<HashSet<u32>>>,
    pending_forward: Arc<AtomicUsize>,
    _parent: Option<Connection>,
    transport_guard: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    window_size: u32,
}

// russh 的 Handle::drop 不取消连接任务，故传输层另持有可唤醒的关闭信号。
pub(crate) struct Transport<S> {
    pub(crate) stream: S,
    pub(crate) closed: tokio::sync::oneshot::Receiver<()>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Transport<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if std::pin::Pin::new(&mut self.closed).poll(cx).is_ready() {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "连接已释放",
            )));
        }
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Transport<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// 一条已登录连接；clone 共享它，通道和下级跳板持有引用直至关闭。
#[derive(Clone)]
pub struct Connection {
    pub(crate) inner: Arc<Inner>,
}

#[derive(Clone)]
/// 供关闭句柄保存的连接弱引用，不延长连接和运行时寿命。
pub(crate) struct WeakConnection(Weak<Inner>);

#[cfg(test)]
#[path = "close_tests.rs"]
mod close_tests;

impl WeakConnection {
    /// 无参数；返回仍存活的共享连接，连接已释放时返回 None。
    pub(crate) fn upgrade(&self) -> Option<Connection> {
        self.0.upgrade().map(|inner| Connection { inner })
    }
}

impl Connection {
    /// 无参数；返回首次握手收到的服务器 SSH 版本字符串，尚未收到时返回 None。
    /// 非 UTF-8 字节替换为替代字符；克隆和池中复用的连接共享此值。
    pub fn remote_sshid(&self) -> Option<&str> {
        self.inner.remote_sshid.get().map(String::as_str)
    }

    /// 无参数；返回不持有连接所有权的弱引用。
    pub(crate) fn downgrade(&self) -> WeakConnection {
        WeakConnection(Arc::downgrade(&self.inner))
    }
    pub(crate) fn executor(&self) -> Arc<Executor> {
        self.inner.executor.clone()
    }
    pub(crate) fn run<T: Send>(
        &self,
        name: &'static str,
        future: impl Future<Output = Result<T>> + Send,
    ) -> Result<T> {
        self.inner.executor.run(self.inner.timeout, name, future)
    }

    pub(crate) fn wait<T: Send>(
        &self,
        future: impl Future<Output = Result<T>> + Send,
    ) -> Result<T> {
        let mut closed = self.inner.closed.clone();
        self.inner.executor.wait(async {
            tokio::select! {
                biased;
                _ = closed.wait_for(|closed| *closed) => Err(Error::Disconnected),
                result = future => result,
            }
        })
    }

    /// 打开 exec 会话，先逐一确认 env，再确认 exec；返回独立的 stdin/stdout/stderr。
    pub fn exec(&self, command: &str, env: &[(&str, &str)]) -> Result<Session> {
        if command.contains('\0') {
            return Err(Error::InvalidInput);
        }
        let channel = self.session_start(env, None, Some(command), None)?;
        Session::new(channel, self.clone())
    }

    /// 请求 PTY 后启动 shell；返回字节流，窗口变化通过 Session::resize 发送。
    pub fn shell(&self, pty: &Pty, env: &[(&str, &str)]) -> Result<Session> {
        if pty.columns == 0 || pty.rows == 0 || pty.term.contains('\0') {
            return Err(Error::InvalidInput);
        }
        let channel = self.session_start(env, Some(pty), None, None)?;
        Session::new(channel, self.clone())
    }

    /// 按 pty 打开交互终端；command 为 None 时请求登录 shell，否则执行已转义命令。
    /// env 与 OpenSSH SendEnv 一致，不要求服务器接受；cancelled 取消打开过程。
    /// 返回的 stdout 按协议顺序合并普通和扩展输出，stderr 为空。
    pub fn terminal(
        &self,
        pty: &Pty,
        env: &[(&str, &str)],
        command: Option<&str>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Session> {
        if pty.columns == 0
            || pty.rows == 0
            || pty.term.contains('\0')
            || command.is_some_and(|c| c.contains('\0'))
        {
            return Err(Error::InvalidInput);
        }
        let channel = self.session_start_controlled(
            env,
            Some(pty),
            command,
            None,
            Some(cancelled),
            self.inner.timeout,
            false,
        )?;
        Session::with_output(channel, self.clone(), true)
    }

    /// 按 command 和 env 打开可取消的 exec 会话；timeout 限制开通道和请求确认的总等待。
    /// cancelled 仅覆盖打开过程；已打开会话通过 closer 取消。
    pub fn exec_cancellable(
        &self,
        command: &str,
        env: &[(&str, &str)],
        cancelled: Arc<AtomicBool>,
        timeout: Duration,
    ) -> Result<Session> {
        if command.contains('\0') || timeout.is_zero() {
            return Err(Error::InvalidInput);
        }
        let channel = self.session_start_controlled(
            env,
            None,
            Some(command),
            None,
            Some(cancelled),
            timeout,
            true,
        )?;
        Session::new(channel, self.clone())
    }

    /// 打开 sftp 子系统并返回独立输入和双输出流，不交换 SFTP 协议包。
    /// cancelled 和 timeout 限制开通道及子系统确认；已打开通道由返回会话负责关闭。
    pub fn sftp_subsystem(&self, cancelled: Arc<AtomicBool>, timeout: Duration) -> Result<Session> {
        if timeout.is_zero() {
            return Err(Error::InvalidInput);
        }
        let channel = self
            .session_start_controlled(
                &[],
                None,
                None,
                Some("sftp"),
                Some(cancelled),
                timeout,
                true,
            )
            .map_err(|error| match error {
                Error::RequestRejected(_) => Error::RequestRejected("sftp"),
                error => error,
            })?;
        Session::new(channel, self.clone())
    }

    pub(crate) fn session_start(
        &self,
        env: &[(&str, &str)],
        pty: Option<&Pty>,
        command: Option<&str>,
        subsystem: Option<&str>,
    ) -> Result<Channel<client::Msg>> {
        self.session_start_controlled(env, pty, command, subsystem, None, self.inner.timeout, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn session_start_controlled(
        &self,
        env: &[(&str, &str)],
        pty: Option<&Pty>,
        command: Option<&str>,
        subsystem: Option<&str>,
        cancelled: Option<Arc<AtomicBool>>,
        timeout: Duration,
        require_env: bool,
    ) -> Result<Channel<client::Msg>> {
        if env
            .iter()
            .any(|(key, value)| key.is_empty() || key.contains(['\0', '=']) || value.contains('\0'))
        {
            return Err(Error::InvalidInput);
        }
        self.inner
            .executor
            .run_cancellable(timeout, "打开会话", cancelled, async {
                if self.is_closed() {
                    return Err(Error::Disconnected);
                }
                self.inner
                    .counters
                    .session_starts
                    .fetch_add(1, Ordering::Relaxed);
                let channel = self
                    .inner
                    .handle
                    .read()
                    .await
                    .channel_open_session()
                    .await?;
                self.inner.counters.sessions.fetch_add(1, Ordering::Relaxed);
                let mut guarded = channel::Opening::new(channel);
                let channel = guarded.channel.as_mut().ok_or(Error::Disconnected)?;
                for (key, value) in env {
                    channel.set_env(require_env, *key, *value).await?;
                    if require_env {
                        channel::ack(channel, "环境变量").await?;
                    }
                }
                if let Some(pty) = pty {
                    channel
                        .request_pty(
                            true,
                            &pty.term,
                            pty.columns,
                            pty.rows,
                            pty.pixel_width,
                            pty.pixel_height,
                            &[],
                        )
                        .await?;
                    channel::ack(channel, "PTY").await?;
                }
                if let Some(command) = command {
                    channel.exec(true, command).await?;
                } else if let Some(subsystem) = subsystem {
                    channel.request_subsystem(true, subsystem).await?;
                } else {
                    channel.request_shell(true).await?;
                }
                channel::ack(channel, "启动会话").await?;
                guarded.channel.take().ok_or(Error::Disconnected)
            })
    }

    /// 打开到服务器可见 TCP 地址的双向流；目标拒绝和转发禁止均保留通道拒绝码。
    pub fn direct_tcpip(&self, host: &str, port: u16) -> Result<Tunnel> {
        if host.is_empty() || host.contains('\0') || port == 0 {
            return Err(Error::InvalidInput);
        }
        let channel = self.run("TCP 转发", async {
            self.inner
                .handle
                .read()
                .await
                .channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", 0)
                .await
                .map_err(Error::from)
        })?;
        Ok(Tunnel::new(channel, self.clone()))
    }

    /// 用 direct-streamlocal@openssh.com 打开远程 Unix 套接字；不拼接 shell 命令。
    pub fn direct_streamlocal(&self, path: &str) -> Result<Tunnel> {
        if path.is_empty() || path.contains('\0') {
            return Err(Error::InvalidInput);
        }
        let channel = self.run("套接字转发", async {
            self.inner
                .handle
                .read()
                .await
                .channel_open_direct_streamlocal(path)
                .await
                .map_err(Error::from)
        })?;
        Ok(Tunnel::new(channel, self.clone()))
    }

    /// 请求仅监听远程回环地址的转发；port 为 0 时返回服务器分配的端口。
    pub fn remote_forward(&self, port: u16) -> Result<u16> {
        if self
            .inner
            .routed
            .lock()
            .map_err(|_| Error::Runtime)?
            .contains_key(&port)
        {
            return Err(Error::RequestRejected("远程转发"));
        }
        // 服务器可能紧接 REQUEST_SUCCESS 发送 forwarded-tcpip，此时调用线程尚未登记分配端口。
        // 暂存这些连接，交给 accept_forward 再核验最终登记的端口。
        self.inner.pending_forward.fetch_add(1, Ordering::AcqRel);
        struct Pending(Arc<AtomicUsize>);
        impl Drop for Pending {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _pending = Pending(self.inner.pending_forward.clone());
        let assigned = self.run("远程转发", async {
            self.inner
                .handle
                .read()
                .await
                .tcpip_forward("127.0.0.1", u32::from(port))
                .await
                .map_err(Error::from)
        })?;
        let actual = if port == 0 {
            u16::try_from(assigned).map_err(|_| Error::Protocol)?
        } else {
            port
        };
        if actual == 0 {
            return Err(Error::Protocol);
        }
        self.inner
            .allowed_ports
            .lock()
            .map_err(|_| Error::Runtime)?
            .insert(u32::from(actual));
        Ok(actual)
    }

    /// 取出服务器转发来的下一条流；队列最多 8 条，超时返回明确错误。
    pub fn accept_forward(&self) -> Result<Forwarded> {
        let incoming = self.run("等待远程转发", async {
            loop {
                let incoming = self
                    .inner
                    .incoming
                    .lock()
                    .await
                    .recv()
                    .await
                    .ok_or(Error::Disconnected)?;
                if self
                    .inner
                    .allowed_ports
                    .lock()
                    .map_err(|_| Error::Runtime)?
                    .contains(&u32::from(incoming.port))
                {
                    return Ok(incoming);
                }
                drop(incoming.channel.into_stream());
            }
        })?;
        Ok(Forwarded {
            connected_port: incoming.port,
            originator_address: incoming.originator_address,
            originator_port: incoming.originator_port,
            stream: Tunnel::new(incoming.channel, self.clone()),
        })
    }

    /// 撤销远程监听；已接受的流继续由调用方管理。
    pub fn cancel_forward(&self, port: u16) -> Result<()> {
        self.run("撤销远程转发", async {
            self.inner
                .handle
                .read()
                .await
                .cancel_tcpip_forward("127.0.0.1", u32::from(port))
                .await
                .map_err(Error::from)
        })?;
        self.inner
            .allowed_ports
            .lock()
            .map_err(|_| Error::Runtime)?
            .remove(&u32::from(port));
        Ok(())
    }

    /// 无参数；返回累计握手、主机密钥公告、接收字节及会话请求数，不访问网络。
    pub fn statistics(&self) -> Statistics {
        let c = &self.inner.counters;
        Statistics {
            key_exchanges: c.kex.load(Ordering::Relaxed),
            hostkey_announcements: c.hostkeys.load(Ordering::Relaxed),
            received_bytes: c.received.load(Ordering::Relaxed),
            opened_sessions: c.sessions.load(Ordering::Relaxed),
            started_sessions: c.session_starts.load(Ordering::Relaxed),
        }
    }

    /// 请求并等待服务器 SSH ping，应答失败或超时返回错误。
    pub fn ping(&self) -> Result<()> {
        self.run("SSH ping", async {
            self.inner
                .handle
                .read()
                .await
                .send_ping()
                .await
                .map_err(Error::from)
        })
    }

    /// 无参数；返回连接或任一级跳板是否已经断开，不发送网络请求。
    pub fn is_closed(&self) -> bool {
        *self.inner.closed.borrow()
            || self
                .inner
                ._parent
                .as_ref()
                .is_some_and(Connection::is_closed)
    }

    /// 无参数；返回是否只有当前所有者持有连接，通道和下级跳板也算使用者。
    pub fn is_exclusively_owned(&self) -> bool {
        Arc::strong_count(&self.inner) == 1
    }

    /// 与 other 比较底层连接身份；不访问网络或暴露地址。
    pub fn same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// 无参数；返回本连接每通道的接收窗口字节数。
    pub fn window_size(&self) -> u32 {
        self.inner.window_size
    }

    /// 在 timeout 内立即探活；超时或断开返回错误，timeout 不得为零。
    pub fn probe(&self, timeout: Duration) -> Result<()> {
        if timeout.is_zero() {
            return Err(Error::InvalidInput);
        }
        if self.is_closed() {
            return Err(Error::Disconnected);
        }
        let result = self.inner.executor.run(timeout, "SSH 探活", async {
            self.inner
                .handle
                .read()
                .await
                .send_ping()
                .await
                .map_err(Error::from)
        });
        if self.is_closed() {
            Err(Error::Disconnected)
        } else {
            result
        }
    }

    /// 立即切断传输并唤醒全部通道；不等待对端，也不等待出站队列腾出空间。
    pub fn abort(&self) {
        self.inner.close_signal.send_replace(true);
        self.inner
            .transport_guard
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
    }

    /// 显式断开共享连接及其全部通道；至多等待三秒让协议任务退出。
    pub fn disconnect(&self) -> Result<()> {
        self.abort();
        self.wait_disconnected(Duration::from_secs(3))
    }

    /// 至多等待 timeout 让已关闭连接的协议任务退出；超时返回受控错误。
    pub fn wait_disconnected(&self, timeout: Duration) -> Result<()> {
        self.inner.executor.run(timeout, "断开连接", async {
            loop {
                if self.inner.handle.read().await.is_closed() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 已知类型只调整顺序且_rsa_仍使用允许的_sha2_签名() {
        let mut opts =
            ConnectionOptions::new("host", 22, "用户", HostKeyCheck::Fingerprint(String::new()));
        assert!(opts.preferred_host_key_algorithms.is_empty());
        let defaults = opts.config().preferred.key.into_owned();
        opts.preferred_host_key_algorithms = vec!["ssh-rsa".into(), "不存在的类型".into()];
        let preferred = opts.config().preferred.key.into_owned();
        assert!(matches!(preferred[0], Algorithm::Rsa { .. }));
        assert!(!preferred.contains(&Algorithm::Rsa { hash: None }));
        assert_eq!(preferred.len(), defaults.len());
        assert!(
            defaults
                .iter()
                .all(|algorithm| preferred.contains(algorithm))
        );
        assert_eq!(
            preferred
                .iter()
                .filter(|a| matches!(a, Algorithm::Rsa { .. }))
                .collect::<Vec<_>>(),
            defaults
                .iter()
                .filter(|a| matches!(a, Algorithm::Rsa { .. }))
                .collect::<Vec<_>>()
        );
        opts.preferred_host_key_algorithms = vec!["ecdsa-sha2-nistp256".into()];
        assert_eq!(
            opts.config().preferred.key[0].to_string(),
            "ecdsa-sha2-nistp256"
        );
        opts.legacy_algorithms = true;
        opts.preferred_host_key_algorithms = vec!["ssh-rsa".into()];
        assert!(
            opts.config()
                .preferred
                .key
                .contains(&Algorithm::Rsa { hash: None })
        );
    }

    #[test]
    fn defaults_validation_and_legacy_are_explicit() {
        let mut opts = ConnectionOptions::new(
            "::1",
            22,
            "测试",
            HostKeyCheck::Fingerprint("SHA256:测试".into()),
        );
        assert!(opts.validate().is_ok());
        assert_eq!(opts.config().window_size, 256 * 1024);
        opts.window_size = 2 * 1024 * 1024;
        assert_eq!(opts.config().window_size, 2 * 1024 * 1024);
        opts.window_size = 0;
        assert_eq!(opts.validate(), Err(Error::InvalidInput));
        opts.window_size = 256 * 1024;
        assert!(
            !opts
                .config()
                .preferred
                .key
                .contains(&Algorithm::Rsa { hash: None })
        );
        opts.legacy_algorithms = true;
        assert!(
            opts.config()
                .preferred
                .key
                .contains(&Algorithm::Rsa { hash: None })
        );
        assert!(
            opts.config()
                .preferred
                .kex
                .contains(&russh::kex::DH_G14_SHA1)
        );
        opts.port = 0;
        assert_eq!(opts.validate(), Err(Error::InvalidInput));
        assert_eq!(Pty::default().columns, 80);
        assert!(Engine::new().is_ok());
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    fn options(stages: Arc<Mutex<Vec<ConnectStage>>>) -> ConnectionOptions {
        let mut options = ConnectionOptions::new(
            "127.0.0.1",
            1,
            "测试",
            HostKeyCheck::Callback(Arc::new(|_| true)),
        );
        options.progress = Some(Arc::new(move |stage| stages.lock().unwrap().push(stage)));
        options
    }

    #[test]
    fn 已登录主连接不再持有终端阶段回调() {
        let engine = Engine::new().unwrap();
        let stages = Arc::new(Mutex::new(Vec::new()));
        let weak = Arc::downgrade(&stages);
        let options = options(stages.clone());
        let connection = crate::test_support::connect_sessions(
            &engine,
            options,
            Authentication::Password("测试密码".into()),
        )
        .unwrap();
        assert_eq!(*stages.lock().unwrap(), [ConnectStage::Auth]);
        drop(stages);
        assert!(weak.upgrade().is_none());
        connection.ping().unwrap();
    }

    #[test]
    fn 解析失败停在解析且预先取消不报告后续阶段() {
        let engine = Engine::new().unwrap();
        let stages = Arc::new(Mutex::new(Vec::new()));
        let mut options = options(stages.clone());
        // 含 NUL 的主机被参数校验拒绝；空格名称由本机解析器直接拒绝，不请求外部 DNS。
        options.host = " ".into();
        assert!(matches!(
            engine.connect(options.clone(), Authentication::Password("测试".into())),
            Err(Error::Dns)
        ));
        assert_eq!(*stages.lock().unwrap(), [ConnectStage::Resolve]);
        stages.lock().unwrap().clear();
        options.cancelled = Some(Arc::new(AtomicBool::new(true)));
        assert!(matches!(
            engine.connect(options, Authentication::Password("测试".into())),
            Err(Error::Callback)
        ));
        assert!(stages.lock().unwrap().is_empty());
    }

    #[test]
    fn 握手失败或握手中取消都停在认证阶段() {
        for cancel in [false, true] {
            let engine = Engine::new().unwrap();
            let stages = Arc::new(Mutex::new(Vec::new()));
            let mut options = options(stages.clone());
            options.connect_timeout = Duration::from_millis(50);
            let cancelled = Arc::new(AtomicBool::new(false));
            options.cancelled = Some(cancelled.clone());
            if cancel {
                let observed = stages.clone();
                options.progress = Some(Arc::new(move |stage| {
                    observed.lock().unwrap().push(stage);
                    cancelled.store(true, Ordering::Release);
                }));
            }
            let (stream, _peer) = tokio::io::duplex(1024);
            let error = engine
                .connect_stream(
                    options,
                    Authentication::Password("测试".into()),
                    stream,
                    None,
                )
                .err()
                .unwrap();
            assert_eq!(
                error,
                if cancel {
                    Error::Callback
                } else {
                    Error::Timeout("SSH 握手")
                }
            );
            assert_eq!(*stages.lock().unwrap(), [ConnectStage::Auth]);
        }
    }
}
