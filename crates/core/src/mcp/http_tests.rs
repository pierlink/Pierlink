//! 真实回环 TCP 客户端覆盖认证、协议、安全限制及入口生命周期。
use super::*;
use crate::mcp::http::test_support::MemoryEntry;
use std::collections::HashMap;
use tokio::{io::AsyncReadExt, net::TcpStream};
use zeroize::Zeroizing;

const AUTH: &str = "Authorization: Bearer test-entry-token\r\n";
const ACCEPT: &str = "Accept: application/json, text/event-stream\r\n";
const VERSION: &str = "MCP-Protocol-Version: 2026-07-28\r\n";

trait TestIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> TestIo for T {}

struct HttpClient {
    io: BufReader<Box<dyn TestIo>>,
}
struct Reply {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}
impl Reply {
    fn json(&self) -> Value {
        if self
            .headers
            .get("content-type")
            .is_some_and(|v| v.starts_with("text/event-stream"))
        {
            self.body
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .find_map(|line| serde_json::from_str(line).ok())
                .expect("SSE 中缺少 JSON")
        } else {
            serde_json::from_str(&self.body).expect("响应不是 JSON")
        }
    }
}
impl HttpClient {
    fn memory(entry: &MemoryEntry) -> Self {
        Self {
            io: BufReader::new(Box::new(entry.connect())),
        }
    }
    async fn connect(port: u16) -> Self {
        Self {
            io: BufReader::new(Box::new(
                TcpStream::connect(("127.0.0.1", port)).await.unwrap(),
            )),
        }
    }
    async fn send(&mut self, method: &str, headers: &str, body: &str) {
        let request = format!(
            "{method} /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}\r\n{body}",
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
            .expect("连接未返回状态行")
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
    async fn receive(&mut self) -> Reply {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut reply = self.headers().await;
            let mut body = Vec::new();
            if reply
                .headers
                .get("transfer-encoding")
                .is_some_and(|v| v == "chunked")
            {
                loop {
                    let mut line = String::new();
                    self.io.read_line(&mut line).await.unwrap();
                    let size =
                        usize::from_str_radix(line.trim().split(';').next().unwrap(), 16).unwrap();
                    let start = body.len();
                    body.resize(start + size, 0);
                    self.io.read_exact(&mut body[start..]).await.unwrap();
                    let mut end = [0; 2];
                    self.io.read_exact(&mut end).await.unwrap();
                    assert_eq!(&end, b"\r\n");
                    if size == 0 {
                        break;
                    }
                }
            } else if let Some(size) = reply.headers.get("content-length") {
                body.resize(size.parse().unwrap(), 0);
                self.io.read_exact(&mut body).await.unwrap();
            }
            reply.body = String::from_utf8(body).unwrap();
            reply
        })
        .await
        .unwrap()
    }
    async fn request(&mut self, method: &str, headers: &str, body: &str) -> Reply {
        self.send(method, headers, body).await;
        self.receive().await
    }
    async fn initialize(&mut self) -> String {
        let reply = self.request("POST", &format!("{AUTH}{ACCEPT}"), &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","clientInfo":{"name":"旧客户端","version":"1"},"capabilities":{}}}).to_string()).await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.json()["result"]["protocolVersion"], "2025-11-25");
        let id = reply.headers["mcp-session-id"].clone();
        let initialized = self
            .request(
                "POST",
                &legacy_headers(&id),
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string(),
            )
            .await;
        assert_eq!(initialized.status, 202);
        id
    }
    async fn closed(&mut self) {
        let mut byte = [0];
        let result = tokio::time::timeout(Duration::from_secs(3), self.io.read(&mut byte))
            .await
            .unwrap();
        assert!(matches!(result, Ok(0) | Err(_)), "连接仍有数据");
    }
    async fn run_command(&mut self, token: &str) -> Value {
        let reply = self.request(
            "POST",
            &format!("Authorization: Bearer {token}\r\n{ACCEPT}{VERSION}Mcp-Method: tools/call\r\nMcp-Name: run_command\r\n"),
            &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run_command","arguments":{"target":"server-id","command":"rm -rf /example"},"_meta":meta("2026-07-28")}}).to_string(),
        ).await;
        assert_eq!(reply.status, 200);
        reply.json()
    }
}
fn legacy_headers(id: &str) -> String {
    format!("{AUTH}{ACCEPT}Mcp-Session-Id: {id}\r\nMCP-Protocol-Version: 2025-11-25\r\n")
}
fn modern_headers() -> String {
    format!("{AUTH}{ACCEPT}{VERSION}Mcp-Method: tools/call\r\nMcp-Name: list_targets\r\n")
}
fn call(version: Option<&str>) -> String {
    let mut params = json!({"name":"list_targets","arguments":{}});
    if let Some(version) = version {
        params["_meta"] = meta(version);
    }
    json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":params}).to_string()
}
fn start(f: &Fixture) -> u16 {
    f.service
        .start_http_entry("server-id", Zeroizing::new("test-entry-token".into()))
        .unwrap()
        .local_port
}
async fn unused(port: u16) {
    let start = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
        assert!(start.elapsed() < Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn 认证失败延时且来源和主机校验不能绕过() {
    let f = Fixture::new();
    let port = start(&f);
    for auth in [
        "",
        "Authorization: Bearer wrong\r\n",
        "Authorization: Basic test-entry-token\r\n",
        "Authorization: Bearer test-entry-tokeN\r\n",
        "Authorization: Bearer test-entry-token-more\r\n",
        "Authorization: Bearer test-entry-token\r\nAuthorization: Bearer test-entry-token\r\n",
    ] {
        let mut client = HttpClient::connect(port).await;
        let start = Instant::now();
        let reply = client
            .request("POST", &format!("{auth}{ACCEPT}"), &call(None))
            .await;
        assert_eq!(reply.status, 401);
        assert!(start.elapsed() >= Duration::from_millis(200));
        assert_eq!(reply.headers["www-authenticate"], "Bearer");
    }
    for origin in ["http://evil.example", "http://127.0.0.1", "null", ""] {
        let mut client = HttpClient::connect(port).await;
        let reply = client
            .request(
                "POST",
                &format!("{AUTH}{ACCEPT}Origin: {origin}\r\n"),
                &call(None),
            )
            .await;
        assert_eq!(reply.status, 403);
    }
    let mut client = HttpClient::connect(port).await;
    client.io.get_mut().write_all(format!("POST /mcp HTTP/1.1\r\nHost: evil.example\r\n{AUTH}{ACCEPT}Content-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
    assert_eq!(client.receive().await.status, 403);
    assert!(f.service.sessions().is_empty());
}

#[tokio::test]
async fn 协议规定缺少接受头和不同版本的获取行为() {
    let f = Fixture::new();
    let mut client = HttpClient::connect(start(&f)).await;
    assert_eq!(client.request("POST", AUTH, &call(None)).await.status, 406);
    assert_eq!(client.request("GET", AUTH, "").await.status, 406);
    assert_eq!(
        client
            .request("GET", &format!("{AUTH}{ACCEPT}{VERSION}"), "")
            .await
            .status,
        405
    );
    assert_eq!(
        client
            .request("GET", &format!("{AUTH}{ACCEPT}"), "")
            .await
            .status,
        400
    );
    let id = client.initialize().await;
    client.send("GET", &legacy_headers(&id), "").await;
    let reply = client.headers().await;
    assert_eq!(reply.status, 200);
    assert!(reply.headers["content-type"].starts_with("text/event-stream"));
}

#[tokio::test]
async fn 旧式会话与现代调用复用确认权限和审计() {
    let f = Fixture::new();
    let port = start(&f);
    let mut legacy = HttpClient::connect(port).await;
    let id = legacy.initialize().await;
    assert_eq!(f.host.confirmations.load(Ordering::Acquire), 0);
    for _ in 0..2 {
        let reply = legacy
            .request("POST", &legacy_headers(&id), &call(None))
            .await;
        assert_eq!(reply.status, 200);
        assert!(
            reply.json()["result"]["structuredContent"].is_array(),
            "{}",
            reply.body
        );
        assert!(!reply.body.contains("user@private"));
    }
    assert_eq!(f.host.confirmations.load(Ordering::Acquire), 1);
    assert_eq!(
        f.service.sessions()[0].entry,
        Entry::Server {
            server_id: "server-id".into()
        }
    );
    let mut modern = HttpClient::connect(port).await;
    let reply = modern
        .request("POST", &modern_headers(), &call(Some("2026-07-28")))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.headers["content-type"].starts_with("text/event-stream"));
    assert!(!reply.headers.contains_key("mcp-session-id"));
    assert!(reply.json()["result"].is_object());
    // 同一个入口、同一代令牌：第一次连接已经确认过，无握手的请求不再问
    assert_eq!(f.host.confirmations.load(Ordering::Acquire), 1);
    for _ in 0..2 {
        let reply = modern
            .request("POST", &modern_headers(), &call(Some("2026-07-28")))
            .await;
        assert_eq!(reply.status, 200, "{}", reply.body);
    }
    assert_eq!(f.host.confirmations.load(Ordering::Acquire), 1);
    assert_eq!(f.service.activity(10).len(), 5);
    assert!(f.service.activity(10).iter().all(|r| r.entry
        == Entry::Server {
            server_id: "server-id".into()
        }));
    let reply = modern
        .request(
            "POST",
            &format!("{AUTH}{ACCEPT}MCP-Protocol-Version: 1900-01-01\r\n"),
            &call(Some("1900-01-01")),
        )
        .await;
    assert_eq!(reply.json()["error"]["code"], -32022, "{}", reply.body);
    let deleted = legacy.request("DELETE", &legacy_headers(&id), "").await;
    assert_eq!(deleted.status, 202);
    until(|| f.service.sessions().is_empty()).await;
}

#[tokio::test]
async fn 请求体上限覆盖声明长度与分块传输() {
    let f = Fixture::new();
    let port = start(&f);
    let mut body = call(Some("2026-07-28"));
    body.extend(std::iter::repeat_n(' ', 8 * 1024 * 1024 - body.len()));
    let mut client = HttpClient::connect(port).await;
    assert_eq!(
        client
            .request("POST", &modern_headers(), &body)
            .await
            .status,
        200
    );
    body.push(' ');
    let mut client = HttpClient::connect(port).await;
    assert_eq!(
        client
            .request("POST", &modern_headers(), &body)
            .await
            .status,
        413
    );
    let mut client = HttpClient::connect(port).await;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\n{}Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
        modern_headers(),
        body.len()
    );
    client
        .io
        .get_mut()
        .write_all(request.as_bytes())
        .await
        .unwrap();
    assert_eq!(client.receive().await.status, 413);
}

#[tokio::test]
async fn 二十条连接可用且第三十三条关闭() {
    let f = Fixture::new();
    let port = start(&f);
    let mut clients = Vec::new();
    for _ in 0..32 {
        let mut client = HttpClient::connect(port).await;
        // 406 不建立逻辑会话，用它确认 TCP 已被服务接纳。
        assert_eq!(client.request("POST", AUTH, "{}").await.status, 406);
        clients.push(client);
    }
    for client in clients.iter_mut().take(20) {
        assert_eq!(
            client
                .request("POST", &modern_headers(), &call(Some("2026-07-28")))
                .await
                .status,
            200
        );
        until(|| f.service.sessions().is_empty()).await;
    }
    let mut extra = HttpClient::connect(port).await;
    extra.closed().await;
    clients.pop();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut freed = HttpClient::connect(port).await;
    assert_eq!(freed.request("POST", AUTH, "{}").await.status, 406);
}

#[tokio::test]
async fn 每入口最多八个会话并可在删除后继续接入() {
    let f = Fixture::new();
    let port = start(&f);
    let mut client = HttpClient::connect(port).await;
    let mut ids = Vec::new();
    for _ in 0..8 {
        ids.push(client.initialize().await);
    }
    assert_eq!(f.service.sessions().len(), 8);
    let reply = client
        .request("POST", &modern_headers(), &call(Some("2026-07-28")))
        .await;
    assert!(
        reply.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("最多允许 8 个")
    );
    let mut other = HttpClient::connect(
        f.service
            .start_http_entry("other", Zeroizing::new("test-entry-token".into()))
            .unwrap()
            .local_port,
    )
    .await;
    other.initialize().await;
    assert_eq!(f.service.sessions().len(), 9);
    assert_eq!(
        client
            .request("DELETE", &legacy_headers(&ids[0]), "")
            .await
            .status,
        202
    );
    until(|| f.service.sessions().len() == 8).await;
    client.initialize().await;
    assert_eq!(f.service.sessions().len(), 9);
}

#[tokio::test]
async fn 令牌轮换取消调用旧会话失效且入口隔离() {
    let f = Fixture::new();
    let port = start(&f);
    let mut client = HttpClient::connect(port).await;
    let id = client.initialize().await;
    let mut idle = HttpClient::connect(port).await;
    assert_eq!(idle.request("POST", AUTH, "{}").await.status, 406);
    let mut other = HttpClient::connect(
        f.service
            .start_http_entry("other", Zeroizing::new("test-entry-token".into()))
            .unwrap()
            .local_port,
    )
    .await;
    let other_id = other.initialize().await;
    f.host.block.store(true, Ordering::Release);
    client.send("POST", &legacy_headers(&id), &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run_command","arguments":{"target":"server-id","command":"sleep"}}}).to_string()).await;
    // 读取响应头，确认请求已进入 SSE 响应阶段。
    let _ = client.headers().await;
    until(|| f.host.started.load(Ordering::Acquire) == 1).await;
    f.service
        .set_token("server-id", Zeroizing::new("rotated-entry-token".into()));
    idle.closed().await;
    until(|| f.host.cancelled.load(Ordering::Acquire) == 1).await;
    until(|| f.service.sessions().len() == 1).await;
    let mut old = HttpClient::connect(port).await;
    assert_eq!(
        old.request("POST", &modern_headers(), &call(Some("2026-07-28")))
            .await
            .status,
        401
    );
    let headers = legacy_headers(&id).replace("test-entry-token", "rotated-entry-token");
    assert_eq!(old.request("POST", &headers, &call(None)).await.status, 404);
    let reply = old
        .request(
            "POST",
            &modern_headers().replace("test-entry-token", "rotated-entry-token"),
            &call(Some("2026-07-28")),
        )
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(
        other
            .request("POST", &legacy_headers(&other_id), &call(None))
            .await
            .status,
        200
    );
    f.service.stop_http_entry("server-id");
    unused(port).await;
    old.closed().await;
    assert_eq!(
        other
            .request("POST", &legacy_headers(&other_id), &call(None))
            .await
            .status,
        200
    );
    f.service.stop_http_entry("server-id");
}

#[tokio::test]
async fn 重新启动全部停止和释放服务会回收连接() {
    let f = Fixture::new();
    let first = start(&f);
    let mut client = HttpClient::connect(first).await;
    client.initialize().await;
    let second = start(&f);
    unused(first).await;
    client.closed().await;
    let mut client = HttpClient::connect(second).await;
    let id = client.initialize().await;
    f.service.stop_all();
    client.closed().await;
    assert!(f.service.sessions().is_empty());
    let mut client = HttpClient::connect(second).await;
    assert_eq!(
        client
            .request("POST", &legacy_headers(&id), &call(None))
            .await
            .status,
        404
    );
    client.initialize().await;
    drop(f.service);
    unused(second).await;
    client.closed().await;
}

#[tokio::test]
async fn 无握手请求共用操作审批标记且不同入口隔离() {
    let f = Fixture::new();
    let entry = MemoryEntry::new(&f.service, "test-entry-token", Duration::from_secs(60));
    *lock(&f.host.action_decision) = Confirmation::AllowAlways;
    let mut first = HttpClient::memory(&entry);
    assert_eq!(
        first.run_command("test-entry-token").await["result"]["isError"],
        false
    );
    let first_session = f.service.activity(1)[0].session.clone();
    assert_eq!(f.service.activity(1)[0].approval, "session");
    until(|| f.service.sessions().is_empty()).await;
    *lock(&f.host.action_decision) = Confirmation::Deny;
    for _ in 0..2 {
        let mut next = HttpClient::memory(&entry);
        assert_eq!(
            next.run_command("test-entry-token").await["result"]["isError"],
            false
        );
        let record = f.service.activity(1).remove(0);
        assert_ne!(record.session, first_session);
        assert_eq!(record.approval, "session");
        until(|| f.service.sessions().is_empty()).await;
    }
    assert_eq!(lock(&f.host.dangers).len(), 1);
    assert_eq!(f.host.confirmations.load(Ordering::Acquire), 1);
    let other_entry = MemoryEntry::for_server(
        &f.service,
        "other",
        "other-entry-token",
        Duration::from_secs(60),
    );
    let mut other = HttpClient::memory(&other_entry);
    assert_eq!(
        error_text(&other.run_command("other-entry-token").await),
        "用户拒绝执行"
    );
    assert_eq!(lock(&f.host.dangers).len(), 2);
    assert_eq!(f.service.activity(1)[0].approval, "");
}

#[tokio::test]
async fn 换令牌重启入口和全部停止后重新审批操作() {
    let f = Fixture::new();
    let mut entry = MemoryEntry::new(&f.service, "test-entry-token", Duration::from_secs(60));
    let mut token = "test-entry-token";
    for reset in ["换令牌", "重启入口", "全部停止"] {
        *lock(&f.host.action_decision) = Confirmation::AllowAlways;
        let mut client = HttpClient::memory(&entry);
        assert_eq!(client.run_command(token).await["result"]["isError"], false);
        assert_eq!(f.service.activity(1)[0].approval, "session");
        until(|| f.service.sessions().is_empty()).await;
        let count = lock(&f.host.dangers).len();
        match reset {
            "换令牌" => {
                token = "rotated-entry-token";
                f.service
                    .set_token("server-id", Zeroizing::new(token.into()));
            }
            "重启入口" => {
                entry = MemoryEntry::new(&f.service, token, Duration::from_secs(60));
            }
            _ => f.service.stop_all(),
        }
        client.closed().await;
        *lock(&f.host.action_decision) = Confirmation::Deny;
        let mut next = HttpClient::memory(&entry);
        assert_eq!(
            error_text(&next.run_command(token).await),
            "用户拒绝执行",
            "{reset}"
        );
        assert_eq!(lock(&f.host.dangers).len(), count + 1, "{reset}");
        assert_eq!(f.service.activity(1)[0].approval, "");
        until(|| f.service.sessions().is_empty()).await;
    }
}

#[tokio::test]
async fn 空闲连接慢速请求头和孤立会话有期限() {
    let f = Fixture::new();
    let timeout = Duration::from_millis(180);
    let port = f
        .service
        .start_http_with_timeout(
            "server-id",
            Zeroizing::new("test-entry-token".into()),
            timeout,
        )
        .unwrap()
        .local_port;
    let mut idle = HttpClient::connect(port).await;
    let id = idle.initialize().await;
    idle.closed().await;
    until(|| f.service.sessions().is_empty()).await;
    // 会话刚回收时底层可能短暂回 500（会话正在关闭），随后稳定为 404：最多等 2 秒
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let mut stale = HttpClient::connect(port).await;
        let status = stale
            .request("POST", &legacy_headers(&id), &call(None))
            .await
            .status;
        if status == 404 {
            break;
        }
        assert_eq!(status, 500, "过期会话只能被拒绝");
        assert!(Instant::now() < deadline, "过期会话一直没有变成 404");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut slow = HttpClient::connect(port).await;
    slow.io
        .get_mut()
        .write_all(b"POST /mcp HTTP/1.1\r\nHost: ")
        .await
        .unwrap();
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        // 机器忙时睡过头，服务器可能已经按期限关掉连接（正是要验证的）：写失败就不再写
        if slow.io.get_mut().write_all(b"x").await.is_err() {
            break;
        }
    }
    slow.closed().await;
}

#[tokio::test]
async fn 空令牌拒绝启动且更新空值停止入口() {
    let f = Fixture::new();
    assert!(
        f.service
            .start_http_entry("server-id", Zeroizing::new(String::new()))
            .is_err()
    );
    f.service
        .set_token("missing", Zeroizing::new("unused-token".into()));
    let port = start(&f);
    f.service
        .set_token("server-id", Zeroizing::new(String::new()));
    unused(port).await;
}

#[tokio::test]
async fn 内存字节流校验认证来源协议和会话回收() {
    let f = Fixture::new();
    let entry = MemoryEntry::new(&f.service, "test-entry-token", Duration::from_secs(60));
    for auth in ["", "Authorization: Bearer wrong\r\n"] {
        let mut client = HttpClient::memory(&entry);
        let start = Instant::now();
        assert_eq!(
            client
                .request("POST", &format!("{auth}{ACCEPT}"), &call(None))
                .await
                .status,
            401
        );
        assert!(start.elapsed() >= Duration::from_millis(200));
    }
    let mut client = HttpClient::memory(&entry);
    for origin in ["http://evil.example", "http://localhost", "null", ""] {
        assert_eq!(
            client
                .request(
                    "POST",
                    &format!("{AUTH}{ACCEPT}Origin: {origin}\r\n"),
                    &call(None)
                )
                .await
                .status,
            403
        );
    }
    client.io.get_mut().write_all(format!("POST /mcp HTTP/1.1\r\nHost: evil.example\r\n{AUTH}{ACCEPT}Content-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
    assert_eq!(client.receive().await.status, 403);
    assert_eq!(client.request("POST", AUTH, &call(None)).await.status, 406);
    assert_eq!(client.request("GET", AUTH, "").await.status, 406);
    assert_eq!(
        client
            .request("GET", &format!("{AUTH}{ACCEPT}{VERSION}"), "")
            .await
            .status,
        405
    );
    assert_eq!(
        client
            .request("GET", &format!("{AUTH}{ACCEPT}"), "")
            .await
            .status,
        400
    );
    let id = client.initialize().await;
    assert_eq!(f.host.confirmations.load(Ordering::Acquire), 0);
    let reply = client
        .request("POST", &legacy_headers(&id), &call(None))
        .await;
    assert_eq!(reply.status, 200);
    assert!(reply.json()["result"]["structuredContent"].is_array());
    assert!(!reply.body.contains("user@private"));
    assert_eq!(f.host.confirmations.load(Ordering::Acquire), 1);
    let reply = client
        .request("POST", &modern_headers(), &call(Some("2026-07-28")))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.headers["content-type"].starts_with("text/event-stream"));
    assert!(!reply.headers.contains_key("mcp-session-id"));
    assert_eq!(f.host.confirmations.load(Ordering::Acquire), 1);
    assert!(f.service.activity(10).iter().all(|r| r.entry
        == Entry::Server {
            server_id: "server-id".into()
        }));
    let reply = client
        .request(
            "POST",
            &format!("{AUTH}{ACCEPT}MCP-Protocol-Version: 1900-01-01\r\n"),
            &call(Some("1900-01-01")),
        )
        .await;
    assert_eq!(reply.json()["error"]["code"], -32022, "{}", reply.body);
    assert_eq!(
        client
            .request("DELETE", &legacy_headers(&id), "")
            .await
            .status,
        202
    );
    until(|| f.service.sessions().is_empty()).await;
    entry.close();
    client.closed().await;
}

#[tokio::test]
async fn 内存字节流请求体上限及慢速请求头期限() {
    let f = Fixture::new();
    let entry = MemoryEntry::new(&f.service, "test-entry-token", Duration::from_secs(60));
    let mut body = call(Some("2026-07-28"));
    body.extend(std::iter::repeat_n(' ', 8 * 1024 * 1024 - body.len()));
    let mut client = HttpClient::memory(&entry);
    assert_eq!(
        client
            .request("POST", &modern_headers(), &body)
            .await
            .status,
        200
    );
    body.push(' ');
    let mut client = HttpClient::memory(&entry);
    assert_eq!(
        client
            .request("POST", &modern_headers(), &body)
            .await
            .status,
        413
    );
    let mut client = HttpClient::memory(&entry);
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\n{}Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
        modern_headers(),
        body.len()
    );
    client
        .io
        .get_mut()
        .write_all(request.as_bytes())
        .await
        .unwrap();
    assert_eq!(client.receive().await.status, 413);
    let entry = MemoryEntry::new(&f.service, "test-entry-token", Duration::from_millis(180));
    let mut idle = HttpClient::memory(&entry);
    idle.closed().await;
    let mut slow = HttpClient::memory(&entry);
    slow.io
        .get_mut()
        .write_all(b"POST /mcp HTTP/1.1\r\nHost: ")
        .await
        .unwrap();
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        // 机器忙时睡过头，服务器可能已经按期限关掉连接（正是要验证的）：写失败就不再写
        if slow.io.get_mut().write_all(b"x").await.is_err() {
            break;
        }
    }
    slow.closed().await;
}

#[tokio::test]
async fn 内存字节流会话上限取消与令牌脱敏() {
    let f = Fixture::new();
    let entry = MemoryEntry::new(&f.service, "test-entry-token", Duration::from_secs(60));
    let mut client = HttpClient::memory(&entry);
    let mut ids = Vec::new();
    for _ in 0..8 {
        ids.push(client.initialize().await);
    }
    assert_eq!(f.service.sessions().len(), 8);
    let reply = client
        .request("POST", &modern_headers(), &call(Some("2026-07-28")))
        .await;
    assert!(
        reply.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("最多允许 8 个")
    );
    lock(&f.host.output).stdout = b"test-entry-token".to_vec();
    let command = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run_command","arguments":{"target":"server-id","command":"printf test-entry-token"}}}).to_string();
    let reply = client
        .request("POST", &legacy_headers(&ids[0]), &command)
        .await;
    assert_eq!(reply.json()["result"]["structuredContent"]["stdout"], "***");
    assert!(
        !f.service.activity(1)[0]
            .summary
            .contains("test-entry-token")
    );
    f.host.block.store(true, Ordering::Release);
    client
        .send("POST", &legacy_headers(&ids[0]), &command)
        .await;
    let _ = client.headers().await;
    until(|| f.host.started.load(Ordering::Acquire) == 2).await;
    entry.close();
    until(|| f.host.cancelled.load(Ordering::Acquire) == 1).await;
    until(|| f.service.sessions().is_empty()).await;
    let entry = MemoryEntry::new(&f.service, "rotated-entry-token", Duration::from_secs(60));
    let mut client = HttpClient::memory(&entry);
    assert_eq!(
        client
            .request("POST", &modern_headers(), &call(Some("2026-07-28")))
            .await
            .status,
        401
    );
    assert_eq!(
        client
            .request(
                "POST",
                &legacy_headers(&ids[0]).replace("test-entry-token", "rotated-entry-token"),
                &call(None)
            )
            .await
            .status,
        404
    );
}

impl HttpClient {
    async fn chunk(&mut self) -> Vec<u8> {
        let mut line = String::new();
        self.io.read_line(&mut line).await.unwrap();
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap(), 16).unwrap();
        let mut bytes = vec![0; size];
        self.io.read_exact(&mut bytes).await.unwrap();
        let mut end = [0; 2];
        self.io.read_exact(&mut end).await.unwrap();
        assert_eq!(&end, b"\r\n");
        bytes
    }
}
#[cfg(unix)]
#[tokio::test]
async fn 真实http长命令立即响应持续进度并返回结果() {
    let f = Fixture::new();
    let port = start(&f);
    let mut client = HttpClient::connect(port).await;
    long_command(&mut client).await;
}
#[cfg(unix)]
#[tokio::test]
async fn 内存http长命令立即响应持续进度并返回结果() {
    let f = Fixture::new();
    let entry = MemoryEntry::new(&f.service, "test-entry-token", Duration::from_secs(60));
    let mut client = HttpClient::memory(&entry);
    long_command(&mut client).await;
}
#[cfg(unix)]
fn long_meta() -> Value {
    let mut value = meta("2026-07-28");
    value["progressToken"] = json!("long-command");
    value
}
#[cfg(unix)]
async fn long_command(client: &mut HttpClient) {
    let started = Instant::now();
    client.send("POST",&format!("{AUTH}{ACCEPT}{VERSION}Mcp-Method: tools/call\r\nMcp-Name: run_command\r\n"),&json!({"jsonrpc":"2.0","id":91,"method":"tools/call","params":{"name":"run_command","arguments":{"target":"windows","command":"sleep 90; printf finished","timeout_secs":120},"_meta":long_meta()}}).to_string()).await;
    let headers = tokio::time::timeout(Duration::from_secs(5), client.headers())
        .await
        .unwrap();
    assert_eq!(headers.status, 200);
    assert!(started.elapsed() < Duration::from_secs(60));
    assert!(headers.headers["content-type"].starts_with("text/event-stream"));
    let mut body = String::new();
    let mut progress = 0;
    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(22), client.chunk())
            .await
            .unwrap();
        if chunk.is_empty() {
            break;
        }
        body.push_str(&String::from_utf8(chunk).unwrap());
        let count = body
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["method"] == "notifications/progress")
            .count();
        if count > progress {
            assert!(started.elapsed() < Duration::from_secs(100));
            progress = count;
        }
    }
    assert!(progress >= 4, "进度不足：{body}");
    let result = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["id"] == 91)
        .unwrap();
    assert_eq!(result["result"]["structuredContent"]["stdout"], "finished");
    assert_eq!(result["result"]["structuredContent"]["exit_code"], 0);
}
#[cfg(unix)]
#[tokio::test]
async fn 真实http无进度令牌立即响应且断开结束命令树() {
    for legacy in [false, true] {
        let f = Fixture::new();
        let port = start(&f);
        let client = HttpClient::connect(port).await;
        disconnected_command(client, legacy).await;
    }
}
#[cfg(unix)]
#[tokio::test]
async fn 内存http无进度令牌立即响应且断开结束命令树() {
    for legacy in [false, true] {
        let f = Fixture::new();
        let entry = MemoryEntry::new(&f.service, "test-entry-token", Duration::from_secs(60));
        disconnected_command(HttpClient::memory(&entry), legacy).await;
    }
}
#[cfg(unix)]
async fn disconnected_command(mut client: HttpClient, legacy: bool) {
    let headers = if legacy {
        let id = client.initialize().await;
        legacy_headers(&id)
    } else {
        format!("{AUTH}{ACCEPT}{VERSION}Mcp-Method: tools/call\r\nMcp-Name: run_command\r\n")
    };
    let root = crate::test_support::tempdir();
    let pidfile = root.path().join("pid");
    let command = format!(
        "sleep 120 & echo $! > {}; wait",
        crate::ssh::quote::sh_quote(pidfile.to_str().unwrap()).unwrap()
    );
    let mut params = json!({"name":"run_command","arguments":{"target":"windows","command":command,"timeout_secs":180}});
    if !legacy {
        params["_meta"] = meta("2026-07-28");
    }
    client
        .send(
            "POST",
            &headers,
            &json!({"jsonrpc":"2.0","id":92,"method":"tools/call","params":params}).to_string(),
        )
        .await;
    let reply = tokio::time::timeout(Duration::from_secs(5), client.headers())
        .await
        .unwrap();
    assert_eq!(reply.status, 200);
    let started = Instant::now();
    while !pidfile.exists() {
        assert!(started.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    drop(client);
    let started = Instant::now();
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()));
        if stat.is_err() || stat.unwrap().split_whitespace().nth(2) == Some("Z") {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "断开后进程仍在运行，旧式协议={legacy}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn 后台任务在无握手请求间保留并在入口关闭后结束() {
    let f = Fixture::new();
    let entry = MemoryEntry::new(&f.service, "test-entry-token", Duration::from_secs(60));
    let root = crate::test_support::tempdir();
    let pidfile = root.path().join("pid");
    let command = format!(
        "sleep 90 & echo $! > {}; wait",
        crate::ssh::quote::sh_quote(pidfile.to_str().unwrap()).unwrap()
    );
    let mut client = HttpClient::memory(&entry);
    let headers =
        format!("{AUTH}{ACCEPT}{VERSION}Mcp-Method: tools/call\r\nMcp-Name: run_command\r\n");
    let reply=client.request("POST",&headers,&json!({"jsonrpc":"2.0","id":93,"method":"tools/call","params":{"name":"run_command","arguments":{"target":"windows","command":command,"background":true},"_meta":meta("2026-07-28")}}).to_string()).await;
    let reply = reply.json();
    let id = reply["result"]["structuredContent"]["job_id"]
        .as_str()
        .unwrap();
    drop(client);
    let mut client = HttpClient::memory(&entry);
    let headers =
        format!("{AUTH}{ACCEPT}{VERSION}Mcp-Method: tools/call\r\nMcp-Name: job_output\r\n");
    let reply=client.request("POST",&headers,&json!({"jsonrpc":"2.0","id":94,"method":"tools/call","params":{"name":"job_output","arguments":{"job_id":id},"_meta":meta("2026-07-28")}}).to_string()).await;
    assert_eq!(reply.json()["result"]["structuredContent"]["running"], true);
    let started = Instant::now();
    while !pidfile.exists() {
        assert!(started.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid = std::fs::read_to_string(pidfile).unwrap();
    f.service.stop_http_entry("server-id");
    let started = Instant::now();
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()));
        if stat.is_err() || stat.unwrap().split_whitespace().nth(2) == Some("Z") {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
