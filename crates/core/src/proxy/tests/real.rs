use super::*;
use pierlink_ssh_engine::test_support::proxy::{Reply, TestProxy};
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
};

#[test]
#[ignore = "需要本机 TCP 端口"]
fn 核心同步连接及错误码() {
    for kind in [ProxyKind::Http, ProxyKind::Socks5] {
        let proxy = TestProxy::start(kind, Reply::Banner).unwrap();
        let mut stream = connect(
            &proxy.server,
            "unresolved.invalid",
            22,
            Duration::from_secs(2),
        )
        .unwrap();
        let mut bytes = String::new();
        stream.read_to_string(&mut bytes).unwrap();
        assert_eq!(bytes, "SSH-2.0-test\r\n");
        let proxy = TestProxy::start(kind, Reply::AuthRequired).unwrap();
        let error = connect(&proxy.server, "host", 22, Duration::from_secs(2)).unwrap_err();
        assert_eq!(error.code, "proxy.auth_required");
        assert_eq!(
            error.params["reason"],
            "代理要求用户名和密码，Pierlink 不支持带密码的代理"
        );
        let proxy = TestProxy::start(kind, Reply::Refuse).unwrap();
        assert_eq!(
            connect(&proxy.server, "host", 22, Duration::from_secs(2))
                .unwrap_err()
                .code,
            "proxy.refused"
        );
    }
}

fn helper() -> std::path::PathBuf {
    let executable = std::env::current_exe().unwrap();
    let path = executable
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/proxy_helper");
    assert!(path.is_file(), "先构建 proxy_helper 示例程序");
    path
}

#[test]
#[ignore = "需要本机 sshd 和 TCP 端口"]
fn 连接池统一代理并且名单内直连() {
    if isolated("proxy::tests::real::连接池统一代理并且名单内直连") {
        return;
    }
    use crate::{
        builtin_test_support::Fixture,
        remote::run::{RunLimits, run_script},
    };
    for kind in [ProxyKind::Http, ProxyKind::Socks5] {
        let mut fixture = Fixture::new();
        let proxy = TestProxy::start(
            kind,
            Reply::Forward(([127, 0, 0, 1], fixture.sshd.port).into()),
        )
        .unwrap();
        set_settings(ProxySettings {
            mode: ProxyMode::Custom(proxy.server.clone()),
            bypass: vec![],
        });
        fixture.server.host = Some("unresolved.invalid".into());
        let command = fixture.command();
        let result = run_script(
            &fixture.program(),
            &command,
            "printf '{\"ok\":true}'",
            RunLimits::default(),
        )
        .unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(
            *proxy.targets.lock().unwrap(),
            [("unresolved.invalid".into(), fixture.sshd.port)]
        );
        // 只断开这台服务器（close_all 会让连接管理器不再接受新连接）
        fixture.manager.close_server(&fixture.server.id).unwrap();
        fixture.server.host = Some("127.0.0.1".into());
        run_script(
            &fixture.program(),
            &fixture.command(),
            "printf '{}'",
            RunLimits::default(),
        )
        .unwrap();
        assert_eq!(proxy.targets.lock().unwrap().len(), 1);
    }
}

#[test]
#[ignore = "需要本机 TCP 端口"]
fn 帮手远端结束时不等待仍打开的标准输入() {
    let proxy = TestProxy::start(ProxyKind::Http, Reply::Banner).unwrap();
    let mut child = Command::new(helper())
        .args(["--proxy-connect", "unresolved.invalid", "22"])
        .envs(environment(&proxy.server))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("帮手没有收尾");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("SSH-2.0-test"));
}

