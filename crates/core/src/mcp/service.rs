//! 共用 MCP 服务：独立运行时、字节流传输、工具分发与审计。
use super::{
    AuditRecord, ClientInfo, Entry, HostEvent, McpHost, SessionHandle, SessionInfo, Target,
    audit::{Audit, now_ms, sanitize_record},
    lock,
    session::Session,
    tools::{
        self, Failure, Registry,
        terminals::{AiTerminal, TerminalSource},
    },
};
use crate::{error::AppError, terminal::TerminalManager};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ClientJsonRpcMessage,
        ClientRequest, ContentBlock, GetMeta, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, ServerJsonRpcMessage,
    },
    service::{RequestContext, RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{Transport, async_rw::AsyncRwTransport},
};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    runtime::{Handle, Runtime},
};
use zeroize::Zeroizing;

pub(super) struct Shared {
    pub host: Arc<dyn McpHost>,
    pub terminals: Arc<TerminalManager>,
    pub registry: Registry,
    pub jobs: tools::jobs::Jobs,
    audit: Audit,
    pub(super) sessions: Mutex<HashMap<String, Arc<Session>>>,
    activity: Mutex<VecDeque<AuditRecord>>,
    secrets: Mutex<Vec<Zeroizing<String>>>,
}
/// 两个入口共享的 MCP 基座，拥有独立的两个异步工作线程。
pub struct McpService {
    pub(super) shared: Arc<Shared>,
    pub(super) http_entries: Mutex<HashMap<String, super::http::EntryWorker>>,
    runtime: Option<Runtime>,
    handle: Handle,
}
impl McpService {
    /// 用 host、terminals 和 audit_dir 建服务，启动时清理超过 30 天的审计；返回服务或运行时错误。
    pub fn new(
        host: Arc<dyn McpHost>,
        terminals: TerminalManager,
        audit_dir: PathBuf,
    ) -> Result<Self, AppError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("pierlink-mcp")
            .enable_all()
            .build()
            .map_err(|_| {
                AppError::new("mcp.runtime_failed").with("reason", "无法创建 MCP 运行时")
            })?;
        let handle = runtime.handle().clone();
        let audit = Audit::new(audit_dir, 30);
        if audit.prune().is_err() {
            eprintln!("MCP 启动时清理审计日志失败");
        }
        let terminals = Arc::new(terminals);
        let shared = Arc::new(Shared {
            host,
            registry: Registry::new(terminals.clone()),
            jobs: tools::jobs::Jobs::default(),
            terminals,
            audit,
            sessions: Mutex::new(HashMap::new()),
            activity: Mutex::new(VecDeque::new()),
            secrets: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&shared);
        // 每秒看一次 AI 终端有没有退出（界面上的“AI 终端已结束”不需要更及时）；没有 AI 终端时什么也不做
        handle.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let Some(shared) = weak.upgrade() else { break };
                if tokio::task::spawn_blocking(move || {
                    shared.jobs.maintain(shared.host.as_ref());
                    if !shared.registry.is_empty() {
                        shared.registry.reap(shared.host.as_ref());
                    }
                })
                .await
                .is_err()
                {
                    break;
                }
            }
        });
        Ok(Self {
            shared,
            http_entries: Mutex::new(HashMap::new()),
            runtime: Some(runtime),
            handle,
        })
    }
    /// 在 entry 的 read/write 字节流上启动后台会话，立即返回可取消和等待的句柄。
    /// 每入口最多接纳 8 个连接；超限连接读取首个请求后回复错误并断开。
    pub fn serve_stream<R, W>(
        &self,
        entry: Entry,
        read: R,
        write: W,
    ) -> Result<SessionHandle, AppError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut sessions = lock(&self.shared.sessions);
        let accepted = sessions
            .values()
            .filter(|session| session.accepted && session.snapshot().entry == entry)
            .count()
            < 8;
        let session = Arc::new(Session::new(entry, accepted));
        sessions.insert(session.snapshot().id, session.clone());
        drop(sessions);
        let (done, receiver) = mpsc::channel();
        let shared = self.shared.clone();
        let worker_session = session.clone();
        self.handle.spawn(async move {
            let transport =
                AsyncRwTransport::new_server(read, write).with_max_line_length(8 * 1024 * 1024);
            let result = serve(shared.clone(), worker_session.clone(), transport).await;
            worker_session.cancel();
            let id = worker_session.snapshot().id;
            let cleanup = shared.clone();
            let _ =
                tokio::task::spawn_blocking(move || worker_session.finish(cleanup.host.as_ref()))
                    .await;
            lock(&shared.sessions).remove(&id);
            let _ = done.send(result);
        });
        Ok(SessionHandle {
            session,
            done: receiver,
        })
    }
    /// 在一条双向字节流（例如命名管道）上服务一个客户端：拆成读写两半后同 serve_stream。
    pub fn serve_duplex<S>(&self, entry: Entry, stream: S) -> Result<SessionHandle, AppError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (read, write) = tokio::io::split(stream);
        self.serve_stream(entry, read, write)
    }
    /// 无参数；返回当前接纳的会话及其建立时权限快照。
    pub fn sessions(&self) -> Vec<SessionInfo> {
        let mut sessions: Vec<_> = lock(&self.shared.sessions)
            .values()
            .filter(|session| session.accepted)
            .map(|session| session.snapshot())
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        sessions
    }
    /// 返回至多 limit 条已脱敏操作，新的在前；内存总量最多 30 条。
    pub fn activity(&self, limit: usize) -> Vec<AuditRecord> {
        lock(&self.shared.activity)
            .iter()
            .take(limit)
            .cloned()
            .collect()
    }
    /// 将 terminal_id 对应的用户终端交给 entry，target 必须与宿主元数据和该入口权限一致。
    /// 成功后发送 TerminalHandedOver 并记录审计；无目标、未授权或重复交出返回 AppError。
    pub fn hand_over(
        &self,
        terminal_id: &str,
        entry: Entry,
        target: Target,
    ) -> Result<(), AppError> {
        let permissions = self.shared.host.permissions(&entry)?.ok_or_else(|| {
            AppError::new("mcp.entry_disabled").with("reason", "这个 MCP 入口未启用")
        })?;
        let targets = self.shared.host.targets(&entry, &permissions)?;
        let target = tools::terminal_targets(&permissions, &targets)
            .into_iter()
            .find(|allowed| allowed.id == target.id && allowed.kind == target.kind)
            .ok_or_else(|| {
                AppError::new("mcp.target_denied").with("reason", "这个入口没有终端目标的操作权限")
            })?;
        if !self.shared.host.user_terminals()?.iter().any(|terminal| {
            terminal.terminal_id == terminal_id
                && terminal.target_id == target.id
                && terminal.kind == target.kind
                && !terminal.target_id.is_empty()
        }) {
            return Err(AppError::new("mcp.terminal_not_found")
                .with("reason", "用户终端不存在或目标不匹配"));
        }
        let info = self.shared.registry.hand_over(
            terminal_id,
            entry,
            target,
            self.shared.host.as_ref(),
        )?;
        self.shared.audit_handover("hand_over", &info);
        Ok(())
    }
    /// 收回 terminal_id，取消此登记的待执行输入和等待调用，保留终端进程。
    /// 成功后发送 TerminalTakenBack 并记录审计；未交出返回 AppError。
    pub fn take_back(&self, terminal_id: &str) -> Result<(), AppError> {
        let info = self
            .shared
            .registry
            .take_back(terminal_id, self.shared.host.as_ref())?;
        self.shared.audit_handover("take_back", &info);
        Ok(())
    }
    /// 无参数、无返回值；取消调用、断开连接，关闭 AI 创建的终端，用户交出的终端只收回。
    pub fn stop_all(&self) {
        self.reset_http_sessions();
        let sessions: Vec<_> = lock(&self.shared.sessions).values().cloned().collect();
        for session in sessions {
            session.cancel();
        }
        self.shared.jobs.cancel_entry(None);
        for info in self.shared.registry.close_all(self.shared.host.as_ref()) {
            self.shared.audit_handover("take_back", &info);
        }
    }
    /// 登记 secret 以遮蔽后续审计及工具结果中的值；空值忽略，秘密在释放时清零。
    pub fn add_secret(&self, secret: Zeroizing<String>) {
        if !secret.is_empty() {
            let mut secrets = lock(&self.shared.secrets);
            if !secrets.iter().any(|value| **value == *secret) {
                secrets.push(secret);
            }
        }
    }
    /// 无参数；返回本服务运行时句柄，供后续传输入口启动任务。
    pub fn runtime(&self) -> Handle {
        self.handle.clone()
    }
}
impl Drop for McpService {
    fn drop(&mut self) {
        lock(&self.http_entries).clear();
        self.stop_all();
        // 允许服务从另一个异步运行时释放；阻塞宿主通过取消标志自行结束。
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

async fn serve<T: Transport<RoleServer> + 'static>(
    shared: Arc<Shared>,
    session: Arc<Session>,
    mut transport: T,
) -> Result<(), AppError> {
    let mut stop = session.stop.subscribe();
    if *stop.borrow() {
        return Ok(());
    }
    if !session.accepted {
        tokio::select! {
            _ = stop.changed() => {},
            _ = tokio::time::sleep(Duration::from_secs(10)) => {},
            _ = async {
                while let Some(message) = transport.receive().await {
                    if let ClientJsonRpcMessage::Request(request) = message {
                        let _ = transport.send(ServerJsonRpcMessage::error(ErrorData::invalid_request("这个入口的会话太多，最多允许 8 个", None),Some(request.id))).await;
                        break;
                    }
                }
            } => {}
        }
        let _ = transport.close().await;
        return Ok(());
    }
    let handler = Handler {
        shared: shared.clone(),
        session: session.clone(),
    };
    let transport = ObservedTransport {
        inner: transport,
        shared,
        session,
        announced: false,
    };
    let running = tokio::select! {
        _ = stop.changed() => return Ok(()),
        result = handler.serve(transport) => result.map_err(|_| session_error())?,
    };
    let token = running.cancellation_token();
    let waiting = running.waiting();
    tokio::pin!(waiting);
    tokio::select! {
        result = &mut waiting => result.map(|_| ()).map_err(|_| session_error()),
        _ = stop.changed() => {
            token.cancel();
            waiting.await.map(|_| ()).map_err(|_| session_error())
        }
    }
}
fn session_error() -> AppError {
    AppError::new("mcp.session_failed").with("reason", "MCP 连接已结束或协议消息无效")
}

// 在传输 EOF 时立即取消阻塞工作，避免等 SDK 排空响应的五秒期限；不改变协议消息。
struct ObservedTransport<T> {
    inner: T,
    shared: Arc<Shared>,
    session: Arc<Session>,
    announced: bool,
}
impl<T: Transport<RoleServer>> Transport<RoleServer> for ObservedTransport<T> {
    type Error = T::Error;
    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }
    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        let message = self.inner.receive().await;
        if let Some(ClientJsonRpcMessage::Request(request)) = &message {
            let client = match &request.request {
                ClientRequest::InitializeRequest(request) => {
                    Some(request.params.client_info.clone())
                }
                request => request.get_meta().client_info(),
            };
            self.session.observe(client);
            if !self.announced {
                self.announced = true;
                let session = self.session.clone();
                let shared = self.shared.clone();
                tokio::task::spawn_blocking(move || session.announce(shared.host.as_ref()));
            }
        } else if message.is_none() {
            self.session.cancel();
        }
        message
    }
    async fn close(&mut self) -> Result<(), Self::Error> {
        self.session.cancel();
        self.inner.close().await
    }
}
pub(super) struct Handler {
    pub(super) shared: Arc<Shared>,
    pub(super) session: Arc<Session>,
}
impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        server_info()
    }
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: tools::definitions(),
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let started = Instant::now();
        let cancelled = self.session.call_flag();
        if context.ct.is_cancelled() {
            cancelled.store(true, Ordering::Release);
        }
        let http_call = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<super::http::HttpCall>())
            .cloned();
        if let Some(call) = &http_call {
            call.start();
        }
        let progress = context.meta.get_progress_token();
        let flag = cancelled.clone();
        let monitor = tokio::spawn(async move {
            let mut next_progress = tokio::time::Instant::now() + Duration::from_secs(15);
            loop {
                tokio::select! {
                    _ = context.ct.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {
                        if http_call.as_ref().is_some_and(|call| call.cancelled.load(Ordering::Acquire)) { break; }
                        if tokio::time::Instant::now() >= next_progress {
                            if let Some(token) = &progress {
                                let _ = context.peer.notify_progress(rmcp::model::ProgressNotificationParam::new(token.clone(), started.elapsed().as_secs_f64())).await;
                            }
                            next_progress += Duration::from_secs(15);
                        }
                    }
                }
            }
            flag.store(true, Ordering::Release);
        });
        let _guard = CallGuard {
            cancelled: cancelled.clone(),
            monitor,
        };
        let shared = self.shared.clone();
        let session = self.session.clone();
        tokio::task::spawn_blocking(move || {
            let args = request.arguments.unwrap_or_default();
            let mut approval = "";
            let result = tools::execute(
                &shared,
                &session,
                &request.name,
                &args,
                cancelled,
                &mut approval,
            );
            shared.complete(&session, &request.name, &args, (result, approval), started)
        })
        .await
        .map_err(|_| ErrorData::internal_error("工具工作线程异常结束", None))?
        .map(Into::into)
    }
}
/// 无参数；返回字节流与 HTTP 入口共用的服务能力和使用说明。
pub(super) fn server_info() -> ServerConfig {
    ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        .with_server_info(Implementation::new("pierlink", env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
}
const INSTRUCTIONS: &str = r#"Pierlink connects this agent to the user's Windows computer and explicitly authorized servers. It supplies information and basic operations; you choose the workflow, directory, shell and timing. It never automatically changes directories or waits for synchronization.

Start with list_targets to inspect the targets, commands permission, authorized folders, home, available shells and path_style. Permissions belong to the entry you connected through and may change. A Windows target can allow files without allowing commands. Use target names or stable IDs exactly as returned. Do not infer a project directory from the Pierlink process directory.

To operate on Windows files from a server, use Pierlink read_file, edit_file, write_file, search, find_files and list_dir. These tools access Windows, while your built-in Read/Edit/Grep/Glob access the machine running the agent. Supply Windows absolute paths, for example C:/project/file.txt or JSON-escaped backslashes. UNC/network paths, devices, alternate data streams, reserved names, parent traversal and links escaping authorized folders are rejected. An outside-path error lists the authorized folders.

read_file defaults to numbered lines: file_path, offset (1-based), limit (default 2000, maximum 5000). Lines longer than 2000 characters are marked and clipped. byte_offset/max_bytes explicitly select raw byte reads; binary content is base64. Text results report encoding, line_ending, mtime_ms and sha256 (null above 64 MiB, with sha256_skipped_reason). edit_file requires exact old_string/new_string, with unique matching unless replace_all is true. Pass expected_sha256 to edit_file or write_file when you need protection against changes since your read. Existing encoding, BOM and newline style are preserved; unrepresentable characters are rejected. Never include read_file's line numbers in replacement text. write_file accepts at most 5 MiB and cannot overwrite existing binary files or files larger than 5 MiB.

search uses Rust regex and supports path, glob, type, output_mode, -i, -n, -A/-B/-C, multiline, head_limit and offset. It defaults to files_with_matches. find_files uses glob patterns and sorts newest files first. Both respect .gitignore and do not follow links. Inspect truncated and narrow the search when a resource limit is reached.

Use run_command for noninteractive work; inspect exit_code, timed_out, stdout and stderr. Windows defaults to powershell and the user home directory; provide cwd explicitly when appropriate and select only a shell listed by list_targets. Long commands can use background:true, then job_output with a since cursor, list_jobs and job_stop. Jobs belong to this entry, survive HTTP request sessions, and have bounded output: lost means older bytes were discarded. Background jobs currently support Windows only: the server execution channel cannot guarantee remote process-tree cancellation. Use a server AI terminal for long-running server work. For interactive or visible long-running work, use an AI terminal. terminal_send only queues input: use terminal_wait to observe completion. Never send passwords, private-key passphrases or verification codes; ask the user to enter them in Pierlink.

For synchronized projects, sync_status reports local_path and each server's remote_path. After changing Windows files, consider sync_wait before building on a server so the build sees the changes. This is a recommendation: you decide when to wait and where to run commands. Review paused/offline status and all errors before continuing. Approval can be required for commands and writes; denied or cancelled operations must not be retried to bypass the user's choice."#;
struct CallGuard {
    cancelled: Arc<AtomicBool>,
    monitor: tokio::task::JoinHandle<()>,
}
impl Drop for CallGuard {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.monitor.abort();
    }
}
impl Shared {
    fn audit_handover(&self, tool: &str, info: &AiTerminal) {
        let TerminalSource::HandedOver { entry } = &info.source else {
            return;
        };
        let mut record = AuditRecord {
            output_preview: None,
            time_ms: now_ms(),
            session: String::new(),
            entry: entry.clone(),
            client: ClientInfo::default(),
            tool: tool.into(),
            target: Some(info.target.id.clone()),
            summary: info.terminal_id.clone(),
            outcome: "ok".into(),
            approval: "user".into(),
            exit_code: None,
            duration_ms: 0,
        };
        let secrets = lock(&self.secrets);
        let secret_refs: Vec<&str> = secrets.iter().map(|secret| secret.as_str()).collect();
        sanitize_record(&mut record, &secret_refs);
        bound(&mut record.summary, 2048);
        drop(secrets);
        self.record_activity(record);
    }
    pub(super) fn job_completed(&self, info: &SessionInfo, approval: &str, value: Value) {
        let secrets = lock(&self.secrets);
        let refs: Vec<_> = secrets.iter().map(|s| s.as_str()).collect();
        let mut record = AuditRecord {
            output_preview: preview(&value, &refs),
            time_ms: now_ms(),
            session: info.id.clone(),
            entry: info.entry.clone(),
            client: info.client.clone(),
            tool: "job_completed".into(),
            target: value["target"].as_str().map(str::to_owned),
            summary: value["job_id"].as_str().unwrap_or_default().into(),
            outcome: if value["error"].is_null() {
                "ok"
            } else {
                "error"
            }
            .into(),
            approval: approval.into(),
            exit_code: value["exit_code"].as_i64().and_then(|v| v.try_into().ok()),
            duration_ms: 0,
        };
        sanitize_record(&mut record, &refs);
        drop(secrets);
        self.record_activity(record);
    }
    fn record_activity(&self, record: AuditRecord) {
        self.audit.record(&record);
        {
            let mut activity = lock(&self.activity);
            activity.push_front(record.clone());
            activity.truncate(30);
        }
        self.host.notify(HostEvent::Activity(record));
    }
    fn complete(
        &self,
        session: &Session,
        name: &str,
        args: &rmcp::model::JsonObject,
        (result, approval): (Result<Value, Failure>, &str),
        started: Instant,
    ) -> Result<CallToolResult, ErrorData> {
        let info = session.snapshot();
        let outcome = match &result {
            Ok(_) => "ok",
            Err(Failure::Params(_)) => "mcp.invalid_params",
            Err(Failure::Tool(error)) => match error.code.as_str() {
                "mcp.connection_denied" | "mcp.action_denied" | "mcp.password_prompt" => "denied",
                "mcp.cancelled" => "cancelled",
                _ => &error.code,
            },
        };
        let password_denied =
            matches!(&result, Err(Failure::Tool(error)) if error.code == "mcp.password_prompt");
        let mut summary = tools::summary(name, args, password_denied);
        // 只发回车的那一次：记下实际提交的整行（被拒绝时记下被拒的那一行）
        if name == "terminal_send"
            && args
                .get("text")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            let lines: Vec<String> = match &result {
                Ok(value) => value["submitted"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                Err(Failure::Tool(error)) if error.code == "mcp.action_denied" => {
                    error.params.get("action").cloned().into_iter().collect()
                }
                Err(_) => Vec::new(),
            };
            if !lines.is_empty() {
                summary.push_str(&format!(" → {}", lines.join(" / ")));
            }
        }
        let mut record = AuditRecord {
            output_preview: None,
            time_ms: now_ms(),
            session: info.id,
            entry: info.entry,
            client: info.client,
            tool: name.into(),
            target: tools::audit_target(args),
            summary,
            outcome: outcome.into(),
            approval: approval.into(),
            exit_code: result
                .as_ref()
                .ok()
                .and_then(|v| v["exit_code"].as_i64())
                .and_then(|v| i32::try_from(v).ok()),
            duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        };
        let secrets = lock(&self.secrets);
        let secret_refs: Vec<&str> = secrets.iter().map(|secret| secret.as_str()).collect();
        sanitize_record(&mut record, &secret_refs);
        if name == "terminal_send" {
            record.summary = record.summary.chars().take(200).collect();
        } else {
            bound(&mut record.summary, 2048);
        }
        bound(&mut record.client.name, 256);
        bound(&mut record.client.version, 128);
        bound(&mut record.tool, 128);
        if matches!(name, "run_command" | "job_output" | "job_stop") {
            record.output_preview = result
                .as_ref()
                .ok()
                .and_then(|value| preview(value, &secret_refs));
        }
        if name == "edit_file"
            && let Ok(value) = &result
        {
            record.summary.push_str(&format!(
                "（替换 {} 次）",
                value["replacements"].as_u64().unwrap_or(0)
            ));
        }
        let response = match result {
            Ok(mut value) => {
                hide_value(&mut value, &secret_refs);
                Ok(CallToolResult::structured(value))
            }
            Err(Failure::Params(error)) => Err(error),
            Err(Failure::Tool(error)) => {
                let reason = if matches!(
                    error.code.as_str(),
                    "ssh.password_needed" | "ssh.two_factor_required"
                ) {
                    "需要在 Pierlink 里登录这台服务器".into()
                } else {
                    error
                        .params
                        .get("reason")
                        .cloned()
                        .unwrap_or_else(|| format!("操作失败（{}）", error.code))
                };
                let mut params = error.params;
                params.remove("action");
                params.insert("reason".into(), reason.clone());
                for text in params.values_mut() {
                    *text = crate::security::redact(text, &secret_refs);
                }
                let mut value = serde_json::json!({"code":error.code,"params":params});
                hide_value(&mut value, &secret_refs);
                let mut result = CallToolResult::error(vec![ContentBlock::text(
                    crate::security::redact(&reason, &secret_refs),
                )]);
                result.structured_content = Some(value);
                Ok(result)
            }
        };
        drop(secrets);
        self.record_activity(record);
        response
    }
}
fn preview(value: &Value, secrets: &[&str]) -> Option<super::OutputPreview> {
    if !value["stdout"].is_string() && !value["stderr"].is_string() {
        return None;
    }
    let tail = |key: &str| {
        let text = crate::security::redact(value[key].as_str().unwrap_or_default(), secrets);
        let mut start = text.len().saturating_sub(8192);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text[start..].to_owned()
    };
    Some(super::OutputPreview {
        stdout: tail("stdout"),
        stderr: tail("stderr"),
    })
}
fn bound(text: &mut String, limit: usize) {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}
// 输出保留原文，只遮蔽显式登记的秘密；同时匹配重叠值，避免替换顺序残留部分秘密。
fn hide_value(value: &mut Value, secrets: &[&str]) {
    match value {
        Value::String(text) => {
            let mut ranges = Vec::new();
            for (start, _) in text.char_indices() {
                for secret in secrets {
                    if !secret.is_empty() && text[start..].starts_with(secret) {
                        ranges.push((start, start + secret.len()));
                    }
                }
            }
            ranges.sort_unstable();
            let mut merged: Vec<(usize, usize)> = Vec::new();
            for (start, end) in ranges {
                if let Some(last) = merged.last_mut()
                    && start <= last.1
                {
                    last.1 = last.1.max(end);
                } else {
                    merged.push((start, end));
                }
            }
            let mut clean = String::new();
            let mut cursor = 0;
            for (start, end) in merged {
                clean.push_str(&text[cursor..start]);
                clean.push_str("***");
                cursor = end;
            }
            clean.push_str(&text[cursor..]);
            *text = clean;
        }
        Value::Array(values) => {
            for value in values {
                hide_value(value, secrets);
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                hide_value(value, secrets);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 静态使用说明及核心工具常驻元数据() {
        assert!(INSTRUCTIONS.chars().count() <= 4096);
        for name in [
            "list_targets",
            "read_file",
            "edit_file",
            "write_file",
            "search",
            "find_files",
            "run_command",
            "job_output",
        ] {
            let tool = tools::definitions()
                .into_iter()
                .find(|t| t.name == name)
                .unwrap();
            assert_eq!(
                serde_json::to_value(tool).unwrap()["_meta"]["anthropic/alwaysLoad"],
                true
            );
        }
        for tool in tools::definitions()
            .into_iter()
            .filter(|t| t.name.starts_with("terminal") || t.name.starts_with("sync"))
        {
            assert!(tool.meta.is_none());
        }
    }
    #[test]
    fn 预览先脱敏再截取双流八千字节() {
        let value = serde_json::json!({"stdout":format!("{}password=secret\n令牌值", "中文".repeat(10000)),"stderr":"API_KEY=private\n尾部"});
        let value = preview(&value, &["令牌值"]).unwrap();
        assert!(value.stdout.len() <= 8192 && value.stderr.len() <= 8192);
        assert!(
            !value.stdout.contains("secret")
                && !value.stdout.contains("令牌值")
                && !value.stderr.contains("private")
        );
    }
}
