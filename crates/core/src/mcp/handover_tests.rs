//! 用户终端的元数据、入口授权、收回和输入竞争。
use super::*;
use crate::terminal::InputSource;

fn target_for(fixture: &Fixture, kind: TargetKind) -> Target {
    fixture
        .host
        .targets(&Entry::Windows, &lock(&fixture.host.permissions))
        .unwrap()
        .into_iter()
        .find(|target| target.kind == kind)
        .unwrap()
}
fn user(fixture: &Fixture, kind: TargetKind) -> String {
    let id = fixture.host.user_terminal();
    lock(&fixture.host.user_terminals).push(UserTerminal {
        terminal_id: id.clone(),
        title: "工作终端".into(),
        kind,
        target_id: if kind == TargetKind::Windows {
            "windows"
        } else {
            "server-id"
        }
        .into(),
        shell: Some("PowerShell".into()),
        busy: Some(true),
        handed_over: false,
    });
    id
}
fn hand_over(fixture: &Fixture, id: &str, entry: Entry, kind: TargetKind) {
    fixture
        .service
        .hand_over(id, entry, target_for(fixture, kind))
        .unwrap();
}
fn listed(value: &Value) -> &Value {
    &value["result"]["structuredContent"]
}

#[tokio::test]
async fn 用户终端列表过滤权限且不查询目录或泄露地址() {
    let fixture = Fixture::new();
    let local = user(&fixture, TargetKind::Windows);
    let remote = user(&fixture, TargetKind::Server);
    {
        let mut users = lock(&fixture.host.user_terminals);
        let mut hidden = users[1].clone();
        hidden.terminal_id = "hidden".into();
        hidden.target_id = "hidden-server".into();
        users.push(hidden.clone());
        hidden.terminal_id = "quick".into();
        hidden.target_id.clear();
        users.push(hidden);
    }
    let mut client = fixture.client(Entry::Server {
        server_id: "entry".into(),
    });
    client.initialize("2025-11-25").await;
    let result = client.call("list_terminals", json!({})).await;
    let users = listed(&result)["user_terminals"].as_array().unwrap();
    assert_eq!(users.len(), 2);
    assert_eq!(users[0]["terminal_id"], local);
    assert_eq!(users[0]["busy"], true);
    assert_eq!(users[0]["shell"], "PowerShell");
    assert!(users[1]["busy"].is_null());
    assert!(users[1]["shell"].is_null());
    assert!(
        users
            .iter()
            .all(|item| item.get("cwd").is_none() && item.get("address").is_none())
    );
    assert!(lock(&fixture.host.cwd_requests).is_empty());
    assert!(!result.to_string().contains("private"));
    lock(&fixture.host.permissions).windows_commands = false;
    let result = client.call("list_terminals", json!({})).await;
    assert_eq!(
        listed(&result)["user_terminals"].as_array().unwrap().len(),
        1
    );
    assert_eq!(listed(&result)["user_terminals"][0]["terminal_id"], remote);
    lock(&fixture.host.permissions).servers.clear();
    assert_eq!(
        listed(&client.call("list_terminals", json!({})).await)["user_terminals"],
        json!([])
    );
    client.close().await;
}