#[test]
#[ignore = "需要本机 sshd、TCP 端口和已构建的 proxy_helper 示例"]
fn 系统openssh通过带空格中文路径的帮手连接及跳板第一跳() {
    if isolated("proxy::tests::real::系统openssh通过带空格中文路径的帮手连接及跳板第一跳")
    {
        return;
    }
    use crate::{
        builtin_test_support::sshd,
        process::{SshProgram, environment},
        ssh::{
            args::{CommonOpts, Purpose, build_args},
            target::SshTarget,
        },
    };
    let root = crate::test_support::tempdir();
    let executable = root.path().join("中文 帮手%程序");
    std::fs::copy(helper(), &executable).unwrap();
    let sshd = sshd::Server::with_root(crate::test_support::tempdir(), "", false);
    let public = std::fs::read_to_string(sshd.root.path().join("host.pub")).unwrap();
    let hosts = root.path().join("known_hosts");
    std::fs::write(
        &hosts,
        format!(
            "[unresolved.invalid]:{} {}\n[127.0.0.1]:{} {}\n",
            sshd.port,
            public.trim(),
            sshd.port,
            public.trim()
        ),
    )
    .unwrap();
    let program = SshProgram {
        path: "/usr/bin/ssh".into(),
    };
    for kind in [ProxyKind::Http, ProxyKind::Socks5] {
        for jump in [false, true] {
            let proxy =
                TestProxy::start(kind, Reply::Forward(([127, 0, 0, 1], sshd.port).into())).unwrap();
            set_settings(ProxySettings {
                mode: ProxyMode::Custom(proxy.server.clone()),
                bypass: vec![],
            });
            let node = |host: &str, jump| SshTarget::Manual {
                host: host.into(),
                port: sshd.port,
                user: Some(sshd.user.clone()),
                identity_file: Some(sshd.root.path().join("client")),
                jump,
            };
            let first = node("unresolved.invalid", None);
            let target = if jump {
                node("127.0.0.1", Some(Box::new(first)))
            } else {
                first
            };
            let command = build_args(
                &target,
                &Purpose::Background,
                &CommonOpts {
                    config_file: Some(root.path().join("original.conf")),
                    known_hosts: Some(hosts.clone()),
                    connect_timeout: Some(3),
                    ..Default::default()
                },
            )
            .unwrap();
            let prepared = openssh::prepare_with_exe(&program, &command, &executable).unwrap();
            assert!(!prepared.command.args.iter().any(|a| a.contains("secret")));
            let mut process = program
                .spawn_with_environment(&prepared.command, environment(&prepared.command))
                .unwrap();
            process
                .child
                .stdin
                .take()
                .unwrap()
                .write_all(b"printf proxy-ok\n")
                .unwrap();
            let mut output =
                crate::process::Reader::start(process.child.stdout.take().unwrap(), 16384).unwrap();
            let mut error =
                crate::process::Reader::start(process.child.stderr.take().unwrap(), 16384).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let status = loop {
                if let Some(status) = process.try_wait().unwrap() {
                    break status;
                }
                assert!(std::time::Instant::now() < deadline, "SSH 代理连接超时");
                std::thread::sleep(Duration::from_millis(10));
            };
            output.join();
            error.join();
            assert!(
                status.success(),
                "{}",
                String::from_utf8_lossy(&error.state.lock().unwrap().bytes)
            );
            assert_eq!(output.state.lock().unwrap().bytes, b"proxy-ok");
            assert_eq!(
                *proxy.targets.lock().unwrap(),
                [("unresolved.invalid".into(), sshd.port)]
            );
        }
    }
}

#[test]
#[ignore = "需要本机 TCP 端口和已构建的 proxy_helper 示例"]
fn 帮手遇到认证要求输出中文原因() {
    let proxy = TestProxy::start(ProxyKind::Http, Reply::AuthRequired).unwrap();
    let output = Command::new(helper())
        .args(["--proxy-connect", "unresolved.invalid", "22"])
        .envs(environment(&proxy.server))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("代理要求用户名和密码，Pierlink 不支持带密码的代理"));
    assert!(!error.contains("secret"));
    assert!(!error.contains("wrong"));
}

#[test]
#[ignore = "需要本机 TCP 端口和已构建的 proxy_helper 示例"]
fn 帮手无代理直连并在输出背压时响应输入结束() {
    use std::net::{Shutdown, TcpListener};
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let remote = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let buffer = vec![b'x'; 1024 * 1024];
        for _ in 0..32 {
            if stream.write_all(&buffer).is_err() {
                break;
            }
        }
        let _ = stream.shutdown(Shutdown::Both);
    });
    let mut child = Command::new(helper())
        .args(["--proxy-connect", "127.0.0.1", &port.to_string()])
        .env_remove("PIERLINK_PROXY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = child.stdout.take().unwrap();
    let mut first = [0; 1];
    output.read_exact(&mut first).unwrap();
    assert_eq!(first, [b'x']);
    std::thread::sleep(Duration::from_millis(100));
    drop(child.stdin.take());
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            remote.join().unwrap();
            panic!("输出背压阻塞了帮手退出");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success());
    remote.join().unwrap();
}
