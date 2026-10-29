use super::*;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream},
    sync::broadcast,
};

struct MockHost {
    approval: AtomicU8,
    approvals: AtomicUsize,
    approve_ready: tokio::sync::Notify,
    screen: Mutex<String>,
    calls: Mutex<Vec<Value>>,
    audits: Mutex<Vec<AuditRecord>>,
    events: broadcast::Sender<String>,
}
impl Default for MockHost {
    fn default() -> Self {
        Self {
            approval: AtomicU8::new(0),
            approvals: AtomicUsize::new(0),
            approve_ready: tokio::sync::Notify::new(),
            screen: Mutex::new("$ ".into()),
            calls: Mutex::default(),
            audits: Mutex::default(),
            events: broadcast::channel(16).0,
        }
    }
}
impl MobileHost for MockHost {
    fn approve_pairing<'a>(&'a self, name: &'a str, peer: IpAddr) -> HostFuture<'a, bool> {
        Box::pin(async move {
            self.approvals.fetch_add(1, Ordering::SeqCst);
            lock(&self.calls).push(json!(["pair", name, peer.to_string()]));
            match self.approval.load(Ordering::SeqCst) {
                0 => Ok(true),
                1 => Ok(false),
                3 => {
                    self.approve_ready.notified().await;
                    Ok(true)
                }
                _ => std::future::pending().await,
            }
        })
    }
    fn overview(&self) -> HostFuture<'_, Value> {
        Box::pin(async { Ok(json!({"servers":[1],"sync":{"online":true}})) })
    }
    fn terminals(&self) -> HostFuture<'_, Value> {
        Box::pin(async { Ok(json!([{"id":"term","title":"终端"}])) })
    }
    fn terminal_screen<'a>(&'a self, id: &'a str) -> HostFuture<'a, Value> {
        Box::pin(async move { Ok(json!({"id":id,"screen":{"text":lock(&self.screen).clone()}})) })
    }
    fn terminal_text<'a>(&'a self, _id: &'a str) -> HostFuture<'a, String> {
        Box::pin(async { Ok(lock(&self.screen).clone()) })
    }
    fn terminal_input<'a>(&'a self, id: &'a str, input: TerminalInput) -> HostFuture<'a, Value> {
        Box::pin(async move {
            lock(&self.calls).push(json!(["input", id, input]));
            Ok(json!({"ok":true}))
        })
    }
    fn confirms(&self) -> HostFuture<'_, Value> {
        Box::pin(async { Ok(json!([{"id":"ask"}])) })
    }
    fn answer_confirm<'a>(&'a self, id: &'a str, allow: bool) -> HostFuture<'a, Value> {
        Box::pin(async move {
            lock(&self.calls).push(json!(["answer", id, allow]));
            Ok(json!({"ok":true}))
        })
    }
    fn stop_all(&self) -> HostFuture<'_, Value> {
        Box::pin(async {
            lock(&self.calls).push(json!(["stop"]));
            Ok(json!({"ok":true}))
        })
    }
    fn activity(&self, limit: usize) -> HostFuture<'_, Value> {
        Box::pin(async move { Ok(json!([{"limit":limit}])) })
    }
    fn audit(&self, record: AuditRecord) {
        lock(&self.audits).push(record);
    }
    fn subscribe(&self) -> Option<broadcast::Receiver<String>> {
        Some(self.events.subscribe())
    }
}

