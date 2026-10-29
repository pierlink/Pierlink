//! 仅对测试创建的显示号、进程和套接字做真实验证。

use super::*;
use crate::{
    builtin::{
        ConnectionCallbacks, ConnectionClass, ConnectionManager, ConnectionRequest,
        DedicatedCredential,
    },
    error::AppError,
    process::{SshProgram, TreeProcess, spawn_tree},
    security::vault::{PasswordVault, Zeroizing},
    ssh::{
        args::{CommonOpts, Purpose, SshCommand, build_args},
        quote::sh_quote,
        target::SshTarget,
    },
    storage::{AuthMethod, ServerSource},
};
use pierlink_ssh_engine::{Challenge, HostKey};
use std::{
    fs,
    io::Read,
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

struct Desktop {
    process: TreeProcess,
    root: tempfile::TempDir,
    display: u16,
}
impl Desktop {
    fn start(binary: &Path) -> Self {
        let root = crate::test_support::tempdir();
        let cookie = Command::new("mcookie").output().unwrap();
        assert!(cookie.status.success());
        let cookie = Zeroizing::new(String::from_utf8(cookie.stdout).unwrap());
        let auth = root.path().join("xauth");
        fs::write(&auth, []).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&auth, fs::Permissions::from_mode(0o600)).unwrap();
        for display in 90..=99 {
            if Path::new(&format!("/tmp/.X{display}-lock")).exists()
                || Path::new(&format!("/tmp/.X11-unix/X{display}")).exists()
            {
                continue;
            }
            let status = Command::new("xauth")
                .arg("-f")
                .arg(&auth)
                .args([
                    "add",
                    &format!(":{display}"),
                    "MIT-MAGIC-COOKIE-1",
                    cookie.trim(),
                ])
                .status()
                .unwrap();
            assert!(status.success());
            let socket = root.path().join("s,中文'\"空间");
            let args = vec![
                format!(":{display}"),
                "-rfbport".into(),
                "-1".into(),
                "-rfbunixpath".into(),
                socket.to_str().unwrap().into(),
                "-rfbunixmode".into(),
                "0600".into(),
                "-SecurityTypes".into(),
                "None".into(),
                "-auth".into(),
                auth.to_str().unwrap().into(),
                "-geometry".into(),
                "800x600".into(),
                "-nolisten".into(),
                "tcp".into(),
                "-noreset".into(),
            ];
            let process = spawn_tree(binary, &args, None, &[]).unwrap();
            let mut desktop = Self {
                process,
                root,
                display,
            };
            let start = Instant::now();
            while !socket.exists() {
                if desktop.process.try_wait().unwrap().is_some() {
                    let mut error = String::new();
                    desktop
                        .process
                        .child
                        .stderr
                        .take()
                        .unwrap()
                        .read_to_string(&mut error)
                        .unwrap();
                    panic!("测试桌面启动失败：{error}");
                }
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "测试套接字未出现"
                );
                thread::sleep(Duration::from_millis(20));
            }
            // 只向当前测试刚创建的显示发送绘图请求。
            let status = Command::new("xsetroot")
                .args(["-solid", "#4080ff"])
                .env("DISPLAY", format!(":{display}"))
                .env("XAUTHORITY", &auth)
                .status()
                .unwrap();
            assert!(status.success());
            return desktop;
        }
        panic!("没有空闲测试显示号（仅允许 90 到 99）");
    }
    fn target(&self) -> VncTarget {
        VncTarget::UnixPath(
            self.root
                .path()
                .join("s,中文'\"空间")
                .to_str()
                .unwrap()
                .into(),
        )
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        // 持有未回收子进程，PID 不会被复用；SIGTERM 让 X server 自行删除它创建的锁和套接字。
        if self.process.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.process.child.id() as i32, libc::SIGTERM);
            }
            let start = Instant::now();
            while self.process.try_wait().ok().flatten().is_none()
                && start.elapsed() < Duration::from_secs(5)
            {
                thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = self.process.terminate();
    }
}

