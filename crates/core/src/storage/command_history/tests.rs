use super::*;
use crate::storage::open;
use rusqlite_migration::{M, Migrations};
use std::path::Path;

fn database() -> Db {
    open(Path::new(":memory:")).unwrap()
}

fn entry(scope: &str, time: i64) -> CommandEntry {
    CommandEntry {
        scope: scope.into(),
        cwd: Some("/home/dev/项目".into()),
        command: "git status".into(),
        exit_code: Some(0),
        used_at_ms: time,
    }
}

fn count(db: &Db, scope: &str) -> i64 {
    db.conn
        .query_row(
            "SELECT count(*) FROM command_history WHERE scope=?1",
            [scope],
            |row| row.get(0),
        )
        .unwrap()
}

fn seed(db: &Db, scope: &str, time: i64, amount: i64) {
    db.conn
        .execute(
            "WITH RECURSIVE numbers(n) AS (
                SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n < ?3
             ) INSERT INTO command_history(scope,command,used_at_ms)
             SELECT ?1,printf('echo %d',n),?2 FROM numbers",
            params![scope, time, amount],
        )
        .unwrap();
}

#[test]
fn 从旧库升级保留全部实体并默认跟随全局预测设置() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("upgrade.db");
    let mut conn = Connection::open(&path).unwrap();
    Migrations::new(vec![
        M::up(include_str!("../v1.sql")),
        M::up(include_str!("../v2.sql")),
        M::up(include_str!("../v3.sql")),
        M::up(include_str!("../v4.sql")),
        M::up(include_str!("../v5.sql")),
        M::up(include_str!("../v6.sql")),
        M::up(include_str!("../v7.sql")),
        M::up(include_str!("../v8.sql")),
    ])
    .to_latest(&mut conn)
    .unwrap();
    conn.execute_batch(
        "INSERT INTO servers(id,name,source,host,auth_method,created_at,updated_at)
         VALUES ('old','旧服务器','manual','example.test','agent',123,456);
         INSERT INTO settings(key,value) VALUES ('theme','\"dark\"');
         INSERT INTO snippets(id,title,body) VALUES ('snippet','示例','ls -la');",
    )
    .unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        8
    );
    drop(conn);

    let mut db = open(&path).unwrap();
    assert_eq!(
        db.conn
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        9
    );
    let mut server = db.get_server("old").unwrap().unwrap();
    assert!(server.command_prediction);
    assert_eq!(server.name, "旧服务器");
    assert_eq!((server.created_at, server.updated_at), (123, 456));
    assert_eq!(
        db.get_setting::<String>("theme").unwrap().as_deref(),
        Some("dark")
    );
    assert_eq!(db.list_snippets().unwrap()[0].body, "ls -la");
    assert!(db.list_commands("server:old").unwrap().is_empty());
    let index_columns: Vec<String> = db
        .conn
        .prepare("PRAGMA index_info(command_history_scope_time)")
        .unwrap()
        .query_map([], |row| row.get(2))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(index_columns, ["scope", "used_at_ms"]);
    server.command_prediction = false;
    db.update_server(&server).unwrap();
    db.add_command(&entry("server:old", i64::MAX)).unwrap();
    drop(db);
    let db = open(&path).unwrap();
    assert_eq!(db.get_server("old").unwrap(), Some(server));
    assert_eq!(db.list_commands("server:old").unwrap().len(), 1);
}

#[test]
fn 按范围读写原文且按时间升序并保留重复执行() {
    let mut db = database();
    let late = entry("local:bash", 30);
    let mut early = entry("local:bash", 10);
    early.command = "  echo 中文  ".into();
    early.cwd = None;
    early.exit_code = None;
    for record in [
        &late,
        &early,
        &early,
        &entry("quick:dev@example.test:22", 5),
    ] {
        db.add_command(record).unwrap();
    }
    assert_eq!(
        db.list_commands("local:bash").unwrap(),
        [early.clone(), early, late]
    );
    assert_eq!(
        db.list_commands("quick:dev@example.test:22").unwrap().len(),
        1
    );
    assert!(db.list_commands("server:missing").unwrap().is_empty());
}

