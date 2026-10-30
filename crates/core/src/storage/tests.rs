use super::*;
use std::fs;

#[test]
fn v1逐级到当前版本按来源迁移且重复打开保留用户选择() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("upgrade.db");
    let mut conn = Connection::open(&path).unwrap();
    let steps = [
        M::up(include_str!("v1.sql")),
        M::up(include_str!("v2.sql")),
        M::up(include_str!("v3.sql")),
    ];
    for version in 1..=3 {
        Migrations::new(steps[..version].to_vec())
            .to_latest(&mut conn)
            .unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            version as u32
        );
        if version == 1 {
            conn.execute_batch("INSERT INTO servers(id,name,source,host,username,auth_method,created_at,updated_at) VALUES ('manual','手动','manual','example.test','dev','agent',1,2);
                INSERT INTO servers(id,name,source,ssh_alias,auth_method,created_at,updated_at) VALUES ('imported','导入','ssh_config','dev','agent',3,4);").unwrap();
        }
    }
    drop(conn);
    let db = open(&path).unwrap();
    assert_eq!(
        db.conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        9
    );
    let mut manual = db.get_server("manual").unwrap().unwrap();
    assert_eq!(manual.connection_engine, Engine::Builtin);
    assert_eq!(manual.engine_note, None);
    assert_eq!((manual.created_at, manual.updated_at), (1, 2));
    let imported = db.get_server("imported").unwrap().unwrap();
    assert_eq!(imported.connection_engine, Engine::OpenSsh);
    assert_eq!(
        imported.engine_note.as_deref(),
        Some("从 ssh config 导入，按原配置连接")
    );
    assert_eq!(imported.ssh_alias.as_deref(), Some("dev"));
    assert_eq!((imported.created_at, imported.updated_at), (3, 4));
    manual.connection_engine = Engine::OpenSsh;
    manual.engine_note = Some("内置连接失败后改用".into());
    db.update_server(&manual).unwrap();
    db.backup_on(&root.path().join("backups"), "20261025")
        .unwrap();
    let backup = open(&root.path().join("backups/pierlink-20261025.db")).unwrap();
    assert_eq!(backup.get_server("manual").unwrap(), Some(manual.clone()));
    assert_eq!(backup.get_server("imported").unwrap(), Some(imported));
    drop(db);
    assert_eq!(
        open(&path).unwrap().get_server("manual").unwrap(),
        Some(manual)
    );
}

#[test]
fn 引擎文本枚举序列化与数据库约束() {
    let root = crate::test_support::tempdir();
    let db = open(&root.path().join("engines.db")).unwrap();
    for (index, (engine, text)) in [(Engine::Builtin, "builtin"), (Engine::OpenSsh, "openssh")]
        .into_iter()
        .enumerate()
    {
        let mut record = server(&index.to_string(), "服务器");
        record.connection_engine = engine;
        record.engine_note = Some("连接方式说明".into());
        db.insert_server(&record).unwrap();
        assert_eq!(db.get_server(&record.id).unwrap(), Some(record));
        let value = serde_json::to_string(&engine).unwrap();
        assert_eq!(value, format!("\"{text}\""));
        assert_eq!(serde_json::from_str::<Engine>(&value).unwrap(), engine);
    }
    assert!(serde_json::from_str::<Engine>("\"unknown\"").is_err());
    for value in [Some("unknown"), None] {
        assert!(
            db.conn
                .execute(
                    "UPDATE servers SET connection_engine=?1 WHERE id='0'",
                    [value]
                )
                .is_err()
        );
    }
    // 数据库本身默认保守地选系统 OpenSSH；应用新建流程另行默认内置。
    db.conn.execute("INSERT INTO servers(id,name,source,host,auth_method,created_at,updated_at) VALUES ('default','默认','manual','example.test','agent',1,2)", []).unwrap();
    assert_eq!(
        db.get_server("default").unwrap().unwrap().connection_engine,
        Engine::OpenSsh
    );
}

#[test]
fn f21_任务和端点同时更新且约束错误完整回滚() {
    let root = crate::test_support::tempdir();
    let mut db = open(&root.path().join("db")).unwrap();
    db.insert_server(&server("a", "甲")).unwrap();
    db.insert_server(&server("b", "乙")).unwrap();
    let original = task("t", "C:/work/project");
    let endpoints = vec![endpoint("t", "a")];
    db.insert_sync_task(&original, &endpoints).unwrap();
    let mut updated = original.clone();
    updated.name = "新名称".into();
    updated.watch_delay_s = 3;
    let new_endpoints = vec![endpoint("t", "b")];
    db.update_sync_task_with_endpoints(&updated, &new_endpoints)
        .unwrap();
    let (saved, saved_endpoints) = db.get_sync_task("t").unwrap().unwrap();
    assert_eq!(saved.name, "新名称");
    assert_eq!(saved.watch_delay_s, 3);
    assert_eq!(saved_endpoints[0].server_id, "b");
    updated.name = "不得保存".into();
    let bad = vec![endpoint("t", "a"), endpoint("t", "missing")];
    assert_eq!(
        db.update_sync_task_with_endpoints(&updated, &bad)
            .unwrap_err()
            .code,
        "storage.constraint"
    );
    let (saved, saved_endpoints) = db.get_sync_task("t").unwrap().unwrap();
    assert_eq!(saved.name, "新名称");
    assert_eq!(saved_endpoints.len(), 1);
    assert_eq!(saved_endpoints[0].server_id, "b");
    assert!(db.update_sync_task_with_endpoints(&updated, &[]).is_err());
    let mut missing = updated.clone();
    missing.id = "missing".into();
    assert_eq!(
        db.update_sync_task_with_endpoints(&missing, &[endpoint("missing", "a")])
            .unwrap_err()
            .code,
        "storage.not_found"
    );
    db.insert_sync_task(&task("other", "C:/other"), &[endpoint("other", "a")])
        .unwrap();
    updated.local_path = "C:/other/nested".into();
    assert!(
        db.update_sync_task_with_endpoints(&updated, &new_endpoints)
            .is_err()
    );
}

fn server(id: &str, name: &str) -> Server {
    Server {
        id: id.into(),
        name: name.into(),
        group_name: Some("研发".into()),
        favorite: true,
        sort_order: 0,
        source: ServerSource::Manual,
        connection_engine: Engine::Builtin,
        engine_note: None,
        ssh_alias: None,
        host: Some("example.test".into()),
        port: Some(2222),
        username: Some("开发者".into()),
        auth_method: AuthMethod::Password,
        identity_file: Some("C:/Users/开发者/.ssh/id_ed25519".into()),
        identity_files: vec![],
        identities_only: false,
        host_key_alias: None,
        jump_server_id: None,
        login_shell: Some("/bin/bash".into()),
        command_prediction: true,
        use_tmux: true,
        remember_password: true,
        password_cred: Some(format!("pierlink/server/{id}/password")),
        host_key_sha256: Some("SHA256:指纹".into()),
        vnc_autostart: true,
        notes: "普通备注".into(),
        created_at: 1_700_000_000_000,
        updated_at: 1_700_000_000_123,
    }
}

