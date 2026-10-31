use super::*;

const INPUT: &str = "/api/terminals/term/input";
const IMAGE: &str = "/api/terminals/term/image";

async fn input_device(entry: &MemoryEntry) -> (String, String) {
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
    (id, cookie)
}

async fn upload(entry: &MemoryEntry, cookie: &str, content_type: &str, bytes: &[u8]) -> Reply {
    let mut client = entry.connect();
    let headers = format!(
        "POST {IMAGE} HTTP/1.1\r\nHost: pc.example.ts.net\r\nX-Pierlink: 1\r\nCookie: {cookie}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
        bytes.len()
    );
    client
        .io
        .get_mut()
        .write_all(headers.as_bytes())
        .await
        .unwrap();
    // 较大的拒绝请求可能提前关连接；仍然读取已经返回的 HTTP 错误。
    let _ = client.io.get_mut().write_all(bytes).await;
    client.receive().await
}

#[tokio::test]
async fn 编辑后只确认屏幕上的危险命令且读屏失败仍确认() {
    let entry = MemoryEntry::new();
    let (_, cookie) = input_device(&entry).await;
    for key in [
        "ArrowUp",
        "ArrowDown",
        "ArrowLeft",
        "ArrowRight",
        "Tab",
        "Home",
        "End",
    ] {
        *lock(&entry.host.screen) = "选择权限\n> 允许本次\n \n".into();
        assert_eq!(
            entry
                .request("POST", INPUT, &cookie, json!({"keys":[key,"Enter"]}))
                .await
                .status,
            200
        );
        for prompt in [
            "$ ",
            "user@host:~ $ ",
            "user@host:~$ ",
            "root@host:~# ",
            "root # ",
            "> ",
            "% ",
            "host % ",
            "PS C:\\work> ",
        ] {
            assert_eq!(
                entry
                    .request("POST", INPUT, &cookie, json!({"keys":[key]}))
                    .await
                    .status,
                200
            );
            *lock(&entry.host.screen) = format!("先前输出\n{prompt}rm -rf /\n\n");
            let reply = entry
                .request("POST", INPUT, &cookie, json!({"keys":["Enter"]}))
                .await;
            assert_eq!(reply.status, 409, "{key} {prompt}");
            assert_eq!(reply.json()["lines"], json!(["rm -rf /"]));
            assert_eq!(
                entry
                    .request(
                        "POST",
                        INPUT,
                        &cookie,
                        json!({"keys":["Enter"],"confirmed":true})
                    )
                    .await
                    .status,
                200
            );
        }
    }
    entry
        .request("POST", INPUT, &cookie, json!({"keys":["ArrowUp"]}))
        .await;
    entry.host.read_error.store(1, Ordering::SeqCst);
    let reply = entry
        .request("POST", INPUT, &cookie, json!({"keys":["Enter"]}))
        .await;
    assert_eq!(reply.status, 409);
    assert!(
        reply.json()["lines"][0]
            .as_str()
            .unwrap()
            .contains("无法读取")
    );
    assert_eq!(
        entry
            .request(
                "POST",
                INPUT,
                &cookie,
                json!({"keys":["Enter"],"confirmed":true})
            )
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn 反向制表和退出键保留未提交命令且默认不是粘贴() {
    let input: TerminalInput = serde_json::from_value(json!({"text":"a"})).unwrap();
    assert!(!input.paste);
    let entry = MemoryEntry::new();
    let (_, cookie) = input_device(&entry).await;
    entry
        .request("POST", INPUT, &cookie, json!({"text":"rm -rf /"}))
        .await;
    for key in ["Shift+Tab", "Escape", "Ctrl+D"] {
        assert_eq!(
            entry
                .request("POST", INPUT, &cookie, json!({"keys":[key]}))
                .await
                .status,
            200
        );
    }
    let reply = entry
        .request("POST", INPUT, &cookie, json!({"keys":["Enter"]}))
        .await;
    assert_eq!(reply.status, 409);
    assert_eq!(reply.json()["lines"], json!(["rm -rf /"]));
}

#[tokio::test]
async fn 多行粘贴整段发送并兼容独立回车和末尾回车() {
    let entry = MemoryEntry::new();
    let (_, cookie) = input_device(&entry).await;
    let body = "请解释这段代码\n\t包含第二行\n";
    assert_eq!(
        entry
            .request("POST", INPUT, &cookie, json!({"text":body,"paste":true}))
            .await
            .status,
        200
    );
    let calls = lock(&entry.host.calls).clone();
    let inputs: Vec<_> = calls.iter().filter(|v| v[0] == "input").collect();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0][2]["text"], body);
    assert_eq!(inputs[0][2]["paste"], true);
    assert!(lock(&entry.server.shared.pending_lines).contains_key("term"));
    assert_eq!(
        entry
            .request("POST", INPUT, &cookie, json!({"keys":["Enter"]}))
            .await
            .status,
        200
    );
    assert!(!lock(&entry.server.shared.pending_lines).contains_key("term"));
    for suffix in ["", "\r"] {
        let text = format!("echo ok\nrm -rf /\nreboot{suffix}");
        let reply = entry
            .request("POST", INPUT, &cookie, json!({"text":text,"paste":true}))
            .await;
        assert_eq!(reply.status, 409);
        assert_eq!(reply.json()["lines"], json!(["rm -rf /", "reboot"]));
        assert_eq!(
            entry
                .request(
                    "POST",
                    INPUT,
                    &cookie,
                    json!({"text":text,"paste":true,"confirmed":true})
                )
                .await
                .status,
            200
        );
        if suffix.is_empty() {
            assert_eq!(
                entry
                    .request("POST", INPUT, &cookie, json!({"keys":["Enter"]}))
                    .await
                    .status,
                409
            );
            entry
                .request(
                    "POST",
                    INPUT,
                    &cookie,
                    json!({"keys":["Enter"],"confirmed":true}),
                )
                .await;
        }
        assert!(!lock(&entry.server.shared.pending_lines).contains_key("term"));
    }
    entry
        .request("POST", INPUT, &cookie, json!({"text":"rm "}))
        .await;
    assert_eq!(
        entry
            .request(
                "POST",
                INPUT,
                &cookie,
                json!({"text":"-rf /\n","paste":true})
            )
            .await
            .status,
        409
    );
}

