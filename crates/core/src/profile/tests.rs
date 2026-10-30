use super::*;
use crate::{
    security::{CredentialStore, MemoryCredentialStore},
    storage::{self, AuthMethod, Engine, ForwardKind, ServerSource},
};
use serde_json::json;

fn database(root: &tempfile::TempDir, name: &str) -> Db {
    storage::open(&root.path().join(name)).unwrap()
}

fn server(id: &str) -> Server {
    Server {
        id: id.into(),
        name: format!("服务器{id}"),
        group_name: Some("开发".into()),
        favorite: true,
        sort_order: 17,
        source: ServerSource::Manual,
        connection_engine: Engine::Builtin,
        engine_note: Some("使用内置连接".into()),
        ssh_alias: None,
        host: Some(format!("{id}.example.test")),
        port: Some(2222),
        username: Some("dev".into()),
        auth_method: AuthMethod::Password,
        identity_file: None,
        identity_files: vec![],
        identities_only: true,
        host_key_alias: Some("已知主机".into()),
        jump_server_id: None,
        login_shell: Some("/bin/zsh".into()),
        use_tmux: false,
        remember_password: true,
        password_cred: Some(format!("pierlink/server/{id}/password")),
        host_key_sha256: Some("SHA256:public-fingerprint".into()),
        vnc_autostart: true,
        notes: "备注\n第二行".into(),
        created_at: 123,
        updated_at: 456,
    }
}

fn forward(id: &str, server_id: &str, kind: ForwardKind, port: u16) -> PortForward {
    PortForward {
        id: id.into(),
        server_id: server_id.into(),
        name: format!("转发{id}"),
        kind,
        local_port: port,
        remote_host: (kind == ForwardKind::Local).then(|| "::1".into()),
        remote_port: (kind != ForwardKind::Dynamic).then_some(5432),
        enabled: true,
        sort_order: 23,
    }
}

fn snippet(id: &str) -> Snippet {
    Snippet {
        id: id.into(),
        title: format!("片段{id}"),
        body: "printf '你好'\r\nls\n".into(),
        group_name: Some("开发".into()),
        sort_order: 29,
    }
}

fn extras() -> ProfileExtras {
    ProfileExtras {
        app_version: "0.6.2".into(),
        settings: json!({"app":{"theme":"dark"},"mcp":{"serverIds":["target"]},"future":[null,true,7]}),
    }
}

fn profile(servers: Vec<Server>) -> Profile {
    Profile {
        format: PROFILE_FORMAT.into(),
        version: PROFILE_VERSION,
        exported_at: 789,
        app_version: extras().app_version,
        servers,
        port_forwards: vec![],
        snippets: vec![],
        extras: extras().settings,
    }
}