struct Callbacks(String);
impl ConnectionCallbacks for Callbacks {
    fn check_host_key(&self, _: &str, key: &HostKey, _: &AtomicBool) -> Result<bool, AppError> {
        Ok(key.fingerprint == self.0)
    }
    fn passphrase(
        &self,
        _: &str,
        _: &Path,
        _: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        panic!("测试密钥不带口令")
    }
    fn password(&self, _: &str, _: &AtomicBool) -> Result<Option<Zeroizing<String>>, AppError> {
        panic!("测试不使用密码")
    }
    fn dedicated_key(
        &self,
        _: &str,
        _: &AtomicBool,
    ) -> Result<Option<DedicatedCredential>, AppError> {
        Ok(None)
    }
    fn keyboard_interactive(
        &self,
        _: &str,
        _: Challenge,
        _: &AtomicBool,
    ) -> Result<Option<Vec<Zeroizing<String>>>, AppError> {
        panic!("测试不使用验证码")
    }
}
struct Sshd {
    process: TreeProcess,
    root: tempfile::TempDir,
    command: SshCommand,
    openssh: SshCommand,
    manager: ConnectionManager,
}
impl Sshd {
    fn start(prohibit: bool) -> Self {
        use std::os::unix::fs::PermissionsExt;
        // 沿用本机 sshd 夹具的密钥和配置方式；清理只遍历本进程的后代，避免扫描其他用户会话。
        let root = crate::test_support::tempdir();
        let p = root.path();
        for name in ["host", "client"] {
            crate::builtin_test_support::sshd::keygen(&p.join(name), "ed25519", "");
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string();
        let fingerprint = String::from_utf8(
            Command::new("/usr/bin/ssh-keygen")
                .args(["-lf"])
                .arg(p.join("host.pub"))
                .args(["-E", "sha256"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();
        let home = p.join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join("xsessions")).unwrap();
        fs::create_dir(home.join("bin")).unwrap();
        fs::write(home.join("bin/loginctl"), "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(home.join("bin/loginctl"), fs::Permissions::from_mode(0o700)).unwrap();
        let config = format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nStrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPermitRootLogin yes\nLogLevel ERROR\nForceCommand /usr/bin/env HOME={} PIERLINK_VNC_XSESSIONS_DIR={} PATH={}:/usr/bin:/bin SHELL=/bin/sh /bin/sh -c \"$SSH_ORIGINAL_COMMAND\"\n{}",
            p.join("host").display(),
            p.join("pid").display(),
            p.join("client.pub").display(),
            sh_quote(home.to_str().unwrap()).unwrap(),
            sh_quote(home.join("xsessions").to_str().unwrap()).unwrap(),
            sh_quote(home.join("bin").to_str().unwrap()).unwrap(),
            if prohibit {
                "DisableForwarding yes\n"
            } else {
                ""
            }
        );
        fs::write(p.join("sshd_config"), config).unwrap();
        let process = spawn_tree(
            Path::new("/usr/sbin/sshd"),
            &[
                "-D".into(),
                "-e".into(),
                "-f".into(),
                p.join("sshd_config").to_str().unwrap().into(),
            ],
            None,
            &[],
        )
        .unwrap();
        let mut server =
            crate::server_input::build_alias_server("桌面测试", None, "vnc", 0).unwrap();
        server.source = ServerSource::Manual;
        server.ssh_alias = None;
        server.host = Some("127.0.0.1".into());
        server.port = Some(port);
        server.username = Some(user.clone());
        server.auth_method = AuthMethod::Key;
        server.identity_file = Some(p.join("client").to_str().unwrap().into());
        server.connection_engine = crate::storage::Engine::Builtin;
        let manager = ConnectionManager::default();
        let command = crate::remote::command::background(
            &manager,
            ConnectionRequest {
                server: &server,
                class: ConnectionClass::Main,
                vault: Arc::new(PasswordVault::default()),
                callbacks: Arc::new(Callbacks(fingerprint)),
                interactive: false,
                cancelled: Arc::new(AtomicBool::new(false)),
            },
            || panic!("必须选择内置连接"),
        )
        .unwrap();
        let known = p.join("known_hosts");
        fs::write(
            &known,
            format!(
                "[127.0.0.1]:{port} {}",
                fs::read_to_string(p.join("host.pub")).unwrap()
            ),
        )
        .unwrap();
        let openssh = build_args(
            &SshTarget::Manual {
                host: "127.0.0.1".into(),
                port,
                user: Some(user),
                identity_file: Some(p.join("client")),
                jump: None,
            },
            &Purpose::Background,
            &CommonOpts {
                known_hosts: Some(known),
                ..Default::default()
            },
        )
        .unwrap();
        let mut sshd = Self {
            process,
            root,
            command,
            openssh,
            manager,
        };
        let start = Instant::now();
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                sshd.process.try_wait().unwrap().is_none(),
                "测试 sshd 启动失败"
            );
            assert!(start.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(20));
        }
        sshd
    }
}
fn stop_owned_tree(pid: u32) {
    if unsafe { libc::kill(pid as i32, libc::SIGSTOP) } != 0 {
        return;
    }
    let children =
        fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap_or_default();
    for child in children.split_whitespace().filter_map(|s| s.parse().ok()) {
        stop_owned_tree(child);
    }
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}
impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.manager.close_all();
        stop_owned_tree(self.process.child.id());
        let _ = self.process.terminate();
    }
}

