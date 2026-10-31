#![allow(dead_code)]
use pierlink_ssh_engine::{Authentication, Connection, ConnectionOptions, Engine, HostKeyCheck};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

static ROOT: OnceLock<Mutex<Option<tempfile::TempDir>>> = OnceLock::new();
unsafe extern "C" {
    fn atexit(callback: extern "C" fn()) -> i32;
}
extern "C" fn cleanup() {
    if let Some(root) = ROOT.get()
        && let Ok(mut root) = root.lock()
    {
        drop(root.take());
    }
}

pub fn tempdir() -> tempfile::TempDir {
    let root = ROOT.get_or_init(|| {
        // 设了 PIERLINK_TEST_TMP（如放到数据盘）就用它，否则用本 crate 目录
        let configured = std::env::var_os("PIERLINK_TEST_TMP").map(std::path::PathBuf::from);
        let base = match configured.as_deref().filter(|p| p.is_dir()) {
            Some(dir) => dir,
            None => Path::new(env!("CARGO_MANIFEST_DIR")),
        };
        let root = tempfile::Builder::new()
            .prefix("pl-engine-")
            .tempdir_in(base)
            .unwrap();
        assert_eq!(unsafe { atexit(cleanup) }, 0);
        Mutex::new(Some(root))
    });
    tempfile::Builder::new()
        .prefix("t")
        .tempdir_in(root.lock().unwrap().as_ref().unwrap().path())
        .unwrap()
}

pub fn required(names: &[&str]) -> Option<Vec<String>> {
    let mut values = Vec::new();
    for name in names {
        match std::env::var(name) {
            Ok(value) if !value.is_empty() => values.push(value),
            _ => {
                eprintln!("跳过：未设置 {name}");
                return None;
            }
        }
    }
    Some(values)
}

pub fn options(endpoint: &str, user: &str) -> ConnectionOptions {
    let (host, port) = endpoint.rsplit_once(':').expect("测试地址必须为 host:port");
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let mut options = ConnectionOptions::new(
        host,
        port.parse().unwrap(),
        user,
        HostKeyCheck::Callback(Arc::new(|_| true)),
    );
    options.io_timeout = Duration::from_secs(300);
    options
}

pub fn key_connection(endpoint_var: &str) -> Option<(Engine, Connection)> {
    let values = required(&[
        endpoint_var,
        "PIERLINK_ENGINE_KEY_USER",
        "PIERLINK_ENGINE_KEY",
    ])?;
    let engine = Engine::new().unwrap();
    let connection = engine
        .connect(options(&values[0], &values[1]), key_auth(&values[2]))
        .unwrap();
    Some((engine, connection))
}

pub fn key_auth(path: impl Into<PathBuf>) -> Authentication {
    Authentication::PrivateKey {
        path: path.into(),
        passphrase: None,
    }
}

pub fn command(connection: &Connection, text: &str) -> Vec<u8> {
    use std::io::Read;
    let mut session = connection.exec(text, &[]).unwrap();
    session.stdin.shutdown().unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    std::thread::scope(|scope| {
        scope.spawn(|| session.stderr.read_to_end(&mut err).unwrap());
        session.stdout.read_to_end(&mut out).unwrap();
    });
    assert_eq!(session.wait().unwrap(), Some(0));
    out
}

