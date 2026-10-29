use super::*;
use crate::test_support::tempdir;
use std::{
    fs,
    sync::atomic::{AtomicBool, Ordering},
};
fn shell(home: &std::path::Path) -> (SshProgram, SshCommand) {
    set_computer_short_id("test0001").unwrap();
    (
        SshProgram {
            path: "/bin/sh".into(),
        },
        SshCommand {
            args: vec!["-s".into()],
            env: vec![
                ("HOME".into(), home.display().to_string()),
                ("PIERLINK_TEST_MACHINE_ID".into(), "测试机器".into()),
                ("XDG_RUNTIME_DIR".into(), "/run/不存在".into()),
            ],
            config_text: None,
            builtin: None,
            askpass: None,
        },
    )
}
#[test]
fn d1_d2_d4_d5_d8_d10_d12_d15_y9_检查与计划() {
    let dir = tempdir();
    let (program, cmd) = shell(dir.path());
    let mut report = inspect(&program, &cmd).unwrap();
    assert!(report.instance.is_none());
    assert!(report.lock.is_none());
    assert_eq!(
        report.instance_key,
        inspect(&program, &cmd).unwrap().instance_key
    );
    assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
    report.runtime_dir = Some("/run/u".into());
    for state in [
        SystemdState::None,
        SystemdState::UserManagerNotRunning,
        SystemdState::BusUnreachable,
        SystemdState::Available,
    ] {
        report.systemd = state;
        assert_eq!(plan(&report, "v2.1.7").unwrap().systemd, state);
    }
    report.home_noexec = true;
    assert_eq!(
        plan(&report, "2.1.7").unwrap_err().code,
        "deploy.home_noexec"
    );
    report.home_noexec = false;
    report.arch = "riscv64".into();
    report.arch_supported = None;
    assert_eq!(
        plan(&report, "2.1.7").unwrap_err().code,
        "deploy.unsupported_arch"
    );
    report.arch = "aarch64".into();
    report.arch_supported = Some(Arch::Aarch64);
    report.home_fs_type = "nfs".into();
    assert_eq!(plan(&report, "2.1.7").unwrap().arch, Arch::Aarch64);
    let mut second = cmd.clone();
    second
        .env
        .push(("PIERLINK_TEST_MACHINE_ID".into(), "另一机器".into()));
    assert_ne!(
        report.instance_key,
        inspect(&program, &second).unwrap().instance_key
    );
}
#[test]
fn d7_版本与恶意字段() {
    assert!(model::version("v2.10.0").unwrap() > model::version("2.9.9").unwrap());
    for s in [
        "2.1",
        "2.1.1;id",
        "2.1.-1",
        "v1.2.3\n",
        "99999999999999999999999.1.0",
    ] {
        assert!(model::version(s).is_err());
    }
    assert!(!model::path("/x/../y"));
    assert!(!model::path("/x\n"));
    assert!(!model::path("/x//y"));
}
#[test]
fn p5_p6_d14_路径和端口() {
    let id = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        socket_path("/h", Some("/run/u"), &id).unwrap(),
        format!("/run/u/pierlink-{}/api.sock", &id[..8])
    );
    assert!(
        socket_path("/h", Some(&format!("/{}", "x".repeat(120))), &id)
            .unwrap()
            .starts_with("/h/")
    );
    assert!(socket_path(&format!("/{}", "x".repeat(100)), None, &id).is_err());
    assert!(manifest_path_allowed("/h", "/h/.local/state/pierlink/abc"));
    for p in [
        "/etc/passwd",
        "/h/.local/state/pierlink",
        "/h/.local/state/pierlink/../../.ssh",
        "/h/.local/state/pierlink2/a",
    ] {
        assert!(!manifest_path_allowed("/h", p));
    }
    let tcp = " sl local_address rem_address st\n0: 0100007F:55F0 00000000:0000 0A\n1: 00000000:55F1 00000000:0000 01";
    assert_eq!(probe_port(tcp, None).unwrap(), 22001);
    assert_eq!(probe_port(tcp, Some(22001)).unwrap(), 22002);
    assert!(probe_port("坏数据", None).is_err());
}
#[test]
fn d6_上传取消续传校验和_dash() {
    use sha2::{Digest, Sha256};
    let dir = tempdir();
    let home = dir.path().join("home");
    fs::create_dir(&home).unwrap();
    let (mut program, cmd) = shell(&home);
    if std::path::Path::new("/bin/dash").exists() {
        program.path = "/bin/dash".into();
    }
    let bytes: Vec<u8> = (0..3 * 1024 * 1024).map(|_| rand::random()).collect();
    let raw = dir.path().join("raw");
    fs::write(&raw, &bytes).unwrap();
    let gz = dir.path().join("raw.gz");
    let output = std::process::Command::new("gzip")
        .args(["-c"])
        .arg(&raw)
        .output()
        .unwrap();
    assert!(output.status.success());
    fs::write(&gz, output.stdout).unwrap();
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let cancel = AtomicBool::new(false);
    let e = upload_binary(
        &program,
        &cmd,
        &gz,
        &hash,
        "2.1.7",
        Arch::X86_64,
        &|n, _| {
            if n >= 1024 * 1024 {
                cancel.store(true, Ordering::Release);
            }
        },
        &cancel,
    )
    .unwrap_err();
    assert_eq!(e.code, "deploy.cancelled");
    cancel.store(false, Ordering::Release);
    // 片段超过本次总长时，仅在带创建标记的目录重新开始。
    let part = home.join(".local/share/pierlink/bin/2.1.7/amd64/syncthing.gz.part");
    fs::OpenOptions::new()
        .write(true)
        .open(&part)
        .unwrap()
        .set_len(fs::metadata(&gz).unwrap().len() + 1)
        .unwrap();
    let path = upload_binary(
        &program,
        &cmd,
        &gz,
        &hash,
        "2.1.7",
        Arch::X86_64,
        &|_, _| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(
        upload_binary(
            &program,
            &cmd,
            &gz,
            &"0".repeat(64),
            "2.1.8",
            Arch::X86_64,
            &|_, _| {},
            &cancel
        )
        .unwrap_err()
        .code,
        "deploy.checksum_mismatch"
    );
    assert!(
        !home
            .join(".local/share/pierlink/bin/2.1.8/amd64/syncthing")
            .exists()
    );
}
#[test]
fn d11_锁等待与残留接管() {
    let dir = tempdir();
    let (program, cmd) = shell(dir.path());
    std::thread::scope(|scope| {
        let first = scope.spawn(|| run(&program, &cmd, "sleep 3\nprintf true", true).unwrap());
        let lock = dir.path().join(".local/state/pierlink/deploy.lock");
        for _ in 0..100 {
            if lock.join("owner").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let start = std::time::Instant::now();
        assert_eq!(run(&program, &cmd, "printf true", true).unwrap(), true);
        assert!(start.elapsed().as_secs() >= 2);
        first.join().unwrap();
        assert!(!lock.exists());
    });
}
#[test]
fn d16_纯函数加固保留服务器密钥() {
    use crate::syncthing::home::*;
    let spec = HomeSpec {
        home: "/h".into(),
        gui: GuiListen::Unix {
            path: "/run/p/api.sock".into(),
        },
        gui_user: "用户".into(),
        listen_port: 22000,
    };
    let xml =
        "<configuration><gui><password>哈希</password></gui><device id=\"设备\"/></configuration>";
    let result = harden_config_xml(
        xml,
        &HardenSpec {
            home: &spec,
            api_key: "密钥",
        },
    )
    .unwrap();
    assert!(result.contains("<apikey>密钥</apikey>"));
    assert!(result.contains("<address>/run/p/api.sock</address>"));
    assert!(result.contains("<autoUpgradeIntervalH>0</autoUpgradeIntervalH>"));
    assert_eq!(lifecycle::devices(&result).unwrap(), vec!["设备"]);
    assert!(
        harden_config_xml(
            "坏XML",
            &HardenSpec {
                home: &spec,
                api_key: "秘密"
            }
        )
        .is_err()
    );
}
#[test]
#[cfg(unix)]
fn d8_d9_d10_d14_d17_d18_真实实例生命周期() {
    use std::os::unix::fs::PermissionsExt;
    let Some(binary) = std::env::var_os("PIERLINK_TEST_SYNCTHING")
        .or_else(|| std::env::var_os("PIERLINK_TEST_SYNCTHING_GENERATE"))
    else {
        eprintln!("跳过真实部署生命周期：需要 PIERLINK_TEST_SYNCTHING 和监听权限");
        return;
    };
    let dir = short_tempdir();
    let home = dir.path().join("h");
    let runtime = dir.path().join("r");
    let tools_dir = dir.path().join("b");
    for p in [&home, &runtime, &tools_dir] {
        fs::create_dir(p).unwrap();
    }
    fs::write(tools_dir.join("systemctl"), "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(
        tools_dir.join("systemctl"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let (program, mut cmd) = shell(&home);
    // 真实进程只能使用本次测试的运行目录，不能回退到当前用户的 /run/user。
    cmd.env.retain(|(name, _)| name != "XDG_RUNTIME_DIR");
    cmd.env
        .push(("XDG_RUNTIME_DIR".into(), runtime.display().to_string()));
    cmd.env.push((
        "PATH".into(),
        format!("{}:/usr/bin:/bin", tools_dir.display()),
    ));
    let report = inspect(&program, &cmd).unwrap();
    let mut plan = plan(&report, "2.1.7").unwrap();
    let target = home.join(format!(
        ".local/share/pierlink/bin/2.1.7/{}/syncthing",
        plan.arch.directory()
    ));
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::copy(&binary, &target).unwrap();
    let generate_args = home.join("generate-args");
    let wrapper = format!(
        "#!/bin/sh\nif [ \"$1\" = generate ]; then printf '%s\\n' \"$@\" > {}; fi\nexec {} \"$@\"\n",
        quote(generate_args.to_str().unwrap()).unwrap(),
        quote(std::path::Path::new(&binary).to_str().unwrap()).unwrap()
    );
    fs::write(&target, wrapper).unwrap();
    let secrets = InstanceSecrets {
        api_key: "DeployTest0123456789".into(),
        gui_user: "测试用户".into(),
        gui_password: "测试口令&带空格".into(),
    };
    assert_eq!(
        create_instance(&program, &cmd, &plan, &secrets)
            .unwrap_err()
            .code,
        "deploy.consent_required"
    );
    plan.autostart_consent = true;
    let created = create_instance(&program, &cmd, &plan, &secrets).unwrap();
    assert!(created.secrets_applied);
    let info = created.info;
    let generated_config =
        fs::read_to_string(format!("{}/home/config.xml", info.state_dir)).unwrap();
    for name in ["manifest.json", "info.json"] {
        fs::remove_file(format!("{}/{name}", info.state_dir)).unwrap();
    }
    let pending_secrets = InstanceSecrets {
        api_key: secrets.api_key.clone(),
        gui_user: secrets.gui_user.clone(),
        gui_password: "中断重建的新口令".into(),
    };
    let pending = create_instance(&program, &cmd, &plan, &pending_secrets).unwrap();
    assert!(pending.secrets_applied);
    let pending_config = fs::read_to_string(format!("{}/home/config.xml", info.state_dir)).unwrap();
    assert_ne!(
        lifecycle::gui_value(&generated_config, "password").unwrap(),
        lifecycle::gui_value(&pending_config, "password").unwrap()
    );
    struct Stop<'a>(&'a SshProgram, &'a SshCommand, String);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            let _ = stop_instance(self.0, self.1, &self.2);
        }
    }
    let _guard = Stop(&program, &cmd, info.id.clone());
    let args = fs::read_to_string(&generate_args).unwrap();
    assert!(args.contains("--gui-user=pierlink-pending"));
    assert!(!args.contains(&secrets.gui_user));
    assert!(!args.contains(&pending_secrets.gui_password));
    fs::copy(&binary, &target).unwrap();
    assert_eq!(
        create_instance(&program, &cmd, &plan, &secrets)
            .unwrap()
            .info
            .id,
        info.id
    );
    assert_eq!(
        fs::metadata(format!("{}/home/config.xml", info.state_dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!home.join("Sync").exists());
    let config_file = format!("{}/home/config.xml", info.state_dir);
    let old_config = fs::read_to_string(&config_file).unwrap();
    assert_eq!(
        read_api_key(&program, &cmd, &info.id).unwrap(),
        secrets.api_key
    );
    let different = InstanceSecrets {
        api_key: "Other0123456789_KEY".into(),
        gui_user: "另一用户".into(),
        gui_password: "另一口令".into(),
    };
    assert!(
        !create_instance(&program, &cmd, &plan, &different)
            .unwrap()
            .secrets_applied
    );
    assert_eq!(fs::read_to_string(&config_file).unwrap(), old_config);
    let mut upgrade = super::plan(&inspect(&program, &cmd).unwrap(), "2.1.8").unwrap();
    let upgraded_binary = target.to_str().unwrap().replace("2.1.7", "2.1.8");
    assert_eq!(
        create_instance(&program, &cmd, &upgrade, &different)
            .unwrap_err()
            .code,
        "deploy.binary_missing"
    );
    fs::create_dir_all(std::path::Path::new(&upgraded_binary).parent().unwrap()).unwrap();
    fs::copy(&target, &upgraded_binary).unwrap();
    upgrade.upgrade_confirmed = true;
    let upgraded = create_instance(&program, &cmd, &upgrade, &different).unwrap();
    assert!(!upgraded.secrets_applied);
    let upgraded_config = fs::read_to_string(&config_file).unwrap();
    for tag in ["apikey", "user", "password"] {
        assert_eq!(
            lifecycle::gui_value(&old_config, tag).unwrap(),
            lifecycle::gui_value(&upgraded_config, tag).unwrap()
        );
    }
    let info = upgraded.info;

    if std::env::var_os("PIERLINK_TEST_SYNCTHING").is_some() {
        let outcome = start_instance(&program, &cmd, &info).unwrap();
        assert!(matches!(
            outcome,
            StartOutcome::Nohup {
                restart_after_reboot: true,
                ..
            }
        ));
        assert!(inspect(&program, &cmd).unwrap().instance.unwrap().running);
        assert!(matches!(
            start_instance(&program, &cmd, &info).unwrap(),
            StartOutcome::AlreadyRunning { .. }
        ));
        // 可执行文件替换后，旧进程的 /proc/exe 带 deleted 后缀，仍应能停止。
        fs::rename(&upgraded_binary, format!("{upgraded_binary}.old")).unwrap();
        fs::copy(&target, &upgraded_binary).unwrap();
        fs::remove_file(format!("{upgraded_binary}.old")).unwrap();
        stop_instance(&program, &cmd, &info.id).unwrap();
    }
    assert!(!inspect(&program, &cmd).unwrap().instance.unwrap().running);
    let manifest_path = format!("{}/manifest.json", info.state_dir);
    let mut manifest: model::Manifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest
        .paths
        .push(home.join("precious").display().to_string());
    fs::write(home.join("precious"), "不删除").unwrap();
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    fs::write(&config_file, "<configuration><device").unwrap();
    let preview = uninstall(&program, &cmd, &info.id, true).unwrap();
    assert!(preview.devices_unavailable);
    fs::remove_file(&config_file).unwrap();
    assert!(
        uninstall(&program, &cmd, &info.id, true)
            .unwrap()
            .devices_unavailable
    );
    assert!(
        inspect(&program, &cmd)
            .unwrap()
            .instance
            .unwrap()
            .devices
            .is_empty()
    );
    assert!(std::path::Path::new(&info.state_dir).exists());
    assert_eq!(preview.skipped.len(), 1);
    let actual = uninstall(&program, &cmd, &info.id, false).unwrap();
    assert_eq!(preview.will_delete, actual.will_delete);
    assert!(!std::path::Path::new(&info.state_dir).exists());
    assert!(home.join("precious").exists());
    assert!(target.exists());
}
#[test]
fn d13_目录归属与已有忽略文件() {
    let root = tempdir();
    let (program, cmd) = shell(root.path());
    let id = uuid::Uuid::new_v4().to_string();
    let home = root.path().join(format!(".local/state/pierlink/{id}/home"));
    fs::create_dir_all(&home).unwrap();
    let project = root.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::write(project.join(".stignore"), "项目规则").unwrap();
    fs::write(home.join("config.xml"), "<configuration/>").unwrap();
    assert!(
        check_sync_directory(&program, &cmd, &id, project.to_str().unwrap())
            .unwrap()
            .has_ignore_file
    );
    fs::create_dir(project.join(".stfolder")).unwrap();
    let check = check_sync_directory(&program, &cmd, &id, project.to_str().unwrap()).unwrap();
    assert!(check.marker_present);
    assert_eq!(check.marker_folder_id, None);
    fs::write(project.join(".stfolder/syncthing-folder-pl-test.txt"), "").unwrap();
    assert_eq!(
        check_sync_directory(&program, &cmd, &id, project.to_str().unwrap())
            .unwrap()
            .marker_folder_id
            .as_deref(),
        Some("pl-test")
    );
    fs::remove_file(project.join(".stfolder/syncthing-folder-pl-test.txt")).unwrap();
    fs::write(project.join(".stfolder/syncthing-folder-external.txt"), "").unwrap();
    let external = check_sync_directory(&program, &cmd, &id, project.to_str().unwrap()).unwrap();
    assert_eq!(external.marker_folder_id.as_deref(), Some("external"));
    assert!(!external.already_managed);
    fs::write(
        home.join("config.xml"),
        format!(
            "<configuration><folder path=\"{}\"/></configuration>",
            project.display()
        ),
    )
    .unwrap();
    assert!(
        check_sync_directory(&program, &cmd, &id, project.to_str().unwrap())
            .unwrap()
            .already_managed
    );
}
#[test]
fn d3_d16_d18_服务参数与日志限额() {
    let id = uuid::Uuid::new_v4().to_string();
    let info = InstanceInfo {
        id: id.clone(),
        version: "2.1.7".into(),
        binary: "/h/.local/share/pierlink/bin/2.1.7/amd64/syncthing".into(),
        state_dir: format!("/h/.local/state/pierlink/{id}"),
        socket_path: "/run/u/api.sock".into(),
        sync_port: 22000,
        device_id: "设备".into(),
        instance_key: "1234567890abcdef".into(),
        autostart_consent: true,
    };
    let script = lifecycle::start_script(&info, "/h", 30).unwrap();
    for item in [
        "Restart=on-failure",
        "RestartSec=10",
        "STNOUPGRADE=1",
        "--gui-address=unix:///run/u/api.sock",
        "--log-max-size=10485760",
        "--log-max-old-files=1",
        "-mtime +29",
        "-lt 30",
        "tail -n 200",
    ] {
        assert!(script.contains(item), "{item}");
    }
    assert!(!script.contains("--gui-address=/run"));
    assert!(
        lifecycle::start_script(&info, "/h", 90)
            .unwrap()
            .contains("-lt 90")
    );
    for shell in ["/bin/sh", "/bin/dash"] {
        if std::path::Path::new(shell).exists() {
            use std::io::Write;
            let mut child = std::process::Command::new(shell)
                .args(["-n"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(format!("{}\n{}", include_str!("common.sh"), script).as_bytes())
                .unwrap();
            assert!(child.wait().unwrap().success());
        }
    }
}
#[test]
fn d7_计划只升不降且列出设备() {
    let root = tempdir();
    let (program, cmd) = shell(root.path());
    let mut report = inspect(&program, &cmd).unwrap();
    report.runtime_dir = Some("/run/u".into());
    let p = plan(&report, "2.1.7").unwrap();
    report.instance = Some(InstanceReport {
        info: InstanceInfo {
            id: p.instance_id.clone(),
            version: "2.1.7".into(),
            binary: format!(
                "{}/.local/share/pierlink/bin/2.1.7/amd64/syncthing",
                report.home
            ),
            state_dir: format!("{}/.local/state/pierlink/{}", report.home, p.instance_id),
            socket_path: p.socket_path,
            sync_port: 22000,
            device_id: "设备".into(),
            instance_key: p.instance_key,
            autostart_consent: true,
        },
        running: false,
        devices: vec!["另一电脑".into()],
    });
    let same = plan(&report, "v2.1.7").unwrap();
    assert_eq!(same.version, "2.1.7");
    assert!(!same.new_instance);
    assert!(!same.upload_required);
    assert!(same.autostart_consent);
    let upgrade = plan(&report, "2.2.0").unwrap();
    assert!(upgrade.upgrade);
    assert_eq!(upgrade.other_devices, vec!["另一电脑"]);
    assert!(!upgrade.upgrade_confirmed);
    let local = "AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA";
    let mut upgraded = upgrade.clone();
    upgraded.other_devices.push(local.into());
    upgraded.identify_local_device(local).unwrap();
    assert!(!upgraded.other_devices.contains(&local.into()));
    assert!(upgraded.identify_local_device("错误设备").is_err());
    assert_eq!(
        plan(&report, "2.1.6").unwrap_err().code,
        "deploy.instance_newer"
    );
}
#[test]
fn d11_残留锁接管与错误释放() {
    let root = tempdir();
    let (program, cmd) = shell(root.path());
    let lock = root.path().join(".local/state/pierlink/deploy.lock");
    fs::create_dir_all(&lock).unwrap();
    fs::write(lock.join("owner"), "旧电脑-旧时间").unwrap();
    let reap = lock.with_file_name("deploy.reap");
    fs::create_dir(&reap).unwrap();
    assert!(
        std::process::Command::new("touch")
            .args(["-d", "20 minutes ago"])
            .arg(&reap)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("touch")
            .args(["-d", "20 minutes ago"])
            .arg(&lock)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        run(&program, &cmd, "printf '%s' \"$taken_over\"", true).unwrap(),
        true
    );
    assert!(!lock.exists());
    assert!(!lock.with_file_name("deploy.reap").exists());
    assert_eq!(
        run(&program, &cmd, "fail generate_failed", true)
            .unwrap_err()
            .code,
        "deploy.generate_failed"
    );
    assert!(!lock.exists());
}
#[test]
fn 检查报告不可信字段和系统状态夹具() {
    let dir = tempdir();
    let (program, cmd) = shell(dir.path());
    let report = inspect(&program, &cmd).unwrap();
    let fixture = serde_json::to_value(report).unwrap();
    for state in [
        "none",
        "user_manager_not_running",
        "bus_unreachable",
        "available",
    ] {
        let mut v = fixture.clone();
        v["systemd"] = state.into();
        v["linger"] = false.into();
        v["kill_user_processes"] = true.into();
        v["home_fs_type"] = "nfs".into();
        v["home_noexec"] = true.into();
        let report: ServerReport = decode(v).unwrap();
        report.validate().unwrap();
        assert_eq!(report.linger, Some(false));
        assert!(report.home_noexec);
    }
    for (field, value) in [
        ("home", "/x\n"),
        ("user", "用户\0"),
        ("arch", "x"),
        ("instance_key", "../x"),
        ("systemd", "unknown"),
    ] {
        let mut v = fixture.clone();
        v[field] = value.into();
        assert!(
            decode::<ServerReport>(v)
                .and_then(|r| r.validate())
                .is_err()
        );
    }
}
#[test]
#[cfg(unix)]
fn 路径符号链接拒绝写入且错误不泄密() {
    use std::os::unix::fs::symlink;
    let root = tempdir();
    let home = root.path().join("h");
    let outside = root.path().join("outside");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&outside).unwrap();
    symlink(&outside, home.join(".local")).unwrap();
    let (program, cmd) = shell(&home);
    assert_eq!(
        run(&program, &cmd, "printf true", true).unwrap_err().code,
        "deploy.unsafe_path"
    );
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
    let e = run(&program, &cmd, "printf '秘密配置和口令' >&2; exit 7", false).unwrap_err();
    assert!(!format!("{e:?}").contains("秘密"));
}
#[test]
fn d14_共享程序目录不删除() {
    let root = tempdir();
    let home = root.path().to_str().unwrap();
    let (program, cmd) = shell(root.path());
    let id = uuid::Uuid::new_v4().to_string();
    let state = format!("{home}/.local/state/pierlink/{id}");
    let binary_dir = format!("{home}/.local/share/pierlink/bin/2.1.7/amd64");
    fs::create_dir_all(format!("{state}/home")).unwrap();
    fs::create_dir_all(format!("{state}/run")).unwrap();
    fs::create_dir_all(&binary_dir).unwrap();
    fs::write(format!("{binary_dir}/.pierlink-created"), "true").unwrap();
    let info = InstanceInfo {
        id: id.clone(),
        version: "2.1.7".into(),
        binary: format!("{binary_dir}/syncthing"),
        state_dir: state.clone(),
        socket_path: "/run/pl/api.sock".into(),
        sync_port: 22000,
        device_id: "设备".into(),
        instance_key: "1234567890abcdef".into(),
        autostart_consent: true,
    };
    let manifest = model::Manifest {
        startup_wait_seconds: 30,
        source_digest: String::new(),
        info,
        paths: vec![state.clone(), binary_dir.clone()],
        devices: vec![],
    };
    fs::write(
        format!("{state}/manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(format!("{state}/home/config.xml"), "<configuration/>").unwrap();
    let other = root.path().join(".local/state/pierlink/other");
    fs::create_dir(&other).unwrap();
    let mut shared = manifest.clone();
    shared.paths = vec![binary_dir.clone()];
    fs::write(
        other.join("manifest.json"),
        serde_json::to_vec(&shared).unwrap(),
    )
    .unwrap();
    // 其他实例的清单截断时无法判断引用，程序目录仍须保留。
    fs::write(other.join("manifest.json"), "{截断").unwrap();
    fs::write(format!("{state}/home/config.xml"), "<configuration>").unwrap();
    let own_path = format!("{state}/manifest.json");
    let mut legacy = serde_json::to_value(&manifest).unwrap();
    legacy["future"] = true.into();
    legacy["info"]["future"] = true.into();
    legacy.as_object_mut().unwrap().remove("devices");
    legacy
        .as_object_mut()
        .unwrap()
        .remove("startup_wait_seconds");
    legacy["info"].as_object_mut().unwrap().remove("sync_port");
    fs::write(&own_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
    let report = uninstall(&program, &cmd, &id, false).unwrap();
    assert!(report.devices_unavailable);
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].reason.contains("其他实例"));
    assert!(std::path::Path::new(&binary_dir).exists());
    assert!(!std::path::Path::new(&state).exists());
}

fn uninstall_fixture(home: &std::path::Path) -> model::Manifest {
    let id = uuid::Uuid::new_v4().to_string();
    let state = home.join(format!(".local/state/pierlink/{id}"));
    let binary = home.join(".local/share/pierlink/bin/2.1.7/amd64");
    let registry = home.join(".local/state/pierlink/instances/1234567890abcdef");
    let unit = home.join(format!(".config/systemd/user/pierlink-{id}.service"));
    fs::create_dir_all(state.join("home")).unwrap();
    fs::create_dir_all(&binary).unwrap();
    for path in [&registry, &unit] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "测试部署记录").unwrap();
    }
    fs::write(binary.join(".pierlink-created"), "true").unwrap();
    fs::write(
        state.join("home/config.xml"),
        "<configuration><device id=\"服务器\"/><device id=\"本电脑\"/></configuration>",
    )
    .unwrap();
    let manifest = model::Manifest {
        info: InstanceInfo {
            id,
            version: "2.1.7".into(),
            binary: binary.join("syncthing").display().to_string(),
            state_dir: state.display().to_string(),
            socket_path: state.join("api.sock").display().to_string(),
            sync_port: 22000,
            device_id: "服务器".into(),
            instance_key: "1234567890abcdef".into(),
            autostart_consent: true,
        },
        paths: vec![binary, registry, unit, state.clone()]
            .into_iter()
            .map(|p| p.display().to_string())
            .collect(),
        ..Default::default()
    };
    fs::write(
        state.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    manifest
}

#[test]
fn 卸载清理空父目录并保留用户配置() {
    for user_config in [false, true] {
        let root = tempdir();
        let home = root.path().join("家目录 ' $(不执行)");
        fs::create_dir(&home).unwrap();
        let own = home.join(".config/其他程序/配置");
        if user_config {
            fs::create_dir_all(own.parent().unwrap()).unwrap();
            fs::write(&own, "用户自己的配置").unwrap();
        }
        let manifest = uninstall_fixture(&home);
        let (program, cmd) = shell(&home);
        let preview = uninstall(&program, &cmd, &manifest.info.id, true).unwrap();
        assert!(preview.skipped.is_empty());
        for path in &manifest.paths {
            assert!(std::path::Path::new(path).exists());
        }
        let actual = uninstall(&program, &cmd, &manifest.info.id, false).unwrap();
        assert_eq!(actual.will_delete, preview.will_delete);
        assert!(home.is_dir(), "不能删除家目录");
        assert!(!home.join(".local").exists());
        if user_config {
            assert_eq!(fs::read_to_string(own).unwrap(), "用户自己的配置");
            assert_eq!(fs::read_dir(&home).unwrap().count(), 1);
        } else {
            assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
        }
    }
}

#[test]
fn 跳过项其他实例多电脑或未删除时不清理父目录() {
    for case in [
        "跳过项",
        "其他实例",
        "多电脑",
        "未删除",
        "动态跳过",
        "设备未知",
    ] {
        let root = tempdir();
        let home = root.path();
        let mut manifest = uninstall_fixture(home);
        let state = std::path::Path::new(&manifest.info.state_dir);
        match case {
            "跳过项" => manifest
                .paths
                .push(home.join("用户文件").display().to_string()),
            "其他实例" => {
                let other = home.join(".local/state/pierlink/other");
                fs::create_dir(&other).unwrap();
                fs::write(other.join("manifest.json"), "{}").unwrap();
            }
            "多电脑" => fs::write(
                state.join("home/config.xml"),
                "<configuration><device id=\"本电脑\"/><device id=\"另一电脑\"/></configuration>",
            )
            .unwrap(),
            "未删除" => manifest.paths.clear(),
            "动态跳过" => fs::remove_file(
                home.join(".local/share/pierlink/bin/2.1.7/amd64/.pierlink-created"),
            )
            .unwrap(),
            "设备未知" => fs::write(state.join("home/config.xml"), "损坏的配置").unwrap(),
            _ => unreachable!(),
        }
        fs::write(
            state.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let (program, cmd) = shell(home);
        uninstall(&program, &cmd, &manifest.info.id, false).unwrap();
        assert!(home.join(".config/systemd/user").is_dir(), "{case}");
        assert!(
            home.join(".local/share/pierlink/bin/2.1.7").is_dir(),
            "{case}"
        );
    }
}

#[test]
fn 本机短编号仅允许合法且稳定的设置() {
    assert!(set_computer_short_id("非法编号").is_err());
    set_computer_short_id("test0001").unwrap();
    set_computer_short_id("test0001").unwrap();
    assert_eq!(
        set_computer_short_id("changed1").unwrap_err().code,
        "deploy.identity_conflict"
    );
}
#[test]
#[cfg(unix)]
fn p6_模拟管理器端口竞争重试与危险监听拒绝() {
    use std::os::unix::fs::PermissionsExt;
    let root = short_tempdir();
    let home = root.path().join("h");
    let runtime = root.path().join("r");
    let bin = root.path().join("b");
    for p in [&home, &runtime, &bin] {
        fs::create_dir(p).unwrap();
    }
    let id = uuid::Uuid::new_v4().to_string();
    let state = home.join(format!(".local/state/pierlink/{id}"));
    for name in ["home", "run", "logs", "versions", "conflicts"] {
        fs::create_dir_all(state.join(name)).unwrap();
    }
    let info = InstanceInfo {
        id: id.clone(),
        version: "2.1.7".into(),
        binary: format!(
            "{}/.local/share/pierlink/bin/2.1.7/amd64/syncthing",
            home.display()
        ),
        state_dir: state.display().to_string(),
        socket_path: format!("{}/pierlink-{}/api.sock", runtime.display(), &id[..8]),
        sync_port: 22000,
        device_id: "设备".into(),
        instance_key: "1234567890abcdef".into(),
        autostart_consent: true,
    };
    let manifest = model::Manifest {
        startup_wait_seconds: 30,
        source_digest: String::new(),
        info: info.clone(),
        paths: vec![info.state_dir.clone()],
        devices: vec![],
    };
    fs::write(
        state.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(state.join("home/config.xml"),"<configuration><options><listenAddress>tcp://127.0.0.1:22000</listenAddress></options></configuration>").unwrap();
    // 仅模拟 systemctl 的结果和日志，不冒充真实 systemd 端到端验收。
    fs::write(bin.join("systemctl"),r#"#!/bin/sh
case "$2" in
 enable)
 if [ -f "$TEST_STATE/unsafe" ]; then
   printf 'GUI and API listening (address="[::]:12345" log.pkg=api)\n' > "$TEST_STATE/logs/syncthing.log"
 elif [ -f "$TEST_STATE/attempt" ]; then
   awk 'BEGIN {for(i=0;i<21846;i++)printf "中";printf "\n"}' > "$TEST_STATE/logs/syncthing.log"
   printf 'GUI and API listening (address=%s log.pkg=api)\n' "$TEST_SOCKET" >> "$TEST_STATE/logs/syncthing.log"
 else
   printf 'address already in use\n' > "$TEST_STATE/logs/syncthing.log"
   touch "$TEST_STATE/attempt"
 fi;;
 stop) touch "$TEST_STATE/stopped";;
 daemon-reload) printf x >> "$HOME/reloads";;
esac
exit 0
"#).unwrap();
    fs::set_permissions(bin.join("systemctl"), fs::Permissions::from_mode(0o700)).unwrap();
    let (program, mut cmd) = shell(&home);
    cmd.env.extend([
        ("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())),
        ("XDG_RUNTIME_DIR".into(), runtime.display().to_string()),
        ("TEST_STATE".into(), info.state_dir.clone()),
        ("TEST_SOCKET".into(), info.socket_path.clone()),
    ]);
    let StartOutcome::Systemd { instance, .. } = start_instance(&program, &cmd, &info).unwrap()
    else {
        panic!("应使用模拟管理器");
    };
    assert_ne!(instance.sync_port, 22000);
    assert!(
        fs::read_to_string(state.join("home/config.xml"))
            .unwrap()
            .contains(&format!("tcp://127.0.0.1:{}", instance.sync_port))
    );
    let unit = fs::read_to_string(home.join(format!(".config/systemd/user/pierlink-{id}.service")))
        .unwrap();
    assert!(unit.contains(&format!("--gui-address=unix://{}", info.socket_path)));
    assert!(unit.contains("RuntimeDirectoryMode=0700"));
    fs::write(state.join("unsafe"), "").unwrap();
    assert_eq!(
        start_instance(&program, &cmd, &instance).unwrap_err().code,
        "deploy.unsafe_listener"
    );
    assert!(state.join("stopped").exists());
    fs::remove_file(state.join("unsafe")).unwrap();
    let fallback = home.join(format!(".local/state/pierlink/r/{}", &id[..8]));
    let mut updated = manifest;
    updated.info.socket_path = socket_path(home.to_str().unwrap(), None, &id).unwrap();
    updated.info.validate(home.to_str().unwrap()).unwrap();
    updated.startup_wait_seconds = 90;
    updated.paths.push(fallback.display().to_string());
    fs::write(
        state.join("manifest.json"),
        serde_json::to_vec(&updated).unwrap(),
    )
    .unwrap();
    cmd.env
        .push(("TEST_SOCKET".into(), updated.info.socket_path.clone()));
    start_instance(&program, &cmd, &updated.info).unwrap();
    assert_eq!(
        fs::metadata(&fallback).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let stored: model::Manifest =
        serde_json::from_slice(&fs::read(state.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(stored.startup_wait_seconds, 30);
    let before = fs::read(home.join("reloads")).unwrap().len();
    uninstall(&program, &cmd, &id, false).unwrap();
    assert_eq!(fs::read(home.join("reloads")).unwrap().len(), before + 1);
    assert!(!fallback.exists());
}

// 套接字测试也使用本次测试共用的数据盘临时根目录。
fn short_tempdir() -> tempfile::TempDir {
    tempdir()
}

#[test]
fn p6_ci临时根目录过深导致回退套接字报告无效() {
    let id = uuid::Uuid::new_v4().to_string();
    // CI 原来的临时根目录在 crates/core 内；首次运行目录套接字合法，回退路径才超长。
    let ci_workspace = "/home/runner/work/Pierlink/Pierlink";
    let deep_home = format!("{ci_workspace}/crates/core/pl-123456/t123456/h");
    let mut info = InstanceInfo {
        id: id.clone(),
        version: "2.1.7".into(),
        binary: format!("{deep_home}/.local/share/pierlink/bin/2.1.7/amd64/syncthing"),
        state_dir: format!("{deep_home}/.local/state/pierlink/{id}"),
        socket_path: socket_path(&deep_home, Some("/run/test"), &id).unwrap(),
        sync_port: 22000,
        device_id: "设备".into(),
        instance_key: "1234567890abcdef".into(),
        autostart_consent: true,
    };
    info.validate(&deep_home).unwrap();
    info.socket_path = format!("{deep_home}/.local/state/pierlink/r/{}/api.sock", &id[..8]);
    assert_eq!(info.socket_path.len(), 109);
    assert_eq!(
        info.validate(&deep_home).unwrap_err().code,
        "deploy.invalid_report"
    );
    assert_eq!(
        socket_path(&deep_home, None, &id).unwrap_err().code,
        "deploy.socket_path_too_long"
    );
    let home = format!("{ci_workspace}/pl-123456/t123456/h");
    info.binary = format!("{home}/.local/share/pierlink/bin/2.1.7/amd64/syncthing");
    info.state_dir = format!("{home}/.local/state/pierlink/{id}");
    info.socket_path = socket_path(&home, None, &id).unwrap();
    assert_eq!(info.socket_path.len(), 97);
    info.validate(&home).unwrap();
}
#[test]
fn 读取密钥校验与不泄密() {
    let root = tempdir();
    let (program, cmd) = shell(root.path());
    let id = uuid::Uuid::new_v4().to_string();
    let home = root.path().join(format!(".local/state/pierlink/{id}/home"));
    fs::create_dir_all(&home).unwrap();
    for key in ["Abcd0123456789_-", "a", "秘密不应回显", &"a".repeat(129)] {
        fs::write(
            home.join("config.xml"),
            format!("<configuration><gui><apikey>{key}</apikey></gui></configuration>"),
        )
        .unwrap();
        let result = read_api_key(&program, &cmd, &id);
        if key.len() == 16 {
            assert_eq!(result.unwrap(), key);
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.code, "deploy.invalid_api_key");
            assert!(error.params.is_empty());
        }
    }
    assert!(read_api_key(&program, &cmd, "../bad").is_err());
}
#[test]
fn 清单兼容默认字段及短回退路径() {
    let m: model::Manifest =
        serde_json::from_str(r#"{"info":{"future":true},"future":42}"#).unwrap();
    assert!(m.paths.is_empty());
    assert!(!m.info.autostart_consent);
    let id = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        socket_path("/h", None, &id).unwrap(),
        format!("/h/.local/state/pierlink/r/{}/api.sock", &id[..8])
    );
    let error = socket_path(&format!("/{}", "x".repeat(100)), None, &id).unwrap_err();
    assert_eq!(error.params["home_bytes"], "101");
}

#[test]
fn d9_程序文件被替换后仍可停止() {
    use std::os::unix::process::CommandExt;
    let root = tempdir();
    let (program, cmd) = shell(root.path());
    let id = uuid::Uuid::new_v4().to_string();
    let state = root.path().join(format!(".local/state/pierlink/{id}"));
    let binary_dir = root.path().join(".local/share/pierlink/bin/2.1.7/amd64");
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(&binary_dir).unwrap();
    let binary = binary_dir.join("syncthing");
    fs::copy("/bin/sleep", &binary).unwrap();
    let manifest = model::Manifest {
        info: InstanceInfo {
            id: id.clone(),
            state_dir: state.display().to_string(),
            binary: binary.display().to_string(),
            instance_key: "1234567890abcdef".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    fs::write(
        state.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Child(
        std::process::Command::new(&binary)
            .arg0(format!("--home={}/home", state.display()))
            .arg("60")
            .spawn()
            .unwrap(),
    );
    let exe = format!("/proc/{}/exe", child.0.id());
    for _ in 0..100 {
        if fs::read_link(&exe).ok().as_ref() == Some(&binary) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(fs::read_link(&exe).unwrap(), binary);
    fs::remove_file(&binary).unwrap();
    fs::copy("/bin/sleep", &binary).unwrap();
    let deleted = format!("{} (deleted)", binary.display());
    for _ in 0..100 {
        if fs::read_link(&exe).unwrap().to_str() == Some(&deleted) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        fs::read_link(&exe).unwrap().to_str(),
        Some(deleted.as_str())
    );
    stop_instance(&program, &cmd, &id).unwrap();
    assert!(!child.0.wait().unwrap().success());
}