#[tokio::test]
async fn 交出只授权指定入口且目录仅对获授权入口可见() {
    let fixture = Fixture::new();
    let id = user(&fixture, TargetKind::Windows);
    let entry = Entry::Server {
        server_id: "entry".into(),
    };
    hand_over(&fixture, &id, entry.clone(), TargetKind::Windows);
    let mut owner = fixture.client(entry.clone());
    owner.initialize("2025-11-25").await;
    let result = owner.call("list_terminals", json!({})).await;
    assert_eq!(
        listed(&result)["ai_terminals"][0]["source"],
        json!({"kind":"handed_over","entry":entry})
    );
    assert_eq!(listed(&result)["ai_terminals"][0]["cwd"], "/用户私有目录");
    assert!(
        listed(&result)["ai_terminals"][0]["target"]
            .get("address")
            .is_none()
    );
    assert_eq!(listed(&result)["user_terminals"][0]["handed_over"], true);
    lock(&fixture.host.cwd_requests).clear();
    for other in [
        Entry::Windows,
        Entry::Server {
            server_id: "other".into(),
        },
    ] {
        let mut client = fixture.client(other);
        client.initialize("2025-11-25").await;
        let result = client.call("list_terminals", json!({})).await;
        assert_eq!(listed(&result)["ai_terminals"], json!([]));
        assert_eq!(listed(&result)["user_terminals"][0]["handed_over"], false);
        assert!(!result.to_string().contains("用户私有目录"));
        for tool in [
            "terminal_send",
            "terminal_read",
            "terminal_screen",
            "terminal_wait",
            "close_terminal",
        ] {
            let args = if tool == "terminal_send" {
                json!({"terminal_id":id,"keys":["Enter"]})
            } else {
                json!({"terminal_id":id})
            };
            assert_eq!(client.call(tool, args).await["error"]["code"], -32602);
        }
        client.close().await;
    }
    assert!(lock(&fixture.host.cwd_requests).is_empty());
    for (tool, args) in [
        ("terminal_send", json!({"terminal_id":id,"text":"hello\n"})),
        ("terminal_read", json!({"terminal_id":id})),
        ("terminal_screen", json!({"terminal_id":id})),
        ("terminal_wait", json!({"terminal_id":id,"quiet_ms":10})),
    ] {
        assert_eq!(owner.call(tool, args).await["result"]["isError"], false);
    }
    assert!(
        error_text(
            &owner
                .call("close_terminal", json!({"terminal_id":id}))
                .await
        )
        .contains("不能由 AI 关闭")
    );
    assert!(fixture.host.terminals.screen_text(&id).is_ok());
    owner.close().await;
    // 入口内连接断开不撤销交出，新的连接仍可使用。
    let mut owner = fixture.client(entry);
    owner.initialize("2025-11-25").await;
    assert_eq!(
        owner
            .call("terminal_screen", json!({"terminal_id":id}))
            .await["result"]["isError"],
        false
    );
    fixture.service.take_back(&id).unwrap();
    assert_eq!(
        owner
            .call("terminal_screen", json!({"terminal_id":id}))
            .await["error"]["code"],
        -32602
    );
    let result = owner.call("list_terminals", json!({})).await;
    assert_eq!(listed(&result)["ai_terminals"], json!([]));
    assert_eq!(listed(&result)["user_terminals"][0]["handed_over"], false);
    assert!(!result.to_string().contains("用户私有目录"));
    assert!(fixture.host.terminals.screen_text(&id).is_ok());
    assert!(lock(&fixture.host.events).iter().any(|event| matches!(event, HostEvent::TerminalHandedOver { terminal_id, .. } if terminal_id == &id)));
    assert!(lock(&fixture.host.events).iter().any(|event| matches!(event, HostEvent::TerminalTakenBack { terminal_id, .. } if terminal_id == &id)));
    owner.close().await;
}

#[test]
fn 交出校验入口权限目标终端存在和重复登记() {
    let fixture = Fixture::new();
    let id = user(&fixture, TargetKind::Windows);
    let target = target_for(&fixture, TargetKind::Windows);
    fixture.host.enabled.store(false, Ordering::Release);
    assert_eq!(
        fixture
            .service
            .hand_over(&id, Entry::Windows, target.clone())
            .unwrap_err()
            .code,
        "mcp.entry_disabled"
    );
    fixture.host.enabled.store(true, Ordering::Release);
    lock(&fixture.host.permissions).windows_commands = false;
    assert_eq!(
        fixture
            .service
            .hand_over(&id, Entry::Windows, target.clone())
            .unwrap_err()
            .code,
        "mcp.target_denied"
    );
    lock(&fixture.host.permissions).windows_commands = true;
    assert_eq!(
        fixture
            .service
            .hand_over(
                &id,
                Entry::Windows,
                target_for(&fixture, TargetKind::Server)
            )
            .unwrap_err()
            .code,
        "mcp.terminal_not_found"
    );
    assert_eq!(
        fixture
            .service
            .hand_over("不存在", Entry::Windows, target.clone())
            .unwrap_err()
            .code,
        "mcp.terminal_not_found"
    );
    hand_over(&fixture, &id, Entry::Windows, TargetKind::Windows);
    assert_eq!(
        fixture
            .service
            .hand_over(&id, Entry::Windows, target)
            .unwrap_err()
            .code,
        "mcp.terminal_registered"
    );
    fixture.service.take_back(&id).unwrap();
    assert_eq!(
        fixture.service.take_back(&id).unwrap_err().code,
        "mcp.terminal_not_found"
    );
    hand_over(&fixture, &id, Entry::Windows, TargetKind::Windows);
    assert!(
        fixture
            .host
            .terminals
            .write_from(&id, b"test", InputSource::Automation)
            .is_ok()
    );
}

