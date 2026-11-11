//! 用字节流客户端验证协议、宿主边界和资源释放。
#[path = "approval_tests.rs"]
mod approval;
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
    confirm_connections: AtomicBool,
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
    cwd_requests: Mutex<Vec<String>>,
    action_input: Mutex<Option<(String, Vec<u8>)>>,
    action_blocked: AtomicBool,
    sftp_opens: Mutex<Vec<String>>,
    sftp_error: Mutex<Option<AppError>>,
    /// 传给宿主的图片：(服务器 ID, 内容, 扩展名)
    images: Mutex<Vec<(String, Vec<u8>, String)>>,
}
impl Host {
    fn new(terminals: Arc<TerminalManager>) -> Self {
        Self {
            enabled: AtomicBool::new(true),
            // 大部分用例要走连接确认：模拟用户在入口设置里打开了“每次连接都问”
            confirm_connections: AtomicBool::new(true),
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
            cwd_requests: Mutex::new(Vec::new()),
            action_input: Mutex::new(None),
            action_blocked: AtomicBool::new(false),
            sftp_opens: Mutex::new(Vec::new()),
            sftp_error: Mutex::new(None),
            images: Mutex::new(Vec::new()),
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
                key: None,
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
                key: None,
            });
        }
        Ok(targets)
    }
    fn confirm_connections(&self, _entry: &Entry) -> bool {
        self.confirm_connections.load(Ordering::Acquire)
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
            self.confirm_connections.store(false, Ordering::Release);
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
    fn open_sftp(&self, server_id: &str) -> Result<crate::sftp::SftpSession, AppError> {
        assert_eq!(server_id, "server-id");
        lock(&self.sftp_opens).push(server_id.into());
        if let Some(error) = lock(&self.sftp_error).clone() {
            return Err(error);
        }
        // 本机的 sftp-server 代替经 SSH 的会话：服务器上的路径就是本机路径
        #[cfg(unix)]
        return crate::sftp::SftpSession::spawn_server(std::path::Path::new(
            "/usr/lib/openssh/sftp-server",
        ));
        #[cfg(not(unix))]
        Err(AppError::new("sftp.connection_lost"))
    }
    fn upload_image(
        &self,
        server_id: &str,
        bytes: &[u8],
        extension: &str,
    ) -> Result<String, AppError> {
        lock(&self.images).push((server_id.into(), bytes.to_vec(), extension.into()));
        if let Some(error) = lock(&self.sftp_error).clone() {
            return Err(error);
        }
        Ok(format!("/tmp/pierlink-paste-test.{extension}"))
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
        // 本机工具会真的启动 shell：CI 的 Windows 上 PowerShell 冷启动偶尔要五秒以上
        tokio::time::timeout(Duration::from_secs(30), self.read.read_line(&mut text))
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
/// 开一个 AI 终端，返回终端 ID
async fn open_terminal(client: &mut Client, target: &str) -> String {
    client
        .call("open_terminal", json!({ "target": target }))
        .await["result"]["structuredContent"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned()
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
        fixture
            .host
            .permission_blocked
            .store(false, Ordering::Release);
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 28);
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
        // 入口关着时不算已连接；打开后才看得到（客户端名照样记下了）
        assert!(fixture.service.sessions().is_empty());
        fixture.host.enabled.store(true, Ordering::Release);
        assert_eq!(fixture.service.sessions()[0].client.name, "测试客户端");
        client.close().await;
        assert!(fixture.service.sessions().is_empty());
        let events = lock(&fixture.host.events);
        let lifecycle: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    HostEvent::SessionOpened(_) | HostEvent::SessionClosed(_)
                )
            })
            .collect();
        assert!(
            matches!(lifecycle[0], HostEvent::SessionOpened(info) if info.client.name=="测试客户端")
        );
        assert!(matches!(
            lifecycle.last(),
            Some(HostEvent::SessionClosed(_))
        ));
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
async fn 拒绝连接后保持拒绝且以后不再问按入口生效() {
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
    assert!(!fixture.host.confirm_connections.load(Ordering::Acquire));
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
async fn 入口没要求确认时连接直接可用() {
    let fixture = Fixture::new();
    fixture
        .host
        .confirm_connections
        .store(false, Ordering::Release);
    *lock(&fixture.host.decision) = Confirmation::Deny;
    let mut client = fixture.legacy().await;
    assert_eq!(
        client.call("list_targets", json!({})).await["result"]["isError"],
        false
    );
    assert_eq!(fixture.host.confirmations.load(Ordering::Acquire), 0);
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
async fn 审计与返回结果遮蔽秘密且内存保留二百条() {
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
    let all = fixture.service.activity(999);
    assert_eq!(all.len(), 200);
    // 只有最新的 30 条带输出预览
    assert!(all.iter().skip(30).all(|r| r.output_preview.is_none()));
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
    // AI 的回车在文字写出 SUBMIT_GAP 后才写；先按 Ctrl+D 的话回车作废，Ctrl+D 落在没提交的行上不会退出
    tokio::time::sleep(crate::terminal::SUBMIT_GAP + Duration::from_millis(300)).await;
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
#[cfg(unix)]
#[tokio::test]
async fn 粘贴的多行不算提交_回车时整段确认_程序不接受粘贴时不问就报错() {
    let fixture = Fixture::new();
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    let mut client = fixture.legacy().await;
    let opened = client
        .call("open_terminal", json!({"target":"server-id"}))
        .await;
    let id = opened["result"]["structuredContent"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let paste = json!({"terminal_id":id,"text":"rm -rf /tmp/x\nls","paste":true});
    // 终端里的程序没开括号粘贴：不问用户，直接报错
    let refused = client.call("terminal_send", paste.clone()).await;
    assert_eq!(
        refused["result"]["structuredContent"]["code"],
        "mcp.paste_unsupported"
    );
    assert!(lock(&fixture.host.dangers).is_empty());
    fixture
        .host
        .terminals
        .feed_output_at(&id, b"\x1b[?2004h", Instant::now())
        .unwrap();
    // 粘贴里的换行不提交：不用确认
    let sent = client.call("terminal_send", paste).await;
    assert_eq!(sent["result"]["isError"], false, "{sent}");
    assert!(lock(&fixture.host.dangers).is_empty());
    assert!(
        fixture.service.activity(1)[0]
            .summary
            .contains("粘贴：rm -rf /tmp/x")
    );
    // 回车时粘贴的整段一起确认
    let enter = client
        .call("terminal_send", json!({"terminal_id":id,"keys":["Enter"]}))
        .await;
    assert!(error_text(&enter).contains("用户拒绝执行"));
    assert_eq!(
        lock(&fixture.host.dangers).as_slice(),
        ["rm -rf /tmp/x\nls"]
    );
    client.close().await;
}
#[cfg(unix)]
#[tokio::test]
async fn 换行键按提交确认_撤销后回车按内容未知确认_滚轮不在全屏时不问就报错() {
    let fixture = Fixture::new();
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    let mut client = fixture.legacy().await;
    let opened = client
        .call("open_terminal", json!({"target":"server-id"}))
        .await;
    let id = opened["result"]["structuredContent"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = client
        .call(
            "terminal_send",
            json!({"terminal_id":id,"text":"rm -rf /tmp/x","keys":["Shift+Enter"]}),
        )
        .await;
    assert!(error_text(&result).contains("用户拒绝执行"));
    assert_eq!(lock(&fixture.host.dangers).as_slice(), ["rm -rf /tmp/x"]);
    // 清掉后又撤销：这一行看不到了，回车时按“内容未知”问
    let result = client
        .call(
            "terminal_send",
            json!({"terminal_id":id,"text":"rm -rf /tmp/x","keys":["Ctrl+U","Ctrl+_","Enter"]}),
        )
        .await;
    assert!(error_text(&result).contains("用户拒绝执行"));
    assert!(lock(&fixture.host.dangers)[1].starts_with("（AI 撤销了修改或粘回了删掉的文字"));
    assert_eq!(*lock(&fixture.host.danger_flags), [true, true]);
    let wheel = client
        .call(
            "terminal_send",
            json!({"terminal_id":id,"keys":["WheelUp","Enter"]}),
        )
        .await;
    assert_eq!(
        wheel["result"]["structuredContent"]["code"],
        "mcp.wheel_unavailable"
    );
    assert_eq!(lock(&fixture.host.dangers).len(), 2);
    client.close().await;
}
#[cfg(unix)]
#[tokio::test]
async fn 图片传到服务器后把路径粘贴进终端_本机终端和不是图片时不传() {
    let fixture = Fixture::new();
    lock(&fixture.host.permissions).folders = vec![fixture._dir.path().into()];
    let image = fixture._dir.path().join("截图.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\n-image").unwrap();
    let text = fixture._dir.path().join("a.txt");
    std::fs::write(&text, "x").unwrap();
    let mut client = fixture.legacy().await;
    let server = open_terminal(&mut client, "server-id").await;
    let sent = client
        .call(
            "terminal_send",
            json!({"terminal_id":server,"image_path":image,"text":" 这张图里是什么"}),
        )
        .await;
    let value = &sent["result"]["structuredContent"];
    assert_eq!(value["image"], "/tmp/pierlink-paste-test.png", "{sent}");
    {
        let images = lock(&fixture.host.images);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].0, "server-id");
        assert_eq!(images[0].1, b"\x89PNG\r\n\x1a\n-image");
        assert_eq!(images[0].2, "png");
    }
    let echoed = client
        .call(
            "terminal_wait",
            json!({"terminal_id":server,"pattern":"pierlink-paste-test\\.png 这张图","since":value["cursor"],"timeout_secs":5}),
        )
        .await;
    assert_eq!(
        echoed["result"]["structuredContent"]["matched"], true,
        "{echoed}"
    );
    assert!(
        fixture
            .service
            .activity(5)
            .iter()
            .any(|record| record.tool == "terminal_send"
                && record.summary.contains("图片：")
                && record.summary.contains("截图.png"))
    );
    // 本机终端、不是图片：不传
    let local = open_terminal(&mut client, "windows").await;
    let refused = client
        .call(
            "terminal_send",
            json!({"terminal_id":local,"image_path":image}),
        )
        .await;
    assert_eq!(
        refused["result"]["structuredContent"]["code"],
        "mcp.image_unsupported"
    );
    let not_image = client
        .call(
            "terminal_send",
            json!({"terminal_id":server,"image_path":text}),
        )
        .await;
    assert_eq!(
        not_image["result"]["structuredContent"]["params"]["reason_key"],
        "mcp.image_must_be_png_jpeg_gif_or_webp"
    );
    assert_eq!(lock(&fixture.host.images).len(), 1);
    // “每次都问”：只发图片也问；被拒绝时不传到服务器
    lock(&fixture.host.permissions).approval = Approval::AskAll;
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    let denied = client
        .call(
            "terminal_send",
            json!({"terminal_id":server,"image_path":image}),
        )
        .await;
    assert!(error_text(&denied).contains("用户拒绝执行"), "{denied}");
    assert!(
        lock(&fixture.host.dangers)
            .last()
            .unwrap()
            .starts_with("把图片传到服务器的临时目录")
    );
    assert_eq!(lock(&fixture.host.images).len(), 1);
    // 带回车被拒：确认框里是占位路径，也不传
    let denied = client
        .call(
            "terminal_send",
            json!({"terminal_id":server,"image_path":image,"keys":["Enter"]}),
        )
        .await;
    assert!(error_text(&denied).contains("用户拒绝执行"), "{denied}");
    assert!(
        lock(&fixture.host.dangers)
            .last()
            .unwrap()
            .contains("/tmp/pierlink-paste-…")
    );
    assert_eq!(lock(&fixture.host.images).len(), 1);
    client.close().await;
}
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
async fn 收回执行权限时正在跑的前台命令一秒内停止() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    let host = fixture.host.clone();
    let revoke = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        lock(&host.permissions).windows_commands = false;
    });
    let before = Instant::now();
    let result = client
        .call(
            "run_command",
            json!({"target":"windows","command":"sleep 30","timeout_secs":60}),
        )
        .await;
    revoke.await.unwrap();
    assert!(before.elapsed() < Duration::from_secs(5));
    assert_eq!(result["result"]["isError"], true);
    assert!(error_text(&result).contains("执行命令的权限已收回"));
    client.close().await;
}
#[tokio::test]
async fn 收回服务器权限或关掉入口时正在跑的服务器命令停止() {
    for close_entry in [false, true] {
        let fixture = Fixture::new();
        let mut client = fixture.legacy().await;
        fixture.host.block.store(true, Ordering::Release);
        let host = fixture.host.clone();
        let revoke = tokio::spawn(async move {
            while host.started.load(Ordering::Acquire) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if close_entry {
                host.enabled.store(false, Ordering::Release);
            } else {
                lock(&host.permissions).servers.clear();
            }
        });
        let before = Instant::now();
        let result = client
            .call(
                "run_command",
                json!({"target":"server-id","command":"sleep 30","timeout_secs":60}),
            )
            .await;
        revoke.await.unwrap();
        fixture.host.block.store(false, Ordering::Release);
        assert!(before.elapsed() < Duration::from_secs(5));
        assert_eq!(result["result"]["isError"], true, "{result}");
        assert!(error_text(&result).contains("执行命令的权限已收回"));
        assert_eq!(fixture.host.cancelled.load(Ordering::Acquire), 1);
        client.close().await;
    }
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
    // 电脑名告诉 agent 确认框弹在哪台电脑（取不到时为 null，但字段一直在）
    assert!(windows.as_object().unwrap().contains_key("computer"));
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
async fn 本机连接空闲超过五分钟不再算已连接且断开即移除() {
    let fixture = Fixture::new();
    let client = fixture.legacy().await;
    let session = client.handle.as_ref().unwrap().session.clone();
    let last = *lock(&session.last_request);
    // AI 客户端开着就一直连着管道：一分钟前用过算已连接，半小时没用就不算了
    assert_eq!(
        fixture
            .service
            .shared
            .sessions_at(last + Duration::from_secs(60))
            .len(),
        1
    );
    assert!(
        fixture
            .service
            .shared
            .sessions_at(last + Duration::from_secs(1800))
            .is_empty()
    );
    client.close().await;
    assert!(fixture.service.sessions().is_empty());
}

#[test]
fn 会话报告排除未接受和已停止连接且两种入口都检查活动时间() {
    let fixture = Fixture::new();
    let mut expected = Vec::new();
    let mut calls = Vec::new();
    let now = Instant::now() + Duration::from_secs(1800);
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
            if busy {
                calls.push(session.call_flag());
            }
            if stopped {
                session.cancel();
            }
            let id = session.snapshot().id;
            // 半小时没请求：两种入口都只剩还有调用在进行的
            if accepted && !stopped && busy {
                expected.push(id.clone());
            }
            lock(&fixture.service.shared.sessions).insert(id, session);
        }
    }
    expected.sort();
    assert_eq!(
        fixture
            .service
            .shared
            .sessions_at(now)
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

#[test]
fn 可见会话集合过期只通知一次且本机连接也按时间过期() {
    let fixture = Fixture::new();
    let shared = &fixture.service.shared;
    let local = Arc::new(session::Session::new(Entry::Windows, true));
    let remote = Arc::new(session::Session::new(
        Entry::Server {
            server_id: "server-id".into(),
        },
        true,
    ));
    let last = *lock(&remote.last_request);
    // 两个连接的最后请求时间对齐，过期的时刻才确定
    *lock(&local.last_request) = last;
    for session in [&local, &remote] {
        lock(&shared.sessions).insert(session.snapshot().id, session.clone());
    }
    let mut previous = std::collections::HashSet::new();
    assert!(shared.sessions_changed(&mut previous, last + Duration::from_secs(300)));
    assert_eq!(previous.len(), 2);
    assert!(!shared.sessions_changed(&mut previous, last + Duration::from_secs(300)));
    // 五分钟没请求：本机连接和服务器连接一样不再算已连接
    assert!(shared.sessions_changed(&mut previous, last + Duration::from_secs(301)));
    assert!(previous.is_empty());
    assert!(!shared.sessions_changed(&mut previous, last + Duration::from_secs(1800)));
    let call = remote.call_flag();
    assert!(shared.sessions_changed(&mut previous, last + Duration::from_secs(1800)));
    let statuses = shared.statuses_at(last + Duration::from_secs(1800));
    let busy = statuses
        .iter()
        .find(|s| s.info.id == remote.snapshot().id)
        .unwrap();
    assert!(busy.busy);
    assert!(busy.last_active_ms > 0);
    drop(call);
    assert!(shared.sessions_changed(&mut previous, last + Duration::from_secs(1800)));
    assert!(
        lock(&fixture.host.events)
            .iter()
            .any(|event| matches!(event, HostEvent::SessionsChanged))
    );
}

#[test]
fn 入口关着时会话不算已连接() {
    let fixture = Fixture::new();
    let shared = &fixture.service.shared;
    let local = Arc::new(session::Session::new(Entry::Windows, true));
    lock(&shared.sessions).insert(local.snapshot().id, local.clone());
    let now = Instant::now();
    assert_eq!(shared.sessions_at(now).len(), 1);
    // 关掉 AI 接入：管道照样连着，界面上不显示“已连接”，状态列表也没有它
    fixture.host.enabled.store(false, Ordering::Release);
    assert!(shared.sessions_at(now).is_empty());
    assert!(shared.statuses_at(now).is_empty());
    let mut previous = std::collections::HashSet::from([(local.snapshot().id, false)]);
    assert!(shared.sessions_changed(&mut previous, now));
    assert!(previous.is_empty());
    fixture.host.enabled.store(true, Ordering::Release);
    assert_eq!(shared.sessions_at(now).len(), 1);
}

#[tokio::test]
async fn 文件审批显示修改预览且拒绝时不创建目录和不记录正文() {
    let fixture = Fixture::new();
    {
        let mut permissions = lock(&fixture.host.permissions);
        permissions.approval = Approval::AskAll;
        permissions.windows_commands = false;
        permissions.folders = vec![fixture._dir.path().into()];
    }
    let mut client = fixture.client(Entry::Server {
        server_id: "server-id".into(),
    });
    client.initialize("2025-11-25").await;
    let path = fixture._dir.path().join("src/components/New.tsx");
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    let content = format!("{}\n2\n3\n4\n5\n6\n禁止显示第七行", "甲".repeat(320));
    let result = client
        .call("write_file", json!({"file_path":path,"content":content}))
        .await;
    assert_eq!(result["result"]["isError"], true);
    assert!(!fixture._dir.path().join("src").exists());
    let action = lock(&fixture.host.dangers)[0].clone();
    assert!(action.contains(&format!("{} 字节", content.len())));
    assert!(action.contains("…（共 320 字）"));
    assert!(!action.contains("禁止显示第七行"));
    assert!(!result.to_string().contains("甲"));
    *lock(&fixture.host.action_decision) = Confirmation::Allow;
    assert_eq!(
        client
            .call("write_file", json!({"file_path":path,"content":content}))
            .await["result"]["isError"],
        false
    );
    let replacement = "乙".repeat(350);
    let result = client
        .call(
            "edit_file",
            json!({"file_path":path,"old_string":"甲".repeat(320),"new_string":replacement}),
        )
        .await;
    assert_eq!(result["result"]["isError"], false);
    let action = lock(&fixture.host.dangers).last().unwrap().clone();
    assert!(action.contains("旧文字："));
    assert!(action.contains("新文字："));
    assert!(action.contains("…（共 350 字）"));
    for record in fixture.service.activity(30) {
        assert!(!record.summary.contains(['甲', '乙']));
        assert!(record.output_preview.is_none());
    }
    let audit_files = std::fs::read_dir(fixture._dir.path()).unwrap();
    for entry in audit_files {
        let path = entry.unwrap().path();
        if path.is_file() {
            let text = std::fs::read_to_string(path).unwrap();
            assert!(!text.contains(['甲', '乙']));
        }
    }
    client.close().await;
}

#[tokio::test]
async fn 多处修改一次确认_列出每处新旧文字_审计记总替换次数() {
    let fixture = Fixture::new();
    {
        let mut permissions = lock(&fixture.host.permissions);
        permissions.approval = Approval::AskAll;
        permissions.folders = vec![fixture._dir.path().into()];
    }
    let path = fixture._dir.path().join("main.rs");
    std::fs::write(&path, "fn a() {}\nfn b() {}\nlet x = 1;\nlet x = 1;\n").unwrap();
    let mut client = fixture.legacy().await;
    let edits: Vec<Value> = (0..7)
        .map(|i| {
            if i == 0 {
                json!({"old_string":"fn a()","new_string":"fn alpha()"})
            } else if i == 1 {
                json!({"old_string":"let x = 1;","new_string":"let x = 2;","replace_all":true})
            } else {
                json!({"old_string":"fn b() {}","new_string":"fn b() {}"})
            }
        })
        .collect();
    let result = client
        .call("edit_file", json!({"file_path":path,"edits":edits}))
        .await;
    assert_eq!(
        result["result"]["structuredContent"]["replacements"], 8,
        "{result}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "fn alpha() {}\nfn b() {}\nlet x = 2;\nlet x = 2;\n"
    );
    let action = lock(&fixture.host.dangers)[0].clone();
    assert!(action.contains("\n共 7 处修改\n第 1 处旧文字：\nfn a()\n第 1 处新文字：\nfn alpha()"));
    assert!(action.contains("第 5 处新文字：") && !action.contains("第 6 处"));
    assert!(action.ends_with("\n…（还有 2 处）"));
    assert!(
        fixture.service.activity(1)[0]
            .summary
            .ends_with("（替换 8 次）")
    );
    client.close().await;
}

#[tokio::test]
async fn 新目录工具按写入审批且删除空目录也确认() {
    for mode in [Approval::AskAll, Approval::Dangerous, Approval::Auto] {
        let fixture = Fixture::new();
        {
            let mut permissions = lock(&fixture.host.permissions);
            permissions.approval = mode;
            permissions.windows_commands = false;
            permissions.folders = vec![fixture._dir.path().into()];
        }
        let mut client = fixture.legacy().await;
        let path = fixture._dir.path().join("dir");
        let to = fixture._dir.path().join("renamed");
        for (name, args) in [
            ("make_dir", json!({"path":path})),
            ("make_dir", json!({"path":path})),
            ("rename_path", json!({"from":path,"to":to})),
            ("delete_path", json!({"path":to})),
        ] {
            let result = client.call(name, args).await;
            assert_eq!(result["result"]["isError"], false, "{result}");
        }
        let expected = match mode {
            Approval::AskAll => vec![false, false, false, true],
            Approval::Dangerous => vec![true],
            Approval::Auto => vec![],
        };
        assert_eq!(*lock(&fixture.host.danger_flags), expected);
        if mode != Approval::Auto {
            assert!(
                lock(&fixture.host.dangers)
                    .last()
                    .unwrap()
                    .contains("递归删除：false")
            );
        }
        assert!(!to.exists());
        client.close().await;
    }
}

#[tokio::test]
async fn 复制按写入规则审批_要确认的目标算危险_拒绝时什么都不复制() {
    let fixture = Fixture::new();
    {
        let mut permissions = lock(&fixture.host.permissions);
        permissions.windows_commands = false;
        permissions.folders = vec![fixture._dir.path().into()];
    }
    let source = fixture._dir.path().join("src");
    std::fs::create_dir_all(source.join("tools")).unwrap();
    std::fs::write(source.join("readme.md"), "说明").unwrap();
    std::fs::write(source.join("tools/run.ps1"), "Write-Output 1").unwrap();
    let mut client = fixture.legacy().await;
    let target = fixture._dir.path().join("dst");
    // 只问危险操作：目标里有脚本，按危险确认；拒绝时什么都不复制
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    let result = client
        .call("copy_path", json!({"source":source,"destination":target}))
        .await;
    assert!(error_text(&result).contains("用户拒绝执行"));
    assert!(!target.exists());
    assert_eq!(*lock(&fixture.host.danger_flags), [true]);
    assert!(lock(&fixture.host.dangers)[0].contains("run.ps1"));
    *lock(&fixture.host.action_decision) = Confirmation::Allow;
    let result = client
        .call("copy_path", json!({"source":source,"destination":target}))
        .await;
    assert_eq!(
        result["result"]["structuredContent"]["files"], 2,
        "{result}"
    );
    assert!(target.join("tools/run.ps1").is_file());
    // 普通文件：只问危险操作时不问；已存在又没给 overwrite 时不问就报错
    let plain = fixture._dir.path().join("readme-copy.md");
    for _ in 0..2 {
        client
            .call(
                "copy_path",
                json!({"source":source.join("readme.md"),"destination":plain}),
            )
            .await;
    }
    assert_eq!(lock(&fixture.host.danger_flags).len(), 2);
    assert_eq!(fixture.service.activity(1)[0].outcome, "mcp.path_exists");
    assert!(
        fixture.service.activity(1)[0]
            .summary
            .ends_with("readme-copy.md")
    );
    client.close().await;
}

#[tokio::test]
async fn 写二进制按原样写入_程序类文件要确认_无效的_base64_不问就报错() {
    let fixture = Fixture::new();
    {
        let mut permissions = lock(&fixture.host.permissions);
        permissions.windows_commands = false;
        permissions.folders = vec![fixture._dir.path().into()];
    }
    let mut client = fixture.legacy().await;
    let path = fixture._dir.path().join("bin/tool.exe");
    let result = client
        .call(
            "write_file",
            json!({"file_path":path,"content_base64":"TVqQ\nAAMA"}),
        )
        .await;
    let written = &result["result"]["structuredContent"];
    assert_eq!(written["bytes"], 6, "{result}");
    assert_eq!(std::fs::read(&path).unwrap(), b"MZ\x90\x00\x03\x00");
    assert_eq!(*lock(&fixture.host.danger_flags), [true]);
    assert!(lock(&fixture.host.dangers)[0].ends_with("内容：6 字节（content_base64，原样写入）"));
    let invalid = client
        .call(
            "write_file",
            json!({"file_path":path,"content_base64":"不是base64"}),
        )
        .await;
    assert_eq!(invalid["error"]["code"], -32602);
    assert_eq!(
        invalid["error"]["data"]["params"]["reason_key"],
        "mcp.content_base64_is_not_valid_base64"
    );
    assert_eq!(lock(&fixture.host.danger_flags).len(), 1);
    client.close().await;
}

/// 只授权 Windows 一侧的 win 文件夹；“服务器”是本机的 sftp-server，路径就是本机路径
#[cfg(unix)]
fn transfer_fixture() -> (Fixture, std::path::PathBuf, std::path::PathBuf) {
    let fixture = Fixture::new();
    let win = fixture._dir.path().join("win");
    let server = fixture._dir.path().join("server");
    std::fs::create_dir_all(&win).unwrap();
    std::fs::create_dir_all(&server).unwrap();
    {
        let mut permissions = lock(&fixture.host.permissions);
        permissions.windows_commands = false;
        permissions.folders = vec![win.clone()];
    }
    (fixture, win, server)
}

#[cfg(unix)]
#[tokio::test]
async fn 在_windows_和服务器之间传文件_自动建父目录_已存在要_overwrite() {
    let (fixture, win, server) = transfer_fixture();
    let mut client = fixture.legacy().await;
    std::fs::write(win.join("a.bin"), [0u8, 1, 2, 255]).unwrap();
    let up = json!({"from_target":"windows","from_path":win.join("a.bin"),"to_target":"server-id","to_path":server.join("新/层/a.bin")});
    let result = client.call("transfer_file", up.clone()).await;
    assert_eq!(
        result["result"]["structuredContent"]["bytes"], 4,
        "{result}"
    );
    assert_eq!(
        std::fs::read(server.join("新/层/a.bin")).unwrap(),
        [0, 1, 2, 255]
    );
    let record = fixture.service.activity(1)[0].clone();
    assert_eq!(record.target.as_deref(), Some("server-id"));
    assert!(record.summary.starts_with("windows:"));
    // 已存在又没给 overwrite：不问用户就报错
    let again = client.call("transfer_file", up).await;
    assert_eq!(
        again["result"]["structuredContent"]["code"],
        "mcp.path_exists"
    );
    // 下载回 Windows，覆盖已有文件
    std::fs::write(server.join("新/层/a.bin"), "服务器上改过").unwrap();
    let down = json!({"from_target":"计算服务器","from_path":server.join("新/层/a.bin"),"to_target":"windows","to_path":win.join("a.bin"),"overwrite":true});
    let result = client.call("transfer_file", down).await;
    assert_eq!(result["result"]["isError"], false, "{result}");
    assert_eq!(
        std::fs::read_to_string(win.join("a.bin")).unwrap(),
        "服务器上改过"
    );
    assert!(lock(&fixture.host.dangers).is_empty());
    assert_eq!(lock(&fixture.host.sftp_opens).len(), 3);
    client.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn 传文件的路径和权限边界_写到配置要确认_要登录时提醒用户() {
    let (fixture, win, server) = transfer_fixture();
    let mut client = fixture.legacy().await;
    std::fs::write(win.join("key.pub"), "ssh-ed25519 AAAA").unwrap();
    std::fs::write(fixture._dir.path().join("outside.txt"), "x").unwrap();
    let code = |value: &Value| value["result"]["structuredContent"]["code"].clone();
    let reason_key = |value: &Value| value["error"]["data"]["params"]["reason_key"].clone();
    // 两端都是 Windows、服务器路径不是绝对路径：参数错误
    let both = client
        .call("transfer_file", json!({"from_target":"windows","from_path":win.join("key.pub"),"to_target":"windows","to_path":win.join("b")}))
        .await;
    assert_eq!(reason_key(&both), "mcp.transfer_needs_windows_and_a_server");
    let relative = client
        .call("transfer_file", json!({"from_target":"windows","from_path":win.join("key.pub"),"to_target":"server-id","to_path":"key.pub"}))
        .await;
    assert_eq!(reason_key(&relative), "mcp.server_path_must_be_absolute");
    // Windows 一侧在授权文件夹外
    let outside = client
        .call("transfer_file", json!({"from_target":"windows","from_path":fixture._dir.path().join("outside.txt"),"to_target":"server-id","to_path":server.join("x")}))
        .await;
    assert_eq!(code(&outside), "mcp.path_outside");
    let outside = client
        .call("transfer_file", json!({"from_target":"server-id","from_path":server.join("x"),"to_target":"windows","to_path":fixture._dir.path().join("new.txt")}))
        .await;
    assert_eq!(code(&outside), "mcp.path_outside");
    assert!(lock(&fixture.host.sftp_opens).is_empty());
    // 写到服务器的 .ssh 下：按危险确认，拒绝时什么都不写
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    let keys = server.join(".ssh/authorized_keys");
    let denied = client
        .call("transfer_file", json!({"from_target":"windows","from_path":win.join("key.pub"),"to_target":"server-id","to_path":keys}))
        .await;
    assert!(error_text(&denied).contains("用户拒绝执行"));
    assert_eq!(*lock(&fixture.host.danger_flags), [true]);
    let action = lock(&fixture.host.dangers)[0].clone();
    assert!(
        action.starts_with("写入 SSH、Git 或 shell 的配置可能让它们执行命令："),
        "{action}"
    );
    assert!(
        action.contains("\n传输文件：Windows ")
            && action.contains("大小：16 字节；覆盖已有目标：false")
    );
    assert!(!server.join(".ssh").exists());
    // 服务器要密码：报错并提醒用户登录
    *lock(&fixture.host.sftp_error) = Some(AppError::new("ssh.password_needed"));
    let login = client
        .call("transfer_file", json!({"from_target":"windows","from_path":win.join("key.pub"),"to_target":"server-id","to_path":server.join("k")}))
        .await;
    assert_eq!(code(&login), "ssh.password_needed");
    assert!(error_text(&login).contains("登录"));
    assert!(lock(&fixture.host.events).iter().any(
        |event| matches!(event, HostEvent::LoginNeeded { server_id } if server_id == "server-id")
    ));
    client.close().await;
}

#[tokio::test]
async fn 打开文件要执行命令权限_每次都问时确认_拒绝时不打开() {
    let fixture = Fixture::new();
    let doc = fixture._dir.path().join("说明.pdf");
    std::fs::write(&doc, "x").unwrap();
    {
        let mut permissions = lock(&fixture.host.permissions);
        permissions.windows_commands = false;
        permissions.folders = vec![fixture._dir.path().into()];
    }
    let mut client = fixture.legacy().await;
    let opened = |path: &std::path::Path| {
        tools::open::OPENED
            .lock()
            .unwrap()
            .iter()
            .any(|o| matches!(o, tools::open::Opening::Path(p) if p == path))
    };
    // 只允许了文件夹：不能打开（打开会启动程序）
    let denied = client.call("open_path", json!({"path":doc})).await;
    assert_eq!(
        denied["result"]["structuredContent"]["code"],
        "mcp.commands_denied"
    );
    {
        let mut permissions = lock(&fixture.host.permissions);
        permissions.windows_commands = true;
        permissions.approval = Approval::AskAll;
    }
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    let result = client.call("open_path", json!({"path":doc})).await;
    assert!(error_text(&result).contains("用户拒绝执行"));
    assert!(!opened(&doc));
    assert!(lock(&fixture.host.dangers)[0].starts_with("用默认程序打开："));
    *lock(&fixture.host.action_decision) = Confirmation::Allow;
    let result = client.call("open_path", json!({"path":doc})).await;
    assert_eq!(result["result"]["isError"], false, "{result}");
    assert!(opened(&doc));
    // 程序类文件、两个参数都给：不问就报错
    let script = fixture._dir.path().join("a.ps1");
    std::fs::write(&script, "x").unwrap();
    let refused = client.call("open_path", json!({"path":script})).await;
    assert_eq!(
        refused["result"]["structuredContent"]["code"],
        "mcp.open_refused"
    );
    let both = client
        .call("open_path", json!({"path":doc,"url":"https://example.com"}))
        .await;
    assert_eq!(
        both["error"]["data"]["params"]["reason_key"],
        "mcp.give_either_path_or_url"
    );
    assert_eq!(lock(&fixture.host.dangers).len(), 2);
    client.close().await;
}

#[tokio::test]
async fn 图片以原生内容返回且文本元数据不含编码正文() {
    let fixture = Fixture::new();
    lock(&fixture.host.permissions).folders = vec![fixture._dir.path().into()];
    let path = fixture._dir.path().join("image.png");
    std::fs::write(&path, b"\x89PNG\r\n\x1a\n").unwrap();
    let mut client = fixture.legacy().await;
    let result = client.call("read_file", json!({"file_path":path})).await;
    let content = result["result"]["content"].as_array().unwrap();
    let image = content.iter().find(|c| c["type"] == "image").unwrap();
    assert_eq!(image["mimeType"], "image/png");
    assert_eq!(image["data"], "iVBORw0KGgo=");
    assert!(result["result"]["structuredContent"].get("image").is_none());
    assert!(
        content
            .iter()
            .filter(|c| c["type"] == "text")
            .all(|c| !c.to_string().contains("iVBORw0KGgo="))
    );
    client.close().await;
}

#[tokio::test]
async fn 一次读多个文件_单个失败不影响别的_图片作为图片内容附在后面() {
    let fixture = Fixture::new();
    lock(&fixture.host.permissions).folders = vec![fixture._dir.path().into()];
    let text = fixture._dir.path().join("a.txt");
    std::fs::write(&text, "第一行\n第二行\n").unwrap();
    let image = fixture._dir.path().join("b.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
    let missing = fixture._dir.path().join("missing.txt");
    let mut client = fixture.legacy().await;
    let result = client
        .call(
            "read_files",
            json!({"file_paths":[text, missing, image],"limit":1}),
        )
        .await;
    let files = result["result"]["structuredContent"]["files"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(files.len(), 3, "{result}");
    assert_eq!(files[0]["file_path"], json!(text));
    assert_eq!(files[0]["content"], "     1\t第一行\n");
    assert_eq!(files[1]["error"]["code"], "mcp.not_found");
    assert_eq!(files[2]["image"], 0);
    let content = result["result"]["content"].as_array().unwrap();
    let images: Vec<_> = content.iter().filter(|c| c["type"] == "image").collect();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0]["data"], "iVBORw0KGgo=");
    assert!(
        !result["result"]["structuredContent"]
            .to_string()
            .contains("iVBORw0KGgo=")
    );
    assert!(
        fixture.service.activity(1)[0]
            .summary
            .contains("missing.txt")
    );
    let empty = client.call("read_files", json!({"file_paths":[]})).await;
    assert_eq!(
        empty["error"]["data"]["params"]["reason_key"],
        "mcp.file_paths_must_have_1_to_20_items"
    );
    client.close().await;
}

#[tokio::test]
async fn 一次读多个文件的文字超过总量时这个和后面的不读() {
    let fixture = Fixture::new();
    lock(&fixture.host.permissions).folders = vec![fixture._dir.path().into()];
    let small = fixture._dir.path().join("small.txt");
    std::fs::write(&small, "一行\n").unwrap();
    let big = fixture._dir.path().join("big.txt");
    std::fs::write(&big, format!("{}\n", "x".repeat(1800)).repeat(5000)).unwrap();
    let mut client = fixture.legacy().await;
    let result = client
        .call(
            "read_files",
            json!({"file_paths":[small, big, small],"limit":5000}),
        )
        .await;
    let files = result["result"]["structuredContent"]["files"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(files[0]["content"], "     1\t一行\n");
    for file in &files[1..] {
        assert_eq!(
            file["error"]["params"]["reason_key"],
            "mcp.read_files_total_limit"
        );
    }
    client.close().await;
}

#[tokio::test]
async fn pierlink_status_带版本和电脑名_不用额外权限() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    let result = client.call("pierlink_status", json!({})).await;
    let value = &result["result"]["structuredContent"];
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"), "{result}");
    assert!(value.get("computer").is_some());
    client.close().await;
}

#[tokio::test]
async fn 远程命令超时提示进程可能仍在运行() {
    let fixture = Fixture::new();
    lock(&fixture.host.output).timed_out = true;
    let mut client = fixture.legacy().await;
    let value = client
        .call(
            "run_command",
            json!({"target":"server-id","command":"slow"}),
        )
        .await;
    assert_eq!(
        value["result"]["structuredContent"]["note"],
        "远端进程可能仍在运行，可以打开终端查看"
    );
    client.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn 宿主可查看并停止不同入口的后台进程树且接收通知() {
    let fixture = Fixture::new();
    fixture
        .service
        .add_secret(zeroize::Zeroizing::new("hidden-secret".into()));
    let mut client = fixture.client(Entry::Server {
        server_id: "server-id".into(),
    });
    client.initialize("2025-11-25").await;
    let before = audit::now_ms();
    let pid = fixture._dir.path().join("child.pid");
    let command = format!(
        "printf hidden-secret; sleep 90 & echo $! > {}; wait",
        crate::ssh::quote::sh_quote(pid.to_str().unwrap()).unwrap()
    );
    let started = client
        .call(
            "run_command",
            json!({"target":"windows","command":command,"background":true}),
        )
        .await;
    let id = started["result"]["structuredContent"]["job_id"]
        .as_str()
        .unwrap();
    let began = Instant::now();
    loop {
        if pid.exists()
            && lock(&fixture.host.events)
                .iter()
                .any(|e| matches!(e, HostEvent::JobsChanged))
        {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let jobs = fixture.service.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, id);
    assert_eq!(jobs[0].entry, "server-id");
    assert_eq!(jobs[0].target, "windows");
    assert!(jobs[0].started_ms >= before);
    assert!(jobs[0].running);
    assert!(!jobs[0].command.contains("hidden-secret"));
    assert!(jobs[0].command.contains("***"));
    assert!(!fixture.service.stop_job("missing"));
    assert!(fixture.service.stop_job(id));
    let began = Instant::now();
    loop {
        if !fixture.service.jobs()[0].running
            && lock(&fixture.host.events)
                .iter()
                .filter(|e| matches!(e, HostEvent::JobsChanged))
                .count()
                >= 2
        {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(6));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let child = std::fs::read_to_string(pid).unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.trim()));
    assert!(stat.is_err() || stat.unwrap().split_whitespace().nth(2) == Some("Z"));
    assert!(fixture.service.stop_job(id));
    client.close().await;
}

#[test]
fn 远程执行取消错误也带进程清理提示() {
    let fixture = Fixture::new();
    fixture.host.block.store(true, Ordering::Release);
    let session = session::Session::new(Entry::Windows, true);
    let cancelled = Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let started = Instant::now();
            while fixture.host.started.load(Ordering::Acquire) == 0 {
                assert!(started.elapsed() < Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(5));
            }
            cancelled.store(true, Ordering::Release);
        });
        let args = json!({"target":"server-id","command":"block"});
        let result = tools::execute(
            &fixture.service.shared,
            &session,
            "run_command",
            args.as_object().unwrap(),
            cancelled.clone(),
            &mut "",
        );
        match result {
            Err(tools::Failure::Tool(error)) => {
                assert_eq!(error.code, "mcp.cancelled");
                assert_eq!(
                    error.params["note"],
                    "远端进程可能仍在运行，可以打开终端查看"
                );
            }
            _ => panic!("取消必须返回带提示的工具错误"),
        }
    });
}