#[cfg(unix)]
pub fn keygen(path: &Path, algorithm: &str, passphrase: &str) {
    let status = std::process::Command::new("/usr/bin/ssh-keygen")
        .args(["-q", "-t", algorithm, "-N", passphrase, "-f"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "生成测试密钥失败");
}

#[cfg(unix)]
pub struct Server {
    pub root: tempfile::TempDir,
    pub port: u16,
    pub user: String,
    child: Option<std::process::Child>,
}

#[cfg(unix)]
impl Server {
    pub fn new(extra: &str) -> Self {
        Self::with_root(tempdir(), extra, false)
    }

    /// 使用调用方共用临时根目录；isolated 将远端 HOME 和授权文件限制在本测试目录。
    pub fn with_root(root: tempfile::TempDir, extra: &str, isolated: bool) -> Self {
        use std::{
            fs,
            net::{TcpListener, TcpStream},
            os::unix::process::CommandExt,
            process::{Command, Stdio},
            thread,
            time::Instant,
        };
        keygen(&root.path().join("host"), "ed25519", "");
        keygen(&root.path().join("client"), "ed25519", "");
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let isolation = if isolated {
            let home = root.path().join("h");
            fs::create_dir(&home).unwrap();
            fs::create_dir(root.path().join("b")).unwrap();
            fs::create_dir(root.path().join("r")).unwrap();
            fs::write(root.path().join("b/systemctl"), "#!/bin/sh\nexit 1\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                root.path().join("b/systemctl"),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            // 临时目录由测试控制，仍对路径使用 POSIX 单引号转义。
            let quote =
                |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
            format!(
                "ForceCommand /usr/bin/env HOME={} PATH={}:/usr/bin:/bin XDG_RUNTIME_DIR={} /bin/sh -c \"$SSH_ORIGINAL_COMMAND\"\n",
                quote(&home),
                quote(&root.path().join("b")),
                quote(&root.path().join("r"))
            )
        } else {
            String::new()
        };
        let authorized = if isolated {
            format!(" {}", root.path().join("h/.ssh/authorized_keys").display())
        } else {
            String::new()
        };
        // 测试 sshd 以当前用户登录、用真实的家目录：交互 shell 退出时会把测试敲的命令写进用户的 ~/.bash_history，所以把 HISTFILE 设成 /dev/null
        let config = format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}{authorized}\nStrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPermitRootLogin yes\nAcceptEnv PIERLINK_TEST\nSubsystem sftp internal-sftp\nLogLevel ERROR\nSetEnv HISTFILE=/dev/null\n{isolation}{extra}\n",
            root.path().join("host").display(),
            root.path().join("pid").display(),
            root.path().join("client.pub").display()
        );
        fs::write(root.path().join("sshd_config"), config).unwrap();
        let log = fs::File::create(root.path().join("sshd.log")).unwrap();
        let child = Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(root.path().join("sshd_config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .process_group(0)
            .spawn()
            .unwrap();
        let mut server = Self {
            root,
            port,
            user,
            child: Some(child),
        };
        let start = Instant::now();
        loop {
            assert!(
                server.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "sshd 启动失败：{}",
                fs::read_to_string(server.root.path().join("sshd.log")).unwrap()
            );
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "等待 sshd 超时");
            thread::sleep(Duration::from_millis(20));
        }
        server
    }

    pub fn options(&self) -> ConnectionOptions {
        let output = std::process::Command::new("/usr/bin/ssh-keygen")
            .args(["-lf"])
            .arg(self.root.path().join("host.pub"))
            .args(["-E", "sha256"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        let fingerprint = output.split_whitespace().nth(1).unwrap().to_owned();
        ConnectionOptions::new(
            "127.0.0.1",
            self.port,
            &self.user,
            HostKeyCheck::Fingerprint(fingerprint),
        )
    }
    pub fn auth(&self) -> Authentication {
        key_auth(self.root.path().join("client"))
    }
    pub fn connect(&self, engine: &Engine) -> Connection {
        engine.connect(self.options(), self.auth()).unwrap()
    }
}

#[cfg(unix)]
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            stop_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(unix)]
fn stop_tree(pid: u32) {
    use std::{
        fs,
        process::{Command, Stdio},
    };
    let signal = |pid: u32, sig: &str| {
        let _ = Command::new("/bin/kill")
            .args([sig, &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    };
    signal(pid, "-STOP");
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(child) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            if stat
                .rsplit_once(") ")
                .and_then(|(_, rest)| rest.split_whitespace().nth(1))
                .and_then(|ppid| ppid.parse::<u32>().ok())
                == Some(pid)
            {
                stop_tree(child);
            }
        }
    }
    signal(pid, "-KILL");
}