fn task(id: &str, path: &str) -> SyncTask {
    SyncTask {
        id: id.into(),
        folder_id: format!("pl-{id}"),
        name: format!("任务{id}"),
        local_path: path.into(),
        local_mode: EndpointMode::SendReceive,
        conflict_policy: ConflictPolicy::Ask,
        collab_mode: false,
        watch_delay_s: 10,
        versioning_enabled: true,
        versioning_days: 30,
        ignore_rules: "(?d).git\n.env".into(),
        editor_id: None,
        paused: true,
        created_at: 1_700_000_000_000,
        updated_at: 1_700_000_000_999,
    }
}

fn endpoint(task: &str, server: &str) -> SyncEndpoint {
    SyncEndpoint {
        task_id: task.into(),
        server_id: server.into(),
        remote_path: "/home/user/project".into(),
        mode: EndpointMode::SendReceive,
    }
}

// 所有涉及文件的场景集中在一次测试的同一个 TempDir 内；结束时整体自动清理。
#[test]
fn 数据库全部公共接口及异常约束() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("nested/pierlink.db");
    let mut db = open(&path).unwrap();
    assert_eq!(
        db.conn
            .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
    assert_eq!(
        db.conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        5000
    );
    assert_eq!(
        db.conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        9
    );
    let tables: i64 = db
        .conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 12);
    // 建表语句要和接口约定文档一致；公开仓库不含开发文档，没有时跳过
    let doc = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/接口约定.md");
    match std::fs::read_to_string(&doc) {
        Ok(doc) => assert_eq!(
            include_str!("v1.sql"),
            doc.split("```sql\n")
                .nth(1)
                .unwrap()
                .split("```")
                .next()
                .unwrap()
        ),
        Err(_) => eprintln!("跳过建表语句与文档的核对：没有 docs/接口约定.md"),
    }
    服务器读写和级联(&mut db);
    任务事务与重叠(&mut db);
    设置及秘密保护(&mut db);
    let short_id = db.device_short_id().unwrap();
    drop(db);
    let mut db = open(&path).unwrap();
    assert_eq!(db.device_short_id().unwrap(), short_id);
    每日备份和保留(&db, root.path());
    不支持硬链接时回退(root.path());
    drop(db);
    损坏检测(root.path());
    并发路径检查和设备标识(root.path());
}

