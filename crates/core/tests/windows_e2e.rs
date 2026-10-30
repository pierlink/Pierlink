//! Windows 端到端：CI 在本机启动 Windows 自带的 OpenSSH 服务器后，用应用实际使用的 ssh（安装包自带的
//! OpenSSH，以及系统自带的）走一遍：首次连接确认指纹并写入 known_hosts → ConPTY 远程终端 →
//! SFTP 列目录、上传、下载（含 4 MiB 文件，验证流水线）。
//! 没有设置 PIERLINK_TEST_WIN_SSHD_KEY / PIERLINK_TEST_WIN_SSHD_USER 时跳过（本地开发机）。
#![cfg(windows)]
#[path = "../src/test_support.rs"]
mod test_support;

use pierlink_core::{
    process::SshProgram,
    sftp::{
        SftpSession,
        transfer::{Conflict, download, plan_download, upload},
    },
    ssh::{
        args::{CommonOpts, Purpose, build_args},
        host_key::{HostKeyPrompt, probe},
        target::SshTarget,
    },
    terminal::{TerminalEvent, TerminalKind, TerminalManager, TerminalSpec},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, atomic::AtomicBool, mpsc},
    time::{Duration, Instant},
};

/// 测试用的 ssh 程序：安装包自带的（应用优先用它）和系统自带的
fn programs() -> Vec<PathBuf> {
    let bundled =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src-tauri/resources/openssh/ssh.exe");
    let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32")
        .join("OpenSSH")
        .join("ssh.exe");
    [bundled, system]
        .into_iter()
        .filter(|p| p.is_file())
        .collect()
}

/// 按命令行筛选指向本机的 ssh，只输出数量和 PID，避免诊断打印连接参数中的秘密。
fn assert_no_local_ssh(round: usize, phase: &str) {
    let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let output = Command::new(powershell)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference = 'Stop'; Get-CimInstance Win32_Process -Filter \"Name = 'ssh.exe'\" | Where-Object { $_.CommandLine -like '*127.0.0.1*' } | ForEach-Object { $_.ProcessId }",
        ])
        .output()
        .expect("查询本机 ssh 进程失败");
    assert!(output.status.success(), "查询本机 ssh 进程失败");
    let pids: Vec<u32> = String::from_utf8(output.stdout)
        .expect("进程编号不是 UTF-8")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.trim().parse().expect("进程编号无效"))
        .collect();
    assert!(
        pids.is_empty(),
        "第 {round}/10 轮{phase}残留 {} 个指向 127.0.0.1 的 ssh 进程，PID：{pids:?}",
        pids.len()
    );
    eprintln!("第 {round}/10 轮{phase}：本机 ssh 残留数量为 0");
}

#[test]
fn windows_端到端_指纹终端与文件传输() {
    let (Some(key), Some(user)) = (
        std::env::var_os("PIERLINK_TEST_WIN_SSHD_KEY"),
        std::env::var("PIERLINK_TEST_WIN_SSHD_USER").ok(),
    ) else {
        eprintln!("跳过：没有本机 OpenSSH 服务器（只在 CI 上运行）");
        return;
    };
    let programs = programs();
    assert!(!programs.is_empty(), "找不到 ssh.exe");
    for ssh in programs {
        eprintln!("测试 ssh：{}", ssh.display());
        run_once(&ssh, Path::new(&key), &user);
    }
}

