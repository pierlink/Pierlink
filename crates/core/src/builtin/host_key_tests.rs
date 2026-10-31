//! 在独立子进程中使用临时用户目录，验证产品登录路径读取 known_hosts。
use super::*;
use crate::ssh::known_hosts::KnownHosts;
use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer};
use std::{fs, process::Command};

struct TrustedHosts {
    observed: Mutex<Vec<(String, HostKey)>>,
}

impl ConnectionCallbacks for TrustedHosts {
    fn check_host_key(
        &self,
        id: &str,
        key: &HostKey,
        cancelled: &AtomicBool,
    ) -> Result<bool, AppError> {
        self.observed.lock().unwrap().push((id.into(), key.clone()));
        KnownHosts::for_user()?.verify(key, None, cancelled, |_, _| {
            Err(AppError::new("ssh.host_key_unknown"))
        })
    }
    fn password(&self, _: &str, _: &AtomicBool) -> Result<Option<Zeroizing<String>>, AppError> {
        panic!("测试密码已在库中")
    }
    fn passphrase(
        &self,
        _: &str,
        _: &Path,
        _: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        panic!("密码认证不读取私钥")
    }
    fn dedicated_key(
        &self,
        _: &str,
        _: &AtomicBool,
    ) -> Result<Option<DedicatedCredential>, AppError> {
        panic!("密码认证不读取专用密钥")
    }
    fn keyboard_interactive(
        &self,
        _: &str,
        _: Challenge,
        _: &AtomicBool,
    ) -> Result<Option<Vec<Zeroizing<String>>>, AppError> {
        panic!("测试服务器不使用验证码")
    }
}

#[test]
#[cfg(unix)]
fn 内置登录按用户主机记录协商各级密钥并在读取失败时回退() {
    let marker = "PIERLINK_TEST_HOST_KEYS_HOME";
    if let Some(root) = std::env::var_os(marker) {
        exercise(Path::new(&root));
        return;
    }
    let root = crate::test_support::tempdir();
    fs::create_dir(root.path().join(".ssh")).unwrap();
    for algorithm in ["ed25519", "rsa", "ecdsa"] {
        assert!(
            Command::new("/usr/bin/ssh-keygen")
                .args(["-q", "-t", algorithm, "-N", "", "-f"])
                .arg(root.path().join(algorithm))
                .status()
                .unwrap()
                .success()
        );
    }
    // 仅子进程替换用户目录，避免并行测试触碰真实记录或依赖全局环境修改。
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "builtin::tests::host_keys::内置登录按用户主机记录协商各级密钥并在读取失败时回退",
            "--nocapture",
        ])
        .env(marker, root.path())
        .env("HOME", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn exercise(root: &Path) {
    let engine = Engine::new().unwrap();
    let keys: Vec<_> = ["ed25519", "rsa", "ecdsa"]
        .map(|name| ssh_key::PrivateKey::read_openssh_file(root.join(name)).unwrap())
        .into();
    let peer = LoginPeer::new(LoginMode::Password).with_host_keys(keys.clone());
    let path = root.join(".ssh/known_hosts");
    for key in &keys {
        for alias in [None, Some("KnownAlias")] {
            let mut f = Fixture::new(AuthMethod::Password);
            f.server.host = Some("target.test".into());
            f.server.port = Some(2222);
            f.server.host_key_alias = alias.map(str::to_owned);
            f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
            let lookup = alias.unwrap_or("[target.test]:2222").to_ascii_lowercase();
            fs::write(
                &path,
                format!("{lookup} {}\n", key.public_key().to_openssh().unwrap()),
            )
            .unwrap();
            let callbacks = Arc::new(TrustedHosts {
                observed: Mutex::new(Vec::new()),
            });
            let mut request = f.request(false);
            request.callbacks = callbacks.clone();
            let connected = peer.clone();
            let executor = engine.clone();
            *f.manager.pool.connector.lock().unwrap() =
                Some(Arc::new(move |_, _, options, auth| {
                    connected.connect(&executor, options, auth)
                }));
            let connection = f.manager.get_connection(request).unwrap();
            connection.ping().unwrap();
            let observed = callbacks.observed.lock().unwrap();
            assert_eq!(observed.len(), 1);
            assert_eq!(observed[0].1.algorithm, key.algorithm().to_string());
            f.manager.close_all().unwrap();
        }
    }

    let f = Fixture::new(AuthMethod::Password);
    let mut chain = Vec::new();
    let mut records = String::new();
    for (index, key) in keys.iter().enumerate() {
        let mut server = f.server.clone();
        server.id = format!("hop{index}");
        server.host = Some(format!("hop{index}.test"));
        server.port = Some(2222 + index as u16);
        server.host_key_alias = (index == 1).then(|| "MiddleAlias".into());
        server.jump_server_id = chain.last().map(|s: &Server| s.id.clone());
        let lookup = server
            .host_key_alias
            .clone()
            .unwrap_or_else(|| format!("[hop{index}.test]:{}", server.port.unwrap()))
            .to_ascii_lowercase();
        records.push_str(&format!(
            "{lookup} {}\n",
            key.public_key().to_openssh().unwrap()
        ));
        f.vault
            .set(&server.id, Zeroizing::new("测试密码".into()))
            .unwrap();
        chain.push(server);
    }
    fs::write(&path, records).unwrap();
    let target = chain.last().unwrap().clone();
    f.manager.set_server_resolver(Arc::new(move |id| {
        Ok(chain.iter().find(|s| s.id == id).cloned())
    }));
    let executor = engine.clone();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
        peer.connect(&executor, options, auth)
    }));
    let callbacks = Arc::new(TrustedHosts {
        observed: Mutex::new(Vec::new()),
    });
    let mut request = f.request(false);
    request.server = &target;
    request.callbacks = callbacks.clone();
    f.manager.get_connection(request).unwrap().ping().unwrap();
    let observed = callbacks.observed.lock().unwrap();
    assert_eq!(observed.len(), 3);
    for (index, (id, key)) in observed.iter().enumerate() {
        assert_eq!(id, &format!("hop{index}"));
        assert_eq!(key.algorithm, keys[index].algorithm().to_string());
    }
    f.manager.close_all().unwrap();

    fs::rename(&path, root.join("saved_hosts")).unwrap();
    fs::create_dir(&path).unwrap();
    let f = Fixture::new(AuthMethod::Password);
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    let peer = LoginPeer::new(LoginMode::Password);
    let connection = f
        .manager
        .connect_with(f.request(false), options(), |opts, auth| {
            assert!(opts.preferred_host_key_algorithms.is_empty());
            peer.connect(&engine, opts, auth)
        })
        .unwrap();
    connection.ping().unwrap();
    assert_eq!(f.callbacks.host_keys.lock().unwrap().len(), 1);
}
