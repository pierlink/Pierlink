//! 几台电脑同时接入：判断规则、ID 前缀、伙伴文件格式、服务器脚本，以及两个服务之间的真实转发。
use super::*;
use crate::{
    error::AppError,
    mcp::{
        Approval, Confirmation, Entry, HostEvent, McpHost, McpService, Permissions, RunLimits,
        RunOutput, SessionInfo, SyncTaskState, Target, TargetKind, TerminalOpenRequest, lock,
    },
    terminal::TerminalManager,
};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const A: &str = "AAAAAAAAaaaaaaaaaaaa";
const B: &str = "BBBBBBBBbbbbbbbbbbbb";
const C: &str = "CCCCCCCCcccccccccccc";
/// B 入口的令牌（和真的入口令牌一样 43 个字符）
const TOKEN_B: &str = "tokenBtokenBtokenBtokenBtokenBtokenBtoken-b";
const TOKEN_A: &str = "tokenAtokenAtokenAtokenAtokenAtokenAtoken-a";
/// 测试用服务器在几台电脑上都一样的标识
const SERVER_KEY: &str = "srv-0123456789ab";

/// B 显示的电脑名：Windows 上两个服务都在这台测试机上，伙伴接口回报的是真实的电脑名（COMPUTERNAME），
/// 别的系统没有电脑名，用伙伴文件里写的
fn name_b() -> String {
    crate::mcp::tools::local::computer_name().unwrap_or_else(|| "电脑B".into())
}

/// A 转来时 B 记的“由谁转来”：有电脑名用电脑名，没有用 A 的编号
fn origin_a() -> String {
    crate::mcp::tools::local::computer_name().unwrap_or_else(|| "AAAAAAAA".into())
}

fn peer(instance: &str, online: bool, idle: Option<u64>, pin: Option<PinRecord>) -> PeerState {
    PeerState {
        owner: short_id(instance).unwrap(),
        record: PeerRecord {
            instance: instance.into(),
            computer: format!("电脑{}", &instance[..1]),
            port: 47822,
            token: Zeroizing::new("t".into()),
            written_at: 0,
            pin,
        },
        online,
        idle_ms: idle,
        local_port: Some(1),
    }
}

fn pin(target: Option<&str>, set_at: u64) -> PinRecord {
    PinRecord {
        target: target.map(str::to_owned),
        computer: target
            .map(|t| format!("电脑{}", &t[..1]))
            .unwrap_or_default(),
        set_at,
    }
}

#[test]
fn 指定优先_指定的电脑不在线就报出来不改到别的电脑() {
    let peers = vec![peer(B, true, Some(1000), None)];
    assert_eq!(
        decide(
            A,
            Some(0),
            &peers,
            Some(&pin(Some(B), 5)),
            &["AAAAAAAA-x".into()]
        ),
        (Current::Peer(0), Basis::Pin)
    );
    assert_eq!(
        decide(A, Some(0), &peers, Some(&pin(Some(C), 5)), &[]).0,
        Current::PinnedOffline("电脑C".into())
    );
    // 有伙伴文件但问不通：同样报出来
    let offline = vec![peer(B, false, None, None)];
    assert_eq!(
        decide(A, Some(0), &offline, Some(&pin(Some(B), 5)), &[]).0,
        Current::PinnedOffline("电脑B".into())
    );
    assert_eq!(
        decide(
            A,
            None,
            &peers,
            Some(&pin(Some(A), 5)),
            &["BBBBBBBB-x".into()]
        ),
        (Current::Here, Basis::Pin)
    );
    // 取消指定（target 为空）就是自动判断
    assert_eq!(
        decide(
            A,
            None,
            &peers,
            Some(&pin(None, 9)),
            &["BBBBBBBB-1234".into()]
        ),
        (Current::Peer(0), Basis::Marker)
    );
}