#[tokio::test]
async fn 全部停止和服务释放只收回用户终端并写审计() {
    for stop_all in [true, false] {
        let fixture = Fixture::new();
        let id = user(&fixture, TargetKind::Windows);
        hand_over(&fixture, &id, Entry::Windows, TargetKind::Windows);
        let mut client = fixture.legacy().await;
        let opened = client
            .call("open_terminal", json!({"target":"windows"}))
            .await;
        let ai_id = listed(&opened)["terminal_id"].as_str().unwrap().to_owned();
        assert_eq!(
            fixture.service.take_back(&ai_id).unwrap_err().code,
            "mcp.terminal_not_handed_over"
        );
        client.close().await;
        let Fixture {
            service,
            host,
            _dir,
        } = fixture;
        if stop_all {
            service.stop_all();
        }
        drop(service);
        assert!(host.terminals.screen_text(&id).is_ok());
        until(|| host.terminals.screen_text(&ai_id).is_err()).await;
        let records: Vec<_> = lock(&host.events)
            .iter()
            .filter_map(|event| match event {
                HostEvent::Activity(record)
                    if ["hand_over", "take_back"].contains(&record.tool.as_str()) =>
                {
                    Some(record.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            records
                .iter()
                .map(|record| record.tool.as_str())
                .collect::<Vec<_>>(),
            ["hand_over", "take_back"]
        );
        for record in &records {
            assert_eq!(record.entry, Entry::Windows);
            assert_eq!(record.target.as_deref(), Some("windows"));
            assert_eq!(record.summary, id);
            assert_eq!(record.outcome, "ok");
        }
        let logs: String = std::fs::read_dir(_dir.path())
            .unwrap()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect();
        assert!(logs.contains("\"tool\":\"hand_over\"") && logs.contains("\"tool\":\"take_back\""));
        host.terminals.close(&id).unwrap();
    }
}

#[tokio::test]
async fn 用户终端退出自动注销并只发一次结束事件() {
    let fixture = Fixture::new();
    let id = user(&fixture, TargetKind::Server);
    hand_over(&fixture, &id, Entry::Windows, TargetKind::Server);
    fixture.host.terminals.write(&id, b"\x04").unwrap();
    until(|| {
        lock(&fixture.host.events)
            .iter()
            .any(|event| matches!(event, HostEvent::TerminalClosed(closed) if closed == &id))
    })
    .await;
    assert!(fixture.service.shared.registry.is_empty());
    fixture.service.shared.registry.reap(fixture.host.as_ref());
    assert_eq!(
        lock(&fixture.host.events)
            .iter()
            .filter(|event| matches!(event, HostEvent::TerminalClosed(closed) if closed == &id))
            .count(),
        1
    );
}

#[tokio::test]
async fn 用户按键作废待提交行且单独回车按内容未知确认() {
    for handed in [false, true] {
        for mode in [Approval::Dangerous, Approval::AskAll, Approval::Auto] {
            let fixture = Fixture::new();
            lock(&fixture.host.permissions).approval = mode;
            let mut client = fixture.legacy().await;
            let id = if handed {
                let id = user(&fixture, TargetKind::Windows);
                hand_over(&fixture, &id, Entry::Windows, TargetKind::Windows);
                id
            } else {
                let result = client
                    .call("open_terminal", json!({"target":"windows"}))
                    .await;
                listed(&result)["terminal_id"].as_str().unwrap().into()
            };
            client
                .call(
                    "terminal_send",
                    json!({"terminal_id":id,"text":"旧的待提交行"}),
                )
                .await;
            fixture.host.terminals.write(&id, b"user input").unwrap();
            // 中间一次非提交按键不能忘记内容未知状态。
            client
                .call("terminal_send", json!({"terminal_id":id,"keys":["Left"]}))
                .await;
            *lock(&fixture.host.action_decision) = Confirmation::Deny;
            for key in ["Enter", "Ctrl+M", "Ctrl+J"] {
                let result = client
                    .call("terminal_send", json!({"terminal_id":id,"keys":[key]}))
                    .await;
                if mode == Approval::Auto {
                    assert_eq!(result["result"]["isError"], false);
                    assert_eq!(fixture.service.activity(1)[0].approval, "auto");
                    // 新的用户活动让下一次回车仍属于未知提交。
                    fixture.host.terminals.write(&id, b"another input").unwrap();
                } else {
                    assert_eq!(error_text(&result), "用户拒绝执行");
                    assert_eq!(lock(&fixture.host.danger_flags).last(), Some(&true));
                }
                let record = &fixture.service.activity(1)[0];
                assert!(record.summary.contains("内容未知"));
                assert!(!record.summary.contains("旧的待提交行"));
                assert!(!record.summary.contains("user input"));
            }
            client.close().await;
        }
    }
}

#[tokio::test]
async fn 审批中用户按键使已批准的提交失效() {
    let fixture = Fixture::new();
    lock(&fixture.host.permissions).approval = Approval::AskAll;
    let mut client = fixture.legacy().await;
    let opened = client
        .call("open_terminal", json!({"target":"windows"}))
        .await;
    let id = listed(&opened)["terminal_id"].as_str().unwrap();
    *lock(&fixture.host.action_input) = Some((id.into(), b"user changed".to_vec()));
    let result = client
        .call(
            "terminal_send",
            json!({"terminal_id":id,"text":"echo ok","keys":["Enter"]}),
        )
        .await;
    assert_eq!(result["result"]["isError"], true);
    assert_eq!(
        fixture.service.activity(1)[0].outcome,
        "terminal.input_changed"
    );
    *lock(&fixture.host.action_decision) = Confirmation::Deny;
    assert_eq!(
        error_text(
            &client
                .call("terminal_send", json!({"terminal_id":id,"keys":["Enter"]}))
                .await
        ),
        "用户拒绝执行"
    );
    assert!(
        lock(&fixture.host.dangers)
            .last()
            .unwrap()
            .contains("内容未知")
    );
    client.close().await;
}

#[tokio::test]
async fn 收回取消等待且重新交出不会恢复旧调用() {
    let fixture = Fixture::new();
    let id = user(&fixture, TargetKind::Windows);
    hand_over(&fixture, &id, Entry::Windows, TargetKind::Windows);
    let mut client = fixture.legacy().await;
    client.id += 1;
    client.send(json!({"jsonrpc":"2.0","id":client.id,"method":"tools/call","params":{"name":"terminal_wait","arguments":{"terminal_id":id,"pattern":"不会出现","timeout_secs":30}}})).await;
    until(|| {
        lock(&fixture.service.shared.sessions)
            .values()
            .any(|session| session.permits.available_permits() < 8)
    })
    .await;
    fixture.service.take_back(&id).unwrap();
    hand_over(&fixture, &id, Entry::Windows, TargetKind::Windows);
    assert_eq!(
        error_text(&client.receive().await),
        "用户已收回这个终端：用 list_terminals 查看，或 open_terminal 新开"
    );
    assert_eq!(
        client
            .call("terminal_screen", json!({"terminal_id":id}))
            .await["result"]["isError"],
        false
    );
    client.close().await;
}

#[tokio::test]
async fn 收回后迟到的审批不能向重新交出的终端输入() {
    let fixture = Fixture::new();
    let id = user(&fixture, TargetKind::Windows);
    hand_over(&fixture, &id, Entry::Windows, TargetKind::Windows);
    fixture.host.action_blocked.store(true, Ordering::Release);
    let mut client = fixture.legacy().await;
    client.id += 1;
    client.send(json!({"jsonrpc":"2.0","id":client.id,"method":"tools/call","params":{"name":"terminal_send","arguments":{"terminal_id":id,"keys":["Enter"]}}})).await;
    until(|| !lock(&fixture.host.dangers).is_empty()).await;
    fixture.service.take_back(&id).unwrap();
    hand_over(&fixture, &id, Entry::Windows, TargetKind::Windows);
    fixture.host.action_blocked.store(false, Ordering::Release);
    assert_eq!(
        error_text(&client.receive().await),
        "用户已收回这个终端：用 list_terminals 查看，或 open_terminal 新开"
    );
    client.close().await;
}

#[tokio::test]
async fn 收回时正在等的确认立即结束() {
    let fixture = Fixture::new();
    let id = user(&fixture, TargetKind::Windows);
    hand_over(&fixture, &id, Entry::Windows, TargetKind::Windows);
    // 确认框一直不回答：收回后不用等用户回答，调用马上以取消结束
    fixture.host.action_blocked.store(true, Ordering::Release);
    let mut client = fixture.legacy().await;
    client.id += 1;
    client.send(json!({"jsonrpc":"2.0","id":client.id,"method":"tools/call","params":{"name":"terminal_send","arguments":{"terminal_id":id,"keys":["Enter"]}}})).await;
    until(|| !lock(&fixture.host.dangers).is_empty()).await;
    fixture.service.take_back(&id).unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), client.receive())
        .await
        .expect("收回后确认没有结束");
    assert_eq!(
        error_text(&reply),
        "用户已收回这个终端：用 list_terminals 查看，或 open_terminal 新开"
    );
    fixture.host.action_blocked.store(false, Ordering::Release);
    client.close().await;
}

#[tokio::test]
async fn 交出终端仍执行密码保护并立即遵守权限撤销() {
    let fixture = Fixture::new();
    lock(&fixture.host.permissions).approval = Approval::Auto;
    let id = user(&fixture, TargetKind::Windows);
    hand_over(&fixture, &id, Entry::Windows, TargetKind::Windows);
    fixture.host.terminals.write(&id, b"Password:\r").unwrap();
    until(|| {
        fixture
            .host
            .terminals
            .screen_text(&id)
            .is_ok_and(|screen| safety::looks_like_password_prompt(&screen))
    })
    .await;
    let mut client = fixture.legacy().await;
    let result = client
        .call(
            "terminal_send",
            json!({"terminal_id":id,"text":"不能进入审计的密码"}),
        )
        .await;
    assert!(error_text(&result).contains("终端正在等待密码"));
    assert_eq!(fixture.service.activity(1)[0].summary, "拒绝");
    assert_eq!(fixture.service.activity(1)[0].approval, "");
    assert_eq!(
        client
            .call("terminal_send", json!({"terminal_id":id,"keys":["Ctrl+C"]}))
            .await["result"]["isError"],
        false
    );
    lock(&fixture.host.permissions).windows_commands = false;
    lock(&fixture.host.cwd_requests).clear();
    let result = client.call("list_terminals", json!({})).await;
    assert_eq!(
        listed(&result),
        &json!({"ai_terminals":[],"user_terminals":[]})
    );
    assert!(lock(&fixture.host.cwd_requests).is_empty());
    assert_eq!(
        client
            .call("terminal_read", json!({"terminal_id":id}))
            .await["error"]["code"],
        -32602
    );
    client.close().await;
}

#[test]
fn 交出和收回审计使用已有秘密脱敏规则() {
    let fixture = Fixture::new();
    let id = user(&fixture, TargetKind::Windows);
    fixture
        .service
        .add_secret(zeroize::Zeroizing::new(id.clone()));
    hand_over(&fixture, &id, Entry::Windows, TargetKind::Windows);
    fixture.service.take_back(&id).unwrap();
    let records = fixture.service.activity(2);
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| record.summary == "***"));
    let logs: String = std::fs::read_dir(fixture._dir.path())
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    assert!(!logs.contains(&id));
}

#[test]
fn 连续收回不会被尚未返回的调用占满订阅() {
    let fixture = Fixture::new();
    let id = user(&fixture, TargetKind::Windows);
    let targets = vec![target_for(&fixture, TargetKind::Windows)];
    let mut pending_calls = Vec::new();
    for _ in 0..40 {
        hand_over(&fixture, &id, Entry::Windows, TargetKind::Windows);
        pending_calls.push(
            fixture
                .service
                .shared
                .registry
                .get(&id, &Entry::Windows, &targets)
                .unwrap(),
        );
        fixture.service.take_back(&id).unwrap();
    }
    assert!(fixture.host.terminals.screen_text(&id).is_ok());
    assert_eq!(pending_calls.len(), 40);
}
