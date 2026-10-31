//! 沙箱外运行：cargo test --offline -p pierlink-core --test known_hosts -- --ignored
//! 所有 SSH 命令禁用用户配置和系统记录，只使用本次测试的临时 known_hosts。
#![cfg(unix)]
#[path = "../src/test_support.rs"]
mod test_support;

use pierlink_core::ssh::known_hosts::{HostKeyStatus, KnownHosts};
use pierlink_ssh_engine::{
    Authentication, ConnectionOptions, Engine, Error, HostKey, HostKeyCheck,
};
use std::{
    fs,
    net::{TcpListener, TcpStream},
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::{Arc, Mutex, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

struct Server {
    child: Child,
    root: tempfile::TempDir,
    port: u16,
    user: String,
}

impl Server {
    fn new() -> Self {
        let root = test_support::tempdir();
        for (name, algorithm) in [
            ("host", "ed25519"),
            ("ec", "ecdsa"),
            ("client", "ed25519"),
            ("changed", "ed25519"),
        ] {
            assert!(
                Command::new("/usr/bin/ssh-keygen")
                    .args(["-q", "-t", algorithm, "-N", "", "-f"])
                    .arg(root.path().join(name))
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let config = format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nStrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nAllowTcpForwarding yes\nLogLevel ERROR\nSetEnv HISTFILE=/dev/null\n",
            root.path().join("host").display(),
            root.path().join("ec").display(),
            root.path().join("pid").display(),
            root.path().join("client.pub").display()
        );
        fs::write(root.path().join("sshd_config"), config).unwrap();
        let child = Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(root.path().join("sshd_config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(root.path().join("sshd.log")).unwrap())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            root,
            port,
            user,
        };
        let start = Instant::now();
        loop {
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "sshd 启动失败：{}",
                fs::read_to_string(server.root.path().join("sshd.log")).unwrap()
            );
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return server;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "等待 sshd 超时");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn auth(&self) -> Authentication {
        Authentication::PrivateKey {
            path: self.root.path().join("client"),
            passphrase: None,
        }
    }

    fn options(
        &self,
        hosts: &KnownHosts,
        observed: Arc<Mutex<Vec<HostKey>>>,
        confirm: bool,
        alias: Option<&str>,
    ) -> ConnectionOptions {
        let preferred = hosts
            .preferred_algorithms("127.0.0.1", self.port, alias)
            .unwrap();
        let hosts = hosts.clone();
        let alias = alias.map(str::to_owned);
        let mut options = ConnectionOptions::new(
            "127.0.0.1",
            self.port,
            &self.user,
            HostKeyCheck::Callback(Arc::new(move |key| {
                observed.lock().unwrap().push(key.clone());
                hosts
                    .verify(&key, alias.as_deref(), &AtomicBool::new(false), |_, _| {
                        assert!(confirm, "已知主机不能再次询问");
                        Ok(true)
                    })
                    .unwrap_or(false)
            })),
        );
        options.preferred_host_key_algorithms = preferred;
        options
    }

    fn ssh(&self, hosts: &Path, strict: &str, extra: &[&str]) -> Output {
        Command::new("/usr/bin/ssh")
            .args([
                "-F",
                "none",
                "-o",
                "GlobalKnownHostsFile=none",
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "HashKnownHosts=no",
                "-o",
                "UpdateHostKeys=no",
                "-o",
                "ConnectTimeout=5",
                "-o",
            ])
            .arg(format!("UserKnownHostsFile={}", hosts.display()))
            .arg("-o")
            .arg(format!("StrictHostKeyChecking={strict}"))
            .args(extra)
            .arg("-i")
            .arg(self.root.path().join("client"))
            .arg("-p")
            .arg(self.port.to_string())
            .arg("-l")
            .arg(&self.user)
            .args(["127.0.0.1", "true"])
            .output()
            .unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // 会话均在断言前等待完成；失败时也结束本次测试独立进程组并回收父进程。
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "系统 ssh 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sample_key(host: &str) -> HostKey {
    let public_key = "AAAAC3NzaC1lZDI1NTE5AAAAILagOJFgwaMNhBWQINinKOXmqS4Gh5NgxgriXwdOoINJ";
    HostKey {
        host: host.into(),
        port: 2222,
        algorithm: "ssh-ed25519".into(),
        public_key: public_key.into(),
        fingerprint: "SHA256:ldyiXa1JQakitNU5tErauu8DvWQ1dZ7aXu+rm7KQuog".into(),
    }
}

#[test]
fn 无网络用系统工具查找内置记录并哈希读回() {
    let root = test_support::tempdir();
    let path = root.path().join("known_hosts");
    let hosts = KnownHosts::new(path.clone(), None);
    for alias in [None, Some("TargetAlias")] {
        let key = sample_key("localhost");
        assert!(
            hosts
                .verify(&key, alias, &AtomicBool::new(false), |_, _| Ok(true))
                .unwrap()
        );
        let lookup = alias.unwrap_or("[localhost]:2222");
        let found = Command::new("/usr/bin/ssh-keygen")
            .args(["-F", lookup, "-f"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(found.status.success(), "系统 ssh-keygen 应认得追加记录");
        assert!(
            String::from_utf8(found.stdout)
                .unwrap()
                .contains(&key.public_key)
        );
    }
    assert!(
        Command::new("/usr/bin/ssh-keygen")
            .args(["-H", "-f"])
            .arg(&path)
            .output()
            .unwrap()
            .status
            .success()
    );
    for alias in [None, Some("TargetAlias")] {
        assert_eq!(
            hosts.check(&sample_key("localhost"), alias).unwrap().status,
            HostKeyStatus::Trusted
        );
    }
}

#[test]
fn 并发子进程追加() {
    // 由父测试指定共享临时文件；单独运行时不接触用户文件或创建其他目录。
    let Some(path) = std::env::var_os("PIERLINK_TEST_KNOWN_HOSTS_PATH") else {
        return;
    };
    let name = std::env::var("PIERLINK_TEST_KNOWN_HOSTS_WRITER").unwrap();
    let hosts = KnownHosts::new(path.into(), None);
    for host in ["shared", name.as_str()] {
        assert!(
            hosts
                .verify(&sample_key(host), None, &AtomicBool::new(false), |_, _| Ok(
                    true
                ))
                .unwrap()
        );
    }
}

#[test]
fn 跨进程追加完整且不重复() {
    struct Writer(Child);
    impl Drop for Writer {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = test_support::tempdir();
    let path = root.path().join("known_hosts");
    fs::write(&path, "# 无末尾换行").unwrap();
    let mut children = Vec::new();
    for index in 0..8 {
        children.push(Writer(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "并发子进程追加"])
                .env("PIERLINK_TEST_KNOWN_HOSTS_PATH", &path)
                .env(
                    "PIERLINK_TEST_KNOWN_HOSTS_WRITER",
                    format!("writer-{index}"),
                )
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        ));
    }
    for child in &mut children {
        assert!(child.0.wait().unwrap().success());
    }
    assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 10);
    let hosts = KnownHosts::new(path, None);
    for host in
        std::iter::once("shared".to_owned()).chain((0..8).map(|index| format!("writer-{index}")))
    {
        let report = hosts.check(&sample_key(&host), None).unwrap();
        assert_eq!(report.status, HostKeyStatus::Trusted);
        assert_eq!(report.invalid_lines, 0);
    }
}

#[test]
#[ignore = "需要沙箱外以普通用户监听本机随机高位端口"]
fn 系统写入内置识别_已知算法优先_哈希后仍识别() {
    let server = Server::new();
    let path = server.root.path().join("known_hosts");
    let hosts = KnownHosts::new(path.clone(), None);
    success(server.ssh(
        &path,
        "accept-new",
        &["-o", "HostKeyAlgorithms=ecdsa-sha2-nistp256"],
    ));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let engine = Engine::new().unwrap();
    engine
        .connect(
            server.options(&hosts, observed.clone(), false, None),
            server.auth(),
        )
        .unwrap()
        .disconnect()
        .unwrap();
    let key = observed.lock().unwrap()[0].clone();
    assert_eq!(key.algorithm, "ecdsa-sha2-nistp256");
    assert_eq!(
        hosts.check(&key, None).unwrap().status,
        HostKeyStatus::Trusted
    );
    assert!(
        Command::new("/usr/bin/ssh-keygen")
            .args(["-H", "-f"])
            .arg(&path)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(fs::read_to_string(&path).unwrap().starts_with("|1|"));
    assert_eq!(
        hosts.check(&key, None).unwrap().status,
        HostKeyStatus::Trusted
    );
    engine
        .connect(server.options(&hosts, observed, false, None), server.auth())
        .unwrap()
        .disconnect()
        .unwrap();
}

#[test]
#[ignore = "需要沙箱外以普通用户监听本机随机高位端口"]
fn 内置写入系统严格识别_变化和撤销两边都拒绝() {
    let server = Server::new();
    let path = server.root.path().join("known_hosts");
    let hosts = KnownHosts::new(path.clone(), None);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let engine = Engine::new().unwrap();
    engine
        .connect(
            server.options(&hosts, observed.clone(), true, None),
            server.auth(),
        )
        .unwrap()
        .disconnect()
        .unwrap();
    let key = observed.lock().unwrap()[0].clone();
    success(server.ssh(&path, "yes", &[]));
    let saved = fs::read_to_string(&path).unwrap();
    // 使用实际协商类型的另一把公钥，避免其他类型被当成首次确认。
    let changed = if key.algorithm == "ssh-ed25519" {
        "changed.pub"
    } else {
        "host.pub"
    };
    assert_eq!(key.algorithm, "ssh-ed25519");
    let wrong = fs::read_to_string(server.root.path().join(changed)).unwrap();
    fs::write(&path, format!("[127.0.0.1]:{} {wrong}", server.port)).unwrap();
    for revoked in [false, true] {
        if revoked {
            fs::write(&path, format!("@revoked {saved}")).unwrap();
        }
        assert_eq!(
            hosts.check(&key, None).unwrap_err().code,
            "ssh.host_key_changed"
        );
        assert!(
            !server
                .ssh(&path, "yes", &["-o", "HostKeyAlgorithms=ssh-ed25519"])
                .status
                .success()
        );
        assert!(matches!(
            engine.connect(
                server.options(&hosts, observed.clone(), false, None),
                server.auth()
            ),
            Err(Error::HostKeyRejected)
        ));
    }
}

#[test]
#[ignore = "需要沙箱外以普通用户监听本机随机高位端口"]
fn 别名代替完整地址且跳板每一级使用自己的端口和公钥() {
    let jump = Server::new();
    let target = Server::new();
    let path = jump.root.path().join("known_hosts");
    let hosts = KnownHosts::new(path.clone(), None);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let engine = Engine::new().unwrap();
    let parent = engine
        .connect(
            jump.options(&hosts, observed.clone(), true, None),
            jump.auth(),
        )
        .unwrap();
    let child = engine
        .connect_via(
            &parent,
            target.options(&hosts, observed.clone(), true, Some("TargetAlias")),
            target.auth(),
        )
        .unwrap();
    child.disconnect().unwrap();
    parent.disconnect().unwrap();
    let keys = observed.lock().unwrap();
    assert_eq!(
        keys.iter().map(|key| key.port).collect::<Vec<_>>(),
        [jump.port, target.port]
    );
    assert_ne!(keys[0].public_key, keys[1].public_key);
    assert_eq!(
        hosts.check(&keys[0], None).unwrap().status,
        HostKeyStatus::Trusted
    );
    assert_eq!(
        hosts.check(&keys[1], Some("TargetAlias")).unwrap().status,
        HostKeyStatus::Trusted
    );
    assert!(matches!(
        hosts.check(&keys[1], None).unwrap().status,
        HostKeyStatus::New { .. }
    ));
    success(jump.ssh(&path, "yes", &[]));
    success(target.ssh(&path, "yes", &["-o", "HostKeyAlias=TargetAlias"]));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("\ntargetalias ssh-ed25519 ")
    );
}
