use super::*;
#[cfg(unix)]
use crate::sftp::tests::server;
#[cfg(unix)]
#[test]
fn 下载冲突路径为普通规范绝对路径且上传来源前缀统一() {
    let root = crate::test_support::tempdir();
    let remote = root.path().join("remote");
    let local = root.path().join("local");
    fs::create_dir(&remote).unwrap();
    fs::create_dir(&local).unwrap();
    fs::write(remote.join("记录.txt"), "remote").unwrap();
    fs::write(local.join("记录.txt"), "local").unwrap();
    let mut session = server();
    let plan = plan_download(
        &mut session,
        &[remote.join("记录.txt").to_str().unwrap().into()],
        &local,
    )
    .unwrap();
    assert_eq!(
        plan.existing,
        [fs::canonicalize(&local)
            .unwrap()
            .join("记录.txt")
            .display()
            .to_string()]
    );
    let plan = plan_upload(
        &mut session,
        &[local.join("记录.txt")],
        remote.to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(
        plan.entries[0].local,
        fs::canonicalize(local.join("记录.txt")).unwrap()
    );
}
#[test]
fn f1_windows非法名尾部与保留名() {
    for (input, output) in [
        ("a<>:\"/\\|?*z", "a_________z"),
        ("中文. ", "中文__"),
        ("CON", "_CON"),
        ("nul.txt", "_nul.txt"),
        ("Lpt9.log", "_Lpt9.log"),
        ("COM1", "_COM1"),
        ("COM¹.txt", "_COM¹.txt"),
        ("COM0", "COM0"),
        ("COM10", "COM10"),
        ("a\n", "a_"),
        ("", "_"),
        ("..", "__"),
        ("普通.txt", "普通.txt"),
    ] {
        assert_eq!(windows_filename(input), output, "{input:?}");
    }
    // 替换后可能重名，必须在开始传输前拒绝碰撞。
    let mut plan = empty_plan();
    for _ in 0..2 {
        plan.push(PlannedEntry {
            local: "same".into(),
            remote: "same".into(),
            kind: EntryKind::File,
            size: 0,
            mtime: 0,
            mode: 0,
        })
        .unwrap();
    }
    assert_eq!(
        plan.validate_targets(false).unwrap_err().code,
        "transfer.exists"
    );
    assert_eq!(
        plan.validate_targets(true).unwrap_err().code,
        "transfer.exists"
    );
    assert!(component("..").is_err());
    assert!(component("a/b").is_err());
}
#[cfg(unix)]
#[test]
fn 递归双向传输空目录链接与修改时间() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = crate::test_support::tempdir();
    let source = root.path().join("source");
    let remote = root.path().join("remote");
    let target = root.path().join("target");
    for dir in [&source, &remote, &target] {
        fs::create_dir(dir).unwrap();
    }
    fs::create_dir(source.join("empty")).unwrap();
    fs::create_dir(source.join("nested")).unwrap();
    let bytes: Vec<_> = (0..1_100_001).map(|i| (i % 199) as u8).collect();
    let file = source.join("nested/中文");
    fs::write(&file, &bytes).unwrap();
    set_mtime(&File::open(&file).unwrap(), 1_234_567_890).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o750)).unwrap();
    fs::write(source.join(".hidden"), b"hidden").unwrap();
    symlink("nested", source.join("link")).unwrap();
    let mut session = server();
    let cancel = AtomicBool::new(false);
    let plan = plan_upload(
        &mut session,
        std::slice::from_ref(&source),
        remote.to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(plan.files_total, 2);
    assert_eq!(plan.total_bytes, bytes.len() as u64 + 6);
    assert!(plan.existing.is_empty());
    assert_eq!(plan.skipped.len(), 1);
    let mut last = None;
    let report = upload(
        &mut session,
        std::slice::from_ref(&source),
        remote.to_str().unwrap(),
        Conflict::Error,
        &mut |p| last = Some(p.clone()),
        &cancel,
    )
    .unwrap();
    assert_eq!(report.succeeded, 2);
    assert!(report.failed.is_empty());
    assert_eq!(report.skipped.len(), 1);
    let last = last.unwrap();
    assert_eq!(last.done_bytes, plan.total_bytes);
    assert_eq!(last.files_done, 2);
    assert!(remote.join("source/empty").is_dir());
    assert!(!remote.join("source/link").exists());
    assert_eq!(
        session
            .stat(remote.join("source/nested/中文").to_str().unwrap())
            .unwrap()
            .unwrap()
            .mtime,
        1_234_567_890
    );
    assert_eq!(
        fs::metadata(remote.join("source/nested/中文"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
    symlink("nested", remote.join("source/link")).unwrap();
    let sources = [remote.join("source").to_str().unwrap().to_owned()];
    assert_eq!(
        plan_download(&mut session, &sources, &target)
            .unwrap()
            .files_total,
        2
    );
    let report = download(
        &mut session,
        &sources,
        &target,
        Conflict::Error,
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(report.succeeded, 2);
    assert!(report.failed.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert!(target.join("source/empty").is_dir());
    assert!(!target.join("source/link").exists());
    assert_eq!(fs::read(target.join("source/nested/中文")).unwrap(), bytes);
    assert_eq!(
        fs_local::unix_seconds(
            fs::metadata(target.join("source/nested/中文"))
                .unwrap()
                .modified()
                .unwrap()
        ),
        1_234_567_890
    );
}
#[cfg(unix)]
#[test]
fn f1_两端三种冲突策略与开始前检查() {
    let root = crate::test_support::tempdir();
    let source = root.path().join("source");
    let remote = root.path().join("remote");
    let target = root.path().join("target");
    for dir in [&source, &remote, &target] {
        fs::create_dir(dir).unwrap();
    }
    for dir in [&source, &target, &remote] {
        fs::write(dir.join("file"), b"old").unwrap();
    }
    fs::write(source.join("file"), b"new").unwrap();
    fs::write(source.join("first"), b"first").unwrap();
    let mut session = server();
    let cancel = AtomicBool::new(false);
    let sources = [source.join("first"), source.join("file")];
    let plan = plan_upload(&mut session, &sources, remote.to_str().unwrap()).unwrap();
    assert_eq!(plan.existing.len(), 1);
    let error = upload(
        &mut session,
        &sources,
        remote.to_str().unwrap(),
        Conflict::Error,
        &mut |_| {},
        &cancel,
    )
    .unwrap_err();
    assert_eq!(error.code, "transfer.exists");
    assert!(error.params["items"].contains("file"));
    assert!(!remote.join("first").exists());
    let report = upload(
        &mut session,
        &sources,
        remote.to_str().unwrap(),
        Conflict::Skip,
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(report.succeeded, 1);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(fs::read(remote.join("file")).unwrap(), b"old");
    assert_eq!(
        upload(
            &mut session,
            &sources,
            remote.to_str().unwrap(),
            Conflict::Overwrite,
            &mut |_| {},
            &cancel
        )
        .unwrap()
        .succeeded,
        2
    );
    assert_eq!(fs::read(remote.join("file")).unwrap(), b"new");
    let sources = [
        remote.join("first").to_str().unwrap().into(),
        remote.join("file").to_str().unwrap().into(),
    ];
    assert_eq!(
        plan_download(&mut session, &sources, &target)
            .unwrap()
            .existing
            .len(),
        1
    );
    assert_eq!(
        download(
            &mut session,
            &sources,
            &target,
            Conflict::Error,
            &mut |_| {},
            &cancel
        )
        .unwrap_err()
        .code,
        "transfer.exists"
    );
    assert!(!target.join("first").exists());
    let report = download(
        &mut session,
        &sources,
        &target,
        Conflict::Skip,
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(report.succeeded, 1);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(fs::read(target.join("file")).unwrap(), b"old");
    let report = download(
        &mut session,
        &sources,
        &target,
        Conflict::Overwrite,
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(report.succeeded, 2);
    assert_eq!(fs::read(target.join("file")).unwrap(), b"new");
}
#[cfg(unix)]
#[test]
fn f2_取消失败断线清理part及保护已有文件() {
    use std::sync::atomic::Ordering;
    let root = crate::test_support::tempdir();
    let source = root.path().join("file");
    let remote = root.path().join("remote");
    let target = root.path().join("target");
    fs::create_dir(&remote).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(&source, vec![42; 1_100_000]).unwrap();
    fs::write(remote.join("file"), b"original").unwrap();
    let mut session = server();
    let cancel = AtomicBool::new(false);
    let report = upload(
        &mut session,
        std::slice::from_ref(&source),
        remote.to_str().unwrap(),
        Conflict::Overwrite,
        &mut |p| {
            if p.done_bytes > 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        &cancel,
    )
    .unwrap_err();
    assert_eq!(report.code, "sftp.cancelled");
    assert!(!remote.join("file.pierlink-part").exists());
    assert_eq!(fs::read(remote.join("file")).unwrap(), b"original");
    cancel.store(false, Ordering::Relaxed);
    // 上次崩溃留下的固定后缀临时文件应删除并重试一次。
    fs::write(remote.join("file.pierlink-part"), b"other transfer").unwrap();
    let report = upload(
        &mut session,
        std::slice::from_ref(&source),
        remote.to_str().unwrap(),
        Conflict::Overwrite,
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert!(report.failed.is_empty());
    assert_eq!(report.succeeded, 1);
    assert!(!remote.join("file.pierlink-part").exists());
    assert_eq!(
        fs::read(remote.join("file")).unwrap(),
        fs::read(&source).unwrap()
    );
    fs::write(remote.join("file"), b"original").unwrap();
    let disconnect = session.transport.disconnect();
    let error = upload(
        &mut session,
        std::slice::from_ref(&source),
        remote.to_str().unwrap(),
        Conflict::Overwrite,
        &mut |p| {
            if p.done_bytes > 0 {
                disconnect();
            }
        },
        &cancel,
    )
    .unwrap_err();
    assert_eq!(error.code, "sftp.connection_lost");
    assert!(!remote.join("file.pierlink-part").exists());
    let mut session = server();
    fs::write(target.join("file"), b"original").unwrap();
    let sources = [source.to_str().unwrap().to_owned()];
    let error = download(
        &mut session,
        &sources,
        &target,
        Conflict::Overwrite,
        &mut |p| {
            if p.done_bytes > 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        &cancel,
    )
    .unwrap_err();
    assert_eq!(error.code, "sftp.cancelled");
    assert!(!target.join("file.pierlink-part").exists());
    assert_eq!(fs::read(target.join("file")).unwrap(), b"original");
    cancel.store(false, Ordering::Relaxed);
    let disconnect = session.transport.disconnect();
    let error = download(
        &mut session,
        &sources,
        &target,
        Conflict::Overwrite,
        &mut |p| {
            if p.done_bytes > 0 {
                disconnect();
            }
        },
        &cancel,
    )
    .unwrap_err();
    assert_eq!(error.code, "sftp.connection_lost");
    assert!(!target.join("file.pierlink-part").exists());
    assert_eq!(fs::read(target.join("file")).unwrap(), b"original");
}
#[cfg(unix)]
#[test]
fn f2_完成改名失败也清理part() {
    let root = crate::test_support::tempdir();
    let remote = root.path().join("remote");
    let target = root.path().join("target");
    fs::create_dir(&remote).unwrap();
    fs::create_dir(&target).unwrap();
    let file = root.path().join("file");
    fs::write(&file, b"contents").unwrap();
    let mut session = server();
    let cancel = AtomicBool::new(false);
    let report = upload(
        &mut session,
        std::slice::from_ref(&file),
        remote.to_str().unwrap(),
        Conflict::Overwrite,
        &mut |p| {
            if p.done_bytes > 0 {
                let _ = fs::create_dir(remote.join("file"));
            }
        },
        &cancel,
    )
    .unwrap();
    assert_eq!(report.failed.len(), 1);
    assert!(!remote.join("file.pierlink-part").exists());
    assert!(remote.join("file").is_dir());
    let sources = [file.to_str().unwrap().to_owned()];
    let report = download(
        &mut session,
        &sources,
        &target,
        Conflict::Overwrite,
        &mut |p| {
            if p.done_bytes > 0 {
                let _ = fs::create_dir(target.join("file"));
            }
        },
        &cancel,
    )
    .unwrap();
    assert_eq!(report.failed.len(), 1);
    assert!(!target.join("file.pierlink-part").exists());
    assert!(target.join("file").is_dir());
}
#[cfg(unix)]
#[test]
fn 目标链接根目录可用且跳过已有目录时继续合并子项() {
    use std::os::unix::fs::symlink;
    let root = crate::test_support::tempdir();
    let source = root.path().join("source");
    let remote = root.path().join("remote");
    let target = root.path().join("target");
    for path in [&source, &remote, &target] {
        fs::create_dir(path).unwrap();
    }
    fs::write(source.join("file"), b"data").unwrap();
    let remote_link = root.path().join("remote-link");
    let target_link = root.path().join("target-link");
    symlink(&remote, &remote_link).unwrap();
    symlink(&target, &target_link).unwrap();
    let mut session = server();
    let cancel = AtomicBool::new(false);
    let report = upload(
        &mut session,
        std::slice::from_ref(&source),
        remote_link.to_str().unwrap(),
        Conflict::Error,
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(report.succeeded, 1);
    fs::write(source.join("new"), b"new").unwrap();
    let report = upload(
        &mut session,
        &[source],
        remote_link.to_str().unwrap(),
        Conflict::Skip,
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(report.succeeded, 1);
    assert_eq!(fs::read(remote.join("source/new")).unwrap(), b"new");
    let sources = [remote.join("source").to_str().unwrap().into()];
    let report = download(
        &mut session,
        &sources,
        &target_link,
        Conflict::Error,
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(report.succeeded, 2);
    fs::remove_file(target.join("source/new")).unwrap();
    assert_eq!(fs::read(target.join("source/file")).unwrap(), b"data");
    fs::write(remote.join("source/new"), b"new").unwrap();
    let report = download(
        &mut session,
        &sources,
        &target_link,
        Conflict::Skip,
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(report.succeeded, 1);
    assert_eq!(fs::read(target.join("source/new")).unwrap(), b"new");
}
#[test]
fn 完成下载时不覆盖扫描之后新出现的目标() {
    let root = crate::test_support::tempdir();
    let from = root.path().join("part");
    let to = root.path().join("final");
    fs::write(&from, b"new").unwrap();
    fs::write(&to, b"old").unwrap();
    assert_eq!(
        complete_local(&from, &to, false).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(fs::read(&to).unwrap(), b"old");
    assert_eq!(fs::read(&from).unwrap(), b"new");
    complete_local(&from, &to, true).unwrap();
    assert_eq!(fs::read(&to).unwrap(), b"new");
    assert!(!from.exists());
}

#[cfg(unix)]
#[test]
fn 双向目录合并文件跳过及类型冲突保护() {
    let root = crate::test_support::tempdir();
    for upload_direction in [true, false] {
        for conflict in [Conflict::Error, Conflict::Skip, Conflict::Overwrite] {
            let case = root.path().join(format!("{upload_direction}-{conflict:?}"));
            let source = case.join("source/tree");
            let target = case.join("target");
            let tree = target.join("tree");
            fs::create_dir_all(source.join("nested")).unwrap();
            fs::create_dir_all(tree.join("nested")).unwrap();
            fs::write(source.join("nested/same"), b"new same").unwrap();
            fs::write(source.join("nested/new"), b"new file").unwrap();
            fs::write(tree.join("nested/same"), b"old same").unwrap();
            fs::write(tree.join("nested/keep"), b"keep").unwrap();
            fs::write(source.join("file-to-dir"), b"file").unwrap();
            fs::create_dir(tree.join("file-to-dir")).unwrap();
            fs::write(tree.join("file-to-dir/keep"), b"keep").unwrap();
            fs::create_dir(source.join("dir-to-file")).unwrap();
            fs::write(source.join("dir-to-file/child"), b"child").unwrap();
            fs::write(tree.join("dir-to-file"), b"old file").unwrap();
            let mut session = server();
            let result = if upload_direction {
                let plan = plan_upload(
                    &mut session,
                    std::slice::from_ref(&source),
                    target.to_str().unwrap(),
                )
                .unwrap();
                assert_eq!(plan.existing, [tree.to_str().unwrap()]);
                upload(
                    &mut session,
                    &[source],
                    target.to_str().unwrap(),
                    conflict,
                    &mut |_| {},
                    &AtomicBool::new(false),
                )
            } else {
                let sources = [source.to_str().unwrap().to_owned()];
                let plan = plan_download(&mut session, &sources, &target).unwrap();
                assert_eq!(plan.existing, [tree.to_str().unwrap()]);
                download(
                    &mut session,
                    &sources,
                    &target,
                    conflict,
                    &mut |_| {},
                    &AtomicBool::new(false),
                )
            };
            if conflict == Conflict::Error {
                assert_eq!(result.unwrap_err().code, "transfer.exists");
                assert!(!tree.join("nested/new").exists());
            } else {
                let report = result.unwrap();
                assert_eq!(fs::read(tree.join("nested/new")).unwrap(), b"new file");
                if conflict == Conflict::Skip {
                    assert_eq!(report.succeeded, 1);
                    assert_eq!(report.skipped.len(), 4);
                    assert!(report.failed.is_empty());
                    assert_eq!(fs::read(tree.join("nested/same")).unwrap(), b"old same");
                } else {
                    assert_eq!(report.succeeded, 2);
                    assert_eq!(report.failed.len(), 2);
                    assert_eq!(report.skipped.len(), 1);
                    assert_eq!(fs::read(tree.join("nested/same")).unwrap(), b"new same");
                }
            }
            assert_eq!(fs::read(tree.join("file-to-dir/keep")).unwrap(), b"keep");
            assert_eq!(fs::read(tree.join("dir-to-file")).unwrap(), b"old file");
            assert_eq!(fs::read(tree.join("nested/keep")).unwrap(), b"keep");
        }
    }
}
#[cfg(unix)]
#[test]
fn 仅远端统计递归目录和跳过链接() {
    use std::os::unix::fs::symlink;
    let root = crate::test_support::tempdir();
    let source = root.path().join("source");
    fs::create_dir_all(source.join("deep/empty")).unwrap();
    fs::write(source.join("deep/file"), b"1234567").unwrap();
    fs::write(source.join(".hidden"), b"12345").unwrap();
    symlink("deep", source.join("dir-link")).unwrap();
    symlink("deep/file", source.join("file-link")).unwrap();
    symlink("missing", source.join("broken-link")).unwrap();
    let mut session = server();
    let plan = plan_remote(&mut session, &[source.to_str().unwrap().into()]).unwrap();
    assert_eq!(plan.files_total, 2);
    assert_eq!(plan.total_bytes, 12);
    assert_eq!(plan.skipped.len(), 3);
    assert!(plan.existing.is_empty() && plan.renamed.is_empty());
    assert!(plan.entries.iter().all(|e| e.local.as_os_str().is_empty()));
    assert_eq!(plan_remote(&mut session, &[]).unwrap().files_total, 0);
    assert_eq!(
        plan_remote(
            &mut session,
            &[source.join("missing").to_str().unwrap().into()]
        )
        .unwrap_err()
        .code,
        "sftp.not_found"
    );
}
#[cfg(unix)]
#[test]
fn 无扩展覆盖报告空窗且失败清理part() {
    use crate::{process::SshProgram, ssh::args::SshCommand};
    let root = crate::test_support::tempdir();
    let source = root.path().join("file");
    let target = root.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(&source, b"new").unwrap();
    fs::write(target.join("file"), b"old").unwrap();
    let mut session = server();
    session.posix_rename = false;
    let report = upload(
        &mut session,
        std::slice::from_ref(&source),
        target.to_str().unwrap(),
        Conflict::Overwrite,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(report.succeeded, 1);
    assert!(report.failed.is_empty());
    assert_eq!(report.warnings.len(), 1);
    assert_eq!(
        report.warnings[0].reason.code,
        "transfer.non_atomic_overwrite"
    );
    assert_eq!(fs::read(target.join("file")).unwrap(), b"new");
    assert!(!target.join("file.pierlink-part").exists());
    let mut session = crate::sftp::sshd_tests::open(
        &SshProgram {
            path: "/usr/lib/openssh/sftp-server".into(),
        },
        &SshCommand {
            args: vec!["-P".into(), "rename".into()],
            env: vec![],
            config_text: None,
            builtin: None,
            askpass: None,
        },
        Duration::from_secs(5),
    )
    .unwrap();
    session.posix_rename = false;
    let report = upload(
        &mut session,
        &[source],
        target.to_str().unwrap(),
        Conflict::Overwrite,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(report.failed.len(), 1);
    assert!(
        report.failed[0]
            .reason
            .params
            .contains_key("overwrite_note")
    );
    assert!(!target.join("file.pierlink-part").exists());
    assert!(!target.join("file").exists());
}
#[cfg(unix)]
#[test]
fn 下载残留part重试且同名临时目录不删除() {
    let root = crate::test_support::tempdir();
    let source = root.path().join("file");
    let target = root.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(&source, b"new").unwrap();
    fs::write(target.join("file.pierlink-part"), b"stale").unwrap();
    let mut session = server();
    let sources = [source.to_str().unwrap().to_owned()];
    let report = download(
        &mut session,
        &sources,
        &target,
        Conflict::Overwrite,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(report.succeeded, 1);
    assert!(report.failed.is_empty());
    assert_eq!(fs::read(target.join("file")).unwrap(), b"new");
    assert!(!target.join("file.pierlink-part").exists());
    fs::create_dir(target.join("file.pierlink-part")).unwrap();
    fs::write(target.join("file.pierlink-part/keep"), b"keep").unwrap();
    let report = download(
        &mut session,
        &sources,
        &target,
        Conflict::Overwrite,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(report.failed.len(), 1);
    let report = upload(
        &mut session,
        &[source],
        target.to_str().unwrap(),
        Conflict::Overwrite,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(report.failed.len(), 1);
    assert_eq!(
        fs::read(target.join("file.pierlink-part/keep")).unwrap(),
        b"keep"
    );
    assert_eq!(fs::read(target.join("file")).unwrap(), b"new");
}
