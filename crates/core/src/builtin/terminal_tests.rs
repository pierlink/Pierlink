//! 内存对端验证终端经过功能分派、共享主连接和真实 SSH 通道。
use super::*;
use crate::terminal::{
    self, InputSource, TerminalEvent, TerminalKind, TerminalManager, TerminalSpec,
};
use pierlink_ssh_engine::test_support::{TerminalRequest, connect_terminal};
use std::sync::mpsc;

const MARKER: &str = "0123456789abcdef0123456789abcdef";

fn fixture() -> (Fixture, Arc<Mutex<Vec<TerminalRequest>>>) {
    fixture_with_sshid(None)
}

fn fixture_with_sshid(sshid: Option<&str>) -> (Fixture, Arc<Mutex<Vec<TerminalRequest>>>) {
    let mut f = Fixture::new(AuthMethod::Password);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = requests.clone();
    let sshid = sshid.map(str::to_owned);
    let engine = Engine::new().unwrap();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
        connect_terminal(&engine, options, auth, observed.clone(), sshid.as_deref())
    }));
    (f, requests)
}

fn open(f: &Fixture, manager: &TerminalManager) -> (String, mpsc::Receiver<TerminalEvent>) {
    open_with_integration(f, manager, false)
}

fn open_with_integration(
    f: &Fixture,
    manager: &TerminalManager,
    integration: bool,
) -> (String, mpsc::Receiver<TerminalEvent>) {
    let (tx, rx) = mpsc::channel();
    let mut request = f.request(false);
    request.class = ConnectionClass::Bulk;
    request.progress = Some(Arc::new(|_| {}));
    let kind = terminal::remote(
        &f.manager,
        request,
        Some(MARKER.into()),
        integration,
        || panic!("不能调用 OpenSSH"),
    )
    .unwrap();
    let (id, registration) = manager
        .open_with_registration(
            TerminalSpec {
                kind,
                cols: 80,
                rows: 24,
            },
            Arc::new(move |e| {
                let _ = tx.send(e);
            }),
        )
        .unwrap();
    assert!(registration.is_none());
    (id, rx)
}

fn output(
    manager: &TerminalManager,
    id: &str,
    rx: &mpsc::Receiver<TerminalEvent>,
    expected: &[u8],
) {
    let mut actual = Vec::new();
    while actual.len() < expected.len() {
        let TerminalEvent::Output(bytes) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("输出前意外退出")
        };
        manager.ack(id, bytes.len()).unwrap();
        actual.extend(bytes);
    }
    assert_eq!(actual, expected);
}

fn exited(rx: &mpsc::Receiver<TerminalEvent>) -> (Option<i32>, bool, bool) {
    loop {
        if let TerminalEvent::Exited {
            code,
            closed_by_user,
            hooked,
        } = rx.recv_timeout(Duration::from_secs(5)).unwrap()
        {
            return (code, closed_by_user, hooked);
        }
    }
}

#[test]
fn 内置终端对windows服务器的集成请求与关闭集成逐字节相同() {
    for shell in [None, Some("/bin/bash")] {
        let sshid = "SSH-2.0-OpenSSH_for_Windows_9.5";
        let (mut f, requests) = fixture_with_sshid(Some(sshid));
        f.server.login_shell = shell.map(str::to_owned);
        let manager = TerminalManager::new();
        let (first, first_rx) = open_with_integration(&f, &manager, true);
        let integrated = requests.lock().unwrap().clone();
        assert_eq!(
            integrated.last(),
            Some(&match shell {
                None => TerminalRequest::Shell,
                Some(_) => TerminalRequest::Exec(b"exec '/bin/bash' -l".to_vec()),
            })
        );
        let connection = f.manager.get_connection(f.request(false)).unwrap();
        assert_eq!(connection.remote_sshid(), Some(sshid));

        let (second, second_rx) = open(&f, &manager);
        assert_eq!(&requests.lock().unwrap()[integrated.len()..], integrated);
        let reused = f.manager.get_connection(f.request(false)).unwrap();
        assert!(connection.same_connection(&reused));
        assert_eq!(reused.remote_sshid(), Some(sshid));
        assert_eq!(connection.statistics().opened_sessions, 2);
        assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 1);

        manager.close(&first).unwrap();
        manager.close(&second).unwrap();
        assert!(matches!(exited(&first_rx), (_, true, false)));
        assert!(matches!(exited(&second_rx), (_, true, false)));
    }
}

