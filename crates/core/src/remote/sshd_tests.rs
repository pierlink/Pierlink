//! 真实后台调用方验收；需在允许普通用户监听随机端口的环境显式执行。
use crate::{
    builtin_test_support::{Fixture, sshd},
    remote::run::{RunLimits, run_script},
    ssh::quote::sh_quote,
};
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool, mpsc},
    time::{Duration, Instant},
};

#[test]
#[ignore = "需要本机 sshd 和随机监听端口"]
fn 内置后台真实脚本输入限额超时及退出分类() {
    let f = Fixture::new();
    let command = f.command();
    assert_eq!(
        run_script(
            &f.program(),
            &command,
            "cat\nprintf true",
            RunLimits::default()
        )
        .unwrap(),
        true
    );
    for (body, code) in [
        (
            "printf 'Permission denied (publickey).' >&2; exit 7",
            "remote.script_failed",
        ),
        (
            "printf 'Permission denied (publickey).' >&2; exit 255",
            "ssh.auth_failed",
        ),
        ("yes", "remote.output_too_large"),
        ("yes >&2", "remote.output_too_large"),
    ] {
        assert_eq!(
            run_script(
                &f.program(),
                &command,
                body,
                RunLimits {
                    stdout_bytes: 1024,
                    stderr_bytes: 1024,
                    ..Default::default()
                }
            )
            .unwrap_err()
            .code,
            code
        );
    }
    assert_eq!(
        run_script(
            &f.program(),
            &command,
            "sleep 30",
            RunLimits {
                timeout: Duration::from_millis(200),
                ..Default::default()
            }
        )
        .unwrap_err()
        .code,
        "remote.timeout"
    );
    assert_eq!(
        run_script(&f.program(), &command, "printf true", RunLimits::default()).unwrap(),
        true
    );
}

#[test]
#[ignore = "需要本机 sshd 和随机监听端口"]
fn 内置专用密钥安装检查独立认证移除与回滚() {
    use crate::keys::remote::*;
    let mut f = Fixture::new();
    let original = f.command();
    let key = f.sshd.root.path().join("dedicated");
    sshd::keygen(&key, "ed25519", "");
    let public = fs::read_to_string(key.with_extension("pub"))
        .unwrap()
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    f.server.identity_file = Some(key.to_str().unwrap().into());
    let verification = f.command();
    // 已有业务连接成功不能让未安装的新密钥通过认证。
    run_script(&f.program(), &original, "printf true", RunLimits::default()).unwrap();
    assert!(verify(&f.program(), &verification).is_err());
    assert_eq!(
        install_and_verify(&f.program(), &original, &verification, &public)
            .unwrap()
            .outcome,
        InstallOutcome::Installed
    );
    assert_eq!(
        install(&f.program(), &original, &public).unwrap().outcome,
        InstallOutcome::AlreadyPresent
    );
    let listed = list(&f.program(), &original, &public).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].ours);
    assert_eq!(
        remove(&f.program(), &original, &public).unwrap(),
        RemoveOutcome::Removed { count: 1 }
    );
    assert!(verify(&f.program(), &verification).is_err());
    let other = fs::read_to_string(f.sshd.root.path().join("client.pub"))
        .unwrap()
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        install_and_verify(&f.program(), &original, &verification, &other)
            .unwrap_err()
            .code,
        "keys.verify_failed"
    );
    assert!(list(&f.program(), &original, &public).unwrap().is_empty());
}