#[test]
fn 几台电脑各自指定过时最新的生效() {
    let peers = vec![peer(B, true, None, Some(pin(Some(B), 1)))];
    assert_eq!(
        effective_pin(Some(&pin(None, 2)), &peers),
        Some(pin(None, 2))
    );
    assert_eq!(effective_pin(None, &peers), Some(pin(Some(B), 1)));
    assert_eq!(
        effective_pin(Some(&pin(Some(A), 0)), &peers),
        Some(pin(Some(B), 1))
    );
    assert_eq!(effective_pin(None, &[]), None);
}

#[test]
fn 名牌认得出且只有一台才算_认不出按最近操作_都不行就是自己() {
    let peers = vec![
        peer(B, true, Some(5_000), None),
        peer(C, true, Some(1_000), None),
    ];
    assert_eq!(
        decide(A, Some(0), &peers, None, &["BBBBBBBB-0123".into()]),
        (Current::Peer(0), Basis::Marker)
    );
    assert_eq!(
        decide(A, Some(999_999), &peers, None, &["AAAAAAAA-0123".into()]),
        (Current::Here, Basis::Marker)
    );
    // 两台电脑的名牌都有：按最近操作（C 一秒前）
    assert_eq!(
        decide(
            A,
            Some(60_000),
            &peers,
            None,
            &["BBBBBBBB-1".into(), "AAAAAAAA-2".into()]
        ),
        (Current::Peer(1), Basis::Idle)
    );
    // 同一台电脑的两个名牌不算有歧义
    assert_eq!(
        decide(
            A,
            Some(0),
            &peers,
            None,
            &["BBBBBBBB-1".into(), "BBBBBBBB-2".into()]
        ),
        (Current::Peer(0), Basis::Marker)
    );
    // 旧格式的名牌、不在线电脑的名牌、不在线电脑的操作时间都不算
    let offline = vec![peer(B, false, Some(1), None)];
    assert_eq!(
        decide(
            A,
            Some(2_000),
            &offline,
            None,
            &[
                "0123456789abcdef0123456789abcdef".into(),
                "BBBBBBBB-1".into()
            ]
        ),
        (Current::Here, Basis::Idle)
    );
    // 都很久没操作：也选最近用过的那台，不回落到“自己”
    let idle = vec![peer(B, true, Some(3 * 60 * 60 * 1000), None)];
    assert_eq!(
        decide(A, Some(5 * 60 * 60 * 1000), &idle, None, &[]),
        (Current::Peer(0), Basis::Idle)
    );
    // 谁都不知道多久没操作：这台自己
    let unknown = vec![peer(B, true, None, None)];
    assert_eq!(
        decide(A, None, &unknown, None, &[]),
        (Current::Here, Basis::Default)
    );
    // 一样久时先算自己
    let same = vec![peer(B, true, Some(100), None)];
    assert_eq!(
        decide(A, Some(100), &same, None, &[]),
        (Current::Here, Basis::Idle)
    );
}

#[test]
fn 伙伴文件只认合规的字段() {
    let good = json!({"instance": B, "computer": "电脑\u{7}B", "port": 47822, "token": TOKEN_B,
        "written_at": 7, "pin": {"target": A, "computer": "电脑A", "set_at": 3}})
    .to_string();
    let record = parse_record(&good).unwrap();
    assert_eq!(record.computer, "电脑B");
    assert_eq!(record.port, 47822);
    assert_eq!(record.token.as_str(), TOKEN_B);
    assert_eq!(record.pin.unwrap().target.as_deref(), Some(A));
    for bad in [
        json!({"instance": "短", "port": 1, "token": "t"}),
        json!({"instance": "BBBBBBBB bad", "port": 1, "token": "t"}),
        json!({"instance": B, "port": 0, "token": "t"}),
        json!({"instance": B, "port": 70000, "token": "t"}),
        json!({"instance": B, "port": 1, "token": "a b"}),
        json!({"instance": B, "port": 1, "token": "x\r\nHost: evil"}),
        json!({"instance": B, "port": 1}),
        // 太短的令牌当伪造（免得被登记成要遮蔽的秘密）
        json!({"instance": B, "port": 1, "token": "e"}),
        json!({"instance": B, "port": 1, "token": "x".repeat(31)}),
    ] {
        assert!(parse_record(&bad.to_string()).is_none(), "{bad}");
    }
    assert!(parse_record("{不是 JSON").is_none());
    // 格式不对的指定当没有指定
    let odd =
        json!({"instance": B, "port": 1, "token": TOKEN_B, "pin": {"target": "x y", "set_at": 1}})
            .to_string();
    assert!(parse_record(&odd).unwrap().pin.is_none());
}