fn read_n(stream: &mut VncStream, n: usize) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bytes = vec![0; n];
    let mut offset = 0;
    while offset < n {
        offset += stream
            .reader
            .read_until(&mut bytes[offset..], Some(deadline))
            .unwrap();
    }
    bytes
}
fn frame(mut stream: VncStream) {
    assert_eq!(read_n(&mut stream, 12), b"RFB 003.008\n");
    stream.writer.write(b"RFB 003.008\n").unwrap();
    let n = read_n(&mut stream, 1)[0] as usize;
    assert!(read_n(&mut stream, n).contains(&1));
    stream.writer.write(&[1]).unwrap();
    assert_eq!(read_n(&mut stream, 4), [0, 0, 0, 0]);
    stream.writer.write(&[1]).unwrap();
    let init = read_n(&mut stream, 24);
    assert_eq!(&init[..4], &[3, 32, 2, 88]);
    let name = u32::from_be_bytes(init[20..24].try_into().unwrap()) as usize;
    assert!(name < 4096);
    read_n(&mut stream, name);
    // 请求原始的 32 位小端 RGB 像素，关闭压缩，核对完整画面。
    stream
        .writer
        .write(&[
            0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0,
        ])
        .unwrap();
    // 游标单独发送，避免被合成到待核对的背景像素中。
    stream
        .writer
        .write(&[2, 0, 0, 2, 0, 0, 0, 0, 255, 255, 255, 17])
        .unwrap();
    stream
        .writer
        .write(&[3, 0, 0, 0, 0, 0, 3, 32, 2, 88])
        .unwrap();
    let mut pixels = 0;
    while pixels < 800 * 600 {
        let update = read_n(&mut stream, 4);
        assert_eq!(update[0], 0);
        let count = u16::from_be_bytes(update[2..4].try_into().unwrap());
        assert!(count > 0);
        for _ in 0..count {
            let rect = read_n(&mut stream, 12);
            let encoding = i32::from_be_bytes(rect[8..12].try_into().unwrap());
            let w = u16::from_be_bytes(rect[4..6].try_into().unwrap()) as usize;
            let h = u16::from_be_bytes(rect[6..8].try_into().unwrap()) as usize;
            assert!(w * h <= 800 * 600);
            if encoding == -239 {
                read_n(&mut stream, w * h * 4 + w.div_ceil(8) * h);
                continue;
            }
            assert_eq!(encoding, 0);
            let bytes = read_n(&mut stream, w * h * 4);
            assert!(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|pixel| pixel[..3] == [255, 128, 64])
            );
            pixels += w * h;
        }
    }
    assert_eq!(pixels, 800 * 600);
    stream.close();
}

#[test]
#[ignore = "需要 PIERLINK_TEST_TIGERVNC、本机 sshd 和随机高位端口"]
fn 临时桌面经直连和中继读取真实像素并检测() {
    let binary = PathBuf::from(
        std::env::var_os("PIERLINK_TEST_TIGERVNC").expect("必须显式指定测试 TigerVNC"),
    );
    let desktop = Desktop::start(&binary);
    let root = desktop.root.path().to_owned();
    let display = desktop.display;
    let pid = desktop.process.child.id();
    let sshd = Sshd::start(false);
    let ssh_root = sshd.root.path().to_owned();
    let program = SshProgram {
        path: "/usr/bin/ssh".into(),
    };
    let target = desktop.target();
    let direct = open_stream(&program, &sshd.command, &Tools::default(), target.clone()).unwrap();
    frame(direct);
    for tools in [
        Tools {
            socat: true,
            ..Default::default()
        },
        Tools {
            python3: true,
            ..Default::default()
        },
        Tools {
            perl: true,
            ..Default::default()
        },
    ] {
        frame(transport::open_relay(&program, &sshd.command, &tools, &target).unwrap());
    }
    let probe = probe_rfb(
        open_stream(&program, &sshd.command, &Tools::default(), target.clone()).unwrap(),
        Duration::from_secs(3),
    )
    .unwrap();
    assert!(probe.no_auth);
    // 限定为本测试的 PID，不遍历或读取其他桌面进程。
    let script = detection::script().replace(
        "for proc in \"$proc_root\"/[0-9]*; do",
        &format!("for proc in /proc/{pid}; do"),
    );
    let value =
        crate::remote::run::run_script(&program, &sshd.command, &script, Default::default())
            .unwrap();
    let detected = detection::parse_detection(value).unwrap();
    assert_eq!(detected.sessions.len(), 1);
    let session = &detected.sessions[0];
    assert_eq!(session.pid, pid);
    assert_eq!(session.display, Some(display));
    assert!(!session.pierlink);
    assert!(session.listens.is_empty());
    if let VncTarget::UnixPath(path) = &target {
        assert_eq!(&session.unix_paths, &vec![path.clone()]);
    }
    // 老版本 OpenSSH 会自动用 exec；支持路径转发的版本保留 RFB 前缀。
    frame(
        open_stream(
            &program,
            &sshd.openssh,
            &Tools {
                python3: true,
                ..Default::default()
            },
            target.clone(),
        )
        .unwrap(),
    );
    drop(sshd);
    assert!(!ssh_root.exists());
    let prohibited = Sshd::start(true);
    frame(
        open_stream(
            &program,
            &prohibited.command,
            &Tools {
                python3: true,
                ..Default::default()
            },
            target.clone(),
        )
        .unwrap(),
    );
    assert_eq!(
        open_stream(
            &program,
            &prohibited.command,
            &Tools::default(),
            target.clone()
        )
        .err()
        .unwrap()
        .code,
        "vnc.relay_unavailable"
    );
    let unavailable = VncTarget::UnixPath(root.join("不存在").to_str().unwrap().into());
    assert_eq!(
        open_stream(
            &program,
            &prohibited.command,
            &Tools {
                python3: true,
                ..Default::default()
            },
            unavailable
        )
        .err()
        .unwrap()
        .code,
        "vnc.forwarding_prohibited"
    );
    drop(prohibited);
    drop(desktop);
    assert!(!root.exists());
    assert!(!Path::new(&format!("/tmp/.X{display}-lock")).exists());
    assert!(!Path::new(&format!("/tmp/.X11-unix/X{display}")).exists());
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
}

