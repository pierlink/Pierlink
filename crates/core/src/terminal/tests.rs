use super::*;

#[cfg(unix)]
#[test]
fn 本机终端多订阅及屏幕增量查询() {
    let manager = TerminalManager::new();
    let (id, rx) = start(&manager, spec("stty raw -echo; printf ready; cat"));
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        TerminalEvent::Output(b"ready".to_vec())
    );
    manager.ack(&id, 5).unwrap();
    let first = manager.subscribe(&id, 64).unwrap();
    let slow = manager.subscribe(&id, 2).unwrap();
    let position = manager.output_since(&id, 0).unwrap().next;
    manager.write(&id, b"\r\none\r\ntwo").unwrap();
    let mut bytes = Vec::new();
    while bytes.len() < 10 {
        let TerminalEvent::Output(data) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("提前退出")
        };
        manager.ack(&id, data.len()).unwrap();
        bytes.extend(data);
    }
    assert_eq!(manager.screen_text(&id).unwrap(), "ready\none\ntwo");
    assert_eq!(manager.recent_lines(&id, 2).unwrap(), ["one", "two"]);
    assert_eq!(manager.output_since(&id, position).unwrap().data, bytes);
    assert!(!first.recv_timeout(Duration::ZERO).unwrap().lost);
    assert!(slow.recv_timeout(Duration::ZERO).unwrap().lost);
    manager.set_user_control(&id, true).unwrap();
    assert_eq!(
        manager
            .write_from(&id, b"auto", InputSource::Automation)
            .unwrap_err()
            .code,
        "terminal.automation_rejected"
    );
    manager.close(&id).unwrap();
    assert!(until_exit(&rx).2);
    // 关闭时会先给 shell 发结束输入的按键，cat 会把它原样输出；跳过这些输出，等退出事件
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let closed_by_user = loop {
        match first
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .unwrap()
            .event
        {
            TerminalEvent::Exited { closed_by_user, .. } => break closed_by_user,
            TerminalEvent::Stage(_) => {}
            TerminalEvent::Output(_) => continue,
        }
    };
    assert!(closed_by_user);
    for result in [
        manager.screen_text("无会话").map(|_| ()),
        manager.recent_lines("无会话", 1).map(|_| ()),
        manager.output_since("无会话", 0).map(|_| ()),
        manager.set_user_control("无会话", true),
    ] {
        assert_eq!(result.unwrap_err().code, "terminal.not_found");
    }
    assert!(manager.subscribe("无会话", 1).is_err());
}