#[test]
fn 导出经文件解析导入空库保留全部配置与不透明设置() {
    let root = crate::test_support::tempdir();
    let source = database(&root, "source.db");
    let jump = server("jump");
    source.insert_server(&jump).unwrap();
    let mut target = server("target");
    target.jump_server_id = Some(jump.id.clone());
    target.sort_order = -1;
    target.auth_method = AuthMethod::DedicatedKey;
    target.identity_file = Some(root.path().join("missing-key").to_string_lossy().into());
    let present = root.path().join("key");
    std::fs::write(&present, "仅测试路径存在，不读取内容").unwrap();
    target.identity_files = vec![
        target.identity_file.clone().unwrap(),
        present.to_string_lossy().into(),
    ];
    source.insert_server(&target).unwrap();
    let alias =
        crate::server_input::build_alias_server("config-alias", Some("开发"), "alias", 10).unwrap();
    source.insert_server(&alias).unwrap();
    let f = forward("forward", "target", ForwardKind::Local, 18080);
    let s = snippet("snippet");
    source.upsert_port_forward(&f).unwrap();
    source.upsert_snippet(&s).unwrap();
    let exported = export_profile(&source, extras()).unwrap();
    assert!(exported.exported_at > 0);
    assert_eq!(exported.app_version, "0.6.2");
    assert_eq!(exported.extras, extras().settings);
    let exported = parse_profile(&serde_json::to_vec(&exported).unwrap()).unwrap();
    let mut destination = database(&root, "destination.db");
    let report = import_profile(&mut destination, &exported).unwrap();
    assert_eq!(
        (
            report.servers_added,
            report.forwards_added,
            report.snippets_added
        ),
        (3, 1, 1)
    );
    assert_eq!(report.server_id_map.len(), 3);
    assert!(report.servers_skipped.is_empty());
    assert_eq!(
        report.missing_identity_files,
        vec![MissingIdentityFile {
            server_id: report.server_id_map["target"].clone(),
            name: target.name.clone(),
            path: target.identity_file.clone().unwrap()
        }]
    );
    for mut expected in [jump, target, alias] {
        let old_id = expected.id.clone();
        expected.id = report.server_id_map[&old_id].clone();
        assert_ne!(old_id, expected.id);
        assert_eq!(
            uuid::Uuid::parse_str(&expected.id)
                .unwrap()
                .get_version_num(),
            4
        );
        expected.jump_server_id = expected
            .jump_server_id
            .map(|id| report.server_id_map[&id].clone());
        clear_credentials(&mut expected);
        assert_eq!(
            destination.get_server(&expected.id).unwrap(),
            Some(expected)
        );
    }
    let imported_forward = destination.list_port_forwards(None).unwrap().remove(0);
    assert_ne!(imported_forward.id, f.id);
    assert_eq!(
        imported_forward,
        PortForward {
            id: imported_forward.id.clone(),
            server_id: report.server_id_map["target"].clone(),
            ..f
        }
    );
    let imported_snippet = destination.list_snippets().unwrap().remove(0);
    assert_ne!(imported_snippet.id, s.id);
    assert_eq!(
        imported_snippet,
        Snippet {
            id: imported_snippet.id.clone(),
            ..s
        }
    );
    assert!(destination.get_setting::<Value>("mcp").unwrap().is_none());
    let repeated = import_profile(&mut destination, &exported).unwrap();
    assert_eq!(
        (
            repeated.servers_added,
            repeated.forwards_added,
            repeated.snippets_added
        ),
        (0, 0, 0)
    );
    assert_eq!(repeated.server_id_map, report.server_id_map);
    assert_eq!(
        (
            repeated.servers_skipped.len(),
            repeated.forwards_skipped.len(),
            repeated.snippets_skipped.len()
        ),
        (3, 1, 1)
    );
    assert!(repeated.missing_identity_files.is_empty());
}

#[test]
fn 同机与别名去重且旧主键碰撞不会覆盖无关记录() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    let mut existing = server("existing");
    existing.port = None;
    db.insert_server(&existing).unwrap();
    let alias = crate::server_input::build_alias_server("dev", None, "alias-existing", 0).unwrap();
    db.insert_server(&alias).unwrap();
    let mut duplicate = existing.clone();
    duplicate.id = "duplicate".into();
    duplicate.name = "名称不同仍是同机".into();
    duplicate.port = Some(22);
    duplicate.password_cred = Some("不可信引用".into());
    let mut alias_duplicate = alias.clone();
    alias_duplicate.id = "alias-old".into();
    alias_duplicate.host = Some("解析地址不同".into());
    let mut new = server("existing");
    new.host = Some("different.example.test".into());
    let mut child = server("child");
    child.jump_server_id = Some("duplicate".into());
    let p = profile(vec![child, duplicate, alias_duplicate, new]);
    let report = import_profile(&mut db, &p).unwrap();
    assert_eq!((report.servers_added, report.servers_skipped.len()), (2, 2));
    assert_eq!(report.server_id_map["duplicate"], existing.id);
    assert_eq!(report.server_id_map["alias-old"], alias.id);
    assert_ne!(report.server_id_map["existing"], "existing");
    assert_eq!(db.get_server("existing").unwrap(), Some(existing));
    assert_eq!(db.get_server("alias-existing").unwrap(), Some(alias));
    assert_eq!(
        db.get_server(&report.server_id_map["child"])
            .unwrap()
            .unwrap()
            .jump_server_id
            .as_deref(),
        Some("existing")
    );
}

#[test]
fn 不同端口用户名及不同别名各自保留并处理文件内重复() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    let base = server("base");
    let mut other_port = base.clone();
    other_port.id = "other-port".into();
    other_port.port = Some(22);
    let mut other_user = base.clone();
    other_user.id = "other-user".into();
    other_user.username = Some("other".into());
    let mut duplicate = base.clone();
    duplicate.id = "duplicate".into();
    let mut p = profile(vec![base, other_port, other_user, duplicate]);
    for alias in ["one", "two"] {
        let mut s = crate::server_input::build_alias_server(alias, None, alias, 0).unwrap();
        s.host = Some("same.example.test".into());
        p.servers.push(s);
    }
    let report = import_profile(&mut db, &p).unwrap();
    assert_eq!(report.servers_added, 5);
    assert_eq!(
        report.server_id_map["base"],
        report.server_id_map["duplicate"]
    );
}

