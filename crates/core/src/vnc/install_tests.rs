use super::{
    desktop_tests::{local, script_file},
    install_check::{
        FALLBACK_DOWNLOAD_KIB, FALLBACK_INSTALLED_KIB, INSTALL_MARGIN_KIB, RawCheck, evaluate,
    },
    *,
};
use crate::ssh::args::SshCommand;
use serde_json::{Value, json};
use std::{fs, path::Path};

/// 假的 id、sudo、df、nproc、apt-get、apt-cache，行为由 FAKE_* 环境变量控制，调用记在家目录里
fn fake_tools(home: &Path, cmd: &mut SshCommand) {
    let bin = home.join("tools");
    fs::create_dir(&bin).unwrap();
    cmd.env
        .push(("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())));
    script_file(
        &bin.join("id"),
        r#"#!/bin/sh
case "$1" in
  -u) printf '%s\n' "${FAKE_UID:-1000}";;
  -Gn) printf '%s\n' "${FAKE_GROUPS:-users}";;
  *) exec /usr/bin/id "$@";;
esac
"#,
    );
    script_file(
        &bin.join("sudo"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/sudo-calls\"\nexit \"${FAKE_SUDO_EXIT:-1}\"\n",
    );
    script_file(
        &bin.join("df"),
        r#"#!/bin/sh
for arg do target=$arg; done
printf '%s\n' "$target" >> "$HOME/df-calls"
printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\n'
case "$target" in
  /usr*) printf '/dev/vda2 41168220 12312772 %s 32%% /\n' "${FAKE_USR_FREE:-27056756}";;
  *) printf '/dev/vdb 41168220 12312772 %s 32%% %s\n' "${FAKE_CACHE_FREE:-27056756}" "${FAKE_CACHE_MOUNT:-/}";;
