//! 真实内存 SSH 和 agent 协议验证登录上限、身份优先级与重复公钥跳过。
use crate::{
    Authentication, ConfiguredAuthentication, ConnectionOptions, Engine, Error, HostKeyCheck,
    PrivateKeyFile,
    test_support::{LoginMode, LoginPeer},
};
use russh::keys::{PrivateKey, ssh_key::private::Ed25519Keypair};
use std::{path::PathBuf, sync::Arc};

use crate::test_files as support;

fn keys(root: &std::path::Path, count: u8) -> Vec<PathBuf> {
    (1..=count)
        .map(|seed| {
            let key: PrivateKey = Ed25519Keypair::from_seed(&[seed; 32]).into();
            let path = root.join(format!("key-{seed}"));
            key.write_openssh_file(&path, Default::default()).unwrap();
            path
        })
        .collect()
}

fn options() -> ConnectionOptions {
    ConnectionOptions::new(
        "内存",
        22,
        "测试",
        HostKeyCheck::Callback(Arc::new(|_| true)),
    )
}

fn auth(paths: &[PathBuf], identities_only: bool) -> Authentication {
    Authentication::Configured(ConfiguredAuthentication {
        agent: true,
        agent_identities_only: identities_only,
        private_keys: paths
            .iter()
            .map(|path| PrivateKeyFile {
                path: path.clone(),
                passphrase: None,
            })
            .collect(),
        skip_unusable_keys: true,
        password: None,
        answers: Arc::new(|_| panic!("公钥登录不应询问密码")),
    })
}

#[test]
fn 认证上限按断开原因和已拒公钥数判断() {
    let root = support::tempdir();
    let paths = keys(root.path(), 8);
    let engine = Engine::new().unwrap();
    for (limit, reason, expected) in [
        (
            3,
            Some((2, "Too many authentication failures".into())),
            Error::TooManyAuthAttempts,
        ),
        (
            3,
            Some((2, "TOO MANY AUTHENTICATION FAILURES".into())),
            Error::TooManyAuthAttempts,
        ),
        (3, Some((14, "其他说明".into())), Error::TooManyAuthAttempts),
        (6, None, Error::TooManyAuthAttempts),
        (2, None, Error::Disconnected),
        (6, Some((11, "登录时限到期".into())), Error::Disconnected),
        (
            6,
            Some((11, "Timeout before authentication".into())),
            Error::Disconnected,
        ),
        // 别的实现：说法不同（Dropbear），或者没说清原因但已有多次公钥被拒
        (
            3,
            Some((11, "Max auth tries reached - user 'x'".into())),
            Error::TooManyAuthAttempts,
        ),
        (6, Some((11, "Bye".into())), Error::TooManyAuthAttempts),
        (2, Some((11, "Bye".into())), Error::Disconnected),
    ] {
        let peer = LoginPeer::new(LoginMode::Password).with_auth_limit(limit, reason);
        let error = peer
            .connect_with_agent(
                &engine,
                options(),
                auth(&[], false),
                &paths[..if limit == 3 { 5 } else { 8 }],
            )
            .err()
            .unwrap();
        assert_eq!(error, expected);
        assert_eq!(peer.public_key_requests(), limit);
    }
}

#[test]
fn 指定身份优先且按文件顺序排列并支持仅公钥文件() {
    let root = support::tempdir();
    let paths = keys(root.path(), 8);
    let engine = Engine::new().unwrap();
    for public_only in [false, true] {
        let preferred = if public_only {
            let path = root.path().join("指定身份");
            let key = russh::keys::load_secret_key(&paths[7], None).unwrap();
            key.public_key()
                .write_openssh_file(path.with_extension("pub"))
                .unwrap();
            path
        } else {
            paths[7].clone()
        };
        for only in [false, true] {
            let peer = LoginPeer::key(&paths[7], None)
                .unwrap()
                .with_auth_limit(3, Some((2, "Too many authentication failures".into())));
            let connection = peer
                .connect_with_agent(
                    &engine,
                    options(),
                    auth(&[preferred.clone(), paths[0].clone()], only),
                    &paths,
                )
                .unwrap();
            assert_eq!(peer.public_key_requests(), 1);
            connection.disconnect().unwrap();
        }
    }
    let peer = LoginPeer::key(&paths[7], None)
        .unwrap()
        .with_auth_limit(3, Some((2, "Too many authentication failures".into())));
    let connection = peer
        .connect_with_agent(
            &engine,
            options(),
            auth(&[paths[5].clone(), paths[7].clone()], false),
            &paths,
        )
        .unwrap();
    assert_eq!(peer.public_key_requests(), 2);
    connection.disconnect().unwrap();
    let peer = LoginPeer::key(&paths[7], None).unwrap();
    assert_eq!(
        peer.connect_with_agent(&engine, options(), auth(&[paths[0].clone()], true), &paths)
            .err()
            .unwrap(),
        Error::Authentication
    );
    assert_eq!(peer.public_key_requests(), 1);
}

#[test]
fn 已拒agent公钥不会作为文件再次认证或询问口令() {
    let root = support::tempdir();
    let paths = keys(root.path(), 2);
    let engine = Engine::new().unwrap();
    let peer = LoginPeer::new(LoginMode::Password);
    let error = peer
        .connect_with_agent(&engine, options(), auth(&paths, false), &paths[..1])
        .err()
        .unwrap();
    assert_eq!(error, Error::Authentication);
    // 第一把在 agent 中被拒后跳过文件；第二把仍从文件发送。
    assert_eq!(peer.public_key_requests(), 2);
    #[cfg(unix)]
    {
        let encrypted = root.path().join("加密副本");
        std::fs::copy(&paths[0], &encrypted).unwrap();
        assert!(
            std::process::Command::new("/usr/bin/ssh-keygen")
                .args(["-p", "-P", "", "-N", "测试口令", "-f"])
                .arg(&encrypted)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        let Authentication::Configured(mut config) = auth(&[encrypted], false) else {
            panic!()
        };
        config.private_keys[0].passphrase = Some(Arc::new(|| panic!("已拒身份不应再询问口令")));
        let peer = LoginPeer::new(LoginMode::Password);
        assert_eq!(
            peer.connect_with_agent(
                &engine,
                options(),
                Authentication::Configured(config),
                &paths[..1]
            )
            .err()
            .unwrap(),
            Error::Authentication
        );
        assert_eq!(peer.public_key_requests(), 1);
    }
}

#[test]
#[cfg(unix)]
#[ignore = "需要监听本机端口"]
fn 本机sshd登录上限区分于网络断开() {
    let server = support::Server::new("MaxAuthTries 2");
    let paths = keys(server.root.path(), 3);
    let engine = Engine::new().unwrap();
    let keys = paths
        .iter()
        .map(|path| russh::keys::load_secret_key(path, None).unwrap())
        .collect();
    let (client, stream) = tokio::io::duplex(64 * 1024);
    engine.executor.handle().unwrap().spawn(async move {
        let _ = crate::test_support::memory_agent(stream, keys).await;
    });
    let Authentication::Configured(config) = auth(&[], false) else {
        panic!()
    };
    let error = engine
        .connect(
            support::options(&format!("127.0.0.1:{}", server.port), &server.user),
            Authentication::MemoryAgent(config, Arc::new(std::sync::Mutex::new(Some(client)))),
        )
        .err()
        .unwrap();
    assert_eq!(error, Error::TooManyAuthAttempts);
}
