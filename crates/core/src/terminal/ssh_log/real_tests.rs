//! 用本机 OpenSSH 和普通用户 sshd 采集实际日志，所有文件随测试根目录回收。
use super::*;
use crate::builtin_test_support::sshd;
use std::{
    fs,
    process::{Child, Command, Stdio},
};

struct Master(Child);
impl Drop for Master {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn server(extra: &str) -> sshd::Server {
    sshd::Server::with_root(crate::test_support::tempdir(), extra, false)
}

fn config(server: &sshd::Server) -> String {
    format!(
        "Host *\n User {}\n HostName 127.0.0.1\n Port {}\n IdentityFile {}\n IdentitiesOnly yes\n StrictHostKeyChecking accept-new\n UserKnownHostsFile {}\nHost jump\nHost target\n ProxyJump jump\n",
        server.user,
        server.port,
        server.root.path().join("client").display(),
        server.root.path().join("known_hosts").display()
    )
}

fn spec(server: &sshd::Server, target: &str, extra: &[&str], config_dir: &Path) -> TerminalSpec {
    let mut args: Vec<String> = ["-tt", "-o", "ConnectTimeout=5"]
        .into_iter()
        .chain(extra.iter().copied())
        .map(str::to_owned)
        .collect();
    args.extend([target.into(), "printf terminal-ok".into()]);
    TerminalSpec {
        kind: TerminalKind::Remote {
            ssh: "/usr/bin/ssh".into(),
            command: crate::ssh::args::SshCommand {
                args,
                env: vec![],
                config_text: Some(config(server)),
                hooked: false,
                builtin: None,
                askpass: None,
            },
            config_dir: config_dir.to_owned(),
        },
        cols: 80,
        rows: 24,
    }
}

fn run(spec: TerminalSpec, password: bool) -> (Vec<ConnectStage>, String, Option<i32>) {
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
    let mut answered = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("终端应及时结束")
        {
            TerminalEvent::Stage(stage) => stages.push(stage),
            TerminalEvent::Output(bytes) => {
                manager.ack(&id, bytes.len()).unwrap();
                output.extend(bytes);
                if password && !answered && String::from_utf8_lossy(&output).contains("password:") {
                    manager.write(&id, b"incorrect-test-password\r").unwrap();
                    answered = true;
                }
            }
            TerminalEvent::Exited { code, .. } => {
                assert!(rx.try_recv().is_err());
                return (stages, String::from_utf8_lossy(&output).into_owned(), code);
            }
        }
    }
}

#[test]
#[ignore = "需要本机 sshd、伪终端和随机监听端口"]
fn 真实openssh终端正常与跳板阶段有序且无调试输出() {
    let server = server("");
    let logs = server.root.path().join("terminal");
    for target in ["direct", "target"] {
        let (stages, output, code) = run(spec(&server, target, &[], &logs), false);
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
        assert_eq!(code, Some(0), "{output}");
        assert!(output.contains("terminal-ok"));
        assert!(!output.contains("debug1:"), "{output}");
        assert_eq!(fs::read_dir(&logs).unwrap().count(), 0);
    }
}

#[test]
#[ignore = "需要本机 sshd、伪终端和随机监听端口"]
fn 真实openssh密码错误与拒绝连接先回显错误再退出() {
    let server = server("Match all\n PasswordAuthentication yes");
    let logs = server.root.path().join("terminal");
    let (stages, output, code) = run(
        spec(
            &server,
            "direct",
            &[
                "-o",
                "PubkeyAuthentication=no",
                "-o",
                "PreferredAuthentications=password",
                "-o",
                "NumberOfPasswordPrompts=1",
            ],
            &logs,
        ),
        true,
    );
    assert_eq!(
        stages,
        [
            ConnectStage::Resolve,
            ConnectStage::Connect,
            ConnectStage::Auth
        ]
    );
    assert_eq!(code, Some(255));
    assert!(output.contains("Permission denied"), "{output}");
    assert!(!output.contains("debug1:"));
    assert_eq!(fs::read_dir(&logs).unwrap().count(), 0);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    drop(listener);
    let (stages, output, code) = run(spec(&server, "direct", &["-p", &port], &logs), false);
    assert_eq!(stages, [ConnectStage::Resolve, ConnectStage::Connect]);
    assert_eq!(code, Some(255));
    assert!(output.contains("Connection refused"), "{output}");
    assert_eq!(fs::read_dir(&logs).unwrap().count(), 0);
}

