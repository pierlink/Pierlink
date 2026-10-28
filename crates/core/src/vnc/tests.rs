use super::*;
use crate::ssh::args::SshCommand;
use std::{
    io::{self, Read, Write},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

#[test]
fn 地址与套接字按内核格式解析() {
    let row = |addr: &str, state: &str| {
        format!("0: {addr}:170D 00000000:0000 {state} 0:0 00:0 0 1000 0 123")
    };
    for (address, expected, scope) in [
        ("0100007F", "127.0.0.1", ListenScope::Loopback),
        ("00000000", "0.0.0.0", ListenScope::All),
        ("0101A8C0", "192.168.1.1", ListenScope::Specific),
    ] {
        let values = parse::tcp(&row(address, "0A"), false);
        assert_eq!(
            values,
            vec![(
                "123".into(),
                Listen {
                    addr: expected.into(),
                    port: 5901,
                    scope
                }
            )]
        );
        assert!(parse::tcp(&row(address, "01"), false).is_empty());
    }
    for (hex, address, scope) in [
        (
            "00000000000000000000000001000000",
            "::1",
            ListenScope::Loopback,
        ),
        ("00000000000000000000000000000000", "::", ListenScope::All),
    ] {
        assert_eq!(
            parse::tcp(&row(hex, "0A"), true)[0].1,
            Listen {
                addr: address.into(),
                port: 5901,
                scope
            }
        );
    }
    assert!(parse::tcp(&row("错误", "0A"), true).is_empty());
    assert!(parse::tcp("字段缺失", false).is_empty());
    assert_eq!(
        parse::unix(
            "Num RefCount Protocol Flags Type St Inode Path\n0: 2 0 10000 1 01 123 /目录/含 空格.sock\n0: 2 0 0 1 03 456"
        ),
        vec![("123".into(), "/目录/含 空格.sock".into())]
    );
}

#[test]
fn 命令行空格与启动来源不会串字段() {
    assert_eq!(parse::cmdline(b"Xtigervnc\0-desktop\0user machine:90 (desktop)\0:90\0-SecurityTypes\0TLSVnc,VncAuth\0"), (Some(90),Some("TLSVnc,VncAuth".into())));
    assert_eq!(
        parse::cmdline(b"Xvnc\0:91\0-SecurityTypes=None\0"),
        (Some(91), Some("None".into()))
    );
    assert_eq!(parse::cmdline(b"Xvnc\0:bad\0"), (None, None));
    for (text, result) in [
        (
            "0::/system.slice/tigervnc-claude.service",
            StartedBy::SystemUnit("tigervnc-claude.service".into()),
        ),
        (
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/vnc.service",
            StartedBy::UserUnit("vnc.service".into()),
        ),
        (
            "0::/user.slice/user-1000.slice/session-5.scope",
            StartedBy::UserSession,
        ),
        (
            "0::/user.slice/user-1000.slice/user@1000.service",
            StartedBy::Unknown,
        ),
        ("无效", StartedBy::Unknown),
    ] {
        assert_eq!(parse::cgroup(text), result);
    }
}

#[test]
fn 桌面列表使用默认名称并排除隐藏和缺少程序的条目() {
    let text = "[Desktop Entry]\nName=XFCE\nName[zh_CN]=中文\nExec=startxfce4 --flag\nDesktopNames=XFCE;\n[Desktop Action ignored]\nName=忽略\n";
    let entry = parse::desktop("xfce", text, true).unwrap();
    assert_eq!(entry.name, "XFCE");
    assert_eq!(entry.exec, "startxfce4 --flag");
    assert_eq!(entry.desktop_names, vec!["XFCE"]);
    assert!(!entry.heavy);
    for flag in ["Hidden=true", "NoDisplay=true"] {
        assert!(
            parse::desktop("xfce", &format!("[Desktop Entry]\nExec=x\n{flag}\n"), true).is_none()
        );
    }
    assert!(parse::desktop("xfce", text, false).is_none());
    assert!(parse::desktop("xfce", "[Desktop Entry]\nName=空", true).is_none());
    assert!(parse::desktop("gnome", text, true).unwrap().heavy);
    let ubuntu = parse::desktop("ubuntu", "[Desktop Entry]\nName=Ubuntu\nExec=env GNOME_SHELL_SESSION_MODE=ubuntu gnome-session\nDesktopNames=ubuntu:GNOME;", true).unwrap();
    assert!(ubuntu.heavy);
    assert_eq!(parse::desktop_order(&ubuntu), 100);
    let mut ids = [
        "gnome", "plasma", "budgie", "cinnamon", "mate", "custom", "xfce", "lxde", "lxqt", "icewm",
        "openbox",
    ];
    ids.sort_by_key(|id| parse::desktop_weight(id));
    assert_eq!(
        ids,
        [
            "openbox", "icewm", "lxqt", "lxde", "xfce", "custom", "mate", "cinnamon", "budgie",
            "plasma", "gnome"
        ]
    );
}

#[test]
fn 版本发行版与官方包前提() {
    for (text, version) in [
        ("OpenSSH_8.9p1 Ubuntu-3ubuntu0.10, OpenSSL …", (8, 9)),
        ("OpenSSH_for_Windows_9.5p1, LibreSSL 3.8.2", (9, 5)),
        ("OpenSSH_10.0p2", (10, 0)),
    ] {
        assert_eq!(parse::ssh_version(text), Some(version));
    }
    assert_eq!(parse::ssh_version("其他 SSH"), None);
    assert_eq!(parse::version("Xvnc TigerVNC 1.16.2"), Some((1, 16, 2)));
    assert_eq!(parse::version("glibc 2.28"), Some((2, 28, 0)));
    assert_eq!(parse::version("未知"), None);
    assert_eq!(parse::flavor("Xvnc", "TurboVNC 3.1"), Flavor::TurboVnc);
    assert_eq!(parse::flavor("Xvnc", "TigerVNC 1.12.0"), Flavor::TigerVnc);
    assert_eq!(parse::flavor("x11vnc", ""), Flavor::X11vnc);
    assert_eq!(parse::flavor("vncserver-x11-core", ""), Flavor::RealVnc);
    assert_eq!(parse::flavor("wayvnc", ""), Flavor::Other);
    let d = parse::distro("ID=rocky\nVERSION_ID=\"9\"\nID_LIKE=\"rhel centos fedora\"\n");
    assert_eq!(d.id, "rocky");
    assert_eq!(d.version_id, "9");
    assert_eq!(d.id_like, vec!["rhel", "centos", "fedora"]);
    let tools = Tools {
        xkbcomp: true,
        xkb_rules: true,
        ..Default::default()
    };
    let libs = "libxcvt.so.0\nlibgbm.so.1\nlibxshmfence.so.1\nlibX11.so.6\nlibpam.so.0\nlibsystemd.so.0\nlibstdc++.so.6";
    assert!(parse::official("x86_64", Some("glibc 2.28"), libs, &tools).ok);
    let missing = parse::official("aarch64", Some("glibc 2.27"), "", &Tools::default());
    assert!(!missing.ok);
    assert_eq!(missing.missing.len(), 11);
}

#[test]
fn 安装命令覆盖轻桌面及企业发行版() {
    let ubuntu = Distro {
        id: "ubuntu".into(),
        ..Default::default()
    };
    assert_eq!(
        install_command(&ubuntu, PackageManager::Apt, false).unwrap(),
        "sudo apt-get install -y tigervnc-standalone-server xfce4 dbus-x11"
    );
    assert_eq!(
        install_command(&ubuntu, PackageManager::Apt, true).unwrap(),
        "sudo apt-get install -y tigervnc-standalone-server openbox xterm dbus-x11"
    );
    for id in ["rhel", "rocky", "almalinux"] {
        assert_eq!(
            install_command(
                &Distro {
                    id: id.into(),
                    ..Default::default()
                },
                PackageManager::Dnf,
                false
            )
            .unwrap(),
            "sudo dnf install -y epel-release && sudo dnf install -y tigervnc-server @xfce-desktop-environment"
        );
    }
    assert_eq!(
        install_command(&Distro::default(), PackageManager::Dnf, true).unwrap(),
        "sudo dnf install -y tigervnc-server openbox xterm"
    );
    assert_eq!(install_command(&ubuntu, PackageManager::None, false), None);
}

pub(super) struct MemoryBackend(mpsc::Sender<Vec<u8>>);
impl stream::Backend for MemoryBackend {
    fn close(&mut self) {
        let _ = self.0.send(Vec::new());
    }
}
struct MemoryRead {
    receiver: mpsc::Receiver<Vec<u8>>,
    pending: io::Cursor<Vec<u8>>,
}
impl Read for MemoryRead {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.pending.read(out)?;
            if n > 0 {
                return Ok(n);
            }
            match self.receiver.recv() {
                Ok(bytes) if !bytes.is_empty() => self.pending = io::Cursor::new(bytes),
                _ => return Ok(0),
            }
        }
    }
}
struct MemoryWrite(Arc<Mutex<Vec<u8>>>);
impl Write for MemoryWrite {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn memory(bytes: Vec<u8>) -> (VncStream, Arc<Mutex<Vec<u8>>>) {
    let (sender, receiver) = mpsc::channel();
    if !bytes.is_empty() {
        sender.send(bytes).unwrap();
    }
    let written = Arc::new(Mutex::new(Vec::new()));
    let stream = VncStream::from_io(
        MemoryBackend(sender),
        MemoryWrite(written.clone()),
        MemoryRead {
            receiver,
            pending: io::Cursor::new(Vec::new()),
        },
    )
    .unwrap();
    (stream, written)
}
fn probe_bytes(types: &[u8], subtypes: &[u32]) -> Vec<u8> {
    let mut bytes = b"RFB 003.008\n".to_vec();
    bytes.push(types.len() as u8);
    bytes.extend(types);
    if types.contains(&19) {
        bytes.extend([0, 2, 0, subtypes.len() as u8]);
        for t in subtypes {
            bytes.extend(t.to_be_bytes());
        }
    }
    bytes
}

#[test]
fn 握手只探测类型且不认证或初始化客户端() {
    for (types, subtypes, no_auth) in [
        (vec![19, 2], vec![258, 2], false),
        (vec![1], vec![], true),
        (vec![19], vec![1], true),
        (vec![19], vec![256], false),
    ] {
        let (stream, written) = memory(probe_bytes(&types, &subtypes));
        let probe = probe_rfb(stream, Duration::from_secs(1)).unwrap();
        assert_eq!(probe.no_auth, no_auth);
        assert!(probe.novnc_supported);
        assert_eq!(
            probe.security_types,
            types.iter().map(|b| u32::from(*b)).collect::<Vec<_>>()
        );
        assert_eq!(probe.vencrypt_subtypes, subtypes);
        let out = written.lock().unwrap();
        // 不发送认证、子类型选择或 ClientInit。
        let expected = if types.contains(&19) {
            b"RFB 003.008\n\x13\0\x02".as_slice()
        } else {
            b"RFB 003.008\n".as_slice()
        };
        assert_eq!(out.as_slice(), expected);
    }
    let mut legacy = b"RFB 003.003\n".to_vec();
    legacy.extend(1u32.to_be_bytes());
    assert!(
        probe_rfb(memory(legacy).0, Duration::from_secs(1))
            .unwrap()
            .no_auth
    );
}

#[test]
fn 握手拒绝仅_tls_非协议失败原因和总超时() {
    let error =
        probe_rfb(memory(probe_bytes(&[19], &[258])).0, Duration::from_secs(1)).unwrap_err();
    assert_eq!(error.code, "vnc.tls_only");
    assert!(error.params["types"].contains("258"));
    let error = probe_rfb(
        memory(b"HTTP/1.1\x1bxxx".to_vec()).0,
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert_eq!(error.code, "vnc.not_rfb");
    assert!(!error.params["banner"].contains('\x1b'));
    let mut refused = b"RFB 003.008\n\0".to_vec();
    let reason = b"password=private\nServer busy";
    refused.extend((reason.len() as u32).to_be_bytes());
    refused.extend(reason);
    let error = probe_rfb(memory(refused).0, Duration::from_secs(1)).unwrap_err();
    assert_eq!(error.code, "vnc.server_refused");
    assert!(!error.params["reason"].contains("private"));
    for bytes in [vec![], b"RFB 003.008\n\x02\x13".to_vec()] {
        let before = Instant::now();
        assert_eq!(
            probe_rfb(memory(bytes).0, Duration::from_millis(30))
                .unwrap_err()
                .code,
            "vnc.probe_timeout"
        );
        assert!(before.elapsed() < Duration::from_secs(1));
    }
    assert_eq!(
        probe_rfb(memory(vec![]).0, Duration::ZERO)
            .unwrap_err()
            .code,
        "vnc.probe_timeout"
    );
}

#[test]
fn 拆分读写与关闭会唤醒阻塞读取() {
    let (stream, written) = memory(vec![1, 2, 3]);
    let duplicate = stream.closer();
    let (mut reader, mut writer, closer) = stream.split();
    let mut buffer = [0; 65536];
    assert_eq!(reader.read(&mut buffer).unwrap(), 3);
    assert_eq!(&buffer[..3], &[1, 2, 3]);
    let read = thread::spawn(move || reader.read(&mut buffer));
    writer.write(&[4, 5, 6]).unwrap();
    writer.flush().unwrap();
    assert_eq!(*written.lock().unwrap(), vec![4, 5, 6]);
    closer.close();
    duplicate.close();
    assert_eq!(read.join().unwrap().unwrap_err().code, "vnc.stream_closed");
    assert_eq!(writer.write(&[7]).unwrap_err().code, "vnc.stream_closed");
    let (mut stream, _) = memory(vec![]);
    assert_eq!(stream.reader.read(&mut []).unwrap(), 0);
    stream.close();
    stream.close();
}

fn background() -> SshCommand {
    SshCommand {
        args: vec!["-T".into(), "host".into(), "/bin/sh -s".into()],
        env: vec![],
        config_text: None,
        builtin: None,
        askpass: None,
    }
}

#[test]
fn 转发与中继参数限制目标且统一转义() {
    let path = "/目录/空格'\"$(id),sock";
    let target = VncTarget::UnixPath(path.into());
    let cmd = transport::forward_command(&background(), &target).unwrap();
    assert_eq!(&cmd.args[..2], &["-W", path]);
    assert_eq!(cmd.args.last().unwrap(), "host");
    let cmd = transport::forward_command(&background(), &VncTarget::Loopback(5990)).unwrap();
    assert_eq!(cmd.args[1], "127.0.0.1:5990");
    assert_eq!(
        relay::command(&Tools::default(), &target).unwrap_err().code,
        "vnc.relay_unavailable"
    );
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
            platform_python: true,
            ..Default::default()
        },
        Tools {
            perl: true,
            ..Default::default()
        },
    ] {
        let command = relay::command(&tools, &target).unwrap();
        assert!(command.starts_with("exec '"));
        assert!(command.contains("'\"'\"'"));
    }
    let program = crate::process::SshProgram {
        path: "不存在".into(),
    };
    for target in [
        VncTarget::Loopback(0),
        VncTarget::UnixPath("relative".into()),
        VncTarget::UnixPath("/x\0".into()),
        VncTarget::UnixPath(format!("/{}", "x".repeat(107))),
    ] {
        assert_eq!(
            open_stream(&program, &background(), &Tools::default(), target)
                .err()
                .unwrap()
                .code,
            "vnc.invalid_target"
        );
    }
    assert_eq!(
        detection::parse_detection(serde_json::json!({}))
            .unwrap_err()
            .code,
        "vnc.invalid_detection"
    );
}

