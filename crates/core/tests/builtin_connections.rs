//! 本机 sshd 集成测试；在允许监听端口的环境显式执行 ignored 测试。
#![cfg(unix)]
#[path = "../../ssh-engine/tests/support/mod.rs"]
mod support;
#[path = "builtin_connections/terminal.rs"]
mod terminal_tests;
use pierlink_core::{
    builtin::{
        ConnectionCallbacks, ConnectionClass, ConnectionManager, ConnectionRequest,
        DedicatedCredential, ExecCommand, ExecOptions,
    },
    error::AppError,
    security::vault::{PasswordVault, Zeroizing},
    storage::{AuthMethod, Engine, Server, ServerSource},
};
use pierlink_ssh_engine::{Challenge, ConnectionOptions, HostKey, HostKeyCheck, Pty};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};

struct Callbacks {
    expected: HashMap<String, String>,
    checked: Mutex<Vec<String>>,
}
impl ConnectionCallbacks for Callbacks {
    fn check_host_key(&self, id: &str, key: &HostKey, _: &AtomicBool) -> Result<bool, AppError> {
        self.checked.lock().unwrap().push(id.into());
        Ok(self.expected.get(id) == Some(&key.fingerprint))
    }
    fn password(&self, _: &str, _: &AtomicBool) -> Result<Option<Zeroizing<String>>, AppError> {
        panic!("密钥认证不能询问密码")
    }
    fn passphrase(
        &self,
        _: &str,
        _: &Path,
        _: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        panic!("测试密钥没有口令")
    }
    fn dedicated_key(
        &self,
        _: &str,
        _: &AtomicBool,
    ) -> Result<Option<DedicatedCredential>, AppError> {
        Ok(None)
    }
    fn keyboard_interactive(
        &self,
        _: &str,
        _: Challenge,
        _: &AtomicBool,
    ) -> Result<Option<Vec<Zeroizing<String>>>, AppError> {
        panic!("密钥认证不能询问验证码")
    }
}
fn record(id: &str, server: &support::Server) -> Server {
    let mut result = pierlink_core::server_input::build_alias_server(id, None, id, 0).unwrap();
    result.source = ServerSource::Manual;
    result.ssh_alias = None;
    result.host = Some("127.0.0.1".into());
    result.port = Some(server.port);
    result.username = Some(server.user.clone());
    result.auth_method = AuthMethod::Key;
    result.identity_file = Some(server.root.path().join("client").to_str().unwrap().into());
    result.connection_engine = Engine::Builtin;
    result
}
fn callbacks(records: &[(&str, ConnectionOptions)]) -> Arc<Callbacks> {
    Arc::new(Callbacks {
        expected: records
            .iter()
            .map(|(id, options)| {
                let HostKeyCheck::Fingerprint(fingerprint) = &options.host_key else {
                    panic!("缺少已知指纹")
                };
                (id.to_string(), fingerprint.clone())
            })
            .collect(),
        checked: Mutex::new(Vec::new()),
    })
}

#[test]
#[ignore = "需要允许普通用户启动本机 sshd 并监听随机端口"]
fn 本机会话上限与命令的输入环境目录双输出状态() {
    let server = support::Server::new("MaxSessions 2");
    let record = record("target", &server);
    let callbacks = callbacks(&[("target", server.options())]);
    let vault = Arc::new(PasswordVault::default());
    let manager = ConnectionManager::default();
    let request = || ConnectionRequest {
        server: &record,
        class: ConnectionClass::Main,
        vault: vault.clone(),
        callbacks: callbacks.clone(),
        interactive: false,
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let mut sessions = Vec::new();
    for _ in 0..4 {
        sessions.push(manager.shell(request(), &Pty::default(), &[]).unwrap());
    }
    assert_eq!(callbacks.checked.lock().unwrap().len(), 2);
    let directory = server.root.path().join("中文 ' $() 目录");
    std::fs::create_dir(&directory).unwrap();
    let result = manager.exec(request(), ExecCommand::Escaped("read value; printf '%s/%s/' \"$PIERLINK_TEST\" \"$value\"; pwd; printf error >&2; exit 7".into()), ExecOptions {
        stdin: b"input\n".to_vec(), env: vec![("PIERLINK_TEST".into(), "环境".into())], directory: Some(directory.to_str().unwrap().into()), ..Default::default()
    }).unwrap();
    assert_eq!(
        result.stdout,
        format!("环境/input/{}\n", directory.display()).as_bytes()
    );
    assert_eq!(result.stderr, b"error");
    assert_eq!(result.exit_code, Some(7));
    let signalled = manager
        .exec(
            request(),
            ExecCommand::Escaped("kill -TERM $$".into()),
            ExecOptions::default(),
        )
        .unwrap();
    assert!(signalled.signal.as_deref() == Some("TERM") || signalled.exit_code == Some(143));
    let result = manager
        .exec(
            request(),
            ExecCommand::Argv(vec!["sleep".into(), "30".into()]),
            ExecOptions {
                timeout: Some(Duration::from_millis(150)),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(result.timed_out);
    drop(sessions);
    manager.close_all().unwrap();
}

#[test]
#[ignore = "需要允许普通用户启动三级本机 sshd 并监听随机端口"]
fn 本机三级跳板逐级核对不同主机密钥并共享跳板() {
    let first = support::Server::new("AllowTcpForwarding yes");
    let second = support::Server::new("AllowTcpForwarding yes");
    let third = support::Server::new("AllowTcpForwarding yes");
    let first_record = record("first", &first);
    let mut second_record = record("second", &second);
    second_record.jump_server_id = Some("first".into());
    let mut target = record("target", &third);
    target.jump_server_id = Some("second".into());
    let callbacks = callbacks(&[
        ("first", first.options()),
        ("second", second.options()),
        ("target", third.options()),
    ]);
    let records = HashMap::from([
        ("first".to_string(), first_record),
        ("second".to_string(), second_record),
    ]);
    let manager = ConnectionManager::default();
    manager.set_server_resolver(Arc::new(move |id| Ok(records.get(id).cloned())));
    let vault = Arc::new(PasswordVault::default());
    let request = || ConnectionRequest {
        server: &target,
        class: ConnectionClass::Main,
        vault: vault.clone(),
        callbacks: callbacks.clone(),
        interactive: false,
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let result = manager
        .exec(
            request(),
            ExecCommand::Argv(vec!["printf".into(), "%s".into(), "三级连接".into()]),
            ExecOptions::default(),
        )
        .unwrap();
    assert_eq!(result.stdout, "三级连接".as_bytes());
    assert_eq!(
        *callbacks.checked.lock().unwrap(),
        vec!["first", "second", "target"]
    );
    let connection = manager.get_connection(request()).unwrap();
    connection.ping().unwrap();
    assert_eq!(callbacks.checked.lock().unwrap().len(), 3);
    manager.close_server("first").unwrap();
    assert!(connection.is_closed());
    manager.close_all().unwrap();
}