#[cfg(unix)]
#[test]
#[ignore = "需要本机安装 vim 和可用的伪终端"]
fn 本机vim备用屏幕查询与尺寸变化() {
    let manager = TerminalManager::new();
    let (id, rx) = start(
        &manager,
        spec("vim -Nu NONE -i NONE -n -N -X -c 'set laststatus=2'; printf returned; read line"),
    );
    let wait_screen = |expected: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if manager.screen_text(&id).unwrap().contains(expected) {
                break;
            }
            assert!(Instant::now() < deadline, "屏幕缺少：{expected}");
            if let Ok(event) = rx.recv_timeout(Duration::from_millis(50)) {
                let TerminalEvent::Output(bytes) = event else {
                    panic!("终端意外结束")
                };
                manager.ack(&id, bytes.len()).unwrap();
            }
        }
    };
    wait_screen("[No Name]");
    manager.write(&id, b"iscreen-one\nsecond-line\x1b").unwrap();
    wait_screen("screen-one\nsecond-line");
    manager.resize(&id, 110, 35).unwrap();
    manager.write(&id, b"ggIresized-\x1b").unwrap();
    wait_screen("resized-screen-one");
    manager.write(&id, b":q!\r").unwrap();
    wait_screen("returned");
    assert!(!manager.screen_text(&id).unwrap().contains("screen-one"));
    manager.close(&id).unwrap();
    assert!(until_exit(&rx).2);
}
#[cfg(unix)]
fn spec(script: &str) -> TerminalSpec {
    TerminalSpec {
        kind: TerminalKind::Local {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: None,
            env: vec![],
        },
        cols: 80,
        rows: 24,
    }
}
#[cfg(unix)]
fn start(manager: &TerminalManager, spec: TerminalSpec) -> (String, mpsc::Receiver<TerminalEvent>) {
    let (tx, rx) = mpsc::channel();
    let id = manager
        .open(
            spec,
            Arc::new(move |e| {
                let _ = tx.send(e);
            }),
        )
        .unwrap();
    (id, rx)
}
#[cfg(unix)]
fn until_exit(rx: &mpsc::Receiver<TerminalEvent>) -> (Vec<u8>, Option<i32>, bool) {
    let mut bytes = Vec::new();
    loop {
        // 全量并行跑测试时机器较忙，给足 10 秒
        match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
            TerminalEvent::Stage(_) => {}
            TerminalEvent::Output(chunk) => bytes.extend(chunk),
            TerminalEvent::Exited {
                code,
                closed_by_user,
                hooked,
            } => {
                assert!(!hooked);
                return (bytes, code, closed_by_user);
            }
        }
    }
}
#[test]
fn 事件序列化和尺寸校验() {
    assert_eq!(
        serde_json::to_value(TerminalEvent::Stage(ConnectStage::Ready)).unwrap(),
        serde_json::json!({"Stage": "ready"})
    );
    assert_eq!(
        serde_json::to_value(TerminalEvent::Output(vec![0, 255])).unwrap(),
        serde_json::json!({"Output":[0,255]})
    );
    assert_eq!(
        serde_json::to_value(TerminalEvent::Exited {
            code: None,
            closed_by_user: true,
            hooked: false,
        })
        .unwrap(),
        serde_json::json!({"Exited":{"code":null,"closed_by_user":true,"hooked":false}})
    );
    assert!(size(0, 1).is_err());
    assert!(size(1, 0).is_err());
}
#[test]
fn 本机shell空闲及运行程序时的状态() {
    let manager = TerminalManager::new();
    assert_eq!(
        manager.busy("不存在").unwrap_err().code,
        "terminal.not_found"
    );
    #[cfg(unix)]
    let (program, args, command) = (
        PathBuf::from("/bin/sh"),
        vec!["-i".to_owned()],
        b"sleep 30\n".as_slice(),
    );
    #[cfg(windows)]
    let (program, args, command) = (
        PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/cmd.exe"),
        vec!["/D".to_owned()],
        b"ping -n 30 127.0.0.1\r".as_slice(),
    );
    let (tx, rx) = mpsc::channel();
    let id = manager
        .open(
            TerminalSpec {
                kind: TerminalKind::Local {
                    program,
                    args,
                    cwd: None,
                    env: vec![],
                },
                cols: 80,
                rows: 24,
            },
            Arc::new(move |e| {
                let _ = tx.send(e);
            }),
        )
        .unwrap();
    let start = Instant::now();
    loop {
        if let Ok(TerminalEvent::Output(bytes)) = rx.recv_timeout(Duration::from_millis(100)) {
            manager.ack(&id, bytes.len()).unwrap();
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(15));
    }
    assert_eq!(manager.busy(&id).unwrap(), Some(false));
    manager.write(&id, command).unwrap();
    let start = Instant::now();
    while manager.busy(&id).unwrap() != Some(true) {
        assert!(start.elapsed() < Duration::from_secs(10));
        thread::sleep(Duration::from_millis(20));
    }
    manager.close(&id).unwrap();
}
#[cfg(unix)]
#[test]
fn 远程会话本机进程状态返回未知() {
    let root = crate::test_support::tempdir();
    let manager = TerminalManager::new();
    let (id, _) = start(
        &manager,
        TerminalSpec {
            kind: TerminalKind::Remote {
                ssh: fake_ssh(),
                command: SshCommand {
                    args: vec!["-i".into()],
                    env: vec![],
                    config_text: None,
                    hooked: false,
                    builtin: None,
                    askpass: None,
                },
                config_dir: root.path().into(),
            },
            cols: 80,
            rows: 24,
        },
    );
    assert_eq!(manager.busy(&id).unwrap(), None);
    manager.close(&id).unwrap();
}
#[cfg(unix)]
#[test]
fn r2_r4_r7_r8_原始字节及退出事件() {
    let manager = TerminalManager::new();
    let (_, rx) = start(
        &manager,
        spec(
            r"printf '\033[31m中文😀\033[0m\033[?2004h\033]52;c;abc\007\033]8;;https://example.com\007'; exit 7",
        ),
    );
    let (bytes, code, closed) = until_exit(&rx);
    assert_eq!(
        bytes,
        "\x1b[31m中文😀\x1b[0m\x1b[?2004h\x1b]52;c;abc\x07\x1b]8;;https://example.com\x07"
            .as_bytes()
    );
    assert_eq!(code, Some(7));
    assert!(!closed);
}
#[cfg(unix)]
#[test]
fn r5_输入回显和立即调整尺寸() {
    let manager = TerminalManager::new();
    let (id, rx) = start(
        &manager,
        spec("read line; stty size; printf '%s' \"$line\""),
    );
    manager.resize(&id, 120, 40).unwrap();
    manager.resize(&id, 120, 40).unwrap();
    assert!(manager.resize(&id, 0, 20).is_err());
    manager.write(&id, "你好\n".as_bytes()).unwrap();
    let (bytes, _, _) = until_exit(&rx);
    let output = String::from_utf8(bytes).unwrap();
    assert!(output.contains("40 120"));
    assert!(output.contains("你好"));
}
#[cfg(unix)]
#[test]
fn r3_背压暂停恢复且总量有界() {
    let manager = TerminalManager::new();
    let (id, rx) = start(&manager, spec("yes 流量"));
    let mut total = 0;
    while total < WINDOW {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            TerminalEvent::Stage(_) => {}
            TerminalEvent::Output(b) => {
                assert!(b.len() <= CHUNK);
                total += b.len();
            }
            _ => panic!("提前退出"),
        }
    }
    assert_eq!(total, WINDOW);
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    assert_eq!(lock(&manager.get(&id).unwrap().flow).pending, WINDOW);
    assert!(manager.ack(&id, WINDOW + 1).is_err());
    manager.ack(&id, CHUNK).unwrap();
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        TerminalEvent::Output(_)
    ));
    manager.close(&id).unwrap();
    manager.close(&id).unwrap();
    assert!(until_exit(&rx).2);
}
#[cfg(unix)]
#[test]
fn r6_信号退出及失败不泄漏秘密() {
    let manager = TerminalManager::new();
    let (_, rx) = start(&manager, spec("kill -KILL $$"));
    assert_eq!(until_exit(&rx).1, None);
    let mut missing = spec("");
    if let TerminalKind::Local { program, .. } = &mut missing.kind {
        *program = "/不存在/password=秘密".into();
    }
    let err = manager.open(missing, Arc::new(|_| {})).unwrap_err();
    assert_eq!(err.code, "terminal.not_found");
    assert!(!format!("{err:?}").contains("秘密"));
    assert!(manager.write("没有", "秘密".as_bytes()).is_err());
    assert!(manager.resize("没有", 1, 1).is_err());
    assert!(manager.ack("没有", 0).is_ok());
    manager.close("没有").unwrap();
}
#[cfg(unix)]
#[test]
fn 远程配置唯一并在所有结束路径删除() {
    let dir = crate::test_support::tempdir();
    let manager = TerminalManager::new();
    let remote = |program: &str| TerminalSpec {
        kind: TerminalKind::Remote {
            ssh: if program == "/bin/sh" {
                fake_ssh()
            } else {
                program.into()
            },
            command: SshCommand {
                args: vec![
                    "-c".into(),
                    "test -f \"$1\" && printf 配置存在; read line".into(),
                    "假ssh".into(),
                    "-F".into(),
                    "占位".into(),
                ],
                env: vec![],
                config_text: Some("Host *\n".into()),
                hooked: false,
                builtin: None,
                askpass: None,
            },
            config_dir: dir.path().into(),
        },
        cols: 80,
        rows: 24,
    };
    // /bin/sh 的位置参数中 $1 为 -F、$2 才是配置路径。
    let mut input = remote("/bin/sh");
    if let TerminalKind::Remote { command, .. } = &mut input.kind {
        command.args[1] = "test -f \"$2\" && printf 配置存在; read line".into();
    }
    let (id, rx) = start(&manager, input);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        TerminalEvent::Stage(ConnectStage::Resolve)
    );
    let event = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(event, TerminalEvent::Output("配置存在".as_bytes().to_vec()));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    manager.close(&id).unwrap();
    until_exit(&rx);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    assert!(manager.open(remote("/不存在"), Arc::new(|_| {})).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
#[cfg(target_os = "linux")]
#[test]
fn l1_r9_丢弃和关闭杀掉子shell后台进程() {
    let dir = crate::test_support::tempdir();
    let pidfile = dir.path().join("pid");
    let manager = TerminalManager::new();
    let mut input = spec("sh -i -c 'sleep 60 & echo $! > \"$PIDFILE\"; wait' & wait");
    if let TerminalKind::Local { env, .. } = &mut input.kind {
        env.push(("PIDFILE".into(), pidfile.to_str().unwrap().into()));
    }
    let (_, rx) = start(&manager, input);
    let start = Instant::now();
    while !pidfile.exists() || std::fs::read_to_string(&pidfile).unwrap().trim().is_empty() {
        assert!(start.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(5));
    }
    let pid = std::fs::read_to_string(pidfile)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    drop(manager);
    assert!(until_exit(&rx).2);
    // 信号是异步送达的，机器忙时进程可能稍晚才结束：最多等 2 秒
    let gone = || {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        stat.is_err() || stat.unwrap().split_once(") ").unwrap().1.starts_with('Z')
    };
    let start = Instant::now();
    while !gone() {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "后台进程 {pid} 没有被结束"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[test]
fn r1_ctrl_c原样交给终端() {
    let manager = TerminalManager::new();
    let (id, rx) = start(
        &manager,
        spec("trap 'printf interrupted; exit 0' INT; printf ready; while :; do sleep 60; done"),
    );
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        TerminalEvent::Output(_)
    ));
    manager.write(&id, &[3]).unwrap();
    let (bytes, code, closed) = until_exit(&rx);
    assert!(String::from_utf8_lossy(&bytes).contains("interrupted"));
    assert_eq!(code, Some(0));
    assert!(!closed);
}
#[cfg(unix)]
#[test]
fn r3_大文件确认后完整排空才退出() {
    let manager = TerminalManager::new();
    let (id, rx) = start(
        &manager,
        spec("dd if=/dev/zero bs=65536 count=32 2>/dev/null"),
    );
    let mut total = 0;
    loop {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            TerminalEvent::Stage(_) => {}
            TerminalEvent::Output(bytes) => {
                assert!(bytes.iter().all(|&b| b == 0));
                total += bytes.len();
                manager.ack(&id, bytes.len()).unwrap();
            }
            TerminalEvent::Exited {
                code,
                closed_by_user,
                hooked,
            } => {
                assert!(!hooked);
                assert_eq!(code, Some(0));
                assert!(!closed_by_user);
                break;
            }
        }
    }
    assert_eq!(total, WINDOW * 2);
}
#[cfg(unix)]
#[test]
fn 环境工作目录及多会话关闭() {
    let dir = crate::test_support::tempdir();
    let manager = TerminalManager::new();
    let mut input = spec("printf '%s:%s:' \"$TERM\" \"$CUSTOM\"; pwd; read line");
    if let TerminalKind::Local { env, cwd, .. } = &mut input.kind {
        *cwd = Some(dir.path().into());
        env.extend([
            ("TERM".into(), "自定义终端".into()),
            ("CUSTOM".into(), "中文参数".into()),
        ]);
    }
    let (_, first) = start(&manager, input);
    let (_, second) = start(&manager, spec("read line"));
    let TerminalEvent::Output(bytes) = first.recv_timeout(Duration::from_secs(2)).unwrap() else {
        panic!("缺少输出")
    };
    let output = String::from_utf8(bytes).unwrap();
    assert!(output.contains("xterm-256color:中文参数:"));
    assert!(output.contains(dir.path().to_str().unwrap()));
    manager.close_all();
    manager.close_all();
    assert!(until_exit(&first).2);
    assert!(until_exit(&second).2);
}
#[cfg(unix)]
#[test]
fn w2_延迟启动不阻塞打开及回调可重入() {
    let manager = Arc::new(TerminalManager::new());
    let weak = Arc::downgrade(&manager);
    let (tx, rx) = mpsc::channel();
    let before = Instant::now();
    manager
        .open(
            spec("sleep 0.2; printf ready; read line"),
            Arc::new(move |event| {
                if matches!(event, TerminalEvent::Output(_)) {
                    weak.upgrade().unwrap().close_all();
                }
                tx.send(event).unwrap();
            }),
        )
        .unwrap();
    assert!(before.elapsed() < Duration::from_secs(1));
    assert!(until_exit(&rx).2);
}
#[cfg(unix)]
#[test]
fn 远程配置并发唯一及自然结束清理() {
    let dir = crate::test_support::tempdir();
    let manager = TerminalManager::new();
    let input = || TerminalSpec {
        kind: TerminalKind::Remote {
            ssh: fake_ssh(),
            command: SshCommand {
                args: vec![
                    "-c".into(),
                    "read line".into(),
                    "假ssh".into(),
                    "-F".into(),
                    "占位".into(),
                ],
                env: vec![],
                config_text: Some("Host *\n".into()),
                hooked: false,
                builtin: None,
                askpass: None,
            },
            config_dir: dir.path().into(),
        },
        cols: 80,
        rows: 24,
    };
    let (a, first) = start(&manager, input());
    let (b, second) = start(&manager, input());
    assert_ne!(a, b);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 4);
    manager.write(&a, b"\n").unwrap();
    until_exit(&first);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    manager.write(&b, b"\n").unwrap();
    until_exit(&second);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