#[cfg(unix)]
#[test]
fn 只读脚本仅收集本人会话且不输出其他参数() {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    let root = crate::test_support::tempdir();
    let p = root.path();
    for dir in [
        "proc/net",
        "proc/900/fd",
        "proc/901/fd",
        "proc/902/fd",
        "xsessions",
        "state",
    ] {
        fs::create_dir_all(p.join(dir)).unwrap();
    }
    for pid in [900, 901, 902] {
        let uid = if pid == 902 {
            (unsafe { libc::getuid() }) + 1
        } else {
            unsafe { libc::getuid() }
        };
        fs::write(
            p.join(format!("proc/{pid}/status")),
            format!("Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
        fs::write(p.join(format!("proc/{pid}/comm")), "Xtigervnc\n").unwrap();
        fs::write(p.join(format!("proc/{pid}/cmdline")),format!("Xtigervnc\0-desktop\0含 空格 :bad\0:{}\0-SecurityTypes\0None\0-password\0绝不输出\0",pid-810)).unwrap();
        fs::write(
            p.join(format!("proc/{pid}/cgroup")),
            "0::/system.slice/vnc.service",
        )
        .unwrap();
        symlink(
            format!("socket:[{pid}]"),
            p.join(format!("proc/{pid}/fd/4")),
        )
        .unwrap();
    }
    fs::write(p.join("proc/net/tcp"),"0: 00000000:1766 0:0 0A 0:0 0:0 0 1000 0 900\n0: 0100007F:1767 0:0 0A 0:0 0:0 0 1000 0 902\n").unwrap();
    fs::write(p.join("proc/net/tcp6"), "").unwrap();
    fs::write(
        p.join("proc/net/unix"),
        "0: 2 0 10000 1 01 900 /tmp/.X11-unix/X90\n0: 2 0 10000 1 01 900 @/tmp/.X11-unix/X90\n0: 2 0 10000 1 01 901 /测试 空格/s\n0: 2 0 10000 1 01 901 @/tmp/.X11-unix/X91\n",
    )
    .unwrap();

    fs::write(p.join("os-release"), "ID=ubuntu\nVERSION_ID=\"22.04\"\n").unwrap();
    for (id, flag) in [
        ("gnome", ""),
        ("xfce", ""),
        ("hidden", "Hidden=true"),
        ("nodisplay", "NoDisplay=true"),
        ("missing", ""),
    ] {
        fs::write(
            p.join(format!("xsessions/{id}.desktop")),
            format!(
                "[Desktop Entry]\nName={id}\nName[zh_CN]=不取此名\nExec={}\n{flag}\n",
                if id == "missing" {
                    "/不存在"
                } else {
                    "/bin/true"
                }
            ),
        )
        .unwrap();
    }
    let replacements = [
        (
            "proc_root=/proc",
            format!(
                "proc_root={}",
                crate::ssh::quote::sh_quote(p.join("proc").to_str().unwrap()).unwrap()
            ),
        ),
        (
            "xsessions=${PIERLINK_VNC_XSESSIONS_DIR:-/usr/share/xsessions}",
            format!(
                "xsessions={}",
                crate::ssh::quote::sh_quote(p.join("xsessions").to_str().unwrap()).unwrap()
            ),
        ),
        (
            "os_release=/etc/os-release",
            format!(
                "os_release={}",
                crate::ssh::quote::sh_quote(p.join("os-release").to_str().unwrap()).unwrap()
            ),
        ),
        (
            "state_dir=$HOME/.local/state/pierlink/vnc",
            format!(
                "state_dir={}",
                crate::ssh::quote::sh_quote(p.join("state").to_str().unwrap()).unwrap()
            ),
        ),
    ];
    let runner = p.join("runner");
    let mut python =
        String::from("#!/usr/bin/python3\nimport subprocess,sys\ns=sys.stdin.read()\n");
    for (from, to) in replacements {
        python.push_str(&format!(
            "s=s.replace({}, {})\n",
            serde_json::to_string(from).unwrap(),
            serde_json::to_string(&to).unwrap()
        ));
    }
    python.push_str("sys.exit(subprocess.run(['/bin/sh'],input=s.encode()).returncode)\n");
    fs::write(&runner, python).unwrap();
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
    let program = crate::process::SshProgram { path: runner };
    let mut cmd = background();
    cmd.args = vec![];
    let result = detect(&program, &cmd).unwrap();
    assert_eq!(result.sessions.len(), 2);
    assert_eq!(result.sessions[0].display, Some(90));
    assert!(!result.sessions[0].pierlink);
    assert_eq!(result.sessions[0].listens[0].scope, ListenScope::All);
    // X 服务器自己的 X11 套接字不算 VNC 入口：只开 TCP 的桌面要走 TCP
    assert!(result.sessions[0].unix_paths.is_empty());
    assert_eq!(result.sessions[1].unix_paths, vec!["/测试 空格/s"]);
    assert!(result.sessions[1].listens.is_empty());
    assert!(!result.sessions[1].pierlink);
    assert_eq!(
        result
            .desktops
            .iter()
            .map(|d| d.id.as_str())
            .collect::<Vec<_>>(),
        vec!["xfce", "gnome"]
    );
    assert!(!serde_json::to_string(&result).unwrap().contains("绝不输出"));
    assert_eq!(result.distro.id, "ubuntu");
}

#[cfg(unix)]
#[test]
fn 系统_ssh_按版本分流并在转发被禁时回退且保留横幅() {
    use std::{fs, os::unix::fs::PermissionsExt};
    for (version, prohibited, expect_forward) in [
        ("8.9", false, false),
        ("9.5", false, true),
        ("10.0", true, true),
    ] {
        let root = crate::test_support::tempdir();
        let runner = root.path().join("ssh");
        let log = root.path().join("calls");
        let source = format!(
            r#"#!/usr/bin/python3
import sys,json
with open({},'a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
if sys.argv[1:]==['-V']:
 sys.stderr.write('OpenSSH_{}p1\n')
 sys.exit(0)
if {} and '-W' in sys.argv:
 sys.stderr.write('channel 0: open failed: administratively prohibited\n')
 sys.exit(255)
sys.stdout.buffer.write(b'RFB 003.008\n');sys.stdout.buffer.flush()
while True:
 data=sys.stdin.buffer.read1(65536)
 if not data: break
 sys.stdout.buffer.write(data);sys.stdout.buffer.flush()
"#,
            serde_json::to_string(log.to_str().unwrap()).unwrap(),
            version,
            if prohibited { "True" } else { "False" }
        );
        fs::write(&runner, source).unwrap();
        fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
        let program = crate::process::SshProgram { path: runner };
        let mut stream = open_stream(
            &program,
            &background(),
            &Tools {
                python3: true,
                ..Default::default()
            },
            VncTarget::UnixPath("/test/sock".into()),
        )
        .unwrap();
        let mut bytes = [0; 12];
        let mut offset = 0;
        let deadline = Instant::now() + Duration::from_secs(2);
        while offset < bytes.len() {
            offset += stream
                .reader
                .read_until(&mut bytes[offset..], Some(deadline))
                .unwrap();
        }
        assert_eq!(&bytes, b"RFB 003.008\n");
        stream.writer.write(&[42]).unwrap();
        stream.writer.flush().unwrap();
        assert_eq!(
            stream
                .reader
                .read_until(&mut bytes, Some(deadline))
                .unwrap(),
            1
        );
        assert_eq!(bytes[0], 42);
        stream.close();
        let calls: Vec<Vec<String>> = fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(calls[0], vec!["-V"]);
        assert_eq!(calls[1].iter().any(|s| s == "-W"), expect_forward);
        if prohibited {
            assert_eq!(calls.len(), 3);
            assert!(calls[2].last().unwrap().contains("python3"));
        }
    }
}

#[test]
fn 关闭唤醒写满队列的发送线程() {
    use std::sync::Condvar;
    struct Gate(Arc<(Mutex<bool>, Condvar)>);
    impl Write for Gate {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            let (lock, wake) = &*self.0;
            let mut closed = lock.lock().unwrap();
            while !*closed {
                closed = wake.wait(closed).unwrap();
            }
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "测试流已关闭"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    struct Cancel {
        gate: Arc<(Mutex<bool>, Condvar)>,
        output: mpsc::Sender<Vec<u8>>,
    }
    impl stream::Backend for Cancel {
        fn close(&mut self) {
            *self.gate.0.lock().unwrap() = true;
            self.gate.1.notify_all();
            let _ = self.output.send(vec![]);
        }
    }
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (tx, rx) = mpsc::channel();
    let stream = VncStream::from_io(
        Cancel {
            gate: gate.clone(),
            output: tx,
        },
        Gate(gate),
        MemoryRead {
            receiver: rx,
            pending: io::Cursor::new(vec![]),
        },
    )
    .unwrap();
    let (reader, mut writer, closer) = stream.split();
    let task = thread::spawn(move || writer.write(&vec![0; 1024 * 1024]));
    thread::sleep(Duration::from_millis(30));
    assert!(!task.is_finished());
    let start = Instant::now();
    closer.close();
    assert_eq!(task.join().unwrap().unwrap_err().code, "vnc.stream_closed");
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(reader);
}
