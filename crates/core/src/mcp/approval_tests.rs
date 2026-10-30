//! 操作审批的行为矩阵、会话隔离与审计兼容性。
use super::*;

fn assert_approval(fixture: &Fixture, source: &str, flags: &[bool]) {
    let record = fixture.service.activity(1).remove(0);
    assert_eq!(record.approval, source);
    assert_eq!(serde_json::to_value(record).unwrap()["approval"], source);
    assert_eq!(lock(&fixture.host.danger_flags).as_slice(), flags);
}

#[test]
fn 审批方式序列化且默认只询问危险操作() {
    assert_eq!(Permissions::default().approval, Approval::Dangerous);
    for (mode, value) in [
        (Approval::AskAll, "ask_all"),
        (Approval::Dangerous, "dangerous"),
        (Approval::Auto, "auto"),
    ] {
        assert_eq!(serde_json::to_value(mode).unwrap(), value);
        assert_eq!(
            serde_json::from_value::<Approval>(json!(value)).unwrap(),
            mode
        );
        assert_eq!(
            serde_json::to_value(Permissions {
                approval: mode,
                ..Default::default()
            })
            .unwrap()["approval"],
            value
        );
    }
}

#[tokio::test]
async fn 三种审批方式覆盖普通和危险命令以及文件写入() {
    for (mode, sources, flags) in [
        (
            Approval::AskAll,
            ["user", "user"],
            vec![false, true, false, true],
        ),
        (Approval::Dangerous, ["", "user"], vec![true, true]),
        // 全自动：本来就不用问的（普通命令、普通文件）来源留空，危险的才标 auto
        (Approval::Auto, ["", "auto"], vec![]),
    ] {
        let fixture = Fixture::new();
        {
            let mut permissions = lock(&fixture.host.permissions);
            permissions.approval = mode;
            permissions.folders = vec![fixture._dir.path().into()];
        }
        let mut client = fixture.legacy().await;
        for (command, source) in ["echo ok", "rm -rf /example"].into_iter().zip(sources) {
            let result = client
                .call(
                    "run_command",
                    json!({"target":"server-id","command":command}),
                )
                .await;
            assert_eq!(result["result"]["isError"], false, "{mode:?}：{result}");
            assert_eq!(fixture.service.activity(1)[0].approval, source);
        }
        for (name, source) in ["notes.txt", "start.ps1"].into_iter().zip(sources) {
            let path = fixture._dir.path().join(name);
            let result = client
                .call("write_file", json!({"file_path":path,"content":"测试内容"}))
                .await;
            assert_eq!(result["result"]["isError"], false, "{mode:?}：{result}");
            assert_eq!(std::fs::read_to_string(path).unwrap(), "测试内容");
            assert_eq!(fixture.service.activity(1)[0].approval, source);
        }
        assert_eq!(lock(&fixture.host.danger_flags).as_slice(), flags);
        for (name, args) in [
            ("list_targets", json!({})),
            ("list_terminals", json!({})),
            (
                "read_file",
                json!({"file_path":fixture._dir.path().join("notes.txt")}),
            ),
            ("list_dir", json!({"path":fixture._dir.path()})),
            ("sync_status", json!({})),
            ("sync_wait", json!({"timeout_secs":1})),
        ] {
            assert_eq!(client.call(name, args).await["result"]["isError"], false);
            assert_approval(&fixture, "", &flags);
        }
        assert_eq!(fixture.host.confirmations.load(Ordering::Acquire), 1);
        client.close().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn 三种审批方式覆盖终端文字回车和多行提交() {
    for (mode, sources, flags) in [
        (
            Approval::AskAll,
            ["user", "user", "user"],
            vec![false, true, true],
        ),
        (Approval::Dangerous, ["", "user", "user"], vec![true, true]),
        (Approval::Auto, ["", "auto", "auto"], vec![]),
    ] {
        let fixture = Fixture::new();
        lock(&fixture.host.permissions).approval = mode;
        let mut client = fixture.legacy().await;
        let opened = client
            .call("open_terminal", json!({"target":"server-id"}))
            .await;
        let id = opened["result"]["structuredContent"]["terminal_id"]
            .as_str()
            .unwrap();
        assert_approval(&fixture, "", &[]);
        // 分两次发送普通行，确认框须使用完整的待提交内容。
        assert_eq!(
            client
                .call("terminal_send", json!({"terminal_id":id,"text":"echo ok"}))
                .await["result"]["isError"],
            false
        );
        assert_approval(&fixture, "", &[]);
        for (args, source) in [
            json!({"terminal_id":id,"keys":["Enter"]}),
            json!({"terminal_id":id,"text":"rm -rf /example","keys":["Ctrl+M"]}),
            json!({"terminal_id":id,"text":"echo first\nrm -rf /example\necho last\r\n"}),
        ]
        .into_iter()
        .zip(sources)
        {
            assert_eq!(
                client.call("terminal_send", args).await["result"]["isError"],
                false
            );
            assert_eq!(fixture.service.activity(1)[0].approval, source);
        }
        let expected_actions: &[&str] = match mode {
            Approval::AskAll => &[
                "echo ok",
                "rm -rf /example",
                "echo first\nrm -rf /example\necho last",
            ],
            Approval::Dangerous => &["rm -rf /example", "rm -rf /example"],
            Approval::Auto => &[],
        };
        assert_eq!(lock(&fixture.host.dangers).as_slice(), expected_actions);
        for (name, args) in [
            ("terminal_send", json!({"terminal_id":id,"keys":["Enter"]})),
            ("terminal_read", json!({"terminal_id":id})),
            ("terminal_screen", json!({"terminal_id":id})),
            ("terminal_wait", json!({"terminal_id":id,"quiet_ms":100})),
            ("close_terminal", json!({"terminal_id":id})),
        ] {
            assert_eq!(client.call(name, args).await["result"]["isError"], false);
            assert_approval(&fixture, "", &flags);
        }
        client.close().await;
    }
}

#[tokio::test]
async fn 每次询问拒绝普通命令和文件且不记为允许() {
    let fixture = Fixture::new();
    lock(&fixture.host.permissions).approval = Approval::AskAll;
    lock(&fixture.host.permissions).folders = vec![fixture._dir.path().into()];
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    let mut client = fixture.legacy().await;
    let path = fixture._dir.path().join("notes.txt");
    for (name, args) in [
        (
            "run_command",
            json!({"target":"server-id","command":"echo ok"}),
        ),
        ("write_file", json!({"file_path":path,"content":"不得写入"})),
    ] {
        let result = client.call(name, args).await;
        assert_eq!(error_text(&result), "用户拒绝执行");
        assert_eq!(fixture.service.activity(1)[0].outcome, "denied");
        assert_eq!(fixture.service.activity(1)[0].approval, "");
    }
    assert!(!path.exists());
    assert!(lock(&fixture.host.commands).is_empty());
    assert_eq!(lock(&fixture.host.danger_flags).as_slice(), [false, false]);
    client.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn 每次询问拒绝终端提交后仍可重新审批同一行() {
    let fixture = Fixture::new();
    lock(&fixture.host.permissions).approval = Approval::AskAll;
    let mut client = fixture.legacy().await;
    let opened = client
        .call("open_terminal", json!({"target":"server-id"}))
        .await;
    let id = opened["result"]["structuredContent"]["terminal_id"]
        .as_str()
        .unwrap();
    client
        .call("terminal_send", json!({"terminal_id":id,"text":"echo ok"}))
        .await;
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    let result = client
        .call("terminal_send", json!({"terminal_id":id,"keys":["Enter"]}))
        .await;
    assert_eq!(error_text(&result), "用户拒绝执行");
    assert_approval(&fixture, "", &[false]);
    assert!(fixture.service.activity(1)[0].summary.contains("echo ok"));
    *lock(&fixture.host.action_decision) = Confirmation::Allow;
    let result = client
        .call("terminal_send", json!({"terminal_id":id,"keys":["Enter"]}))
        .await;
    assert_eq!(
        result["result"]["structuredContent"]["submitted"],
        json!(["echo ok"])
    );
    assert_approval(&fixture, "user", &[false, false]);
    assert_eq!(
        lock(&fixture.host.dangers).as_slice(),
        ["echo ok", "echo ok"]
    );
    client.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn 连接内不再询问覆盖所有写操作但不授权其他连接() {
    for mode in [Approval::AskAll, Approval::Dangerous] {
        let fixture = Fixture::new();
        lock(&fixture.host.permissions).approval = mode;
        lock(&fixture.host.permissions).folders = vec![fixture._dir.path().into()];
        *lock(&fixture.host.action_decision) = Confirmation::AllowAlways;
        let mut client = fixture.legacy().await;
        let result = client
            .call(
                "run_command",
                json!({"target":"server-id","command":"rm -rf /example"}),
            )
            .await;
        assert_eq!(result["result"]["isError"], false);
        assert_approval(&fixture, "session", &[true]);
        *lock(&fixture.host.action_decision) = Confirmation::Deny;
        let opened = client
            .call("open_terminal", json!({"target":"server-id"}))
            .await;
        let id = opened["result"]["structuredContent"]["terminal_id"]
            .as_str()
            .unwrap();
        assert_approval(&fixture, "", &[true]);
        // 第三项：按这个方式本来要不要问（不用问的来源留空）
        for (name, args, would_ask) in [
            (
                "run_command",
                json!({"target":"server-id","command":"echo ok"}),
                mode == Approval::AskAll,
            ),
            (
                "run_command",
                json!({"target":"server-id","command":"rm -rf /example"}),
                true,
            ),
            (
                "write_file",
                json!({"file_path":fixture._dir.path().join("notes.txt"),"content":"内容"}),
                mode == Approval::AskAll,
            ),
            (
                "write_file",
                json!({"file_path":fixture._dir.path().join("start.ps1"),"content":"内容"}),
                true,
            ),
            (
                "terminal_send",
                json!({"terminal_id":id,"text":"echo ok\nrm -rf /example\n"}),
                true,
            ),
        ] {
            assert_eq!(client.call(name, args).await["result"]["isError"], false);
            assert_approval(&fixture, if would_ask { "session" } else { "" }, &[true]);
        }
        let mut other = fixture.legacy().await;
        let result = other
            .call(
                "run_command",
                json!({"target":"server-id","command":"rm -rf /example"}),
            )
            .await;
        assert_eq!(error_text(&result), "用户拒绝执行");
        assert_approval(&fixture, "", &[true, true]);
        client.close().await;
        other.close().await;
    }
}

#[tokio::test]
async fn 审批使用当前设置且单次允许不记住后续操作() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    for (mode, source, flags) in [
        (Approval::Dangerous, "", vec![]),
        (Approval::AskAll, "user", vec![false]),
        (Approval::AskAll, "user", vec![false, false]),
        (Approval::Auto, "", vec![false, false]),
        (Approval::Dangerous, "", vec![false, false]),
    ] {
        lock(&fixture.host.permissions).approval = mode;
        assert_eq!(
            client
                .call(
                    "run_command",
                    json!({"target":"server-id","command":"echo ok"})
                )
                .await["result"]["isError"],
            false
        );
        assert_approval(&fixture, source, &flags);
    }
    client.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn 全自动和连接内不再询问仍拒绝密码输入() {
    for mode in [Approval::AskAll, Approval::Dangerous, Approval::Auto] {
        for remembered in [false, true] {
            let fixture = Fixture::new();
            let mut client = fixture.legacy().await;
            if remembered {
                *lock(&fixture.host.action_decision) = Confirmation::AllowAlways;
                client
                    .call(
                        "run_command",
                        json!({"target":"server-id","command":"rm -rf /example"}),
                    )
                    .await;
            }
            lock(&fixture.host.permissions).approval = mode;
            let count = lock(&fixture.host.dangers).len();
            let opened = client
                .call("open_terminal", json!({"target":"server-id"}))
                .await;
            let id = opened["result"]["structuredContent"]["terminal_id"]
                .as_str()
                .unwrap();
            fixture.host.terminals.write(id, b"Password:\r").unwrap();
            until(|| {
                fixture
                    .host
                    .terminals
                    .screen_text(id)
                    .is_ok_and(|screen| safety::looks_like_password_prompt(&screen))
            })
            .await;
            let result = client
                .call(
                    "terminal_send",
                    json!({"terminal_id":id,"text":"不得记录的输入","keys":["Enter"]}),
                )
                .await;
            assert!(error_text(&result).contains("请用户在 Pierlink"));
            let record = fixture.service.activity(1).remove(0);
            assert_eq!(record.summary, "拒绝");
            assert_eq!(record.approval, "");
            assert_eq!(record.outcome, "denied");
            assert_eq!(lock(&fixture.host.dangers).len(), count);
            assert_eq!(
                client
                    .call("terminal_send", json!({"terminal_id":id,"keys":["Ctrl+C"]}))
                    .await["result"]["isError"],
                false
            );
            client.close().await;
        }
    }
}

#[tokio::test]
async fn 审批来源写入日志且旧日志缺字段仍可读取() {
    let fixture = Fixture::new();
    let mut client = fixture.legacy().await;
    for mode in [Approval::Dangerous, Approval::AskAll, Approval::Auto] {
        lock(&fixture.host.permissions).approval = mode;
        client
            .call(
                "run_command",
                json!({"target":"server-id","command":"echo ok"}),
            )
            .await;
    }
    lock(&fixture.host.permissions).approval = Approval::AskAll;
    *lock(&fixture.host.action_decision) = Confirmation::AllowAlways;
    client
        .call(
            "run_command",
            json!({"target":"server-id","command":"echo ok"}),
        )
        .await;
    let mut records = Vec::new();
    for entry in std::fs::read_dir(fixture._dir.path()).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        records.extend(
            text.lines()
                .map(|line| serde_json::from_str::<AuditRecord>(line).unwrap()),
        );
    }
    records.sort_by_key(|record| record.time_ms);
    assert_eq!(
        records
            .iter()
            .map(|record| record.approval.as_str())
            .collect::<Vec<_>>(),
        ["", "user", "", "session"]
    );
    let mut old = serde_json::to_value(&records[0]).unwrap();
    old.as_object_mut().unwrap().remove("approval");
    assert_eq!(
        serde_json::from_value::<AuditRecord>(old).unwrap().approval,
        ""
    );
    client.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn 每次都问时翻历史后回车也要问() {
    let fixture = Fixture::new();
    lock(&fixture.host.permissions).approval = Approval::AskAll;
    let mut client = fixture.legacy().await;
    let opened = client
        .call("open_terminal", json!({"target":"server-id"}))
        .await;
    let id = opened["result"]["structuredContent"]["terminal_id"]
        .as_str()
        .unwrap();
    assert_eq!(
        client
            .call(
                "terminal_send",
                json!({"terminal_id":id,"keys":["Up","Enter"]})
            )
            .await["result"]["isError"],
        false
    );
    assert_approval(&fixture, "user", &[true]);
    assert_eq!(
        lock(&fixture.host.dangers).last().map(String::as_str),
        Some("（翻历史后回车，内容未知）")
    );
    client.close().await;
}

#[tokio::test]
async fn 开发源码写入不触发危险审批而每次询问仍确认() {
    for mode in [Approval::Dangerous, Approval::AskAll] {
        let fixture = Fixture::new();
        {
            let mut permissions = lock(&fixture.host.permissions);
            permissions.approval = mode;
            permissions.folders = vec![fixture._dir.path().into()];
        }
        let mut client = fixture.legacy().await;
        for ext in ["js", "py", "sh", "ts", "mjs", "jsx"] {
            let path = fixture._dir.path().join(format!("source.{ext}"));
            assert_eq!(
                client
                    .call("write_file", json!({"file_path":path,"content":"example"}))
                    .await["result"]["isError"],
                false
            );
        }
        assert_eq!(
            lock(&fixture.host.dangers).len(),
            if mode == Approval::AskAll { 6 } else { 0 }
        );
        client.close().await;
    }
}
