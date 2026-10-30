use super::*;

#[derive(Default)]
struct MockState {
    connected: bool,
    paused: bool,
    patches: Vec<Value>,
    fail_pause: bool,
    fail_resume: usize,
    fail_read: usize,
    malformed_config: bool,
    malformed: bool,
    address: Option<String>,
    fail_address: usize,
}

fn mock(id: &'static str, peer: &'static str) -> (Client, Arc<Mutex<MockState>>) {
    let state = Arc::new(Mutex::new(MockState::default()));
    let data = state.clone();
    let mut client = Client::new("http://127.0.0.1:1", "不可泄露的测试密钥").unwrap();
    client.test_handler = Some(Arc::new(move |method, path, query, body| {
        assert!(query.is_empty() || path == "/rest/events");
        let mut state = data.lock().unwrap();
        match (method, path) {
            ("GET", "/rest/system/status") => {
                Ok(json!({"myID": id, "startTime": "2026-10-24T00:00:00Z"}))
            }
            ("GET", "/rest/events") => Ok(json!([])),
            ("GET", path) if path == format!("/rest/config/devices/{peer}") => {
                if state.fail_read > 0 {
                    state.fail_read -= 1;
                    Err(AppError::new("syncthing.unreachable"))
                } else if state.malformed_config {
                    Ok(json!(null))
                } else {
                    Ok(
                        json!({"paused": state.paused, "addresses": [state.address.as_deref().unwrap_or("tcp://127.0.0.1:12345")]}),
                    )
                }
            }
            ("GET", "/rest/system/connections") => {
                if state.malformed {
                    Ok(json!({}))
                } else {
                    Ok(
                        json!({"connections": {peer: {"connected": state.connected, "paused": state.paused}}}),
                    )
                }
            }
            ("PATCH", path) if path == format!("/rest/config/devices/{peer}") => {
                let body = body.unwrap();
                state.patches.push(body.clone());
                if let Some(address) = body["addresses"][0].as_str() {
                    state.address = Some(address.into());
                    if state.fail_address > 0 {
                        state.fail_address -= 1;
                        return Err(AppError::new("syncthing.unreachable"));
                    }
                }
                if let Some(paused) = body["paused"].as_bool() {
                    if !paused && state.fail_resume > 0 {
                        state.fail_resume -= 1;
                        return Err(AppError::new("syncthing.unreachable"));
                    }
                    state.paused = paused;
                    if paused && state.fail_pause {
                        return Err(AppError::new("syncthing.unreachable"));
                    }
                }
                Ok(Value::Null)
            }
            _ => panic!("不应扫描、重启、改其他设备或文件夹：{method} {path}"),
        }
    }));
    (client, state)
}

fn recovery(local: Client) -> ReconnectState {
    ReconnectState {
        local,
        address_updates: Default::default(),
        local_id: "本电脑".into(),
        remote_id: "服务器".into(),
        api_key: "不可泄露的测试密钥".into(),
        resume_remote: false,
        remote: None,
    }
}

#[test]
fn 仅重拨离线目标设备_远端旧连接释放_地址变化() {
    let (local, a) = mock("本电脑", "服务器");
    let (remote, b) = mock("服务器", "本电脑");
    b.lock().unwrap().connected = true;
    recovery(local).recover(&remote, 23456).unwrap();
    assert_eq!(
        b.lock().unwrap().patches,
        [json!({"paused": true}), json!({"paused": false})]
    );
    assert_eq!(
        a.lock().unwrap().patches,
        [json!({"addresses": ["tcp4://127.0.0.1:23456"]})]
    );
}

#[test]
fn 重拨连续交替地址_仅写本机目标地址() {
    let (local, a) = mock("本电脑", "服务器");
    let (remote, b) = mock("服务器", "本电脑");
    let mut recovery = recovery(local);
    for _ in 0..3 {
        recovery.recover(&remote, 12345).unwrap();
    }
    assert_eq!(
        a.lock().unwrap().patches,
        [
            json!({"addresses": ["tcp4://127.0.0.1:12345"]}),
            json!({"addresses": ["tcp://127.0.0.1:12345"]}),
            json!({"addresses": ["tcp4://127.0.0.1:12345"]}),
        ]
    );
    assert!(!a.lock().unwrap().paused);
    assert!(b.lock().unwrap().patches.is_empty());
}

