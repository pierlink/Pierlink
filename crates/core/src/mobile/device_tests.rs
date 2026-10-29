use super::*;

#[tokio::test]
async fn 配对记录浏览器并且只使用实际来源地址() {
    let entry = MemoryEntry::new();
    let invite = entry.server.create_invite().unwrap();
    let agent = format!(" Browser\t/{}  ", "机".repeat(300));
    let response = entry.connect().request(
        "POST", "/api/pair",
        &format!("X-Pierlink: 1\r\nUser-Agent: {agent}\r\nX-Forwarded-For: 8.8.8.8\r\nForwarded: for=8.8.4.4\r\n"),
        &json!({"code":invite.code,"device_name":"手机"}).to_string(),
    ).await;
    assert_eq!(response.status, 200);
    let device = entry.server.list_devices().unwrap().remove(0);
    assert_eq!(
        device.user_agent,
        Some(format!("Browser/{}", "机".repeat(248)))
    );
    assert_eq!(device.last_ip.as_deref(), Some("100.64.0.2"));

    let (id, _) = entry.pair().await;
    let device = entry
        .server
        .list_devices()
        .unwrap()
        .into_iter()
        .find(|d| d.id == id)
        .unwrap();
    assert_eq!(device.user_agent, None);
    assert_eq!(device.last_ip.as_deref(), Some("100.64.0.2"));
}

#[tokio::test]
async fn 空白或清理后为空的浏览器标识不保存空字符串() {
    let entry = MemoryEntry::new();
    for agent in ["", " \t ", "\u{85}"] {
        let invite = entry.server.create_invite().unwrap();
        let response = entry
            .connect()
            .request(
                "POST",
                "/api/pair",
                &format!("X-Pierlink: 1\r\nUser-Agent: {agent}\r\n"),
                &json!({"code":invite.code,"device_name":"手机"}).to_string(),
            )
            .await;
        assert_eq!(response.status, 200);
        let id = response.json()["id"].as_str().unwrap().to_owned();
        let device = entry
            .server
            .list_devices()
            .unwrap()
            .into_iter()
            .find(|d| d.id == id)
            .unwrap();
        assert_eq!(device.user_agent, None);
    }
}

#[tokio::test]
async fn 活跃信息同来源节流而换网络立即写入且失败认证不更新() {
    let entry = MemoryEntry::new();
    let (id, cookie) = entry.pair().await;
    let at = now() - 10;
    entry.sql("UPDATE mobile_devices SET last_active_at=?1", [at]);
    entry.sql("CREATE TABLE writes (id TEXT)", []);
    entry.sql("CREATE TRIGGER count_writes AFTER UPDATE ON mobile_devices BEGIN INSERT INTO writes VALUES (NEW.id); END", []);
    let writes = || {
        rusqlite::Connection::open(entry.root.path().join("db"))
            .unwrap()
            .query_row("SELECT count(*) FROM writes", [], |r| r.get::<_, u32>(0))
            .unwrap()
    };
    let device = || {
        entry
            .server
            .list_devices()
            .unwrap()
            .into_iter()
            .find(|d| d.id == id)
            .unwrap()
    };
    for _ in 0..2 {
        assert_eq!(
            entry
                .request("GET", "/api/me", &cookie, json!(null))
                .await
                .status,
            200
        );
    }
    assert_eq!(writes(), 0);
    assert_eq!(device().last_active_at, at);

    let mut client = entry.peer("fd7a:115c:a1e0::2".parse().unwrap());
    let reply = client
        .request(
            "GET",
            "/api/me",
            &format!("Cookie: {cookie}\r\nX-Forwarded-For: 8.8.8.8\r\n"),
            "",
        )
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(writes(), 1);
    assert_eq!(device().last_ip.as_deref(), Some("fd7a:115c:a1e0::2"));
    assert!(device().last_active_at > at);

    assert_eq!(
        entry
            .request("GET", "/api/me", "pl_device=bad", json!(null))
            .await
            .status,
        401
    );
    assert_eq!(writes(), 1);
    assert_eq!(device().last_ip.as_deref(), Some("fd7a:115c:a1e0::2"));

    entry.sql(
        "UPDATE mobile_devices SET last_active_at=?1",
        [now() - auth::TOUCH_INTERVAL_SECONDS],
    );
    entry.sql("DELETE FROM writes", []);
    assert_eq!(
        client
            .request("GET", "/api/me", &format!("Cookie: {cookie}\r\n"), "")
            .await
            .status,
        200
    );
    assert_eq!(writes(), 1);
    assert!(device().last_active_at >= at);

    entry.sql("UPDATE mobile_devices SET last_ip=NULL", []);
    entry.sql("DELETE FROM writes", []);
    assert_eq!(
        client
            .request("GET", "/api/me", &format!("Cookie: {cookie}\r\n"), "")
            .await
            .status,
        200
    );
    assert_eq!(writes(), 1);
    assert_eq!(device().last_ip.as_deref(), Some("fd7a:115c:a1e0::2"));
}

