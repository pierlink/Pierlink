//! 用字节流客户端验证协议、宿主边界和资源释放。
#[path = "approval_tests.rs"]
mod approval;
#[cfg(unix)]
#[path = "handover_tests.rs"]
mod handover;
#[path = "http_tests.rs"]
mod http;
use super::*;
use crate::{error::AppError, terminal::TerminalManager};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

struct Host {
    enabled: AtomicBool,
    remembered: AtomicBool,
    permission_blocked: AtomicBool,
    decision: Mutex<Confirmation>,
    action_decision: Mutex<Confirmation>,
    confirmations: AtomicUsize,
    confirmation_blocked: AtomicBool,
    cancel_on_no_answer: AtomicBool,
    dangers: Mutex<Vec<String>>,
    danger_flags: Mutex<Vec<bool>>,
    commands: Mutex<Vec<String>>,
    events: Mutex<Vec<HostEvent>>,
    permissions: Mutex<Permissions>,
    block: AtomicBool,
    started: AtomicUsize,
    cancelled: AtomicUsize,
    output: Mutex<RunOutput>,
    terminals: Arc<TerminalManager>,
    user_terminals: Mutex<Vec<UserTerminal>>,
    cwd_requests: Mutex<Vec<String>>,
    action_input: Mutex<Option<(String, Vec<u8>)>>,
    action_blocked: AtomicBool,
}
impl Host {
    fn new(terminals: Arc<TerminalManager>) -> Self {
        Self {
            enabled: AtomicBool::new(true),
            remembered: AtomicBool::new(false),
            permission_blocked: AtomicBool::new(false),
            decision: Mutex::new(Confirmation::Allow),
            action_decision: Mutex::new(Confirmation::Allow),
            confirmations: AtomicUsize::new(0),
            confirmation_blocked: AtomicBool::new(false),
            cancel_on_no_answer: AtomicBool::new(false),
            dangers: Mutex::new(Vec::new()),
            danger_flags: Mutex::new(Vec::new()),
            commands: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            permissions: Mutex::new(Permissions {
                approval: Approval::default(),
                windows_commands: true,
                folders: vec!["C:\\work".into()],
                servers: vec!["server-id".into()],
            }),
            block: AtomicBool::new(false),
            started: AtomicUsize::new(0),
            cancelled: AtomicUsize::new(0),
            output: Mutex::new(RunOutput {
                exit_code: Some(7),
                stdout: b"\x1b[31mout\x1b[0m\r\n\xff".to_vec(),
                stderr: b"err\n".to_vec(),
                truncated: false,
                timed_out: false,
                duration_ms: 42,
            }),
            terminals,
            user_terminals: Mutex::new(Vec::new()),
            cwd_requests: Mutex::new(Vec::new()),
            action_input: Mutex::new(None),
            action_blocked: AtomicBool::new(false),
        }
    }
    fn user_terminal(&self) -> String {
        #[cfg(unix)]
        let program = std::path::PathBuf::from("/bin/cat");
        #[cfg(windows)]
        let program = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/cmd.exe");
        self.terminals
            .open(
                crate::terminal::TerminalSpec {
                    kind: crate::terminal::TerminalKind::Local {
                        program,
                        args: vec![],
                        cwd: None,
                        env: vec![],
                    },
                    cols: 80,
                    rows: 24,
                },
                Arc::new(|_| {}),
            )
            .unwrap()
    }
}
impl McpHost for Host {
    fn permissions(&self, _entry: &Entry) -> Result<Option<Permissions>, AppError> {
        while self.permission_blocked.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(self
            .enabled
            .load(Ordering::Acquire)
            .then(|| lock(&self.permissions).clone()))
    }
    fn targets(&self, entry: &Entry, permissions: &Permissions) -> Result<Vec<Target>, AppError> {
        let mut targets = vec![];
        // 和真实宿主一样：允许执行命令或允许了文件夹，Windows 都算可操作的目标
        if permissions.windows_commands || !permissions.folders.is_empty() {
            targets.push(Target {
                target: "windows".into(),
                id: "windows".into(),
                name: "Windows".into(),
                kind: TargetKind::Windows,
                online: None,
                engine: None,
                address: None,
            });
        }
        for id in &permissions.servers {
            targets.push(Target {
                target: "计算服务器".into(),
                id: id.clone(),
                name: "计算服务器".into(),
                kind: TargetKind::Server,
                online: Some(true),
                engine: Some("builtin".into()),
                address: matches!(entry, Entry::Windows).then(|| "user@private:22".into()),
            });
        }
        Ok(targets)
    }
    fn remembered(&self, _entry: &Entry) -> bool {
        self.remembered.load(Ordering::Acquire)
    }
    fn confirm_connect(
        &self,
        _session: &SessionInfo,
        _targets: &[Target],
        cancelled: &AtomicBool,
    ) -> Result<Confirmation, AppError> {
        self.confirmations.fetch_add(1, Ordering::AcqRel);
        while self.confirmation_blocked.load(Ordering::Acquire) {
            if cancelled.load(Ordering::Acquire) {
                return Err(session::cancelled_error());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(session::cancelled_error());
        }
        let decision = *lock(&self.decision);
        if decision == Confirmation::NoAnswer && self.cancel_on_no_answer.load(Ordering::Acquire) {
            cancelled.store(true, Ordering::Release);
        }
        if decision == Confirmation::AllowAlways {
            self.remembered.store(true, Ordering::Release);
        }
        Ok(decision)
    }
    fn confirm_action(
        &self,
        _session: &SessionInfo,
        _target: &Target,
        action: &str,
        dangerous: bool,
        cancelled: &AtomicBool,
    ) -> Result<Confirmation, AppError> {
        lock(&self.dangers).push(action.into());
        lock(&self.danger_flags).push(dangerous);
        if let Some((id, data)) = lock(&self.action_input).take() {
            self.terminals.write(&id, &data)?;
        }
        // 和界面一样：确认框在等的时候调用被取消，框关掉，按拒绝返回
        while self.action_blocked.load(Ordering::Acquire) {
            if cancelled.load(Ordering::Acquire) {
                return Ok(Confirmation::Deny);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let decision = *lock(&self.action_decision);
        if decision == Confirmation::NoAnswer && self.cancel_on_no_answer.load(Ordering::Acquire) {
            cancelled.store(true, Ordering::Release);
        }
        Ok(decision)
    }
    fn run_server_command(
        &self,
        server_id: &str,
        command: &str,
        cwd: Option<&str>,
        limits: RunLimits,
        cancelled: Arc<AtomicBool>,
    ) -> Result<RunOutput, AppError> {
        assert_eq!(server_id, "server-id");
        assert_eq!(limits.output_bytes, 256 * 1024);
        assert!(limits.timeout <= Duration::from_secs(1800));
        if command == "pwd" {
            assert_eq!(cwd, Some("/a b"));
        }
        lock(&self.commands).push(command.into());
        self.started.fetch_add(1, Ordering::AcqRel);
        while self.block.load(Ordering::Acquire) {
            if cancelled.load(Ordering::Acquire) {
                self.cancelled.fetch_add(1, Ordering::AcqRel);
                return Err(session::cancelled_error());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if matches!(
            command,
            "ssh.host_key_rejected" | "terminal.shell_not_found"
        ) {
            return Err(AppError::new(command));
        }
        if command == "auth-limit" {
            return Err(AppError::new("ssh.too_many_auth_attempts"));
        }
        if command == "login" {
            return Err(AppError::new("ssh.password_needed"));
        }
        Ok(lock(&self.output).clone())
    }
    fn open_terminal(
        &self,
        _request: TerminalOpenRequest,
        cancelled: &AtomicBool,
    ) -> Result<String, AppError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(session::cancelled_error());
        }
        Ok(self.user_terminal())
    }
    fn user_terminals(&self) -> Result<Vec<UserTerminal>, AppError> {
        Ok(lock(&self.user_terminals).clone())
    }
    fn terminal_cwd(&self, terminal_id: &str) -> Result<Option<String>, AppError> {
        lock(&self.cwd_requests).push(terminal_id.into());
        Ok(Some("/用户私有目录".into()))
    }
    fn sync_status(&self, task: Option<&str>) -> Result<Vec<SyncTaskState>, AppError> {
        Ok(vec![SyncTaskState {
            local_path: "C:\\work".into(),
            id: task.unwrap_or("task").into(),
            name: "同步".into(),
            folder_id: "folder".into(),
            state: "idle".into(),
            errors: vec![],
            paused: false,
            need_bytes: 10,
            servers: ["server-id", "hidden"]
                .into_iter()
                .map(|id| SyncServerState {
                    remote_path: "/work".into(),
                    server_id: id.into(),
                    name: id.into(),
                    connected: true,
                    completion: Some(99.0),
                    need_items: Some(1),
                    need_deletes: Some(0),
                })
                .collect(),
        }])
    }
    fn sync_rescan(&self, _folder_id: &str, _timeout: Duration) -> Result<(), AppError> {
        Ok(())
    }
    fn notify(&self, event: HostEvent) {
        lock(&self.events).push(event);
    }
}
struct Fixture {
    service: McpService,
    host: Arc<Host>,
    _dir: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let dir = crate::test_support::tempdir();
        let terminals = TerminalManager::new();
        let host = Arc::new(Host::new(Arc::new(terminals.clone())));
        let service = McpService::new(host.clone(), terminals, dir.path().into()).unwrap();
        Self {
            service,
            host,
            _dir: dir,
        }
    }
    fn client(&self, entry: Entry) -> Client {
        let (server, client) = tokio::io::duplex(64 * 1024);
        let (read, write) = tokio::io::split(server);
        let handle = self.service.serve_stream(entry, read, write).unwrap();
        let (read, write) = tokio::io::split(client);
        Client {
            read: BufReader::new(read),
            write,
            handle: Some(handle),
            id: 0,
        }
    }
    async fn legacy(&self) -> Client {
        let mut client = self.client(Entry::Windows);
        client.initialize("2025-11-25").await;
        client
    }
}
struct Client {
    read: BufReader<ReadHalf<DuplexStream>>,
    write: WriteHalf<DuplexStream>,
    handle: Option<SessionHandle>,
    id: u64,
}
impl Client {
    async fn send(&mut self, value: Value) {
        self.write
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn receive(&mut self) -> Value {
        let mut text = String::new();
        tokio::time::timeout(Duration::from_secs(5), self.read.read_line(&mut text))
            .await
            .unwrap()
            .unwrap();
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("无有效响应：{text}"))
    }
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        self.send(json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params}))
            .await;
        let value = self.receive().await;
        assert_eq!(value["id"], self.id);
        value
    }
    async fn call(&mut self, name: &str, args: Value) -> Value {
        self.request("tools/call", json!({"name":name,"arguments":args}))
            .await
    }
    async fn initialize(&mut self, version: &str) {
        let reply=self.request("initialize",json!({"protocolVersion":version,"clientInfo":{"name":"测试客户端","version":"1"},"capabilities":{}})).await;
        assert_eq!(reply["result"]["protocolVersion"], version);
        assert_eq!(reply["result"]["serverInfo"]["name"], "pierlink");
        assert!(reply["result"]["capabilities"]["tools"].is_object());
        assert!(
            reply["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("sync_wait")
        );
        assert!(
            reply["result"]["instructions"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 4096
        );
        self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
    }
    async fn close(self) {
        let Self {
            read,
            write,
            handle,
            ..
        } = self;
        drop(read);
        drop(write);
        let result = tokio::task::spawn_blocking(move || handle.unwrap().wait())
            .await
            .unwrap();
        result.unwrap();
    }
}
fn meta(version: &str) -> Value {
    json!({"io.modelcontextprotocol/protocolVersion":version,"io.modelcontextprotocol/clientInfo":{"name":"现代客户端","version":"2"},"io.modelcontextprotocol/clientCapabilities":{}})
}
async fn until(mut check: impl FnMut() -> bool) {
    let started = Instant::now();
    while !check() {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "条件未在两秒内满足"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
fn error_text(value: &Value) -> &str {
    value["result"]["content"][0]["text"].as_str().unwrap()
}

#[tokio::test]
async fn 双向字节流也能服务() {
    let fixture = Fixture::new();
    let (server, client) = tokio::io::duplex(64 * 1024);
    let handle = fixture
        .service
        .serve_duplex(Entry::Windows, server)
        .unwrap();
    let (read, write) = tokio::io::split(client);
    let mut client = Client {
        read: BufReader::new(read),
        write,
        handle: Some(handle),
        id: 0,
    };
    client.initialize("2025-11-25").await;
    client.close().await;
}

#[tokio::test]
async fn 旧式版本和工具列表不等待入口授权() {
    for version in ["2025-06-18", "2025-11-25"] {
        let fixture = Fixture::new();
        fixture.host.enabled.store(false, Ordering::Release);
        fixture
            .host
            .permission_blocked
            .store(true, Ordering::Release);
        let mut client = fixture.client(Entry::Windows);
        client.initialize(version).await;
        let list = client.request("tools/list", json!({})).await;
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 20);
        assert!(
            tools
                .iter()
                .all(|tool| tool["inputSchema"]["type"] == "object")
        );
        fixture
            .host
            .permission_blocked
            .store(false, Ordering::Release);
        let result = client.call("list_targets", json!({})).await;
        assert_eq!(result["result"]["isError"], true);
        assert!(error_text(&result).contains("没有打开这个入口"));
        assert_eq!(fixture.host.confirmations.load(Ordering::Acquire), 0);
        assert_eq!(fixture.service.sessions()[0].client.name, "测试客户端");
        client.close().await;
        assert!(fixture.service.sessions().is_empty());
        let events = lock(&fixture.host.events);
        assert!(
            matches!(&events[0],HostEvent::SessionOpened(info) if info.client.name=="测试客户端")
        );
        assert!(matches!(events.last(), Some(HostEvent::SessionClosed(_))));
    }
}
#[tokio::test]
async fn 现代发现无握手及不支持版本() {
    let fixture = Fixture::new();
    let mut client = fixture.client(Entry::Windows);
    let discover = client
        .request("server/discover", json!({"_meta":meta("2026-07-28")}))
        .await;
    assert_eq!(
        discover["result"]["supportedVersions"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    let result = client
        .request(
            "tools/call",
            json!({"name":"list_targets","arguments":{},"_meta":meta("2026-07-28")}),
        )
        .await;
    assert_eq!(result["result"]["isError"], false);
    assert_eq!(
        result["result"]["structuredContent"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(fixture.service.sessions()[0].client.name, "现代客户端");
    let error = client
        .request("tools/list", json!({"_meta":meta("1900-01-01")}))
        .await;
    assert_eq!(error["error"]["code"], -32022);
    assert_eq!(
        error["error"]["data"]["supported"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    client.close().await;
    let mut client = fixture.client(Entry::Windows);
    let error = client
        .request("tools/list", json!({"_meta":meta("1900-01-01")}))
        .await;
    assert_eq!(error["error"]["code"], -32022);
    client.initialize("2025-11-25").await;
    client.close().await;
}
#[tokio::test]
async fn 拒绝连接后保持拒绝且总是允许按入口记住() {
    let fixture = Fixture::new();
    *lock(&fixture.host.decision) = Confirmation::Deny;
    let mut client = fixture.legacy().await;
    for name in ["list_targets", "run_command"] {
        let result = client.call(name, json!({})).await;
        assert!(error_text(&result).contains("用户拒绝了这个连接"));
    }
    assert_eq!(fixture.host.confirmations.load(Ordering::Acquire), 1);
    client.close().await;
    *lock(&fixture.host.decision) = Confirmation::AllowAlways;
    let mut client = fixture.legacy().await;
    assert_eq!(
        client.call("list_targets", json!({})).await["result"]["isError"],
        false
    );
    assert!(fixture.host.remembered.load(Ordering::Acquire));
    client.close().await;
    let mut client = fixture.legacy().await;
    assert_eq!(
        client.call("list_targets", json!({})).await["result"]["isError"],
        false
    );
    assert_eq!(fixture.host.confirmations.load(Ordering::Acquire), 2);
    client.close().await;
}
#[tokio::test]
async fn 按名称和编号匹配目标且服务器命令原样交给宿主() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    for target in ["server-id", "计算服务器"] {
        let result = client
            .call(
                "run_command",
                json!({"target":target,"command":"pwd","cwd":"/a b","timeout_secs":1800}),
            )
            .await;
        let output = &result["result"]["structuredContent"];
        assert_eq!(output["exit_code"], 7);
        assert_eq!(output["stdout"], "out\n�");
        assert_eq!(output["stderr"], "err\n");
        assert_eq!(output["timed_out"], false);
        assert_eq!(output["truncated"], false);
        assert_eq!(output["duration_ms"], 42);
    }
    assert_eq!(lock(&fixture.host.commands).as_slice(), ["pwd", "pwd"]);
    for target in ["hidden", "不存在"] {
        assert_eq!(
            client
                .call("run_command", json!({"target":target,"command":"whoami"}))
                .await["error"]["code"],
            -32602
        );
    }
    let result = client
        .call(
            "run_command",
            json!({"target":"server-id","command":"login"}),
        )
        .await;
    assert!(error_text(&result).contains("需要在 Pierlink 里登录"));
    assert!(
        lock(&fixture.host.events).iter().any(
            |event| matches!(event,HostEvent::LoginNeeded{server_id} if server_id=="server-id")
        )
    );
    lock(&fixture.host.events).clear();
    let result = client
        .call(
            "run_command",
            json!({"target":"server-id","command":"auth-limit"}),
        )
        .await;
    assert!(error_text(&result).contains("ssh.too_many_auth_attempts"));
    assert!(
        !lock(&fixture.host.events)
            .iter()
            .any(|event| matches!(event, HostEvent::LoginNeeded { .. }))
    );
    lock(&fixture.host.permissions).servers.clear();
    assert_eq!(
        client
            .call(
                "run_command",
                json!({"target":"server-id","command":"whoami"})
            )
            .await["error"]["code"],
        -32602
    );
    client.close().await;
}
#[tokio::test]
async fn 只允许文件夹时不能在_windows_上执行命令或开终端() {
    let fixture = Fixture::new();
    lock(&fixture.host.permissions).windows_commands = false;
    let mut client = fixture.legacy().await;
    let targets = client.call("list_targets", json!({})).await;
    assert!(
        targets["result"]["structuredContent"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["target"] == "windows")
    );
    for (tool, args) in [
        (
            "run_command",
            json!({"target":"windows","command":"whoami"}),
        ),
        ("open_terminal", json!({"target":"windows"})),
    ] {
        let result = client.call(tool, args).await;
        assert_eq!(result["result"]["isError"], true, "{tool}");
        assert!(error_text(&result).contains("没有允许在 Windows 上执行命令"));
    }
    assert!(lock(&fixture.host.commands).is_empty());
    client.close().await;
}
// 要开真实的本机终端：GitHub 的 Windows 测试环境里建不了 Windows 终端资源，和下面同类测试一样只在 Unix 上跑
#[cfg(unix)]
#[tokio::test]
async fn 没有执行命令权限时看不到也操作不了_windows_的_ai_终端() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    // 有权限时打开一个本机 AI 终端（比如别的入口打开的）
    let opened = client
        .call("open_terminal", json!({"target":"windows"}))
        .await;
    let id = opened["result"]["structuredContent"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // 之后这个入口只剩文件夹权限
    lock(&fixture.host.permissions).windows_commands = false;
    let listed = client.call("list_terminals", json!({})).await;
    assert!(
        !listed["result"]["structuredContent"]
            .to_string()
            .contains(&id)
    );
    for (tool, args) in [
        (
            "terminal_send",
            json!({"terminal_id":id,"text":"whoami","keys":["Enter"]}),
        ),
        ("terminal_read", json!({"terminal_id":id})),
        ("terminal_screen", json!({"terminal_id":id})),
        ("close_terminal", json!({"terminal_id":id})),
    ] {
        assert_eq!(
            client.call(tool, args).await["error"]["code"],
            -32602,
            "{tool}"
        );
    }
    client.close().await;
}
#[tokio::test]
async fn 危险确认收到命令全文且拒绝后不执行() {
    let fixture = Fixture::new();
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    let mut client = fixture.legacy().await;
    let command = "rm -rf /tmp/x";
    let result = client
        .call(
            "run_command",
            json!({"target":"server-id","command":command}),
        )
        .await;
    assert!(error_text(&result).contains("用户拒绝执行"));
    assert_eq!(lock(&fixture.host.dangers).as_slice(), [command]);
    assert!(lock(&fixture.host.commands).is_empty());
    assert_eq!(fixture.service.activity(1)[0].outcome, "denied");
    client.close().await;
}
// 要开真实的本机终端：GitHub 的 Windows 测试环境里建不了 Windows 终端资源，和下面同类测试一样只在 Unix 上跑
#[cfg(unix)]
#[tokio::test]
async fn 文字和回车分两次发送时回车那一次确认危险命令() {
    let fixture = Fixture::new();
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    let mut client = fixture.legacy().await;
    let opened = client
        .call("open_terminal", json!({"target":"windows"}))
        .await;
    let id = opened["result"]["structuredContent"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // 只发文字：还没提交，不用确认
    let sent = client
        .call(
            "terminal_send",
            json!({"terminal_id":id,"text":"rm -rf /tmp/x"}),
        )
        .await;
    assert_eq!(sent["result"]["isError"], false);
    assert!(lock(&fixture.host.dangers).is_empty());
    // 单独发回车：按整行判断，拒绝后不发送
    for enter in ["Enter", "Ctrl+M"] {
        let result = client
            .call("terminal_send", json!({"terminal_id":id,"keys":[enter]}))
            .await;
        assert!(error_text(&result).contains("用户拒绝执行"), "{enter}");
    }
    assert_eq!(
        lock(&fixture.host.dangers).as_slice(),
        ["rm -rf /tmp/x", "rm -rf /tmp/x"]
    );
    // Ctrl+C 清掉这一行之后再回车：不再确认
    let cleared = client
        .call(
            "terminal_send",
            json!({"terminal_id":id,"keys":["Ctrl+C","Enter"]}),
        )
        .await;
    assert_eq!(cleared["result"]["isError"], false);
    assert_eq!(lock(&fixture.host.dangers).len(), 2);
    client.close().await;
}
#[tokio::test]
async fn 取消通知和断开连接都立即取消阻塞宿主() {
    for disconnect in [false, true] {
        let fixture = Fixture::new();
        fixture.host.block.store(true, Ordering::Release);
        let mut client = fixture.legacy().await;
        client.send(json!({"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"run_command","arguments":{"target":"server-id","command":"block"}}})).await;
        until(|| fixture.host.started.load(Ordering::Acquire) == 1).await;
        if disconnect {
            client.close().await;
        } else {
            client.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":100}})).await;
            until(|| fixture.host.cancelled.load(Ordering::Acquire) == 1).await;
            assert_eq!(
                client.call("list_targets", json!({})).await["result"]["isError"],
                false
            );
            client.close().await;
        }
        assert_eq!(fixture.host.cancelled.load(Ordering::Acquire), 1);
        until(|| {
            fixture
                .service
                .activity(10)
                .iter()
                .any(|record| record.outcome == "cancelled")
        })
        .await;
    }
}
#[tokio::test]
async fn 每会话八个并发并且只确认一次() {
    let fixture = Fixture::new();
    fixture.host.block.store(true, Ordering::Release);
    let mut client = fixture.legacy().await;
    for id in 100..108 {
        client.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"run_command","arguments":{"target":"server-id","command":"block"}}})).await;
    }
    until(|| fixture.host.started.load(Ordering::Acquire) == 8).await;
    assert_eq!(fixture.host.confirmations.load(Ordering::Acquire), 1);
    let result = client.call("list_targets", json!({})).await;
    assert!(error_text(&result).contains("同时进行的操作太多"));
    for id in 100..108 {
        client.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}})).await;
    }
    until(|| fixture.host.cancelled.load(Ordering::Acquire) == 8).await;
    client.close().await;
}
#[tokio::test]
async fn 每入口八个会话且全部停止会断开所有连接() {
    let fixture = Fixture::new();
    let mut clients = vec![];
    for _ in 0..8 {
        clients.push(fixture.legacy().await);
    }
    let mut ninth = fixture.client(Entry::Windows);
    let result=ninth.request("initialize",json!({"protocolVersion":"2025-11-25","clientInfo":{"name":"测试","version":"1"},"capabilities":{}})).await;
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("最多允许 8 个")
    );
    ninth.close().await;
    let mut other = fixture.client(Entry::Server {
        server_id: "entry".into(),
    });
    other.initialize("2025-11-25").await;
    clients.push(other);
    let pending = fixture.client(Entry::Server {
        server_id: "pending".into(),
    });
    clients.push(pending);
    fixture.service.stop_all();
    for mut client in clients {
        let mut line = String::new();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), client.read.read_line(&mut line))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        client.close().await;
    }
    assert!(fixture.service.sessions().is_empty());
}
#[tokio::test]
async fn 单会话句柄取消与运行时句柄() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    assert_eq!(
        fixture.service.runtime().spawn(async { 42 }).await.unwrap(),
        42
    );
    client.handle.as_ref().unwrap().cancel();
    let mut line = String::new();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), client.read.read_line(&mut line))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    client.close().await;
}
#[tokio::test]
async fn 审计与返回结果遮蔽秘密且内存保留三十条() {
    let fixture = Fixture::new();
    fixture
        .service
        .add_secret(zeroize::Zeroizing::new("服务入口令牌".into()));
    fixture
        .service
        .add_secret(zeroize::Zeroizing::new("".into()));
    fixture
        .service
        .add_secret(zeroize::Zeroizing::new("服务入口".into()));
    lock(&fixture.host.output).stdout = "服务入口令牌 password=原样输出".as_bytes().to_vec();
    let mut client = fixture.legacy().await;
    let result = client
        .call(
            "run_command",
            json!({"target":"server-id","command":"echo 服务入口令牌\npassword=敏感值"}),
        )
        .await;
    assert!(!result.to_string().contains("服务入口"));
    assert_eq!(
        result["result"]["structuredContent"]["stdout"],
        "*** password=原样输出"
    );
    let record = fixture.service.activity(1).remove(0);
    assert!(!record.summary.contains("敏感值"));
    assert!(!record.summary.contains("服务入口"));
    assert_eq!(record.exit_code, Some(7));
    let long = format!("{}password=不能出现", "甲".repeat(800));
    client
        .call("run_command", json!({"target":"server-id","command":long}))
        .await;
    assert!(fixture.service.activity(1)[0].summary.len() <= 2048);
    for _ in 0..205 {
        client.call("list_targets", json!({})).await;
    }
    assert_eq!(fixture.service.activity(999).len(), 30);
    assert!(fixture.service.activity(0).is_empty());
    assert_eq!(fixture.service.activity(1)[0].tool, "list_targets");
    let logs = std::fs::read_dir(fixture._dir.path())
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<String>();
    assert!(!logs.contains("敏感值"));
    assert!(!logs.contains("服务入口"));
    assert!(!logs.contains("原样输出"));
    client.close().await;
}
#[tokio::test]
async fn 参数无效与本机工具和同步状态() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    for (name, args) in [
        ("run_command", json!({"target":"server-id"})),
        (
            "run_command",
            json!({"target":"server-id","command":"x","timeout_secs":1801}),
        ),
        ("run_command", json!({"target":"server-id","command":123})),
        ("list_targets", json!({"password":"秘密"})),
        (
            "terminal_send",
            json!({"terminal_id":"x","keys":["Invalid"]}),
        ),
        ("terminal_wait", json!({"terminal_id":"x","pattern":"["})),
        ("read_file", json!({"file_path":"C:\\work\\x","offset":-1})),
        ("unknown", json!({})),
    ] {
        let result = client.call(name, args).await;
        assert_eq!(result["error"]["code"], -32602, "{result}");
        assert!(!result.to_string().contains("秘密"));
    }
    lock(&fixture.host.permissions).folders = vec![fixture._dir.path().into()];
    let path = fixture._dir.path().join("文件.txt");
    for (name, args) in [
        (
            "run_command",
            json!({"target":"windows","command":"echo ok"}),
        ),
        (
            "write_file",
            json!({"file_path":path,"content":"文件内容不记录"}),
        ),
        ("read_file", json!({"file_path":path})),
        ("list_dir", json!({"path":fixture._dir.path()})),
        ("sync_wait", json!({"timeout_secs":1})),
    ] {
        let result = client.call(name, args).await;
        assert_eq!(result["result"]["isError"], false, "{result}");
    }
    assert!(
        !serde_json::to_string(&fixture.service.activity(100))
            .unwrap()
            .contains("文件内容不记录")
    );
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    let result = client
        .call(
            "write_file",
            json!({"file_path":fixture._dir.path().join("start.ps1"),"content":"x"}),
        )
        .await;
    assert_eq!(error_text(&result), "用户拒绝执行");
    let result = client.call("sync_status", json!({"task":"mine"})).await;
    let state = &result["result"]["structuredContent"][0];
    assert_eq!(state["id"], "mine");
    assert_eq!(state["servers"].as_array().unwrap().len(), 1);
    client.close().await;
}
#[cfg(unix)]
#[tokio::test]
async fn 终端登记退出清理会话断开保留以及拒绝用户终端() {
    let fixture = Fixture::new();
    let user = fixture.host.user_terminal();
    let mut client = fixture.legacy().await;
    assert_eq!(
        client
            .call("close_terminal", json!({"terminal_id":user}))
            .await["error"]["code"],
        -32602
    );
    assert!(fixture.host.terminals.screen_text(&user).is_ok());
    let opened = client
        .call(
            "open_terminal",
            json!({"target":"server-id","cwd":"/work","title":"测试"}),
        )
        .await;
    let id = opened["result"]["structuredContent"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();
    for (name, args) in [
        ("terminal_send", json!({"terminal_id":id,"text":"hello\n"})),
        ("terminal_read", json!({"terminal_id":id,"since":0})),
        ("terminal_screen", json!({"terminal_id":id})),
        ("terminal_wait", json!({"terminal_id":id,"quiet_ms":100})),
    ] {
        assert_eq!(client.call(name, args).await["result"]["isError"], false);
    }
    assert_eq!(
        client.call("list_terminals", json!({})).await["result"]["structuredContent"]["ai_terminals"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fixture.host.terminals.write(&id, b"\x04").unwrap();
    until(|| {
        lock(&fixture.host.events)
            .iter()
            .any(|event| matches!(event,HostEvent::TerminalClosed(closed) if closed==&id))
    })
    .await;
    assert_eq!(
        client.call("list_terminals", json!({})).await["result"]["structuredContent"]["ai_terminals"],
        json!([])
    );
    let opened = client
        .call("open_terminal", json!({"target":"server-id"}))
        .await;
    let id = opened["result"]["structuredContent"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();
    client.close().await;
    assert!(fixture.host.terminals.screen_text(&id).is_ok());
    let mut second = fixture.legacy().await;
    assert_eq!(
        second
            .call("close_terminal", json!({"terminal_id":id}))
            .await["result"]["structuredContent"]["closed"],
        true
    );
    let opened = second
        .call("open_terminal", json!({"target":"server-id"}))
        .await;
    let id = opened["result"]["structuredContent"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture.service.stop_all();
    until(|| fixture.host.terminals.screen_text(&id).is_err()).await;
    assert!(fixture.host.terminals.screen_text(&user).is_ok());
    second.close().await;
    fixture.host.terminals.close(&user).unwrap();
}
#[cfg(unix)]
#[tokio::test]
async fn 密码提示拒绝文字且审计不保留发送内容() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    let opened = client
        .call("open_terminal", json!({"target":"server-id"}))
        .await;
    let id = opened["result"]["structuredContent"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture.host.terminals.write(&id, b"Password:\r").unwrap();
    until(|| {
        fixture
            .host
            .terminals
            .screen_text(&id)
            .is_ok_and(|screen| safety::looks_like_password_prompt(&screen))
    })
    .await;
    let result = client
        .call(
            "terminal_send",
            json!({"terminal_id":id,"text":"不得记录的输入"}),
        )
        .await;
    assert!(error_text(&result).contains("请用户在 Pierlink"));
    assert_eq!(fixture.service.activity(1)[0].summary, "拒绝");
    assert!(
        lock(&fixture.host.events).iter().any(
            |event| matches!(event,HostEvent::PasswordPrompt{terminal_id} if terminal_id==&id)
        )
    );
    assert_eq!(
        client
            .call("terminal_send", json!({"terminal_id":id,"keys":["Ctrl+C"]}))
            .await["result"]["structuredContent"]["queued"],
        true
    );
    client.close().await;
}
#[test]
fn 危险文件路径识别() {
    for path in [
        "C:\\work\\start.PS1",
        "C:\\work\\a.bat",
        "C:\\work\\a.lnk",
        "C:\\Users\\a\\Start Menu\\Programs\\Startup\\x.txt",
        "C:\\work\\a.cmd. ",
    ] {
        assert!(tools::files::is_dangerous_path(path), "{path}");
    }
    for path in ["C:\\work\\readme.txt", "C:\\work\\startup_notes.txt"] {
        assert!(!tools::files::is_dangerous_path(path));
    }
}

#[tokio::test]
async fn 全部停止取消所有正在运行的调用() {
    let fixture = Fixture::new();
    fixture.host.block.store(true, Ordering::Release);
    let mut clients = Vec::new();
    for _ in 0..2 {
        let mut client = fixture.legacy().await;
        client.send(json!({"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"run_command","arguments":{"target":"server-id","command":"block"}}})).await;
        clients.push(client);
    }
    until(|| fixture.host.started.load(Ordering::Acquire) == 2).await;
    fixture.service.stop_all();
    until(|| fixture.host.cancelled.load(Ordering::Acquire) == 2).await;
    for client in clients {
        client.close().await;
    }
    assert!(fixture.service.sessions().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn 跨入口终端列表按当前权限隐藏地址并判断发送危险() {
    let fixture = Fixture::new();
    let mut windows = fixture.legacy().await;
    let opened = windows
        .call("open_terminal", json!({"target":"server-id"}))
        .await;
    let id = opened["result"]["structuredContent"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut server = fixture.client(Entry::Server {
        server_id: "entry".into(),
    });
    server.initialize("2025-11-25").await;
    let result = server.call("list_terminals", json!({})).await;
    assert!(
        result["result"]["structuredContent"]["ai_terminals"][0]["target"]["address"].is_null()
    );
    assert!(!result.to_string().contains("private"));
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    for args in [
        json!({"terminal_id":id,"text":"rm -rf /tmp/x\r"}),
        json!({"terminal_id":id,"text":"rm -rf /tmp/x","keys":["Enter"]}),
    ] {
        assert_eq!(
            error_text(&server.call("terminal_send", args).await),
            "用户拒绝执行"
        );
    }
    assert_eq!(
        lock(&fixture.host.dangers).as_slice(),
        ["rm -rf /tmp/x", "rm -rf /tmp/x"]
    );
    assert_eq!(
        server
            .call(
                "terminal_send",
                json!({"terminal_id":id,"text":"甲".repeat(300)})
            )
            .await["result"]["structuredContent"]["queued"],
        true
    );
    assert_eq!(fixture.service.activity(1)[0].summary.chars().count(), 200);
    lock(&fixture.host.permissions).servers.clear();
    assert_eq!(
        server.call("list_terminals", json!({})).await["result"]["structuredContent"]["ai_terminals"],
        json!([])
    );
    assert_eq!(
        server
            .call("close_terminal", json!({"terminal_id":id}))
            .await["error"]["code"],
        -32602
    );
    windows.close().await;
    server.close().await;
}

#[tokio::test]
async fn 等待首次确认的其他请求可以独立取消() {
    let fixture = Fixture::new();
    fixture
        .host
        .confirmation_blocked
        .store(true, Ordering::Release);
    let mut client = fixture.legacy().await;
    for (id, command) in [(100, "first"), (101, "second")] {
        client.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"run_command","arguments":{"target":"server-id","command":command}}})).await;
        if id == 100 {
            until(|| fixture.host.confirmations.load(Ordering::Acquire) == 1).await;
        }
    }
    client
        .send(
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":101}}),
        )
        .await;
    until(|| {
        fixture
            .service
            .activity(10)
            .iter()
            .any(|record| record.summary == "second" && record.outcome == "cancelled")
    })
    .await;
    assert_eq!(fixture.host.confirmations.load(Ordering::Acquire), 1);
    client
        .send(
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":100}}),
        )
        .await;
    until(|| {
        fixture
            .service
            .activity(10)
            .iter()
            .any(|record| record.summary == "first" && record.outcome == "cancelled")
    })
    .await;
    fixture
        .host
        .confirmation_blocked
        .store(false, Ordering::Release);
    assert_eq!(
        client.call("list_targets", json!({})).await["result"]["isError"],
        false
    );
    client.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn 后台任务跨会话读取且撤权和关闭入口时清理() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    let started = client
        .call(
            "run_command",
            json!({"target":"windows","command":"printf ready; sleep 90","background":true}),
        )
        .await;
    let id = started["result"]["structuredContent"]["job_id"]
        .as_str()
        .unwrap()
        .to_owned();
    client.close().await;
    let mut client = fixture.legacy().await;
    let before = Instant::now();
    loop {
        let value = client.call("job_output", json!({"job_id":id})).await;
        if value["result"]["structuredContent"]["stdout"] == "ready" {
            break;
        }
        assert!(before.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    lock(&fixture.host.permissions).windows_commands = false;
    let before = Instant::now();
    loop {
        fixture.service.shared.jobs.maintain(fixture.host.as_ref());
        if fixture
            .service
            .activity(30)
            .iter()
            .any(|r| r.tool == "job_completed")
        {
            break;
        }
        assert!(before.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        client.call("job_output", json!({"job_id":id})).await["result"]["isError"],
        true
    );
    lock(&fixture.host.permissions).windows_commands = true;
    let result = client.call("job_output", json!({"job_id":id})).await;
    let output = &result["result"]["structuredContent"];
    assert_eq!(output["stop_reason"], "permission_revoked");
    assert_eq!(output["running"], false);
    assert_eq!(output["timed_out"], false);
    let jobs = client.call("list_jobs", json!({})).await;
    assert_eq!(
        jobs["result"]["structuredContent"][0]["stop_reason"],
        "permission_revoked"
    );
    client.close().await;
}
#[cfg(unix)]
#[tokio::test]
async fn 后台任务不受命令时限限制() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    let started = client
        .call(
            "run_command",
            json!({"target":"windows","command":"sleep 3; printf done","background":true,"timeout_secs":1}),
        )
        .await;
    let id = started["result"]["structuredContent"]["job_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let before = Instant::now();
    loop {
        let value = client.call("job_output", json!({"job_id":id})).await;
        let out = &value["result"]["structuredContent"];
        if out["running"] == false {
            assert_eq!(
                out["stdout"], "done",
                "超过 timeout_secs 的后台任务被结束了"
            );
            break;
        }
        assert!(before.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    client.close().await;
}
#[tokio::test]
async fn 目标信息同步路径与输出预览不写入审计() {
    let fixture = Fixture::new();
    lock(&fixture.host.permissions).folders =
        vec![fixture._dir.path().into(), r"\\server\share\project".into()];
    let mut client = fixture.legacy().await;
    let targets = client.call("list_targets", json!({})).await;
    let targets = targets["result"]["structuredContent"].as_array().unwrap();
    let windows = targets.iter().find(|t| t["id"] == "windows").unwrap();
    assert_eq!(windows["commands"], true);
    assert_eq!(windows["path_style"], "windows");
    assert_eq!(windows["folders"].as_array().unwrap().len(), 1);
    assert_eq!(
        windows["unavailable_folders"][0]["code"],
        "mcp.network_folder"
    );
    assert_eq!(
        windows["unavailable_folders"][0]["params"]["reason"],
        "不支持网络盘上的文件夹"
    );
    assert!(windows["home"].is_string() && windows["shells"].is_array());
    let sync = client.call("sync_status", json!({})).await;
    assert!(sync["result"]["structuredContent"][0]["local_path"].is_string());
    assert_eq!(
        sync["result"]["structuredContent"][0]["servers"][0]["remote_path"],
        "/work"
    );
    lock(&fixture.host.output).stdout = b"unique-output-preview".to_vec();
    client
        .call(
            "run_command",
            json!({"target":"server-id","command":"echo test"}),
        )
        .await;
    assert_eq!(
        fixture.service.activity(1)[0]
            .output_preview
            .as_ref()
            .unwrap()
            .stdout,
        "unique-output-preview"
    );
    for entry in std::fs::read_dir(fixture._dir.path()).unwrap() {
        let value = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        assert!(!value.contains("unique-output-preview") && !value.contains("output_preview"));
    }
    client.close().await;
}

#[tokio::test]
async fn 编辑按写入审批并只审计路径与次数() {
    let fixture = Fixture::new();
    lock(&fixture.host.permissions).folders = vec![fixture._dir.path().into()];
    let path = fixture._dir.path().join("start.ps1");
    std::fs::write(&path, "old-marker").unwrap();
    let mut client = fixture.legacy().await;
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    let args = json!({"file_path":path,"old_string":"old-marker","new_string":"new-marker"});
    let denied = client.call("edit_file", args.clone()).await;
    assert_eq!(
        denied["result"]["structuredContent"]["code"],
        "mcp.action_denied"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "old-marker");
    *lock(&fixture.host.action_decision) = Confirmation::Allow;
    let edited = client.call("edit_file", args).await;
    assert_eq!(edited["result"]["structuredContent"]["replacements"], 1);
    let record = fixture.service.activity(1).remove(0);
    assert_eq!(record.approval, "user");
    assert!(record.summary.contains("替换 1 次") && !record.summary.contains("marker"));
    let read = client.call("read_file", json!({"file_path":path})).await;
    assert_eq!(
        read["result"]["structuredContent"]["sha256"],
        edited["result"]["structuredContent"]["sha256"]
    );
    let outside = client
        .call(
            "read_file",
            json!({"file_path":fixture._dir.path().parent().unwrap()}),
        )
        .await;
    assert_eq!(
        outside["result"]["structuredContent"]["code"],
        "mcp.path_outside"
    );
    assert!(
        outside["result"]["structuredContent"]["params"]["allowed"]
            .as_str()
            .unwrap()
            .contains(fixture._dir.path().file_name().unwrap().to_str().unwrap())
    );
    client.close().await;
}

#[tokio::test]
async fn 服务器后台运行明确说明通道限制且不执行命令() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    let value = client
        .call(
            "run_command",
            json!({"target":"server-id","command":"sleep 90","background":true}),
        )
        .await;
    assert_eq!(
        value["result"]["structuredContent"]["code"],
        "mcp.background_unsupported"
    );
    assert!(lock(&fixture.host.commands).is_empty());
    client.close().await;
}

#[tokio::test]
async fn 首次确认无人回应不记住且下一次重新询问() {
    let fixture = Fixture::new();
    *lock(&fixture.host.decision) = Confirmation::NoAnswer;
    let mut client = fixture.legacy().await;
    let result = client.call("list_targets", json!({})).await;
    assert!(error_text(&result).contains("60 秒内没人回应"));
    *lock(&fixture.host.decision) = Confirmation::Allow;
    assert_eq!(
        client.call("list_targets", json!({})).await["result"]["isError"],
        false
    );
    assert_eq!(fixture.host.confirmations.load(Ordering::Acquire), 2);
    client.close().await;
}

#[tokio::test]
async fn 操作确认无人回应不记为拒绝且可以重试() {
    let fixture = Fixture::new();
    *lock(&fixture.host.action_decision) = Confirmation::NoAnswer;
    let mut client = fixture.legacy().await;
    let args = json!({"target":"server-id","command":"rm -rf /example"});
    assert!(
        error_text(&client.call("run_command", args.clone()).await).contains("60 秒内没人回应")
    );
    assert_eq!(fixture.service.activity(1)[0].outcome, "mcp.no_answer");
    *lock(&fixture.host.action_decision) = Confirmation::Allow;
    assert_eq!(
        client.call("run_command", args).await["result"]["isError"],
        false
    );
    assert_eq!(lock(&fixture.host.dangers).len(), 2);
    client.close().await;
}

#[tokio::test]
async fn 确认返回无人回应时优先报告已取消且不记住决定() {
    let fixture = Fixture::new();
    fixture
        .host
        .cancel_on_no_answer
        .store(true, Ordering::Release);
    let mut client = fixture.legacy().await;
    *lock(&fixture.host.decision) = Confirmation::NoAnswer;
    let result = client.call("list_targets", json!({})).await;
    assert_eq!(
        result["result"]["structuredContent"]["code"],
        "mcp.cancelled"
    );
    assert!(error_text(&result).contains("操作已取消"));
    *lock(&fixture.host.decision) = Confirmation::Allow;
    assert_eq!(
        client.call("list_targets", json!({})).await["result"]["isError"],
        false
    );
    assert_eq!(fixture.host.confirmations.load(Ordering::Acquire), 2);

    *lock(&fixture.host.action_decision) = Confirmation::NoAnswer;
    let args = json!({"target":"server-id","command":"rm -rf /example"});
    let result = client.call("run_command", args.clone()).await;
    assert_eq!(
        result["result"]["structuredContent"]["code"],
        "mcp.cancelled"
    );
    assert!(error_text(&result).contains("操作已取消"));
    assert!(lock(&fixture.host.commands).is_empty());
    *lock(&fixture.host.action_decision) = Confirmation::Allow;
    assert_eq!(
        client.call("run_command", args).await["result"]["isError"],
        false
    );
    assert_eq!(lock(&fixture.host.dangers).len(), 2);
    client.close().await;
}

#[tokio::test]
async fn 本机连接空闲超过五分钟仍显示且断开即移除() {
    let fixture = Fixture::new();
    let client = fixture.legacy().await;
    let session = client.handle.as_ref().unwrap().session.clone();
    *lock(&session.last_request) = Instant::now() - Duration::from_secs(1800);
    assert_eq!(fixture.service.sessions().len(), 1);
    client.close().await;
    assert!(fixture.service.sessions().is_empty());
}

#[test]
fn 会话报告排除未接受和已停止连接且仅服务器入口检查活动时间() {
    let fixture = Fixture::new();
    let mut expected = Vec::new();
    let mut calls = Vec::new();
    for entry in [
        Entry::Windows,
        Entry::Server {
            server_id: "server-id".into(),
        },
    ] {
        for (accepted, stopped, busy) in [
            (false, false, true),
            (true, true, true),
            (true, false, false),
            (true, false, true),
        ] {
            let session = Arc::new(session::Session::new(entry.clone(), accepted));
            *lock(&session.last_request) = Instant::now() - Duration::from_secs(1800);
            if busy {
                calls.push(session.call_flag());
            }
            if stopped {
                session.cancel();
            }
            let id = session.snapshot().id;
            if accepted && !stopped && (entry == Entry::Windows || busy) {
                expected.push(id.clone());
            }
            lock(&fixture.service.shared.sessions).insert(id, session);
        }
    }
    expected.sort();
    assert_eq!(
        fixture
            .service
            .sessions()
            .into_iter()
            .map(|s| s.id)
            .collect::<Vec<_>>(),
        expected
    );
}

#[tokio::test]
async fn 宿主指纹拒绝和缺少终端程序给出明确原因() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    for (code, reason) in [
        (
            "ssh.host_key_rejected",
            "用户拒绝了这台服务器的指纹，不要立即重试",
        ),
        ("terminal.shell_not_found", "找不到终端 shell"),
    ] {
        assert!(
            error_text(
                &client
                    .call("run_command", json!({"target":"server-id","command":code}))
                    .await
            )
            .contains(reason)
        );
    }
    client.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn 终端发送游标可用于多行正则并记录打开目录() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    let opened = client
        .call(
            "open_terminal",
            json!({"target":"server-id","cwd":"/work/project"}),
        )
        .await;
    let id = opened["result"]["structuredContent"]["terminal_id"]
        .as_str()
        .unwrap();
    assert!(
        fixture.service.activity(1)[0]
            .summary
            .contains("cwd=/work/project")
    );
    let sent = client
        .call(
            "terminal_send",
            json!({"terminal_id":id,"text":"first\nREADY\r\nlast\n"}),
        )
        .await;
    let sent = &sent["result"]["structuredContent"];
    assert_eq!(sent["submitted"], json!(["first", "READY", "last"]));
    let wait = client
        .call(
            "terminal_wait",
            json!({"terminal_id":id,"pattern":"^READY$","since":sent["cursor"],"timeout_secs":3}),
        )
        .await;
    assert_eq!(
        wait["result"]["structuredContent"]["matched"], true,
        "{wait}"
    );
    client.close().await;
}
