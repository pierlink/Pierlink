//! 本机 POSIX shell 替身验证；所有测试文件都由共用数据盘临时根目录托管。
#![cfg(unix)]
use super::*;
use crate::test_support::tempdir;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    sync::mpsc::{self, Receiver},
};

struct Fixture {
    root: tempfile::TempDir,
    program: SshProgram,
    command: SshCommand,
}
impl Fixture {
    fn new() -> Self {
        let root = tempdir();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for name in ["awk", "grep", "sleep", "date"] {
            symlink(format!("/usr/bin/{name}"), bin.join(name)).unwrap();
        }
        let proc = root.path().join("proc");
        fs::create_dir(&proc).unwrap();
        for (name, content) in [
            ("stat", "cpu 100 0 100 800 0 0 0 0 0 0\n"),
            ("uptime", "12345.67 98765.43\n"),
            ("loadavg", "1.25 2.5 3.75 1/100 99\n"),
            ("cpuinfo", "processor : 0\nprocessor : 1\n"),
            (
                "meminfo",
                "MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 20 kB\nCached: 30 kB\nSwapTotal: 2000 kB\nSwapFree: 500 kB\n",
            ),
            ("mounts", "/dev/root / ext4 rw 0 0\n"),
        ] {
            fs::write(proc.join(name), content).unwrap();
        }
        fs::write(
            root.path().join("os-release"),
            "PRETTY_NAME=\"测试 \\\"系统\\\" \\\\ Linux\"\n",
        )
        .unwrap();
        let command = SshCommand {
            args: vec!["-s".into()],
            env: vec![
                ("PATH".into(), bin.display().to_string()),
                ("HOME".into(), "/home/user".into()),
                (
                    "PIERLINK_MONITOR_PROC_ROOT".into(),
                    proc.display().to_string(),
                ),
                (
                    "PIERLINK_MONITOR_OS_RELEASE".into(),
                    root.path().join("os-release").display().to_string(),
                ),
            ],
            config_text: None,
            builtin: None,
            askpass: None,
        };
        Self {
            root,
            program: SshProgram {
                path: "/bin/sh".into(),
            },
            command,
        }
    }
    fn executable(&self, name: &str, body: &str) {
        let path = self.root.path().join("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn env(&mut self, key: &str, value: impl ToString) {
        self.command.env.retain(|(k, _)| k != key);
        self.command.env.push((key.into(), value.to_string()));
    }
    fn write_proc(&self, name: &str, value: &str) {
        fs::write(self.root.path().join("proc").join(name), value).unwrap();
    }
    fn session(&self, interval: Duration) -> Session {
        Session::start(
            &self.program,
            &self.command,
            sampling_script(interval).unwrap(),
        )
        .unwrap()
    }
    fn monitor(&self, interval: Duration) -> (Monitor, Receiver<MonitorEvent>) {
        let (tx, rx) = mpsc::channel();
        let monitor = Monitor::start(
            self.program.clone(),
            self.command.clone(),
            interval,
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
        (monitor, rx)
    }
    fn record_pid(&mut self) {
        self.env("PLMON_PID_FILE", self.root.path().join("pids").display());
        self.command.args = vec![
            "-c".into(),
            "printf '%s\\n' \"$$\" >> \"$PLMON_PID_FILE\"; exec /bin/sh -s".into(),
        ];
    }
    fn pids(&self) -> Vec<u32> {
        fs::read_to_string(self.root.path().join("pids"))
            .unwrap()
            .lines()
            .map(|p| p.parse().unwrap())
            .collect()
    }
}

fn samples(session: &mut Session, count: usize, timeout: Duration) -> Vec<ResourceSample> {
    let start = Instant::now();
    let mut samples = Vec::new();
    while samples.len() < count {
        for frame in session.frames().unwrap() {
            if let Frame::Sample(sample) = frame {
                samples.push(*sample);
            }
        }
        assert!(
            start.elapsed() < timeout,
            "资源帧超时，已收到 {} 帧",
            samples.len()
        );
        session.poll().unwrap();
        thread::sleep(Duration::from_millis(10));
    }
    samples
}

fn event(rx: &Receiver<MonitorEvent>, predicate: impl Fn(&MonitorEvent) -> bool) -> MonitorEvent {
    let start = Instant::now();
    loop {
        let event = rx
            .recv_timeout(Duration::from_secs(4).saturating_sub(start.elapsed()))
            .unwrap();
        if predicate(&event) {
            return event;
        }
    }
}

fn no_process(pid: u32) {
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "进程 {pid} 未回收"
    );
}

#[test]
fn f8_本机_sh_两帧字段完整_首次_cpu为空_第二次有值() {
    let mut fixture = Fixture::new();
    fixture.env("PIERLINK_MONITOR_PROC_ROOT", "/proc");
    let start = Instant::now();
    let frames = samples(
        &mut fixture.session(Duration::from_secs(1)),
        2,
        Duration::from_secs(2),
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(frames[0].cpu_percent.is_none());
    assert!(frames[0].net_rx_bps.is_none() && frames[0].net_tx_bps.is_none());
    assert!(frames[1].net_rx_bps.is_some() && frames[1].net_tx_bps.is_some());
    assert!(
        frames[1]
            .cpu_percent
            .is_some_and(|cpu| (0.0..=100.0).contains(&cpu))
    );
    assert!(frames[0].cpu_cores > 0 && frames[0].mem_total > 0 && frames[0].uptime_s > 0);
    assert!(frames[0].at_ms > 0 && frames[1].at_ms >= frames[0].at_ms);
    assert_eq!(frames[0].os_name.as_deref(), Some("测试 \"系统\" \\ Linux"));
    assert!(frames[1].os_name.is_none());
    assert!(frames[0].gpus.is_empty() && frames[0].gpu_error.is_none());
    let json = serde_json::to_value(&frames[0]).unwrap();
    assert_eq!(json.as_object().unwrap().len(), 16);
    // 交换区：读的是真实 /proc/meminfo，剩余不会超过总量
    assert!(frames[0].swap_free <= frames[0].swap_total);
    assert!(json.get("at_ms").is_some());
}

#[test]
fn f7_旧内核内存回退_核数回退_busybox格式df_特殊挂载点及gpu字符串转义() {
    let fixture = Fixture::new();
    fixture.write_proc(
        "mounts",
        "/dev/root / ext4 rw 0 0\n/dev/data /home ext4 rw 0 0\n/dev/other /unrelated ext4 rw 0 0\n",
    );
    fixture.executable("df", "[ \"$1\" = -Pk ] || exit 2\nprintf 'Filesystem 1024-blocks Used Available Capacity Mounted on\\n/dev/long-device\\n 1000 200 800 20%% %s\\n' \"$2\"");
    fixture.executable("nvidia-smi", "printf '%s\\n' '0, \"NVIDIA \"\"RTX\"\", 测试\\卡\", 75, 512, 2048, 63' '1, 第二张卡, N/A, [Not Supported], 4096, N/A'");
    let sample = samples(
        &mut fixture.session(Duration::from_secs(1)),
        1,
        Duration::from_secs(2),
    )
    .remove(0);
    assert_eq!(sample.cpu_cores, 2);
    assert_eq!(sample.mem_total, 1_024_000);
    assert_eq!(sample.mem_available, 150 * 1024);
    assert_eq!(
        (sample.swap_total, sample.swap_free),
        (2000 * 1024, 500 * 1024)
    );
    assert_eq!(sample.load, [1.25, 2.5, 3.75]);
    assert_eq!(sample.uptime_s, 12345);
    assert_eq!(
        sample
            .disks
            .iter()
            .map(|d| d.mount.as_str())
            .collect::<Vec<_>>(),
        ["/", "/home", "/unrelated"]
    );
    assert_eq!(sample.disks[0].total, 1000 * 1024);
    assert_eq!(sample.disks[0].free, 800 * 1024);
    assert_eq!(sample.disks[0].used, 200 * 1024);
    assert_eq!(sample.disks[0].fs_type, "ext4");
    assert_eq!(sample.gpus.len(), 2);
    assert_eq!(sample.gpus[0].name, "NVIDIA \"RTX\", 测试\\卡");
    assert_eq!(sample.gpus[0].util_percent, Some(75));
    assert_eq!(sample.gpus[0].mem_used, Some(512 * 1024 * 1024));
    assert_eq!(sample.gpus[0].mem_total, Some(2048 * 1024 * 1024));
    assert_eq!(sample.gpus[0].temp_c, Some(63));
    assert_eq!(sample.gpus[1].index, 1);
    assert!(
        sample.gpus[1].util_percent.is_none()
            && sample.gpus[1].mem_used.is_none()
            && sample.gpus[1].temp_c.is_none()
    );
    assert!(sample.gpu_error.is_none());
}

#[test]
fn 特殊挂载点不拆参_网络文件系统与伪文件系统不查() {
    let mut fixture = Fixture::new();
    fixture.env("HOME", "/home/a b\t\n\"\\/user");
    fixture.write_proc("mounts", "/dev/root / nfs rw 0 0\n/dev/a /home ext4 rw 0 0\n/dev/data /home/a\\040b\\011\\012\"\\134 xfs rw 0 0\n/dev/other /ignored ext4 rw 0 0\n");
    fixture.executable("df", "[ \"$#\" = 2 ] || exit 2\n[ \"$1\" = -Pk ] || exit 3\nprintf 'Filesystem 1024-blocks Used Available Capacity Mounted on\\n/dev/a 100 10 90 10%% %s\\n' \"$2\"");
    let sample = samples(
        &mut fixture.session(Duration::from_secs(1)),
        1,
        Duration::from_secs(2),
    )
    .remove(0);
    assert_eq!(sample.disks.len(), 3);
    assert_eq!(sample.disks[1].mount, "/home/a b\t\n\"\\");
    fixture.write_proc("mounts", "/dev/root / overlay rw 0 0\nserver:/home /home nfs rw 0 0\n/dev/other /ignored tmpfs rw 0 0\n");
    fixture.executable("df", "printf '%s' \"不应调用\" > \"$PLMON_UNEXPECTED_DF\"");
    fixture.env(
        "PLMON_UNEXPECTED_DF",
        fixture.root.path().join("unexpected").display(),
    );
    let sample = samples(
        &mut fixture.session(Duration::from_secs(1)),
        1,
        Duration::from_secs(2),
    )
    .remove(0);
    assert!(sample.disks.is_empty());
    assert!(!fixture.root.path().join("unexpected").exists());
}

fn df_timeout(mode: &str) {
    let fixture = Fixture::new();
    if mode == "modern" {
        symlink("/usr/bin/timeout", fixture.root.path().join("bin/timeout")).unwrap();
    } else if mode == "legacy" {
        fixture.executable(
            "timeout",
            "[ \"$1\" = -t ] || exit 2\nshift\nexec /usr/bin/timeout \"$@\"",
        );
    }
    // 故意保留一个 sleep 子进程，验证超时会关闭其继承的输出管道。
    fixture.executable("df", "sleep 30\nprintf '不应出现\\n'");
    let frames = samples(
        &mut fixture.session(Duration::from_millis(100)),
        2,
        Duration::from_secs(13),
    );
    assert!(frames.iter().all(|s| s.disks.is_empty()));
}
#[test]
fn f7_df超时_现代timeout_下一帧继续() {
    df_timeout("modern");
}
#[test]
fn f7_df超时_旧busybox_timeout参数_下一帧继续() {
    df_timeout("legacy");
}
#[test]
fn f7_df超时_无timeout_下一帧继续() {
    df_timeout("none");
}

#[test]
fn f8_gpu超时或失败_只输出固定原因_不泄露错误原文() {
    let fixture = Fixture::new();
    fixture.executable("nvidia-smi", "printf '%s' 'password=秘密'\nexit 1");
    let sample = samples(
        &mut fixture.session(Duration::from_secs(1)),
        1,
        Duration::from_secs(2),
    )
    .remove(0);
    assert!(sample.gpus.is_empty());
    assert_eq!(sample.gpu_error.as_deref(), Some("采样失败"));
    fixture.executable("nvidia-smi", "printf '%s\\n' '畸形CSV'");
    let sample = samples(
        &mut fixture.session(Duration::from_secs(1)),
        1,
        Duration::from_secs(2),
    )
    .remove(0);
    assert_eq!(sample.gpu_error.as_deref(), Some("输出格式无效"));
    fixture.executable("nvidia-smi", "sleep 30");
    let frames = samples(
        &mut fixture.session(Duration::from_millis(100)),
        2,
        Duration::from_secs(13),
    );
    assert!(
        frames
            .iter()
            .all(|s| s.gpus.is_empty() && s.gpu_error.as_deref() == Some("采样超时"))
    );
}

#[test]
fn busybox_sh和工具集_两帧及df_gpu超时() {
    if !Path::new("/usr/bin/busybox").exists() {
        return;
    }
    let mut fixture = Fixture::new();
    fixture.program.path = "/usr/bin/busybox".into();
    fixture.command.args.insert(0, "sh".into());
    for name in ["awk", "grep", "sleep", "date", "timeout", "df"] {
        // 这是测试生成文件的替换，由 TempDir 在测试结束时整体清理。
        let path = fixture.root.path().join("bin").join(name);
        if path.exists() {
            fs::remove_file(&path).unwrap();
        }
        symlink("/usr/bin/busybox", path).unwrap();
    }
    fixture.env("PIERLINK_MONITOR_PROC_ROOT", "/proc");
    let frames = samples(
        &mut fixture.session(Duration::from_secs(1)),
        2,
        Duration::from_secs(3),
    );
    assert!(frames[0].cpu_percent.is_none() && frames[1].cpu_percent.is_some());
    assert!(frames[0].net_rx_bps.is_none() && frames[1].net_rx_bps.is_some());
    assert!(frames[0].net_tx_bps.is_none() && frames[1].net_tx_bps.is_some());
    fixture.env(
        "PIERLINK_MONITOR_PROC_ROOT",
        fixture.root.path().join("proc").display(),
    );
    // 保留 BusyBox timeout，换入真正带子进程的慢命令。
    let df = fixture.root.path().join("bin/df");
    fs::remove_file(df).unwrap();
    fixture.executable("df", "sleep 30");
    fixture.executable("nvidia-smi", "sleep 30");
    let frame = samples(
        &mut fixture.session(Duration::from_secs(1)),
        1,
        Duration::from_secs(13),
    )
    .remove(0);
    assert!(frame.disks.is_empty());
    assert_eq!(frame.gpu_error.as_deref(), Some("采样超时"));
}

#[test]
fn f8_无proc_终止且网络恢复也不重试() {
    let mut fixture = Fixture::new();
    fixture.env(
        "PIERLINK_MONITOR_PROC_ROOT",
        fixture.root.path().join("missing").display(),
    );
    fixture.record_pid();
    let (monitor, rx) = fixture.monitor(Duration::from_secs(1));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        MonitorEvent::Connecting
    ));
    assert!(
        matches!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), MonitorEvent::Unsupported { reason } if reason == "no_proc")
    );
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        MonitorEvent::Stopped
    ));
    monitor.network_restored();
    monitor.stop();
    let pids = fixture.pids();
    assert_eq!(pids.len(), 1);
    no_process(pids[0]);
    assert!(rx.try_recv().is_err());
}

