use super::*;

fn parse(bytes: &[u8]) -> Vec<TerminalEvent> {
    let mut parser = Parser::default();
    let mut events = VecDeque::new();
    for chunk in bytes.chunks(3) {
        parser.feed(chunk, &mut events);
    }
    parser.finish(&mut events);
    events.into()
}

#[test]
fn 日志阶段单向推进并保留就绪后的普通诊断() {
    let events = parse(
        b"debug1: Connecting to localhost [127.0.0.1] port 22.\n\
        debug1: Connection established.\n\
        debug1: Authenticating to localhost:22 as 'user'\n\
        Authenticated to localhost ([127.0.0.1]:22) using \"password\".\n\
        debug1: Entering interactive session.\n\
        debug2: channel 0: rcvd close\n\
        debug1: Connecting to localhost [127.0.0.1] port 22.\n\
        Connection to localhost closed by remote host.\r\n\
        Connection to localhost closed.",
    );
    let stages: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            TerminalEvent::Stage(stage) => Some(*stage),
            _ => None,
        })
        .collect();
    assert_eq!(
        stages,
        [
            ConnectStage::Connect,
            ConnectStage::Auth,
            ConnectStage::Session,
            ConnectStage::Ready
        ]
    );
    let output: Vec<_> = events
        .into_iter()
        .flat_map(|e| match e {
            TerminalEvent::Output(bytes) => bytes,
            _ => vec![],
        })
        .collect();
    assert!(output.ends_with(
        b"Connection to localhost closed by remote host.\r\nConnection to localhost closed.\r\n"
    ));
    assert!(!String::from_utf8_lossy(&output).contains("debug"));
}

#[test]
fn 代理复用和旧版认证信息可识别且失败不就绪() {
    for (log, expected) in [
        (
            "debug1: Executing proxy command: exec ssh -W '[host]:22' jump\ndebug1: Authenticating to host:22 as 'user'\ndebug1: Authentication succeeded (publickey).\ndebug1: Entering interactive session.\n",
            vec![
                ConnectStage::Connect,
                ConnectStage::Auth,
                ConnectStage::Session,
                ConnectStage::Ready,
            ],
        ),
        (
            "debug1: mux_client_request_session: master session id: 2\n",
            vec![ConnectStage::Session, ConnectStage::Ready],
        ),
        (
            "debug1: auto-mux: Trying existing master at 'socket'\nmux_client_request_session: session request failed: Session open refused by peer\n",
            vec![],
        ),
        (
            "debug1: Connecting to host [127.0.0.1] port 1.\nssh: connect to host host port 1: Connection refused\n",
            vec![ConnectStage::Connect],
        ),
        (
            "debug1: Connection established.\nPermission denied, please try again.\nuser@host: Permission denied (publickey,password).\n",
            vec![ConnectStage::Auth],
        ),
    ] {
        let events = parse(log.as_bytes());
        let stages: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                TerminalEvent::Stage(stage) => Some(*stage),
                _ => None,
            })
            .collect();
        assert_eq!(stages, expected);
        for line in log.lines().filter(|line| !line.starts_with("debug")) {
            assert!(events.contains(&TerminalEvent::Output(format!("{line}\r\n").into_bytes())));
        }
    }
}

#[test]
fn 日志不存在可重试且文件权限受限释放即删除() {
    let dir = crate::test_support::tempdir();
    let mut log = Log::create(dir.path()).unwrap();
    let path = log.path().to_owned();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    std::fs::remove_file(&path).unwrap();
    assert!(log.read(false));
    std::fs::write(&path, b"Permission denied").unwrap();
    assert!(!log.read(false));
    assert!(log.events.is_empty());
    assert!(log.read(true));
    assert_eq!(
        log.events.pop_front(),
        Some(TerminalEvent::Output(b"Permission denied\r\n".to_vec()))
    );
    drop(log);
    assert!(!path.exists());
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, b"").unwrap();
    assert!(Log::create(&blocker).is_none());
}