fn 服务器读写和级联(db: &mut Db) {
    assert!(db.get_server("missing").unwrap().is_none());
    let mut a = server("a", "B");
    db.insert_server(&a).unwrap();
    assert_eq!(db.get_server("a").unwrap(), Some(a.clone()));
    let b = server("b", "A");
    db.insert_server(&b).unwrap();
    assert_eq!(db.list_servers().unwrap(), vec![b.clone(), a.clone()]);
    a.name = "修改".into();
    a.sort_order = -1;
    a.source = ServerSource::SshConfig;
    a.ssh_alias = Some("alias".into());
    a.connection_engine = Engine::OpenSsh;
    a.engine_note = Some("保留原 SSH 配置".into());
    a.auth_method = AuthMethod::DedicatedKey;
    a.host = None;
    a.updated_at += 1;
    db.update_server(&a).unwrap();
    assert_eq!(db.list_servers().unwrap(), vec![a.clone(), b]);
    assert_eq!(
        db.update_server(&server("missing", "无")).unwrap_err().code,
        "storage.not_found"
    );
    let mut invalid = server("invalid", "无");
    invalid.host = None;
    assert_eq!(
        db.insert_server(&invalid).unwrap_err().code,
        "storage.constraint"
    );
    db.conn.execute_batch("INSERT INTO server_instances VALUES ('a','实例','2.1','x86_64','/state','/socket',22000,'设备','pierlink/instance/a/apikey','pierlink/instance/a/gui-password',1,1,2);
        INSERT INTO project_paths VALUES ('项目','a','/project',NULL);
        INSERT INTO port_forwards(id,server_id,name,kind,local_port,remote_host,remote_port,enabled) VALUES ('转发','a','转发','local',8080,'localhost',80,0);
        UPDATE servers SET jump_server_id='a' WHERE id='b';").unwrap();
    db.delete_server("a").unwrap();
    assert!(db.get_server("a").unwrap().is_none());
    assert!(
        db.get_server("b")
            .unwrap()
            .unwrap()
            .jump_server_id
            .is_none()
    );
    for table in ["server_instances", "project_paths", "port_forwards"] {
        assert_eq!(
            db.conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        db.delete_server("missing").unwrap_err().code,
        "storage.not_found"
    );
}

fn 任务事务与重叠(db: &mut Db) {
    let mut t = task("1", "C:/Work/./工程/../项目/");
    let e = endpoint("1", "b");
    db.insert_sync_task(&t, std::slice::from_ref(&e)).unwrap();
    t.local_path = "c:/work/项目".into();
    assert_eq!(
        db.get_sync_task("1").unwrap(),
        Some((t.clone(), vec![e.clone()]))
    );
    assert_eq!(db.list_sync_tasks().unwrap(), vec![t.clone()]);
    let error = db.delete_server("b").unwrap_err();
    assert_eq!(error.code, "storage.server_in_use");
    assert_eq!(error.params["task_names"], "[\"任务1\"]");
    assert!(
        db.conn
            .execute("DELETE FROM servers WHERE id='b'", [])
            .is_err()
    );
    for path in ["C:\\WORK\\项目", "c:/work", "C:/Work/项目/src"] {
        let error = db
            .insert_sync_task(&task("2", path), &[endpoint("2", "b")])
            .unwrap_err();
        assert_eq!(error.code, "storage.local_path_overlap");
        assert_eq!(error.params["task_name"], t.name);
    }
    let mut other = task("2", "/projects/other");
    db.insert_sync_task(&other, &[endpoint("2", "b")]).unwrap();
    other.local_path = "C:/Work/项目/sub".into();
    assert_eq!(
        db.update_sync_task(&other).unwrap_err().code,
        "storage.local_path_overlap"
    );
    assert_eq!(
        db.get_sync_task("2").unwrap().unwrap().0.local_path,
        "/projects/other"
    );
    t.conflict_policy = ConflictPolicy::Newest;
    t.collab_mode = true;
    t.watch_delay_s = 1;
    t.paused = false;
    t.updated_at += 1;
    db.update_sync_task(&t).unwrap();
    assert_eq!(db.get_sync_task("1").unwrap().unwrap().0, t);
    assert_eq!(
        db.update_sync_task(&task("missing", "/not-found"))
            .unwrap_err()
            .code,
        "storage.not_found"
    );
    let bad = task("3", "/projects/failed");
    assert!(
        db.insert_sync_task(&bad, &[endpoint("3", "b"), endpoint("3", "missing")])
            .is_err()
    );
    assert!(db.get_sync_task("3").unwrap().is_none());
    assert!(db.insert_sync_task(&bad, &[]).is_err());
    assert!(
        db.insert_sync_task(&bad, &[endpoint("wrong", "b")])
            .is_err()
    );
    assert!(
        db.insert_sync_task(&task("3", "relative"), &[endpoint("3", "b")])
            .is_err()
    );
    assert!(
        db.set_endpoints("1", &[e.clone(), endpoint("1", "missing")])
            .is_err()
    );
    assert_eq!(db.get_sync_task("1").unwrap().unwrap().1, vec![e.clone()]);
    assert!(db.set_endpoints("1", &[e.clone(), e.clone()]).is_err());
    assert!(db.set_endpoints("1", &[]).is_err());
    assert!(db.set_endpoints("1", &[endpoint("2", "b")]).is_err());
    assert_eq!(
        db.set_endpoints("missing", &[endpoint("missing", "b")])
            .unwrap_err()
            .code,
        "storage.not_found"
    );
    let mut updated = e;
    updated.mode = EndpointMode::ReceiveOnly;
    updated.remote_path = "/new/远程".into();
    db.set_endpoints("1", std::slice::from_ref(&updated))
        .unwrap();
    assert_eq!(db.get_sync_task("1").unwrap().unwrap().1, vec![updated]);
    db.delete_sync_task("1").unwrap();
    db.delete_sync_task("2").unwrap();
    assert!(db.list_sync_tasks().unwrap().is_empty());
    assert_eq!(
        db.conn
            .query_row("SELECT count(*) FROM sync_endpoints", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.delete_sync_task("missing").unwrap_err().code,
        "storage.not_found"
    );
}

fn 设置及秘密保护(db: &mut Db) {
    assert_eq!(db.get_setting::<String>("missing").unwrap(), None);
    let value = serde_json::json!({"theme": "深色", "count": 12});
    db.set_setting("ui", &value).unwrap();
    assert_eq!(
        db.get_setting::<serde_json::Value>("ui").unwrap(),
        Some(value)
    );
    db.set_setting("ui", &"浅色").unwrap();
    assert_eq!(
        db.get_setting::<String>("ui").unwrap().as_deref(),
        Some("浅色")
    );
    assert_eq!(
        db.get_setting::<u64>("ui").unwrap_err().code,
        "storage.json"
    );
    db.conn
        .execute("INSERT INTO settings VALUES ('invalid','{')", [])
        .unwrap();
    assert_eq!(
        db.get_setting::<String>("invalid").unwrap_err().code,
        "storage.json"
    );
    let id = db.device_short_id().unwrap();
    assert_eq!(id.len(), 8);
    assert!(
        id.bytes()
            .all(|v| v.is_ascii_lowercase() || v.is_ascii_digit())
    );
    assert_eq!(db.device_short_id().unwrap(), id);
    db.set_setting("device.short_id", &"无效").unwrap();
    assert_eq!(
        db.device_short_id().unwrap_err().code,
        "storage.invalid_device_id"
    );
    db.set_setting("device.short_id", &id).unwrap();
    let secret = "不应落库的密码";
    let mut s = server("secret", "服务器");
    s.password_cred = Some(secret.into());
    for error in [
        db.insert_server(&s).unwrap_err(),
        db.update_server(&s).unwrap_err(),
    ] {
        assert!(!serde_json::to_string(&error).unwrap().contains(secret));
        assert_eq!(error.code, "storage.invalid_credential_ref");
    }
    assert!(db.get_server("secret").unwrap().is_none());
    let error = sql_error(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(19),
        Some(secret.into()),
    ));
    assert!(!serde_json::to_string(&error).unwrap().contains(secret));
    for auth in [
        AuthMethod::Key,
        AuthMethod::Agent,
        AuthMethod::Password,
        AuthMethod::DedicatedKey,
    ] {
        let mut s = server("b", "服务器");
        s.auth_method = auth;
        db.update_server(&s).unwrap();
        assert_eq!(db.get_server("b").unwrap().unwrap(), s);
        assert_eq!(
            serde_json::from_str::<Server>(&serde_json::to_string(&s).unwrap()).unwrap(),
            s
        );
    }
    {
        let t = task("json", "/project");
        assert_eq!(
            serde_json::from_str::<SyncTask>(&serde_json::to_string(&t).unwrap()).unwrap(),
            t
        );
    }
    let e = endpoint("json", "b");
    assert_eq!(
        serde_json::from_str::<SyncEndpoint>(&serde_json::to_string(&e).unwrap()).unwrap(),
        e
    );
}

fn 每日备份和保留(db: &Db, root: &Path) {
    let dir = root.join("backups");
    for day in 1..=10 {
        db.set_setting("backup.day", &day).unwrap();
        db.backup_on(&dir, &format!("202001{day:02}")).unwrap();
    }
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 7);
    assert!(!dir.join("pierlink-20200103.db").exists());
    let backup = open(&dir.join("pierlink-20200110.db")).unwrap();
    assert_eq!(backup.get_setting::<u32>("backup.day").unwrap(), Some(10));
    drop(backup);
    fs::write(dir.join("notes.txt"), "保留").unwrap();
    fs::write(dir.join("pierlink-20209999.db"), "保留").unwrap();
    db.backup_daily(&dir).unwrap();
    let date: String = db
        .conn
        .query_row("SELECT strftime('%Y%m%d','now')", [], |r| r.get(0))
        .unwrap();
    let current = dir.join(format!("pierlink-{date}.db"));
    let before = fs::read(&current).unwrap();
    db.set_setting("backup.day", &11).unwrap();
    db.backup_daily(&dir).unwrap();
    assert_eq!(fs::read(&current).unwrap(), before);
    assert!(dir.join("notes.txt").exists());
    assert!(dir.join("pierlink-20209999.db").exists());
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 9);
    let invalid_dir = root.join("file");
    fs::write(&invalid_dir, "不是目录").unwrap();
    assert_eq!(
        db.backup_daily(&invalid_dir).unwrap_err().code,
        "storage.io"
    );
    let blocked = root.join("blocked");
    fs::create_dir_all(blocked.join(format!("pierlink-{date}.db"))).unwrap();
    assert_eq!(
        db.backup_daily(&blocked).unwrap_err().code,
        "storage.backup"
    );
}

fn 损坏检测(root: &Path) {
    let path = root.join("corrupt.db");
    fs::write(&path, b"not a sqlite database").unwrap();
    assert_eq!(open(&path).err().unwrap().code, "storage.corrupt");
    let path = root.join("truncated.db");
    let db = open(&path).unwrap();
    db.set_setting("data", &"用于截断检测").unwrap();
    drop(db);
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(200).unwrap();
    drop(file);
    assert_eq!(open(&path).err().unwrap().code, "storage.corrupt");
}

fn 并发路径检查和设备标识(root: &Path) {
    let path = root.join("concurrent.db");
    let db = open(&path).unwrap();
    db.insert_server(&server("s", "服务器")).unwrap();
    drop(db);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut db = open(&path).unwrap();
                barrier.wait();
                let id = db.device_short_id().unwrap();
                let task_id = i.to_string();
                let result = db.insert_sync_task(
                    &task(
                        &task_id,
                        if i == 0 {
                            "C:/project"
                        } else {
                            "c:/PROJECT/sub"
                        },
                    ),
                    &[endpoint(&task_id, "s")],
                );
                (id, result)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results[0].0, results[1].0);
    assert_eq!(results.iter().filter(|(_, r)| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .find_map(|(_, r)| r.as_ref().err())
            .unwrap()
            .code,
        "storage.local_path_overlap"
    );
}