#[test]
#[ignore = "需要本机 sshd、伪终端和随机监听端口"]
fn 内置目录跟随查询真实终端及缺失标记() {
    use crate::{
        cwd::*,
        terminal::{TerminalEvent, TerminalKind, TerminalManager, TerminalSpec},
    };
    let f = Fixture::new();
    let command = f.command();
    let marker = uuid::Uuid::new_v4().to_string();
    assert!(
        remote_cwd(&f.program(), &command, &marker)
            .unwrap()
            .is_none()
    );
    let directory = f.sshd.root.path().join("中文 ' $() 目录\n换行");
    fs::create_dir(&directory).unwrap();
    let manager = TerminalManager::new();
    let (tx, rx) = mpsc::channel();
    let id = manager
        .open(
            TerminalSpec {
                kind: TerminalKind::Local {
                    program: "/bin/bash".into(),
                    args: vec!["--noprofile".into(), "--norc".into(), "-i".into()],
                    cwd: None,
                    // 不读 .bashrc 的交互 bash 退出时会按默认的 500 行截断并改写真实的 ~/.bash_history
                    env: vec![
                        ("LC_PIERLINK_TERM".into(), marker.clone()),
                        ("HISTFILE".into(), "/dev/null".into()),
                    ],
                },
                cols: 80,
                rows: 24,
            },
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
    manager
        .write(
            &id,
            format!(
                "cd -- {}\nprintf '\\nREADY\\n'\n",
                sh_quote(directory.to_str().unwrap()).unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
    let start = Instant::now();
    loop {
        if let Ok(TerminalEvent::Output(bytes)) = rx.recv_timeout(Duration::from_millis(50)) {
            manager.ack(&id, bytes.len()).unwrap();
        }
        if let Some(status) = remote_shell_status(&f.program(), &command, &marker).unwrap()
            && status.cwd == directory.to_str().unwrap()
        {
            assert!(!status.busy);
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10));
    }
    assert_eq!(
        remote_cwd(&f.program(), &command, &marker)
            .unwrap()
            .as_deref(),
        directory.to_str()
    );
    manager.close(&id).unwrap();
}

#[test]
#[ignore = "需要本机 sshd 和随机监听端口，持续采样两分钟"]
fn 内置资源概览两分钟至少八帧且停止后无采样进程() {
    use crate::monitor::{Monitor, MonitorEvent};
    let f = Fixture::new();
    let command = f.command();
    let (tx, rx) = mpsc::channel();
    let monitor = Monitor::start(
        f.program(),
        command.clone(),
        Duration::from_secs(15),
        Arc::new(move |event| {
            tx.send(event).unwrap();
        }),
    )
    .unwrap();
    let start = Instant::now();
    let mut frames = 0;
    let mut pid = None;
    while start.elapsed() < Duration::from_secs(120) {
        if let Ok(event) = rx.recv_timeout(Duration::from_secs(1)) {
            match event {
                MonitorEvent::Sample(_) => {
                    frames += 1;
                    if pid.is_none() {
                        // 按独立 HOME 锁定采样 shell；它正在等待当前间隔的 sleep 子进程。
                        let value = run_script(&f.program(), &command, r#"
for p in /proc/[0-9]*; do
    [ -r "$p/environ" ] || continue
    awk -v h="HOME=$HOME" 'BEGIN {RS="\0"} $0==h {found=1} END {exit !found}' "$p/environ" 2>/dev/null || continue
    read -r comm < "$p/comm" || continue
    [ "$comm" = sh ] || continue
    for c in $(cat "$p/task/${p##*/}/children" 2>/dev/null); do
        [ "$(cat /proc/$c/comm 2>/dev/null)" = sleep ] || continue
        printf '%s' "${p##*/}"; exit 0
    done
done
printf null
"#, RunLimits::default()).unwrap();
                        pid = value.as_u64();
                    }
                }
                MonitorEvent::Connecting => {}
                other => panic!("采样意外中断：{other:?}"),
            }
        }
    }
    assert!(frames >= 8, "两分钟只收到 {frames} 帧");
    let pid = pid.expect("未找到采样 shell");
    let children = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap();
    let descendants: Vec<_> = children.split_whitespace().map(str::to_owned).collect();
    monitor.stop();
    let stopped = Instant::now();
    while process_alive(pid)
        || descendants
            .iter()
            .any(|pid| process_alive(pid.parse().unwrap()))
    {
        assert!(
            stopped.elapsed() < Duration::from_secs(5),
            "采样进程或子进程未退出"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // 停监控不影响同服务器其他后台命令。
    assert_eq!(
        run_script(&f.program(), &command, "printf true", RunLimits::default()).unwrap(),
        true
    );
}
fn process_alive(pid: u64) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| !stat.rsplit_once(") ").unwrap().1.starts_with('Z'))
}

#[test]
#[ignore = "需要本机 sshd、随机监听端口和 PIERLINK_TEST_SYNCTHING 指定的真实程序"]
fn 内置部署完整上传创建启动重启目录检查与卸载() {
    use crate::deploy::*;
    use sha2::{Digest, Sha256};
    let binary =
        std::env::var_os("PIERLINK_TEST_SYNCTHING").expect("必须设置真实 Syncthing 程序路径");
    let f = Fixture::new();
    set_computer_short_id("test0001").unwrap();
    let command = f.command();
    let report = inspect(&f.program(), &command).unwrap();
    assert!(report.instance.is_none());
    let mut plan = plan(&report, "2.1.7").unwrap();
    let compressed = f.sshd.root.path().join("syncthing.gz");
    let output = std::process::Command::new("gzip")
        .arg("-c")
        .arg(&binary)
        .stdout(fs::File::create(&compressed).unwrap())
        .status()
        .unwrap();
    assert!(output.success());
    let hash = format!("{:x}", Sha256::digest(fs::read(&binary).unwrap()));
    upload_binary(
        &f.program(),
        &command,
        &compressed,
        &hash,
        "2.1.7",
        plan.arch,
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    plan.autostart_consent = true;
    let secrets = InstanceSecrets {
        api_key: "BackgroundTest0123456789".into(),
        gui_user: "测试用户".into(),
        gui_password: "测试口令".into(),
    };
    let info = create_instance(&f.program(), &command, &plan, &secrets)
        .unwrap()
        .info;
    struct Stop<'a>(&'a Fixture, &'a crate::ssh::args::SshCommand, String);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            let _ = stop_instance(&self.0.program(), self.1, &self.2);
        }
    }
    let _stop = Stop(&f, &command, info.id.clone());
    assert_eq!(
        read_api_key(&f.program(), &command, &info.id).unwrap(),
        secrets.api_key
    );
    assert!(matches!(
        start_instance(&f.program(), &command, &info).unwrap(),
        StartOutcome::Nohup { .. }
    ));
    assert!(
        inspect(&f.program(), &command)
            .unwrap()
            .instance
            .unwrap()
            .running
    );
    assert!(matches!(
        start_instance(&f.program(), &command, &info).unwrap(),
        StartOutcome::AlreadyRunning { .. }
    ));
    let project = f.sshd.root.path().join("h/project");
    fs::create_dir(&project).unwrap();
    assert!(
        !check_sync_directory(&f.program(), &command, &info.id, project.to_str().unwrap())
            .unwrap()
            .already_managed
    );
    stop_instance(&f.program(), &command, &info.id).unwrap();
    start_instance(&f.program(), &command, &info).unwrap();
    uninstall(&f.program(), &command, &info.id, true).unwrap();
    uninstall(&f.program(), &command, &info.id, false).unwrap();
    assert!(project.is_dir());
    assert!(inspect(&f.program(), &command).unwrap().instance.is_none());
}