#[test]
fn 超长行和字节切分不泄漏调试行且保留普通字节() {
    let mut bytes = b"debug3: ".to_vec();
    bytes.extend(vec![b'x'; CHUNK * 2]);
    bytes.extend(b"\n");
    let plain = vec![b'x'; CHUNK * 2];
    bytes.extend(&plain);
    bytes.extend(b"\n");
    let output: Vec<_> = parse(&bytes)
        .into_iter()
        .flat_map(|e| match e {
            TerminalEvent::Output(bytes) => bytes,
            _ => vec![],
        })
        .collect();
    assert_eq!(output, [plain, b"\r\n".to_vec()].concat());
}

#[cfg(unix)]
fn mock_spec(root: &Path, body: &str, directory: &Path) -> TerminalSpec {
    use std::os::unix::fs::PermissionsExt;
    let program = root.join("ssh");
    std::fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    TerminalSpec {
        kind: TerminalKind::Remote {
            ssh: program,
            command: SshCommand {
                args: vec![],
                env: vec![],
                config_text: None,
                hooked: false,
                builtin: None,
                askpass: None,
            },
            config_dir: directory.into(),
        },
        cols: 80,
        rows: 24,
    }
}

#[cfg(unix)]
#[test]
fn 日志回显计入窗口且末尾无换行诊断在删除文件后才报退出() {
    let root = crate::test_support::tempdir();
    let directory = root.path().join("中文 空格日志");
    let payload = root.path().join("payload");
    let mut data = b"debug1: Connecting to host [127.0.0.1] port 22.\ndebug1: Connection established.\ndebug1: Authentication succeeded (password).\ndebug1: Entering interactive session.\n".to_vec();
    data.extend(vec![b'x'; WINDOW * 2]);
    data.extend(b"\nPermission denied");
    std::fs::write(&payload, &data).unwrap();
    let mut spec = mock_spec(
        root.path(),
        "[ \"$1\" = -o ] && [ \"$2\" = LogLevel=DEBUG1 ] && [ \"$3\" = -E ] || exit 1\ncat \"$5\" >> \"$4\"",
        &directory,
    );
    if let TerminalKind::Remote { command, .. } = &mut spec.kind {
        command.args.push(payload.to_str().unwrap().into());
    }
    let manager = TerminalManager::new();
    let (tx, rx) = mpsc::channel();
    let id = manager
        .open(
            spec,
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
    let mut stages = Vec::new();
    let mut output = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
            TerminalEvent::Stage(stage) => stages.push(stage),
            TerminalEvent::Output(bytes) => {
                if let Ok(session) = manager.get(&id) {
                    let flow = lock(&session.flow);
                    assert!(flow.pending + flow.reading <= WINDOW);
                }
                manager.ack(&id, bytes.len()).unwrap();
                output.extend(bytes);
            }
            TerminalEvent::Exited { code, .. } => {
                assert_eq!(code, Some(0));
                assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
                break;
            }
        }
    }
    assert_eq!(
        stages,
        [
            ConnectStage::Resolve,
            ConnectStage::Connect,
            ConnectStage::Auth,
            ConnectStage::Session,
            ConnectStage::Ready
        ]
    );
    assert_eq!(
        output,
        [
            vec![b'x'; WINDOW * 2],
            b"\r\nPermission denied\r\n".to_vec()
        ]
        .concat()
    );
    assert!(rx.try_recv().is_err());
}

#[cfg(unix)]
#[test]
fn 建日志失败仍正常启动且不报告阶段() {
    let root = crate::test_support::tempdir();
    let blocked = root.path().join("blocked");
    std::fs::write(&blocked, b"").unwrap();
    let spec = mock_spec(
        root.path(),
        "[ \"$#\" = 0 ] || exit 1\nprintf connected",
        &blocked,
    );
    let manager = TerminalManager::new();
    let (tx, rx) = mpsc::channel();
    let id = manager
        .open(
            spec,
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
    let mut output = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            TerminalEvent::Output(bytes) => {
                manager.ack(&id, bytes.len()).unwrap();
                output.extend(bytes);
            }
            TerminalEvent::Exited { code, .. } => {
                assert_eq!(code, Some(0));
                break;
            }
            TerminalEvent::Stage(_) => panic!("建日志失败不能报告阶段"),
        }
    }
    assert_eq!(output, b"connected");
}