#[tokio::test]
async fn 粘贴校验字段控制字符和密码提示() {
    let entry = MemoryEntry::new();
    let (_, cookie) = input_device(&entry).await;
    for body in [
        json!({"keys":["Enter"],"paste":true}),
        json!({"paste":true}),
        json!({"text":"","paste":true}),
        json!({"text":"a\rb","paste":true}),
        json!({"text":"a\u{1b}[201~","paste":true}),
        json!({"text":"a\u{0}","paste":true}),
    ] {
        let reply = entry.request("POST", INPUT, &cookie, body).await;
        assert_eq!(reply.status, 400);
        assert!(reply.json()["params"]["reason"].is_string());
    }
    *lock(&entry.host.screen) = "Password: ".into();
    assert_eq!(
        entry
            .request(
                "POST",
                INPUT,
                &cookie,
                json!({"text":"正文\n第二行","paste":true,"confirmed":true})
            )
            .await
            .status,
        403
    );
}

#[tokio::test]
async fn 屏幕行数有默认值上下限且非法值不交宿主() {
    let entry = MemoryEntry::new();
    let (_, cookie) = entry.pair().await;
    for (query, lines) in [("", 300), ("?lines=1", 1), ("?lines=2000", 2000)] {
        let reply = entry
            .request(
                "GET",
                &format!("/api/terminals/term/screen{query}"),
                &cookie,
                json!(null),
            )
            .await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.json()["lines"], lines);
    }
    for query in [
        "lines=0",
        "lines=2001",
        "lines=-1",
        "lines=one",
        "lines=",
        "limit=1",
        "lines=1&lines=2",
        "lines=999999999999999999999",
    ] {
        let reply = entry
            .request(
                "GET",
                &format!("/api/terminals/term/screen?{query}"),
                &cookie,
                json!(null),
            )
            .await;
        assert_eq!(reply.status, 400, "{query}");
        assert!(reply.json()["params"]["reason"].is_string());
    }
}

#[tokio::test]
async fn 图片按类型交给宿主且审计只含大小() {
    let entry = MemoryEntry::new();
    let (_, cookie) = input_device(&entry).await;
    for (kind, body, ext) in [
        ("image/png", b"\x89PNG\r\n\x1a\n".as_slice(), "png"),
        ("image/jpeg", b"\xff\xd8\xff", "jpg"),
        ("image/webp", b"RIFF\x04\x00\x00\x00WEBP", "webp"),
        ("image/gif", b"GIF87a", "gif"),
        ("image/gif", b"GIF89a", "gif"),
    ] {
        let reply = upload(&entry, &cookie, kind, body).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!(
            lock(&entry.host.calls).last().unwrap(),
            &json!(["image", "term", body.len(), ext])
        );
        let audits = lock(&entry.host.audits);
        let audit = audits.last().unwrap();
        assert_eq!(audit.action, "terminal_image");
        assert_eq!(audit.bytes, Some(body.len()));
        assert_eq!(audit.target.as_deref(), Some("term"));
        assert_eq!(audit.result, "ok");
        let serialized = serde_json::to_string(audit).unwrap();
        assert!(!serialized.contains("GIF"));
        assert!(!serialized.contains(&cookie));
    }
}