#[test]
fn 数量留余量且查询截断最新记录并稳定处理同毫秒() {
    let mut db = database();
    seed(&db, "local:bash", 100, 5199);
    seed(&db, "quick:dev@host:22", 100, 5200);
    let newer = entry("local:bash", 101);
    db.add_command(&newer).unwrap();
    assert_eq!(count(&db, "local:bash"), 5200);
    let rows = db.list_commands("local:bash").unwrap();
    assert_eq!(rows.len(), 5000);
    assert_eq!(rows[0].command, "echo 201");
    assert_eq!(rows.last(), Some(&newer));

    // 补录的时间较旧，即使最后插入，也应先被清理。
    db.add_command(&entry("local:bash", 99)).unwrap();
    assert_eq!(count(&db, "local:bash"), 5000);
    assert_eq!(db.list_commands("local:bash").unwrap(), rows);
    assert_eq!(count(&db, "quick:dev@host:22"), 5200);
    db.add_command(&entry("local:bash", 102)).unwrap();
    assert_eq!(count(&db, "local:bash"), 5001);
    assert_eq!(
        db.list_commands("local:bash").unwrap()[0].command,
        "echo 202"
    );
}

#[test]
fn 插入和数量清理失败时在同一事务回滚且错误不泄露原文() {
    let mut db = database();
    seed(&db, "local:bash", 100, 5200);
    db.conn
        .execute_batch(
            "CREATE TRIGGER fail_prune BEFORE DELETE ON command_history
             BEGIN SELECT RAISE(ABORT,'不得出现在错误中的原文'); END;",
        )
        .unwrap();
    let error = db.add_command(&entry("local:bash", 101)).unwrap_err();
    assert_eq!(error.code, "storage.constraint");
    assert!(
        !serde_json::to_string(&error)
            .unwrap()
            .contains("不得出现在错误中的原文")
    );
    assert_eq!(count(&db, "local:bash"), 5200);
    assert!(
        db.list_commands("local:bash")
            .unwrap()
            .iter()
            .all(|r| r.used_at_ms == 100)
    );
}

#[test]
fn 过期清理覆盖所有范围并保留恰好一百八十天和极端时间() {
    let mut db = database();
    let now = 2 * RETENTION_MS;
    for scope in ["local:bash", "server:one", "quick:dev@host:22"] {
        for time in [now - RETENTION_MS - 1, now - RETENTION_MS, now] {
            db.add_command(&entry(scope, time)).unwrap();
        }
        // 插入不执行全表过期清理。
        assert_eq!(count(&db, scope), 3);
    }
    db.prune_commands(now).unwrap();
    for scope in ["local:bash", "server:one", "quick:dev@host:22"] {
        let rows = db.list_commands(scope).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].used_at_ms, now - RETENTION_MS);
    }
    db.add_command(&entry("server:min", i64::MIN)).unwrap();
    db.prune_commands(i64::MIN).unwrap();
    assert_eq!(count(&db, "server:min"), 1);
    db.add_command(&entry("server:max", i64::MAX)).unwrap();
    db.prune_commands(i64::MAX).unwrap();
    assert_eq!(count(&db, "server:min"), 0);
    assert_eq!(count(&db, "server:max"), 1);
}

#[test]
fn 重新打开数据库自动清理过期历史() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("history.db");
    let mut db = open(&path).unwrap();
    db.add_command(&entry("local:bash", 0)).unwrap();
    db.add_command(&entry("local:bash", i64::MAX)).unwrap();
    assert_eq!(count(&db, "local:bash"), 2);
    drop(db);
    assert_eq!(
        open(&path).unwrap().list_commands("local:bash").unwrap(),
        [entry("local:bash", i64::MAX)]
    );
}

#[test]
fn 清空单个范围及全部记录且重复清除成功() {
    let mut db = database();
    for scope in ["local:bash", "server:one", "quick:dev@host:22"] {
        db.add_command(&entry(scope, 1)).unwrap();
    }
    db.clear_commands(Some("server:one")).unwrap();
    db.clear_commands(Some("server:one")).unwrap();
    assert_eq!(count(&db, "server:one"), 0);
    assert_eq!(count(&db, "local:bash"), 1);
    assert_eq!(count(&db, "quick:dev@host:22"), 1);
    db.clear_commands(None).unwrap();
    db.clear_commands(None).unwrap();
    for scope in ["local:bash", "server:one", "quick:dev@host:22"] {
        assert_eq!(count(&db, scope), 0);
    }
}