struct ManagedDesktop<'a> {
    program: &'a SshProgram,
    sshd: &'a Sshd,
    desktop: PierlinkDesktop,
}
impl Drop for ManagedDesktop<'_> {
    fn drop(&mut self) {
        let _ = stop_desktop(self.program, &self.sshd.command, self.desktop.display);
    }
}

#[test]
#[ignore = "需要显式指定 PIERLINK_TEST_TIGERVNC，只创建 90 到 99 的测试显示"]
fn 专用桌面创建握手检测关闭和重启遗留清理() {
    use std::os::unix::fs::PermissionsExt;
    let binary = std::env::var("PIERLINK_TEST_TIGERVNC").expect("必须显式指定测试 TigerVNC");
    let sshd = Sshd::start(false);
    let home = sshd.root.path().join("home");
    fs::write(
        home.join("xsessions/test.desktop"),
        "[Desktop Entry]\nName=测试桌面\nExec=/bin/sh -c \"sleep 600\"\nDesktopNames=TEST;\n",
    )
    .unwrap();
    let program = SshProgram {
        path: "/usr/bin/ssh".into(),
    };
    let xvnc = desktop::verify_binary(&program, &sshd.command, &binary).unwrap();
    let created = create_desktop(
        &program,
        &sshd.command,
        CreateDesktop {
            desktop_id: "test".into(),
            width: 800,
            height: 600,
            xvnc,
        },
    )
    .unwrap();
    let managed = ManagedDesktop {
        program: &program,
        sshd: &sshd,
        desktop: created,
    };
    assert!((90..=99).contains(&managed.desktop.display));
    let dir = home.join(".local/state/pierlink/vnc");
    let state_file = dir.join(format!("{}.json", managed.desktop.display));
    let state: serde_json::Value = serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
    let pid = state["xvnc_pid"].as_u64().unwrap();
    assert_eq!(
        fs::metadata(&state_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&managed.desktop.socket)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(state["xauth"].as_str().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let stream = open_stream(
        &program,
        &sshd.command,
        &Tools::default(),
        VncTarget::UnixPath(managed.desktop.socket.clone()),
    )
    .unwrap();
    let probe = probe_rfb(stream, Duration::from_secs(5)).unwrap();
    assert!(probe.no_auth);
    // 检测脚本只采样本测试拥有的 PID，绝不读取其他显示。
    let script = detection::script().replace(
        "for proc in \"$proc_root\"/[0-9]*; do",
        &format!("for proc in /proc/{pid}; do"),
    );
    let value =
        crate::remote::run::run_script(&program, &sshd.command, &script, Default::default())
            .unwrap();
    let result = detection::parse_detection(value).unwrap();
    assert_eq!(result.pierlink_desktops, vec![managed.desktop.clone()]);
    assert!(result.sessions[0].pierlink);
    assert!(result.sessions[0].listens.is_empty());
    stop_desktop(&program, &sshd.command, managed.desktop.display).unwrap();
    assert!(!state_file.exists());
    assert!(!Path::new(&managed.desktop.socket).exists());
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert!(!Path::new(&format!("/tmp/.X{}-lock", managed.desktop.display)).exists());
    assert!(!Path::new(&format!("/tmp/.X11-unix/X{}", managed.desktop.display)).exists());
    let mut child = spawn_tree(Path::new("/bin/sleep"), &["30".into()], None, &[]).unwrap();
    let mut old = state;
    old["boot_id"] = "已重启".into();
    old["xvnc_pid"] = child.child.id().into();
    fs::write(&state_file, serde_json::to_vec(&old).unwrap()).unwrap();
    stop_desktop(&program, &sshd.command, managed.desktop.display).unwrap();
    assert!(child.try_wait().unwrap().is_none());
    child.terminate().unwrap();
}

#[test]
#[ignore = "需要 PIERLINK_TEST_APT=1 和可用发行版源，只解包到临时 HOME"]
fn 发行版程序在临时家目录免_root_获取() {
    if std::env::var("PIERLINK_TEST_APT").as_deref() != Ok("1") {
        eprintln!("跳过：没有设置 PIERLINK_TEST_APT=1");
        return;
    }
    let sshd = Sshd::start(false);
    let program = SshProgram {
        path: "/usr/bin/ssh".into(),
    };
    let binary = acquire::remote_package(&program, &sshd.command, true).unwrap();
    assert!(binary.path.starts_with(sshd.root.path().to_str().unwrap()));
    assert_eq!(binary.flavor, Flavor::TigerVnc);
    assert!(binary.version >= (1, 9, 0));
    let root = sshd.root.path().join("home/.local/share/pierlink/vnc");
    assert_eq!(fs::read_dir(root).unwrap().count(), 1);
}

#[test]
#[ignore = "需要 PIERLINK_TEST_REMOTE=用户@主机 和 PIERLINK_TEST_REMOTE_KEY=私钥路径"]
fn 真实服务器轻量桌面获取创建握手并关闭() {
    use crate::remote::run::run_script;

    fn checked<T>(step: &str, result: Result<T, AppError>) -> T {
        result.unwrap_or_else(|error| {
            panic!("{step}失败：错误码={}，参数={:?}", error.code, error.params)
        })
    }

    struct Cleanup<'a> {
        program: &'a SshProgram,
        command: &'a SshCommand,
        display: Option<u16>,
    }
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            if let Some(display) = self.display
                && let Err(error) = stop_desktop(self.program, self.command, display)
            {
                // 展开 panic 时仍尝试关闭自己创建的桌面，不用第二次 panic 遮住原始错误。
                eprintln!(
                    "清理桌面 :{display} 失败：错误码={}，参数={:?}",
                    error.code, error.params
                );
            }
        }
    }

    let (Some(remote), Some(key)) = (
        std::env::var_os("PIERLINK_TEST_REMOTE").filter(|s| !s.is_empty()),
        std::env::var_os("PIERLINK_TEST_REMOTE_KEY").filter(|s| !s.is_empty()),
    ) else {
        eprintln!("跳过：必须同时设置 PIERLINK_TEST_REMOTE 和 PIERLINK_TEST_REMOTE_KEY");
        return;
    };
    let (user, host) = checked(
        "解析服务器地址",
        remote
            .to_str()
            .and_then(|s| s.split_once('@'))
            .filter(|(user, host)| !user.is_empty() && !host.is_empty() && !host.contains('@'))
            .ok_or_else(|| {
                AppError::new("ssh.invalid_parameter")
                    .with("field", "PIERLINK_TEST_REMOTE")
                    .with("reason", "应为 用户@主机")
            }),
    );
    let root = crate::test_support::tempdir();
    let program = SshProgram {
        path: "/usr/bin/ssh".into(),
    };
    let mut command = checked(
        "构造 SSH 参数",
        build_args(
            &SshTarget::Manual {
                host: host.into(),
                port: 22,
                user: Some(user.into()),
                identity_file: Some(key.into()),
                jump: None,
            },
            &Purpose::Background,
            &CommonOpts {
                known_hosts: Some(root.path().join("known_hosts")),
                ..Default::default()
            },
        ),
    );
    // 首次连接接受主机密钥，后续步骤复用同一临时记录；保留后台用途的其他限制。
    for arg in &mut command.args {
        if arg == "StrictHostKeyChecking=yes" {
            *arg = "StrictHostKeyChecking=accept-new".into();
        }
    }

    eprintln!("开始检测服务器");
    let detected = checked("检测服务器", detect(&program, &command));
    eprintln!(
        "检测结果：发行版={} {}，架构={}，有 TigerVNC={}，桌面环境数={}，light_desktop_available={}，xkb_namespace={:?}",
        detected.distro.id,
        detected.distro.version_id,
        detected.arch,
        detected.tigervnc.is_some(),
        detected.desktops.len(),
        detected.light_desktop_available,
        detected.xkb_namespace,
    );
    let cancel = AtomicBool::new(false);
    let xvnc = checked(
        "获取 TigerVNC",
        acquire_xvnc(
            &program,
            &command,
            &detected,
            AcquireOptions {
                allow_official_download: false,
                manual_package: None,
            },
            &|step| eprintln!("获取 TigerVNC：{step:?}"),
            &cancel,
        ),
    );
    eprintln!(
        "TigerVNC 就绪：路径={}，版本={}.{}.{}",
        xvnc.path, xvnc.version.0, xvnc.version.1, xvnc.version.2
    );
    let prefix = checked(
        "获取轻量桌面",
        acquire_light_desktop(
            &program,
            &command,
            &|step| match step {
                AcquireStep::Download { bytes, .. } => {
                    eprintln!("轻量桌面下载字节数：{bytes}");
                }
                other => eprintln!("获取轻量桌面：{other:?}"),
            },
            &cancel,
        ),
    );
    eprintln!("轻量桌面就绪：前缀={prefix}");
    let created = checked(
        "创建轻量桌面",
        create_desktop(
            &program,
            &command,
            CreateDesktop {
                desktop_id: LIGHT_DESKTOP_ID.into(),
                width: 800,
                height: 600,
                xvnc,
            },
        ),
    );
    let mut cleanup = Cleanup {
        program: &program,
        command: &command,
        display: Some(created.display),
    };
    eprintln!(
        "桌面已创建：显示号=:{}，套接字={}",
        created.display, created.socket
    );
    let stream = checked(
        "打开桌面字节流",
        open_stream(
            &program,
            &command,
            &detected.tools,
            VncTarget::UnixPath(created.socket.clone()),
        ),
    );
    // 与应用的 RFB 探测时限一致，其余步骤直接沿用接口的默认时限。
    let probe = checked("探测 RFB 握手", probe_rfb(stream, Duration::from_secs(15)));
    assert!(
        probe.no_auth,
        "RFB 未提供无密码认证：错误码=test.rfb_auth_required，参数={probe:?}"
    );
    eprintln!("RFB 握手成功：版本={}，无密码认证=true", probe.version);

    // 停止前记住自己桌面的 PID；其他现有桌面的进程可以继续运行。
    let active = checked("检测新桌面进程", detect(&program, &command));
    let pid = checked(
        "定位新桌面进程",
        active
            .sessions
            .iter()
            .find(|session| session.pierlink && session.display == Some(created.display))
            .map(|session| session.pid)
            .ok_or_else(|| {
                AppError::new("test.desktop_process_missing").with("display", created.display)
            }),
    );
    checked(
        "关闭轻量桌面",
        stop_desktop(&program, &command, created.display),
    );
    cleanup.display = None;
    let stopped = checked("关闭后检测服务器", detect(&program, &command));
    assert!(
        !stopped
            .pierlink_desktops
            .iter()
            .any(|desktop| desktop.display == created.display || desktop.socket == created.socket)
            && !stopped.sessions.iter().any(|session| session.pid == pid),
        "关闭后仍检测到桌面：错误码=test.desktop_remaining，参数={{显示号:{}, PID:{pid}}}",
        created.display
    );
    // pgrep 的 1 表示没有匹配；工具缺失或查询失败必须使测试失败。
    let value = checked(
        "查询服务器残留进程",
        run_script(
            &program,
            &command,
            r#"
uid=$(id -u) || exit
printf '['
separator=
for name in Xvnc Xtigervnc; do
    pids=$(pgrep -u "$uid" -x "$name")
    status=$?
    case "$status" in
        0|1) ;;
        *) printf '无法查询 %s 进程\n' "$name" >&2; exit "$status";;
    esac
    for pid in $pids; do
        printf '%s%s' "$separator" "$pid"
        separator=,
    done