fn run_once(ssh: &Path, key: &Path, user: &str) {
    let root = test_support::tempdir();
    let target = SshTarget::Manual {
        host: "127.0.0.1".into(),
        port: 22,
        user: Some(user.into()),
        identity_file: Some(key.into()),
        jump: None,
    };
    let mut opts = CommonOpts {
        connect_timeout: Some(10),
        ..CommonOpts::default()
    };
    let program = SshProgram { path: ssh.into() };

    // 1. 连续十轮首次连接；每轮独立 known_hosts，覆盖提问和“信任并连接”后的等待。
    for round in 1..=10 {
        let known_hosts = root.path().join(format!("known_hosts-{round}"));
        opts.known_hosts = Some(known_hosts.clone());
        let mut cmd = build_args(&target, &Purpose::HostKeyProbe, &opts).unwrap();
        let global = root.path().join("global_known_hosts");
        cmd.args.splice(
            0..0,
            [
                "-o".to_owned(),
                format!("GlobalKnownHostsFile={}", global.display()),
            ],
        );
        if round == 1 {
            let prompt = probe(&program, &cmd).expect("取消前未拿到指纹提问");
            assert!(matches!(&prompt, HostKeyPrompt::Confirm(_)));
            drop(prompt);
            assert_no_local_ssh(round, "取消后");
            let prompt = probe(&program, &cmd).expect("拒绝前未拿到指纹提问");
            assert!(matches!(&prompt, HostKeyPrompt::Confirm(_)));
            assert_eq!(
                prompt.answer(false).err().unwrap().code,
                "ssh.host_key_rejected"
            );
            assert_no_local_ssh(round, "拒绝后");
        }
        let started = Instant::now();
        let prompt =
            probe(&program, &cmd).unwrap_or_else(|e| panic!("第 {round}/10 轮指纹探测失败：{e:?}"));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "第 {round}/10 轮拿到提问超过 10 秒：{:?}",
            started.elapsed()
        );
        let HostKeyPrompt::Confirm(c) = prompt else {
            panic!("第 {round}/10 轮：全新的 known_hosts 应当要求确认指纹");
        };
        assert!(c.info.fingerprint.starts_with("SHA256:"));
        let answered = Instant::now();
        let trusted = HostKeyPrompt::Confirm(c)
            .answer(true)
            .unwrap_or_else(|e| panic!("第 {round}/10 轮信任后未完成校验：{e:?}"));
        assert!(matches!(trusted, HostKeyPrompt::Trusted));
        assert!(
            answered.elapsed() < Duration::from_secs(10),
            "第 {round}/10 轮信任后等待超过 10 秒：{:?}",
            answered.elapsed()
        );
        assert_no_local_ssh(round, "信任后");
        let saved = fs::read_to_string(&known_hosts).unwrap();
        assert!(
            saved.contains("127.0.0.1"),
            "第 {round}/10 轮 known_hosts 没写入：{saved}"
        );
        let rechecked = Instant::now();
        assert!(matches!(
            probe(&program, &cmd)
                .unwrap_or_else(|e| panic!("第 {round}/10 轮复查已信任指纹失败：{e:?}")),
            HostKeyPrompt::Trusted
        ));
        assert!(
            rechecked.elapsed() < Duration::from_secs(10),
            "第 {round}/10 轮复查已信任指纹超过 10 秒：{:?}",
            rechecked.elapsed()
        );
        assert_no_local_ssh(round, "复查后");
        eprintln!("第 {round}/10 轮通过：提问、信任后校验及已信任复查均在 10 秒内完成");
    }

    // 2. 远程终端（ConPTY 里运行 ssh；Windows 服务器默认 shell 是 cmd）
    let command = build_args(
        &target,
        &Purpose::Terminal {
            login_shell: None,
            cwd_marker: Some("winE2E".into()),
        },
        &opts,
    )
    .unwrap();
    let manager = TerminalManager::new();
    let (tx, rx) = mpsc::channel();
    let id = manager
        .open(
            TerminalSpec {
                kind: TerminalKind::Remote {
                    ssh: ssh.into(),
                    command,
                    config_dir: root.path().into(),
                },
                cols: 100,
                rows: 30,
            },
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
    // %OS% 展开后是 Windows_NT，和输入的文字不同，确认是远程 shell 真正执行的输出
    let start = Instant::now();
    let mut output = Vec::new();
    let mut sent = false;
    let ok = loop {
        if start.elapsed() > Duration::from_secs(40) {
            break false;
        }
        if !sent && start.elapsed() > Duration::from_secs(3) {
            manager.write(&id, b"echo PL-%OS%-OK\r").unwrap();
            sent = true;
        }
        if let Ok(TerminalEvent::Output(bytes)) = rx.recv_timeout(Duration::from_millis(300)) {
            let _ = manager.ack(&id, bytes.len());
            output.extend(bytes);
        }
        if String::from_utf8_lossy(&output).contains("PL-Windows_NT-OK") {
            break true;
        }
    };
    let _ = manager.close(&id);
    assert!(
        ok,
        "远程终端没有执行命令：{:?}",
        String::from_utf8_lossy(&output[output.len().saturating_sub(800)..])
    );

    // 3. SFTP：家目录、建目录、上传 4 MiB 文件、列目录、下载并比对
    let sftp = build_args(&target, &Purpose::Sftp, &opts).unwrap();
    let mut session = SftpSession::open(&program, &sftp, Duration::from_secs(20)).unwrap();
    let home = session.realpath(".").unwrap();
    let remote_dir = format!("{}/pl-e2e-{}", home.trim_end_matches('/'), uuid_like(ssh));
    session.mkdir(&remote_dir).unwrap();
    let source = root.path().join("大文件 测试.bin");
    let bytes: Vec<u8> = (0..4 * 1024 * 1024 + 123)
        .map(|i| (i * 31 % 251) as u8)
        .collect();
    fs::write(&source, &bytes).unwrap();
    let cancel = AtomicBool::new(false);
    let report = upload(
        &mut session,
        &[source],
        &remote_dir,
        Conflict::Error,
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(report.succeeded, 1, "{report:?}");
    let listing = session.list_dir(&remote_dir, 100).unwrap();
    assert!(
        listing
            .entries
            .iter()
            .any(|e| e.name == "大文件 测试.bin" && e.size == bytes.len() as u64),
        "{listing:?}"
    );
    let back = root.path().join("back");
    fs::create_dir(&back).unwrap();
    let report = download(
        &mut session,
        &[format!("{remote_dir}/大文件 测试.bin")],
        &back,
        Conflict::Error,
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(report.succeeded, 1, "{report:?}");
    assert_eq!(fs::read(back.join("大文件 测试.bin")).unwrap(), bytes);
    let plan = plan_download(
        &mut session,
        &[format!("{remote_dir}/大文件 测试.bin")],
        &back,
    )
    .unwrap();
    assert_eq!(plan.existing.len(), 1);
    assert!(!plan.existing[0].starts_with(r"\\?\"));
    let _ = session.remove_file(&format!("{remote_dir}/大文件 测试.bin"));
}

/// 每个 ssh 程序用不同的远程目录，避免两轮测试互相影响
fn uuid_like(ssh: &Path) -> String {
    let name = ssh.to_string_lossy();
    format!(
        "{}-{}",
        if name.contains("resources") {
            "bundled"
        } else {
            "system"
        },
        std::process::id()
    )
}
