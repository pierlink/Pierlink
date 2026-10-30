use super::*;
#[test]
fn f12_同秒等长内容变化拒绝覆盖并返回含bom原字节哈希() {
    let root = crate::test_support::tempdir();
    let file = root.path().join("文本");
    let path = file.to_str().unwrap();
    let mut session = crate::sftp::tests::server();
    fs::write(path, b"\xef\xbb\xbfaaa").unwrap();
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    let loaded = session.read_text(path, 100).unwrap();
    assert_eq!(
        loaded.hash.as_deref(),
        Some(text::hash(b"\xef\xbb\xbfaaa").as_str())
    );
    let expected = FileVersion {
        size: loaded.size,
        mtime: loaded.mtime,
        hash: loaded.hash,
    };
    fs::write(path, b"\xef\xbb\xbfbbb").unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert_eq!(
        session
            .write_text(path, "mine", true, Some(expected))
            .unwrap_err()
            .code,
        "files.changed"
    );
    assert_eq!(fs::read(path).unwrap(), b"\xef\xbb\xbfbbb");
    let written = session.write_text(path, "mine", true, None).unwrap();
    assert_eq!(written.hash, session.read_text(path, 100).unwrap().hash);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
#[test]
fn 排他创建重命名和权限校验() {
    let root = crate::test_support::tempdir();
    let a = root.path().join("甲").to_str().unwrap().to_owned();
    let b = root.path().join("乙").to_str().unwrap().to_owned();
    let dir = root.path().join("目录").to_str().unwrap().to_owned();
    let mut session = crate::sftp::tests::server();
    session.create_file(&a).unwrap();
    session.create_file(&b).unwrap();
    session.create_dir(&dir).unwrap();
    assert_eq!(session.create_file(&a).unwrap_err().code, "sftp.exists");
    assert_eq!(session.create_dir(&dir).unwrap_err().code, "sftp.exists");
    let e = session.rename(&a, &b).unwrap_err();
    assert_eq!(e.code, "sftp.exists");
    assert_eq!(e.params["path"], b);
    session.remove_file(&b).unwrap();
    session.rename(&a, &b).unwrap();
    session.chmod(&b, 0o2751).unwrap();
    assert_eq!(session.stat(&b).unwrap().unwrap().mode & 0o7777, 0o2751);
    assert_eq!(
        session.chmod(&b, 0o10000).unwrap_err().code,
        "sftp.invalid_mode"
    );
    for name in ["", ".", "..", "x\0y"] {
        let path = format!("{}/{name}", root.path().display());
        assert_eq!(
            session.create_file(&path).unwrap_err().code,
            "files.invalid_name"
        );
        assert_eq!(
            session.create_dir(&path).unwrap_err().code,
            "files.invalid_name"
        );
        assert_eq!(
            session.rename(&b, &path).unwrap_err().code,
            "files.invalid_name"
        );
    }
    // Linux 服务器允许本机 Windows 禁止的名字。
    session
        .create_file(&format!("{}/CON", root.path().display()))
        .unwrap();
}
#[test]
fn 删除目录树与链接但不删除链接目标() {
    let root = crate::test_support::tempdir();
    let outside = root.path().join("外部");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("保留"), "ok").unwrap();
    let tree = root.path().join("树");
    fs::create_dir_all(tree.join("子目录")).unwrap();
    fs::write(tree.join("子目录/文件"), "ok").unwrap();
    symlink(&outside, tree.join("链接")).unwrap();
    symlink(root.path().join("没有"), tree.join("坏链接")).unwrap();
    let mut session = crate::sftp::tests::server();
    assert_eq!(session.delete(&[tree.to_str().unwrap().into()]).unwrap(), 5);
    assert_eq!(fs::read(outside.join("保留")).unwrap(), b"ok");
    let missing = root.path().join("缺失").to_str().unwrap().to_owned();
    let e = session.delete(std::slice::from_ref(&missing)).unwrap_err();
    assert_eq!(e.code, "sftp.not_found");
    assert_eq!(e.params["path"], missing);
    assert_eq!(session.delete(&[]).unwrap(), 0);
}
#[test]
fn 文本三种拒绝bom往返版本冲突权限不变及临时文件清理() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("文本").to_str().unwrap().to_owned();
    let mut session = crate::sftp::tests::server();
    for (bytes, limit, code) in [
        (b"1234".as_slice(), 3, "files.too_large"),
        (b"a\0b", 10, "files.not_text"),
        (b"\xff", 10, "files.not_text"),
    ] {
        fs::write(&path, bytes).unwrap();
        assert_eq!(session.read_text(&path, limit).unwrap_err().code, code);
    }
    fs::write(&path, b"\xef\xbb\xbfhello\r\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o2751)).unwrap();
    let loaded = session.read_text(&path, 100).unwrap();
    assert!(loaded.bom);
    assert_eq!(loaded.text, "hello\r\n");
    assert_eq!(loaded.size, 10);
    let expected = FileVersion {
        hash: None,
        size: loaded.size,
        mtime: loaded.mtime,
    };
    let updated = session
        .write_text(&path, "中文\r\n", true, Some(expected.clone()))
        .unwrap();
    assert_eq!(updated.size, 11);
    assert_eq!(fs::read(&path).unwrap(), "\u{feff}中文\r\n".as_bytes());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o2751
    );
    assert_eq!(
        session
            .write_text(&path, "overwrite", false, Some(expected.clone()))
            .unwrap_err()
            .code,
        "files.changed"
    );
    session.write_text(&path, "plain", false, None).unwrap();
    assert!(!session.read_text(&path, 100).unwrap().bom);
    let link = root.path().join("链接");
    symlink(&path, &link).unwrap();
    session
        .write_text(link.to_str().unwrap(), "target", false, None)
        .unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&path).unwrap(), b"target");
    session.posix_rename = false;
    assert_eq!(
        session
            .write_text(&path, "no", false, None)
            .unwrap_err()
            .code,
        "sftp.atomic_rename_unsupported"
    );
    assert_eq!(fs::read(&path).unwrap(), b"target");
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    session.posix_rename = true;
    let current = session.read_text(&path, 100).unwrap();
    let changed_time = FileVersion {
        hash: None,
        size: current.size,
        mtime: current.mtime - 1,
    };
    assert_eq!(
        session
            .write_text(&path, "x", false, Some(changed_time.clone()))
            .unwrap_err()
            .code,
        "files.changed"
    );
    assert_eq!(
        session
            .write_text(
                &format!("{path}.moved"),
                "x",
                false,
                Some(changed_time.clone())
            )
            .unwrap_err()
            .code,
        "files.changed"
    );
}