#[test]
fn 缺失跳板及其依赖跳过且不使用库中碰巧同名的旧id() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    db.insert_server(&server("outside")).unwrap();
    let mut child = server("child");
    child.jump_server_id = Some("parent".into());
    let mut parent = server("parent");
    parent.jump_server_id = Some("outside".into());
    let mut p = profile(vec![child, parent, server("valid")]);
    p.port_forwards = vec![
        forward("outside", "outside", ForwardKind::Local, 18080),
        forward("child", "child", ForwardKind::Dynamic, 18081),
    ];
    let report = import_profile(&mut db, &p).unwrap();
    assert_eq!(report.servers_added, 1);
    assert_eq!(report.servers_skipped.len(), 2);
    assert!(
        report
            .servers_skipped
            .iter()
            .all(|s| s.reason == ImportReason::MissingJumpServer)
    );
    assert_eq!(report.server_id_map.len(), 1);
    assert_eq!(report.forwards_skipped.len(), 2);
    assert!(
        report
            .forwards_skipped
            .iter()
            .all(|s| s.reason == ImportReason::MissingServer)
    );
}

#[test]
fn 跳板环路及依赖环路的服务器跳过() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    let mut p = profile(vec![server("a"), server("b"), server("c"), server("self")]);
    for (server, jump) in p.servers.iter_mut().zip(["b", "a", "a", "self"]) {
        server.jump_server_id = Some(jump.into());
    }
    let report = import_profile(&mut db, &p).unwrap();
    assert_eq!(report.servers_added, 0);
    assert_eq!(report.servers_skipped.len(), 4);
    assert!(
        report
            .servers_skipped
            .iter()
            .all(|s| s.reason == ImportReason::JumpCycle)
    );
    assert!(report.server_id_map.is_empty());
}

#[test]
fn 转发按监听端口去重且本机跨服务器冲突停用() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    db.insert_server(&server("a")).unwrap();
    let existing = forward("old", "a", ForwardKind::Local, 18080);
    db.upsert_port_forward(&existing).unwrap();
    let mut p = profile(vec![server("a"), server("b")]);
    let mut duplicate = forward("duplicate", "a", ForwardKind::Local, 18080);
    duplicate.remote_port = Some(9999);
    p.port_forwards = vec![
        duplicate,
        forward("dynamic", "a", ForwardKind::Dynamic, 18080),
        forward("cross", "b", ForwardKind::Local, 18080),
        forward("new", "b", ForwardKind::Local, 18081),
        forward("batch-conflict", "a", ForwardKind::Dynamic, 18081),
        forward("remote", "a", ForwardKind::Remote, 18080),
        forward("remote-duplicate", "a", ForwardKind::Remote, 19999),
        forward("remote-other-server", "b", ForwardKind::Remote, 18080),
    ];
    let mut remote_new_port = forward("remote-new-port", "a", ForwardKind::Remote, 18080);
    remote_new_port.remote_port = Some(5433);
    p.port_forwards.push(remote_new_port);
    let report = import_profile(&mut db, &p).unwrap();
    assert_eq!(
        (
            report.forwards_added,
            report.forwards_skipped.len(),
            report.forwards_disabled.len()
        ),
        (7, 2, 3)
    );
    let forwards = db.list_port_forwards(None).unwrap();
    assert_eq!(forwards.iter().find(|f| f.id == "old"), Some(&existing));
    for id in ["dynamic", "cross", "batch-conflict"] {
        assert!(
            !forwards
                .iter()
                .find(|f| f.name == format!("转发{id}"))
                .unwrap()
                .enabled
        );
    }
    assert!(
        forwards
            .iter()
            .filter(|f| f.kind == ForwardKind::Remote)
            .all(|f| f.enabled)
    );
}

#[test]
fn 停用的已有转发同样保留端口并将导入冲突项停用() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    db.insert_server(&server("a")).unwrap();
    let mut old = forward("old", "a", ForwardKind::Local, 18080);
    old.enabled = false;
    db.upsert_port_forward(&old).unwrap();
    let mut p = profile(vec![server("b")]);
    p.port_forwards
        .push(forward("new", "b", ForwardKind::Dynamic, 18080));
    let report = import_profile(&mut db, &p).unwrap();
    assert_eq!(report.forwards_disabled.len(), 1);
    assert!(
        db.list_port_forwards(None)
            .unwrap()
            .iter()
            .all(|f| !f.enabled)
    );
}