#[test]
fn y8_跨平台路径重叠() {
    for (a, b, expected) in [
        ("C:\\Work\\项目\\", "c:/work/项目/src", true),
        ("C:/a/./b/../c", "c:/a/c/", true),
        ("C:/a", "C:/ab", false),
        ("C:/a", "D:/a", false),
        ("\\\\HOST\\Share\\x", "//host/share/X/y", true),
        ("//host/share", "//host/other", false),
        ("//host/share/../../x", "//HOST/share/x", true),
        ("\\\\?\\C:\\a", "c:/a", true),
        ("/a/./b/../c", "/a/c/d", true),
        ("/A", "/a", false),
        ("/a", "/ab", false),
        ("/", "/a", true),
        ("/../../a", "/a", true),
        ("a/../b", "b", true),
        ("../a", "a", false),
        ("/a\\b", "/a/b", false),
        ("", "/a", false),
        ("C:a", "C:/a", false),
    ] {
        assert_eq!(paths_overlap(a, b), expected, "{a} 与 {b}");
        assert_eq!(paths_overlap(b, a), expected, "{b} 与 {a}");
    }
}

#[test]
fn y24_危险目录() {
    let home = "C:/Users/开发者";
    let appdata = "C:/Users/开发者/AppData/Local";
    for path in [
        "C:/",
        "D:\\",
        "C:/Users/开发者/.",
        "C:/Users/开发者/.SSH/keys",
        "C:/Users/开发者/AppData/Local/Pierlink/logs",
        "C:/Windows/System32",
        "D:/WinNT/system",
        "C:/Program Files/Pierlink",
        "\\\\host\\share",
        "relative",
    ] {
        assert!(
            dangerous_local_path(path, home, appdata, "D:/WinNT"),
            "{path}"
        );
    }
    for path in [
        "C:/work/project",
        "C:/Users/开发者/project",
        "C:/Windows-old",
        "C:/Users/开发者/AppData/Local/Pierlink-other",
        "\\\\host\\share\\project",
    ] {
        assert!(
            !dangerous_local_path(path, home, appdata, "D:/WinNT"),
            "{path}"
        );
    }
    for path in [
        "/",
        "~",
        "~/.",
        "/home/user",
        "/etc/ssh",
        "/usr/bin",
        "/bin",
        "/boot",
        "/var/log",
        "/proc",
        "/sys",
        "~/.ssh/config",
        "~/.local/share/pierlink/bin",
        "~/.local/state/pierlink/x",
        "/home/user/project/../.ssh",
        "relative",
    ] {
        assert!(dangerous_remote_path(path, "/home/user"), "{path}");
    }
    for path in [
        "/home/user/project",
        "~/project",
        "/etc-old",
        "~/.SSH",
        "~/.local/share/pierlink-other",
        "/work",
    ] {
        assert!(!dangerous_remote_path(path, "/home/user"), "{path}");
    }
}

#[test]
fn y24_敏感目录上级与普通项目() {
    for path in [
        "/home",
        "/home/dev/..",
        "/home/dev/.local",
        "/home/dev/.local/state",
        "/home/dev/.local/share",
    ] {
        assert!(dangerous_remote_path(path, "/home/dev"), "{path}");
    }
    assert!(!dangerous_remote_path("/home/dev/projects/x", "/home/dev"));
    for (path, dangerous) in [
        (r"C:\Users", true),
        (r"C:\Users\张三\AppData", true),
        (r"C:\Users\张三\AppData\Local", true),
        (r"D:\code\x", false),
        (r"C:\Users\张三\code\x", false),
    ] {
        assert_eq!(
            dangerous_local_path(
                path,
                r"C:\Users\张三",
                r"C:\Users\张三\AppData\Local",
                r"C:\Windows"
            ),
            dangerous,
            "{path}"
        );
    }
}

fn 不支持硬链接时回退(root: &Path) {
    use std::io::{Error, ErrorKind};
    let source = root.join("待发布备份.db");
    let target = root.join("回退备份.db");
    fs::write(&source, b"backup").unwrap();
    backup::publish_backup(&source, &target, |_, _| Err(ErrorKind::Unsupported.into())).unwrap();
    assert!(!source.exists());
    assert_eq!(fs::read(&target).unwrap(), b"backup");

    fs::write(&source, b"new backup").unwrap();
    for kind in [ErrorKind::Unsupported, ErrorKind::AlreadyExists] {
        backup::publish_backup(&source, &target, |_, _| Err(kind.into())).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"backup");
        assert_eq!(fs::read(&source).unwrap(), b"new backup");
    }
    let missing = root.join("未发布备份.db");
    let error = backup::publish_backup(&source, &missing, |_, _| {
        Err(Error::from(ErrorKind::PermissionDenied))
    })
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    assert!(!missing.exists());
    assert!(source.exists());
}

#[test]
fn 服务器实例记录的写入替换与删除() {
    let dir = crate::test_support::tempdir();
    let db = open(&dir.path().join("t.db")).unwrap();
    let server = Server {
        id: "srv".into(),
        name: "gpu".into(),
        group_name: None,
        favorite: false,
        sort_order: 0,
        source: ServerSource::Manual,
        connection_engine: Engine::Builtin,
        engine_note: None,
        ssh_alias: None,
        host: Some("h".into()),
        port: Some(22),
        username: None,
        auth_method: AuthMethod::Agent,
        identity_file: None,
        identity_files: vec![],
        identities_only: false,
        host_key_alias: None,
        jump_server_id: None,
        login_shell: None,
        command_prediction: true,
        use_tmux: false,
        remember_password: false,
        password_cred: None,
        host_key_sha256: None,
        vnc_autostart: false,
        notes: String::new(),
        created_at: 1,
        updated_at: 1,
    };
    db.insert_server(&server).unwrap();
    let mut inst = ServerInstance {
        server_id: "srv".into(),
        instance_id: "i1".into(),
        syncthing_version: "v2.1.7".into(),
        arch: "x86_64".into(),
        state_dir: "/home/u/.local/state/pierlink/i1".into(),
        socket_path: "/run/user/1/pl/g".into(),
        sync_port: 22100,
        device_id: "DEV".into(),
        apikey_cred: "pierlink/instance/srv/apikey".into(),
        gui_password_cred: "pierlink/instance/srv/gui-password".into(),
        autostart_consent: true,
        deployed_at: 5,
        updated_at: 5,
    };
    assert_eq!(db.get_instance("srv").unwrap(), None);
    db.upsert_instance(&inst).unwrap();
    inst.sync_port = 22101;
    inst.updated_at = 6;
    db.upsert_instance(&inst).unwrap();
    assert_eq!(db.get_instance("srv").unwrap(), Some(inst.clone()));
    // 凭据字段只能是本服务器的引用名
    let mut bad = inst.clone();
    bad.apikey_cred = "真正的密钥".into();
    assert_eq!(
        db.upsert_instance(&bad).unwrap_err().code,
        "storage.invalid_credential_ref"
    );
    db.delete_instance("srv").unwrap();
    db.delete_instance("srv").unwrap();
    assert_eq!(db.get_instance("srv").unwrap(), None);
}

