use super::*;
use std::{
    fs, io,
    os::unix::fs::{PermissionsExt, symlink},
    sync::atomic::Ordering,
    time::Instant,
};

pub(super) fn server() -> SftpSession {
    super::sshd_tests::open(
        &SshProgram {
            path: "/usr/lib/openssh/sftp-server".into(),
        },
        &SshCommand {
            args: vec![],
            env: vec![],
            config_text: None,
            hooked: false,
            builtin: None,
            askpass: None,
        },
        Duration::from_secs(10),
    )
    .unwrap()
}
#[test]
fn 大量符号链接分批查询结果正确() {
    let root = crate::test_support::tempdir();
    let path = root.path().to_str().unwrap().to_owned();
    fs::create_dir(join_path(&path, "目标目录")).unwrap();
    fs::write(join_path(&path, "目标文件"), b"x").unwrap();
    // 远超一批的符号链接（一批 = 在途请求窗口）：一半指向目录、一半指向文件，再加几个坏链接
    for i in 0..150 {
        let target = if i % 2 == 0 {
            "目标目录"
        } else {
            "目标文件"
        };
        symlink(target, join_path(&path, &format!("链接{i:03}"))).unwrap();
    }
    for i in 0..5 {
        symlink("不存在", join_path(&path, &format!("坏{i}"))).unwrap();
    }
    let mut session = server();
    let listing = session.list_dir(&path, 1000).unwrap();
    assert_eq!(listing.entries.len(), 157);
    for i in 0..150 {
        let e = listing
            .entries
            .iter()
            .find(|e| e.name == format!("链接{i:03}"))
            .unwrap();
        assert_eq!(e.kind, EntryKind::Symlink);
        assert_eq!(e.target_is_dir, Some(i % 2 == 0), "{}", e.name);
    }
    assert!(
        listing
            .entries
            .iter()
            .filter(|e| e.name.starts_with('坏'))
            .all(|e| e.target_is_dir.is_none())
    );
    // 截断仍按条数算
    let short = session.list_dir(&path, 100).unwrap();
    assert_eq!(short.entries.len(), 100);
    assert!(short.truncated);
}
#[test]
fn 全部目录操作隐藏文件与链接() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("中文 ' $() 目录");
    let path = path.to_str().unwrap();
    let mut session = server();
    assert_eq!(
        session.realpath(".").unwrap(),
        std::env::current_dir().unwrap().to_str().unwrap()
    );
    assert!(session.stat(path).unwrap().is_none());
    session.mkdir(path).unwrap();
    let hidden = join_path(path, ".隐藏");
    fs::write(&hidden, b"abc").unwrap();
    fs::create_dir(join_path(path, "空目录")).unwrap();
    symlink("空目录", join_path(path, "目录链接")).unwrap();
    symlink(".隐藏", join_path(path, "文件链接")).unwrap();
    symlink("没有", join_path(path, "坏链接")).unwrap();
    let listing = session.list_dir(path, 20).unwrap();
    assert_eq!(listing.entries.len(), 5);
    assert!(!listing.truncated);
    assert!(
        listing
            .entries
            .iter()
            .any(|e| e.name == ".隐藏" && e.kind == EntryKind::File && e.size == 3)
    );
    for (name, target) in [
        ("目录链接", Some(true)),
        ("文件链接", Some(false)),
        ("坏链接", None),
    ] {
        let entry = listing.entries.iter().find(|e| e.name == name).unwrap();
        assert_eq!(entry.kind, EntryKind::Symlink);
        assert_eq!(entry.target_is_dir, target);
    }
    let renamed = join_path(path, "改名");
    session.rename(&hidden, &renamed).unwrap();
    session.set_mtime(&renamed, 1_234_567_890).unwrap();
    assert_eq!(
        session.stat(&renamed).unwrap().unwrap().mtime,
        1_234_567_890
    );
    fs::write(&hidden, b"replacement").unwrap();
    assert!(session.posix_rename);
    session.rename_file(&hidden, &renamed, true).unwrap();
    assert_eq!(fs::read(&renamed).unwrap(), b"replacement");
    // 不宣告扩展的服务器走 v3 RENAME。
    session.posix_rename = false;
    session.rename(&renamed, &hidden).unwrap();
    session.remove_file(&hidden).unwrap();
    assert!(session.stat(&hidden).unwrap().is_none());
    assert_eq!(
        session.remove_file(&hidden).unwrap_err().code,
        "sftp.not_found"
    );
    // OpenSSH realpath 允许最后一段不存在；中间目录不存在才返回未找到。
    assert_eq!(session.realpath(&hidden).unwrap(), hidden);
    assert_eq!(
        session
            .realpath(&join_path(&hidden, "child"))
            .unwrap_err()
            .code,
        "sftp.not_found"
    );
    assert_eq!(
        session.set_mtime(path, -1).unwrap_err().code,
        "sftp.protocol"
    );
    assert_eq!(session.stat("a\0b").unwrap_err().code, "sftp.protocol");
    let denied = join_path(path, "无权");
    fs::create_dir(&denied).unwrap();
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o0)).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert_eq!(
            session.list_dir(&denied, 1).unwrap_err().code,
            "sftp.permission_denied"
        );
    }
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
}
#[test]
fn f4_五千项截断及非utf8计数() {
    use std::os::unix::ffi::OsStringExt;
    let root = crate::test_support::tempdir();
    let mut session = server();
    assert!(
        !session
            .list_dir(root.path().to_str().unwrap(), 0)
            .unwrap()
            .truncated
    );
    for i in 0..5003 {
        fs::write(root.path().join(format!("文件{i}")), b"").unwrap();
    }
    let listing = session
        .list_dir(root.path().to_str().unwrap(), 5000)
        .unwrap();
    assert_eq!(listing.entries.len(), 5000);
    assert!(listing.truncated);
    let listing = session
        .list_dir(root.path().to_str().unwrap(), 5003)
        .unwrap();
    assert_eq!(listing.entries.len(), 5003);
    assert!(!listing.truncated);
    let invalid = root.path().join("非utf8");
    fs::create_dir(&invalid).unwrap();
    fs::write(invalid.join(std::ffi::OsString::from_vec(vec![0xff])), b"").unwrap();
    let listing = session.list_dir(invalid.to_str().unwrap(), 10).unwrap();
    assert!(listing.entries.is_empty());
    assert_eq!(listing.skipped_non_utf8, 1);
}
#[test]
fn 大文件内容进度取消及句柄关闭() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("大文件");
    let path = path.to_str().unwrap();
    let mut session = server();
    let bytes: Vec<_> = (0..5_123_457).map(|i| (i % 251) as u8).collect();
    let cancel = AtomicBool::new(false);
    let mut counts = vec![];
    assert_eq!(
        session
            .write_from(
                path,
                &mut &bytes[..],
                Some(0o640),
                &mut |n| counts.push(n),
                &cancel
            )
            .unwrap(),
        bytes.len() as u64
    );
    assert_eq!(*counts.last().unwrap(), bytes.len() as u64);
    assert_eq!(session.transport.max_pending, 8);
    assert!(
        counts
            .windows(2)
            .all(|w| w[0] < w[1] && w[1] - w[0] <= 32768)
    );
    assert_eq!(session.stat(path).unwrap().unwrap().mode & 0o777, 0o640);
    session.transport.max_pending = 0;
    let mut output = vec![];
    assert_eq!(
        session
            .read_to(path, &mut output, &mut |_| {}, &cancel)
            .unwrap(),
        bytes.len() as u64
    );
    assert_eq!(output, bytes);
    assert_eq!(session.transport.max_pending, 8);
    let mut output = vec![];
    assert_eq!(
        session
            .read_to(
                path,
                &mut output,
                &mut |_| cancel.store(true, Ordering::Relaxed),
                &cancel
            )
            .unwrap_err()
            .code,
        "sftp.cancelled"
    );
    assert_eq!(output.len(), 32768);
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        session
            .write_from(
                path,
                &mut &bytes[..],
                None,
                &mut |_| cancel.store(true, Ordering::Relaxed),
                &cancel
            )
            .unwrap_err()
            .code,
        "sftp.cancelled"
    );
    assert!(session.stat(path).unwrap().is_some());
    assert_eq!(
        session
            .read_to(path, &mut vec![], &mut |_| {}, &cancel)
            .unwrap_err()
            .code,
        "sftp.cancelled"
    );
    cancel.store(false, Ordering::Relaxed);
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::Other.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::Other.into())
        }
    }
    assert_eq!(
        session
            .read_to(path, &mut Broken, &mut |_| {}, &cancel)
            .unwrap_err()
            .code,
        "sftp.failure"
    );
    assert_eq!(
        session
            .write_from(path, &mut Broken, None, &mut |_| {}, &cancel)
            .unwrap_err()
            .code,
        "sftp.failure"
    );
    assert_eq!(
        session
            .write_from(path, &mut &b""[..], None, &mut |_| {}, &cancel)
            .unwrap(),
        0
    );
    assert_eq!(
        session
            .read_to(path, &mut vec![], &mut |_| {}, &cancel)
            .unwrap(),
        0
    );
}
#[test]
fn 服务器中途退出及超时均不挂起() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("大文件");
    fs::write(&path, vec![1; 5_000_000]).unwrap();
    let mut session = server();
    let disconnect = session.transport.disconnect();
    let error = session
        .read_to(
            path.to_str().unwrap(),
            &mut vec![],
            &mut |_| disconnect(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert_eq!(error.code, "sftp.connection_lost");
    assert_eq!(session.stat(".").unwrap_err().code, "sftp.connection_lost");
    let start = Instant::now();
    let result = SftpSession::open(
        &SshProgram {
            path: "/usr/bin/sleep".into(),
        },
        &SshCommand {
            args: vec!["30".into()],
            env: vec![],
            config_text: None,
            hooked: false,
            builtin: None,
            askpass: None,
        },
        Duration::from_millis(50),
    );
    assert_eq!(result.err().unwrap().code, "sftp.connection_lost");
    assert!(start.elapsed() < Duration::from_secs(3));
}
#[test]
fn 不可信协议包与状态映射() {
    for (code, expected) in [
        (2, "sftp.not_found"),
        (3, "sftp.permission_denied"),
        (4, "sftp.failure"),
        (5, "sftp.protocol"),
        (6, "sftp.connection_lost"),
        (7, "sftp.connection_lost"),
    ] {
        assert_eq!(status_error(code).code, expected);
    }
    assert!(Decoder(&[0, 0, 0, 20]).string().is_err());
    assert!(Decoder(&[0, 0, 0, 16]).attrs().is_err());
    for (reply, code) in [
        ("b'\\x00\\x10\\x00\\x01'", "sftp.protocol"),
        (
            "b'\\x00\\x00\\x00\\x05\\x02\\x00\\x00\\x00\\x04'",
            "sftp.protocol",
        ),
        ("b'\\x00\\x00\\x00\\x05\\x02'", "sftp.connection_lost"),
    ] {
        let script = format!(
            "import sys\nsys.stdin.buffer.read(9)\nsys.stdout.buffer.write({reply})\nsys.stdout.buffer.flush()\n"
        );
        let result = super::sshd_tests::open(
            &SshProgram {
                path: "/usr/bin/python3".into(),
            },
            &SshCommand {
                args: vec!["-c".into(), script],
                env: vec![],
                config_text: None,
                hooked: false,
                builtin: None,
                askpass: None,
            },
            Duration::from_secs(2),
        );
        assert_eq!(result.err().unwrap().code, code);
    }
    let script = "import sys,struct\nr=sys.stdin.buffer;w=sys.stdout.buffer\nr.read(9);w.write(struct.pack('>IBI',5,2,3));w.flush()\nn=struct.unpack('>I',r.read(4))[0];r.read(n)\nw.write(struct.pack('>IBI',5,101,999));w.flush()";
    let mut session = super::sshd_tests::open(
        &SshProgram {
            path: "/usr/bin/python3".into(),
        },
        &SshCommand {
            args: vec!["-c".into(), script.into()],
            env: vec![],
            config_text: None,
            hooked: false,
            builtin: None,
            askpass: None,
        },
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(session.stat(".").unwrap_err().code, "sftp.protocol");
    assert_eq!(session.stat(".").unwrap_err().code, "sftp.connection_lost");
}

#[cfg(unix)]
#[test]
fn 握手失败按ssh错误输出归类() {
    use crate::{process::SshProgram, ssh::args::SshCommand};
    let open = |script: &str| {
        SftpSession::open(
            &SshProgram {
                path: "/bin/sh".into(),
            },
            &SshCommand {
                args: vec!["-c".into(), script.into()],
                env: vec![],
                config_text: None,
                hooked: false,
                builtin: None,
                askpass: None,
            },
            Duration::from_secs(5),
        )
        .err()
        .map(|e| e.code)
    };
    assert_eq!(
        open("echo 'dev@h: Permission denied (publickey,password).' >&2; exit 255").as_deref(),
        Some("ssh.auth_failed")
    );
    assert_eq!(
        open("echo 'subsystem request failed on channel 0' >&2; exit 255").as_deref(),
        Some("sftp.unavailable")
    );
    assert_eq!(
        open("echo 'ssh: connect to host h port 22: Connection refused' >&2; exit 255").as_deref(),
        Some("ssh.unreachable")
    );
    // 没有任何错误输出：仍是连接断开
    assert_eq!(open("exit 1").as_deref(), Some("sftp.connection_lost"));
}
