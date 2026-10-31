//! 内置后台的本机 sshd 测试夹具；所有文件共用 core 的测试根目录。
#[path = "../../../ssh-engine/tests/support/mod.rs"]
pub(crate) mod sshd;
use crate::{
    builtin::{
        ConnectionCallbacks, ConnectionClass, ConnectionManager, ConnectionRequest,
        DedicatedCredential,
    },
    error::AppError,
    process::SshProgram,
    security::vault::{PasswordVault, Zeroizing},
    ssh::args::SshCommand,
    storage::{AuthMethod, Engine, Server, ServerSource},
};
use pierlink_ssh_engine::{Challenge, HostKey, HostKeyCheck};
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

pub(crate) struct Fixture {
    pub(crate) sshd: sshd::Server,
    pub(crate) manager: ConnectionManager,
    pub(crate) server: Server,
}
struct Callbacks(String);
impl ConnectionCallbacks for Callbacks {
    fn check_host_key(&self, _: &str, key: &HostKey, _: &AtomicBool) -> Result<bool, AppError> {
        Ok(key.fingerprint == self.0)
    }
    fn passphrase(
        &self,
        _: &str,
        _: &Path,
        _: &AtomicBool,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        panic!("测试私钥没有口令")
    }
    fn password(&self, _: &str, _: &AtomicBool) -> Result<Option<Zeroizing<String>>, AppError> {
        panic!("后台不能弹出密码框")
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
        panic!("后台不能询问验证码")
    }
}
impl Fixture {
    pub(crate) fn new() -> Self {
        let sshd = sshd::Server::with_root(crate::test_support::tempdir(), "", true);
        Self::from_sshd(sshd)
    }
    pub(crate) fn from_sshd(sshd: sshd::Server) -> Self {
        let mut server =
            crate::server_input::build_alias_server("后台测试", None, "background", 0).unwrap();
        server.source = ServerSource::Manual;
        server.ssh_alias = None;
        server.host = Some("127.0.0.1".into());
        server.port = Some(sshd.port);
        server.username = Some(sshd.user.clone());
        server.auth_method = AuthMethod::Key;
        server.identity_file = Some(sshd.root.path().join("client").to_str().unwrap().into());
        server.connection_engine = Engine::Builtin;
        Self {
            sshd,
            manager: ConnectionManager::default(),
            server,
        }
    }
    pub(crate) fn command(&self) -> SshCommand {
        let HostKeyCheck::Fingerprint(fingerprint) = self.sshd.options().host_key else {
            panic!("缺少测试指纹")
        };
        crate::remote::command::background(
            &self.manager,
            ConnectionRequest {
                progress: None,
                server: &self.server,
                class: ConnectionClass::Main,
                vault: Arc::new(PasswordVault::default()),
                callbacks: Arc::new(Callbacks(fingerprint)),
                interactive: false,
                cancelled: Arc::new(AtomicBool::new(false)),
            },
            || panic!("不能回退到 OpenSSH"),
        )
        .unwrap()
    }
    pub(crate) fn sftp_command(&self) -> SshCommand {
        let background = self.command();
        crate::sftp::command(
            &self.manager,
            background.builtin.as_ref().unwrap().request(),
            || panic!("不能回退到 OpenSSH"),
        )
        .unwrap()
    }
    pub(crate) fn program(&self) -> SshProgram {
        SshProgram {
            path: "/不存在的ssh程序".into(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.manager.close_all();
    }
}