#[test]
fn l4_损坏时从最新完好的备份恢复() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("pierlink.db");
    let backups = root.path().join("backups");
    {
        let db = open(&path).unwrap();
        db.set_setting("value", &1).unwrap();
        db.backup_on(&backups, "20260101").unwrap();
        db.set_setting("value", &2).unwrap();
        db.backup_on(&backups, "20260102").unwrap();
    }
    // 最新的备份也坏了：应当退到前一天的
    fs::write(backups.join("pierlink-20260103.db"), b"not a database").unwrap();
    let before = fs::read(backups.join("pierlink-20260102.db")).unwrap();
    // 正常时直接打开，不恢复
    let (db, restored) = open_or_restore(&path, &backups).unwrap();
    assert!(restored.is_none());
    drop(db);
    // 写坏数据库文件
    let mut bytes = fs::read(&path).unwrap();
    for b in bytes.iter_mut().skip(100).take(4000) {
        *b = 0xA5;
    }
    fs::write(&path, &bytes).unwrap();
    let (db, restored) = open_or_restore(&path, &backups).unwrap();
    let restored = restored.unwrap();
    assert_eq!(restored.backup_date, "20260102");
    assert!(restored.corrupt_saved_as.exists());
    assert_eq!(db.get_setting::<u32>("value").unwrap(), Some(2));
    // 备份文件本身没有被改动，也没有在备份目录里留下 WAL 文件
    assert_eq!(
        fs::read(backups.join("pierlink-20260102.db")).unwrap(),
        before
    );
    assert!(!backups.join("pierlink-20260102.db-wal").exists());
}

#[test]
fn l4_没有可用备份时保持原状并报错() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("pierlink.db");
    fs::write(&path, vec![0x5Au8; 8192]).unwrap();
    let original = fs::read(&path).unwrap();
    let backups = root.path().join("backups");
    fs::create_dir_all(&backups).unwrap();
    fs::write(backups.join("pierlink-20260101.db"), b"bad").unwrap();
    let error = open_or_restore(&path, &backups).err().unwrap();
    assert!(
        error.code == "storage.corrupt" || error.code.starts_with("storage."),
        "{}",
        error.code
    );
    assert_eq!(fs::read(&path).unwrap(), original);
}

fn sync_event(task_id: &str, time_ms: i64) -> crate::sync::SyncEvent {
    crate::sync::SyncEvent {
        folder_id: format!("pl-{task_id}"),
        time_ms,
        kind: crate::sync::SyncEventKind::Downloaded,
        device: Some("服务器设备".into()),
        by: Some("local".into()),
        path: Some("中文目录/hello.txt".into()),
        detail: Some("已经脱敏的服务器详情 ***".into()),
    }
}
fn event_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn stored_event(id: i64, event: crate::sync::SyncEvent) -> StoredSyncEvent {
    StoredSyncEvent {
        id,
        folder_id: event.folder_id,
        time_ms: event.time_ms,
        kind: event.kind,
        device: event.device,
        by: event.by,
        path: event.path,
        detail: event.detail,
    }
}

