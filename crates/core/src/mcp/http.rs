//! 仅监听 IPv4 回环的 MCP HTTP 入口，认证通过后交给官方协议实现。
use super::{
    Entry, McpService, lock,
    service::{Handler, Shared, server_info},
    session::Session,
};
use crate::error::AppError;
use bytes::Bytes;
use hmac::digest::{CtOutput, OutputSizeUser, consts::U32};
use http::{Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, DiscoverResult, Implementation,
        InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
        ServerConfig, Tool,
    },
    service::RequestContext,
    transport::streamable_http_server::{
        SessionManager, StreamableHttpServerConfig, StreamableHttpService,
        session::local::LocalSessionManager,
    },
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    convert::Infallible,
    io,
    net::{Ipv4Addr, TcpListener},
    pin::Pin,
    sync::{Arc, Mutex, OnceLock, atomic::AtomicBool},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{Semaphore, watch},
    task::{JoinHandle, JoinSet},
    time::Instant,
};
use zeroize::Zeroizing;

const MAX_BODY: usize = 8 * 1024 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const SESSION_KEEP_ALIVE: Duration = Duration::from_secs(30 * 60);
type HttpResponse = Response<BoxBody<Bytes, Infallible>>;

/// 服务器入口的本机监听端口，供宿主建立远程转发。
#[derive(Debug)]
pub struct HttpEntry {
    pub local_port: u16,
}

pub(super) struct EntryWorker {
    current: Arc<Mutex<Arc<Generation>>>,
    listener: JoinHandle<()>,
    timeout: Duration,
}
impl Drop for EntryWorker {
    fn drop(&mut self) {
        lock(&self.current).close();
        self.listener.abort();
    }
}

impl McpService {
    /// 为 server_id 绑定 127.0.0.1 随机端口，替换同入口的旧监听与会话；返回端口或启动错误。
    /// token 为空时拒绝启动；令牌自动登记到脱敏表。
    pub fn start_http_entry(
        &self,
        server_id: &str,
        token: Zeroizing<String>,
    ) -> Result<HttpEntry, AppError> {
        self.start_http_with_timeout(server_id, token, IDLE_TIMEOUT)
    }

    /// 为 server_id 启动入口，timeout 限制请求头读取和空闲时间；返回端口或启动错误。
    pub(super) fn start_http_with_timeout(
        &self,
        server_id: &str,
        token: Zeroizing<String>,
        timeout: Duration,
    ) -> Result<HttpEntry, AppError> {
        let mut entries = lock(&self.http_entries);
        entries.remove(server_id);
        if token.is_empty() {
            return Err(AppError::new("mcp.invalid_token")
                .with_reason("mcp.entry_token_cannot_be_empty", "入口令牌不能为空"));
        }
        self.add_secret(token.clone());
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(http_error)?;
        listener.set_nonblocking(true).map_err(http_error)?;
        let local_port = listener.local_addr().map_err(http_error)?.port();
        let handle = self.runtime();
        let _entered = handle.enter();
        let listener = tokio::net::TcpListener::from_std(listener).map_err(http_error)?;
        let current = Arc::new(Mutex::new(Generation::new(
            self.shared.clone(),
            server_id,
            token,
            timeout,
            handle.clone(),
        )));
        let worker_current = current.clone();
        let worker = handle.spawn(async move {
            let permits = Arc::new(Semaphore::new(32));
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    Some(_) = connections.join_next(), if !connections.is_empty() => {},
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let Ok(permit) = permits.clone().try_acquire_owned() else { continue };
                        let generation = lock(&worker_current).clone();
                        connections.spawn(async move {
                            let _permit = permit;
                            serve_connection(stream, generation, timeout).await;
                        });
                    }
                }
            }
            lock(&worker_current).close();
        });
        entries.insert(
            server_id.into(),
            EntryWorker {
                current,
                listener: worker,
                timeout,
            },
        );
        Ok(HttpEntry { local_port })
    }

    /// 停止 server_id 的监听，取消该入口所有 HTTP 会话与调用；无返回值，可重复调用。
    pub fn stop_http_entry(&self, server_id: &str) {
        lock(&self.http_entries).remove(server_id);
    }

    /// 将 server_id 的活动入口换成 token，断开旧连接和会话；入口未启动时无操作。
    /// 空令牌会停用入口，防止空 Bearer 值通过认证；无返回值。
    pub fn set_token(&self, server_id: &str, token: Zeroizing<String>) {
        if token.is_empty() {
            self.stop_http_entry(server_id);
            return;
        }
        let entries = lock(&self.http_entries);
        let Some(worker) = entries.get(server_id) else {
            return;
        };
        self.add_secret(token.clone());
        let mut current = lock(&worker.current);
        current.close();
        *current = Generation::new(
            self.shared.clone(),
            server_id,
            token,
            worker.timeout,
            self.runtime(),
        );
    }

    /// 无参数、无返回值；断开各 HTTP 入口的连接与会话，保留端口和令牌供后续连接。
    pub(super) fn reset_http_sessions(&self) {
        for (server_id, worker) in lock(&self.http_entries).iter() {
            let mut current = lock(&worker.current);
            current.close();
            *current = Generation::new(
                self.shared.clone(),
                server_id,
                current.token.clone(),
                worker.timeout,
                self.runtime(),
            );
        }
    }
}