#[test]
fn 别的电脑的终端和任务_id_加前缀并能拆回() {
    let mut value = json!({"terminal_id": "t1", "list": [{"job_id": "j1"}, {"job_id": "pc:CCCCCCCC/j2"}], "other": "x"});
    prefix_ids(&mut value, "BBBBBBBB");
    assert_eq!(value["terminal_id"], "pc:BBBBBBBB/t1");
    assert_eq!(value["list"][0]["job_id"], "pc:BBBBBBBB/j1");
    assert_eq!(value["list"][1]["job_id"], "pc:CCCCCCCC/j2");
    assert_eq!(value["other"], "x");
    assert_eq!(split_id("pc:BBBBBBBB/t1"), Some(("BBBBBBBB", "t1")));
    assert_eq!(split_id("t1"), None);
    assert_eq!(split_id("pc:short/t1"), None);
    assert_eq!(split_id("pc:BBBBBBBB/"), None);
    assert_eq!(marker_owner("BBBBBBBB-0123abcd"), Some("BBBBBBBB"));
    assert_eq!(marker_owner("0123456789abcdef0123456789abcdef"), None);
    assert_eq!(marker_owner("BBBB-0123"), None);
    assert_eq!(short_id("ab-cd_ef12gh34ij"), Some("abcdef12".into()));
    assert_eq!(short_id("abc"), None);
}

#[cfg(unix)]
mod shell {
    use super::*;
    use std::{
        io::Write,
        os::unix::fs::PermissionsExt,
        path::Path,
        process::{Command, Stdio},
    };

    fn run(home: &Path, script: &str) -> std::process::Output {
        let mut child = Command::new("/bin/sh")
            .arg("-s")
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn json(home: &Path, script: &str) -> Value {
        let output = run(home, script);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("不是 JSON：{}", String::from_utf8_lossy(&output.stdout)))
    }