fn event_count(db: &Db, task_id: &str) -> i64 {
    db.conn
        .query_row(
            "SELECT count(*) FROM sync_events WHERE task_id=?1",
            [task_id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn 事件只存显示种类且重启保留主键与字段() {
    use crate::sync::SyncEventKind::*;
    let root = crate::test_support::tempdir();
    let path = root.path().join("events.db");
    let mut db = open(&path).unwrap();
    db.insert_server(&server("s", "服务器")).unwrap();
    for id in ["a", "b"] {
        db.insert_sync_task(&task(id, &format!("/project/{id}")), &[endpoint(id, "s")])
            .unwrap();
    }
    let now = event_now_ms();
    let events: Vec<_> = [
        Downloaded,
        Uploaded,
        Deleted,
        Conflict,
        Error,
        DeviceConnected,
        DeviceDisconnected,
        Paused,
        StateChanged,
    ]
    .into_iter()
    .enumerate()
    .map(|(n, kind)| {
        let mut event = sync_event("a", now + n as i64);
        event.kind = kind;
        if matches!(event.kind, DeviceConnected | DeviceDisconnected) {
            event.folder_id.clear();
        }
        if n % 2 == 0 {
            event.device = None;
            event.by = None;
            event.path = None;
            event.detail = None;
        }
        event
    })
    .collect();
    db.insert_sync_events("a", &events).unwrap();
    // 只收到状态和暂停事件时同样不写入，也不占用自增主键。
    db.insert_sync_events("a", &events[7..]).unwrap();
    let expected: Vec<_> = events[..7]
        .iter()
        .enumerate()
        .rev()
        .map(|(n, event)| stored_event(n as i64 + 1, event.clone()))
        .collect();
    let other = sync_event("b", now);
    db.insert_sync_events("b", std::slice::from_ref(&other))
        .unwrap();
    drop(db);
    let mut db = open(&path).unwrap();
    assert_eq!(db.list_sync_events("a", 200, None).unwrap(), expected);
    assert_eq!(db.list_sync_events("a", 2, None).unwrap(), expected[..2]);
    assert_eq!(
        db.list_sync_events("b", 200, None).unwrap(),
        [stored_event(8, other.clone())]
    );
    let json = serde_json::to_value(&expected[0]).unwrap();
    assert_eq!(json["id"], expected[0].id);
    assert_eq!(
        serde_json::from_value::<StoredSyncEvent>(json).unwrap(),
        expected[0]
    );
    assert_eq!(
        db.insert_sync_events("absent", &[]).unwrap_err().code,
        "storage.not_found"
    );
    assert_eq!(
        db.insert_sync_events("a", &[events[0].clone(), other])
            .unwrap_err()
            .code,
        "storage.invalid_events"
    );
    assert_eq!(db.list_sync_events("a", 200, None).unwrap(), expected);
    db.delete_sync_task("a").unwrap();
    assert!(db.list_sync_events("a", 200, None).unwrap().is_empty());
    assert_eq!(event_count(&db, "a"), 0);
    assert_eq!(event_count(&db, "b"), 1);
}

#[test]
fn 事件超过两万零五百条才按时间和主键清理到两万条() {
    let root = crate::test_support::tempdir();
    let mut db = open(&root.path().join("events.db")).unwrap();
    db.insert_server(&server("s", "服务器")).unwrap();
    for id in ["a", "b"] {
        db.insert_sync_task(&task(id, &format!("/project/{id}")), &[endpoint(id, "s")])
            .unwrap();
    }
    let now = event_now_ms();
    // 逆序写入且每毫秒两条，检验时间优先、同毫秒按主键排序。
    let many: Vec<_> = (0..20_000)
        .rev()
        .map(|n| sync_event("a", now + 1000 + n / 2))
        .collect();
    db.insert_sync_events("a", &many).unwrap();
    assert_eq!(event_count(&db, "a"), 20_000);
    db.insert_sync_events("a", &[sync_event("a", now)]).unwrap();
    assert_eq!(event_count(&db, "a"), 20_001);
    db.insert_sync_events("a", &vec![sync_event("a", now); 499])
        .unwrap();
    assert_eq!(event_count(&db, "a"), 20_500);
    // 每页上限不会溢出，剩余缓冲记录仍可翻页读取。
    let first = db.list_sync_events("a", usize::MAX, None).unwrap();
    assert_eq!(first.len(), 20_000);
    let last = first.last().unwrap();
    assert_eq!(
        db.list_sync_events("a", usize::MAX, Some((last.time_ms, last.id)))
            .unwrap()
            .len(),
        500
    );
    db.insert_sync_events("b", &[sync_event("b", now)]).unwrap();
    assert_eq!(event_count(&db, "a"), 20_500);
    db.insert_sync_events("a", &[sync_event("a", now)]).unwrap();
    assert_eq!(event_count(&db, "a"), 20_000);
    assert_eq!(event_count(&db, "b"), 1);
    // 后写的旧事件全部删除；现有新事件的主键和内容不变。
    assert_eq!(db.list_sync_events("a", usize::MAX, None).unwrap(), first);

    // 单批远超阈值时一次删足差额；所有时间相同时保留后写的主键。
    db.insert_sync_events("a", &vec![sync_event("a", now + 30_000); 20_501])
        .unwrap();
    let kept = db.list_sync_events("a", usize::MAX, None).unwrap();
    assert_eq!(event_count(&db, "a"), 20_000);
    assert!(kept.iter().all(|event| event.time_ms == now + 30_000));
    assert!(kept.windows(2).all(|pair| pair[0].id == pair[1].id + 1));
    assert_eq!(kept.first().unwrap().id, 41_003);
    assert_eq!(kept.last().unwrap().id, 21_004);
}

#[test]
fn 启动按任务阈值清理且插入不清理其他任务的数量() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("events.db");
    let mut db = open(&path).unwrap();
    db.insert_server(&server("s", "服务器")).unwrap();
    let now = event_now_ms();
    for id in ["a", "b"] {
        db.insert_sync_task(&task(id, &format!("/project/{id}")), &[endpoint(id, "s")])
            .unwrap();
        db.insert_sync_events(id, &vec![sync_event(id, now); 20_500])
            .unwrap();
    }
    // 模拟已有超额记录，插入另一任务不触发它的数量清理。
    db.conn.execute("INSERT INTO sync_events(task_id,folder_id,time_ms,kind) VALUES ('a','pl-a',?1,'\"Downloaded\"')", [now]).unwrap();
    db.insert_sync_events("b", &[]).unwrap();
    assert_eq!(event_count(&db, "a"), 20_501);
    drop(db);
    let mut db = open(&path).unwrap();
    db.prune_sync_events(now).unwrap();
    assert_eq!(event_count(&db, "a"), 20_000);
    assert_eq!(event_count(&db, "b"), 20_500);
}

#[test]
fn 事件插入和启动按九十天清理且保留边界() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("events.db");
    let mut db = open(&path).unwrap();
    db.insert_server(&server("s", "服务器")).unwrap();
    for id in ["a", "b"] {
        db.insert_sync_task(&task(id, &format!("/project/{id}")), &[endpoint(id, "s")])
            .unwrap();
    }
    let now = event_now_ms();
    let day = 24 * 60 * 60 * 1000;
    let retention = 90 * day;
    let events = [
        sync_event("a", now - retention - 1000),
        sync_event("a", now - 89 * day),
        sync_event("a", now),
    ];
    db.insert_sync_events("a", &events).unwrap();
    assert_eq!(
        db.list_sync_events("a", 200, None).unwrap(),
        [
            stored_event(3, events[2].clone()),
            stored_event(2, events[1].clone())
        ]
    );
    // 时间清理仍覆盖所有任务，旧的不可见事件也自然过期。
    db.conn.execute("INSERT INTO sync_events(task_id,folder_id,time_ms,kind) VALUES ('b','pl-b',?1,'\"Paused\"')", [now - retention - 1000]).unwrap();
    db.insert_sync_events("a", &[]).unwrap();
    assert_eq!(event_count(&db, "b"), 0);
    for kind in ["Paused", "StateChanged"] {
        db.conn
            .execute(
                "INSERT INTO sync_events(task_id,folder_id,time_ms,kind) VALUES ('b','pl-b',?1,?2)",
                rusqlite::params![now, serde_json::to_string(kind).unwrap()],
            )
            .unwrap();
    }
    db.insert_sync_events("a", &[]).unwrap();
    drop(db);
    let mut db = open(&path).unwrap();
    let legacy = db.list_sync_events("b", 200, None).unwrap();
    assert_eq!(
        legacy.iter().map(|event| &event.kind).collect::<Vec<_>>(),
        [
            &crate::sync::SyncEventKind::StateChanged,
            &crate::sync::SyncEventKind::Paused
        ]
    );
    db.prune_sync_events(now + retention).unwrap();
    assert_eq!(
        db.list_sync_events("a", 200, None).unwrap(),
        [stored_event(3, events[2].clone())]
    );
    assert_eq!(db.list_sync_events("b", 200, None).unwrap(), legacy);
    db.prune_sync_events(now + retention + 1).unwrap();
    assert!(db.list_sync_events("a", 200, None).unwrap().is_empty());
    assert!(db.list_sync_events("b", 200, None).unwrap().is_empty());
    db.prune_sync_events(i64::MIN).unwrap();
}

#[test]
fn 事件键集连续翻页同毫秒不重不漏并隔离任务() {
    let root = crate::test_support::tempdir();
    let mut db = open(&root.path().join("events.db")).unwrap();
    db.insert_server(&server("s", "服务器")).unwrap();
    for id in ["a", "b"] {
        db.insert_sync_task(&task(id, &format!("/project/{id}")), &[endpoint(id, "s")])
            .unwrap();
    }
    let now = event_now_ms();
    let mut expected = Vec::new();
    for (n, offset) in [2, 0, 1, 2, 1, 2, 0, 2, 3, 2, 0].into_iter().enumerate() {
        let event = sync_event("a", now + offset);
        db.insert_sync_events("a", std::slice::from_ref(&event))
            .unwrap();
        expected.push(stored_event(n as i64 * 2 + 1, event));
        db.insert_sync_events("b", &[sync_event("b", now + offset)])
            .unwrap();
    }
    expected.sort_by_key(|event| std::cmp::Reverse((event.time_ms, event.id)));
    for limit in [1, 2, 3, 4, 20] {
        let mut before = None;
        let mut pages = Vec::new();
        // 最多每条一页，再读一次空页；避免错误分页导致测试无限循环。
        for _ in 0..=expected.len() {
            let page = db.list_sync_events("a", limit, before).unwrap();
            if page.is_empty() {
                break;
            }
            let last = page.last().unwrap();
            before = Some((last.time_ms, last.id));
            pages.extend(page);
        }
        assert_eq!(pages, expected);
        assert!(db.list_sync_events("a", limit, before).unwrap().is_empty());
    }
    for before in [None, Some((now + 2, 15))] {
        assert!(db.list_sync_events("a", 0, before).unwrap().is_empty());
        assert!(
            db.list_sync_events("absent", 200, before)
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        db.list_sync_events("a", 200, Some((i64::MIN, i64::MIN)))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.list_sync_events("a", 200, Some((i64::MAX, i64::MAX)))
            .unwrap(),
        expected
    );
}

#[test]
fn 翻页期间新增事件与删除游标行不影响后续页() {
    let root = crate::test_support::tempdir();
    let mut db = open(&root.path().join("events.db")).unwrap();
    db.insert_server(&server("s", "服务器")).unwrap();
    db.insert_sync_task(&task("a", "/project/a"), &[endpoint("a", "s")])
        .unwrap();
    let now = event_now_ms();
    db.insert_sync_events("a", &vec![sync_event("a", now); 6])
        .unwrap();
    let original = db.list_sync_events("a", 6, None).unwrap();
    let first = db.list_sync_events("a", 2, None).unwrap();
    let last = first.last().unwrap();
    db.conn
        .execute("DELETE FROM sync_events WHERE id=?1", [last.id])
        .unwrap();
    db.insert_sync_events("a", &[sync_event("a", now), sync_event("a", now + 1)])
        .unwrap();
    assert_eq!(
        db.list_sync_events("a", 6, Some((last.time_ms, last.id)))
            .unwrap(),
        original[2..]
    );
}

#[test]
fn v1数据库升级保留任务且本机默认收发_设置往返() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("old.db");
    let mut conn = Connection::open(&path).unwrap();
    Migrations::new(vec![M::up(include_str!("v1.sql"))])
        .to_latest(&mut conn)
        .unwrap();
    conn.execute("INSERT INTO sync_tasks(id,folder_id,name,local_path,ignore_rules,created_at,updated_at) VALUES ('old','pl-old','旧任务','/project/old','.git',1,2)", []).unwrap();
    drop(conn);
    let mut db = open(&path).unwrap();
    let mut stored = db.get_sync_task("old").unwrap().unwrap().0;
    assert_eq!(stored.name, "旧任务");
    assert_eq!(stored.local_mode, EndpointMode::SendReceive);
    assert_eq!(
        db.conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        9
    );
    db.insert_server(&server("s", "服务器")).unwrap();
    db.set_endpoints("old", &[endpoint("old", "s")]).unwrap();
    stored.local_mode = EndpointMode::ReceiveOnly;
    stored.collab_mode = true;
    stored.watch_delay_s = 23;
    stored.versioning_enabled = false;
    db.update_sync_task(&stored).unwrap();
    assert_eq!(db.get_sync_task("old").unwrap().unwrap().0, stored);
    stored.local_mode = EndpointMode::SendReceive;
    db.update_sync_task(&stored).unwrap();
    let mut ep = endpoint("old", "s");
    ep.mode = EndpointMode::ReceiveOnly;
    db.set_endpoints("old", &[ep.clone()]).unwrap();
    let result = db.get_sync_task("old").unwrap().unwrap();
    assert_eq!(result, (stored, vec![ep]));
    let event = sync_event("old", event_now_ms());
    db.insert_sync_events("old", std::slice::from_ref(&event))
        .unwrap();
    assert_eq!(
        db.list_sync_events("old", 200, None).unwrap(),
        [stored_event(1, event)]
    );
    // 旧接口输入省略本机方式时仍按收发解释。
    let mut json = serde_json::to_value(&result.0).unwrap();
    json.as_object_mut().unwrap().remove("local_mode");
    assert_eq!(
        serde_json::from_value::<SyncTask>(json).unwrap().local_mode,
        EndpointMode::SendReceive
    );
}

fn forward(id: &str, server_id: &str, kind: ForwardKind, port: u16) -> PortForward {
    PortForward {
        id: id.into(),
        server_id: server_id.into(),
        name: format!("转发{id}"),
        kind,
        local_port: port,
        remote_host: (kind == ForwardKind::Local).then(|| "db.internal".into()),
        remote_port: (kind != ForwardKind::Dynamic).then_some(5432),
        enabled: true,
        sort_order: 0,
    }
}

#[test]
fn 转发与片段增删改查_启用校验_删除服务器级联() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("db");
    let db = open(&path).unwrap();
    db.insert_server(&server("a", "甲")).unwrap();
    db.insert_server(&server("b", "乙")).unwrap();
    assert!(db.list_port_forwards(None).unwrap().is_empty());
    let mut local = forward("l", "a", ForwardKind::Local, 18080);
    db.upsert_port_forward(&local).unwrap();
    local.name = "数据库".into();
    local.remote_host = Some("::1".into());
    local.sort_order = 9;
    db.upsert_port_forward(&local).unwrap();
    let remote = forward("r", "a", ForwardKind::Remote, 18080);
    db.upsert_port_forward(&remote).unwrap();
    let mut dynamic = forward("d", "a", ForwardKind::Dynamic, 18080);
    assert_eq!(
        db.upsert_port_forward(&dynamic).unwrap_err().code,
        "tunnel.port_in_use"
    );
    dynamic.enabled = false;
    db.upsert_port_forward(&dynamic).unwrap();
    assert_eq!(
        db.set_port_forward_enabled("d", true).unwrap_err().code,
        "tunnel.port_in_use"
    );
    assert!(
        !db.list_port_forwards(Some("a"))
            .unwrap()
            .iter()
            .find(|f| f.id == "d")
            .unwrap()
            .enabled
    );
    dynamic.local_port = 1080;
    db.upsert_port_forward(&dynamic).unwrap();
    db.set_port_forward_enabled("d", true).unwrap();
    let all = db.list_port_forwards(None).unwrap();
    assert_eq!(
        all.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
        ["d", "r", "l"]
    );
    assert_eq!(all[2], local);
    let other = forward("b", "b", ForwardKind::Dynamic, 18080);
    db.upsert_port_forward(&other).unwrap();
    assert_eq!(db.list_port_forwards(Some("b")).unwrap(), [other]);
    assert!(db.list_port_forwards(Some("unknown")).unwrap().is_empty());
    let mut duplicate_remote = remote.clone();
    duplicate_remote.id = "remote-conflict".into();
    assert_eq!(
        db.upsert_port_forward(&duplicate_remote)
            .unwrap_err()
            .params["side"],
        "remote"
    );
    let mut invalid = dynamic.clone();
    invalid.remote_port = Some(80);
    assert_eq!(
        db.upsert_port_forward(&invalid).unwrap_err().code,
        "tunnel.invalid_forward"
    );
    let mut missing = local.clone();
    missing.id = "missing".into();
    missing.server_id = "missing".into();
    assert_eq!(
        db.upsert_port_forward(&missing).unwrap_err().code,
        "storage.constraint"
    );
    db.set_port_forward_enabled("l", false).unwrap();
    assert_eq!(
        db.set_port_forward_enabled("missing", true)
            .unwrap_err()
            .code,
        "storage.not_found"
    );
    db.delete_port_forward("d").unwrap();
    assert_eq!(
        db.delete_port_forward("d").unwrap_err().code,
        "storage.not_found"
    );

    assert!(db.list_snippets().unwrap().is_empty());
    let mut snippet = Snippet {
        id: "s".into(),
        title: "片段".into(),
        body: "echo {{name}}\r\necho done\r".into(),
        group_name: None,
        sort_order: 10,
    };
    db.upsert_snippet(&snippet).unwrap();
    snippet.title = "部署".into();
    snippet.group_name = Some("已删除的分组".into());
    db.upsert_snippet(&snippet).unwrap();
    let generic = Snippet {
        id: "generic".into(),
        title: "通用".into(),
        body: "date".into(),
        group_name: None,
        sort_order: -1,
    };
    db.upsert_snippet(&generic).unwrap();
    assert_eq!(db.list_snippets().unwrap(), [generic, snippet.clone()]);
    let json = serde_json::to_value(&snippet).unwrap();
    assert_eq!(serde_json::from_value::<Snippet>(json).unwrap(), snippet);
    let json = serde_json::to_value(&remote).unwrap();
    assert_eq!(json["kind"], "remote");
    assert_eq!(serde_json::from_value::<PortForward>(json).unwrap(), remote);
    assert!(serde_json::from_str::<ForwardKind>("\"unknown\"").is_err());
    db.delete_snippet("generic").unwrap();
    assert_eq!(
        db.delete_snippet("missing").unwrap_err().code,
        "storage.not_found"
    );
    for (field, value) in [
        ("id", ""),
        ("title", " "),
        ("body", "secret\0"),
        ("group_name", ""),
    ] {
        let mut invalid = snippet.clone();
        match field {
            "id" => invalid.id = value.into(),
            "title" => invalid.title = value.into(),
            "body" => invalid.body = value.into(),
            _ => invalid.group_name = Some(value.into()),
        }
        let error = db.upsert_snippet(&invalid).unwrap_err();
        assert_eq!(error.code, "storage.invalid_snippet");
        assert_eq!(error.params["field"], field);
        assert!(!format!("{error:?}").contains("secret"));
    }
    drop(db);
    let mut db = open(&path).unwrap();
    assert_eq!(db.list_snippets().unwrap(), [snippet]);
    assert!(
        !db.list_port_forwards(Some("a"))
            .unwrap()
            .iter()
            .find(|f| f.id == "l")
            .unwrap()
            .enabled
    );
    db.delete_server("a").unwrap();
    assert!(db.list_port_forwards(Some("a")).unwrap().is_empty());
    assert_eq!(db.list_port_forwards(None).unwrap().len(), 1);
    assert_eq!(db.list_snippets().unwrap().len(), 1);
    db.delete_snippet("s").unwrap();
}