esac
"#,
    );
    script_file(
        &bin.join("nproc"),
        "#!/bin/sh\nprintf '%s\\n' \"${FAKE_CPUS:-2}\"\n",
    );
    script_file(
        &bin.join("apt-get"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/apt-get-calls"
if [ -n "${FAKE_APT_FAIL:-}" ]; then printf 'E: Unable to locate package xfce4\n' >&2; exit 100; fi
printf 'NOTE: This is only a simulation!\n'
printf 'Inst libx11-6 [2:1.7.5-1ubuntu0.3] (2:1.7.5-1ubuntu0.4 Ubuntu:22.04/jammy-updates [amd64])\n'
printf 'Inst xfce4 (4.16 Ubuntu:22.04/jammy [all])\n'
printf 'Conf xfce4 (4.16 Ubuntu:22.04/jammy [all])\n'
"#,
    );
    script_file(
        &bin.join("apt-cache"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/apt-cache-calls"
shift 2
for package do
  [ "$package" = "${FAKE_CACHE_MISSING:-}" ] && continue
  printf 'Package: %s\nInstalled-Size: 1000\nSize: 2048000\n\n' "$package"
done
"#,
    );
}

fn ubuntu() -> Distro {
    Distro {
        id: "ubuntu".into(),
        ..Default::default()
    }
}

fn raw(value: Value) -> RawCheck {
    serde_json::from_value(value).unwrap()
}

/// 内存、CPU 正常，两个位置在同一个文件系统
fn sample() -> Value {
    json!({
        "admin": "password",
        "memory": "1977936",
        "available_memory": "1405808",
        "cpus": "2",
        "install": {"available": "27056756", "mount": "/"},
        "cache": {"available": "27056756", "mount": "/"},
        "estimate": {"packages": "147", "records": "147", "installed": "353917", "download": "120559056"}
    })
}

#[test]
fn 同一个文件系统要装好后大小加下载大小再加余量() {
    let check = evaluate(raw(sample())).unwrap();
    assert_eq!(check.admin, AdminAccess::Password);
    assert!(!check.estimated);
    assert_eq!(check.installed_kib, 353917);
    assert_eq!(check.download_bytes, 120559056);
    // 下载按 KiB 向上取整
    let download_kib = 120559056u64.div_ceil(1024);
    assert_eq!(
        check.disks,
        vec![DiskSpace {
            mount: "/".into(),
            available_kib: 27056756,
            need_kib: 353917 + download_kib + INSTALL_MARGIN_KIB,
        }]
    );
    assert!(check.enough_space());
    // 标称 2G 的云主机（实际报 1.9G）、2 核：不提醒
    assert!(!check.low_spec);

    // 剩余正好等于需要时够，少 1 KiB 就不够
    let need = 353917 + download_kib + INSTALL_MARGIN_KIB;
    let mut value = sample();
    value["install"]["available"] = json!(need.to_string());
    value["cache"]["available"] = json!(need.to_string());
    assert!(evaluate(raw(value.clone())).unwrap().enough_space());
    value["install"]["available"] = json!((need - 1).to_string());
    value["cache"]["available"] = json!((need - 1).to_string());
    assert!(!evaluate(raw(value)).unwrap().enough_space());
}

#[test]
fn 不同文件系统各算各的并各自留余量() {
    let mut value = sample();
    value["install"] = json!({"available": "2000000", "mount": "/"});
    value["cache"] = json!({"available": "500000", "mount": "/var"});
    let check = evaluate(raw(value)).unwrap();
    assert_eq!(check.disks.len(), 2);
    assert_eq!(check.disks[0].mount, "/");
    assert_eq!(check.disks[0].need_kib, 353917 + INSTALL_MARGIN_KIB);
    assert!(check.disks[0].enough());
    assert_eq!(check.disks[1].mount, "/var");
    assert_eq!(
        check.disks[1].need_kib,
        120559056u64.div_ceil(1024) + INSTALL_MARGIN_KIB
    );
    // /var 只剩约 488 MiB，不够下载加 1 GiB 余量
    assert!(!check.disks[1].enough());
    assert!(!check.enough_space());
}

#[test]
fn 拿不到估算或查不全时用固定的保守值() {
    for estimate in [
        json!(null),
        // 软件源里查到的记录比要装的包少：少算了会把空间判宽
        json!({"packages": "147", "records": "140", "installed": "300000", "download": "100000000"}),
        json!({"packages": "坏", "records": "1", "installed": "1", "download": "1"}),
    ] {
        let mut value = sample();
        value["estimate"] = estimate;
        let check = evaluate(raw(value)).unwrap();
        assert!(check.estimated);
        assert_eq!(check.installed_kib, FALLBACK_INSTALLED_KIB);
        assert_eq!(check.download_bytes, FALLBACK_DOWNLOAD_KIB * 1024);
        assert_eq!(
            check.disks[0].need_kib,
            FALLBACK_INSTALLED_KIB + FALLBACK_DOWNLOAD_KIB + INSTALL_MARGIN_KIB
        );
    }
    // 固定估计加余量是 2.5 GiB：只剩 2 GiB 的服务器不让装
    let mut value = sample();
    value["estimate"] = json!(null);
    value["install"]["available"] = json!((2 * 1024 * 1024).to_string());
    value["cache"]["available"] = json!((2 * 1024 * 1024).to_string());
    assert!(!evaluate(raw(value)).unwrap().enough_space());
}

#[test]
fn 内存小可用内存少或只有一核时提醒配置低_读不到的不算() {
    for (memory, available, cpus, low) in [
        ("1977936", "1405808", "2", false),
        // 1G 内存
        ("1006632", "800000", "2", true),
        // 1.8 GiB 正好不算低，少一点算
        ("1887436", "1000000", "4", false),
        ("1887435", "1000000", "4", true),
        // 可用内存不到 700 MiB
        ("8000000", "716799", "4", true),
        ("8000000", "716800", "4", false),
        // 只有一核
        ("8000000", "4000000", "1", true),
        // 读不到时为 0，不据此提醒
        ("", "", "", false),
    ] {
        let mut value = sample();
        value["memory"] = json!(memory);
        value["available_memory"] = json!(available);
        value["cpus"] = json!(cpus);
        let check = evaluate(raw(value)).unwrap();
        assert_eq!(
            check.low_spec, low,
            "内存 {memory} 可用 {available} 核 {cpus}"
        );
    }
}

#[test]
fn 磁盘数读不出时报结果不完整() {
    for disk in [
        json!({"available": "", "mount": "/"}),
        json!({"available": "12", "mount": ""}),
    ] {
        let mut value = sample();
        value["cache"] = disk;
        assert_eq!(evaluate(raw(value)).unwrap_err().code, "vnc.invalid_report");
    }
}

#[test]
fn 检查脚本模拟安装累加大小且不碰系统() {
    let (root, program, mut cmd) = local();
    fake_tools(root.path(), &mut cmd);
    let check = install_check(&program, &cmd, &ubuntu(), PackageManager::Apt).unwrap();
    assert!(!check.estimated);
    // 两个包各 1000 KiB、2048000 字节
    assert_eq!(check.installed_kib, 2000);
    assert_eq!(check.download_bytes, 4096000);
    assert_eq!(
        check.disks,
        vec![DiskSpace {
            mount: "/".into(),
            available_kib: 27056756,
            need_kib: 2000 + 4000 + INSTALL_MARGIN_KIB,
        }]
    );
    assert_eq!(check.cpus, 2);
    // 只是模拟：apt-get 只带 -s 跑过一次，要装的包和安装命令里的一致
    let calls = fs::read_to_string(root.path().join("apt-get-calls")).unwrap();
    assert_eq!(calls.lines().count(), 1);
    assert!(calls.starts_with("-s "));
    assert!(calls.contains("--no-install-recommends"));
    assert!(
        calls
            .trim_end()
            .ends_with(&apt_packages(&ubuntu()).join(" "))
    );
    let cache = fs::read_to_string(root.path().join("apt-cache-calls")).unwrap();
    assert_eq!(cache.trim_end(), "show --no-all-versions libx11-6 xfce4");
    // 装进系统的位置和 apt 的下载缓存都看了剩余空间
    let df = fs::read_to_string(root.path().join("df-calls")).unwrap();
    assert_eq!(df, "/usr\n/var/cache/apt/archives\n");
}

#[test]
fn 模拟失败或软件源里查不全时用固定估计_dnf_也是() {
    let (root, program, mut cmd) = local();
    fake_tools(root.path(), &mut cmd);
    cmd.env.push(("FAKE_APT_FAIL".into(), "1".into()));
    let check = install_check(&program, &cmd, &ubuntu(), PackageManager::Apt).unwrap();
    assert!(check.estimated);
    assert!(!root.path().join("apt-cache-calls").exists());

    let (root, program, mut cmd) = local();
    fake_tools(root.path(), &mut cmd);
    cmd.env.push(("FAKE_CACHE_MISSING".into(), "xfce4".into()));
    assert!(
        install_check(&program, &cmd, &ubuntu(), PackageManager::Apt)
            .unwrap()
            .estimated
    );

    let (root, program, mut cmd) = local();
    fake_tools(root.path(), &mut cmd);
    let check = install_check(&program, &cmd, &Distro::default(), PackageManager::Dnf).unwrap();
    assert!(check.estimated);
    assert!(!root.path().join("apt-get-calls").exists());
    let df = fs::read_to_string(root.path().join("df-calls")).unwrap();
    // dnf 的缓存目录还没有时看上一级
    let expected_cache = if Path::new("/var/cache/dnf").exists() {
        "/var/cache/dnf"
    } else {
        "/var/cache"
    };
    assert_eq!(df, format!("/usr\n{expected_cache}\n"));

    assert_eq!(
        install_check(&program, &cmd, &ubuntu(), PackageManager::None)
            .unwrap_err()
            .code,
        "vnc.install_unsupported"
    );
}

#[test]
fn 管理员权限按_root_免密_sudo_管理员组区分且不在组里不试_sudo() {
    for (uid, groups, sudo_exit, expected, tried) in [
        ("0", "root", "1", AdminAccess::Root, false),
        (
            "1000",
            "ubuntu adm sudo",
            "0",
            AdminAccess::NoPassword,
            true,
        ),
        ("1000", "pl sudo", "1", AdminAccess::Password, true),
        ("1000", "ec2-user wheel", "1", AdminAccess::Password, true),
        ("1000", "admin", "0", AdminAccess::NoPassword, true),
        (
            "1000",
            "alice google-sudoers",
            "0",
            AdminAccess::NoPassword,
            true,
        ),
        // 不在管理员组里：不试 sudo（sudoers 里没有的人试一次会被记成安全事件）
        ("1002", "pltest", "0", AdminAccess::None, false),
        ("1002", "sudoers-like nosudo", "0", AdminAccess::None, false),
    ] {
        let (root, program, mut cmd) = local();
        fake_tools(root.path(), &mut cmd);
        cmd.env.push(("FAKE_UID".into(), uid.into()));
        cmd.env.push(("FAKE_GROUPS".into(), groups.into()));
        cmd.env.push(("FAKE_SUDO_EXIT".into(), sudo_exit.into()));
        let check = install_check(&program, &cmd, &ubuntu(), PackageManager::Apt).unwrap();
        assert_eq!(check.admin, expected, "组 {groups}");
        let calls = root.path().join("sudo-calls");
        assert_eq!(calls.exists(), tried, "组 {groups}");
        if tried {
            // 只用 -n：不会停下来等密码
            assert_eq!(fs::read_to_string(calls).unwrap(), "-n true\n");
        }
    }
}

#[test]
fn 检查脚本读内存和核数() {
    let (root, program, mut cmd) = local();
    fake_tools(root.path(), &mut cmd);
    cmd.env.push(("FAKE_CPUS".into(), "1".into()));
    let meminfo = root.path().join("meminfo");
    fs::write(
        &meminfo,
        "MemTotal:        1006632 kB\nMemFree:          110964 kB\nMemAvailable:     405808 kB\n",
    )
    .unwrap();
    let body = install_check::script("apt", "xfce4", "/usr", "/var/cache/apt/archives")
        .unwrap()
        .replace(
            "meminfo=/proc/meminfo",
            &format!(
                "meminfo={}",
                crate::ssh::quote::sh_quote(meminfo.to_str().unwrap()).unwrap()
            ),
        );
    let check = evaluate(raw(desktop::run(&program, &cmd, &body, 30).unwrap())).unwrap();
    assert_eq!(check.memory_kib, 1006632);
    assert_eq!(check.available_memory_kib, 405808);
    assert_eq!(check.cpus, 1);
    assert!(check.low_spec);
}

#[test]
fn 检测报告_root_和正在装软件() {
    let base = json!({"arch":"x86_64","glibc":"2.35","os_release":"","package_manager":"Apt","binary_path":"","binary_version":"","libraries":"","tools":Tools::default(),"kill_user_processes":"","linger":"","sessions":[],"desktops":[]});
    let d = detection::parse_detection(base.clone()).unwrap();
    assert!(!d.root);
    assert!(!d.package_busy);
    let mut value = base;
    value["uid"] = json!("0");
    value["package_busy"] = json!(true);
    let d = detection::parse_detection(value).unwrap();
    assert!(d.root);
    assert!(d.package_busy);

    // 检测脚本：有 apt-get 进程时报正在装软件；unattended-upgrades 常驻后台，不算
    let (root, program, cmd) = local();
    for (comm, busy) in [
        ("unattended-upgr\n", false),
        ("apt-get\n", true),
        ("dpkg\n", true),
    ] {
        let proc = root.path().join(format!("proc-{}", comm.trim()));
        fs::create_dir_all(proc.join("4242")).unwrap();
        fs::write(proc.join("4242/comm"), comm).unwrap();
        let script = detection::script().replace(
            "proc_root=/proc",
            &format!(
                "proc_root={}",
                crate::ssh::quote::sh_quote(proc.to_str().unwrap()).unwrap()
            ),
        );
        let value =
            crate::remote::run::run_script(&program, &cmd, &script, Default::default()).unwrap();
        assert_eq!(value["package_busy"], busy, "{comm}");
        assert!(value["uid"].as_str().is_some_and(|uid| !uid.is_empty()));
    }
}

#[test]
fn 空间大小按_mb_gb_显示() {
    let (_root, program, cmd) = local();
    let value = desktop::run(
        &program,
        &cmd,
        r#"printf '{"a":'; json "$(human_size 1)"; printf ',"b":'; json "$(human_size 512000)"
printf ',"c":'; json "$(human_size 1048576)"; printf ',"d":'; json "$(human_size 27056756)"; printf '}'"#,
        10,
    )
    .unwrap();
    assert_eq!(value["a"], "1 MB");
    assert_eq!(value["b"], "500 MB");
    assert_eq!(value["c"], "1.0 GB");
    assert_eq!(value["d"], "25.8 GB");
}

#[test]
fn 上传程序前也看家目录空间且不留下暂存目录() {
    let (root, program, mut cmd) = local();
    fake_tools(root.path(), &mut cmd);
    cmd.env.push(("FAKE_CACHE_FREE".into(), "1000".into()));
    let gzip = root.path().join("Xvnc.gz");
    fs::write(&gzip, b"x").unwrap();
    let e = acquire::upload_package(
        &program,
        &cmd,
        &gzip,
        &"0".repeat(64),
        &|_| panic!("空间不够时不该开始上传"),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(e.code, "vnc.no_space");
    assert_eq!(e.params["available"], "1 MB");
    assert_eq!(e.params["need"], "564 MB");
    let binroot = root.path().join(".local/share/pierlink/vnc");
    assert_eq!(fs::read_dir(&binroot).unwrap().count(), 0);
}

#[test]
fn 新建的桌面会话让_xfce_面板直接用默认布局() {
    let script = desktop::create_script(&CreateDesktop {
        desktop_id: "xfce".into(),
        width: 1280,
        height: 800,
        xvnc: XvncBinary {
            path: "/usr/bin/Xtigervnc".into(),
            version: (1, 12, 0),
            flavor: Flavor::TigerVnc,
        },
    })
    .unwrap();
    assert!(script.contains("XFCE_PANEL_MIGRATE_DEFAULT=1"));
}