done
printf ']'
"#,
            Default::default(),
        ),
    );
    let remaining: Vec<u32> = checked(
        "解析服务器进程列表",
        serde_json::from_value(value).map_err(|_| AppError::new("test.invalid_process_list")),
    );
    assert!(
        !remaining.contains(&pid),
        "桌面进程未退出：错误码=test.desktop_process_remaining，参数={{PID:{pid}}}"
    );
    eprintln!("桌面 :{} 已关闭，进程 {pid} 已退出", created.display);
}

#[test]
#[ignore = "需要 PIERLINK_TEST_APT=1、PIERLINK_TEST_TIGERVNC、软件源及本机 sshd"]
fn 免_root_轻量桌面出现窗口管理器终端并完整回收() {
    assert_eq!(std::env::var("PIERLINK_TEST_APT").as_deref(), Ok("1"));
    let binary = std::env::var("PIERLINK_TEST_TIGERVNC").expect("必须指定测试 TigerVNC");
    let sshd = Sshd::start(false);
    let home = sshd.root.path().join("home");
    let program = SshProgram {
        path: "/usr/bin/ssh".into(),
    };
    let prefix =
        acquire_light_desktop(&program, &sshd.command, &|_| {}, &AtomicBool::new(false)).unwrap();
    assert!(Path::new(&prefix).starts_with(&home));
    for name in ["openbox", "xterm"] {
        assert!(Path::new(&prefix).join("usr/bin").join(name).is_file());
    }
    let created = create_desktop(
        &program,
        &sshd.command,
        CreateDesktop {
            desktop_id: LIGHT_DESKTOP_ID.into(),
            width: 800,
            height: 600,
            xvnc: desktop::verify_binary(&program, &sshd.command, &binary).unwrap(),
        },
    )
    .unwrap();
    let managed = ManagedDesktop {
        program: &program,
        sshd: &sshd,
        desktop: created,
    };
    let display = managed.desktop.display;
    assert!((90..=99).contains(&display));
    let dir = home.join(".local/state/pierlink/vnc");
    let state_file = dir.join(format!("{display}.json"));
    let state: serde_json::Value = serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
    let pid = state["xvnc_pid"].as_u64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        // 只查询本测试新建的显示与认证文件。
        let tree = Command::new("xwininfo")
            .args(["-root", "-tree"])
            .env("DISPLAY", format!(":{display}"))
            .env("XAUTHORITY", state["xauth"].as_str().unwrap())
            .output()
            .unwrap();
        let tree = String::from_utf8_lossy(&tree.stdout);
        if tree.contains("Openbox") && tree.contains("Pierlink") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "未出现 Openbox 和终端窗口：{tree}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    // 记录自己监督进程的后代，关闭后逐个核对，避免遍历其他会话。
    fn descendants(pid: u32, out: &mut Vec<u32>) {
        out.push(pid);
        let children =
            fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap_or_default();
        for child in children.split_whitespace().filter_map(|s| s.parse().ok()) {
            descendants(child, out);
        }
    }
    let mut owned = Vec::new();
    descendants(state["pgid"].as_u64().unwrap() as u32, &mut owned);
    let resources = dir.join(format!("{display}.cache/Xresources"));
    assert_eq!(
        fs::metadata(&resources).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        fs::read_to_string(&resources)
            .unwrap()
            .contains("XTerm*selectToClipboard: true")
    );
    let expected = format!("XENVIRONMENT={}", resources.display());
    for name in ["xterm", "openbox"] {
        let process = owned
            .iter()
            .find(|pid| {
                fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|s| s.trim() == name)
            })
            .unwrap_or_else(|| panic!("没有找到桌面进程：{name}"));
        let environment = fs::read(format!("/proc/{process}/environ")).unwrap();
        assert!(
            environment
                .split(|b| *b == 0)
                .any(|item| item == expected.as_bytes()),
            "{name} 未继承终端资源"
        );
    }
    stop_desktop(&program, &sshd.command, display).unwrap();
    for child in owned {
        if let Ok(stat) = fs::read_to_string(format!("/proc/{child}/stat")) {
            assert!(
                stat.rsplit_once(") ").unwrap().1.starts_with('Z'),
                "桌面后代进程未退出：{child}"
            );
        }
    }
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    for path in [
        state_file,
        dir.join(format!("{display}.xauth")),
        dir.join(format!("{display}.cache")),
        PathBuf::from(&managed.desktop.socket),
        PathBuf::from(format!("/tmp/.X{display}-lock")),
        PathBuf::from(format!("/tmp/.X11-unix/X{display}")),
    ] {
        assert!(!path.exists(), "桌面残留未清理：{}", path.display());
    }
    assert_eq!(
        fs::read_dir(home.join(".cache/pierlink/apt"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
#[ignore = "需要 PIERLINK_TEST_TIGERVNC；不允许用户命名空间时说明原因并跳过"]
fn 用户命名空间桌面握手属主权限和清理() {
    use std::{
        io::Write,
        os::unix::{
            fs::{MetadataExt, PermissionsExt, symlink},
            net::UnixStream,
        },
    };
    let Some(binary) = std::env::var_os("PIERLINK_TEST_TIGERVNC") else {
        eprintln!("跳过：没有设置 PIERLINK_TEST_TIGERVNC");
        return;
    };
    let (root, program, mut cmd) = desktop_tests::local();
    let capability = desktop::run(
        &program,
        &cmd,
        r#"if reason=$(xkb_namespace_probe); then printf '{"available":true}'; else printf '{"available":false,"reason":'; json "$reason"; printf '}'; fi"#,
        15,
    ).unwrap();
    if capability["available"] != true {
        eprintln!("跳过：{}", capability["reason"]);
        return;
    }
    let binary = fs::canonicalize(binary).unwrap();
    let compiler = std::env::var_os("PIERLINK_TEST_XKBCOMP")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let sibling = binary.parent().unwrap().join("xkbcomp");
            if sibling.is_file() {
                sibling
            } else {
                "/usr/bin/xkbcomp".into()
            }
        });
    assert!(
        compiler.is_file(),
        "需要前缀 xkbcomp 或 PIERLINK_TEST_XKBCOMP"
    );
    let prefix = root.path().join(".local/share/pierlink/vnc/test");
    fs::create_dir_all(prefix.join("usr/bin")).unwrap();
    fs::write(prefix.join(".pierlink-created"), "true").unwrap();
    // 真实前缀可能自带库和键盘数据，沿用这些只读资源。
    let original_usr = binary.parent().unwrap().parent().unwrap();
    for relative in ["lib", "share/X11/xkb"] {
        let original = original_usr.join(relative);
        if original.is_dir() {
            let destination = prefix.join("usr").join(relative);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            symlink(original, destination).unwrap();
        }
    }
    // 复制而不链接系统路径，避免 overlay 覆盖后形成符号链接环。
    fs::copy(compiler, prefix.join("usr/bin/xkbcomp")).unwrap();
    let binary_link = prefix.join("usr/bin/Xvnc");
    symlink(&binary, &binary_link).unwrap();
    let sessions = root.path().join("xsessions");
    fs::create_dir(&sessions).unwrap();
    let session = root.path().join("session");
    desktop_tests::script_file(
        &session,
        &format!(
            "#!/bin/sh\nreadlink /proc/self/ns/user > {user}\nreadlink /proc/self/ns/mnt > {mount}\nid -u > {uid}\nsleep 600\n",
            user = sh_quote(root.path().join("session-user").to_str().unwrap()).unwrap(),
            mount = sh_quote(root.path().join("session-mount").to_str().unwrap()).unwrap(),
            uid = sh_quote(root.path().join("session-uid").to_str().unwrap()).unwrap(),
        ),
    );
    fs::write(
        sessions.join("test.desktop"),
        format!(
            "[Desktop Entry]\nName=测试桌面\nExec={}\n",
            session.display()
        ),
    )
    .unwrap();
    cmd.env.extend([
        (
            "PIERLINK_VNC_XSESSIONS_DIR".into(),
            sessions.to_str().unwrap().into(),
        ),
        ("PIERLINK_TEST_XKB_NAMESPACE".into(), "1".into()),
        ("SHELL".into(), "/bin/sh".into()),
    ]);
    let xvnc = desktop::verify_binary(&program, &cmd, binary_link.to_str().unwrap()).unwrap();
    let created = create_desktop(
        &program,
        &cmd,
        CreateDesktop {
            desktop_id: "test".into(),
            width: 800,
            height: 600,
            xvnc,
        },
    )
    .unwrap();
    struct Cleanup<'a>(&'a SshProgram, &'a SshCommand, u16);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = stop_desktop(self.0, self.1, self.2);
        }
    }
    let _cleanup = Cleanup(&program, &cmd, created.display);
    let display = created.display;
    assert!((90..=99).contains(&display));
    let dir = root.path().join(".local/state/pierlink/vnc");
    let state_file = dir.join(format!("{display}.json"));
    let state: serde_json::Value = serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
    let pid = state["xvnc_pid"].as_u64().unwrap();
    let uid = unsafe { libc::getuid() };
    assert_eq!(fs::metadata(format!("/proc/{pid}")).unwrap().uid(), uid);
    let mapping = fs::read_to_string(format!("/proc/{pid}/uid_map")).unwrap();
    let mapping: Vec<u32> = mapping
        .split_whitespace()
        .map(|n| n.parse().unwrap())
        .collect();
    assert_eq!(mapping, [0, uid, 1]);
    assert!(matches!(
        fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap()
            .trim(),
        "Xvnc" | "Xtigervnc"
    ));
    let namespace = dir.join(format!("{display}.xkb"));
    for path in [
        &namespace,
        &namespace.join("upper"),
        &namespace.join("work"),
    ] {
        let metadata = fs::metadata(path).unwrap();
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }
    let xlock = PathBuf::from(format!("/tmp/.X{display}-lock"));
    let xsocket = PathBuf::from(format!("/tmp/.X11-unix/X{display}"));
    for path in [
        &state_file,
        &PathBuf::from(&created.socket),
        &dir.join(format!("{display}.xauth")),
        &xlock,
        &xsocket,
    ] {
        let metadata = fs::metadata(path).unwrap();
        assert_eq!(metadata.uid(), uid, "文件属主不符：{}", path.display());
        assert_eq!(
            metadata.permissions().mode() & 0o777,
            0o600,
            "文件权限不符：{}",
            path.display()
        );
    }
    let mut stream = UnixStream::connect(&created.socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut banner = [0; 12];
    stream.read_exact(&mut banner).unwrap();
    assert_eq!(&banner, b"RFB 003.008\n");
    stream.write_all(&banner).unwrap();
    let mut count = [0];
    stream.read_exact(&mut count).unwrap();
    let mut security = vec![0; usize::from(count[0])];
    stream.read_exact(&mut security).unwrap();
    assert!(security.contains(&1));
    stream.write_all(&[1]).unwrap();
    let mut result = [0; 4];
    stream.read_exact(&mut result).unwrap();
    assert_eq!(result, [0; 4]);
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::read_to_string(root.path().join("session-uid"))
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        assert!(Instant::now() < deadline, "桌面会话未就绪");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        fs::read_to_string(root.path().join("session-uid"))
            .unwrap()
            .trim(),
        uid.to_string()
    );
    for (name, file) in [("user", "session-user"), ("mnt", "session-mount")] {
        assert_eq!(
            fs::read_to_string(root.path().join(file)).unwrap().trim(),
            fs::read_link(format!("/proc/self/ns/{name}"))
                .unwrap()
                .to_str()
                .unwrap()
        );
        assert_ne!(
            fs::read_link(format!("/proc/{pid}/ns/{name}")).unwrap(),
            fs::read_link(format!("/proc/self/ns/{name}")).unwrap()
        );
    }
    // 检查状态标识和监听结果，检测只能查看本测试创建的进程。
    let script = detection::script().replace(
        "for proc in \"$proc_root\"/[0-9]*; do",
        &format!("for proc in /proc/{pid}; do"),
    );
    let detected =
        detection::parse_detection(desktop::run(&program, &cmd, &script, 20).unwrap()).unwrap();
    assert_eq!(detected.pierlink_desktops, vec![created.clone()]);
    assert!(detected.sessions[0].pierlink);
    assert!(detected.sessions[0].listens.is_empty());
    stop_desktop(&program, &cmd, display).unwrap();
    for path in [
        state_file,
        namespace,
        PathBuf::from(created.socket),
        dir.join(format!("{display}.xauth")),
        xlock,
        xsocket,
    ] {
        assert!(!path.exists(), "桌面残留未清理：{}", path.display());
    }
    eprintln!(
        "已验证：映射 root 的进程和文件在外部仍属当前用户，文件 0600、目录 0700，会话在原命名空间，停止后无残留"
    );
}