#[tokio::test]
async fn 改名共用配对校验且入口关闭后仍可修改() {
    let entry = MemoryEntry::new();
    let (id, cookie) = entry.pair().await;
    let original = entry.server.list_devices().unwrap().remove(0);
    entry.server.rename_device(&id, "  工作手机  ").unwrap();
    let mut expected = original.clone();
    expected.name = "工作手机".into();
    assert_eq!(entry.server.list_devices().unwrap()[0], expected);
    assert_eq!(
        entry
            .request("GET", "/api/me", &cookie, json!(null))
            .await
            .json()["name"],
        "工作手机"
    );
    let db = lock(&entry.server.shared.state).db.clone();
    entry.server.stop();
    let db = lock(&db);
    let longest = "机".repeat(80);
    rename_in(&db, &id, &longest).unwrap();
    assert_eq!(devices_in(&db).unwrap()[0].name, longest);
    for name in [
        "".into(),
        "  \t\n".into(),
        "机".repeat(81),
        "手机\0".into(),
        "手\n机".into(),
        "手\u{7f}机".into(),
    ] {
        assert_eq!(
            rename_in(&db, &id, &name).unwrap_err().code,
            "mobile.invalid_request"
        );
        assert_eq!(
            device_name(&name).unwrap_err().code,
            "mobile.invalid_request"
        );
    }
    assert_eq!(devices_in(&db).unwrap()[0].name, longest);
    assert_eq!(
        rename_in(&db, "missing", "手机").unwrap_err().code,
        "storage.not_found"
    );
    revoke_in(&db, Some(&id)).unwrap();
    assert_eq!(
        rename_in(&db, &id, "手机").unwrap_err().code,
        "storage.not_found"
    );
    drop(db);
    assert_eq!(
        entry.server.rename_device(&id, "手机").unwrap_err().code,
        "storage.not_found"
    );
    assert_eq!(
        entry
            .server
            .rename_device("missing", "手机")
            .unwrap_err()
            .code,
        "storage.not_found"
    );
    assert_eq!(
        entry.server.rename_device(&id, "").unwrap_err().code,
        "mobile.invalid_request"
    );
}

#[tokio::test]
async fn 取消邀请使未使用邀请码失效且可重复取消和重新邀请() {
    let entry = MemoryEntry::new();
    let invite = entry.server.create_invite().unwrap();
    entry.server.cancel_invites();
    entry.server.cancel_invites();
    let reply = entry.pair_code(&invite.code).await;
    assert_eq!(reply.status, 401);
    assert_eq!(reply.json()["code"], "mobile.invalid_invite");
    assert_eq!(entry.host.approvals.load(Ordering::SeqCst), 0);
    entry.pair().await;
}

#[tokio::test]
async fn 取消未用邀请不影响已经等待批准的配对() {
    let entry = MemoryEntry::new();
    entry.host.approval.store(3, Ordering::SeqCst);
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
    tokio::time::timeout(Duration::from_secs(3), async {
        while entry.host.approvals.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    entry.server.cancel_invites();
    entry.host.approve_ready.notify_one();
    assert_eq!(client.receive().await.status, 200);
    assert_eq!(entry.server.list_devices().unwrap().len(), 1);
}

#[tokio::test]
async fn 网页请求传递多行编码协商并保留安全头() {
    use std::io::{Read, Write};
    let entry = MemoryEntry::new();
    let plain = b"console.log(1)";
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(plain).unwrap();
    let compressed = encoder.finish().unwrap();
    std::fs::write(
        entry.root.path().join("assets/app-a1b2c3d4.js.gz"),
        &compressed,
    )
    .unwrap();
    for (headers, accepts_gzip) in [
        (
            "Accept-Encoding: br\r\nAccept-Encoding: gzip;q=0.8\r\n",
            true,
        ),
        ("Accept-Encoding: gzip;q=0\r\n", false),
        ("", false),
    ] {
        let mut client = entry.connect();
        client.send("GET", "/app-a1b2c3d4.js", headers, "").await;
        let reply = client.headers().await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.headers["vary"], "Accept-Encoding");
        assert_eq!(reply.headers["x-content-type-options"], "nosniff");
        assert_eq!(reply.headers.contains_key("content-encoding"), accepts_gzip);
        let mut bytes = vec![0; reply.headers["content-length"].parse().unwrap()];
        client.io.read_exact(&mut bytes).await.unwrap();
        if accepts_gzip {
            assert_eq!(bytes, compressed);
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(&bytes[..])
                .read_to_end(&mut decoded)
                .unwrap();
            assert_eq!(decoded, plain);
        } else {
            assert_eq!(bytes, plain);
        }
    }
}