#[test]
fn s3_认证及主机密钥问题终止_错误中不出现任何服务器原文() {
    for (message, code) in [
        ("Permission denied (publickey)", "ssh.auth_failed"),
        (
            "Too many authentication failures",
            "ssh.too_many_auth_attempts",
        ),
        ("Host key verification failed", "ssh.host_key_unknown"),
        (
            "REMOTE HOST IDENTIFICATION HAS CHANGED",
            "ssh.host_key_changed",
        ),
    ] {
        let mut fixture = Fixture::new();
        fixture.command.args = vec![
            "-c".into(),
            format!("printf '%s\\n' '{message} password=不能泄露' >&2; exit 255"),
        ];
        let (monitor, rx) = fixture.monitor(Duration::from_secs(1));
        let failed = event(&rx, |event| matches!(event, MonitorEvent::Failed { .. }));
        if let MonitorEvent::Failed { error } = failed {
            assert_eq!(error.code, code);
            assert!(!serde_json::to_string(&error).unwrap().contains("不能泄露"));
        }
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            MonitorEvent::Stopped
        ));
        monitor.network_restored();
        monitor.stop();
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn n1_杀掉sh后按退避重建_stop及drop均回收进程() {
    let mut fixture = Fixture::new();
    fixture.record_pid();
    let (monitor, rx) = fixture.monitor(Duration::ZERO);
    event(&rx, |event| matches!(event, MonitorEvent::Sample(_)));
    let pid = fixture.pids()[0];
    let killed_at = Instant::now();
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    let reconnect = event(&rx, |event| {
        matches!(event, MonitorEvent::Reconnecting { .. })
    });
    if let MonitorEvent::Reconnecting {
        attempt,
        next_retry_at_ms,
        ..
    } = reconnect
    {
        assert_eq!(attempt, 1);
        assert!(next_retry_at_ms >= now_ms());
    }
    event(&rx, |event| matches!(event, MonitorEvent::Sample(_)));
    assert!(killed_at.elapsed() >= Duration::from_millis(900));
    assert_eq!(fixture.pids().len(), 2);
    no_process(pid);
    let stopped_at = Instant::now();
    monitor.stop();
    assert!(stopped_at.elapsed() < Duration::from_secs(2));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        MonitorEvent::Stopped
    ));
    for pid in fixture.pids() {
        no_process(pid);
    }
    let (monitor, rx) = fixture.monitor(Duration::from_secs(15));
    event(&rx, |event| matches!(event, MonitorEvent::Sample(_)));
    drop(monitor);
    for pid in fixture.pids() {
        no_process(pid);
    }
}