#[test]
fn 读取中增长仍受限且写入中失败或并发修改会清理临时文件() {
    // 在真实 sftp-server 前插入协议故障，不改动被测会话的实现。
    let script = r#"
import os, struct, subprocess, sys
mode, target = sys.argv[1:]
server = subprocess.Popen(['/usr/lib/openssh/sftp-server'], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
def exact(stream, size):
    data = b''
    while len(data) < size:
        chunk = stream.read(size - len(data))
        if not chunk: raise EOFError()
        data += chunk
    return data
def packet(stream):
    return exact(stream, struct.unpack('>I', exact(stream, 4))[0])
def send(stream, data):
    stream.write(struct.pack('>I', len(data)) + data)
    stream.flush()
try:
    while True:
        request = packet(sys.stdin.buffer)
        kind = request[0]
        if kind == 9 and mode == 'chmod':
            response = bytes([101]) + request[1:5] + struct.pack('>III', 4, 0, 0)
        else:
            send(server.stdin, request)
            response = packet(server.stdout)
        if kind == 17 and mode == 'grow':
            with open(target, 'wb') as file: file.write(b'x' * 100)
        if kind == 9 and mode == 'changed':
            with open(target, 'wb') as file: file.write(b'concurrent change')
        send(sys.stdout.buffer, response)
except (EOFError, BrokenPipeError):
    pass
finally:
    server.kill()
    server.wait()
"#;
    let root = crate::test_support::tempdir();
    let path = root.path().join("文本").to_str().unwrap().to_owned();
    for (mode, code) in [
        ("grow", "files.too_large"),
        ("chmod", "sftp.failure"),
        ("changed", "files.changed"),
    ] {
        fs::write(&path, "old").unwrap();
        let mut session = crate::sftp::sshd_tests::open(
            &SshProgram {
                path: "/usr/bin/python3".into(),
            },
            &SshCommand {
                args: vec!["-c".into(), script.into(), mode.into(), path.clone()],
                env: vec![],
                config_text: None,
                builtin: None,
                askpass: None,
            },
            Duration::from_secs(5),
        )
        .unwrap();
        let expected = FileVersion {
            hash: None,
            size: 3,
            mtime: crate::fs_local::unix_seconds(fs::metadata(&path).unwrap().modified().unwrap()),
        };
        let error = if mode == "grow" {
            session.read_text(&path, 5).unwrap_err()
        } else {
            session
                .write_text(&path, "new", false, Some(expected.clone()))
                .unwrap_err()
        };
        assert_eq!(error.code, code);
        assert_eq!(error.params["path"], path);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        assert_eq!(
            fs::read(&path).unwrap(),
            match mode {
                "grow" => vec![b'x'; 100],
                "changed" => b"concurrent change".to_vec(),
                _ => b"old".to_vec(),
            }
        );
    }
}