#[test]
#[ignore = "需要本机 sshd、伪终端和随机监听端口"]
fn 真实openssh复用主连接直接进入会话和就绪() {
    let server = server("");
    let conf = server.root.path().join("master.conf");
    fs::write(&conf, config(&server)).unwrap();
    let socket = server.root.path().join("mux");
    let mut master = Master(
        Command::new("/usr/bin/ssh")
            .args(["-M", "-N", "-F"])
            .arg(&conf)
            .arg("-S")
            .arg(&socket)
            .arg("direct")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(master.0.try_wait().unwrap().is_none(), "主连接意外退出");
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    let logs = server.root.path().join("terminal");
    let (stages, output, code) = run(
        spec(&server, "direct", &["-S", socket.to_str().unwrap()], &logs),
        false,
    );
    assert_eq!(
        stages,
        [
            ConnectStage::Resolve,
            ConnectStage::Session,
            ConnectStage::Ready
        ]
    );
    assert_eq!(code, Some(0), "{output}");
    assert!(output.contains("terminal-ok"));
    assert!(!output.contains("debug1:"));
    assert_eq!(fs::read_dir(&logs).unwrap().count(), 0);
}

#[test]
#[ignore = "需要本机 sshd、伪终端和随机监听端口"]
fn 真实日志片段覆盖直连与代理格式() {
    let server = server("");
    let conf = server.root.path().join("ssh.conf");
    fs::write(&conf, config(&server)).unwrap();
    for target in ["direct", "target"] {
        let path = server.root.path().join(format!("{target}.log"));
        let status = Command::new("/usr/bin/ssh")
            .args(["-F"])
            .arg(&conf)
            .args(["-o", "LogLevel=DEBUG1", "-E"])
            .arg(&path)
            .args([target, "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = fs::read(&path).unwrap();
        let mut parser = Parser::default();
        let mut events = VecDeque::new();
        parser.feed(&bytes, &mut events);
        parser.finish(&mut events);
        let stages: Vec<_> = events
            .into_iter()
            .filter_map(|e| match e {
                TerminalEvent::Stage(stage) => Some(stage),
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
    }
}

#[test]
#[ignore = "需要本机 sshd 和随机监听端口"]
fn 内置真实直连跳板与复用的完整阶段顺序() {
    use crate::builtin_test_support::Fixture;
    for jump in [false, true] {
        let mut f = Fixture::new();
        if jump {
            let mut hop = f.server.clone();
            hop.id = "jump".into();
            f.server.jump_server_id = Some(hop.id.clone());
            f.manager
                .set_server_resolver(Arc::new(move |_| Ok(Some(hop.clone()))));
        }
        let manager = TerminalManager::new();
        let command = f.command();
        for expected in [
            vec![
                ConnectStage::Resolve,
                ConnectStage::Connect,
                ConnectStage::Auth,
                ConnectStage::Session,
                ConnectStage::Ready,
            ],
            vec![ConnectStage::Session, ConnectStage::Ready],
        ] {
            let (tx, rx) = mpsc::channel();
            let kind = crate::terminal::remote(
                &f.manager,
                command.builtin.as_ref().unwrap().request(),
                None,
                false,
                || panic!("必须使用内置引擎"),
            )
            .unwrap();
            let id = manager
                .open(
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
            let mut stages = Vec::new();
            while stages.last() != Some(&ConnectStage::Ready) {
                match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TerminalEvent::Stage(stage) => stages.push(stage),
                    TerminalEvent::Output(bytes) => manager.ack(&id, bytes.len()).unwrap(),
                    TerminalEvent::Exited { .. } => panic!("会话提前结束"),
                }
            }
            assert_eq!(stages, expected);
            manager.close(&id).unwrap();
        }
    }
}