#[test]
fn n1_n5_无有效帧超时_网络恢复中断退避和活动会话() {
    let mut fixture = Fixture::new();
    fixture.record_pid();
    fixture.command.args[1] = "printf '%s\\n' \"$$\" >> \"$PLMON_PID_FILE\"; while :; do printf '启动噪声\\nPLMON1 {bad} PLMON1\\n'; sleep 0.02; done".into();
    let (monitor, rx) = fixture.monitor(Duration::from_millis(50));
    let reconnect = event(&rx, |event| {
        matches!(event, MonitorEvent::Reconnecting { .. })
    });
    if let MonitorEvent::Reconnecting { error, .. } = reconnect {
        assert_eq!(error.code, "monitor.sample_timeout");
    }
    monitor.network_restored();
    let start = Instant::now();
    event(&rx, |event| matches!(event, MonitorEvent::Connecting));
    assert!(start.elapsed() < Duration::from_millis(500));
    // 当前连接尚无有效帧，收到通知后立即重建。
    monitor.network_restored();
    event(&rx, |event| matches!(event, MonitorEvent::Connecting));
    monitor.stop();
    for pid in fixture.pids() {
        no_process(pid);
    }
}

#[test]
fn 参数上限_序列化事件及类型导出() {
    let fixture = Fixture::new();
    assert!(
        Monitor::start(
            fixture.program.clone(),
            fixture.command.clone(),
            Duration::MAX,
            Arc::new(|_| {})
        )
        .is_err()
    );
    assert!(
        sampling_script(Duration::from_secs(15))
            .unwrap()
            .contains("interval='15'")
    );
    let types = specta::Types::default().register::<MonitorEvent>();
    assert!(!types.is_empty());
    assert_eq!(
        serde_json::to_value(MonitorEvent::Stopped).unwrap(),
        "Stopped"
    );
    assert_eq!(
        serde_json::to_value(MonitorEvent::Unsupported {
            reason: "no_proc".into()
        })
        .unwrap(),
        serde_json::json!({"Unsupported":{"reason":"no_proc"}})
    );
}