#[test]
fn 内置终端对普通openssh服务器保留集成命令且仍可关闭() {
    for shell in [None, Some("/bin/bash")] {
        let sshid = "SSH-2.0-OpenSSH_9.6p1 Ubuntu-3ubuntu13";
        let (mut f, requests) = fixture_with_sshid(Some(sshid));
        f.server.login_shell = shell.map(str::to_owned);
        let manager = TerminalManager::new();
        let (id, rx) = open_with_integration(&f, &manager, true);
        let expected = crate::shell_integration::command(shell).unwrap();
        assert!(expected.starts_with("exec /bin/sh -c '"));
        assert_eq!(
            requests.lock().unwrap().last(),
            Some(&TerminalRequest::Exec(expected.into_bytes()))
        );
        let connection = f.manager.get_connection(f.request(false)).unwrap();
        assert_eq!(connection.remote_sshid(), Some(sshid));

        let (plain, plain_rx) = open(&f, &manager);
        assert_eq!(
            requests.lock().unwrap().last(),
            Some(&match shell {
                None => TerminalRequest::Shell,
                Some(_) => TerminalRequest::Exec(b"exec '/bin/bash' -l".to_vec()),
            })
        );
        assert_eq!(connection.statistics().opened_sessions, 2);
        manager.close(&id).unwrap();
        manager.close(&plain).unwrap();
        assert!(matches!(exited(&rx), (_, true, true)));
        assert!(matches!(exited(&plain_rx), (_, true, false)));
    }
}

#[test]
fn 内置终端复用主连接发送目录标记与登录shell请求() {
    let (f, requests) = fixture();
    let connection = f.manager.get_connection(f.request(true)).unwrap();
    let manager = TerminalManager::new();
    let (id, rx) = open(&f, &manager);
    assert_eq!(connection.statistics().opened_sessions, 1);
    assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 1);
    assert_eq!(manager.busy(&id).unwrap(), None);
    assert_eq!(
        *requests.lock().unwrap(),
        vec![
            TerminalRequest::Env("LC_PIERLINK_TERM".into(), MARKER.into()),
            TerminalRequest::Pty("xterm-256color".into(), 80, 24),
            TerminalRequest::Shell,
        ]
    );
    manager.resize(&id, 100, 40).unwrap();
    manager.resize(&id, 100, 40).unwrap();
    let bytes = "中文😀\x1b[?2004h\x1b[200~a\nb\x1b[201~\x03\x1b]52;c;abc\x07\x1b]8;;https://example.test\x07".as_bytes();
    manager.write(&id, bytes).unwrap();
    output(&manager, &id, &rx, bytes);
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, TerminalRequest::Resize(100, 40)))
            .count(),
        1
    );
    manager.write(&id, b"\x04").unwrap();
    output(&manager, &id, &rx, b"bye");
    assert_eq!(exited(&rx), (Some(7), false, false));
    assert!(!connection.is_closed());
}

#[test]
fn 内置输出多订阅屏幕查询及输入三种状态() {
    let (f, _) = fixture();
    let manager = TerminalManager::new();
    let (id, rx) = open(&f, &manager);
    let fast = manager.subscribe(&id, 4096).unwrap();
    let slow = manager.subscribe(&id, 2).unwrap();
    manager
        .write_from(&id, b"auto", InputSource::Automation)
        .unwrap();
    output(&manager, &id, &rx, b"auto");
    assert!(!fast.recv_timeout(Duration::from_secs(1)).unwrap().lost);
    manager.write(&id, b"user").unwrap();
    manager
        .write_from(&id, b"queued", InputSource::Automation)
        .unwrap();
    output(&manager, &id, &rx, b"user");
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    output(&manager, &id, &rx, b"queued");
    assert!(slow.recv_timeout(Duration::ZERO).unwrap().lost);
    let cursor = manager.output_since(&id, 0).unwrap();
    assert_eq!(cursor.data, b"autouserqueued");
    assert_eq!(manager.screen_text(&id).unwrap(), "autouserqueued");
    assert_eq!(manager.recent_lines(&id, 1).unwrap(), ["autouserqueued"]);
    manager
        .write(&id, b"\x1b[?1049h\x1b[2J\x1b[Htop\x1b[2;1HCPU")
        .unwrap();
    output(
        &manager,
        &id,
        &rx,
        b"\x1b[?1049h\x1b[2J\x1b[Htop\x1b[2;1HCPU",
    );
    assert_eq!(manager.screen_text(&id).unwrap(), "top\nCPU");
    assert!(
        !manager
            .output_since(&id, cursor.next)
            .unwrap()
            .data
            .is_empty()
    );
    manager
        .write_from(&id, b"discarded", InputSource::Automation)
        .unwrap();
    manager.set_user_control(&id, true).unwrap();
    assert_eq!(
        manager
            .write_from(&id, b"denied", InputSource::Automation)
            .unwrap_err()
            .code,
        "terminal.automation_rejected"
    );
    manager.write(&id, b"\x1b[?1049l").unwrap();
    output(&manager, &id, &rx, b"\x1b[?1049l");
    assert_eq!(manager.screen_text(&id).unwrap(), "autouserqueued");
    manager.set_user_control(&id, false).unwrap();
    manager
        .write_from(&id, b"resumed", InputSource::Automation)
        .unwrap();
    output(&manager, &id, &rx, b"resumed");
    fast.unsubscribe();
    manager.close(&id).unwrap();
    assert!(exited(&rx).1);
}

