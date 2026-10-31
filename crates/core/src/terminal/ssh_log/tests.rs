use super::*;

fn output(events: impl IntoIterator<Item = TerminalEvent>) -> Vec<u8> {
    events
        .into_iter()
        .flat_map(|event| match event {
            TerminalEvent::Output(bytes) => bytes,
            _ => vec![],
        })
        .collect()
}

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
    assert_eq!(
        output,
        b"Connection to localhost closed by remote host.\r\nConnection to localhost closed.\r\n"
    );
}

#[test]
fn 过滤详细状态与统计但保留警告错误和相似正文() {
    let events = parse(
        b"Authenticated with partial success.\r\n\
        Authenticated to host using password.\n\
        debug1: Entering interactive session.\n\
        Authenticated to jump using publickey.\n\
        Transferred: sent 100, received 200 bytes, in 1 seconds\n\
        Bytes per second: sent 100, received 200\n\
        Warning: Authenticated to host is only text\n\
        Permission denied, please try again.\n Transferred: sent is indented text\n\
        message: Bytes per second: 100\n\
        Bytes per second: sent 0, received 0",
    );
    let stages: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            TerminalEvent::Stage(stage) => Some(*stage),
            _ => None,
        })
        .collect();
    assert_eq!(stages, [ConnectStage::Session, ConnectStage::Ready]);
    assert_eq!(
        output(events),
        b"Warning: Authenticated to host is only text\r\n\
        Permission denied, please try again.\r\n Transferred: sent is indented text\r\n\
        message: Bytes per second: 100\r\n"
    );
    assert!(parse(b"Authenticated with partial success.\n").is_empty());
}

#[test]
fn 详细日志跨块和无换行尾部均不泄漏() {
    for prefix in [
        "Authenticated to ",
        "Authenticated with partial success",
        "Transferred: sent ",
        "Bytes per second: ",
    ] {
        for ending in [b"\r\n".as_slice(), b""] {
            let mut bytes = prefix.as_bytes().to_vec();
            bytes.extend(vec![b'x'; CHUNK * 2]);
            bytes.extend(ending);
            assert!(output(parse(&bytes)).is_empty());
        }
    }
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

#[cfg(windows)]
#[test]
fn windows日志保留目录权限继承() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetFileSecurityW, GetSecurityDescriptorControl,
        SE_DACL_PROTECTED,
    };
    let dir = crate::test_support::tempdir();
    let log = Log::create(dir.path()).unwrap();
    let path: Vec<_> = log
        .path()
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut size = 0;
    unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut size,
        )
    };
    assert!(size > 0);
    let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
    let descriptor = buffer.as_mut_ptr().cast();
    assert_ne!(
        unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                descriptor,
                size,
                &mut size,
            )
        },
        0
    );
    let (mut control, mut revision) = (0, 0);
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
        0
    );
    assert_eq!(control & SE_DACL_PROTECTED, 0, "日志应继承目录 ACL");
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

#[test]
fn 只有就绪后才放宽日志轮询间隔且不倒退() {
    let root = crate::test_support::tempdir();
    let mut log = Log::create(root.path()).unwrap();
    assert_eq!(log.interval(), Duration::from_millis(50));
    for bytes in [
        b"debug1: Connecting to host [127.0.0.1] port 22.\n".as_slice(),
        b"debug1: Connection established.\n",
        b"Authenticated to host using password.\n",
    ] {
        log.parser.feed(bytes, &mut log.events);
        assert_eq!(log.interval(), Duration::from_millis(50));
    }
    log.parser
        .feed(b"debug1: Entering interactive session.\n", &mut log.events);
    assert_eq!(log.interval(), Duration::from_millis(250));
    log.parser
        .feed(b"debug1: Connection established.\n", &mut log.events);
    assert_eq!(log.interval(), Duration::from_millis(250));
}

