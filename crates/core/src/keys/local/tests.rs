use super::*;
use crate::security::MemoryCredentialStore;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[test]
fn 生成复用修复权限与重建() {
    let root = crate::test_support::tempdir();
    let dir = root.path().join("keys");
    let store = MemoryCredentialStore::default();
    assert!(load_local_key(&dir, &store).unwrap().is_none());
    let comment = key_comment("电脑 My-Host", "abcd1234").unwrap();
    assert_eq!(comment, "pierlink-My-Host-abcd1234");
    assert!(key_comment("x", "bad").is_err());
    let first = ensure_local_key(&dir, &comment, &store).unwrap();
    assert!(!first.regenerated);
    assert_eq!(
        first.public_line,
        load_local_key(&dir, &store).unwrap().unwrap().public_line
    );
    #[cfg(unix)]
    fs::set_permissions(&first.private_path, fs::Permissions::from_mode(0o666)).unwrap();
    // Windows ACL 放宽和修复由 windows 模块的真实测试覆盖。
    let second = ensure_local_key(&dir, &comment, &store).unwrap();
    assert_eq!(first.fingerprint, second.fingerprint);
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&first.private_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    fs::write(dir.join("pierlink_ed25519.pub"), "bad").unwrap();
    let third = ensure_local_key(&dir, &comment, &store).unwrap();
    assert!(third.regenerated);
    assert_ne!(first.fingerprint, third.fingerprint);
    fs::rename(&third.private_path, root.path().join("missing-private")).unwrap();
    assert!(load_local_key(&dir, &store).unwrap().is_none());
    assert!(
        ensure_local_key(&dir, &comment, &store)
            .unwrap()
            .regenerated
    );
    store.delete(CREDENTIAL_ID).unwrap();
    assert!(
        ensure_local_key(&dir, &comment, &store)
            .unwrap()
            .regenerated
    );
}
#[test]
fn 损坏私钥口令不符和公钥缺失均归档重建() {
    let root = crate::test_support::tempdir();
    let dir = root.path().join("keys");
    let store = MemoryCredentialStore::default();
    let mut key = ensure_local_key(&dir, "pierlink-test-abcd1234", &store).unwrap();
    for fault in 0..5 {
        match fault {
                0 => fs::rename(dir.join("pierlink_ed25519.pub"), root.path().join("missing-public")).unwrap(),
                1 => fs::write(&key.private_path, "损坏的私钥").unwrap(),
                2 => store.set(CREDENTIAL_ID, &"0".repeat(64)).unwrap(),
                3 => fs::write(dir.join("pierlink_ed25519.pub"), "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA pierlink-other").unwrap(),
                _ => store.delete(CREDENTIAL_ID).unwrap(),
            }
        let replacement = ensure_local_key(&dir, "pierlink-test-abcd1234", &store).unwrap();
        assert!(replacement.regenerated);
        assert_ne!(replacement.fingerprint, key.fingerprint);
        key = replacement;
    }
    assert_eq!(
        fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("old-"))
            .count(),
        3
    );
}
#[test]
fn 本机新锁保留且错误包含路径() {
    let root = crate::test_support::tempdir();
    let held = lock(root.path()).unwrap();
    let error = lock_with_timeout(root.path(), Duration::ZERO)
        .err()
        .unwrap();
    assert_eq!(error.params["path"], held.0.to_str().unwrap());
    assert!(held.0.is_dir());
    drop(held);
    assert!(!root.path().join(".pierlink-lock").exists());
}

#[cfg(unix)]
#[test]
fn 本机陈旧锁含残留文件时自动回收() {
    let root = crate::test_support::tempdir();
    let path = root.path().join(".pierlink-lock");
    fs::create_dir(&path).unwrap();
    fs::write(path.join("残留"), "测试").unwrap();
    fs::File::open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(601)))
        .unwrap();
    let held = lock(root.path()).unwrap();
    assert!(held.0.is_dir());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}
fn keygen_program() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/OpenSSH/ssh-keygen.exe")
    } else {
        PathBuf::from("/usr/bin/ssh-keygen")
    }
}

#[test]
fn 加密格式与系统解密及旧密钥兼容() {
    let root = crate::test_support::tempdir();
    let dir = root.path().join("keys");
    let store = MemoryCredentialStore::default();
    let key = ensure_local_key(&dir, "pierlink-test-abcd1234", &store).unwrap();
    let secret = store.get(CREDENTIAL_ID).unwrap().unwrap();
    let encoded = fs::read_to_string(&key.private_path).unwrap();
    let encrypted = PrivateKey::from_openssh(encoded).unwrap();
    assert_eq!(encrypted.cipher(), Cipher::Aes256Ctr);
    assert!(matches!(encrypted.kdf(), Kdf::Bcrypt { rounds: 64, .. }));
    assert!(encrypted.decrypt("错误口令").is_err());
    let output = legacy::keygen(
        &keygen_program(),
        &[
            "-y".into(),
            "-f".into(),
            key.private_path.to_str().unwrap().into(),
        ],
        &secret,
        &[legacy::decrypt_prompts(key.private_path.to_str().unwrap())],
        Duration::from_secs(30),
    )
    .unwrap()
    .unwrap();
    assert!(
        output
            .lines()
            .filter_map(parse)
            .any(|p| p.fingerprint == key.fingerprint)
    );
    let old_dir = root.path().join("old");
    fs::create_dir(&old_dir).unwrap();
    let old_path = old_dir.join("pierlink_ed25519");
    let old_name = old_path.to_str().unwrap();
    legacy::keygen(
        &keygen_program(),
        &[
            "-t".into(),
            "ed25519".into(),
            "-a".into(),
            "64".into(),
            "-C".into(),
            "pierlink-old-abcd1234".into(),
            "-f".into(),
            old_name.into(),
            "-q".into(),
        ],
        &secret,
        &legacy::generate_prompts(old_name),
        Duration::from_secs(30),
    )
    .unwrap()
    .unwrap();
    let bytes = fs::read(&old_path).unwrap();
    let loaded = load_local_key(&old_dir, &store).unwrap().unwrap();
    assert!(loaded.public_line.ends_with("pierlink-old-abcd1234"));
    assert!(
        !ensure_local_key(&old_dir, "pierlink-new-abcd1234", &store)
            .unwrap()
            .regenerated
    );
    assert_eq!(fs::read(&old_path).unwrap(), bytes);
}