#[test]
fn 地址响应丢失后重读实际方案_读取失败不写配置() {
    let (local, a) = mock("本电脑", "服务器");
    let (remote, _) = mock("服务器", "本电脑");
    a.lock().unwrap().fail_address = 1;
    let mut recovery = recovery(local);
    assert_eq!(
        recovery.recover(&remote, 12345).unwrap_err().code,
        "syncthing.unreachable"
    );
    recovery.recover(&remote, 23456).unwrap();
    assert_eq!(
        a.lock().unwrap().address.as_deref(),
        Some("tcp://127.0.0.1:23456")
    );
    for malformed in [false, true] {
        a.lock().unwrap().fail_read = if malformed { 0 } else { 1 };
        a.lock().unwrap().malformed_config = malformed;
        assert!(recovery.recover(&remote, 23456).is_err());
    }
    assert_eq!(a.lock().unwrap().patches.len(), 2);
    assert!(!a.lock().unwrap().paused);
}

#[test]
fn 已连接或用户暂停不打断_身份异常不发送配置() {
    for case in ["在线", "本机暂停", "远端暂停", "身份异常", "无连接状态"] {
        let (local, a) = mock("本电脑", "服务器");
        let (remote, b) = mock(
            if case == "身份异常" {
                "其他服务器"
            } else {
                "服务器"
            },
            "本电脑",
        );
        match case {
            "在线" => {
                a.lock().unwrap().connected = true;
                b.lock().unwrap().connected = true;
            }
            "本机暂停" => a.lock().unwrap().paused = true,
            "远端暂停" => b.lock().unwrap().paused = true,
            "无连接状态" => a.lock().unwrap().malformed = true,
            _ => {}
        }
        let result = recovery(local).recover(&remote, 12345);
        if case == "身份异常" {
            assert_eq!(result.unwrap_err().code, "sync.device_mismatch");
        } else if case == "无连接状态" {
            assert_eq!(result.unwrap_err().code, "sync.invalid_response");
        } else {
            result.unwrap();
        }
        assert!(a.lock().unwrap().patches.is_empty());
        assert!(b.lock().unwrap().patches.is_empty());
    }
}

#[test]
fn 暂停响应丢失也恢复_恢复失败重试并报告() {
    let (local, a) = mock("本电脑", "服务器");
    let (remote, b) = mock("服务器", "本电脑");
    b.lock().unwrap().connected = true;
    b.lock().unwrap().fail_pause = true;
    let mut recovery = recovery(local);
    assert_eq!(
        recovery.recover(&remote, 12345).unwrap_err().code,
        "syncthing.unreachable"
    );
    assert!(!b.lock().unwrap().paused);
    assert!(a.lock().unwrap().patches.is_empty());
    b.lock().unwrap().fail_resume = 4;
    let error = recovery.recover(&remote, 12345).unwrap_err();
    assert_eq!(error.code, "sync.cleanup_failed");
    assert!(!format!("{error:?}").contains("不可泄露"));
    assert!(recovery.resume_remote);
    recovery.resume_pending().unwrap();
    assert!(!recovery.resume_remote);
    assert!(!b.lock().unwrap().paused);
    assert!(a.lock().unwrap().patches.is_empty());
}

#[test]
fn 远端恢复失败不暂停本机_停止时恢复遗留暂停() {
    let (local, a) = mock("本电脑", "服务器");
    let (remote, b) = mock("服务器", "本电脑");
    b.lock().unwrap().fail_resume = 3;
    b.lock().unwrap().paused = true;
    let mut recovery = recovery(local);
    recovery.remote = Some(remote);
    recovery.resume_remote = true;
    assert_eq!(
        recovery.resume_pending().unwrap_err().code,
        "sync.cleanup_failed"
    );
    assert!(!a.lock().unwrap().paused);
    assert!(a.lock().unwrap().patches.is_empty());
    b.lock().unwrap().paused = true;
    let count = b.lock().unwrap().patches.len();
    let reconnect = Reconnector {
        state: Arc::new(Mutex::new(recovery)),
        stopping: Arc::new(AtomicBool::new(false)),
        worker: Mutex::new(None),
    };
    reconnect.stop().unwrap();
    assert!(!reconnect.state.lock().unwrap().resume_remote);
    assert!(!b.lock().unwrap().paused);
    assert_eq!(b.lock().unwrap().patches.len(), count + 1);
}