#[test]
fn 片段按标题和原始正文去重且新id避免覆盖() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    let old = snippet("same-id");
    db.upsert_snippet(&old).unwrap();
    let mut duplicate = old.clone();
    duplicate.id = "duplicate".into();
    duplicate.group_name = None;
    let mut different = old.clone();
    different.body.push_str("pwd");
    let mut another_title = old.clone();
    another_title.id = "another-title".into();
    another_title.title.push('二');
    let mut repeated = different.clone();
    repeated.id = "repeated".into();
    let mut p = profile(vec![]);
    p.snippets = vec![duplicate, different, another_title, repeated];
    let report = import_profile(&mut db, &p).unwrap();
    assert_eq!(
        (report.snippets_added, report.snippets_skipped.len()),
        (2, 2)
    );
    assert_eq!(
        db.list_snippets()
            .unwrap()
            .iter()
            .find(|s| s.id == "same-id"),
        Some(&old)
    );
}

#[test]
fn 拒绝格式和不支持版本且不回显不可信数据() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    let mut p = profile(vec![server("a")]);
    p.format = "不应回显的秘密".into();
    let err = import_profile(&mut db, &p).unwrap_err();
    assert_eq!(err.code, "profile.invalid_format");
    assert!(!format!("{err:?}").contains(&p.format));
    p.format = PROFILE_FORMAT.into();
    for version in [0, 2, u32::MAX] {
        p.version = version;
        let err = import_profile(&mut db, &p).unwrap_err();
        assert_eq!(err.code, "profile.unsupported_version");
        assert_eq!(err.params["version"], version.to_string());
        assert!(db.list_servers().unwrap().is_empty());
    }
    let err = parse_profile(br#"{"format":"pierlink-profile","version":2}"#).unwrap_err();
    assert_eq!(err.code, "profile.unsupported_version");
    for json in [
        b"not-json".as_slice(),
        br#"{"format":"pierlink-profile","version":1}"#,
    ] {
        assert_eq!(
            parse_profile(json).unwrap_err().code,
            "profile.invalid_json"
        );
    }
}

#[test]
fn 集合字符串身份列表和不透明json超限均拒绝() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    let assert_limit = |db: &mut Db, p: &Profile| {
        assert_eq!(
            import_profile(db, p).unwrap_err().code,
            "profile.limit_exceeded"
        );
        assert!(db.list_servers().unwrap().is_empty());
    };
    let mut p = profile(vec![server("a"); MAX_PROFILE_ITEMS + 1]);
    assert_limit(&mut db, &p);
    p.servers.clear();
    p.snippets = vec![snippet("a"); MAX_PROFILE_ITEMS + 1];
    assert_limit(&mut db, &p);
    p.snippets.clear();
    p.port_forwards = vec![forward("a", "a", ForwardKind::Dynamic, 1); MAX_PROFILE_ITEMS + 1];
    assert_limit(&mut db, &p);
    p.port_forwards.clear();
    p.servers.push(server("a"));
    p.servers[0].notes = "x".repeat(MAX_STRING_BYTES + 1);
    assert_limit(&mut db, &p);
    p.servers[0].notes.clear();
    p.servers[0].identity_files = vec!["key".into(); 129];
    assert_limit(&mut db, &p);
    p.servers.clear();
    p.app_version = "字".repeat(MAX_STRING_BYTES / 3 + 1);
    assert_limit(&mut db, &p);
    p.app_version.clear();
    p.extras = json!({"large":"x".repeat(MAX_STRING_BYTES+1)});
    assert_limit(&mut db, &p);
    p.extras = json!({"x".repeat(MAX_STRING_BYTES+1):null});
    assert_limit(&mut db, &p);
    p.extras = Value::Null;
    for _ in 0..34 {
        p.extras = json!([p.extras]);
    }
    assert_limit(&mut db, &p);
    p.extras = Value::Array(vec![Value::Null; 500_001]);
    assert_limit(&mut db, &p);
    assert_eq!(
        parse_profile(&vec![b' '; MAX_PROFILE_BYTES + 1])
            .unwrap_err()
            .code,
        "profile.limit_exceeded"
    );
}