#[test]
fn f35_无posix_shell的服务器终止并报告不支持() {
    for message in [
        "The system cannot find the path specified.",
        "'/bin/sh' is not recognized as an internal or external command",
        "/bin/sh: No such file or directory",
        "/bin/sh: not found",
        "The term '/bin/sh' is not recognized as a name of a cmdlet, function, script file, or executable program.\nFullyQualifiedErrorId : CommandNotFoundException\npassword=不能泄露",
        "无法执行采样命令 password=不能泄露",
        "",
    ] {
        let mut fixture = Fixture::new();
        fixture.env("PLMON_SHELL_ERROR", message);
        fixture.command.args = vec![
            "-c".into(),
            "printf '%s\\n' \"$PLMON_SHELL_ERROR\" >&2; exit 127".into(),
        ];
        let (monitor, rx) = fixture.monitor(Duration::from_secs(1));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            MonitorEvent::Connecting
        ));
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), MonitorEvent::Unsupported { reason } if reason == "no_posix_shell")
        );
        monitor.network_restored();
        monitor.stop();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            MonitorEvent::Stopped
        ));
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn f35_n5_连接失败仍重试_已有定界符或成功退出不误判无shell() {
    for (message, stdout, exit_code, code) in [
        ("Connection refused", "", 255, "ssh.unreachable"),
        ("Connection timed out", "", 255, "ssh.unreachable"),
        ("Connection reset by peer", "", 255, "ssh.unreachable"),
        (
            "Connection closed by remote host",
            "",
            255,
            "ssh.unreachable",
        ),
        ("No route to host", "", 255, "ssh.unreachable"),
        ("Network is unreachable", "", 255, "ssh.unreachable"),
        ("Could not resolve hostname", "", 255, "ssh.unreachable"),
        ("Operation timed out", "", 255, "ssh.unreachable"),
        ("采样进程退出", "PLMON1 {bad} PLMON1", 1, "ssh.failed"),
        ("", "启动噪声", 0, "ssh.failed"),
    ] {
        let mut fixture = Fixture::new();
        fixture.env("PLMON_SHELL_ERROR", format!("{message} password=不能泄露"));
        fixture.env("PLMON_STDOUT", stdout);
        fixture.command.args = vec![
            "-c".into(),
            format!(
                "printf '%s\\n' \"$PLMON_STDOUT\"; printf '%s\\n' \"$PLMON_SHELL_ERROR\" >&2; exit {exit_code}"
            ),
        ];
        let (monitor, rx) = fixture.monitor(Duration::from_secs(1));
        let reconnect = event(&rx, |event| {
            matches!(event, MonitorEvent::Reconnecting { .. })
        });
        if let MonitorEvent::Reconnecting { error, attempt, .. } = reconnect {
            assert_eq!(error.code, code);
            assert_eq!(attempt, 1);
            assert!(!serde_json::to_string(&error).unwrap().contains("不能泄露"));
        }
        monitor.network_restored();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            MonitorEvent::Connecting
        ));
        monitor.stop();
    }
}