#[test]
fn 凭据故障不覆盖密钥且错误不包含秘密() {
    struct Unavailable;
    impl CredentialStore for Unavailable {
        fn get(&self, _: &str) -> Result<Option<String>, AppError> {
            Err(AppError::new("秘密口令"))
        }
        fn set(&self, _: &str, _: &str) -> Result<(), AppError> {
            Err(AppError::new("秘密口令"))
        }
        fn delete(&self, _: &str) -> Result<(), AppError> {
            panic!("不应删除凭据")
        }
    }
    let root = crate::test_support::tempdir();
    let dir = root.path().join("keys");
    let store = MemoryCredentialStore::default();
    let key = ensure_local_key(&dir, "pierlink-test-abcd1234", &store).unwrap();
    let original = fs::read(&key.private_path).unwrap();
    for error in [
        ensure_local_key(&dir, "pierlink-test-abcd1234", &Unavailable).unwrap_err(),
        load_local_key(&dir, &Unavailable).unwrap_err(),
    ] {
        assert!(!format!("{error:?}").contains("秘密口令"));
    }
    assert_eq!(fs::read(&key.private_path).unwrap(), original);
    let failed_dir = root.path().join("failed");
    assert!(ensure_local_key(&failed_dir, "pierlink-test-abcd1234", &Unavailable).is_err());
    assert_eq!(fs::read_dir(failed_dir).unwrap().count(), 0);
    #[cfg(unix)]
    {
        let actual = root.path().join("actual");
        fs::rename(&key.private_path, &actual).unwrap();
        std::os::unix::fs::symlink(actual, &key.private_path).unwrap();
        assert!(ensure_local_key(&dir, "pierlink-test-abcd1234", &store).is_err());
        assert!(key.private_path.is_symlink());
    }
}

#[test]
fn 并发仅生成一次且不创建子进程() {
    let root = crate::test_support::tempdir();
    let dir = root.path().join("keys");
    let store = MemoryCredentialStore::default();
    thread::scope(|scope| {
        let a = scope.spawn(|| ensure_local_key(&dir, "pierlink-test-abcd1234", &store).unwrap());
        let b = scope.spawn(|| ensure_local_key(&dir, "pierlink-test-abcd1234", &store).unwrap());
        #[cfg(target_os = "linux")]
        while !a.is_finished() || !b.is_finished() {
            for entry in fs::read_dir("/proc").unwrap().flatten() {
                let args = fs::read(entry.path().join("cmdline")).unwrap_or_default();
                assert!(!String::from_utf8_lossy(&args).contains(dir.to_str().unwrap()));
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(a.join().unwrap().fingerprint, b.join().unwrap().fingerprint);
    });
    assert!(load_local_key(&dir, &store).unwrap().is_some());
    assert_eq!(fs::read_dir(dir).unwrap().count(), 2);
}

#[cfg(unix)]
use crate::builtin_test_support::sshd as support;

#[cfg(unix)]
#[test]
#[ignore = "需要允许普通用户启动本机 sshd 并监听随机端口"]
fn 生成密钥同时供系统与内置引擎登录本机服务器() {
    let server = support::Server::with_root(crate::test_support::tempdir(), "", false);
    let store = MemoryCredentialStore::default();
    let key = ensure_local_key(
        &server.root.path().join("keys"),
        "pierlink-test-abcd1234",
        &store,
    )
    .unwrap();
    let secret = store.get(CREDENTIAL_ID).unwrap().unwrap();
    fs::write(
        server.root.path().join("client.pub"),
        format!("{}\n", key.public_line),
    )
    .unwrap();
    let known = server.root.path().join("known_hosts");
    fs::write(
        &known,
        format!(
            "[127.0.0.1]:{} {}",
            server.port,
            fs::read_to_string(server.root.path().join("host.pub")).unwrap()
        ),
    )
    .unwrap();
    let output = legacy::keygen(
        Path::new("/usr/bin/ssh"),
        &[
            "-F".into(),
            "none".into(),
            "-T".into(),
            "-o".into(),
            "StrictHostKeyChecking=yes".into(),
            "-o".into(),
            format!("UserKnownHostsFile={}", known.display()),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-o".into(),
            "PreferredAuthentications=publickey".into(),
            "-p".into(),
            server.port.to_string(),
            "-l".into(),
            server.user.clone(),
            "-i".into(),
            key.private_path.to_str().unwrap().into(),
            "127.0.0.1".into(),
            "printf key-ok".into(),
        ],
        &secret,
        &[vec![format!(
            "Enter passphrase for key '{}':",
            key.private_path.display()
        )]],
        Duration::from_secs(30),
    )
    .unwrap()
    .unwrap();
    assert!(output.contains("key-ok"));
    let engine = pierlink_ssh_engine::Engine::new().unwrap();
    let connection = engine
        .connect(
            server.options(),
            pierlink_ssh_engine::Authentication::PrivateKey {
                path: key.private_path,
                passphrase: Some(std::sync::Arc::new(move || Some(secret.clone()))),
            },
        )
        .unwrap();
    assert_eq!(support::command(&connection, "printf key-ok"), b"key-ok");
}