struct MemoryEntry {
    server: MobileServer,
    host: Arc<MockHost>,
    root: tempfile::TempDir,
}
impl MemoryEntry {
    fn new() -> Self {
        Self::limits(Limits::default())
    }
    fn limits(mut limits: Limits) -> Self {
        limits.failure_delay = Duration::from_millis(2);
        let root = crate::test_support::tempdir();
        let assets = root.path().join("assets");
        std::fs::create_dir(&assets).unwrap();
        std::fs::write(assets.join("index.html"), "<html>手机</html>").unwrap();
        std::fs::write(assets.join("app-a1b2c3d4.js"), "console.log(1)").unwrap();
        std::fs::write(assets.join("style.css"), "body{}").unwrap();
        let db = Arc::new(Mutex::new(
            crate::storage::open(&root.path().join("db")).unwrap(),
        ));
        let host = Arc::new(MockHost::default());
        let mut shared = Shared::new(host.clone(), assets, db).unwrap();
        Arc::get_mut(&mut shared).unwrap().limits = limits;
        Self {
            server: MobileServer {
                shared,
                address: "127.0.0.1:12345".parse().unwrap(),
                listener: None,
            },
            host,
            root,
        }
    }
    fn connect(&self) -> Client {
        self.peer("100.64.0.2".parse().unwrap())
    }
    fn peer(&self, peer: IpAddr) -> Client {
        let (server, client) = tokio::io::duplex(256 * 1024);
        if let Some(connection) = self.server.shared.register() {
            tokio::spawn(http::serve(
                server,
                self.server.shared.clone(),
                connection,
                peer,
            ));
        }
        Client {
            io: BufReader::new(client),
        }
    }
    async fn request(&self, method: &str, path: &str, cookie: &str, body: Value) -> Reply {
        self.connect()
            .request(
                method,
                path,
                &format!("X-Pierlink: 1\r\nCookie: {cookie}\r\n"),
                &body.to_string(),
            )
            .await
    }
    async fn pair_code(&self, code: &str) -> Reply {
        self.request(
            "POST",
            "/api/pair",
            "",
            json!({"code":code,"device_name":"测试手机"}),
        )
        .await
    }
    async fn pair(&self) -> (String, String) {
        let invite = self.server.create_invite().unwrap();
        let reply = self.pair_code(&invite.code).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        let cookie = reply.headers["set-cookie"]
            .split(';')
            .next()
            .unwrap()
            .to_string();
        (reply.json()["id"].as_str().unwrap().into(), cookie)
    }
    fn sql(&self, sql: &str, params: impl rusqlite::Params) {
        rusqlite::Connection::open(self.root.path().join("db"))
            .unwrap()
            .execute(sql, params)
            .unwrap();
    }
}
struct Client {
    io: BufReader<DuplexStream>,
}
struct Reply {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}
impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
}
impl Client {
    async fn send(&mut self, method: &str, path: &str, headers: &str, body: &str) {
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: pc.example.ts.net:12345\r\nContent-Length: {}\r\n{headers}\r\n{body}",
            body.len()
        );
        self.io
            .get_mut()
            .write_all(request.as_bytes())
            .await
            .unwrap();
    }
    async fn headers(&mut self) -> Reply {
        let mut line = String::new();
        self.io.read_line(&mut line).await.unwrap();
        let status = line
            .split_whitespace()
            .nth(1)
            .expect("没有 HTTP 响应")
            .parse()
            .unwrap();
        let mut headers = HashMap::new();
        loop {
            line.clear();
            assert!(self.io.read_line(&mut line).await.unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            let (key, value) = line.split_once(':').unwrap();
            headers.insert(key.to_ascii_lowercase(), value.trim().into());
        }
        Reply {
            status,
            headers,
            body: String::new(),
        }
    }
    async fn chunk(&mut self) -> Vec<u8> {
        let mut size = String::new();
        self.io.read_line(&mut size).await.unwrap();
        let size = usize::from_str_radix(size.trim(), 16).unwrap();
        let mut bytes = vec![0; size];
        self.io.read_exact(&mut bytes).await.unwrap();
        let mut end = [0; 2];
        self.io.read_exact(&mut end).await.unwrap();
        assert_eq!(&end, b"\r\n");
        bytes
    }
    async fn receive(&mut self) -> Reply {
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut reply = self.headers().await;
            let mut bytes = Vec::new();
            if let Some(size) = reply.headers.get("content-length") {
                bytes.resize(size.parse().unwrap(), 0);
                self.io.read_exact(&mut bytes).await.unwrap();
            } else if reply
                .headers
                .get("transfer-encoding")
                .is_some_and(|s| s == "chunked")
            {
                loop {
                    let chunk = self.chunk().await;
                    if chunk.is_empty() {
                        break;
                    }
                    bytes.extend(chunk);
                }
            }
            reply.body = String::from_utf8(bytes).unwrap();
            reply
        })
        .await
        .unwrap()
    }
    async fn request(&mut self, method: &str, path: &str, headers: &str, body: &str) -> Reply {
        self.send(method, path, headers, body).await;
        self.receive().await
    }
    async fn closed(&mut self) {
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), self.io.read_to_end(&mut bytes))
            .await
            .expect("连接没有被关闭")
            .unwrap();
    }
}

#[test]
fn 地址仅接受回环私网和指定组网网段() {
    for ip in [
        "127.0.0.1",
        "127.255.255.254",
        "10.0.0.1",
        "172.16.0.1",
        "172.31.255.254",
        "192.168.0.1",
        "100.64.0.0",
        "100.127.255.255",
        "::1",
        "fd00::1",
        "fd7a:115c:a1e0::1",
        "fdff::1",
    ] {
        assert!(
            validate_bind_address(SocketAddr::new(ip.parse().unwrap(), 0)).is_ok(),
            "{ip}"
        );
    }
    for ip in [
        "0.0.0.0",
        "::",
        "8.8.8.8",
        "172.15.0.1",
        "172.32.0.1",
        "192.169.0.1",
        "100.63.255.255",
        "100.128.0.0",
        "169.254.0.1",
        "224.0.0.1",
        "255.255.255.255",
        "fc00::1",
        "fe80::1",
        "2001:4860:4860::8888",
        "::ffff:127.0.0.1",
        "::ffff:8.8.8.8",
    ] {
        assert_eq!(
            validate_bind_address(SocketAddr::new(ip.parse().unwrap(), 1))
                .unwrap_err()
                .code,
            "mobile.invalid_bind",
            "{ip}"
        );
    }
}