#[cfg(windows)]
#[test]
fn windows_conpty输出与关闭() {
    let manager = TerminalManager::new();
    let (tx, rx) = mpsc::channel();
    // 故意用正斜杠拼路径：实现必须把命令行里的程序路径规范成反斜杠，否则 cmd.exe 会把 “/cmd.exe” 当成 /c 开关
    let program = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/cmd.exe");
    let id = manager
        .open(
            TerminalSpec {
                kind: TerminalKind::Local {
                    program,
                    args: vec!["/Q".into()],
                    cwd: None,
                    env: vec![],
                },
                cols: 80,
                rows: 24,
            },
            Arc::new(move |e| {
                let _ = tx.send(e);
            }),
        )
        .unwrap();
    manager.resize(&id, 100, 30).unwrap();
    // 真实终端里回车只发 \r（xterm.js 也是如此），不能发 \r\n
    manager.write(&id, b"echo pl-conpty\rexit 0\r").unwrap();
    let mut output = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
            TerminalEvent::Stage(_) => {}
            TerminalEvent::Output(bytes) => {
                let _ = manager.ack(&id, bytes.len());
                output.extend(bytes);
            }
            TerminalEvent::Exited {
                code,
                closed_by_user,
                hooked,
            } => {
                assert!(!hooked);
                // 失败时把终端输出带上，便于在 CI 日志里看清 cmd 实际收到了什么
                let text = String::from_utf8_lossy(&output);
                assert_eq!(code, Some(0), "终端输出: {text:?}");
                assert!(!closed_by_user);
                break;
            }
        }
    }
    assert!(String::from_utf8_lossy(&output).contains("pl-conpty"));
}

