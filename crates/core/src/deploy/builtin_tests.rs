//! 内置上传沿用分块续传和校验安装流程；所有远端路径由测试 sshd 隔离。
use super::*;
use crate::{builtin::ConnectionClass, builtin_test_support::Fixture};
use sha2::{Digest, Sha256};
use std::{
    fs,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[test]
#[ignore = "需要普通用户本机 sshd 和随机监听端口"]
fn 内置上传取消断线续传校验失败保留旧程序及复用已安装文件() {
    let f = Fixture::new();
    set_computer_short_id("test0001").unwrap();
    let command = f.command();
    let context = command.builtin.as_ref().unwrap();
    let main = f.manager.get_connection(context.request()).unwrap();
    let bytes: Vec<u8> = (0..2 * 1024 * 1024 + 17).map(|_| rand::random()).collect();
    let root = f.sshd.root.path();
    let raw = root.join("raw");
    let gz = root.join("raw.gz");
    fs::write(&raw, &bytes).unwrap();
    assert!(
        std::process::Command::new("gzip")
            .arg("-c")
            .arg(&raw)
            .stdout(fs::File::create(&gz).unwrap())
            .status()
            .unwrap()
            .success()
    );
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let total = fs::metadata(&gz).unwrap().len();
    let cancel = AtomicBool::new(false);
    let error = upload_binary(
        &f.program(),
        &command,
        &gz,
        &hash,
        "2.1.7",
        Arch::X86_64,
        &|done, size| {
            assert_eq!(size, total);
            if done >= 1024 * 1024 {
                cancel.store(true, Ordering::Release);
            }
        },
        &cancel,
    )
    .unwrap_err();
    assert_eq!(error.code, "deploy.cancelled");
    let binary = root.join("h/.local/share/pierlink/bin/2.1.7/amd64/syncthing");
    assert!(!binary.exists());
    let part = binary.with_extension("gz.part");
    assert_eq!(fs::metadata(&part).unwrap().len(), 1024 * 1024);
    let mut request = context.request();
    request.class = ConnectionClass::Bulk;
    let old = f.manager.get_connection(request).unwrap();
    assert!(!old.same_connection(&main));
    old.abort();
    cancel.store(false, Ordering::Release);
    let progress = Mutex::new(Vec::new());
    let uploaded = upload_binary(
        &f.program(),
        &command,
        &gz,
        &hash,
        "2.1.7",
        Arch::X86_64,
        &|done, size| {
            assert_eq!(size, total);
            progress.lock().unwrap().push(done);
        },
        &cancel,
    )
    .unwrap();
    assert_eq!(std::path::Path::new(&uploaded), binary);
    assert_eq!(fs::read(&binary).unwrap(), bytes);
    assert!(!part.exists());
    let progress = progress.into_inner().unwrap();
    assert_eq!(progress[0], 1024 * 1024);
    assert_eq!(progress.last(), Some(&total));
    assert!(progress.windows(2).all(|p| p[0] <= p[1]));
    assert!(!main.is_closed());
    let mut request = context.request();
    request.class = ConnectionClass::Bulk;
    assert!(!old.same_connection(&f.manager.get_connection(request).unwrap()));
    let progress = Mutex::new(Vec::new());
    assert_eq!(
        upload_binary(
            &f.program(),
            &command,
            &gz,
            &hash,
            "2.1.7",
            Arch::X86_64,
            &|n, _| progress.lock().unwrap().push(n),
            &cancel
        )
        .unwrap(),
        uploaded
    );
    assert_eq!(*progress.lock().unwrap(), [total]);
    let error = upload_binary(
        &f.program(),
        &command,
        &gz,
        &"0".repeat(64),
        "2.1.7",
        Arch::X86_64,
        &|_, _| {},
        &cancel,
    )
    .unwrap_err();
    assert_eq!(error.code, "deploy.checksum_mismatch");
    assert_eq!(fs::read(&binary).unwrap(), bytes);
    assert!(!part.exists());
    assert!(!binary.with_extension("unpack").exists());
    assert_eq!(context.request().class, ConnectionClass::Main);
}
