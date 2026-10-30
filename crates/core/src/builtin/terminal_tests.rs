//! 内存对端验证终端经过功能分派、共享主连接和真实 SSH 通道。
use super::*;
use crate::terminal::{
    self, InputSource, TerminalEvent, TerminalKind, TerminalManager, TerminalSpec,
};
use pierlink_ssh_engine::test_support::{TerminalRequest, connect_terminal};
use std::sync::mpsc;

const MARKER: &str = "0123456789abcdef0123456789abcdef";

fn fixture() -> (Fixture, Arc<Mutex<Vec<TerminalRequest>>>) {
    let mut f = Fixture::new(AuthMethod::Password);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = requests.clone();
    let engine = Engine::new().unwrap();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
        connect_terminal(&engine, options, auth, observed.clone())
    }));
    (f, requests)
}

fn open(f: &Fixture, manager: &TerminalManager) -> (String, mpsc::Receiver<TerminalEvent>) {
    let (tx, rx) = mpsc::channel();
    let mut request = f.request(false);
    request.class = ConnectionClass::Bulk;
    let kind = terminal::remote(&f.manager, request, Some(MARKER.into()), || {
        panic!("不能调用 OpenSSH")
    })
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

fn exited(rx: &mpsc::Receiver<TerminalEvent>) -> (Option<i32>, bool) {
    loop {
        if let TerminalEvent::Exited {
            code,
            closed_by_user,
        } = rx.recv_timeout(Duration::from_secs(5)).unwrap()
        {
            return (code, closed_by_user);
        }
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
    assert_eq!(exited(&rx), (Some(7), false));
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
    assert_eq!(exited(&second_rx), (Some(255), false));
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
            closed_by_user: false
        }
    ));
}

#[test]
fn 内存对端断开时终端退出码标明连接中断() {
    let (f, _) = fixture();
    let manager = TerminalManager::new();
    let (id, rx) = open(&f, &manager);
    manager.write(&id, b"\x1d").unwrap();
    assert_eq!(exited(&rx), (Some(255), false));
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
            terminal::remote(&f.manager, f.request(true), None, || panic!("不能回退")).is_err()
        );
    }
    f.server.login_shell = None;
    assert!(
        terminal::remote(
            &f.manager,
            f.request(true),
            Some("无效标记".into()),
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
        let kind = terminal::remote(&f.manager, f.request(true), None, || {
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