#[test]
fn 远端遗留标记遇到本机暂停也恢复() {
    for local_paused in [false, true] {
        let (local, a) = mock("本电脑", "服务器");
        let (remote, b) = mock("服务器", "本电脑");
        let mut recovery = recovery(local);
        recovery.resume_remote = true;
        b.lock().unwrap().paused = true;
        a.lock().unwrap().paused = local_paused;
        recovery.recover(&remote, 12345).unwrap();
        assert!(!recovery.resume_remote);
        assert_eq!(a.lock().unwrap().paused, local_paused);
        assert!(!b.lock().unwrap().paused);
        assert_eq!(b.lock().unwrap().patches, [json!({"paused": false})]);
    }
}

#[test]
fn 恢复前读取失败保留远端标记_本机不受影响() {
    for malformed in [false, true] {
        let (local, a) = mock("本电脑", "服务器");
        let (remote, b) = mock("服务器", "本电脑");
        b.lock().unwrap().paused = true;
        b.lock().unwrap().fail_read = if malformed { 0 } else { 3 };
        b.lock().unwrap().malformed_config = malformed;
        let mut recovery = recovery(local);
        recovery.remote = Some(remote);
        recovery.resume_remote = true;
        assert_eq!(
            recovery.resume_pending().unwrap_err().code,
            "sync.cleanup_failed"
        );
        assert!(recovery.resume_remote);
        assert!(!a.lock().unwrap().paused && b.lock().unwrap().paused);
        assert!(a.lock().unwrap().patches.is_empty());
        assert!(b.lock().unwrap().patches.is_empty());
        b.lock().unwrap().malformed_config = false;
        b.lock().unwrap().fail_read = 0;
        recovery.resume_pending().unwrap();
        assert!(!recovery.resume_remote);
        assert!(!b.lock().unwrap().paused);
    }
}

#[test]
fn 已由用户恢复的待处理设备无需再次恢复() {
    let (local, _) = mock("本电脑", "服务器");
    let (remote, state) = mock("服务器", "本电脑");
    let mut recovery = recovery(local);
    recovery.remote = Some(remote);
    recovery.resume_remote = true;
    recovery.resume_pending().unwrap();
    assert!(!recovery.resume_remote);
    assert!(state.lock().unwrap().patches.is_empty());
}

#[test]
fn 探活只对持续不可达的隧道请求重建() {
    for code in [
        None,
        Some("syncthing.unreachable"),
        Some("syncthing.unauthorized"),
        Some("syncthing.invalid_response"),
    ] {
        let (control, received, worker) = crate::tunnel::test_reconnect_control();
        let mut remote = Client::new("http://127.0.0.1:1", "test-key").unwrap();
        remote.test_handler = Some(Arc::new(move |method, path, _, _| {
            assert_eq!((method, path), ("GET", "/rest/system/ping"));
            code.map_or_else(
                || Ok(json!({"ping": "pong"})),
                |code| Err(AppError::new(code)),
            )
        }));
        let mut since = if code.is_none() {
            Some(Instant::now() - UNREACHABLE_REBUILD_AFTER)
        } else {
            None
        };
        let result = remote_reachable(&control, "同步", Some(123), &remote, &mut since);
        match code {
            None => {
                assert!(result.unwrap());
                assert!(since.is_none(), "探活成功清除不可达起点");
            }
            Some("syncthing.unreachable") => {
                // 刚开始不可达只重试，不重建（远端管理套接字可能还没就绪）
                assert_eq!(result.unwrap_err().code, "syncthing.unreachable");
                assert!(since.is_some());
                since = Some(Instant::now() - UNREACHABLE_REBUILD_AFTER);
                assert!(
                    !remote_reachable(&control, "同步", Some(123), &remote, &mut since).unwrap()
                );
            }
            Some(code) => assert_eq!(result.unwrap_err().code, code),
        }
        drop(control);
        worker.join().unwrap();
        let requests: Vec<_> = received.try_iter().collect();
        assert_eq!(
            requests,
            match code {
                Some("syncthing.unreachable") => vec![("同步".into(), Some(123), false)],
                None => vec![("同步".into(), Some(123), true)],
                _ => vec![],
            }
        );
    }
}

// 两个真实 Syncthing 实例，经可断开的 TCP 转发模拟同步隧道。
#[cfg(unix)]
mod real;