#[test]
fn 网络通知保留持续出帧的监控会话_故障仍退避重试() {
    let mut fixture = Fixture::new();
    fixture.record_pid();
    let (monitor, rx) = fixture.monitor(Duration::from_millis(100));
    event(&rx, |event| matches!(event, MonitorEvent::Sample(_)));
    let pid = fixture.pids()[0];
    for _ in 0..3 {
        monitor.network_restored();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            MonitorEvent::Sample(_)
        ));
        assert_eq!(fixture.pids(), [pid]);
    }
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    let reconnect = event(&rx, |event| {
        matches!(event, MonitorEvent::Reconnecting { .. })
    });
    if let MonitorEvent::Reconnecting {
        attempt,
        next_retry_at_ms,
        ..
    } = reconnect
    {
        assert_eq!(attempt, 1);
        assert!(next_retry_at_ms > now_ms().saturating_add(500));
    }
    // 处理过的网络通知不能跳过以后发生的故障退避；新的通知仍能立即唤醒。
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    monitor.network_restored();
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(500)).unwrap(),
        MonitorEvent::Connecting
    ));
    event(&rx, |event| matches!(event, MonitorEvent::Sample(_)));
    assert_eq!(fixture.pids().len(), 2);
    no_process(pid);
    monitor.stop();
    for pid in fixture.pids() {
        no_process(pid);
    }
}