fn http_error(error: io::Error) -> AppError {
    AppError::new("mcp.http_failed")
        .with_reason(
            "mcp.couldn_t_start_the_local_mcp_http_listener",
            "无法启动本机 MCP HTTP 监听",
        )
        .with("io_kind", format!("{:?}", error.kind()))
}

// 每次令牌更新都换一代协议服务，旧会话 ID 和正在接收的请求不能越过更新边界。
struct Generation {
    token: Zeroizing<String>,
    http: StreamableHttpService<HttpHandler, LocalSessionManager>,
    group: Arc<SessionGroup>,
    manager: Arc<LocalSessionManager>,
    runtime: tokio::runtime::Handle,
}
impl Generation {
    fn new(
        shared: Arc<Shared>,
        server_id: &str,
        token: Zeroizing<String>,
        timeout: Duration,
        runtime: tokio::runtime::Handle,
    ) -> Arc<Self> {
        let mut manager = LocalSessionManager::default();
        manager.session_config.keep_alive = Some(SESSION_KEEP_ALIVE);
        manager.session_config.init_timeout = Some(timeout);
        let manager = Arc::new(manager);
        let group = Arc::new(SessionGroup {
            shared,
            entry: Entry::Server {
                server_id: server_id.into(),
            },
            sessions: Mutex::new(Some(HashMap::new())),
            admission: tokio::sync::Mutex::new(()),
            manager: manager.clone(),
            stop: watch::channel(false).0,
            confirmation: Arc::default(),
            actions_allowed: Arc::default(),
        });
        let factory = group.clone();
        let config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(true)
            .with_json_response(false)
            .with_max_request_body_bytes(MAX_BODY)
            .with_sse_keep_alive(Some(Duration::from_secs(15)))
            .enforce_origin_validation();
        let http = StreamableHttpService::new(
            move || {
                Ok(HttpHandler {
                    group: factory.clone(),
                    handler: OnceLock::new(),
                })
            },
            manager.clone(),
            config,
        );
        Arc::new(Self {
            token,
            http,
            group,
            manager,
            runtime,
        })
    }
    fn close(&self) {
        let sessions = lock(&self.group.sessions).take();
        if let Some(sessions) = sessions {
            let mut shared_sessions = lock(&self.group.shared.sessions);
            for (id, session) in sessions {
                session.session.cancel();
                shared_sessions.remove(&id);
            }
            let manager = self.manager.clone();
            self.runtime.spawn(async move {
                let ids: Vec<_> = manager.sessions.read().await.keys().cloned().collect();
                for id in ids {
                    let _ = manager.close_session(&id).await;
                }
            });
        }
        self.group.shared.jobs.cancel_entry(Some(&self.group.entry));
        self.group.stop.send_replace(true);
        self.http.config.cancellation_token.cancel();
    }
    async fn handle(self: &Arc<Self>, mut request: Request<Incoming>) -> HttpResponse {
        // 不记录认证头。固定长度 SHA-256 摘要交由库做常量时间比较，不逐字符提前退出。
        let headers = request.headers();
        let supplied = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        let valid = supplied.is_some_and(|value| {
            CtOutput::<TokenDigest>::new(<[u8; 32]>::from(Sha256::digest(value.as_bytes())).into())
                == CtOutput::<TokenDigest>::new(
                    <[u8; 32]>::from(Sha256::digest(self.token.as_bytes())).into(),
                )
        }) && headers.get_all(header::AUTHORIZATION).iter().count() == 1;
        if !valid {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let mut response = response(StatusCode::UNAUTHORIZED);
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                http::HeaderValue::from_static("Bearer"),
            );
            return response;
        }
        // 协议层不需要认证头，移除后不会进入请求扩展和第三方库的调试输出。
        request.headers_mut().remove(header::AUTHORIZATION);
        if request.uri().path() != "/mcp" {
            return response(StatusCode::NOT_FOUND);
        }
        // GET 和通知也属于客户端活动；服务端的 SSE 心跳不刷新这个时间。
        if let Some(id) = request
            .headers()
            .get("mcp-session-id")
            .and_then(|id| id.to_str().ok())
            && let Some(sessions) = lock(&self.group.sessions).as_ref()
            && let Some(session) = sessions
                .values()
                .find(|s| s.protocol_id.as_deref() == Some(id))
        {
            session.session.touch();
        }
        let (ready, mut started) = watch::channel(false);
        let signal = HttpCall {
            ready,
            cancelled: Arc::new(AtomicBool::new(false)),
            session_id: Arc::new(OnceLock::new()),
        };
        request.extensions_mut().insert(signal.clone());
        let generation = self.clone();
        let binding = signal.clone();
        let mut response = Box::pin(async move {
            let response = generation.http.handle(request).await;
            // initialize 的请求头还没有 SDK 会话 ID，需从响应头绑定到业务会话。
            if let Some(id) = binding.session_id.get()
                && let Some(protocol_id) = response
                    .headers()
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                && let Some(sessions) = lock(&generation.group.sessions).as_mut()
                && let Some(session) = sessions.get_mut(id)
            {
                session.protocol_id = Some(protocol_id.to_owned());
            }
            response
        });
        tokio::select! {
            result = &mut response => result.map(|body| DeferredSse { response:None,body:Some(body),signal,first:false,heartbeat:tokio::time::interval(Duration::from_secs(15)) }.boxed()),
            changed = started.changed() => {
                if changed.is_err() { return response.await; }
                let response: Pin<Box<dyn Future<Output=HttpResponse> + Send>> = Box::pin(async move {
                    let result=response.await;
                    if result.headers().get(header::CONTENT_TYPE).is_some_and(|v|v.as_bytes().starts_with(b"text/event-stream")) { return result; }
                    let bytes=match result.into_body().collect().await { Ok(body)=>body.to_bytes(),Err(never)=>match never {} };
                    Response::new(Full::new(Bytes::from(format!("event: message\ndata: {}\n\n",String::from_utf8_lossy(&bytes)))).boxed())
                });
                // SDK 的无握手分支等待首条消息；工具开始后先发 SSE 响应头和注释。
                let body = DeferredSse { response: Some(Mutex::new(response)), body: None, signal, first: true, heartbeat: tokio::time::interval_at(Instant::now()+Duration::from_secs(15),Duration::from_secs(15)) };
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .body(body.boxed())
                    .unwrap_or_else(|_| self::response(StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }
}
// 每个 HTTP 调用单独取消；旧式会话也不能在响应连接断开后继续执行命令。
#[derive(Clone)]
pub(super) struct HttpCall {
    ready: watch::Sender<bool>,
    pub cancelled: Arc<AtomicBool>,
    session_id: Arc<OnceLock<String>>,
}
impl HttpCall {
    pub(super) fn start(&self) {
        self.ready.send_replace(true);
    }
}
struct DeferredSse {
    response: Option<Mutex<Pin<Box<dyn Future<Output = HttpResponse> + Send>>>>,
    body: Option<BoxBody<Bytes, Infallible>>,
    signal: HttpCall,
    first: bool,
    heartbeat: tokio::time::Interval,
}
impl hyper::body::Body for DeferredSse {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
        if self.first {
            self.first = false;
            return Poll::Ready(Some(Ok(hyper::body::Frame::data(Bytes::from_static(
                b": connected\n\n",
            )))));
        }
        if let Some(response) = self.response.as_mut() {
            let polled = lock(response).as_mut().poll(cx);
            match polled {
                Poll::Pending => {
                    return if self.heartbeat.poll_tick(cx).is_ready() {
                        Poll::Ready(Some(Ok(hyper::body::Frame::data(Bytes::from_static(
                            b": keep-alive\n\n",
                        )))))
                    } else {
                        Poll::Pending
                    };
                }
                Poll::Ready(response) => {
                    self.body = Some(response.into_body());
                    self.response = None;
                }
            }
        }
        match self.body.as_mut() {
            Some(body) => Pin::new(body).poll_frame(cx),
            None => Poll::Ready(None),
        }
    }
}
impl Drop for DeferredSse {
    fn drop(&mut self) {
        self.signal
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }
}
// 用固定的 32 字节输出适配现有密码库的常量时间比较类型。
struct TokenDigest;
impl OutputSizeUser for TokenDigest {
    type OutputSize = U32;
}

fn response(status: StatusCode) -> HttpResponse {
    let mut response = Response::new(Full::new(Bytes::new()).boxed());
    *response.status_mut() = status;
    response
}

struct SessionGroup {
    shared: Arc<Shared>,
    entry: Entry,
    sessions: Mutex<Option<HashMap<String, HttpSession>>>,
    // 串行处理接纳和请求登记，淘汰候选与开始新调用不能交错。
    admission: tokio::sync::Mutex<()>,
    manager: Arc<LocalSessionManager>,
    stop: watch::Sender<bool>,
    /// 这一代令牌上第一次连接的确认结果，组里所有会话共用（入口重启或换令牌后重新问）
    confirmation: Arc<Mutex<Option<bool>>>,
    /// 这一代令牌的操作审批标记；换令牌、重启入口或全部停止后随新一代清零
    actions_allowed: Arc<AtomicBool>,
}

struct HttpSession {
    session: Arc<Session>,
    protocol_id: Option<String>,
}
impl SessionGroup {
    async fn make_room(&self) -> Result<(), ErrorData> {
        let evicted = {
            let mut sessions = lock(&self.sessions);
            let Some(sessions) = sessions.as_mut() else {
                return Ok(());
            };
            if sessions.len() < 8 {
                return Ok(());
            }
            let candidate = sessions
                .iter()
                // 未完成 initialize 响应绑定的会话暂不淘汰，避免遗留 SDK 会话。
                .filter(|(_, s)| s.protocol_id.is_some())
                .filter_map(|(id, s)| s.session.idle_since().map(|last| (id.clone(), last)))
                .min_by_key(|(_, last)| *last)
                .map(|(id, _)| id);
            let Some(id) = candidate else {
                let reason =
                    "这个入口同时连着的 AI 太多（最多 8 个），请在 Pierlink 里点全部停止后重试";
                return Err(ErrorData::invalid_request(
                    reason,
                    Some(serde_json::json!(
                        AppError::new("mcp.session_failed")
                            .with_reason("mcp.session_limit", reason)
                    )),
                ));
            };
            let removed = sessions.remove(&id);
            if let Some(session) = &removed {
                session.session.cancel();
                lock(&self.shared.sessions).remove(&id);
            }
            removed
        };
        if let Some(HttpSession {
            protocol_id: Some(id),
            ..
        }) = evicted
        {
            self.manager.close_session(&id.into()).await.map_err(|_| {
                ErrorData::internal_error(
                    "无法关闭空闲会话",
                    Some(serde_json::json!(
                        AppError::new("mcp.session_failed")
                            .with_reason("mcp.session_stopped", "无法关闭空闲会话")
                    )),
                )
            })?;
        }
        Ok(())
    }
}

struct HttpHandler {
    group: Arc<SessionGroup>,
    handler: OnceLock<Result<Handler, ErrorData>>,
}
impl HttpHandler {
    async fn handler(
        &self,
        client: Option<Implementation>,
        context: &RequestContext<RoleServer>,
    ) -> Result<(&Handler, Arc<AtomicBool>), ErrorData> {
        let _admission = self.group.admission.lock().await;
        if self.handler.get().is_none() {
            self.group.make_room().await?;
        }
        let handler = self
            .handler
            .get_or_init(|| {
                let mut group = lock(&self.group.sessions);
                let group = group.as_mut().ok_or_else(|| {
                    ErrorData::invalid_request(
                        "入口已停止",
                        Some(serde_json::json!(
                            AppError::new("mcp.session_failed")
                                .with_reason("mcp.entry_stopped", "入口已停止")
                        )),
                    )
                })?;
                let mut all = lock(&self.group.shared.sessions);
                let session = Arc::new(Session::with_confirmation(
                    self.group.entry.clone(),
                    true,
                    self.group.confirmation.clone(),
                    self.group.actions_allowed.clone(),
                ));
                session.observe(client.clone());
                let id = session.snapshot().id;
                all.insert(id.clone(), session.clone());
                group.insert(
                    id,
                    HttpSession {
                        session: session.clone(),
                        protocol_id: None,
                    },
                );
                let announced = session.clone();
                let shared = self.group.shared.clone();
                tokio::task::spawn_blocking(move || announced.announce(shared.host.as_ref()));
                Ok(Handler {
                    shared: self.group.shared.clone(),
                    session,
                })
            })
            .as_ref()
            .map_err(Clone::clone)?;
        handler.session.observe(client);
        if handler
            .session
            .stopped
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(ErrorData::invalid_request(
                "会话已停止",
                Some(serde_json::json!(
                    AppError::new("mcp.session_failed")
                        .with_reason("mcp.session_stopped", "会话已停止")
                )),
            ));
        }
        if let Some(call) = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<HttpCall>())
        {
            let _ = call.session_id.set(handler.session.snapshot().id);
        }
        // 标记覆盖整个协议请求；工具自己的标记会继续保护尚未退出的阻塞工作。
        let active = handler.session.call_flag();
        Ok((handler, active))
    }
}
impl Drop for HttpHandler {
    fn drop(&mut self) {
        if let Some(Ok(handler)) = self.handler.get() {
            handler.session.cancel();
            let id = handler.session.snapshot().id;
            if let Some(group) = lock(&self.group.sessions).as_mut() {
                group.remove(&id);
            }
            lock(&self.group.shared.sessions).remove(&id);
            let session = handler.session.clone();
            let shared = self.group.shared.clone();
            tokio::task::spawn_blocking(move || session.finish(shared.host.as_ref()));
        }
    }
}
impl ServerHandler for HttpHandler {
    fn get_info(&self) -> ServerConfig {
        server_info()
    }
    fn get_tool(&self, name: &str) -> Option<Tool> {
        super::tools::definitions()
            .into_iter()
            .find(|tool| tool.name == name)
    }
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let (handler, _active) = self
            .handler(Some(request.client_info.clone()), &context)
            .await?;
        handler.initialize(request, context).await
    }
    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        let (handler, _active) = self.handler(context.meta.client_info(), &context).await?;
        handler.discover(context).await
    }
    async fn ping(&self, context: RequestContext<RoleServer>) -> Result<(), ErrorData> {
        let (handler, _active) = self.handler(context.meta.client_info(), &context).await?;
        handler.ping(context).await
    }
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let (handler, _active) = self.handler(context.meta.client_info(), &context).await?;
        handler.list_tools(request, context).await
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let (handler, _active) = self.handler(context.meta.client_info(), &context).await?;
        handler.call_tool(request, context).await
    }
}