#[cfg(unix)]
#[test]
fn r2_括号粘贴与二进制输入不转换() {
    let manager = TerminalManager::new();
    let bytes = b"\x1b[200~a\nb\x1b[201~\0\xff";
    let (id, rx) = start(
        &manager,
        spec(&format!(
            "stty raw -echo; printf ready; dd bs=1 count={} 2>/dev/null",
            bytes.len()
        )),
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        TerminalEvent::Output(b"ready".to_vec())
    );
    manager.write(&id, bytes).unwrap();
    let (output, code, _) = until_exit(&rx);
    assert_eq!(output, bytes);
    assert_eq!(code, Some(0));
}

#[cfg(unix)]
#[test]
fn 输入有界且调用不阻塞() {
    let manager = TerminalManager::new();
    let (id, rx) = start(&manager, spec("stty raw -echo; printf ready; sleep 60"));
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let before = Instant::now();
    manager.write(&id, &vec![b'x'; WINDOW]).unwrap();
    assert!(before.elapsed() < Duration::from_millis(500));
    assert_eq!(
        manager.write(&id, b"x").unwrap_err().code,
        "terminal.input_overflow"
    );
    manager.close(&id).unwrap();
    assert!(until_exit(&rx).2);
}
#[cfg(unix)]
#[test]
fn 关闭阻塞不持有会话表锁() {
    let manager = Arc::new(TerminalManager::new());
    let (a, first) = start(&manager, spec("read line"));
    let (b, second) = start(&manager, spec("read line"));
    let session = manager.get(&a).unwrap();
    let guard = lock(&session.child);
    let closing = manager.clone();
    let task = thread::spawn(move || closing.close(&a).unwrap());
    let deadline = Instant::now();
    while !lock(&session.flow).closed {
        assert!(deadline.elapsed() < Duration::from_secs(2));
        thread::yield_now();
    }
    let other = manager.clone();
    let (tx, rx) = mpsc::channel();
    let writing = thread::spawn(move || {
        other.ack(&b, 0).unwrap();
        other.write(&b, b"\n").unwrap();
        tx.send(()).unwrap();
    });
    let result = rx.recv_timeout(Duration::from_millis(500));
    drop(guard);
    task.join().unwrap();
    writing.join().unwrap();
    assert!(result.is_ok());
    assert!(until_exit(&first).2);
    assert!(!until_exit(&second).2);
}
#[cfg(unix)]
#[test]
fn 丢失ack超时关闭() {
    let manager = TerminalManager::new();
    let (id, rx) = start(&manager, spec("yes 输出"));
    let mut total = 0;
    while total < WINDOW {
        if let TerminalEvent::Output(bytes) = rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            total += bytes.len();
        }
    }
    let session = manager.get(&id).unwrap();
    manager.ack(&id, 0).unwrap();
    assert!(lock(&session.flow).last_ack.elapsed() < Duration::from_secs(1));
    assert!(rx.recv_timeout(Duration::from_millis(250)).is_err());
    let now = lock(&session.flow).last_ack + Duration::from_secs(301);
    session.stop_if_stale(now);
    assert!(until_exit(&rx).2);
}
#[cfg(unix)]
#[test]
fn 工作目录缺失单独报错() {
    let dir = crate::test_support::tempdir();
    let mut input = spec("");
    if let TerminalKind::Local { cwd, .. } = &mut input.kind {
        *cwd = Some(dir.path().join("不存在"));
    }
    assert_eq!(
        TerminalManager::new()
            .open(input, Arc::new(|_| {}))
            .unwrap_err()
            .code,
        "terminal.cwd_not_found"
    );
}
#[test]
fn 清理残留配置不递归且缺目录成功() {
    let dir = crate::test_support::tempdir();
    assert_eq!(
        cleanup_stale_configs(&dir.path().join("不存在")).unwrap(),
        0
    );
    for name in [
        "terminal-a.conf",
        "terminal-b.conf",
        "terminal-old.log",
        "保留.conf",
        "terminal-a.txt",
    ] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }
    std::fs::create_dir(dir.path().join("terminal-dir.conf")).unwrap();
    assert_eq!(cleanup_stale_configs(dir.path()).unwrap(), 3);
    assert_eq!(cleanup_stale_configs(dir.path()).unwrap(), 0);
    assert!(cleanup_stale_configs(&dir.path().join("保留.conf")).is_err());
}
#[cfg(unix)]
#[test]
fn 远程创建目录并强制term() {
    let dir = crate::test_support::tempdir();
    let config_dir = dir.path().join("新目录/配置");
    let manager = TerminalManager::new();
    let (_, rx) = start(
        &manager,
        TerminalSpec {
            kind: TerminalKind::Remote {
                ssh: fake_ssh(),
                config_dir: config_dir.clone(),
                command: SshCommand {
                    args: vec![
                        "-c".into(),
                        "printf '%s' \"$TERM\"".into(),
                        "假ssh".into(),
                        "-F".into(),
                        "占位".into(),
                    ],
                    env: vec![("TERM".into(), "错误".into())],
                    config_text: Some("Host *\n".into()),
                    hooked: false,
                    builtin: None,
                    askpass: None,
                },
            },
            cols: 80,
            rows: 24,
        },
    );
    assert_eq!(until_exit(&rx).0, b"xterm-256color");
    assert!(config_dir.is_dir());
    assert_eq!(std::fs::read_dir(config_dir).unwrap().count(), 0);
}
#[cfg(target_os = "linux")]
#[test]
fn 自然退出保留后代并在两秒后结束排空() {
    let dir = crate::test_support::tempdir();
    let pidfile = dir.path().join("后台pid");
    struct Background(PathBuf);
    impl Drop for Background {
        fn drop(&mut self) {
            if let Ok(text) = std::fs::read_to_string(&self.0)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
    let _background = Background(pidfile.clone());
    let manager = TerminalManager::new();
    let mut input = spec("trap '' HUP; sleep 30 & echo $! > \"$PIDFILE\"; exit 0");
    if let TerminalKind::Local { env, .. } = &mut input.kind {
        env.push(("PIDFILE".into(), pidfile.to_str().unwrap().into()));
    }
    let before = Instant::now();
    let (_, rx) = start(&manager, input);
    let result = until_exit(&rx);
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(alive);
    assert_eq!(result.1, Some(0));
    assert!(!result.2);
    assert!(before.elapsed() >= Duration::from_secs(2));
    assert!(before.elapsed() < Duration::from_secs(4));
}

#[cfg(unix)]
#[test]
fn 空闲等待与输出即时唤醒及控制终端() {
    let manager = TerminalManager::new();
    let (id, rx) = start(
        &manager,
        spec("test -t 0 && test -t 1 && printf ready; read line; printf '%s' \"$line\""),
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        TerminalEvent::Output(b"ready".to_vec())
    );
    assert!(rx.recv_timeout(Duration::from_millis(250)).is_err());
    let before = Instant::now();
    manager.write(&id, b"awake\n").unwrap();
    let result = until_exit(&rx);
    assert!(before.elapsed() < Duration::from_millis(500));
    assert!(String::from_utf8_lossy(&result.0).contains("awake"));
    assert_eq!(result.1, Some(0));
}

#[cfg(unix)]
#[test]
fn 终端返回实际登记_退出后可检查代数并保护新密码() {
    use crate::{
        security::vault::Zeroizing,
        ssh::askpass_pipe::{PromptKind, test_answer, test_registration},
    };
    let root = crate::test_support::tempdir();
    let template = test_registration("user@host");
    template
        .set_password(Zeroizing::new("旧密码".into()))
        .unwrap();
    let manager = TerminalManager::new();
    let (tx, rx) = mpsc::channel();
    let (_id, actual) = manager
        .open_with_registration(
            TerminalSpec {
                kind: TerminalKind::Remote {
                    ssh: fake_ssh(),
                    command: SshCommand {
                        args: vec!["-c".into(), "exit 255".into()],
                        env: vec![],
                        config_text: None,
                        hooked: false,
                        builtin: None,
                        askpass: Some(template.clone()),
                    },
                    config_dir: root.path().into(),
                },
                cols: 80,
                rows: 24,
            },
            Arc::new(move |event| {
                tx.send(event).unwrap();
            }),
        )
        .unwrap();
    let actual = actual.unwrap();
    assert_ne!(actual.nonce(), template.nonce());
    test_answer(&actual, "user@host's password: ", PromptKind::Password).unwrap();
    loop {
        if matches!(
            rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            TerminalEvent::Exited { .. }
        ) {
            break;
        }
    }
    assert!(actual.status().used_generation.is_some());
    template
        .set_password(Zeroizing::new("新密码".into()))
        .unwrap();
    assert_eq!(
        actual.finish_error(AppError::new("ssh.auth_failed")).code,
        "ssh.auth_failed"
    );
    assert!(template.has_password());
}

#[cfg(unix)]
#[test]
fn 用户每次非空输入改变代数且审批过期时不入队() {
    let manager = TerminalManager::new();
    let (id, rx) = start(&manager, spec("stty raw -echo; printf ready; cat"));
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(manager.control_generation(&id).unwrap(), 0);
    manager.write_from(&id, b"", InputSource::User).unwrap();
    assert_eq!(manager.control_generation(&id).unwrap(), 0);
    manager
        .write_from(&id, b"a", InputSource::Automation)
        .unwrap();
    assert_eq!(manager.control_generation(&id).unwrap(), 0);
    manager.write(&id, b"u").unwrap();
    assert_eq!(manager.control_generation(&id).unwrap(), 1);
    manager
        .write_from(&id, b"\x1b[D", InputSource::User)
        .unwrap();
    assert_eq!(manager.control_generation(&id).unwrap(), 2);
    assert_eq!(
        manager
            .write_automation_at_generation(&id, b"\r", 1)
            .unwrap_err()
            .code,
        "terminal.input_changed"
    );
    manager
        .write_automation_at_generation(&id, b"queued", 2)
        .unwrap();
    manager.set_user_control(&id, true).unwrap();
    assert_eq!(manager.control_generation(&id).unwrap(), 3);
    assert_eq!(
        manager
            .write_automation_at_generation(&id, b"queued", 3)
            .unwrap_err()
            .code,
        "terminal.automation_rejected"
    );
    manager.set_user_control(&id, false).unwrap();
    assert_eq!(manager.control_generation(&id).unwrap(), 3);
    assert_eq!(
        manager.control_generation("不存在").unwrap_err().code,
        "terminal.not_found"
    );
    manager.close(&id).unwrap();
}

#[cfg(unix)]
#[test]
fn 管理器克隆释放保留会话且最后一个释放关闭会话() {
    let manager = TerminalManager::new();
    let (id, rx) = start(&manager, spec("printf ready; read line"));
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let other = manager.clone();
    drop(manager);
    assert!(other.screen_text(&id).is_ok());
    other.write(&id, b"a").unwrap();
    drop(other);
    assert!(until_exit(&rx).2);
}

#[cfg(unix)]
#[test]
fn 系统ssh退出事件保留命令生成的实际钩子状态() {
    use crate::ssh::{
        args::{CommonOpts, Purpose, build_args},
        target::SshTarget,
    };
    let root = crate::test_support::tempdir();
    for has_remote_command in [true, false] {
        let command = build_args(
            &SshTarget::Alias {
                alias: "example".into(),
            },
            &Purpose::Terminal {
                login_shell: None,
                cwd_marker: None,
                integration: true,
            },
            &CommonOpts {
                has_remote_command: Some(has_remote_command),
                ..Default::default()
            },
        )
        .unwrap();
        let manager = TerminalManager::new();
        let (_, rx) = start(
            &manager,
            TerminalSpec {
                kind: TerminalKind::Remote {
                    ssh: "/bin/true".into(),
                    command,
                    config_dir: root.path().into(),
                },
                cols: 80,
                rows: 24,
            },
        );
        loop {
            if let TerminalEvent::Exited { hooked, .. } =
                rx.recv_timeout(Duration::from_secs(5)).unwrap()
            {
                assert_eq!(hooked, !has_remote_command);
                break;
            }
        }
    }
}

#[cfg(unix)]
fn fake_ssh() -> PathBuf {
    use std::sync::OnceLock;
    static SCRIPT: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    SCRIPT
        .get_or_init(|| {
            use std::os::unix::fs::PermissionsExt;
            let dir = crate::test_support::tempdir();
            let path = dir.path().join("ssh");
            std::fs::write(
                &path,
                "#!/bin/sh\nif [ \"$1\" = -o ]; then shift 4; fi\nexec /bin/sh \"$@\"\n",
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            (dir, path)
        })
        .1
        .clone()
}