#[test]
fn s27_采样子命令读标准输入不会吞掉脚本() {
    let fixture = Fixture::new();
    fixture.executable("nvidia-smi", "exec /bin/cat");
    let frames = samples(
        &mut fixture.session(Duration::from_millis(100)),
        2,
        Duration::from_secs(2),
    );
    assert!(
        frames
            .iter()
            .all(|sample| sample.gpus.is_empty() && sample.gpu_error.is_none())
    );
}

#[test]
fn 远端输出管道断开_下一次采样退出而不残留循环() {
    use std::io::{BufRead, BufReader, Write};
    let fixture = Fixture::new();
    let mut process = fixture.program.spawn(&fixture.command).unwrap();
    process
        .child
        .stdin
        .take()
        .unwrap()
        .write_all(
            sampling_script(Duration::from_millis(100))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    let mut stdout = BufReader::new(process.child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert!(line.starts_with("PLMON1 "));
    drop(stdout);
    let start = Instant::now();
    while process.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(10));
    }
    let pid = process.child.id();
    process.terminate().unwrap();
    no_process(pid);
}

#[test]
fn 采样等待密码不启动_新密码后立即恢复_stop可中断等待() {
    use crate::{security::vault::Zeroizing, ssh::askpass_pipe::test_registration_mode};
    let mut fixture = Fixture::new();
    fixture.record_pid();
    let guard = test_registration_mode("user@host", false);
    fixture.command.askpass = Some(guard.clone());
    let (monitor, rx) = fixture.monitor(Duration::from_secs(1));
    let waiting = event(&rx, |e| matches!(e, MonitorEvent::WaitingPassword { .. }));
    assert!(
        matches!(waiting, MonitorEvent::WaitingPassword { error } if error.code == "ssh.password_needed")
    );
    assert!(!fixture.root.path().join("pids").exists());
    monitor.network_restored();
    assert!(rx.recv_timeout(Duration::from_millis(80)).is_err());
    let start = Instant::now();
    guard.set_password(Zeroizing::new("新密码".into())).unwrap();
    event(&rx, |e| matches!(e, MonitorEvent::Sample(_)));
    assert!(start.elapsed() < Duration::from_secs(1));
    monitor.stop();
    for pid in fixture.pids() {
        no_process(pid);
    }
    let fixture = Fixture::new();
    let mut command = fixture.command.clone();
    command.askpass = Some(test_registration_mode("user@host", false));
    let (tx, rx) = mpsc::channel();
    let monitor = Monitor::start(
        fixture.program.clone(),
        command,
        Duration::from_secs(1),
        Arc::new(move |e| {
            let _ = tx.send(e);
        }),
    )
    .unwrap();
    event(&rx, |e| matches!(e, MonitorEvent::WaitingPassword { .. }));
    let start = Instant::now();
    monitor.stop();
    assert!(start.elapsed() < Duration::from_secs(1));
    event(&rx, |e| matches!(e, MonitorEvent::Stopped));
}

#[test]
fn 磁盘已用取df_used而非总量减可用() {
    let fixture = Fixture::new();
    fixture.executable("df", "printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\\n/dev/root 1000 10 940 2%% /\\n'");
    let sample = samples(
        &mut fixture.session(Duration::from_secs(1)),
        1,
        Duration::from_secs(2),
    )
    .remove(0);
    let disk = &sample.disks[0];
    assert_eq!(
        (disk.total, disk.used, disk.free),
        (1000 * 1024, 10 * 1024, 940 * 1024)
    );
    assert_ne!(disk.used, disk.total - disk.free);
}

