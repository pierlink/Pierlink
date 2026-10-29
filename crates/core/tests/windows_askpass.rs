//! Windows 端到端：用打包出的 Pierlink.exe 作为 ssh 的自动填密码程序（SSH_ASKPASS），
//! 密码放在内存密码库里，经命名管道交给 Pierlink.exe，登录 CI 本机的 OpenSSH 服务器（0.3.1 起的做法）。
//! 密码对：登录成功；密码错：报 ssh.auth_failed（不是笼统的连接断开），并把错的密码从库里作废。
//! 需要 PIERLINK_TEST_ASKPASS_EXE、PIERLINK_TEST_PW_USER、PIERLINK_TEST_PW_PASS（CI 打包后设置），否则跳过。
#![cfg(windows)]
#[path = "../src/test_support.rs"]
mod test_support;

use pierlink_core::{
    error::AppError,
    process::SshProgram,
    security::vault::{PasswordVault, Zeroizing},
    sftp::SftpSession,
    ssh::{
        args::{AskPass, CommonOpts, Purpose, build_args},
        askpass_pipe::{AskHandler, AskpassServer, PromptKind, RegistrationSpec},
        host_key::{HostKeyPrompt, probe},
        target::SshTarget,
    },
};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};

/// 后台连接不弹框：测试里不会被调用
struct NoUi;
impl AskHandler for NoUi {
    fn ask_password(&self, _: &str, _: &str) -> Result<Option<Zeroizing<String>>, AppError> {
        Ok(None)
    }
    fn ask_other(
        &self,
        _: &str,
        _: &str,
        _: PromptKind,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        Ok(None)
    }
}

#[test]
fn windows_askpass_用打包的程序自动填密码登录() {
    let (Some(exe), Ok(user), Ok(pass)) = (
        std::env::var_os("PIERLINK_TEST_ASKPASS_EXE"),
        std::env::var("PIERLINK_TEST_PW_USER"),
        std::env::var("PIERLINK_TEST_PW_PASS"),
    ) else {
        eprintln!("跳过：没有打包好的 Pierlink.exe 和测试账户（只在 CI 打包后运行）");
        return;
    };
    let ssh =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../src-tauri/resources/openssh/ssh.exe");
    let ssh = if ssh.is_file() {
        ssh
    } else {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/OpenSSH/ssh.exe")
    };
    let program = SshProgram { path: ssh };
    let root = test_support::tempdir();
    let target = SshTarget::Manual {
        host: "127.0.0.1".into(),
        port: 22,
        user: Some(user.clone()),
        identity_file: None,
        jump: None,
    };
    let known_hosts = root.path().join("known_hosts");

    // 先确认指纹（应用里添加服务器后第一次连接也是这样）
    let plain = CommonOpts {
        known_hosts: Some(known_hosts.clone()),
        connect_timeout: Some(10),
        ..CommonOpts::default()
    };
    let mut cmd = build_args(&target, &Purpose::HostKeyProbe, &plain).unwrap();
    let global = root.path().join("global_known_hosts");
    cmd.args.splice(
        0..0,
        [
            "-o".to_owned(),
            format!("GlobalKnownHostsFile={}", global.display()),
        ],
    );
    if let HostKeyPrompt::Confirm(c) = probe(&program, &cmd).unwrap() {
        // 探测禁止密码登录，确认指纹后登录失败属于正常；只要指纹写进去了
        let _ = HostKeyPrompt::Confirm(c).answer(true);
    }
    assert!(
        fs::read_to_string(&known_hosts)
            .unwrap_or_default()
            .contains("127.0.0.1")
    );

    // 管道服务只接受打包出的 Pierlink.exe 来要密码
    let vault = Arc::new(PasswordVault::default());
    let server = AskpassServer::start_with_client(
        vault.clone(),
        Arc::new(NoUi),
        root.path(),
        Some(std::path::Path::new(&exe)),
    )
    .unwrap();
    let registration = server
        .register(RegistrationSpec {
            server_id: "e2e".into(),
            // ssh 的密码提示里主机名是小写的
            expected_prompt: format!("{user}@127.0.0.1"),
            interactive: false,
            purpose: "Windows 端到端".into(),
            password_only: true,
        })
        .unwrap();
    let opts = CommonOpts {
        known_hosts: Some(known_hosts.clone()),
        connect_timeout: Some(10),
        askpass: Some(AskPass {
            program: PathBuf::from(&exe),
            registration: registration.clone(),
        }),
        ..CommonOpts::default()
    };

    // 库里没有密码：后台连接不启动 ssh，直接报需要密码
    let cmd = build_args(&target, &Purpose::Sftp, &opts).unwrap();
    let error = SftpSession::open(&program, &cmd, Duration::from_secs(30))
        .err()
        .unwrap();
    assert_eq!(error.code, "ssh.password_needed", "{error:?}");

    // 密码正确：SFTP 会话能建立（登录成功）
    vault.set("e2e", Zeroizing::new(pass.clone())).unwrap();

    // 诊断：像 ssh 那样直接运行 Pierlink.exe，看它是否经管道拿到了密码（只比较，不打印密码）
    let probe_guard = registration.prepare().unwrap();
    let out = std::process::Command::new(&exe)
        .arg(format!("{user}@127.0.0.1's password: "))
        .env("PIERLINK_ASKPASS_PIPE", probe_guard.pipe())
        .env("PIERLINK_ASKPASS_NONCE", probe_guard.nonce())
        .output()
        .unwrap();
    let printed = String::from_utf8_lossy(&out.stdout);
    eprintln!(
        "直接运行 askpass：退出码={:?}，输出了正确密码={}，输出长度={}，错误输出={:?}",
        out.status.code(),
        printed.trim_end_matches(['\r', '\n']) == pass,
        out.stdout.len(),
        String::from_utf8_lossy(&out.stderr)
    );
    drop(probe_guard);
    let cmd = build_args(&target, &Purpose::Sftp, &opts).unwrap();
    let mut session = SftpSession::open(&program, &cmd, Duration::from_secs(30))
        .unwrap_or_else(|e| panic!("自动填密码登录失败：{e:?}"));
    let home = session.realpath(".").unwrap();
    assert!(!home.is_empty());
    drop(session);

    // 密码错误：明确报登录失败，错的密码从库里作废
    vault
        .set("e2e", Zeroizing::new("definitely-wrong-password".into()))
        .unwrap();
    let cmd = build_args(&target, &Purpose::Sftp, &opts).unwrap();
    let error = SftpSession::open(&program, &cmd, Duration::from_secs(30))
        .err()
        .unwrap();
    assert_eq!(error.code, "ssh.auth_failed", "{error:?}");
    assert!(!vault.has("e2e"), "错的密码应当作废");
}