#[tokio::test]
async fn 启动拒绝非法地址或资源目录且停止可重复() {
    let entry = MemoryEntry::new();
    let db = lock(&entry.server.shared.state).db.clone();
    assert!(
        MobileServer::start(
            "0.0.0.0:0".parse().unwrap(),
            entry.host.clone(),
            entry.root.path().into(),
            db.clone()
        )
        .await
        .is_err()
    );
    assert!(
        MobileServer::start(
            "127.0.0.1:0".parse().unwrap(),
            entry.host.clone(),
            entry.root.path().join("missing"),
            db
        )
        .await
        .is_err()
    );
    assert_eq!(entry.server.local_addr().port(), 12345);
    let mut client = entry.connect();
    entry.server.stop();
    entry.server.stop();
    client.closed().await;
    assert_eq!(
        entry.server.create_invite().err().unwrap().code,
        "mobile.stopped"
    );
}

#[tokio::test]
async fn 邀请码替换一次性与十分钟过期() {
    let entry = MemoryEntry::new();
    let first = entry.server.create_invite().unwrap();
    let second = entry.server.create_invite().unwrap();
    assert_eq!(second.code.len(), 8);
    assert!(
        second
            .code
            .bytes()
            .all(|b| b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(&b))
    );
    assert!((599..=600).contains(&(second.expires_at - now())));
    assert_eq!(entry.pair_code(&first.code).await.status, 401);
    assert_eq!(entry.pair_code(&second.code).await.status, 200);
    assert_eq!(entry.pair_code(&second.code).await.status, 401);
    let third = entry.server.create_invite().unwrap();
    auth::expire_invite(&entry.server.shared);
    assert_eq!(entry.pair_code(&third.code).await.status, 401);
    assert_eq!(entry.host.approvals.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn 来源限速与跨来源十次错误作废邀请() {
    let entry = MemoryEntry::new();
    let invite = entry.server.create_invite().unwrap();
    for _ in 0..5 {
        assert_eq!(entry.pair_code("WRONG").await.status, 401);
    }
    assert_eq!(entry.pair_code(&invite.code).await.status, 429);
    for _ in 0..5 {
        let mut client = entry.peer("100.64.0.3".parse().unwrap());
        assert_eq!(
            client
                .request(
                    "POST",
                    "/api/pair",
                    "X-Pierlink: 1\r\n",
                    &json!({"code":"WRONG","device_name":"手机"}).to_string()
                )
                .await
                .status,
            401
        );
    }
    let mut client = entry.peer("100.64.0.4".parse().unwrap());
    assert_eq!(
        client
            .request(
                "POST",
                "/api/pair",
                "X-Pierlink: 1\r\n",
                &json!({"code":invite.code,"device_name":"手机"}).to_string()
            )
            .await
            .status,
        401
    );
    assert_eq!(entry.host.approvals.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn 桌面拒绝与超时都不创建设备且邀请码不能重用() {
    let entry = MemoryEntry::limits(Limits {
        approval: Duration::from_millis(40),
        ..Limits::default()
    });
    for (mode, status) in [(1, 403), (2, 504)] {
        entry.host.approval.store(mode, Ordering::SeqCst);
        let invite = entry.server.create_invite().unwrap();
        assert_eq!(entry.pair_code(&invite.code).await.status, status);
        assert_eq!(entry.pair_code(&invite.code).await.status, 401);
    }
    assert!(entry.server.list_devices().unwrap().is_empty());
}

#[tokio::test]
async fn 配对等待不受普通空闲期限影响且能被作废() {
    let entry = MemoryEntry::limits(Limits {
        idle: Duration::from_millis(15),
        approval: Duration::from_millis(90),
        ..Limits::default()
    });
    entry.host.approval.store(2, Ordering::SeqCst);
    let invite = entry.server.create_invite().unwrap();
    assert_eq!(entry.pair_code(&invite.code).await.status, 504);
    for revoke in [false, true] {
        let invite = entry.server.create_invite().unwrap();
        let mut client = entry.connect();
        client
            .send(
                "POST",
                "/api/pair",
                "X-Pierlink: 1\r\n",
                &json!({"code":invite.code,"device_name":"手机"}).to_string(),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        if revoke {
            entry.server.revoke_all().unwrap();
            client.closed().await;
        } else {
            entry.server.create_invite().unwrap();
            assert_eq!(client.receive().await.status, 401);
        }
    }
    assert!(entry.server.list_devices().unwrap().is_empty());
}

#[tokio::test]
async fn 配对写入哈希并设置受限cookie及正确来源() {
    let entry = MemoryEntry::new();
    let invite = entry.server.create_invite().unwrap();
    let reply = entry.pair_code(&invite.code).await;
    assert_eq!(reply.status, 200);
    let cookie = &reply.headers["set-cookie"];
    assert!(cookie.contains("HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000"));
    assert!(!cookie.contains("Secure"));
    let token = cookie
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("pl_device=")
        .unwrap();
    assert_eq!(token.len(), 43);
    let db = lock(&entry.server.shared.state).db.clone();
    let records = lock(&db).mobile_devices().unwrap();
    assert_eq!(records[0].token_hash, auth::digest(token));
    assert_eq!(records[0].device.permissions, Permissions::default());
    let serialized = serde_json::to_string(&entry.server.list_devices().unwrap()).unwrap();
    assert!(!serialized.contains(token));
    assert!(!serialized.contains("token_hash"));
    assert_eq!(
        lock(&entry.host.calls)[0],
        json!(["pair", "测试手机", "100.64.0.2"])
    );
    let bad = entry
        .request("GET", "/api/me", "pl_device=bad", json!(null))
        .await;
    assert_eq!(bad.status, 401);
    assert_eq!(
        entry
            .request(
                "GET",
                "/api/me",
                &format!("pl_device={token}; pl_device={token}"),
                json!(null)
            )
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn 三项权限独立拦截且查询结果原样透传() {
    let entry = MemoryEntry::new();
    let (id, cookie) = entry.pair().await;
    assert_eq!(
        entry
            .request("GET", "/api/overview", &cookie, json!(null))
            .await
            .json(),
        json!({"servers":[1],"sync":{"online":true}})
    );
    assert_eq!(
        entry
            .request("GET", "/api/terminals", &cookie, json!(null))
            .await
            .json(),
        json!([{"id":"term","title":"终端"}])
    );
    assert_eq!(
        entry
            .request("GET", "/api/terminals/term/screen", &cookie, json!(null))
            .await
            .json()["screen"]["text"],
        "$ "
    );
    assert_eq!(
        entry
            .request("GET", "/api/confirms", &cookie, json!(null))
            .await
            .json(),
        json!([{"id":"ask"}])
    );
    assert_eq!(
        entry
            .request("GET", "/api/activity?limit=3", &cookie, json!(null))
            .await
            .json(),
        json!([{"limit":3}])
    );
    assert_eq!(
        entry
            .request("GET", "/api/activity", &cookie, json!(null))
            .await
            .json(),
        json!([{"limit":50}])
    );
    for path in [
        "/api/activity?limit=0",
        "/api/activity?limit=201",
        "/api/activity?limit=2&limit=3",
    ] {
        assert_eq!(
            entry
                .request("GET", path, &cookie, json!(null))
                .await
                .status,
            400
        );
    }
    assert_eq!(
        entry
            .request(
                "POST",
                "/api/terminals/term/input",
                &cookie,
                json!({"text":"echo ok\r"})
            )
            .await
            .status,
        403
    );
    assert_eq!(
        entry
            .request("POST", "/api/stop-all", &cookie, json!({}))
            .await
            .status,
        403
    );
    assert_eq!(
        entry
            .request(
                "POST",
                "/api/confirms/ask",
                &cookie,
                json!({"answer":"allow"})
            )
            .await
            .status,
        403
    );
    entry
        .server
        .set_permissions(
            &id,
            Permissions {
                view: false,
                input: true,
                confirm: false,
            },
        )
        .unwrap();
    for path in [
        "/api/overview",
        "/api/terminals",
        "/api/terminals/term/screen",
        "/api/confirms",
        "/api/activity",
        "/api/events",
    ] {
        assert_eq!(
            entry
                .request("GET", path, &cookie, json!(null))
                .await
                .status,
            403
        );
    }
    assert_eq!(
        entry
            .request("GET", "/api/me", &cookie, json!(null))
            .await
            .json()["permissions"]["input"],
        true
    );
    assert_eq!(
        entry
            .request(
                "POST",
                "/api/terminals/term/input",
                &cookie,
                json!({"text":"echo ok\r"})
            )
            .await
            .status,
        200
    );
    entry
        .server
        .set_permissions(
            &id,
            Permissions {
                view: false,
                input: false,
                confirm: true,
            },
        )
        .unwrap();
    assert_eq!(
        entry
            .request("POST", "/api/stop-all", &cookie, json!({}))
            .await
            .status,
        200
    );
    for answer in ["allow", "deny"] {
        assert_eq!(
            entry
                .request(
                    "POST",
                    "/api/confirms/ask",
                    &cookie,
                    json!({"answer":answer})
                )
                .await
                .status,
            200
        );
    }
    assert_eq!(
        entry
            .request(
                "POST",
                "/api/confirms/ask",
                &cookie,
                json!({"answer":"yes"})
            )
            .await
            .status,
        400
    );
    assert_eq!(
        entry
            .request(
                "POST",
                "/api/terminals/term/input",
                &cookie,
                json!({"text":"x"})
            )
            .await
            .status,
        403
    );
    assert!(lock(&entry.host.calls).contains(&json!(["answer", "ask", false])));
}

#[tokio::test]
async fn 别的域名指过来的请求被拒绝() {
    let entry = MemoryEntry::new();
    let (_, cookie) = entry.pair().await;
    for (host, status) in [
        ("evil.example:12345", 403),
        ("evil.example.ts.net.attacker.test", 403),
        ("100.101.1.2:48600", 200),
        ("[fd7a:115c:a1e0::1]:48600", 200),
        ("mypc:48600", 200),
        ("mypc.tail1234.ts.net", 200),
    ] {
        let mut client = entry.connect();
        client
            .io
            .get_mut()
            .write_all(
                format!("GET /api/me HTTP/1.1\r\nHost: {host}\r\nCookie: {cookie}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        assert_eq!(client.headers().await.status, status, "{host}");
    }
}

#[tokio::test]
async fn 跨站来源和缺少自定义头被拒绝() {
    let entry = MemoryEntry::new();
    let (_, cookie) = entry.pair().await;
    for origin in [
        "http://evil.test:12345",
        "http://pc.example.ts.net:9",
        "null",
        "http://user@pc.example.ts.net:12345",
        "http://pc.example.ts.net:12345/evil",
    ] {
        let reply = entry
            .connect()
            .request(
                "GET",
                "/api/me",
                &format!("Cookie: {cookie}\r\nOrigin: {origin}\r\n"),
                "",
            )
            .await;
        assert_eq!(reply.status, 403, "{origin}");
    }
    assert_eq!(
        entry
            .connect()
            .request(
                "GET",
                "/api/me",
                &format!("Cookie: {cookie}\r\nOrigin: http://PC.EXAMPLE.ts.net:12345\r\n"),
                ""
            )
            .await
            .status,
        200
    );
    for path in ["/api/pair", "/api/logout", "/api/stop-all", "/anything"] {
        assert_eq!(
            entry
                .connect()
                .request("POST", path, &format!("Cookie: {cookie}\r\n"), "{}")
                .await
                .status,
            403
        );
    }
    assert_eq!(
        entry
            .connect()
            .request(
                "POST",
                "/api/logout",
                &format!("Cookie: {cookie}\r\nX-Pierlink: 1\r\nX-Pierlink: 1\r\n"),
                "{}"
            )
            .await
            .status,
        403
    );
}

#[tokio::test]
async fn 危险命令要求确认并保护分次提交和密码输入() {
    let entry = MemoryEntry::new();
    let (id, cookie) = entry.pair().await;
    entry
        .server
        .set_permissions(
            &id,
            Permissions {
                input: true,
                ..Permissions::default()
            },
        )
        .unwrap();
    let path = "/api/terminals/term/input";
    let reply = entry
        .request(
            "POST",
            path,
            &cookie,
            json!({"text":"echo ok\r\nrm -rf /work\r"}),
        )
        .await;
    assert_eq!(reply.status, 409);
    assert_eq!(
        reply.json(),
        json!({"needs_confirm":true,"lines":["rm -rf /work"]})
    );
    assert!(!lock(&entry.host.calls).iter().any(|v| v[0] == "input"));
    assert_eq!(
        entry
            .request(
                "POST",
                path,
                &cookie,
                json!({"text":"rm -rf /work\r","confirmed":true})
            )
            .await
            .status,
        200
    );
    assert_eq!(
        entry
            .request("POST", path, &cookie, json!({"text":"reboot"}))
            .await
            .status,
        200
    );
    assert_eq!(
        entry
            .request("POST", path, &cookie, json!({"keys":["Enter"]}))
            .await
            .status,
        409
    );
    assert_eq!(
        entry
            .request(
                "POST",
                path,
                &cookie,
                json!({"keys":["Ctrl+M"],"confirmed":true})
            )
            .await
            .status,
        200
    );
    assert_eq!(
        entry
            .request("POST", path, &cookie, json!({"keys":["ArrowUp","Enter"]}))
            .await
            .status,
        409
    );
    *lock(&entry.host.screen) = "[sudo] password for user: ".into();
    let count = lock(&entry.host.calls).len();
    assert_eq!(
        entry
            .request(
                "POST",
                path,
                &cookie,
                json!({"text":"不应出现的秘密","confirmed":true})
            )
            .await
            .status,
        403
    );
    assert_eq!(lock(&entry.host.calls).len(), count);
    assert_eq!(
        entry
            .request("POST", path, &cookie, json!({"keys":["Ctrl+C"]}))
            .await
            .status,
        200
    );
    for value in [
        json!({"keys":["秘密"]}),
        json!({"text":"a","keys":["Enter"]}),
        json!({"text":"\u{1b}[A"}),
        json!({"text":""}),
        json!({"keys":[]}),
    ] {
        assert_eq!(
            entry.request("POST", path, &cookie, value).await.status,
            400
        );
    }
    let audits = serde_json::to_string(&*lock(&entry.host.audits)).unwrap();
    for secret in ["不应出现的秘密", "rm -rf", "pl_device="] {
        assert!(!audits.contains(secret));
    }
    assert!(
        lock(&entry.host.audits)
            .iter()
            .any(|r| r.device_name == "测试手机"
                && r.result == "needs_confirm"
                && r.target.as_deref() == Some("term"))
    );
}

#[tokio::test]
async fn 静态文件回退缓存安全头及路径穿越() {
    let entry = MemoryEntry::new();
    for path in ["/", "/nested/mobile/page"] {
        let reply = entry.request("GET", path, "", json!(null)).await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, "<html>手机</html>");
        assert_eq!(reply.headers["cache-control"], "no-store");
        assert_eq!(reply.headers["content-type"], "text/html; charset=utf-8");
        assert!(
            reply.headers["content-security-policy"]
                .contains("script-src 'self'; style-src 'self'")
        );
        assert_eq!(reply.headers["x-content-type-options"], "nosniff");
        assert_eq!(reply.headers["referrer-policy"], "no-referrer");
    }
    let reply = entry
        .request("GET", "/app-a1b2c3d4.js", "", json!(null))
        .await;
    assert_eq!(
        reply.headers["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        reply.headers["content-type"],
        "text/javascript; charset=utf-8"
    );
    assert_eq!(
        entry
            .request("GET", "/style.css", "", json!(null))
            .await
            .headers["cache-control"],
        "no-cache"
    );
    for path in [
        "/../db",
        "/%2e%2e/db",
        "/%252e%252e/db",
        "//etc/passwd",
        "/%2fetc/passwd",
        "/C:%5cWindows",
        "/a%5c..%5cdb",
        "/foo%00bar",
        "/%xx",
    ] {
        assert_eq!(
            entry.request("GET", path, "", json!(null)).await.status,
            403,
            "{path}"
        );
    }
    assert_eq!(
        entry
            .request("GET", "/api/unknown", "", json!(null))
            .await
            .status,
        401
    );
}

#[cfg(unix)]
#[tokio::test]
async fn 符号链接不得越出资源目录() {
    let entry = MemoryEntry::new();
    std::os::unix::fs::symlink(entry.root.path(), entry.root.path().join("assets/outside"))
        .unwrap();
    for path in ["/outside/db", "/outside/missing"] {
        assert_eq!(
            entry.request("GET", path, "", json!(null)).await.status,
            403
        );
    }
}

#[tokio::test]
async fn 三十天失效且撤销记录有保留期限() {
    let entry = MemoryEntry::new();
    let (id, cookie) = entry.pair().await;
    entry.sql(
        "UPDATE mobile_devices SET last_active_at=?1 WHERE id=?2",
        rusqlite::params![now() - INACTIVE_SECONDS, &id],
    );
    assert_eq!(
        entry
            .request("GET", "/api/me", &cookie, json!(null))
            .await
            .status,
        401
    );
    assert_eq!(entry.server.list_devices().unwrap().len(), 1);
    entry.sql(
        "UPDATE mobile_devices SET last_active_at=?1 WHERE id=?2",
        rusqlite::params![now() - 2 * INACTIVE_SECONDS, &id],
    );
    assert!(entry.server.list_devices().unwrap().is_empty());
    let (id, _) = entry.pair().await;
    entry.server.revoke_device(&id).unwrap();
    entry.server.revoke_device(&id).unwrap();
    assert!(entry.server.list_devices().unwrap()[0].revoked_at.is_some());
    entry.sql(
        "UPDATE mobile_devices SET revoked_at=?1 WHERE id=?2",
        rusqlite::params![now() - INACTIVE_SECONDS, &id],
    );
    assert!(entry.server.list_devices().unwrap().is_empty());
    assert!(entry.server.revoke_device("missing").is_err());
    assert!(
        entry
            .server
            .set_permissions("missing", Permissions::default())
            .is_err()
    );
}

async fn sse(entry: &MemoryEntry, cookie: &str) -> Client {
    let mut client = entry.connect();
    client
        .send("GET", "/api/events", &format!("Cookie: {cookie}\r\n"), "")
        .await;
    let reply = client.headers().await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.headers["content-type"], "text/event-stream");
    assert_eq!(client.chunk().await, b": connected\n\n");
    client
}

#[tokio::test]
async fn 事件推送心跳不重放且撤销立即断开对应连接() {
    let entry = MemoryEntry::limits(Limits {
        heartbeat: Duration::from_millis(20),
        ..Limits::default()
    });
    let (id, cookie) = entry.pair().await;
    let (other_id, other_cookie) = entry.pair().await;
    let mut events = sse(&entry, &cookie).await;
    let mut other = sse(&entry, &other_cookie).await;
    entry.host.events.send("confirms".into()).unwrap();
    assert_eq!(events.chunk().await, b"event: confirms\ndata: {}\n\n");
    assert_eq!(other.chunk().await, b"event: confirms\ndata: {}\n\n");
    assert_eq!(events.chunk().await, b": heartbeat\n\n");
    entry.server.revoke_device(&id).unwrap();
    events.closed().await;
    assert_eq!(
        entry
            .request("GET", "/api/me", &cookie, json!(null))
            .await
            .status,
        401
    );
    assert_eq!(
        entry
            .request("GET", "/api/me", &other_cookie, json!(null))
            .await
            .status,
        200
    );
    drop(other);
    let mut reconnect = sse(&entry, &other_cookie).await;
    assert_eq!(reconnect.chunk().await, b": heartbeat\n\n");
    entry
        .server
        .set_permissions(
            &other_id,
            Permissions {
                view: false,
                input: false,
                confirm: true,
            },
        )
        .unwrap();
    reconnect.closed().await;
}

#[tokio::test]
async fn 撤销全部与退出登录关闭已打开连接() {
    let entry = MemoryEntry::new();
    let (_, cookie) = entry.pair().await;
    let mut events = sse(&entry, &cookie).await;
    let reply = entry
        .request("POST", "/api/logout", &cookie, json!({}))
        .await;
    assert_eq!(reply.status, 200);
    assert!(reply.headers["set-cookie"].ends_with("Max-Age=0"));
    events.closed().await;
    let (_, cookie) = entry.pair().await;
    let mut events = sse(&entry, &cookie).await;
    entry.server.revoke_all().unwrap();
    events.closed().await;
    assert_eq!(
        entry
            .request("GET", "/api/me", &cookie, json!(null))
            .await
            .status,
        401
    );
    assert!(
        entry
            .server
            .list_devices()
            .unwrap()
            .iter()
            .all(|d| d.revoked_at.is_some())
    );
}

#[tokio::test]
async fn 请求体定长和分块都受上限约束() {
    let entry = MemoryEntry::new();
    let (_, cookie) = entry.pair().await;
    let big = "x".repeat(64 * 1024 + 1);
    assert_eq!(
        entry
            .connect()
            .request("POST", "/api/pair", "X-Pierlink: 1\r\n", &big)
            .await
            .status,
        413
    );
    let mut client = entry.connect();
    let request = format!(
        "POST /api/logout HTTP/1.1\r\nHost: pc.example.ts.net\r\nCookie: {cookie}\r\nX-Pierlink: 1\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{big}\r\n0\r\n\r\n",
        big.len()
    );
    client
        .io
        .get_mut()
        .write_all(request.as_bytes())
        .await
        .unwrap();
    assert_eq!(client.receive().await.status, 413);
    assert_eq!(
        entry
            .request("GET", "/api/me", &cookie, json!(null))
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn 请求头期限不被慢速字节续期且连接数有上限() {
    let entry = MemoryEntry::limits(Limits {
        header: Duration::from_millis(35),
        idle: Duration::from_millis(100),
        ..Limits::default()
    });
    let mut client = entry.connect();
    client
        .io
        .get_mut()
        .write_all(b"GET / HTTP/1.1\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    client.io.get_mut().write_all(b"Host: ").await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    client.closed().await;
    let mut connections = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        connections.push(entry.connect());
    }
    let mut extra = entry.connect();
    extra.closed().await;
    entry.server.stop();
    for mut connection in connections {
        connection.closed().await;
    }
}

#[tokio::test]
async fn 空闲连接及入口释放关闭连接() {
    let entry = MemoryEntry::limits(Limits {
        header: Duration::from_secs(1),
        idle: Duration::from_millis(25),
        ..Limits::default()
    });
    let mut client = entry.connect();
    assert_eq!(client.request("GET", "/", "", "").await.status, 200);
    client.closed().await;
    let mut client = entry.connect();
    client.send("HEAD", "/", "", "").await;
    assert_eq!(client.headers().await.status, 200);
    client.closed().await;
    let mut client = entry.connect();
    drop(entry);
    client.closed().await;
}

#[tokio::test]
async fn 宿主默认查询为空且写操作与安全读屏拒绝() {
    struct Empty;
    impl MobileHost for Empty {}
    let host = Empty;
    assert!(
        !host
            .approve_pairing("手机", "127.0.0.1".parse().unwrap())
            .await
            .unwrap()
    );
    assert_eq!(host.overview().await.unwrap(), json!({}));
    assert_eq!(host.terminals().await.unwrap(), json!([]));
    assert_eq!(host.confirms().await.unwrap(), json!([]));
    assert_eq!(host.activity(1).await.unwrap(), json!([]));
    assert!(host.terminal_screen("t").await.is_err());
    assert!(host.terminal_text("t").await.is_err());
    assert!(
        host.terminal_input("t", TerminalInput::default())
            .await
            .is_err()
    );
    assert!(host.answer_confirm("c", true).await.is_err());
    assert!(host.stop_all().await.is_err());
    assert!(host.subscribe().is_none());
    host.audit(AuditRecord {
        device_name: "手机".into(),
        action: "me".into(),
        target: None,
        result: "ok".into(),
        peer: "127.0.0.1".into(),
    });
}

#[tokio::test]
async fn 活动请求刷新设备时间和cookie且认证不能在同连接换设备() {
    let entry = MemoryEntry::new();
    let (id, cookie) = entry.pair().await;
    let (_, other) = entry.pair().await;
    entry.sql(
        "UPDATE mobile_devices SET last_active_at=?1 WHERE id=?2",
        rusqlite::params![now() - INACTIVE_SECONDS + 60, &id],
    );
    let mut client = entry.connect();
    let reply = client
        .request("GET", "/api/me", &format!("Cookie: {cookie}\r\n"), "")
        .await;
    assert_eq!(reply.status, 200);
    assert!(reply.headers["set-cookie"].starts_with(&cookie));
    assert!(
        entry
            .server
            .list_devices()
            .unwrap()
            .iter()
            .find(|d| d.id == id)
            .unwrap()
            .last_active_at
            >= now() - 1
    );
    assert_eq!(
        client
            .request("GET", "/api/me", &format!("Cookie: {other}\r\n"), "")
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn 撤销取消尚未收完的输入并断开已认证普通连接() {
    let entry = MemoryEntry::new();
    let (id, cookie) = entry.pair().await;
    entry
        .server
        .set_permissions(
            &id,
            Permissions {
                input: true,
                ..Permissions::default()
            },
        )
        .unwrap();
    let mut idle = entry.connect();
    assert_eq!(
        idle.request("GET", "/api/me", &format!("Cookie: {cookie}\r\n"), "")
            .await
            .status,
        200
    );
    let mut slow = entry.connect();
    slow.io.get_mut().write_all(format!("POST /api/terminals/term/input HTTP/1.1\r\nHost: pc.example.ts.net\r\nX-Pierlink: 1\r\nCookie: {cookie}\r\nContent-Length: 40\r\n\r\n{{").as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    entry.server.revoke_device(&id).unwrap();
    idle.closed().await;
    slow.closed().await;
    assert!(!lock(&entry.host.calls).iter().any(|v| v[0] == "input"));
    assert!(
        lock(&entry.host.audits)
            .iter()
            .any(|r| r.action == "terminal_input" && r.result == "cancelled")
    );
}

#[tokio::test]
async fn 同邀请码并发只发起一次审批且停止取消等待() {
    let entry = MemoryEntry::new();
    entry.host.approval.store(2, Ordering::SeqCst);
    let invite = entry.server.create_invite().unwrap();
    let mut waiting = entry.connect();
    waiting
        .send(
            "POST",
            "/api/pair",
            "X-Pierlink: 1\r\n",
            &json!({"code":invite.code,"device_name":"手机"}).to_string(),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(entry.pair_code(&invite.code).await.status, 401);
    assert_eq!(entry.host.approvals.load(Ordering::SeqCst), 1);
    entry.server.stop();
    waiting.closed().await;
    assert!(entry.server.list_devices().unwrap().is_empty());
}

#[tokio::test]
async fn 广播滞后结束订阅并过滤伪造事件名() {
    let entry = MemoryEntry::limits(Limits {
        heartbeat: Duration::from_millis(15),
        ..Limits::default()
    });
    let (_, cookie) = entry.pair().await;
    let mut events = sse(&entry, &cookie).await;
    entry
        .host
        .events
        .send("confirms\ndata: 秘密".into())
        .unwrap();
    assert_eq!(events.chunk().await, b": heartbeat\n\n");
    for _ in 0..32 {
        entry.host.events.send("overview".into()).unwrap();
    }
    assert!(events.chunk().await.is_empty());
}

#[tokio::test]
async fn 已配对数量有界且失败配对有延时() {
    let entry = MemoryEntry::new();
    for n in 0..MAX_DEVICES {
        let device = Device {
            id: n.to_string(),
            name: "手机".into(),
            user_agent: None,
            last_ip: None,
            permissions: Permissions::default(),
            created_at: now(),
            last_active_at: now(),
            revoked_at: None,
        };
        let db = lock(&entry.server.shared.state).db.clone();
        lock(&db)
            .insert_mobile_device(&device, &auth::digest(&n.to_string()))
            .unwrap();
    }
    let invite = entry.server.create_invite().unwrap();
    assert_eq!(entry.pair_code(&invite.code).await.status, 429);
    let started = tokio::time::Instant::now();
    assert_eq!(entry.pair_code("wrong").await.status, 401);
    assert!(started.elapsed() >= entry.server.shared.limits.failure_delay);
    entry.server.revoke_device("0").unwrap();
    entry.pair().await;
    entry.server.revoke_all().unwrap();
    assert_eq!(entry.server.list_devices().unwrap().len(), MAX_DEVICES);
}

#[path = "device_tests.rs"]
mod device_tests;