async fn serve_connection<S>(stream: S, generation: Arc<Generation>, timeout: Duration)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut stopped = generation.group.stop.subscribe();
    if *stopped.borrow() {
        return;
    }
    let activity = Arc::new(Mutex::new(Instant::now()));
    let io = ActivityIo {
        stream,
        activity: activity.clone(),
    };
    let service = service_fn(move |request| {
        let generation = generation.clone();
        async move { Ok::<_, Infallible>(generation.handle(request).await) }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(timeout)
        .max_buf_size(16 * 1024);
    tokio::select! {
        _ = builder.serve_connection(TokioIo::new(io), service) => {},
        _ = stopped.changed() => {},
        _ = async {
            loop {
                let deadline = *lock(&activity) + timeout;
                tokio::time::sleep_until(deadline).await;
                if lock(&activity).elapsed() >= timeout { break; }
            }
        } => {},
    }
}

// 以实际收发字节计算 TCP 空闲时间，同时由 hyper 单独限制慢速请求头。
struct ActivityIo<S> {
    stream: S,
    activity: Arc<Mutex<Instant>>,
}
impl<S: AsyncRead + Unpin> AsyncRead for ActivityIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        if buf.filled().len() > before {
            *lock(&self.activity) = Instant::now();
        }
        result
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for ActivityIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, bytes);
        if let Poll::Ready(Ok(count)) = result
            && count > 0
        {
            *lock(&self.activity) = Instant::now();
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

#[cfg(test)]
pub(super) mod test_support {
    use super::*;

    pub(crate) struct MemoryEntry {
        current: Arc<Mutex<Arc<Generation>>>,
        timeout: Duration,
    }
    impl MemoryEntry {
        /// 用 service 的共用宿主创建内存入口，token 用于认证，timeout 限制空闲时间。
        pub(crate) fn new(service: &McpService, token: &str, timeout: Duration) -> Self {
            Self::for_server(service, "server-id", token, timeout)
        }
        /// 为 server_id 创建内存入口并登记到 service，返回可经过真实令牌轮换和停止流程的入口。
        /// token 用于认证，timeout 限制空闲时间。
        pub(crate) fn for_server(
            service: &McpService,
            server_id: &str,
            token: &str,
            timeout: Duration,
        ) -> Self {
            service.add_secret(Zeroizing::new(token.into()));
            let current = Arc::new(Mutex::new(Generation::new(
                service.shared.clone(),
                server_id,
                Zeroizing::new(token.into()),
                timeout,
                service.runtime(),
            )));
            lock(&service.http_entries).insert(
                server_id.into(),
                EntryWorker {
                    current: current.clone(),
                    listener: service.runtime().spawn(std::future::pending()),
                    timeout,
                },
            );
            Self { current, timeout }
        }
        /// 无参数；返回客户端字节流，服务端运行与 TCP 相同的 hyper 驱动。
        pub(crate) fn connect(&self) -> tokio::io::DuplexStream {
            let (server, client) = tokio::io::duplex(64 * 1024);
            let generation = lock(&self.current).clone();
            generation
                .runtime
                .spawn(serve_connection(server, generation.clone(), self.timeout));
            client
        }
        /// 无参数、无返回值；取消入口及其全部逻辑会话。
        pub(crate) fn close(&self) {
            lock(&self.current).close();
        }
    }
    impl Drop for MemoryEntry {
        fn drop(&mut self) {
            self.close();
        }
    }
}