#[test]
fn dash_网卡汇总排除lo_两帧差值及读不到重置() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.path().join("proc/net")).unwrap();
    fixture.write_proc("net/dev", "Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n lo: 999999 0 0 0 0 0 0 0 999999 0 0 0 0 0 0 0\n eth0: 100 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0\n eth1: 300 0 0 0 0 0 0 0 400 0 0 0 0 0 0 0\n");
    let mut session = fixture.session(Duration::from_secs(1));
    let first = samples(&mut session, 1, Duration::from_secs(2)).remove(0);
    assert!(first.net_rx_bps.is_none() && first.net_tx_bps.is_none());
    assert_eq!(
        first
            .nets
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>(),
        ["eth0", "eth1"]
    );
    fixture.write_proc("uptime", "12346.67 0\n");
    fixture.write_proc("net/dev", "lo: 999999999 0 0 0 0 0 0 0 999999999 0 0 0 0 0 0 0\neth0: 1100 0 0 0 0 0 0 0 2200 0 0 0 0 0 0 0\neth1: 1300 0 0 0 0 0 0 0 2400 0 0 0 0 0 0 0\n");
    let second = samples(&mut session, 1, Duration::from_secs(2)).remove(0);
    assert_eq!(
        (second.net_rx_bps, second.net_tx_bps),
        (Some(2000.0), Some(4000.0))
    );
    assert_eq!(
        (second.nets[0].rx_bps, second.nets[0].tx_bps),
        (Some(1000.0), Some(2000.0))
    );
    fixture.write_proc("uptime", "12347.67 0\n");
    fixture.write_proc("net/dev", "无法读取有效网卡\n");
    let third = samples(&mut session, 1, Duration::from_secs(2)).remove(0);
    assert!(third.net_rx_bps.is_none() && third.net_tx_bps.is_none());
}

#[test]
fn 真实df输出_used逐列一致() {
    let fixture = Fixture::new();
    let recorded = fixture.root.path().join("df-output");
    fixture.executable(
        "df",
        &format!(
            "/bin/df \"$@\" | /usr/bin/tee {}",
            sh_quote(recorded.to_str().unwrap()).unwrap()
        ),
    );
    let sample = samples(
        &mut fixture.session(Duration::from_secs(1)),
        1,
        Duration::from_secs(2),
    )
    .remove(0);
    let output = fs::read_to_string(recorded).unwrap();
    let columns: Vec<_> = output.lines().nth(1).unwrap().split_whitespace().collect();
    let disk = &sample.disks[0];
    assert_eq!(disk.total, columns[1].parse::<u64>().unwrap() * 1024);
    assert_eq!(disk.used, columns[2].parse::<u64>().unwrap() * 1024);
    assert_eq!(disk.free, columns[3].parse::<u64>().unwrap() * 1024);
}

#[test]
fn 每秒刷新时磁盘约十秒_gpu约两秒采一次() {
    let script = sampling_script(Duration::from_secs(1)).unwrap();
    assert!(script.contains("interval='1'") || script.contains("interval=1"));
    assert!(script.contains("disk_period=10") && script.contains("gpu_period=2"));
    // 间隔本来就长：每轮都采
    let script = sampling_script(Duration::from_secs(15)).unwrap();
    assert!(script.contains("disk_period=10") && script.contains("gpu_period=10"));
}

#[test]
fn 调整间隔改变帧频率并保持原进程与超时规则() {
    let mut fixture = Fixture::new();
    fixture.record_pid();
    let (monitor, rx) = fixture.monitor(Duration::from_millis(100));
    event(&rx, |e| matches!(e, MonitorEvent::Sample(_)));
    monitor.set_interval(Duration::from_millis(400)).unwrap();
    event(&rx, |e| matches!(e, MonitorEvent::Sample(_)));
    let start = Instant::now();
    event(&rx, |e| matches!(e, MonitorEvent::Sample(_)));
    assert!(start.elapsed() >= Duration::from_millis(300));
    monitor.set_interval(Duration::from_millis(80)).unwrap();
    event(&rx, |e| matches!(e, MonitorEvent::Sample(_)));
    let start = Instant::now();
    event(&rx, |e| matches!(e, MonitorEvent::Sample(_)));
    assert!(start.elapsed() < Duration::from_millis(300));
    assert_eq!(fixture.pids().len(), 1);
    assert!(monitor.set_interval(Duration::MAX).is_err());
    assert_eq!(
        sample_timeout(Duration::from_secs(1)).unwrap(),
        Duration::from_secs(20)
    );
    assert_eq!(
        sample_timeout(Duration::from_secs(5)).unwrap(),
        Duration::from_secs(20)
    );
    assert_eq!(
        sample_timeout(Duration::from_secs(10)).unwrap(),
        Duration::from_secs(30)
    );
    assert_eq!(interval_line(Duration::from_secs(5)), "5.000000000 10\n");
    monitor.stop();
    for pid in fixture.pids() {
        no_process(pid);
    }
}