#[cfg(unix)]
fn finished_session(pending: usize) -> Arc<Session> {
    let pair = portable_pty::native_pty_system()
        .openpty(size(80, 24).unwrap())
        .unwrap();
    let mut process = child::spawn(
        TerminalKind::Local {
            program: "/bin/true".into(),
            args: vec![],
            cwd: None,
            env: vec![],
        },
        &pair,
    )
    .unwrap();
    process.finish();
    Arc::new(Session {
        hooked: false,
        flow: Mutex::new(Flow {
            pending,
            ..Default::default()
        }),
        wake: Condvar::new(),
        master: Mutex::new((None, 80, 24)),
        input: Mutex::new(input::Input::new(false)),
        output: Mutex::new(super::super::output::Output::new(80, 24)),
        child: Mutex::new(builtin::Process::Local(process)),
    })
}

#[cfg(unix)]
fn dispatch_log(log: Log, chunks: Vec<Vec<u8>>) -> Vec<TerminalEvent> {
    let session = finished_session(chunks.iter().map(Vec::len).sum());
    let (tx, rx) = mpsc::channel();
    for chunk in chunks {
        tx.send(chunk).unwrap();
    }
    drop(tx);
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let active = session.clone();
    dispatch(
        &session,
        rx,
        Arc::new(move |event| {
            if let TerminalEvent::Output(bytes) = &event {
                let mut flow = lock(&active.flow);
                assert!(flow.pending + flow.reading <= WINDOW);
                assert!(bytes.len() <= CHUNK);
                flow.pending -= bytes.len();
            }
            lock(&observed).push(event);
        }),
        Some(log),
    );
    assert_eq!(lock(&session.flow).pending, 0);
    let events = std::mem::take(&mut *lock(&events));
    assert!(matches!(events.last(), Some(TerminalEvent::Exited { .. })));
    events
}

#[cfg(unix)]
#[test]
fn 每轮日志诊断先于终端提示且合并溢出的块也保持顺序() {
    let root = crate::test_support::tempdir();
    for chunks in [
        vec![b"user@host's password: ".to_vec()],
        vec![vec![b'a'; CHUNK - 1], b"user@host's password: ".to_vec()],
    ] {
        let log = Log::create(root.path()).unwrap();
        let diagnostic = b"Permission denied, please try again.\n";
        std::fs::write(log.path(), diagnostic).unwrap();
        let expected = [
            b"Permission denied, please try again.\r\n".to_vec(),
            chunks.concat(),
        ]
        .concat();
        assert_eq!(output(dispatch_log(log, chunks)), expected);
    }
}

#[cfg(unix)]
#[test]
fn 进程退出后连续排空大量日志不逐块等待() {
    let root = crate::test_support::tempdir();
    let log = Log::create(root.path()).unwrap();
    let path = log.path().to_owned();
    let mut file = File::options().append(true).open(&path).unwrap();
    file.write_all(b"debug1: Entering interactive session.\n")
        .unwrap();
    let mut line = b"debug1: ".to_vec();
    line.resize(CHUNK - 1, b'x');
    line.push(b'\n');
    for _ in 0..256 {
        file.write_all(&line).unwrap();
    }
    file.write_all(b"Connection closed").unwrap();
    drop(file);
    let start = Instant::now();
    let events = dispatch_log(log, vec![]);
    // 逐块等待即使每次只有 50 毫秒也要超过 12 秒，连续读取应很快排空。
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(output(events), b"Connection closed\r\n");
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn 日志背压让出线程且确认后才能继续排空() {
    let root = crate::test_support::tempdir();
    let mut log = Log::create(root.path()).unwrap();
    std::fs::write(log.path(), b"Permission denied\n").unwrap();
    let session = finished_session(WINDOW);
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let callback: Callback = Arc::new(move |event| lock(&observed).push(event));
    assert_eq!(log.pump(&session, &callback, true), Pump::Pending);
    assert!(lock(&events).is_empty());
    assert_eq!(log.pump(&session, &callback, true), Pump::Pending);
    lock(&session.flow).pending = 0;
    assert_eq!(log.pump(&session, &callback, true), Pump::Progress);
    assert_eq!(log.pump(&session, &callback, true), Pump::Drained);
    assert_eq!(
        *lock(&events),
        [TerminalEvent::Output(b"Permission denied\r\n".to_vec())]
    );
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