#[tokio::test]
async fn 图片拒绝未授权设备密码状态和错误文件头() {
    let entry = MemoryEntry::new();
    let (id, cookie) = entry.pair().await;
    assert_eq!(
        upload(&entry, &cookie, "image/gif", b"GIF89a").await.status,
        403
    );
    assert_eq!(upload(&entry, "", "image/gif", b"GIF89a").await.status, 401);
    entry
        .server
        .set_permissions(
            &id,
            Permissions {
                input: true,
                view: false,
                confirm: false,
            },
        )
        .unwrap();
    for (kind, bytes, status, code) in [
        (
            "text/plain",
            b"GIF89a".as_slice(),
            415,
            "mobile.unsupported_image_type",
        ),
        ("image/png", b"GIF89a", 400, "mobile.invalid_image"),
        ("image/webp", b"RIFF", 400, "mobile.invalid_image"),
        ("image/jpeg", b"", 400, "mobile.invalid_image"),
        ("image/gif", b"GIF", 400, "mobile.invalid_image"),
    ] {
        let reply = upload(&entry, &cookie, kind, bytes).await;
        assert_eq!(reply.status, status);
        assert_eq!(reply.json()["code"], code);
        assert!(reply.json()["params"]["reason"].is_string());
    }
    *lock(&entry.host.screen) = "Enter verification code: ".into();
    assert_eq!(
        upload(&entry, &cookie, "image/gif", b"GIF89a").await.status,
        403
    );
    entry.host.read_error.store(1, Ordering::SeqCst);
    assert_eq!(
        upload(&entry, &cookie, "image/gif", b"GIF89a").await.status,
        500
    );
    assert!(
        !lock(&entry.host.calls)
            .iter()
            .any(|call| call[0] == "image")
    );
}

#[tokio::test]
async fn 图片限额按设备累计且其他设备不受影响() {
    let entry = MemoryEntry::new();
    let (_, cookie) = input_device(&entry).await;
    for _ in 0..10 {
        assert_eq!(
            upload(&entry, &cookie, "image/gif", b"GIF89a").await.status,
            200
        );
    }
    let reply = upload(&entry, &cookie, "image/gif", b"GIF89a").await;
    assert_eq!(reply.status, 429);
    assert_eq!(reply.json()["code"], "mobile.rate_limited");
    let (_, other) = input_device(&entry).await;
    assert_eq!(
        upload(&entry, &other, "image/gif", b"GIF89a").await.status,
        200
    );
}

#[tokio::test]
async fn 只有图片路由允许八兆且普通输入仍限六十四kib() {
    let entry = MemoryEntry::new();
    let (_, cookie) = input_device(&entry).await;
    let mut bytes = vec![0; image::MAX_IMAGE_BODY];
    bytes[..6].copy_from_slice(b"GIF89a");
    assert_eq!(
        upload(&entry, &cookie, "image/gif", &bytes).await.status,
        200
    );
    for (path, size) in [(IMAGE, image::MAX_IMAGE_BODY + 1), (INPUT, 64 * 1024 + 1)] {
        let mut client = entry.connect();
        client.io.get_mut().write_all(format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nX-Pierlink: 1\r\nCookie: {cookie}\r\nContent-Type: image/gif\r\nContent-Length: {size}\r\n\r\n").as_bytes()).await.unwrap();
        let reply = client.receive().await;
        assert_eq!(reply.status, 413);
        assert_eq!(reply.json()["code"], "mobile.body_too_large");
        assert!(reply.json()["params"]["reason"].is_string());
    }
}

#[tokio::test]
async fn 图片分块请求也受八兆限制() {
    let entry = MemoryEntry::new();
    let (_, cookie) = input_device(&entry).await;
    let mut client = entry.connect();
    client.io.get_mut().write_all(format!("POST {IMAGE} HTTP/1.1\r\nHost: localhost\r\nX-Pierlink: 1\r\nCookie: {cookie}\r\nContent-Type: image/gif\r\nTransfer-Encoding: chunked\r\n\r\n").as_bytes()).await.unwrap();
    let mut chunk = vec![b'x'; 64 * 1024];
    chunk[..6].copy_from_slice(b"GIF89a");
    for _ in 0..128 {
        client.io.get_mut().write_all(b"10000\r\n").await.unwrap();
        client.io.get_mut().write_all(&chunk).await.unwrap();
        client.io.get_mut().write_all(b"\r\n").await.unwrap();
    }
    let _ = client.io.get_mut().write_all(b"1\r\nx\r\n0\r\n\r\n").await;
    let reply = client.receive().await;
    assert_eq!(reply.status, 413);
    assert_eq!(
        reply.json()["params"]["limit"],
        image::MAX_IMAGE_BODY.to_string()
    );
    assert!(
        !lock(&entry.host.calls)
            .iter()
            .any(|call| call[0] == "image")
    );
}

#[tokio::test]
async fn 图片缺少或重复内容类型时拒绝且停止错误带中文原因() {
    let entry = MemoryEntry::new();
    let (_, cookie) = input_device(&entry).await;
    let reply = entry.request("POST", IMAGE, &cookie, json!(null)).await;
    assert_eq!(reply.status, 415);
    assert_eq!(
        upload(
            &entry,
            &cookie,
            "image/gif\r\nContent-Type: image/gif",
            b"GIF89a"
        )
        .await
        .status,
        415
    );
    entry.server.stop();
    let stopped = entry.server.create_invite().err().unwrap();
    assert_eq!(stopped.code, "mobile.stopped");
    assert_eq!(stopped.params["reason"], "手机入口已停止");
}
