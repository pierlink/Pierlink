use super::*;
use crate::security::MemoryCredentialStore;
use ssh_key::Algorithm;

fn new_key() -> PrivateKey {
    let mut rng = rand::rng();
    let mut key = PrivateKey::random(&mut rng, Algorithm::Ed25519).unwrap();
    key.set_comment("me@pc");
    key
}

/// 把私钥写成文件（给了口令就加密）
fn write_source(dir: &Path, name: &str, key: &PrivateKey, passphrase: Option<&str>) -> PathBuf {
    let text = match passphrase {
        Some(p) => encrypt(key, p).unwrap(),
        None => key.to_openssh(LineEnding::LF).unwrap(),
    };
    let path = dir.join(name);
    fs::write(&path, text.as_bytes()).unwrap();
    path
}

fn fingerprint(key: &PrivateKey) -> String {
    key.public_key().fingerprint(HashAlg::Sha256).to_string()
}

#[test]
fn 导入没有口令的私钥_加密存好_随机口令存进凭据管理器() {
    let (src, dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = MemoryCredentialStore::default();
    let key = new_key();
    let source = write_source(src.path(), "id_ed25519", &key, None);
    let imported = import(dir.path(), &source, "", None, false, &store).unwrap();
    // 名字空着时用文件名
    assert_eq!(imported.name, "id_ed25519");
    assert_eq!(imported.algorithm, "ssh-ed25519");
    assert_eq!(imported.fingerprint, fingerprint(&key));
    assert_eq!(imported.comment, "me@pc");
    assert!(!imported.user_passphrase);
    assert!(imported.passphrase_saved);
    // 存下的是加密的私钥，用凭据管理器里的口令才解得开
    let stored = PrivateKey::from_openssh(fs::read(&imported.path).unwrap()).unwrap();
    assert!(stored.is_encrypted());
    let secret = saved_passphrase(dir.path(), &imported.path, &store).unwrap();
    let opened = stored.decrypt(secret.as_bytes()).unwrap();
    assert_eq!(fingerprint(&opened), fingerprint(&key));
    assert_eq!(list(dir.path(), &store).unwrap(), vec![imported]);
}

#[test]
fn 导入有口令的私钥_要口令_口令不对不收_口令可以之后再存或忘掉() {
    let (src, dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = MemoryCredentialStore::default();
    let key = new_key();
    let source = write_source(src.path(), "work", &key, Some("secret pw"));
    let code = |r: Result<ImportedKey, AppError>| r.unwrap_err().code;
    assert_eq!(
        code(import(dir.path(), &source, "", None, true, &store)),
        "keys.imported_needs_passphrase"
    );
    assert_eq!(
        code(import(dir.path(), &source, "", Some("wrong"), true, &store)),
        "keys.imported_wrong_passphrase"
    );
    // 口令不对时什么也没留下
    assert!(list(dir.path(), &store).unwrap().is_empty());

    let imported = import(
        dir.path(),
        &source,
        " 工作机 ",
        Some("secret pw"),
        false,
        &store,
    )
    .unwrap();
    assert_eq!(imported.name, "工作机");
    assert!(imported.user_passphrase);
    assert!(!imported.passphrase_saved);
    assert!(saved_passphrase(dir.path(), &imported.path, &store).is_none());
    // 存下的私钥还是用原来的口令解开
    let stored = PrivateKey::from_openssh(fs::read(&imported.path).unwrap()).unwrap();
    assert_eq!(
        fingerprint(&stored.decrypt(b"secret pw").unwrap()),
        fingerprint(&key)
    );

    assert_eq!(
        set_passphrase(dir.path(), &imported.id, Some("nope"), &store)
            .unwrap_err()
            .code,
        "keys.imported_wrong_passphrase"
    );
    set_passphrase(dir.path(), &imported.id, Some("secret pw"), &store).unwrap();
    assert_eq!(
        saved_passphrase(dir.path(), &imported.path, &store)
            .as_deref()
            .map(String::as_str),
        Some("secret pw")
    );
    assert!(list(dir.path(), &store).unwrap()[0].passphrase_saved);
    set_passphrase(dir.path(), &imported.id, None, &store).unwrap();
    assert!(saved_passphrase(dir.path(), &imported.path, &store).is_none());
}

#[test]
fn 重复导入时用原来那一把_挡掉公钥和不是私钥的文件() {
    let (src, dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = MemoryCredentialStore::default();
    let key = new_key();
    let source = write_source(src.path(), "a", &key, None);
    let first = import(dir.path(), &source, "第一把", None, false, &store).unwrap();
    let again = import(dir.path(), &source, "别的名字", None, false, &store).unwrap();
    assert_eq!(again, first);

    let public = src.path().join("a.pub");
    fs::write(&public, key.public_key().to_openssh().unwrap()).unwrap();
    assert_eq!(
        import(dir.path(), &public, "", None, false, &store)
            .unwrap_err()
            .code,
        "keys.imported_public_key"
    );

    let other = src.path().join("notes.txt");
    fs::write(&other, "hello").unwrap();
    assert_eq!(
        import(dir.path(), &other, "", None, false, &store)
            .unwrap_err()
            .code,
        "keys.imported_unsupported"
    );

    let missing = src.path().join("missing");
    assert_eq!(
        import(dir.path(), &missing, "", None, false, &store)
            .unwrap_err()
            .code,
        "keys.imported_unreadable"
    );
    assert_eq!(list(dir.path(), &store).unwrap().len(), 1);
}

#[test]
fn 改名删除和认出导入的私钥() {
    let (src, dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = MemoryCredentialStore::default();
    let source = write_source(src.path(), "k", &new_key(), None);
    let imported = import(dir.path(), &source, "", None, false, &store).unwrap();

    assert_eq!(
        id_of_path(dir.path(), &imported.path).as_deref(),
        Some(imported.id.as_str())
    );
    assert_eq!(id_of_path(dir.path(), &source), None);
    assert_eq!(id_of_path(dir.path(), &dir.path().join("../k.key")), None);
    assert!(is_credential_id(&credential_id(&imported.id)));
    assert!(!is_credential_id("pierlink/local/dedicated-key-passphrase"));
    assert!(!is_credential_id("pierlink/imported-key/../x"));

    let renamed = rename(dir.path(), &imported.id, "  新名字 ", &store).unwrap();
    assert_eq!(renamed.name, "新名字");
    assert_eq!(
        rename(dir.path(), &imported.id, "  ", &store)
            .unwrap_err()
            .code,
        "keys.imported_name_invalid"
    );
    assert_eq!(
        rename(dir.path(), "nonexistent1", "x", &store)
            .unwrap_err()
            .code,
        "keys.imported_not_found"
    );
    // 随机口令不能忘掉，不然私钥就打不开了
    assert_eq!(
        set_passphrase(dir.path(), &imported.id, None, &store)
            .unwrap_err()
            .code,
        "keys.imported_random_passphrase"
    );

    remove(dir.path(), &imported.id, &store).unwrap();
    assert!(list(dir.path(), &store).unwrap().is_empty());
    assert!(!imported.path.exists());
    assert!(store.get(&credential_id(&imported.id)).unwrap().is_none());
}

#[test]
fn 随机口令丢了时再导入同一把_原地重新加密() {
    let (src, dir) = (
        crate::test_support::tempdir(),
        crate::test_support::tempdir(),
    );
    let store = MemoryCredentialStore::default();
    let key = new_key();
    let source = write_source(src.path(), "k", &key, None);
    let first = import(dir.path(), &source, "", None, false, &store).unwrap();
    store.delete(&credential_id(&first.id)).unwrap();
    assert!(!list(dir.path(), &store).unwrap()[0].passphrase_saved);
    let again = import(dir.path(), &source, "", None, false, &store).unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(again.path, first.path);
    assert!(again.passphrase_saved);
    let stored = PrivateKey::from_openssh(fs::read(&again.path).unwrap()).unwrap();
    let secret = saved_passphrase(dir.path(), &again.path, &store).unwrap();
    assert_eq!(
        fingerprint(&stored.decrypt(secret.as_bytes()).unwrap()),
        fingerprint(&key)
    );
}

/// 用系统的 ssh-keygen 生成别的格式的私钥（没有 ssh-keygen 时返回 None，测试跳过）
fn keygen(dir: &Path, name: &str, args: &[&str]) -> Option<PathBuf> {
    let path = dir.join(name);
    let ok = std::process::Command::new("ssh-keygen")
        .args(["-q", "-C", "test"])
        .args(args)
        .arg("-f")
        .arg(&path)
        .status()
        .ok()?
        .success();
    ok.then_some(path)
}

#[test]
fn 导入_pem_和_pkcs8_格式_有口令和没口令() {
    let (src, dir) = (
        crate::test_support::tempdir(),
        crate::test_support::tempdir(),
    );
    let store = MemoryCredentialStore::default();
    let cases: [(&str, &[&str], Option<&str>); 3] = [
        (
            "rsa-pem",
            &["-t", "rsa", "-b", "2048", "-m", "PEM", "-N", ""],
            None,
        ),
        (
            "rsa-pem-pw",
            &["-t", "rsa", "-b", "2048", "-m", "PEM", "-N", "pem pw"],
            Some("pem pw"),
        ),
        (
            "rsa-pkcs8-pw",
            &["-t", "rsa", "-b", "2048", "-m", "PKCS8", "-N", "p8 pw"],
            Some("p8 pw"),
        ),
    ];
    for (name, args, pw) in cases {
        let Some(source) = keygen(src.path(), name, args) else {
            eprintln!("没有 ssh-keygen，跳过");
            return;
        };
        if pw.is_some() {
            assert_eq!(
                import(dir.path(), &source, "", None, false, &store)
                    .unwrap_err()
                    .code,
                "keys.imported_needs_passphrase",
                "{name}"
            );
        }
        let k = import(dir.path(), &source, "", pw, true, &store)
            .unwrap_or_else(|e| panic!("{name}: {}", e.code));
        assert_eq!(k.algorithm, "ssh-rsa", "{name}");
        // 存下的统一是加密的 OpenSSH 格式，用存着的口令解得开
        let stored = PrivateKey::from_openssh(fs::read(&k.path).unwrap()).unwrap();
        let secret = saved_passphrase(dir.path(), &k.path, &store).unwrap();
        stored
            .decrypt(secret.as_bytes())
            .unwrap_or_else(|_| panic!("{name}"));
    }
}

#[test]
fn 老格式的加密私钥读不了时单独报出来() {
    let (src, dir) = (
        crate::test_support::tempdir(),
        crate::test_support::tempdir(),
    );
    let store = MemoryCredentialStore::default();
    // 带口令的 EC 私钥存成老式 PEM：解码器读不了，要先用 ssh-keygen -p 转成新格式
    let Some(source) = keygen(
        src.path(),
        "ec-pem-pw",
        &["-t", "ecdsa", "-m", "PEM", "-N", "pem pw"],
    ) else {
        eprintln!("没有 ssh-keygen，跳过");
        return;
    };
    for pw in [None, Some("pem pw")] {
        assert_eq!(
            import(dir.path(), &source, "", pw, true, &store)
                .unwrap_err()
                .code,
            "keys.imported_old_format"
        );
    }
    assert!(list(dir.path(), &store).unwrap().is_empty());
    // 不带口令的老式 EC 私钥能读
    let Some(plain) = keygen(
        src.path(),
        "ec-pem",
        &["-t", "ecdsa", "-m", "PEM", "-N", ""],
    ) else {
        return;
    };
    let k = import(dir.path(), &plain, "", None, false, &store).unwrap();
    assert_eq!(k.algorithm, "ecdsa-sha2-nistp256");
}