#[test]
fn v1_v2升级当前版本保留转发与片段_排序默认零() {
    let root = crate::test_support::tempdir();
    for version in [1, 2] {
        let path = root.path().join(format!("v{version}.db"));
        let mut conn = Connection::open(&path).unwrap();
        let mut steps = vec![M::up(include_str!("v1.sql"))];
        if version == 2 {
            steps.push(M::up(include_str!("v2.sql")));
        }
        Migrations::new(steps).to_latest(&mut conn).unwrap();
        conn.execute("INSERT INTO servers(id,name,source,host,auth_method,created_at,updated_at) VALUES ('a','旧服务器','manual','example.test','agent',1,2)", []).unwrap();
        conn.execute("INSERT INTO port_forwards(id,server_id,name,kind,local_port,enabled) VALUES ('old','a','旧转发','dynamic',1080,1)", []).unwrap();
        conn.execute("INSERT INTO snippets(id,title,body,group_name,sort_order) VALUES ('s','旧片段','date','历史分组',7)", []).unwrap();
        drop(conn);
        let db = open(&path).unwrap();
        assert_eq!(
            db.conn
                .query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            9
        );
        let forwards = db.list_port_forwards(None).unwrap();
        assert_eq!(forwards[0].sort_order, 0);
        assert!(forwards[0].enabled);
        assert_eq!(forwards[0].kind, ForwardKind::Dynamic);
        let snippets = db.list_snippets().unwrap();
        assert_eq!(snippets[0].body, "date");
        assert_eq!(snippets[0].group_name.as_deref(), Some("历史分组"));
        assert_eq!(snippets[0].sort_order, 7);
        drop(db);
        assert_eq!(
            open(&path).unwrap().list_port_forwards(None).unwrap(),
            forwards
        );
    }
}