#[test]
fn 关闭只结束本通道而断开主连接产生唯一退出提示() {
    let (f, _) = fixture();
    let manager = TerminalManager::new();
    let (first, first_rx) = open(&f, &manager);
    let (second, second_rx) = open(&f, &manager);
    let subscriber = manager.subscribe(&second, 64).unwrap();
    manager.close(&first).unwrap();
    assert!(exited(&first_rx).1);
    manager.write(&second, b"alive").unwrap();
    output(&manager, &second, &second_rx, b"alive");
    f.manager.close_server(&f.server.id).unwrap();
    assert_eq!(exited(&second_rx), (Some(255), false, false));
    assert!(!matches!(
        second_rx.try_recv(),
        Ok(TerminalEvent::Exited { .. })
    ));
    assert!(matches!(
        subscriber.recv_timeout(Duration::ZERO).unwrap().event,
        TerminalEvent::Output(_)
    ));
    assert!(matches!(
        subscriber.recv_timeout(Duration::ZERO).unwrap().event,
        TerminalEvent::Exited {
            code: Some(255),
            closed_by_user: false,
            hooked: false,
        }
    ));
}

#[test]
fn 内存对端断开时终端退出码标明连接中断() {
    let (f, _) = fixture();
    let manager = TerminalManager::new();
    let (id, rx) = open(&f, &manager);
    manager.write(&id, b"\x1d").unwrap();
    assert_eq!(exited(&rx), (Some(255), false, false));
    assert!(!matches!(rx.try_recv(), Ok(TerminalEvent::Exited { .. })));
}

#[test]
fn 内置终端界面背压有界且慢订阅不阻塞同连接其他终端() {
    let (f, _) = fixture();
    let manager = TerminalManager::new();
    let (id, rx) = open(&f, &manager);
    let slow = manager.subscribe(&id, 1024).unwrap();
    let window = 1024 * 1024;
    manager.write(&id, &vec![b'x'; window]).unwrap();
    let mut received = 0;
    while received < window {
        let TerminalEvent::Output(bytes) = rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("收到 {received}/{window} 字节后等待输出失败：{e}"))
        else {
            panic!("输出提前结束")
        };
        received += bytes.len();
    }
    assert_eq!(received, window);
    manager.write(&id, &vec![b'y'; window]).unwrap();
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    let (other, other_rx) = open(&f, &manager);
    manager.write(&other, b"responsive").unwrap();
    output(&manager, &other, &other_rx, b"responsive");
    manager.ack(&id, window).unwrap();
    received = 0;
    while received < window {
        let TerminalEvent::Output(bytes) = rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("收到 {received}/{window} 字节后等待输出失败：{e}"))
        else {
            panic!("输出提前结束")
        };
        assert!(bytes.iter().all(|b| *b == b'y'));
        received += bytes.len();
        manager.ack(&id, bytes.len()).unwrap();
    }
    assert!(slow.recv_timeout(Duration::ZERO).unwrap().lost);
    manager.close_all();
    assert!(exited(&rx).1);
    assert!(exited(&other_rx).1);
}

#[test]
fn 指定登录shell安全转义且系统模式原样回退() {
    let (mut f, requests) = fixture();
    f.server.login_shell = Some("/bin/bash".into());
    let manager = TerminalManager::new();
    let (id, rx) = open(&f, &manager);
    assert!(
        requests
            .lock()
            .unwrap()
            .contains(&TerminalRequest::Exec(b"exec '/bin/bash' -l".to_vec()))
    );
    manager.close(&id).unwrap();
    assert!(exited(&rx).1);
    for shell in ["/bin/sh;id", "$(id)", "x\0secret", "-sh", ""] {
        f.server.login_shell = Some(shell.into());
        assert!(
            terminal::remote(&f.manager, f.request(true), None, false, || panic!(
                "不能回退"
            ))
            .is_err()
        );
    }
    f.server.login_shell = None;
    assert!(
        terminal::remote(
            &f.manager,
            f.request(true),
            Some("无效标记".into()),
            false,
            || panic!("不能回退")
        )
        .is_err()
    );
    for alias in [false, true] {
        f.server.connection_engine = if alias {
            crate::storage::Engine::Builtin
        } else {
            crate::storage::Engine::OpenSsh
        };
        if alias {
            f.server.ssh_alias = Some("dev".into());
        }
        let mut called = false;
        let kind = terminal::remote(&f.manager, f.request(true), None, false, || {
            called = true;
            Ok(TerminalKind::Local {
                program: "原路径".into(),
                args: vec!["原参数".into()],
                cwd: None,
                env: vec![],
            })
        })
        .unwrap();
        assert!(called);
        assert!(matches!(kind, TerminalKind::Local { args, .. } if args == ["原参数"]));
    }
}