#[test]
fn 删除服务器联动历史且失败时完整回滚() {
    let mut db = database();
    let server = crate::server_input::build_alias_server("dev", None, "one", 1).unwrap();
    db.insert_server(&server).unwrap();
    for scope in ["server:one", "server:one-more", "local:bash"] {
        db.add_command(&entry(scope, 1)).unwrap();
    }
    db.conn.execute_batch(
        "INSERT INTO sync_tasks(id,folder_id,name,local_path,ignore_rules,created_at,updated_at)
         VALUES ('task','pl-task','任务','/workspace','',1,2);
         INSERT INTO sync_endpoints(task_id,server_id,remote_path) VALUES ('task','one','/workspace');",
    ).unwrap();
    assert_eq!(
        db.delete_server("one").unwrap_err().code,
        "storage.server_in_use"
    );
    assert_eq!(count(&db, "server:one"), 1);
    db.conn
        .execute_batch(
            "DELETE FROM sync_tasks;
         CREATE TRIGGER fail_history_delete BEFORE DELETE ON command_history
         BEGIN SELECT RAISE(ABORT,'禁止删除历史'); END;",
        )
        .unwrap();
    assert_eq!(
        db.delete_server("one").unwrap_err().code,
        "storage.constraint"
    );
    assert_eq!(db.get_server("one").unwrap(), Some(server));
    assert_eq!(count(&db, "server:one"), 1);
    db.conn
        .execute_batch("DROP TRIGGER fail_history_delete;")
        .unwrap();
    db.delete_server("one").unwrap();
    assert!(db.get_server("one").unwrap().is_none());
    assert_eq!(count(&db, "server:one"), 0);
    assert_eq!(count(&db, "server:one-more"), 1);
    assert_eq!(count(&db, "local:bash"), 1);
    assert_eq!(
        db.delete_server("missing").unwrap_err().code,
        "storage.not_found"
    );
}

fn assert_invalid(db: &mut Db, record: &CommandEntry, field: &str) {
    let error = db.add_command(record).unwrap_err();
    assert_eq!(error.code, "history.invalid_entry");
    assert_eq!(error.params.len(), 1);
    assert_eq!(error.params["field"], field);
}