    #[test]
    fn 伙伴文件写删和读回() {
        let home = crate::test_support::tempdir();
        let pinned = pin(Some(A), 42);
        let written = json(
            home.path(),
            &peer_file_script(A, "电脑 \"A\" \\ 办公室", 47821, TOKEN_A, 5, Some(&pinned)),
        );
        assert_eq!(written, json!({"ok": true}));
        json(
            home.path(),
            &peer_file_script(B, "电脑B", 47822, TOKEN_B, 6, None),
        );
        let dir = home.path().join(".pierlink/peers");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(format!("{A}.json"))), 0o600);
        // 不相干的文件、链接不读
        std::fs::write(dir.join("readme.txt"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.join("link.json")).unwrap();
        let found = json(home.path(), &discover_script(None));
        let mut records: Vec<PeerRecord> = found["peers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| parse_record(v.as_str().unwrap()))
            .collect();
        records.sort_by(|x, y| x.instance.cmp(&y.instance));
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].computer, "电脑 \"A\" \\ 办公室");
        assert_eq!(records[0].token.as_str(), TOKEN_A);
        assert_eq!(records[0].pin, Some(pinned));
        assert_eq!(records[1].port, 47822);
        assert_eq!(found["markers"], json!([]));
        assert_eq!(
            json(home.path(), &remove_peer_file_script(A)),
            json!({"ok": true})
        );
        let found = json(home.path(), &discover_script(None));
        assert_eq!(found["peers"].as_array().unwrap().len(), 1);
        // 没有文件也算删成功；实例号不合规直接失败
        assert_eq!(
            json(home.path(), &remove_peer_file_script(A)),
            json!({"ok": true})
        );
        assert!(
            !run(home.path(), &remove_peer_file_script("../x"))
                .status
                .success()
        );
        assert!(
            !run(home.path(), &peer_file_script("a b", "x", 1, "t", 0, None))
                .status
                .success()
        );
    }

    #[test]
    fn 伙伴目录是链接时不读不写() {
        let home = crate::test_support::tempdir();
        let elsewhere = crate::test_support::tempdir();
        std::fs::create_dir_all(home.path().join(".pierlink")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), home.path().join(".pierlink/peers")).unwrap();
        assert!(
            !run(home.path(), &peer_file_script(A, "x", 1, "t", 0, None))
                .status
                .success()
        );
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
        std::fs::write(elsewhere.path().join(format!("{B}.json")), "{}").unwrap();
        let found = json(home.path(), &discover_script(None));
        assert_eq!(found["peers"], json!([]));
    }

    #[test]
    fn 只读两小时内写过且文件名正常的伙伴文件_坏字节不影响读别的() {
        let home = crate::test_support::tempdir();
        json(
            home.path(),
            &peer_file_script(B, "电脑B", 47822, TOKEN_B, 6, None),
        );
        let dir = home.path().join(".pierlink/peers");
        // 三小时前写的（那台电脑没删掉就关了）
        let old = dir.join(format!("{C}.json"));
        std::fs::write(
            &old,
            json!({"instance": C, "computer": "旧", "port": 1, "token": TOKEN_A}).to_string(),
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(3 * 60 * 60))
            .unwrap();
        // 文件名里有空格、内容里有坏的 UTF-8 字节
        std::fs::write(dir.join("bad name.json"), "{}").unwrap();
        let mut junk = b"{\"instance\":\"x\xff\xfe".to_vec();
        junk.extend_from_slice(b"\"}");
        std::fs::write(dir.join("junk.json"), junk).unwrap();
        let found = json(home.path(), &discover_script(None));
        let records: Vec<PeerRecord> = found["peers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| parse_record(v.as_str().unwrap()))
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].instance, B);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn 找名牌_连着入口端口的进程用它自己的名牌() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(format!(
                "import socket,time\ns=socket.create_connection(('127.0.0.1',{port}))\ntime.sleep(30)"
            ))
            .env("LC_PIERLINK_TERM", "AAAAAAAA-0123456789abcdef")
            .env_remove("STY")
            .env_remove("TMUX")
            .spawn()
            .unwrap();
        let (_connection, _) = listener.accept().unwrap();
        let home = crate::test_support::tempdir();
        let found = json(home.path(), &discover_script(Some(port)));
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(found["markers"], json!(["AAAAAAAA-0123456789abcdef"]));
        assert_eq!(found["peers"], json!([]));
    }
}

/// 只做转发测试要用的事的宿主：一台服务器目标，命令只记下来
struct PeerHost {
    name: &'static str,
    idle: Option<u64>,
    /// 这台电脑给测试服务器起的名字（几台电脑上可以不一样）
    server: Mutex<String>,
    /// 新连接先请用户确认
    confirm: AtomicBool,
    connects: AtomicUsize,
    /// 服务器上读到的伙伴文件和名牌
    discovery: Mutex<Value>,
    /// 服务器上的端口 → 本机转发端口
    tunnels: Mutex<HashMap<u16, u16>>,
    pin: Mutex<Option<PinRecord>>,
    commands: Mutex<Vec<String>>,
    scripts: AtomicUsize,
    confirmations: AtomicUsize,
    /// 命令一直不结束，直到被取消
    block: AtomicBool,
    cancelled: AtomicUsize,
}