#[test]
fn 并发启用同一服务器端口只有一条成功() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("db");
    let db = open(&path).unwrap();
    db.insert_server(&server("a", "甲")).unwrap();
    for id in ["one", "two"] {
        let mut f = forward(id, "a", ForwardKind::Dynamic, 1080);
        f.enabled = false;
        db.upsert_port_forward(&f).unwrap();
    }
    let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = ["one", "two"]
        .into_iter()
        .map(|id| {
            let conn = open(&path).unwrap();
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.wait();
                conn.set_port_forward_enabled(id, true)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results.into_iter().find_map(Result::err).unwrap().code,
        "tunnel.port_in_use"
    );
    assert_eq!(
        db.list_port_forwards(None)
            .unwrap()
            .iter()
            .filter(|f| f.enabled)
            .count(),
        1
    );
}

#[test]
fn 旧事件迁移来源为空新事件保留作者() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("event-migration.db");
    let mut conn = Connection::open(&path).unwrap();
    Migrations::new(vec![
        M::up(include_str!("v1.sql")),
        M::up(include_str!("v2.sql")),
        M::up(include_str!("v3.sql")),
        M::up(include_str!("v4.sql")),
        M::up(include_str!("v5.sql")),
    ])
    .to_latest(&mut conn)
    .unwrap();
    conn.execute("INSERT INTO sync_tasks(id,folder_id,name,local_path,ignore_rules,created_at,updated_at) VALUES ('old','pl-old','旧任务','/project/old','.git',1,2)", []).unwrap();
    conn.execute("INSERT INTO sync_events(task_id,folder_id,time_ms,kind,device,path,detail) VALUES ('old','pl-old',?1,'\"Downloaded\"','server','a','旧事件')", [event_now_ms()]).unwrap();
    drop(conn);
    let mut db = open(&path).unwrap();
    let old = db.list_sync_events("old", 200, None).unwrap().remove(0);
    assert!(old.by.is_none());
    let mut new = sync_event("old", old.time_ms);
    new.device = old.device.clone();
    new.path = old.path.clone();
    new.detail = old.detail.clone();
    new.by = Some("local".into());
    new.kind = crate::sync::SyncEventKind::Uploaded;
    db.insert_sync_events("old", std::slice::from_ref(&new))
        .unwrap();
    drop(db);
    assert_eq!(
        open(&path)
            .unwrap()
            .list_sync_events("old", 200, None)
            .unwrap(),
        vec![stored_event(old.id + 1, new), old]
    );
}