#[test]
fn 范围只接受三种格式并校验组成部分与长度() {
    let mut db = database();
    for scope in [
        "",
        "local:",
        "local:zsh",
        "local:bash:extra",
        "server:",
        "server:a b",
        "server:a/b",
        "server:a:22",
        "server:a\0",
        "other:a",
        "quick:dev@host",
        "quick:dev@:22",
        "quick:dev@host:0",
        "quick:dev@host:65536",
        "quick:dev@host:-1",
        "quick:dev@host:+22",
        "quick:dev@host:",
        "quick:dev@host:abc",
        "quick:dev@host:22:3",
        "quick:dev@@host:22",
        "quick:dev@host\n:22",
        "quick:dev@[bad::ip]:22",
        "quick:dev@[host]:22",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain([format!("server:{}", "a".repeat(294))])
    {
        assert_invalid(&mut db, &entry(&scope, 1), "scope");
        assert_eq!(
            db.list_commands(&scope).unwrap_err().code,
            "history.invalid_entry"
        );
        assert_eq!(
            db.clear_commands(Some(&scope)).unwrap_err().params["field"],
            "scope"
        );
    }
    for scope in [
        "server:550e8400-e29b-41d4-a716-446655440000",
        "server:one_2",
        "local:bash",
        "quick:dev@host:1",
        "quick:@host:22",
        "quick:dev@host:65535",
        "quick:dev@127.0.0.1:22",
        "quick:dev@[2001:db8::1]:22",
        "quick:dev@2001:db8::1:22",
        "quick:dev@主机:22",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain([format!("server:{}", "a".repeat(293))])
    {
        db.add_command(&entry(&scope, 1)).unwrap();
        assert_eq!(count(&db, &scope), 1);
    }
}

#[test]
fn 命令按字符限长且拒绝空值和所有终端控制字节() {
    let mut db = database();
    let mut record = entry("local:bash", 1);
    for command in [String::new(), "字".repeat(1001), "a".repeat(1001)] {
        record.command = command;
        assert_invalid(&mut db, &record, "command");
    }
    for byte in (0..0x20).chain([0x7f]) {
        record.command = format!("echo {}value", char::from(byte));
        assert_invalid(&mut db, &record, "command");
    }
    record.command = "字".repeat(1000);
    db.add_command(&record).unwrap();
    assert_eq!(db.list_commands("local:bash").unwrap(), [record]);
}

#[test]
fn 目录按字节限长且拒绝控制字符并校验退出码边界() {
    let mut db = database();
    let mut record = entry("local:bash", 1);
    for cwd in [
        "a".repeat(4097),
        "字".repeat(1366),
        "/a\n".into(),
        "/a\0".into(),
        "/a\u{85}".into(),
        "/a\u{7f}".into(),
    ] {
        record.cwd = Some(cwd);
        assert_invalid(&mut db, &record, "cwd");
    }
    for cwd in [
        None,
        Some(String::new()),
        Some("a".repeat(4096)),
        Some(format!("{}a", "字".repeat(1365))),
    ] {
        record.cwd = cwd;
        db.add_command(&record).unwrap();
    }
    for code in [-1, 256, i32::MIN, i32::MAX] {
        record.exit_code = Some(code);
        assert_invalid(&mut db, &record, "exit_code");
    }
    for code in [None, Some(0), Some(255)] {
        record.exit_code = code;
        db.add_command(&record).unwrap();
    }
    assert_eq!(count(&db, "local:bash"), 7);
}

#[test]
fn 隐私规则逐项命中且写入静默忽略() {
    assert!(SENSITIVE_COMMAND.is_ok());
    let mut db = database();
    let commands = [
        "echo password=example",
        "echo PASSWD=example",
        "echo passwd",
        "echo SeCrEt",
        "echo TOKEN",
        "echo apikey",
        "echo API_KEY",
        "echo api-key",
        "echo privatekey",
        "echo private_key",
        "echo PRIVATE-KEY",
        "echo '-----BEGIN RSA PRIVATE KEY-----'",
        "echo 'BEGIN OPENSSH PRIVATE KEY'",
        "echo 'BEGIN PRIVATE KEY'",
        "echo Authorization",
        "sshpass -f file ssh host",
        "mysql --password example",
        "mysql -pexample",
        "-pexample",
        "mysql\u{a0}-Pexample",
        "cd ~/project-passwords-doc",
        "echo AKIA0123456789ABCDEF",
        "echo akia0123456789abcdef",
        "echo ghp_0123456789abcdef",
        "echo github_pat_123456789",
        "echo xoxa-0123456789abcdef",
        "echo xoxb-0123456789abcdef",
        "echo xoxp-0123456789abcdef",
        "echo xoxr-0123456789abcdef",
        "echo xoxs-0123456789abcdef",
        "echo sk-0123456789abcdefg",
        "echo SK-0123456789ABCDEFG",
    ];
    for command in commands {
        assert!(is_sensitive_command(command), "{command}");
        let mut record = entry("local:bash", 1);
        record.command = command.into();
        db.add_command(&record).unwrap();
    }
    assert_eq!(count(&db, "local:bash"), 0);
}

#[test]
fn 普通命令和未达长度的令牌前缀不被误伤() {
    let mut db = database();
    for command in [
        "git push",
        "npm run dev",
        "ls -la",
        "git status",
        "echo public_key",
        "echo pass",
        "echo -p",
        "echo a-pvalue",
        "cd ~/my-project",
        "echo ghp_short",
        "echo github_pat_short",
        "echo AKIA0123456789ABCDE",
        "echo sk-short",
    ] {
        assert!(!is_sensitive_command(command), "{command}");
        let mut record = entry("local:bash", 1);
        record.command = command.into();
        db.add_command(&record).unwrap();
    }
    assert_eq!(count(&db, "local:bash"), 13);
    for prefix in [
        "ghp_",
        "github_pat_",
        "xoxa-",
        "xoxb-",
        "xoxp-",
        "xoxr-",
        "xoxs-",
        "sk-",
    ] {
        let short = format!("{prefix}{}", "a".repeat(19 - prefix.len()));
        assert!(!is_sensitive_command(&short), "{prefix}");
        assert!(is_sensitive_command(&format!("{short}a")), "{prefix}");
    }
}

#[test]
fn 命令记录可序列化且校验错误不回传不可信字段() {
    let original = entry("local:bash", 1);
    assert_eq!(
        serde_json::from_str::<CommandEntry>(&serde_json::to_string(&original).unwrap()).unwrap(),
        original
    );
    let mut db = database();
    let mut record = original;
    record.command = "password=不应回传\n".into();
    assert_invalid(&mut db, &record, "command");
    record.scope = "server:秘密@scope".into();
    assert_invalid(&mut db, &record, "scope");
}