#[test]
fn 内置终端阶段回调与事件支持复用且就绪先于退出() {
    use terminal::ConnectStage::*;
    let (f, _) = fixture();
    let manager = TerminalManager::new();
    for expected in [vec![Auth, Session, Ready], vec![Session, Ready]] {
        let (tx, rx) = mpsc::channel();
        let kind = terminal::remote(&f.manager, f.request(true), None, false, || panic!()).unwrap();
        let (id, _) = manager
            .open_with_registration(
                TerminalSpec {
                    kind,
                    cols: 80,
                    rows: 24,
                },
                Arc::new(move |e| {
                    let _ = tx.send(e);
                }),
            )
            .unwrap();
        let stages: Vec<_> = rx
            .try_iter()
            .map(|event| match event {
                TerminalEvent::Stage(stage) => stage,
                event => panic!("意外事件：{event:?}"),
            })
            .collect();
        // 内存对端跳过 DNS 和 TCP，但真实握手仍进入认证。
        assert_eq!(stages, expected);
        manager.close(&id).unwrap();
        assert!(exited(&rx).1);
    }
    let observed = Arc::new(Mutex::new(Vec::new()));
    let stages = observed.clone();
    let mut request = f.request(true);
    request.progress = Some(Arc::new(move |stage| stages.lock().unwrap().push(stage)));
    let kind = terminal::remote(&f.manager, request, None, false, || panic!()).unwrap();
    let id = manager
        .open(
            TerminalSpec {
                kind,
                cols: 80,
                rows: 24,
            },
            Arc::new(|event| assert!(!matches!(event, TerminalEvent::Stage(_)))),
        )
        .unwrap();
    assert_eq!(*observed.lock().unwrap(), [Session, Ready]);
    manager.close(&id).unwrap();
}

#[test]
fn 内置终端取得密码后才开始建立连接() {
    let (f, requests) = fixture();
    let engine = Engine::new().unwrap();
    let callbacks = f.callbacks.clone();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
        assert_eq!(callbacks.passwords.load(Ordering::SeqCst), 1);
        connect_terminal(&engine, options, auth, requests.clone(), None)
    }));
    let manager = TerminalManager::new();
    let (id, rx) = open(&f, &manager);
    assert_eq!(f.callbacks.passwords.load(Ordering::SeqCst), 1);
    manager.close(&id).unwrap();
    assert!(exited(&rx).1);
}

#[test]
fn 内置终端密码错误停在认证且取消密码不报告阶段() {
    use terminal::ConnectStage::*;
    for cancel in [false, true] {
        let (mut f, _) = fixture();
        if cancel {
            f.callbacks = Arc::new(Callbacks {
                cancel: true,
                ..Default::default()
            });
        } else {
            *f.callbacks.password.lock().unwrap() = Some("错误密码".into());
        }
        let (tx, rx) = mpsc::channel();
        let kind = terminal::remote(&f.manager, f.request(true), None, false, || panic!()).unwrap();
        let result = TerminalManager::new().open(
            TerminalSpec {
                kind,
                cols: 80,
                rows: 24,
            },
            Arc::new(move |e| {
                let _ = tx.send(e);
            }),
        );
        assert_eq!(
            result.unwrap_err().code,
            if cancel {
                "ssh.auth_cancelled"
            } else {
                "ssh.auth_failed"
            }
        );
        let expected = if cancel {
            vec![]
        } else {
            vec![TerminalEvent::Stage(Auth)]
        };
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), expected);
        if cancel {
            assert!(f.callbacks.host_keys.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn 打开通道时取消停在会话阶段并保留共享连接() {
    use terminal::ConnectStage::*;
    let (f, _) = fixture();
    let mut request = f.request(true);
    let cancelled = request.cancelled.clone();
    let stages = Arc::new(Mutex::new(Vec::new()));
    let observed = stages.clone();
    request.progress = Some(Arc::new(move |stage| {
        observed.lock().unwrap().push(stage);
        if stage == Session {
            cancelled.store(true, Ordering::Release);
        }
    }));
    let kind = terminal::remote(&f.manager, request, None, false, || panic!()).unwrap();
    let result = TerminalManager::new().open(
        TerminalSpec {
            kind,
            cols: 80,
            rows: 24,
        },
        Arc::new(|_| {}),
    );
    assert_eq!(result.unwrap_err().code, "ssh.auth_cancelled");
    assert_eq!(*stages.lock().unwrap(), [Auth, Session]);
    f.cancelled.store(false, Ordering::Release);
    f.manager
        .get_connection(f.request(false))
        .unwrap()
        .ping()
        .unwrap();
}
