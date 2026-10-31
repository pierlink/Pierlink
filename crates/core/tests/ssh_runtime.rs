//! 普通用户启动本机 sshd；本次集成测试的全部文件共用一个数据盘临时根目录。
#![cfg(unix)]
#[path = "../src/test_support.rs"]
mod test_support;

use pierlink_core::{
    process::SshProgram,
    remote::run::{RunLimits, run_script},
    ssh::{
        args::{CommonOpts, Forward, ForwardTarget, Purpose, Readiness, SshCommand, build_args},
        host_key::{HostKeyPrompt, probe},
        target::SshTarget,
    },
    tunnel::{TunnelEvent, TunnelForward, TunnelManager, TunnelSpec, TunnelState},
};
use std::{
    fs,
    net::TcpListener,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
struct Server {
    child: Option<Child>,
    root: tempfile::TempDir,
    port: u16,
    user: String,
    sftp: bool,
}
impl Server {
    fn new() -> Self {
        let root = test_support::tempdir();
        for name in ["host", "client", "changed"] {
            let out = Command::new("/usr/bin/ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(root.path().join(name))
                .output()
                .unwrap();
            assert!(out.status.success());
        }
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string();
        let socket = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        let mut server = Self {
            child: None,
            root,
            port,
            user,
            sftp: false,
        };
        fs::write(server.path(".bashrc"), "printf '.bashrc 噪声\\n'\n").unwrap();
        server.start("host", true);
        server
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    fn start(&mut self, key: &str, forwarding: bool) {
        let mut config = format!(
            "Port {}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {} {}\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAcceptEnv LC_*\nAllowTcpForwarding {}\nLogLevel ERROR\nForceCommand /bin/bash --rcfile {} -ic \"$SSH_ORIGINAL_COMMAND\"\n",
            self.port,
            self.path(key).display(),
            self.path("sshd.pid").display(),
            self.path("client.pub").display(),
            self.path(".ssh/authorized_keys").display(),
            if forwarding { "yes" } else { "no" },
            self.path(".bashrc").display()
        );
        if self.sftp {
            config = config
                .lines()
                .filter(|line| !line.starts_with("ForceCommand "))
                .map(|line| format!("{line}\n"))
                .collect();
            config.push_str("Subsystem sftp /usr/lib/openssh/sftp-server\n");
        }
        fs::write(self.path("sshd_config"), config).unwrap();
        let log = fs::File::create(self.path("sshd.log")).unwrap();
        let child = Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(self.path("sshd_config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .process_group(0)
            .spawn()
            .unwrap();
        self.child = Some(child);
        let start = Instant::now();
        loop {
            assert!(
                self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "sshd 启动失败：{}",
                fs::read_to_string(self.path("sshd.log")).unwrap()
            );
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(3));
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // sshd 的会话可能独立 setsid；先暂停父进程，再枚举并杀掉后代，避免重连时残留会话。
            let pid = child.id();
            unsafe {
                libc::kill(pid as i32, libc::SIGSTOP);
            }
            kill_descendants(pid);
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    fn target(&self) -> SshTarget {
        SshTarget::Manual {
            host: "127.0.0.1".into(),
            port: self.port,
            user: Some(self.user.clone()),
            identity_file: Some(self.path("client")),
            jump: None,
        }
    }
    fn opts(&self, hosts: &str) -> CommonOpts {
        CommonOpts {
            known_hosts: Some(self.path(hosts)),
            connect_timeout: Some(2),
            ..Default::default()
        }
    }
    fn cmd(&self, hosts: &str) -> SshCommand {
        build_args(&self.target(), &Purpose::Background, &self.opts(hosts)).unwrap()
    }
    fn trust(&self, hosts: &str) {
        let public = fs::read_to_string(self.path("host.pub")).unwrap();
        fs::write(
            self.path(hosts),
            format!("[127.0.0.1]:{} {public}", self.port),
        )
        .unwrap();
    }
    fn spec(&self, id: &str, port: Option<u16>, replace: bool) -> TunnelSpec {
        TunnelSpec {
            builtin: None,
            id: id.into(),
            server_id: "本机测试".into(),
            target: self.target(),
            options: self.opts("known_hosts"),
            forwards: vec![TunnelForward::Local {
                local_port: port,
                remote: ForwardTarget::Tcp { port: self.port },
                replace_on_conflict: replace,
            }],
            readiness: Readiness::ProbeLocalPorts,
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}
fn kill_descendants(pid: u32) {
    let Ok(entries) = fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(child) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(") ") else {
            continue;
        };
        if rest
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u32>().ok())
            == Some(pid)
        {
            unsafe {
                libc::kill(child as i32, libc::SIGSTOP);
            }
            kill_descendants(child);
            unsafe {
                libc::kill(child as i32, libc::SIGKILL);
            }
        }
    }
}
fn wait_state(manager: &TunnelManager, id: &str, state: TunnelState) -> TunnelEvent {
    let start = Instant::now();
    loop {
        for event in manager.drain_events().unwrap() {
            if event.tunnel_id == id && event.state == state {
                return event;
            }
            assert!(
                event.state != TunnelState::Error || state == TunnelState::Error,
                "隧道失败：{event:?}"
            );
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "未收到 {state:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}
fn assert_confirm(prompt: &HostKeyPrompt) {
    match prompt {
        HostKeyPrompt::Confirm(c) => {
            assert_eq!(c.info.key_type, "ED25519");
            assert!(c.info.fingerprint.starts_with("SHA256:"));
        }
        _ => panic!("应当询问指纹"),
    }
}

#[test]
fn sshd_别名与hostname不同_认证标记就绪且不连接用户服务() {
    use pierlink_core::ssh::{args::explicit_target, config::parse_ssh_g};
    let server = Server::new();
    server.trust("known_hosts");
    let config = server.path("alias_config");
    fs::write(
        &config,
        format!(
            "Host destination\n HostName 127.0.0.1\n Port {}\n User {}\n IdentityFile {}\n IdentitiesOnly yes\n",
            server.port,
            server.user,
            server.path("client").display()
        ),
    )
    .unwrap();
    let output = Command::new("/usr/bin/ssh")
        .args(["-G", "-F"])
        .arg(&config)
        .arg("destination")
        .output()
        .unwrap();
    assert!(output.status.success());
    let resolved = parse_ssh_g(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    assert_eq!(resolved.hostname, "127.0.0.1");
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut spec = server.spec("别名转发", None, true);
    spec.target = SshTarget::Explicit(Box::new(
        explicit_target(&resolved, "destination", &config).unwrap(),
    ));
    spec.readiness = Readiness::AuthMarker;
    spec.forwards = vec![TunnelForward::Local {
        local_port: None,
        remote: ForwardTarget::Tcp {
            port: listener.local_addr().unwrap().port(),
        },
        replace_on_conflict: false,
    }];
    let manager = TunnelManager::new(SshProgram {
        path: "/usr/bin/ssh".into(),
    })
    .unwrap();
    let started = Instant::now();
    let ports = manager.start(spec).unwrap();
    let event = wait_state(&manager, "别名转发", TunnelState::Connected);
    assert!(started.elapsed() >= Duration::from_millis(500));
    assert_eq!(event.local_ports, ports);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "认证就绪不能探测用户服务"
    );
    manager.stop("别名转发").unwrap();
}

#[test]
#[ignore = "需要允许本机高位端口监听的沙箱外环境"]
fn 真实ssh子系统文件传输() {
    use pierlink_core::sftp::{
        SftpSession,
        transfer::{Conflict, download, upload},
    };
    use std::sync::atomic::AtomicBool;
    let mut server = Server::new();
    server.stop();
    server.sftp = true;
    server.start("host", true);
    server.trust("sftp_hosts");
    let command = build_args(&server.target(), &Purpose::Sftp, &server.opts("sftp_hosts")).unwrap();
    let mut session = SftpSession::open(
        &SshProgram {
            path: "/usr/bin/ssh".into(),
        },
        &command,
        Duration::from_secs(5),
    )
    .unwrap();
    let source = server.path("source");
    let remote = server.path("remote");
    let target = server.path("target");
    fs::create_dir(&remote).unwrap();
    fs::create_dir(&target).unwrap();
    let bytes = vec![123; 1_100_001];
    fs::write(&source, &bytes).unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        upload(
            &mut session,
            &[source],
            remote.to_str().unwrap(),
            Conflict::Error,
            &mut |_| {},
            &cancel
        )
        .unwrap()
        .succeeded,
        1
    );
    assert_eq!(
        session
            .list_dir(remote.to_str().unwrap(), 10)
            .unwrap()
            .entries
            .len(),
        1
    );
    assert_eq!(
        download(
            &mut session,
            &[remote.join("source").to_str().unwrap().into()],
            &target,
            Conflict::Error,
            &mut |_| {},
            &cancel
        )
        .unwrap()
        .succeeded,
        1
    );
    assert_eq!(fs::read(target.join("source")).unwrap(), bytes);
}
#[test]
fn sshd_后台脚本_指纹确认_隧道及重连() {
    let started = Instant::now();
    assert!(pierlink_core::tunnel::allocate_port().unwrap() > 0);
    let mut server = Server::new();
    let program = SshProgram {
        path: "/usr/bin/ssh".into(),
    };
    let limits = RunLimits {
        timeout: Duration::from_secs(4),
        ..Default::default()
    };
    let unknown = run_script(&program, &server.cmd("unknown"), "printf true", limits).unwrap_err();
    assert_eq!(unknown.code, "ssh.host_key_unknown");
    let prompt = probe(&program, &server.cmd("rejected")).unwrap();
    assert_confirm(&prompt);
    assert_eq!(
        prompt.answer(false).err().unwrap().code,
        "ssh.host_key_rejected"
    );
    let prompt = probe(&program, &server.cmd("known_hosts")).unwrap();
    assert_confirm(&prompt);
    assert!(matches!(
        prompt.answer(true).unwrap(),
        HostKeyPrompt::Trusted
    ));
    assert!(matches!(
        probe(&program, &server.cmd("known_hosts")).unwrap(),
        HostKeyPrompt::Trusted
    ));
    assert!(Path::new(&server.path("known_hosts")).exists());
    let cmd = server.cmd("known_hosts");
    // S11、S27：实际 bash rc 文件输出噪声，正文 cat 读 stdin 后仍能得到完整 JSON。
    assert_eq!(
        run_script(&program, &cmd, "cat\nprintf '{\"ok\":true}'", limits).unwrap(),
        serde_json::json!({"ok":true})
    );
    assert_eq!(
        run_script(
            &program,
            &cmd,
            "sleep 2\nprintf true",
            RunLimits {
                timeout: Duration::from_millis(300),
                ..limits
            }
        )
        .unwrap_err()
        .code,
        "remote.timeout"
    );
    for body in ["yes", "yes >&2"] {
        assert_eq!(
            run_script(
                &program,
                &cmd,
                body,
                RunLimits {
                    stdout_bytes: 4096,
                    stderr_bytes: 4096,
                    ..limits
                }
            )
            .unwrap_err()
            .code,
            "remote.output_too_large"
        );
    }
    let collision = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let occupied = collision.local_addr().unwrap().port();
    // P1、P2、P8：自动换端口、固定端口错误、重连保持新端口。
    let manager = TunnelManager::new(program.clone()).unwrap();
    manager
        .start(server.spec("固定", Some(occupied), false))
        .unwrap();
    let error = wait_state(&manager, "固定", TunnelState::Error);
    assert_eq!(error.error.unwrap().code, "tunnel.port_in_use");
    manager.stop("固定").unwrap();
    manager
        .start(server.spec("自动", Some(occupied), true))
        .unwrap();
    let connected = wait_state(&manager, "自动", TunnelState::Connected);
    let port = connected.local_ports[0];
    assert_ne!(port, occupied);
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
    let direct = build_args(
        &server.target(),
        &Purpose::Tunnel {
            forwards: vec![Forward::Local {
                local_port: occupied,
                remote: ForwardTarget::Tcp { port: server.port },
            }],
            readiness: Readiness::ProbeLocalPorts,
        },
        &server.opts("known_hosts"),
    )
    .unwrap();
    assert_eq!(
        run_script(&program, &direct, "", limits).unwrap_err().code,
        "ssh.failed"
    );
    server.stop();
    thread::sleep(Duration::from_millis(400));
    server.start("host", true);
    manager.network_restored().unwrap();
    let reconnected = wait_state(&manager, "自动", TunnelState::Connected);
    assert_eq!(reconnected.local_ports[0], port);
    manager.stop("自动").unwrap();
    assert_eq!(
        wait_state(&manager, "自动", TunnelState::Stopped).state,
        TunnelState::Stopped
    );
    assert_eq!(manager.stop("不存在").unwrap_err().code, "tunnel.not_found");
    // P9：本地监听建立后，实际访问触发服务器拒绝转发。
    server.stop();
    server.start("host", false);
    manager.start(server.spec("禁止", None, true)).unwrap();
    assert_eq!(
        wait_state(&manager, "禁止", TunnelState::Error)
            .error
            .unwrap()
            .code,
        "ssh.forwarding_prohibited"
    );
    manager.stop("禁止").unwrap();
    server.stop();
    server.start("changed", true);
    assert_eq!(
        run_script(&program, &cmd, "printf true", limits)
            .unwrap_err()
            .code,
        "ssh.host_key_changed"
    );
    assert_eq!(
        probe(&program, &cmd).err().unwrap().code,
        "ssh.host_key_changed"
    );
    server.trust("unrelated");
    drop(manager);
    assert!(started.elapsed() < Duration::from_secs(60));
}

#[test]
#[ignore = "需要允许本机高位端口监听的沙箱外环境"]
fn 真实ssh终端当前目录跟随cd() {
    use pierlink_core::cwd::{remote_cwd, remote_shell_status};
    use pierlink_core::terminal::{TerminalEvent, TerminalKind, TerminalManager, TerminalSpec};
    use std::sync::{Arc, mpsc};
    let mut server = Server::new();
    // 不用 ForceCommand，走用户真实的登录 shell
    server.stop();
    server.sftp = true;
    server.start("host", true);
    server.trust("cwd_hosts");
    let marker = "testcwdmarker01";
    let command = build_args(
        &server.target(),
        &Purpose::Terminal {
            login_shell: None,
            cwd_marker: Some(marker.into()),
            integration: false,
        },
        &server.opts("cwd_hosts"),
    )
    .unwrap();
    let manager = TerminalManager::new();
    let (tx, rx) = mpsc::channel();
    let id = manager
        .open(
            TerminalSpec {
                kind: TerminalKind::Remote {
                    ssh: "/usr/bin/ssh".into(),
                    command,
                    config_dir: server.root.path().into(),
                },
                cols: 80,
                rows: 24,
            },
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
    let program = SshProgram {
        path: "/usr/bin/ssh".into(),
    };
    let target = server.path("目录 with space");
    fs::create_dir(&target).unwrap();
    // 等登录完成（shell 进程出现）后 cd，再查询
    let drain = |rx: &mpsc::Receiver<TerminalEvent>| {
        while let Ok(event) = rx.recv_timeout(Duration::from_millis(500)) {
            if let TerminalEvent::Output(bytes) = event {
                let _ = manager.ack(&id, bytes.len());
            }
        }
    };
    drain(&rx);
    manager
        .write(&id, format!("cd '{}'\n", target.display()).as_bytes())
        .unwrap();
    drain(&rx);
    let found = remote_cwd(&program, &server.cmd("cwd_hosts"), marker).unwrap();
    assert_eq!(found.as_deref(), target.to_str());
    assert_eq!(manager.busy(&id).unwrap(), None);
    assert!(
        !remote_shell_status(&program, &server.cmd("cwd_hosts"), marker)
            .unwrap()
            .unwrap()
            .busy
    );
    manager.write(&id, b"sleep 30\n").unwrap();
    let start = Instant::now();
    while !remote_shell_status(&program, &server.cmd("cwd_hosts"), marker)
        .unwrap()
        .unwrap()
        .busy
    {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(20));
    }
    // 标记不存在时为 None
    assert_eq!(
        remote_cwd(&program, &server.cmd("cwd_hosts"), "nosuchmarker").unwrap(),
        None
    );
    manager.close(&id).unwrap();
}

#[test]
fn 真实ssh交互终端回显() {
    use pierlink_core::terminal::{TerminalEvent, TerminalKind, TerminalManager, TerminalSpec};
    use std::sync::{Arc, mpsc};
    let server = Server::new();
    server.trust("terminal_hosts");
    let command = build_args(
        &server.target(),
        &Purpose::Terminal {
            login_shell: Some("/bin/sh".into()),
            cwd_marker: None,
            integration: false,
        },
        &server.opts("terminal_hosts"),
    )
    .unwrap();
    let manager = TerminalManager::new();
    let (tx, rx) = mpsc::channel();
    let id = manager
        .open(
            TerminalSpec {
                kind: TerminalKind::Remote {
                    ssh: "/usr/bin/ssh".into(),
                    command,
                    config_dir: server.root.path().into(),
                },
                cols: 80,
                rows: 24,
            },
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
    manager
        .write(&id, b"printf 'pl-%s\\n' 'connected'; exit\n")
        .unwrap();
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
                assert_eq!(code, Some(0));
                assert!(!closed_by_user);
                break;
            }
        }
    }
    assert!(String::from_utf8_lossy(&output).contains("pl-connected"));
}

#[test]
fn 同步双转发_重连改端口与空闲隧道回收() {
    use pierlink_core::{
        sync::{
            EndpointSpec, LocalSync, ServerEndpointInfo, ServerLink, TaskSpec, create_task,
            remove_task,
        },
        syncthing::{
            folder::FolderKind,
            home::{GuiListen, HomeSpec, init_home},
            process::{SyncthingBin, start},
        },
    };
    let Some(bin) = std::env::var_os("PIERLINK_TEST_SYNCTHING") else {
        eprintln!("跳过 SSH 同步测试：未设置 PIERLINK_TEST_SYNCTHING");
        return;
    };
    let bin = SyncthingBin { path: bin.into() };
    let root = test_support::tempdir();
    let mut server = Server::new();
    server.trust("known_hosts");
    let gui = pierlink_core::tunnel::allocate_port().unwrap();
    let mut sync = pierlink_core::tunnel::allocate_port().unwrap();
    while sync == gui {
        sync = pierlink_core::tunnel::allocate_port().unwrap();
    }
    let local = LocalSync::start(
        &bin,
        &root.path().join("l"),
        gui,
        sync,
        "local-key",
        "password",
    )
    .unwrap();
    let remote_sync = pierlink_core::tunnel::allocate_port().unwrap();
    let remote_spec = HomeSpec {
        home: root.path().join("r"),
        gui: GuiListen::Unix {
            path: root.path().join("s/api"),
        },
        gui_user: "pierlink".into(),
        listen_port: remote_sync,
    };
    let remote_info = init_home(&bin, &remote_spec, "password").unwrap();
    let remote = start(&bin, &remote_spec.home, &remote_spec.gui, "remote-key").unwrap();
    let info = ServerEndpointInfo {
        server_id: "sync-server".into(),
        name: "测试服务器".into(),
        device_id: remote_info.device_id.clone(),
        socket_path: root.path().join("s/api").display().to_string(),
        sync_port: remote_sync,
        api_key: "remote-key".into(),
        state_dir: root.path().join("state").display().to_string(),
    };
    let manager = TunnelManager::new(SshProgram {
        path: "/usr/bin/ssh".into(),
    })
    .unwrap();
    let mut link = ServerLink::connect(
        &manager,
        server.target(),
        server.opts("known_hosts"),
        &info,
        &local,
        "测试电脑",
    )
    .unwrap();
    link.client().ping().unwrap();
    let first = link.local_sync_port();
    manager.reconnect_all().unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if link.refresh_address(&local).is_ok() {
            break;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(first, link.local_sync_port());
    // 停止测试服务器，使 SSH 无法先于测试抢回端口，再占住原 A 并恢复服务器。
    server.stop();
    manager.reconnect_all().unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    let occupied = loop {
        if let Ok(s) = TcpListener::bind(("127.0.0.1", first)) {
            break s;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(5));
    };
    server.start("host", true);
    manager.network_restored().unwrap();
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        if link.refresh_address(&local).is_ok() && link.local_sync_port() != first {
            break;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(50));
    }
    drop(occupied);
    let devices = local.client().list_devices().unwrap();
    let device = devices
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["deviceID"] == info.device_id)
        .unwrap();
    // 重拨会在 tcp:// 和 tcp4:// 之间切换地址写法（绕过 Syncthing 的重拨冷却），两种都对
    let port = link.local_sync_port();
    assert!(
        device["addresses"] == serde_json::json!([format!("tcp://127.0.0.1:{port}")])
            || device["addresses"] == serde_json::json!([format!("tcp4://127.0.0.1:{port}")]),
        "设备地址不对：{}",
        device["addresses"]
    );
    let lp = root.path().join("lf");
    let rp = root.path().join("rf");
    fs::create_dir(&lp).unwrap();
    fs::create_dir(&rp).unwrap();
    let spec = TaskSpec {
        data_dir: root.path().join("data"),
        ignores_source: pierlink_core::sync::IgnoresSource::Unconfirmed,
        folder_id: None,
        name: "任务".into(),
        local_path: lp,
        local_mode: FolderKind::default(),
        endpoints: vec![EndpointSpec {
            server_id: "sync-server".into(),
            remote_path: rp.display().to_string(),
            mode: FolderKind::SendReceive,
        }],
        ignore_rules: vec![],
        watch_delay_s: 1,
        collab: false,
        versioning: false,
        versions_root: root.path().join("v"),
    };
    let id = create_task(&local, &[&link], &spec).unwrap();
    remove_task(&local, &[&link], &id).unwrap();
    assert_eq!(
        manager.stop("sync:sync-server").unwrap_err().code,
        "tunnel.not_found"
    );
    link.disconnect(&manager).unwrap();
    remote.stop().unwrap();
    local.stop().unwrap();
}

#[test]
fn 真实安装验证移除专用密钥() {
    use pierlink_core::{keys::remote, ssh::args::DedicatedKey};
    let server = Server::new();
    server.trust("known_hosts");
    fs::write(
        server.path(".bashrc"),
        format!(
            "export HOME={}\n",
            pierlink_core::ssh::quote::sh_quote(server.root.path().to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    let program = SshProgram {
        path: "/usr/bin/ssh".into(),
    };
    let bootstrap = server.cmd("known_hosts");
    // 密钥加密和口令传递由本机单元测试验证；此处只验证服务端授权生命周期。
    let mut opts = server.opts("known_hosts");
    opts.dedicated_key = Some(DedicatedKey {
        private_path: server.path("changed"),
        askpass_program: "/bin/false".into(),
        credential_id: "pierlink/test/unused".into(),
    });
    let key_cmd = build_args(&server.target(), &Purpose::Background, &opts).unwrap();
    assert!(remote::verify(&program, &key_cmd).is_err());
    let public = fs::read_to_string(server.path("changed.pub")).unwrap();
    let public = public.trim();
    assert_eq!(
        remote::install_and_verify(&program, &bootstrap, &key_cmd, public)
            .unwrap()
            .outcome,
        remote::InstallOutcome::Installed
    );
    assert!(remote::list(&program, &key_cmd, public).unwrap()[0].ours);
    assert_eq!(
        remote::remove(&program, &bootstrap, public).unwrap(),
        remote::RemoveOutcome::Removed { count: 1 }
    );
    assert!(remote::verify(&program, &key_cmd).is_err());
}

#[test]
#[ignore = "需要沙箱外运行：真实 Unix IPC 与假 SSH 密码认证，沙箱禁止 socket"]
fn 真实ipc_共享密码_缺密码不起进程_隧道与采样自动恢复() {
    use pierlink_core::{
        monitor::{Monitor, MonitorEvent},
        security::vault::{PasswordVault, Zeroizing},
        ssh::{
            args::AskPass,
            askpass_pipe::{AskHandler, AskpassServer, PromptKind, RegistrationSpec},
        },
    };
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{Arc, mpsc},
    };
    struct NoUi;
    impl AskHandler for NoUi {
        fn ask_password(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<Zeroizing<String>>, pierlink_core::error::AppError> {
            panic!("后台连接不应弹框")
        }
        fn ask_other(
            &self,
            _: &str,
            _: &str,
            _: PromptKind,
        ) -> Result<Option<Zeroizing<String>>, pierlink_core::error::AppError> {
            panic!("后台连接不应弹框")
        }
    }
    let root = test_support::tempdir();
    let vault = Arc::new(PasswordVault::default());
    let server = AskpassServer::start(vault.clone(), Arc::new(NoUi), root.path()).unwrap();
    let registration = server
        .register(RegistrationSpec {
            server_id: "password-server".into(),
            expected_prompt: "user@host".into(),
            interactive: false,
            purpose: "测试后台".into(),
            password_only: true,
        })
        .unwrap();
    let fake = root.path().join("ssh");
    fs::write(&fake, include_str!("ssh_runtime/fake_password_ssh.py")).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let launches = root.path().join("launches");
    let options = CommonOpts {
        askpass: Some(AskPass {
            program: std::env::current_exe().unwrap(),
            registration: registration.clone(),
        }),
        ..Default::default()
    };
    let target = SshTarget::Manual {
        host: "host".into(),
        port: 22,
        user: Some("user".into()),
        identity_file: None,
        jump: None,
    };
    let mut command = build_args(&target, &Purpose::Background, &options).unwrap();
    command.env.push((
        "PIERLINK_TEST_SSH_LAUNCHES".into(),
        launches.display().to_string(),
    ));
    let program = SshProgram { path: fake };
    let limits = RunLimits {
        timeout: Duration::from_secs(5),
        ..Default::default()
    };
    assert_eq!(
        run_script(&program, &command, "printf true", limits)
            .unwrap_err()
            .code,
        "ssh.password_needed"
    );
    assert!(!launches.exists());
    vault
        .set("password-server", Zeroizing::new("测试密码".into()))
        .unwrap();
    assert_eq!(
        run_script(&program, &command, "printf true", limits).unwrap(),
        true
    );
    let password = vault.get("password-server").unwrap().unwrap();
    assert!(vault.invalidate("password-server", password.generation));
    let (tx, rx) = mpsc::channel();
    let monitor = Monitor::start(
        program.clone(),
        command.clone(),
        Duration::from_secs(1),
        Arc::new(move |e| {
            tx.send(e).unwrap();
        }),
    )
    .unwrap();
    loop {
        if matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            MonitorEvent::WaitingPassword { .. }
        ) {
            break;
        }
    }
    // 隧道命令没有额外测试环境；替身的计数文件路径通过脚本内固定测试目录提供。
    let source = include_str!("ssh_runtime/fake_password_ssh.py").replace(
        "os.environ['PIERLINK_TEST_SSH_LAUNCHES']",
        &serde_json::to_string(&launches.display().to_string()).unwrap(),
    );
    fs::write(&program.path, source).unwrap();
    let manager = TunnelManager::new(program.clone()).unwrap();
    manager
        .start(TunnelSpec {
            builtin: None,
            id: "password-tunnel".into(),
            server_id: "password-server".into(),
            target,
            options,
            forwards: vec![TunnelForward::Remote {
                remote_port: 28080,
                local_port: 18080,
            }],
            readiness: Readiness::AuthMarker,
        })
        .unwrap();
    let waiting = wait_state(&manager, "password-tunnel", TunnelState::WaitingPassword);
    assert_eq!(waiting.attempt, 0);
    assert!(waiting.next_retry_at_ms.is_none());
    assert_eq!(fs::read_to_string(&launches).unwrap().lines().count(), 1);
    let start = Instant::now();
    vault
        .set("password-server", Zeroizing::new("测试密码".into()))
        .unwrap();
    wait_state(&manager, "password-tunnel", TunnelState::Connected);
    assert!(start.elapsed() < Duration::from_secs(5));
    loop {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            MonitorEvent::Sample(_) => break,
            MonitorEvent::Failed { error } => panic!("采样失败：{error}"),
            _ => {}
        }
    }
    monitor.stop();
    manager.stop("password-tunnel").unwrap();
}
