//! 转发调度与认证就绪的无网络测试。
use super::*;
fn spec(forwards: Vec<TunnelForward>) -> TunnelSpec {
    TunnelSpec {
        builtin: None,
        id: "forward:test".into(),
        server_id: "server".into(),
        target: SshTarget::Alias {
            alias: "destination".into(),
        },
        options: CommonOpts::default(),
        forwards,
        readiness: Readiness::AuthMarker,
    }
}
fn events() -> Events {
    Arc::new(Mutex::new(BTreeMap::new()))
}

#[test]
fn 只返回本地监听端口_远程目标不占用本地端口() {
    let entry = Entry::new(spec(vec![
        TunnelForward::Remote {
            remote_port: 28080,
            local_port: 18080,
        },
        TunnelForward::Dynamic { local_port: 1080 },
        TunnelForward::Local {
            local_port: Some(18080),
            remote: ForwardTarget::Tcp { port: 80 },
            replace_on_conflict: false,
        },
    ]))
    .unwrap();
    assert_eq!(entry.ports, [1080, 18080]);
    assert_eq!(entry.event.local_ports, [1080, 18080]);
    let cmd = entry.command().unwrap();
    assert_eq!(
        &cmd.args[cmd.args.len() - 7..],
        [
            "-R",
            "127.0.0.1:28080:127.0.0.1:18080",
            "-D",
            "127.0.0.1:1080",
            "-L",
            "127.0.0.1:18080:127.0.0.1:80",
            "destination"
        ]
    );
    let mut invalid = spec(vec![TunnelForward::Remote {
        remote_port: 28080,
        local_port: 18080,
    }]);
    invalid.readiness = Readiness::ProbeLocalPorts;
    assert!(Entry::new(invalid).is_err());
    assert!(Entry::new(spec(vec![TunnelForward::Dynamic { local_port: 0 }])).is_err());
    assert!(
        Entry::new(spec(vec![TunnelForward::Remote {
            remote_port: 0,
            local_port: 80
        }]))
        .is_err()
    );
    assert!(
        Entry::new(spec(vec![
            TunnelForward::Dynamic { local_port: 1080 },
            TunnelForward::Dynamic { local_port: 1080 }
        ]))
        .is_err()
    );
}

#[test]
fn 固定端口不更换_远程转发失败退避_保留端口致命() {
    let mut entry = Entry::new(spec(vec![
        TunnelForward::Remote {
            remote_port: 12345,
            local_port: 1080,
        },
        TunnelForward::Dynamic { local_port: 1080 },
    ]))
    .unwrap();
    assert_eq!(
        entry
            .replace_busy_ports_with(None, |_| false, || panic!("固定端口不能更换"))
            .unwrap_err()
            .code,
        "tunnel.port_in_use"
    );
    assert_eq!(
        entry
            .replace_busy_ports_with(
                Some("bind [127.0.0.1]:1080: Address already in use"),
                |_| true,
                || panic!("固定端口不能更换")
            )
            .unwrap_err()
            .code,
        "tunnel.port_in_use"
    );
    for (attempt, delay) in [1, 2, 5, 10, 30, 60, 60].into_iter().enumerate() {
        let before = Instant::now();
        entry.fail(
            AppError::new("tunnel.remote_forward_failed").with("port", 12345),
            &events(),
        );
        assert!(!entry.fatal);
        assert_eq!(entry.event.attempt, attempt as u32 + 1);
        assert_eq!(entry.event.state, TunnelState::Reconnecting);
        assert!(entry.due >= before + Duration::from_secs(delay));
        assert_eq!(entry.ports, [1080]);
    }
    entry.network_restored(&events());
    assert!(entry.due <= Instant::now());
    entry.fail(AppError::new("tunnel.port_reserved"), &events());
    assert!(entry.fatal);
    assert_eq!(entry.event.state, TunnelState::Error);
    assert!(entry.event.next_retry_at_ms.is_none());
}