#[test]
fn 字符串边界可往返且总大小与转义后大小受限() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    let mut p = profile(vec![]);
    p.extras = Value::String("x".repeat(MAX_STRING_BYTES));
    let encoded = serde_json::to_vec(&p).unwrap();
    assert_eq!(parse_profile(&encoded).unwrap(), p);
    import_profile(&mut db, &p).unwrap();
    for text in [
        "x".repeat(MAX_STRING_BYTES),
        "\u{1}".repeat(MAX_STRING_BYTES),
    ] {
        p.extras = Value::Array(vec![Value::String(text); 513]);
        assert_eq!(
            import_profile(&mut db, &p).unwrap_err().code,
            "profile.limit_exceeded"
        );
    }
    p.extras = Value::Array(vec![Value::String("\u{1}".repeat(MAX_STRING_BYTES)); 86]);
    let err = import_profile(&mut db, &p).unwrap_err();
    assert_eq!(err.code, "profile.limit_exceeded");
    assert_eq!(err.params["field"], "file_bytes");
}

#[test]
fn 重复id缺失字段和非法转发拒绝且不回显字段值() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    let mut p = profile(vec![server("a"), server("a")]);
    assert_eq!(
        import_profile(&mut db, &p).unwrap_err().code,
        "profile.duplicate_id"
    );
    p.servers.pop();
    p.servers[0].host = None;
    assert_eq!(
        import_profile(&mut db, &p).unwrap_err().code,
        "profile.invalid_field"
    );
    p.servers[0] = server("a");
    p.port_forwards
        .push(forward("a", "a", ForwardKind::Dynamic, 0));
    assert_eq!(
        import_profile(&mut db, &p).unwrap_err().code,
        "tunnel.invalid_forward"
    );
    assert!(db.list_servers().unwrap().is_empty());
}

#[test]
fn 写入服务器转发和首个片段后发生错误会整体回滚() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    let existing = server("existing");
    db.insert_server(&existing).unwrap();
    db.set_setting("keep", &json!({"value":42})).unwrap();
    let conn = rusqlite::Connection::open(root.path().join("db")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_snippet BEFORE INSERT ON snippets WHEN NEW.title='失败片段' BEGIN SELECT RAISE(ABORT, '不可泄露的触发器秘密'); END;").unwrap();
    let mut p = profile(vec![server("new")]);
    p.port_forwards
        .push(forward("new", "new", ForwardKind::Dynamic, 18080));
    p.snippets = vec![snippet("first"), snippet("second")];
    p.snippets[1].title = "失败片段".into();
    let err = import_profile(&mut db, &p).unwrap_err();
    assert_eq!(err.code, "storage.constraint");
    assert!(!format!("{err:?}").contains("不可泄露"));
    assert_eq!(db.list_servers().unwrap(), vec![existing]);
    assert!(db.list_port_forwards(None).unwrap().is_empty());
    assert!(db.list_snippets().unwrap().is_empty());
    assert_eq!(
        db.get_setting::<Value>("keep").unwrap(),
        Some(json!({"value":42}))
    );
    // 失败后的连接仍可开启事务并成功写入。
    p.snippets.pop();
    assert_eq!(import_profile(&mut db, &p).unwrap().servers_added, 1);
}