#[test]
fn 改间隔后无帧超时使用最新阈值() {
    let mut fixture = Fixture::new();
    fixture.record_pid();
    fixture.command.args[1] =
        "printf '%s\\n' \"$$\" >> \"$PLMON_PID_FILE\"; exec /bin/sleep 30".into();
    let (monitor, rx) = fixture.monitor(Duration::from_millis(100));
    event(&rx, |e| matches!(e, MonitorEvent::Connecting));
    monitor.set_interval(Duration::from_millis(500)).unwrap();
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
    let start = Instant::now();
    monitor.set_interval(Duration::from_millis(100)).unwrap();
    let event = event(&rx, |e| matches!(e, MonitorEvent::Reconnecting { .. }));
    let MonitorEvent::Reconnecting { error, .. } = event else {
        unreachable!()
    };
    assert_eq!(error.code, "monitor.sample_timeout");
    assert_eq!(error.params["timeout_ms"], "300");
    assert!(start.elapsed() < Duration::from_secs(1));
    monitor.stop();
    for pid in fixture.pids() {
        no_process(pid);
    }
}

#[test]
#[ignore = "需要允许普通用户 sshd 随机监听端口"]
fn 内置引擎改间隔不增加认证或采样通道() {
    let fixture = crate::builtin_test_support::Fixture::new();
    let command = fixture.command();
    let context = command.builtin.as_ref().unwrap();
    let connection = fixture.manager.get_connection(context.request()).unwrap();
    let (tx, rx) = mpsc::channel();
    let monitor = Monitor::start(
        fixture.program(),
        command,
        Duration::from_millis(100),
        Arc::new(move |e| {
            let _ = tx.send(e);
        }),
    )
    .unwrap();
    event(&rx, |e| matches!(e, MonitorEvent::Sample(_)));
    let sessions = connection.statistics().started_sessions;
    monitor.set_interval(Duration::from_millis(500)).unwrap();
    event(&rx, |e| matches!(e, MonitorEvent::Sample(_)));
    let start = Instant::now();
    event(&rx, |e| matches!(e, MonitorEvent::Sample(_)));
    assert!(start.elapsed() >= Duration::from_millis(350));
    monitor.set_interval(Duration::from_millis(100)).unwrap();
    event(&rx, |e| matches!(e, MonitorEvent::Sample(_)));
    let start = Instant::now();
    event(&rx, |e| matches!(e, MonitorEvent::Sample(_)));
    assert!(start.elapsed() < Duration::from_millis(350));
    assert_eq!(connection.statistics().started_sessions, sessions);
    assert!(!connection.is_closed());
    monitor.stop();
}

#[test]
fn 本地挂载排序设备去重数据盘类型与数量上限() {
    let fixture = Fixture::new();
    let mut mounts = String::from("/dev/root /z ext4 rw 0 0\n/dev/root / ext4 rw 0 0\n");
    for (i, fs) in [
        "ext2",
        "ext3",
        "ext4",
        "xfs",
        "btrfs",
        "zfs",
        "f2fs",
        "vfat",
        "exfat",
        "ntfs",
        "ntfs3",
        "tmpfs",
        "devtmpfs",
        "overlay",
        "squashfs",
        "proc",
        "sysfs",
        "cgroup",
        "nfs",
        "cifs",
        "fuse.sshfs",
    ]
    .iter()
    .enumerate()
    {
        mounts.push_str(&format!("/dev/d{i} /data/{i:02} {fs} rw 0 0\n"));
    }
    for i in (0..20).rev() {
        mounts.push_str(&format!("/dev/e{i} /extra/{i:02} ext4 rw 0 0\n"));
    }
    fixture.write_proc("mounts", &mounts);
    fixture.executable("df", "printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\\n/dev/a 100 10 90 10%% %s\\n' \"$2\"");
    let result = samples(
        &mut fixture.session(Duration::from_secs(1)),
        1,
        Duration::from_secs(3),
    )
    .remove(0);
    assert_eq!(result.disks.len(), 16);
    let mut expected = vec!["/".to_string()];
    expected.extend((0..11).map(|i| format!("/data/{i:02}")));
    expected.extend((0..4).map(|i| format!("/extra/{i:02}")));
    assert_eq!(
        result
            .disks
            .iter()
            .map(|d| d.mount.clone())
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn 网络盘覆盖本地挂载时跳过且fuse数据盘可以采样() {
    let fixture = Fixture::new();
    fixture.write_proc("mounts", "/dev/a /covered ext4 rw 0 0\nserver:/data /covered nfs rw 0 0\n/dev/b /data fuseblk rw 0 0\nsshfs /network fuse.sshfs rw 0 0\n");
    fixture.executable("df", "[ \"$2\" = /data ] || exit 2\nprintf 'Filesystem 1024-blocks Used Available Capacity Mounted on\\n/dev/b 100 10 90 10%% /data\\n'");
    let result = samples(
        &mut fixture.session(Duration::from_secs(1)),
        1,
        Duration::from_secs(2),
    )
    .remove(0);
    assert_eq!(result.disks.len(), 1);
    assert_eq!(result.disks[0].mount, "/data");
}