#[test]
fn 错误转发格式是致命参数错误且网络恢复不重试() {
    for kind in ["local", "remote", "dynamic"] {
        let mut entry =
            Entry::new(spec(vec![TunnelForward::Dynamic { local_port: 1080 }])).unwrap();
        let error = classify_error(
            Some(255),
            &format!("Bad {kind} forwarding specification: password=不能泄露"),
            &[],
        );
        assert_eq!(error.code, "ssh.invalid_argument");
        assert!(!format!("{error:?}").contains("不能泄露"));
        entry.fail(error, &events());
        assert!(entry.fatal);
        assert_eq!(entry.event.state, TunnelState::Error);
        assert!(entry.event.next_retry_at_ms.is_none());
        entry.network_restored(&events());
        assert_eq!(entry.event.state, TunnelState::Error);
    }
    assert!(matches!(
        Entry::new(spec(vec![TunnelForward::Local {
            local_port: Some(18080),
            remote: ForwardTarget::Host { host: "a/b".into(), port: 80 },
            replace_on_conflict: false,
        }])),
        Err(error) if error.code == "ssh.invalid_argument"
    ));
}

#[test]
fn 同步与三种转发共用每服务器三个建立名额() {
    let mut entries = HashMap::new();
    let mut queue = VecDeque::new();
    for (n, forward) in [
        TunnelForward::Local {
            local_port: Some(10001),
            remote: ForwardTarget::Tcp { port: 80 },
            replace_on_conflict: true,
        },
        TunnelForward::Remote {
            remote_port: 10002,
            local_port: 80,
        },
        TunnelForward::Dynamic { local_port: 10003 },
        TunnelForward::Local {
            local_port: Some(10004),
            remote: ForwardTarget::Tcp { port: 80 },
            replace_on_conflict: false,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let mut s = spec(vec![forward]);
        s.id = n.to_string();
        if n == 0 {
            s.readiness = Readiness::ProbeLocalPorts;
        }
        queue.push_back(s.id.clone());
        entries.insert(s.id.clone(), Entry::new(s).unwrap());
    }
    assert_eq!(
        ready_batch(&mut queue, &entries, &mut HashMap::new()),
        ["0", "1", "2"]
    );
    assert_eq!(queue, ["3"]);
}

#[cfg(unix)]
fn alive(entry: &mut Entry, program: &str, args: Vec<String>) {
    let mut process = SshProgram {
        path: program.into(),
    }
    .spawn(&crate::ssh::args::SshCommand {
        args,
        env: vec![],
        config_text: None,
        builtin: None,
        askpass: None,
    })
    .unwrap();
    drop(process.child.stdin.take());
    drop(process.child.stdout.take());
    let stderr = Reader::tail_authenticated(process.child.stderr.take().unwrap(), 128).unwrap();
    entry.connection = Some(Connection {
        process,
        stderr,
        started: Instant::now() - Duration::from_secs(10),
        ready: false,
        exited: None,
        authenticated_since: None,
        prohibited_logged: false,
    });
}

#[cfg(unix)]
#[test]
fn 别名目标接受不同hostname的认证标记且仍等待500毫秒() {
    let mut entry = Entry::new(spec(vec![TunnelForward::Remote {
        remote_port: 12345,
        local_port: 80,
    }]))
    .unwrap();
    assert_eq!(entry.command().unwrap().args.last().unwrap(), "destination");
    let process = SshProgram {
        path: "/usr/bin/sleep".into(),
    }
    .spawn(&crate::ssh::args::SshCommand {
        args: vec!["30".into()],
        env: vec![],
        config_text: None,
        builtin: None,
        askpass: None,
    })
    .unwrap();
    let mut stderr = Reader::tail_authenticated(
        std::io::Cursor::new(b"Authenticated to resolved.internal (via proxy) using publickey.\n"),
        128,
    )
    .unwrap();
    stderr.join();
    entry.connection = Some(Connection {
        process,
        stderr,
        started: Instant::now(),
        ready: false,
        exited: None,
        authenticated_since: None,
        prohibited_logged: false,
    });
    entry.poll(&events()).unwrap();
    assert_eq!(entry.event.state, TunnelState::Connecting);
    entry.connection.as_mut().unwrap().authenticated_since =
        Some(Instant::now() - Duration::from_millis(501));
    entry.poll(&events()).unwrap();
    assert_eq!(entry.event.state, TunnelState::Connected);
    assert!(entry.ports.is_empty());
}

#[cfg(unix)]
#[test]
fn 认证后仍需存活500毫秒_三种转发不探端口_网络恢复保留连接() {
    for forward in [
        TunnelForward::Local {
            local_port: Some(12345),
            remote: ForwardTarget::Tcp { port: 80 },
            replace_on_conflict: false,
        },
        TunnelForward::Remote {
            remote_port: 12345,
            local_port: 80,
        },
        TunnelForward::Dynamic { local_port: 12345 },
    ] {
        let mut entry = Entry::new(spec(vec![forward])).unwrap();
        let ports = entry.ports.clone();
        alive(&mut entry, "/usr/bin/sleep", vec!["30".into()]);
        let pid = entry.connection.as_ref().unwrap().process.child.id();
        entry.network_restored(&events());
        assert_eq!(entry.connection.as_ref().unwrap().process.child.id(), pid);
        entry.poll(&events()).unwrap();
        assert!(
            !entry.connection.as_ref().unwrap().ready,
            "只有远程转发也必须等标记"
        );
        entry
            .connection
            .as_ref()
            .unwrap()
            .stderr
            .state
            .lock()
            .unwrap()
            .authenticated = true;
        entry.poll(&events()).unwrap();
        assert!(!entry.connection.as_ref().unwrap().ready);
        entry.connection.as_mut().unwrap().authenticated_since =
            Some(Instant::now() - Duration::from_millis(501));
        entry.poll(&events()).unwrap();
        assert!(
            entry.connection.as_ref().unwrap().ready,
            "无端口监听时认证模式也应就绪"
        );
        assert_eq!(entry.event.state, TunnelState::Connected);
        let (notify, notices) = mpsc::sync_channel(1);
        entry.connected_observer = Some(notify);
        let connected_at = entry.event.last_connected_at_ms;
        entry.network_restored(&events());
        assert_eq!(entry.connection.as_ref().unwrap().process.child.id(), pid);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
        assert_eq!(entry.ports, ports);
        assert_eq!(entry.event.state, TunnelState::Connected);
        assert_eq!(entry.event.last_connected_at_ms, connected_at);
        assert!(notices.try_recv().is_ok());
    }
    let mut entry = Entry::new(spec(vec![TunnelForward::Remote {
        remote_port: 12345,
        local_port: 80,
    }]))
    .unwrap();
    alive(&mut entry, "/bin/true", vec![]);
    while entry
        .connection
        .as_mut()
        .unwrap()
        .process
        .try_wait()
        .unwrap()
        .is_none()
    {
        thread::sleep(Duration::from_millis(1));
    }
    entry
        .connection
        .as_ref()
        .unwrap()
        .stderr
        .state
        .lock()
        .unwrap()
        .authenticated = true;
    entry.connection.as_mut().unwrap().authenticated_since =
        Some(Instant::now() - Duration::from_secs(1));
    // 等待管道 EOF，模拟认证后立刻退出的 SSH。
    entry.connection.as_mut().unwrap().stderr.join();
    let error = entry.poll(&events()).unwrap_err();
    assert_ne!(entry.event.state, TunnelState::Connected);
    entry.fail(error, &events());
    entry.network_restored(&events());
    assert!(entry.connection.is_none());
    assert!(!entry.fatal);
    assert!(entry.due <= Instant::now());
    assert_eq!(entry.event.state, TunnelState::Reconnecting);
}

#[cfg(unix)]
#[test]
fn 就绪前禁止转发致命_端口转发就绪后只记录一次且继续_同步隧道始终致命_后续断网仍重试() {
    for readiness in [Readiness::AuthMarker, Readiness::ProbeLocalPorts] {
        for ready in [false, true] {
            let mut spec = spec(vec![TunnelForward::Local {
                local_port: Some(18080),
                remote: ForwardTarget::Tcp { port: 80 },
                replace_on_conflict: false,
            }]);
            spec.readiness = readiness;
            let mut entry = Entry::new(spec).unwrap();
            let process = SshProgram {
                path: "/usr/bin/sleep".into(),
            }
            .spawn(&crate::ssh::args::SshCommand {
                args: vec!["30".into()],
                env: vec![],
                config_text: None,
                builtin: None,
                askpass: None,
            })
            .unwrap();
            let pid = process.child.id();
            let mut stderr = Reader::tail_authenticated(std::io::Cursor::new(
                "channel 1: open failed: administratively prohibited: password=不能泄露\nConnection reset by peer\n".as_bytes()
            ), 512).unwrap();
            stderr.join();
            entry.connection = Some(Connection {
                process,
                stderr,
                started: Instant::now(),
                ready,
                exited: None,
                authenticated_since: None,
                prohibited_logged: false,
            });
            // 同步隧道（探本地端口）整条依赖转发，就绪后被禁止也致命；端口转发就绪后只记录
            if !ready || readiness == Readiness::ProbeLocalPorts {
                let error = entry.poll(&events()).unwrap_err();
                assert_eq!(error.code, "ssh.forwarding_prohibited");
                assert!(error.params.is_empty());
                assert!(!entry.connection.as_ref().unwrap().prohibited_logged);
                entry.fail(error, &events());
                assert!(entry.fatal);
                continue;
            }
            entry.event.state = TunnelState::Connected;
            for _ in 0..3 {
                entry.poll(&events()).unwrap();
                let connection = entry.connection.as_ref().unwrap();
                assert!(connection.prohibited_logged);
                assert_eq!(connection.process.child.id(), pid);
                assert_eq!(entry.event.state, TunnelState::Connected);
                assert!(entry.event.error.is_none());
            }
            entry
                .connection
                .as_mut()
                .unwrap()
                .process
                .terminate()
                .unwrap();
            let error = entry.poll(&events()).unwrap_err();
            assert_eq!(error.code, "ssh.unreachable");
            assert!(!format!("{error:?}").contains("不能泄露"));
            entry.fail(error, &events());
            assert!(!entry.fatal);
            assert_eq!(entry.event.state, TunnelState::Reconnecting);
        }
    }
}

#[cfg(unix)]
#[test]
fn 缺密码不起进程_等待不退避_新密码立即唤醒_二次验证停止() {
    use crate::security::vault::Zeroizing;
    use crate::ssh::{
        args::AskPass,
        askpass_pipe::{PromptKind, test_answer, test_registration_mode},
    };
    let root = crate::test_support::tempdir();
    let guard = test_registration_mode("user@host", false);
    let mut spec = spec(vec![TunnelForward::Remote {
        remote_port: 28080,
        local_port: 18080,
    }]);
    spec.target = SshTarget::Manual {
        host: "host".into(),
        port: 22,
        user: Some("user".into()),
        identity_file: None,
        jump: None,
    };
    spec.options.askpass = Some(AskPass {
        program: "askpass".into(),
        registration: guard.clone(),
    });
    let mut entry = Entry::new(spec).unwrap();
    let program = SshProgram {
        path: root.path().join("未启动的程序"),
    };
    let error = entry.connect(&program).unwrap_err();
    assert_eq!(error.code, "ssh.password_needed");
    let events = events();
    entry.fail(error, &events);
    assert_eq!(entry.event.state, TunnelState::WaitingPassword);
    assert_eq!(entry.event.attempt, 0);
    assert!(entry.event.next_retry_at_ms.is_none());
    entry.network_restored(&events);
    entry.reconnect(&events);
    assert_eq!(entry.event.state, TunnelState::WaitingPassword);
    guard.set_password(Zeroizing::new("新密码".into())).unwrap();
    entry.password_available(&events);
    assert!(entry.password_wait.is_none());
    assert_eq!(entry.event.attempt, 0);
    assert!(entry.due <= Instant::now());
    let actual = entry.command().unwrap().prepare().unwrap();
    let actual = actual.askpass.unwrap();
    assert!(test_answer(&actual, "user@host's password: ", PromptKind::Password).is_some());
    let error = actual.finish_error(AppError::new("ssh.auth_failed"));
    assert_eq!(error.code, "ssh.auth_failed");
    assert!(!guard.has_password());
    guard
        .set_password(Zeroizing::new("更新密码".into()))
        .unwrap();
    let actual = guard.prepare().unwrap();
    test_answer(&actual, "user@host's password: ", PromptKind::Password).unwrap();
    assert!(test_answer(&actual, "验证码", PromptKind::Password).is_none());
    entry.fail(
        actual.finish_error(AppError::new("ssh.auth_failed")),
        &events,
    );
    assert!(entry.fatal);
    assert_eq!(
        entry.event.error.as_ref().unwrap().code,
        "ssh.two_factor_required"
    );
    entry.password_available(&events);
    assert_eq!(entry.event.state, TunnelState::Error);
}