impl McpHost for PeerHost {
    fn permissions(&self, _entry: &Entry) -> Result<Option<Permissions>, AppError> {
        Ok(Some(Permissions {
            approval: Approval::AskAll,
            windows_commands: false,
            folders: vec![],
            servers: vec!["srv".into()],
        }))
    }
    fn targets(&self, _entry: &Entry, permissions: &Permissions) -> Result<Vec<Target>, AppError> {
        let server = lock(&self.server).clone();
        Ok(permissions
            .servers
            .iter()
            .map(|id| Target {
                target: server.clone(),
                id: id.clone(),
                name: server.clone(),
                kind: TargetKind::Server,
                online: Some(true),
                engine: Some("builtin".into()),
                address: None,
                key: Some(SERVER_KEY.into()),
            })
            .collect())
    }
    fn confirm_connections(&self, _entry: &Entry) -> bool {
        self.confirm.load(Ordering::Acquire)
    }
    fn confirm_connect(
        &self,
        _session: &SessionInfo,
        _targets: &[Target],
        _cancelled: &AtomicBool,
    ) -> Result<Confirmation, AppError> {
        self.connects.fetch_add(1, Ordering::AcqRel);
        Ok(Confirmation::Allow)
    }
    fn confirm_action(
        &self,
        _session: &SessionInfo,
        _target: &Target,
        _action: &str,
        _dangerous: bool,
        _cancelled: &AtomicBool,
    ) -> Result<Confirmation, AppError> {
        self.confirmations.fetch_add(1, Ordering::AcqRel);
        Ok(Confirmation::Allow)
    }
    fn run_server_command(
        &self,
        server_id: &str,
        command: &str,
        _cwd: Option<&str>,
        _limits: RunLimits,
        cancelled: Arc<AtomicBool>,
    ) -> Result<RunOutput, AppError> {
        lock(&self.commands).push(format!("{server_id}:{command}"));
        while self.block.load(Ordering::Acquire) {
            if cancelled.load(Ordering::Acquire) {
                self.cancelled.fetch_add(1, Ordering::AcqRel);
                return Err(AppError::new("mcp.cancelled"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(RunOutput {
            exit_code: Some(0),
            stdout: self.name.as_bytes().to_vec(),
            ..Default::default()
        })
    }
    fn open_terminal(
        &self,
        _request: TerminalOpenRequest,
        _cancelled: &AtomicBool,
    ) -> Result<String, AppError> {
        Err(AppError::new("mcp.ui_timeout"))
    }
    fn sync_status(&self, _task: Option<&str>) -> Result<Vec<SyncTaskState>, AppError> {
        Ok(vec![])
    }
    fn sync_rescan(&self, _folder_id: &str, _timeout: Duration) -> Result<(), AppError> {
        Ok(())
    }
    fn open_sftp(&self, _server_id: &str) -> Result<crate::sftp::SftpSession, AppError> {
        Err(AppError::new("mcp.peer_unreachable"))
    }
    fn upload_image(
        &self,
        _server_id: &str,
        _bytes: &[u8],
        _extension: &str,
    ) -> Result<String, AppError> {
        Err(AppError::new("mcp.peer_unreachable"))
    }
    fn notify(&self, _event: HostEvent) {}
    fn entry_port(&self, _server_id: &str) -> Option<u16> {
        Some(47821)
    }
    fn entry_script(
        &self,
        _server_id: &str,
        _script: &str,
        _timeout: Duration,
    ) -> Result<Value, AppError> {
        self.scripts.fetch_add(1, Ordering::AcqRel);
        Ok(lock(&self.discovery).clone())
    }
    fn peer_tunnel(&self, _server_id: &str, port: u16) -> Result<u16, AppError> {
        lock(&self.tunnels)
            .get(&port)
            .copied()
            .ok_or_else(|| AppError::new("mcp.peer_unreachable"))
    }
    fn idle_ms(&self) -> Option<u64> {
        self.idle
    }
    fn pin(&self) -> Option<PinRecord> {
        lock(&self.pin).clone()
    }
}

struct Pc {
    service: McpService,
    host: Arc<PeerHost>,
    _dir: tempfile::TempDir,
}

fn pc(name: &'static str, instance: &str, idle: Option<u64>) -> Pc {
    let dir = crate::test_support::tempdir();
    let terminals = TerminalManager::new();
    let host = Arc::new(PeerHost {
        name,
        idle,
        server: Mutex::new("服务器".into()),
        confirm: AtomicBool::new(false),
        connects: AtomicUsize::new(0),
        discovery: Mutex::new(json!({"peers": [], "markers": []})),
        tunnels: Mutex::new(HashMap::new()),
        pin: Mutex::new(None),
        commands: Mutex::new(Vec::new()),
        scripts: AtomicUsize::new(0),
        confirmations: AtomicUsize::new(0),
        block: AtomicBool::new(false),
        cancelled: AtomicUsize::new(0),
    });
    let service = McpService::new(host.clone(), terminals, dir.path().into()).unwrap();
    service.set_instance(instance);
    Pc {
        service,
        host,
        _dir: dir,
    }
}

/// A 是 AI 连着的入口；B 开着自己的入口（服务器端口 47999），A 的服务器上有 B 的伙伴文件
fn pair(idle_a: Option<u64>, idle_b: Option<u64>) -> (Pc, Pc, u16) {
    let a = pc("电脑A", A, idle_a);
    let b = pc("电脑B", B, idle_b);
    let port = b
        .service
        .start_http_entry("srv", Zeroizing::new(TOKEN_B.into()))
        .unwrap()
        .local_port;
    let record = json!({"instance": B, "computer": "电脑B", "port": 47999, "token": TOKEN_B, "written_at": 1})
        .to_string();
    *lock(&a.host.discovery) = json!({"peers": [record], "markers": []});
    lock(&a.host.tunnels).insert(47999, port);
    (a, b, port)
}

/// 经字节流接入 A 的服务器入口的 AI 客户端
struct Ai {
    read: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    _handle: crate::mcp::SessionHandle,
    id: u64,
}

impl Ai {
    async fn connect(service: &McpService) -> Self {
        let (server, client) = tokio::io::duplex(1 << 20);
        let (read, write) = tokio::io::split(server);
        let handle = service
            .serve_stream(
                Entry::Server {
                    server_id: "srv".into(),
                },
                read,
                write,
            )
            .unwrap();
        let (read, write) = tokio::io::split(client);
        let mut ai = Self {
            read: BufReader::new(read),
            write,
            _handle: handle,
            id: 0,
        };
        ai.request("initialize", json!({"protocolVersion":"2025-11-25","clientInfo":{"name":"测试 AI","version":"1"},"capabilities":{}}))
            .await;
        ai.write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        ai
    }
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let line = json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params});
        self.write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let mut text = String::new();
            tokio::time::timeout(Duration::from_secs(30), self.read.read_line(&mut text))
                .await
                .unwrap()
                .unwrap();
            let value: Value = serde_json::from_str(&text).unwrap();
            if value["id"] == self.id {
                return value;
            }
        }
    }
    async fn call(&mut self, name: &str, args: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": args}))
            .await["result"]
            .clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn 用户在另一台电脑前时整次转过去执行并记下转给了谁() {
    let (a, b, _) = pair(Some(300_000), Some(1_000));
    let mut ai = Ai::connect(&a.service).await;
    let result = ai
        .call(
            "run_command",
            json!({"target": "服务器", "command": "echo hi"}),
        )
        .await;
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["structuredContent"]["computer"], json!(name_b()));
    assert_eq!(result["structuredContent"]["stdout"], "电脑B");
    // 文字版本和结构化结果一致
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, result["structuredContent"]);
    assert_eq!(*lock(&b.host.commands), vec!["srv:echo hi"]);
    assert!(lock(&a.host.commands).is_empty());
    // 确认框只在执行的那台弹；B 收到的调用不会再转（它一次都没去服务器上查）
    assert_eq!(b.host.confirmations.load(Ordering::Acquire), 1);
    assert_eq!(a.host.confirmations.load(Ordering::Acquire), 0);
    assert_eq!(b.host.scripts.load(Ordering::Acquire), 0);
    let mine = &a.service.activity(5)[0];
    assert_eq!(mine.forwarded_to, Some(name_b()));
    assert_eq!(mine.outcome, "ok");
    let theirs = &b.service.activity(5)[0];
    assert_eq!(theirs.forwarded_from, Some(origin_a()));
    assert_eq!(theirs.client.name, "测试 AI");
    // 判断结果缓存着：第二次调用不再去服务器上查
    ai.call(
        "run_command",
        json!({"target": "服务器", "command": "echo 2"}),
    )
    .await;
    assert_eq!(a.host.scripts.load(Ordering::Acquire), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn 只有一台电脑或用户就在这台时照常在本机执行() {
    let (a, b, _) = pair(Some(1_000), Some(300_000));
    let mut ai = Ai::connect(&a.service).await;
    let result = ai
        .call(
            "run_command",
            json!({"target": "服务器", "command": "echo here"}),
        )
        .await;
    assert_eq!(result["structuredContent"]["stdout"], "电脑A");
    assert!(result["structuredContent"]["computer"].is_null());
    assert!(lock(&b.host.commands).is_empty());
    let alone = pc("电脑A", A, None);
    let mut ai = Ai::connect(&alone.service).await;
    let result = ai
        .call(
            "run_command",
            json!({"target": "服务器", "command": "echo alone"}),
        )
        .await;
    assert_eq!(result["structuredContent"]["stdout"], "电脑A");
    assert_eq!(alone.service.activity(1)[0].forwarded_to, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn 指定的电脑不在线时报错不执行() {
    let (a, b, _) = pair(Some(1_000), Some(1_000));
    *lock(&a.host.pin) = Some(pin(Some(C), 9));
    let mut ai = Ai::connect(&a.service).await;
    let result = ai
        .call(
            "run_command",
            json!({"target": "服务器", "command": "echo x"}),
        )
        .await;
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["code"],
        "mcp.pinned_computer_offline"
    );
    assert_eq!(result["structuredContent"]["params"]["computer"], "电脑C");
    assert!(lock(&a.host.commands).is_empty() && lock(&b.host.commands).is_empty());
    // 指定 B（在别的电脑上指定的也算）：转给 B
    *lock(&a.host.pin) = None;
    let record = json!({"instance": B, "computer": "电脑B", "port": 47999, "token": TOKEN_B,
        "written_at": 1, "pin": {"target": B, "computer": "电脑B", "set_at": 10}})
    .to_string();
    *lock(&a.host.discovery) = json!({"peers": [record], "markers": ["AAAAAAAA-1"]});
    a.service.shared.router.forget("srv");
    let result = ai
        .call(
            "run_command",
            json!({"target": "服务器", "command": "echo b"}),
        )
        .await;
    assert_eq!(result["structuredContent"]["computer"], json!(name_b()));
    assert_eq!(*lock(&b.host.commands), vec!["srv:echo b"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn 带电脑前缀的_id_转给那台_那台不在线就报出来() {
    let (a, b, _) = pair(Some(1_000), Some(300_000));
    let mut ai = Ai::connect(&a.service).await;
    let result = ai
        .call("terminal_read", json!({"terminal_id": "pc:CCCCCCCC/t1"}))
        .await;
    assert_eq!(result["structuredContent"]["code"], "mcp.peer_offline");
    let reply = ai
        .request(
            "tools/call",
            json!({"name": "terminal_read", "arguments": {"terminal_id": "pc:BBBBBBBB/t1"}}),
        )
        .await;
    // B 上没有这个终端：错误（参数错误）是 B 回的，转发记录写着 B
    assert!(reply["error"].is_object(), "{reply}");
    assert_eq!(a.service.activity(1)[0].forwarded_to, Some(name_b()));
    assert_eq!(b.service.activity(1)[0].tool, "terminal_read");
    // 自己的前缀：去掉后在本机执行
    let reply = ai
        .request(
            "tools/call",
            json!({"name": "terminal_read", "arguments": {"terminal_id": "pc:AAAAAAAA/t1"}}),
        )
        .await;
    assert!(reply["error"].is_object(), "{reply}");
    assert_eq!(a.service.activity(1)[0].forwarded_to, None);
    // 列后台任务：本机的和 B 的合在一起（都没有）
    let result = ai.call("list_jobs", json!({})).await;
    assert_eq!(result["structuredContent"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_断开时转过去的调用也取消() {
    let (a, b, _) = pair(Some(300_000), Some(1_000));
    b.host.block.store(true, Ordering::Release);
    let mut ai = Ai::connect(&a.service).await;
    ai.id += 1;
    let line = json!({"jsonrpc":"2.0","id":ai.id,"method":"tools/call","params":{"name":"run_command","arguments":{"target":"服务器","command":"sleep"}}});
    ai.write
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap();
    let started = std::time::Instant::now();
    while lock(&b.host.commands).is_empty() {
        assert!(started.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(ai);
    while b.host.cancelled.load(Ordering::Acquire) == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "B 上的命令没有被取消"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn 转发接口要令牌和发起方_不收转给自己的() {
    let (_a, _b, port) = pair(None, None);
    let send = |headers: String, body: &'static str| async move {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let request = format!(
            "POST {FORWARD_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut reply = String::new();
        let _ =
            tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut reply)).await;
        reply
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned()
    };
    let call = r#"{"tool":"list_targets","arguments":{}}"#;
    let auth = format!("Authorization: Bearer {TOKEN_B}\r\n");
    assert_eq!(send(String::new(), call).await, "401");
    assert_eq!(send(auth.clone(), call).await, "400");
    assert_eq!(
        send(format!("{auth}{FORWARDED_HEADER}: {B}\r\n"), call).await,
        "400"
    );
    assert_eq!(
        send(format!("{auth}{FORWARDED_HEADER}: {A}\r\n"), "{坏").await,
        "400"
    );
    assert_eq!(
        send(format!("{auth}{FORWARDED_HEADER}: {A}\r\n"), call).await,
        "200"
    );
    // 伙伴接口要令牌
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(
            format!("GET {PEER_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply).await;
    assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn 两台电脑给服务器起的名字不同时按通用标识换成那台的名字() {
    let (a, b, _) = pair(Some(300_000), Some(1_000));
    *lock(&b.host.server) = "另一个名字".into();
    let mut ai = Ai::connect(&a.service).await;
    let result = ai
        .call(
            "run_command",
            json!({"target": "服务器", "command": "echo t"}),
        )
        .await;
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(*lock(&b.host.commands), vec!["srv:echo t"]);
    // 用通用标识（list_targets 给的 id）也行
    let result = ai
        .call(
            "run_command",
            json!({"target": SERVER_KEY, "command": "echo k"}),
        )
        .await;
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(lock(&b.host.commands).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn 列目标时服务器的_id_是几台电脑上都一样的标识() {
    let alone = pc("电脑A", A, None);
    let mut ai = Ai::connect(&alone.service).await;
    let result = ai.call("list_targets", json!({})).await;
    let server = result["structuredContent"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["kind"] == "server")
        .cloned()
        .unwrap();
    assert_eq!(server["id"], SERVER_KEY);
    assert!(server.get("key").is_none());
    let result = ai
        .call(
            "run_command",
            json!({"target": SERVER_KEY, "command": "echo id"}),
        )
        .await;
    assert_eq!(result["isError"], false, "{result}");
    // 这台电脑自己的记录 ID 照样认
    let result = ai
        .call(
            "run_command",
            json!({"target": "srv", "command": "echo db"}),
        )
        .await;
    assert_eq!(result["isError"], false, "{result}");
}

#[tokio::test(flavor = "multi_thread")]
async fn 合并列表时那台还没确认过连接就跳过不弹确认框() {
    let (a, b, _) = pair(Some(1_000), Some(300_000));
    b.host.confirm.store(true, Ordering::Release);
    let mut ai = Ai::connect(&a.service).await;
    let result = ai.call("list_jobs", json!({})).await;
    assert_eq!(result["structuredContent"], json!([]));
    assert_eq!(b.host.connects.load(Ordering::Acquire), 0);
    assert!(b.service.activity(5).is_empty());
}