#[test]
fn 导出不包含秘密引用私钥内容同步数据或数据库设置() {
    let root = crate::test_support::tempdir();
    let source = database(&root, "source");
    let credentials = MemoryCredentialStore::default();
    let mut s = server("source");
    let private_key =
        "-----BEGIN OPENSSH PRIVATE KEY-----\n私钥测试内容\n-----END OPENSSH PRIVATE KEY-----";
    let key_path = root.path().join("key");
    std::fs::write(&key_path, private_key).unwrap();
    s.identity_file = Some(key_path.to_string_lossy().into());
    source.insert_server(&s).unwrap();
    let password_ref = s.password_cred.clone().unwrap();
    for (key, secret) in [
        (&password_ref[..], "密码测试值"),
        ("pierlink/local/key-passphrase", "口令测试值"),
        ("pierlink/mcp/token", "令牌测试值"),
    ] {
        credentials.set(key, secret).unwrap();
    }
    source
        .set_setting("excluded-setting", &"不迁移的设置标记")
        .unwrap();
    let conn = rusqlite::Connection::open(root.path().join("source")).unwrap();
    conn.execute_batch("INSERT INTO server_instances VALUES ('source','不迁移的实例','v','arch','state','socket',123,'device','apikey-ref','gui-password-ref',1,0,0);
        INSERT INTO sync_tasks(id,folder_id,name,local_path,ignore_rules,created_at,updated_at) VALUES ('task','folder','不迁移的任务','/local','',0,0);
        INSERT INTO sync_endpoints VALUES ('task','source','/remote','sendreceive');
        INSERT INTO sync_events(task_id,folder_id,time_ms,kind,detail) VALUES ('task','folder',0,'created','不迁移的事件');").unwrap();
    let exported = export_profile(&source, extras()).unwrap();
    let json = serde_json::to_string(&exported).unwrap();
    for forbidden in [
        &password_ref[..],
        "密码测试值",
        "口令测试值",
        "令牌测试值",
        "私钥测试内容",
        "OPENSSH PRIVATE KEY",
        "不迁移的设置标记",
        "不迁移的实例",
        "不迁移的任务",
        "不迁移的事件",
        "apikey-ref",
        "gui-password-ref",
        "sync_tasks",
        "server_instances",
        "sync_events",
    ] {
        assert!(!json.contains(forbidden), "不应导出 {forbidden}");
    }
    assert!(exported.servers[0].password_cred.is_none());
    assert!(!exported.servers[0].remember_password);
    assert_eq!(source.get_server("source").unwrap(), Some(s));
    let mut destination = database(&root, "destination");
    let report = import_profile(&mut destination, &exported).unwrap();
    assert!(destination.list_sync_tasks().unwrap().is_empty());
    assert!(
        destination
            .get_instance(&report.server_id_map["source"])
            .unwrap()
            .is_none()
    );
    assert_eq!(
        credentials.get(&password_ref).unwrap().as_deref(),
        Some("密码测试值")
    );
}

#[test]
fn 不可信密码引用不会写入新服务器或出现在报告中() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    let mut p = profile(vec![server("a")]);
    p.servers[0].password_cred = Some("输入中的秘密引用".into());
    let report = import_profile(&mut db, &p).unwrap();
    let actual = db.get_server(&report.server_id_map["a"]).unwrap().unwrap();
    assert!(actual.password_cred.is_none());
    assert!(!actual.remember_password);
    assert!(
        !serde_json::to_string(&report)
            .unwrap()
            .contains("输入中的秘密引用")
    );
    assert_eq!(actual.host_key_sha256, p.servers[0].host_key_sha256);
}

#[test]
fn 五千条片段边界可导入且深度边界可往返() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    let mut p = profile(vec![]);
    p.snippets = (0..MAX_PROFILE_ITEMS)
        .map(|i| snippet(&i.to_string()))
        .collect();
    let report = import_profile(&mut db, &p).unwrap();
    assert_eq!(report.snippets_added, MAX_PROFILE_ITEMS);
    p.snippets.clear();
    p.extras = Value::Null;
    for _ in 0..31 {
        p.extras = json!([p.extras]);
    }
    import_profile(&mut db, &p).unwrap();
    assert_eq!(parse_profile(&serde_json::to_vec(&p).unwrap()).unwrap(), p);
    p.extras = json!([p.extras]);
    assert_eq!(
        import_profile(&mut db, &p).unwrap_err().code,
        "profile.limit_exceeded"
    );
}

#[test]
fn 转发引用去重后的已有服务器并在映射后查重() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    let existing = server("existing");
    db.insert_server(&existing).unwrap();
    db.upsert_port_forward(&forward("saved", "existing", ForwardKind::Dynamic, 18080))
        .unwrap();
    let mut source = existing;
    source.id = "old".into();
    let mut p = profile(vec![source]);
    p.port_forwards = vec![
        forward("duplicate", "old", ForwardKind::Dynamic, 18080),
        forward("new", "old", ForwardKind::Dynamic, 18081),
    ];
    let report = import_profile(&mut db, &p).unwrap();
    assert_eq!(
        (report.forwards_added, report.forwards_skipped.len()),
        (1, 1)
    );
    assert!(
        db.list_port_forwards(None)
            .unwrap()
            .iter()
            .all(|f| f.server_id == "existing")
    );
}

#[test]
fn 非法片段不写入且回滚之前的实体() {
    let root = crate::test_support::tempdir();
    let mut db = database(&root, "db");
    let mut p = profile(vec![server("new")]);
    p.snippets.push(snippet("invalid"));
    p.snippets[0].body = "\0不应回显的正文".into();
    let error = import_profile(&mut db, &p).unwrap_err();
    assert_eq!(error.code, "storage.invalid_snippet");
    assert!(!format!("{error:?}").contains("不应回显"));
    assert!(db.list_servers().unwrap().is_empty());
    assert!(db.list_snippets().unwrap().is_empty());
}
