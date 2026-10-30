use super::*;
use crate::{
    process::SshProgram,
    ssh::{args::SshCommand, quote::sh_quote},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Mutex, atomic::AtomicBool},
};

pub(super) fn local() -> (tempfile::TempDir, SshProgram, SshCommand) {
    let root = crate::test_support::tempdir();
    let cmd = SshCommand {
        args: vec![],
        env: vec![
            ("HOME".into(), root.path().to_str().unwrap().into()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ],
        builtin: None,
        askpass: None,
        config_text: None,
    };
    (
        root,
        SshProgram {
            path: "/bin/sh".into(),
        },
        cmd,
    )
}
fn binary() -> XvncBinary {
    XvncBinary {
        path: "/usr/bin/Xtigervnc".into(),
        version: (1, 12, 0),
        flavor: Flavor::TigerVnc,
    }
}
fn request() -> CreateDesktop {
    CreateDesktop {
        desktop_id: "test".into(),
        width: 800,
        height: 600,
        xvnc: binary(),
    }
}
fn state(home: &Path) -> Value {
    let dir = home.join(".local/state/pierlink/vnc");
    json!({"display":90,"socket":dir.join("90.sock"),"xauth":dir.join("90.xauth"),"log":dir.join("90.log"),"xvnc_pid":99999999,"pgid":99999998,"starttime":"123","boot_id":"旧启动","desktop_id":"test","desktop_name":"测试桌面","created_at":1000,"geometry":"800x600"})
}
fn save(home: &Path, value: &Value) {
    let dir = home.join(".local/state/pierlink/vnc");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("90.json"), serde_json::to_vec(value).unwrap()).unwrap();
}
pub(super) fn script_file(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn detection() -> VncDetection {
    VncDetection {
        arch: "x86_64".into(),
        glibc: Some("2.35".into()),
        distro: Distro::default(),
        package_manager: PackageManager::None,
        tigervnc: None,
        sessions: vec![],
        desktops: vec![],
        light_desktop_available: false,
        pierlink_desktops: vec![],
        tools: Tools::default(),
        xkb_namespace: None,
        logind: None,
        official_pkg: OfficialPkgCheck {
            ok: true,
            missing: vec![],
        },
    }
}

#[test]
fn 状态解析核对启动标识进程名及进程组() {
    let root = crate::test_support::tempdir();
    let value = state(root.path());
    let mut snapshot = desktop::Snapshot {
        file: 90,
        content: value.to_string(),
        boot_id: "旧启动".into(),
        identity: "123 99999998".into(),
        comm: "Xtigervnc".into(),
    };
    let s = snapshot.state().unwrap();
    assert!(!snapshot.stale(&s));
    assert_eq!(s.desktop(false).geometry, "800x600");
    for (boot, ident, comm) in [
        ("新启动", "123 99999998", "Xtigervnc"),
        ("旧启动", "124 99999998", "Xtigervnc"),
        ("旧启动", "123 99999997", "Xtigervnc"),
        ("旧启动", "123 99999998", "sleep"),
        ("旧启动", "", "Xvnc"),
    ] {
        snapshot.boot_id = boot.into();
        snapshot.identity = ident.into();
        snapshot.comm = comm.into();
        assert!(snapshot.stale(&s));
    }
    snapshot.file = 91;
    assert_eq!(snapshot.state().unwrap_err().code, "vnc.stale");
    snapshot.content = "{坏数据".into();
    assert!(snapshot.state().is_err());
}

#[test]
fn 检测保留非空过期状态并查找自部署程序() {
    let (root, program, cmd) = local();
    let saved = state(root.path());
    save(root.path(), &saved);
    let bin = root.path().join(".local/share/pierlink/vnc/1.16.2");
    fs::create_dir_all(&bin).unwrap();
    script_file(&bin.join("Xvnc"), "#!/bin/sh\nprintf 'TigerVNC 1.16.2'\n");
    let script = detection::script()
        .replace(
            "for proc in \"$proc_root\"/[0-9]*; do",
            "for proc in /不存在的测试进程; do",
        )
        .replace("$(command -v Xtigervnc)", "")
        .replace("$(command -v Xvnc)", "");
    let value =
        crate::remote::run::run_script(&program, &cmd, &script, Default::default()).unwrap();
    let result = detection::parse_detection(value).unwrap();
    assert_eq!(result.pierlink_desktops.len(), 1);
    assert!(result.pierlink_desktops[0].stale);
    assert_eq!(result.pierlink_desktops[0].desktop_name, "测试桌面");
    assert_eq!(
        result.tigervnc.unwrap().path,
        bin.join("Xvnc").to_str().unwrap()
    );
    assert!(result.sessions.is_empty());
}

#[test]
fn 桌面参数拒绝路径与注入并夹取尺寸() {
    let mut r = request();
    r.width = 1;
    r.height = u16::MAX;
    let script = desktop::create_script(&r).unwrap();
    assert!(script.contains("geometry='640x1600'"));
    r.width = u16::MAX;
    r.height = 0;
    assert!(
        desktop::create_script(&r)
            .unwrap()
            .contains("geometry='2560x480'")
    );
    for id in ["", "../x", "a/b", "$(id)", "x\ny", ".."] {
        r.desktop_id = id.into();
        assert_eq!(
            desktop::create_script(&r).unwrap_err().code,
            "vnc.desktop_missing"
        );
    }
    let mut b = binary();
    b.flavor = Flavor::TurboVnc;
    assert_eq!(
        desktop::check_binary(&b).unwrap_err().code,
        "vnc.not_tigervnc"
    );
    b.flavor = Flavor::TigerVnc;
    b.version = (1, 8, 9);
    assert_eq!(
        desktop::check_binary(&b).unwrap_err().code,
        "vnc.xvnc_too_old"
    );
}

#[test]
fn 创建时重新读取桌面条目并检查隐藏和程序存在() {
    let (root, program, mut cmd) = local();
    let x = root.path().join("xsessions");
    fs::create_dir(&x).unwrap();
    cmd.env.push((
        "PIERLINK_VNC_XSESSIONS_DIR".into(),
        x.to_str().unwrap().into(),
    ));
    for contents in [
        "",
        "[Desktop Entry]\nName=测试\nExec=/不存在",
        "[Desktop Entry]\nName=测试\nExec=/bin/true\nHidden=true",
    ] {
        fs::write(x.join("test.desktop"), contents).unwrap();
        let script = desktop::create_script(&request()).unwrap();
        assert_eq!(
            desktop::run(&program, &cmd, &script, 10).unwrap_err().code,
            "vnc.desktop_missing"
        );
    }
    let body = format!(
        "desktop_id=test\nxsessions={}\n{}\nprintf '{{\"exec\":'; json \"$desktop_exec\"; printf '}}'",
        sh_quote(x.to_str().unwrap()).unwrap(),
        include_str!("desktop_entry.sh")
    );
    fs::write(
        x.join("test.desktop"),
        "[Desktop Entry]\nName=含'空格\nExec=/bin/sh -c \"sleep 600\"\n",
    )
    .unwrap();
    assert_eq!(
        desktop::run(&program, &cmd, &body, 10).unwrap()["exec"],
        " '/bin/sh' '-c' 'sleep 600'"
    );
}

#[test]
fn 状态目录不存在为空且过期清理不结束任何进程() {
    let (root, program, cmd) = local();
    assert_eq!(cleanup_stale(&program, &cmd).unwrap(), 0);
    assert_eq!(
        stop_desktop(&program, &cmd, 90).unwrap_err().code,
        "vnc.stale"
    );
    let mut child =
        crate::process::spawn_tree(Path::new("/bin/sleep"), &["30".into()], None, &[]).unwrap();
    let mut s = state(root.path());
    s["xvnc_pid"] = child.child.id().into();
    s["pgid"] = child.child.id().into();
    save(root.path(), &s);
    stop_desktop(&program, &cmd, 90).unwrap();
    assert!(child.try_wait().unwrap().is_none());
    save(root.path(), &s);
    let socket = root.path().join(".local/state/pierlink/vnc/90.sock");
    fs::write(&socket, []).unwrap();
    let namespace = socket.with_extension("xkb");
    fs::create_dir_all(namespace.join("work/work")).unwrap();
    fs::set_permissions(
        namespace.join("work/work"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    assert_eq!(cleanup_stale(&program, &cmd).unwrap(), 1);
    assert!(!socket.exists());
    assert!(!namespace.exists());
    assert!(child.try_wait().unwrap().is_none());
    child.terminate().unwrap();
}

#[test]
fn 残留清理拒绝越界路径且保留仍被监听的套接字() {
    let (root, program, cmd) = local();
    let mut s = state(root.path());
    s["socket"] = "/不属于应用的路径".into();
    save(root.path(), &s);
    assert_eq!(cleanup_stale(&program, &cmd).unwrap_err().code, "vnc.stale");
    let socket = root.path().join(".local/state/pierlink/vnc/90.sock");
    fs::write(&socket, []).unwrap();
    let net = root.path().join("unix");
    fs::write(
        &net,
        format!("0: 2 0 10000 1 01 123 {}\n", socket.display()),
    )
    .unwrap();
    let runner = root.path().join("runner");
    script_file(
        &runner,
        &format!(
            "#!/usr/bin/python3\nimport subprocess,sys\ns=sys.stdin.read().replace('/proc/net/unix',{})\nsys.exit(subprocess.run(['/bin/sh'],input=s.encode()).returncode)\n",
            serde_json::to_string(net.to_str().unwrap()).unwrap()
        ),
    );
    let program = SshProgram { path: runner };
    save(root.path(), &state(root.path()));
    assert_eq!(cleanup_stale(&program, &cmd).unwrap(), 1);
    assert!(socket.exists());
}

#[test]
fn 重启后短套接字目录已消失仍能清理旧状态() {
    let (root, _program, cmd) = local();
    let runner = root.path().join("runner");
    script_file(
        &runner,
        "#!/usr/bin/python3\nimport os,subprocess,sys\ns=sys.stdin.read().replace('/tmp/pierlink-vnc-',os.environ['HOME']+'/fallback-')\nsys.exit(subprocess.run(['/bin/sh'],input=s.encode()).returncode)\n",
    );
    let program = SshProgram { path: runner };
    let mut s = state(root.path());
    let socket = root
        .path()
        .join(format!("fallback-{}/90.sock", unsafe { libc::getuid() }));
    assert!(!socket.parent().unwrap().exists());
    s["socket"] = socket.to_str().unwrap().into();
    save(root.path(), &s);
    stop_desktop(&program, &cmd, 90).unwrap();
    save(root.path(), &s);
    assert_eq!(cleanup_stale(&program, &cmd).unwrap(), 1);
}

#[test]
fn 已结束日志只保留最近五个并保留运行中日志() {
    let (root, program, cmd) = local();
    let dir = root.path().join(".local/state/pierlink/vnc");
    fs::create_dir_all(&dir).unwrap();
    for n in 90..100 {
        fs::write(dir.join(format!("{n}.log")), "日志").unwrap();
    }
    fs::write(dir.join("90.json"), "{}").unwrap();
    desktop::run(&program, &cmd, "prune_logs\nprintf '{}'", 10).unwrap();
    let logs = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|s| s == "log"))
        .count();
    assert_eq!(logs, 6);
    assert!(dir.join("90.log").exists());
}

#[test]
fn 启动日志尾部脱敏不泄漏凭据和_cookie() {
    let err=desktop::decode_error(json!({"error":"vnc.start_timeout","log":"password=绝密\n00112233445566778899aabbccddeeff\n普通错误"})).unwrap_err();
    assert!(!err.params["log"].contains("绝密"));
    assert!(!err.params["log"].contains("00112233"));
    assert!(err.params["log"].contains("普通错误"));
}

#[test]
fn 持续日志限制大小并屏蔽敏感字段和私钥块() {
    let (root, program, cmd) = local();
    let input = root.path().join("input");
    let log = root.path().join("log");
    let mut contents = "普通日志\n".repeat(120_000);
    contents.push_str("password=绝密\n-----BEGIN PRIVATE KEY-----\n私有内容\n-----END PRIVATE KEY-----\n00112233445566778899aabbccddeeff\n正常结尾\n");
    fs::write(&input, contents).unwrap();
    let body = format!(
        "write_log {} < {}\nprintf '{{}}'",
        sh_quote(log.to_str().unwrap()).unwrap(),
        sh_quote(input.to_str().unwrap()).unwrap()
    );
    desktop::run(&program, &cmd, &body, 10).unwrap();
    let contents = fs::read_to_string(&log).unwrap();
    assert!(contents.len() <= 1024 * 1024);
    assert!(!contents.contains("绝密"));
    assert!(!contents.contains("私有内容"));
    assert!(!contents.contains("0011223344556677"));
    assert!(contents.ends_with("正常结尾\n"));
}

fn package(path: &Path, name: &str, kind: tar::EntryType, data: &[u8]) -> String {
    let file = fs::File::create(path).unwrap();
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut archive = tar::Builder::new(gz);
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o755);
    header.set_entry_type(kind);
    header.set_cksum();
    archive.append_data(&mut header, name, data).unwrap();
    archive.into_inner().unwrap().finish().unwrap();
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

#[test]
fn 官方包先核对摘要且仅解出固定普通文件() {
    let root = crate::test_support::tempdir();
    let p = root.path().join("p.tar.gz");
    let out = root.path().join("x.gz");
    let cancel = AtomicBool::new(false);
    let hash = package(
        &p,
        "tigervnc-1.16.2.x86_64/usr/bin/Xvnc",
        tar::EntryType::Regular,
        "测试程序".as_bytes(),
    );
    assert_eq!(
        acquire::unpack(&p, &out, &hash, &cancel).unwrap(),
        format!("{:x}", Sha256::digest("测试程序".as_bytes()))
    );
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(fs::File::open(&out).unwrap())
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, "测试程序".as_bytes());
    assert_eq!(
        acquire::unpack(&p, &out, "错误摘要", &cancel)
            .unwrap_err()
            .code,
        "vnc.sha_mismatch"
    );
    for (name, kind) in [
        ("usr/bin/other", tar::EntryType::Regular),
        ("usr/bin/Xvnc", tar::EntryType::Symlink),
    ] {
        let hash = package(&p, name, kind, b"");
        assert_eq!(
            acquire::unpack(&p, &out, &hash, &cancel).unwrap_err().code,
            "vnc.invalid_package"
        );
    }
    let cancel = AtomicBool::new(true);
    assert_eq!(
        acquire::unpack(&p, &out, &hash, &cancel).unwrap_err().code,
        "vnc.cancelled"
    );
}

#[test]
fn 获取按顺序汇总失败且取消不继续后续步骤() {
    let (_root, program, cmd) = local();
    let mut d = detection();
    d.arch = "aarch64".into();
    let events = Mutex::new(Vec::new());
    let e = acquire_xvnc(
        &program,
        &cmd,
        &d,
        AcquireOptions::default(),
        &|s| events.lock().unwrap().push(s),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(e.code, "vnc.acquire_failed");
    let steps: Vec<Value> = serde_json::from_str(&e.params["steps"]).unwrap();
    assert_eq!(steps.len(), 5);
    assert_eq!(steps[0]["Failed"]["source"], "Installed");
    assert_eq!(steps[1]["Failed"]["source"], "Apt");
    assert_eq!(steps[4]["Failed"]["error"]["code"], "vnc.unsupported_arch");
    assert_eq!(
        acquire_xvnc(
            &program,
            &cmd,
            &d,
            AcquireOptions::default(),
            &|_| panic!("取消后不报告新步骤"),
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .code,
        "vnc.cancelled"
    );
}

#[test]
fn 获取复用已装版本并拒绝伪装和旧版() {
    let (root, program, cmd) = local();
    let path = root.path().join("Xvnc");
    let mut d = detection();
    let mut b = binary();
    b.path = path.to_str().unwrap().into();
    d.tigervnc = Some(b.clone());
    for (output, expected) in [
        ("TurboVNC 3.0", "vnc.not_tigervnc"),
        ("TigerVNC 1.8.0", "vnc.xvnc_too_old"),
    ] {
        script_file(&path, &format!("#!/bin/sh\nprintf '%s' '{output}'\n"));
        assert_eq!(
            desktop::verify_binary(&program, &cmd, &b.path)
                .unwrap_err()
                .code,
            expected
        );
    }
    script_file(&path, "#!/bin/sh\nprintf 'TigerVNC 1.12.0'\n");
    let got = acquire_xvnc(
        &program,
        &cmd,
        &d,
        AcquireOptions::default(),
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(got, b);
}

#[test]
fn 发行版与服务器下载失败回收临时目录并汇总原因() {
    let (root, program, mut cmd) = local();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for name in ["apt-get", "curl", "wget"] {
        script_file(&bin.join(name), "#!/bin/sh\nexit 1\n");
    }
    cmd.env
        .push(("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())));
    let mut d = detection();
    d.package_manager = PackageManager::Apt;
    let e = acquire_xvnc(
        &program,
        &cmd,
        &d,
        AcquireOptions::default(),
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap_err();
    let steps: Vec<Value> = serde_json::from_str(&e.params["steps"]).unwrap();
    assert_eq!(steps[1]["Failed"]["error"]["code"], "vnc.download_failed");
    assert_eq!(steps[2]["Failed"]["error"]["code"], "vnc.download_failed");
    assert_eq!(
        fs::read_dir(root.path().join(".local/share/pierlink/vnc"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn 官方包前提失败不会执行下载且保留缺失原因() {
    let (_root, program, cmd) = local();
    let mut d = detection();
    d.official_pkg = OfficialPkgCheck {
        ok: false,
        missing: vec!["libxcvt.so.0".into()],
    };
    let e = acquire_xvnc(
        &program,
        &cmd,
        &d,
        AcquireOptions::default(),
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap_err();
    let steps: Vec<Value> = serde_json::from_str(&e.params["steps"]).unwrap();
    for step in &steps[2..] {
        assert_eq!(step["Failed"]["error"]["code"], "vnc.official_prereq");
        assert_eq!(step["Failed"]["error"]["params"]["missing"], "libxcvt.so.0");
    }
}

#[test]
fn 本地包上传校验后替换旧版并清理取消片段() {
    let (root, program, cmd) = local();
    let binroot = root.path().join(".local/share/pierlink/vnc");
    let old = binroot.join("old");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join(".pierlink-created"), "true").unwrap();
    let p = root.path().join("包.tar.gz");
    let gzip = root.path().join("Xvnc.gz");
    let hash = package(
        &p,
        "usr/bin/Xvnc",
        tar::EntryType::Regular,
        b"#!/bin/sh\nprintf 'TigerVNC 1.16.2'\n",
    );
    let cancel = AtomicBool::new(false);
    let hash = acquire::unpack(&p, &gzip, &hash, &cancel).unwrap();
    let events = Mutex::new(Vec::new());
    let binary = acquire::upload_package(
        &program,
        &cmd,
        &gzip,
        &hash,
        &|event| events.lock().unwrap().push(event),
        &cancel,
    )
    .unwrap();
    assert_eq!(binary.path, binroot.join("1.16.2/Xvnc").to_str().unwrap());
    assert!(!old.exists());
    assert!(events.lock().unwrap().iter().any(
        |e| matches!(e, AcquireStep::Upload { bytes, total } if bytes == total && *bytes > 0)
    ));
    assert_eq!(fs::read_dir(&binroot).unwrap().count(), 1);
    let e = acquire::upload_package(
        &program,
        &cmd,
        &gzip,
        &hash,
        &|_| {
            cancel.store(true, std::sync::atomic::Ordering::Release);
        },
        &cancel,
    )
    .unwrap_err();
    assert!(e.code.ends_with(".cancelled"));
    assert_eq!(fs::read_dir(&binroot).unwrap().count(), 1);
    assert!(Path::new(&binary.path).exists());
    let e = acquire::upload_package(
        &program,
        &cmd,
        &gzip,
        &"0".repeat(64),
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(e.code, "vnc.checksum_mismatch");
    assert_eq!(fs::read_dir(&binroot).unwrap().count(), 1);
}

#[test]
fn 安装缺库或版本命令失败时保留已装程序() {
    let (root, program, mut cmd) = local();
    let tools = root.path().join("tools");
    fs::create_dir(&tools).unwrap();
    cmd.env
        .push(("PATH".into(), format!("{}:/usr/bin:/bin", tools.display())));
    let work = root.path().join("work");
    fs::create_dir_all(work.join("unpack")).unwrap();
    let binary = work.join("unpack/Xvnc");
    script_file(&binary, "#!/bin/sh\nprintf 'TigerVNC 1.16.2'\nexit 1\n");
    let body = format!(
        "work={}\nbinary={}\nversion=1.16.2\nrelative=Xvnc\n{}",
        sh_quote(work.to_str().unwrap()).unwrap(),
        sh_quote(binary.to_str().unwrap()).unwrap(),
        include_str!("install.sh")
    );
    script_file(
        &tools.join("ldd"),
        "#!/bin/sh\nprintf 'libxcvt.so.0 => not found\\n'\n",
    );
    let e = desktop::run(&program, &cmd, &body, 10).unwrap_err();
    assert_eq!(e.code, "vnc.missing_libs");
    assert_eq!(e.params["libs"], "libxcvt.so.0");
    script_file(&tools.join("ldd"), "#!/bin/sh\nexit 0\n");
    assert_eq!(
        desktop::run(&program, &cmd, &body, 10).unwrap_err().code,
        "vnc.not_tigervnc"
    );
    assert!(binary.exists());
}

#[test]
fn 所有远程脚本符合_posix_shell_语法() {
    let (_root, program, mut cmd) = local();
    cmd.args = vec!["-n".into()];
    for script in [
        include_str!("common.sh"),
        include_str!("apt.sh"),
        include_str!("environment.sh"),
        include_str!("xkb.sh"),
        include_str!("light.sh"),
        include_str!("light_entry.sh"),
        include_str!("light_session.sh"),
        include_str!("light_dbus.sh"),
        include_str!("states.sh"),
        include_str!("launch.sh"),
        include_str!("supervise.sh"),
        include_str!("stop.sh"),
        include_str!("acquire.sh"),
        include_str!("install.sh"),
    ] {
        let mut child = program.spawn(&cmd).unwrap();
        child
            .child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        assert!(child.child.wait().unwrap().success());
    }
}

static FAKE_DESKTOP_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn 命名空间启动失败返回安装指引且成功后仍可停止() {
    for (failure, reason) in [
        ("namespace", Some("AppArmor")),
        ("mount", Some("overlay")),
        ("", None),
    ] {
        let mut f = FakeDesktop::new();
        let bin = f.root.path().join("bin");
        super::xkb_tests::mock_tools(&bin);
        fs::write(bin.join(".pierlink-created"), "true").unwrap();
        fs::create_dir_all(bin.join("usr/bin")).unwrap();
        script_file(&bin.join("usr/bin/xkbcomp"), "#!/bin/sh\nexit 0\n");
        f.cmd.env.extend([
            ("PIERLINK_TEST_XKB_NAMESPACE".into(), "1".into()),
            ("MOCK_FAILURE".into(), failure.into()),
        ]);
        let result = create_desktop(&f.program, &f.cmd, f.req());
        if let Some(reason) = reason {
            let error = result.unwrap_err();
            assert_eq!(error.code, "vnc.xkb_unavailable");
            assert!(error.params["reason"].contains(reason));
            assert_eq!(
                error.params["install_command"],
                "sudo apt-get install -y x11-xkb-utils"
            );
        } else {
            let created = result.unwrap();
            let dir = f.root.path().join(".local/state/pierlink/vnc");
            assert!(
                dir.join(format!("{}.xkb/upper/xkbcomp", created.display))
                    .exists()
            );
            assert_eq!(cleanup_stale(&f.program, &f.cmd).unwrap(), 0);
            stop_desktop(&f.program, &f.cmd, created.display).unwrap();
        }
        let dir = f.root.path().join(".local/state/pierlink/vnc");
        for entry in fs::read_dir(dir).unwrap().flatten() {
            assert_eq!(entry.path().extension().unwrap(), "log");
        }
    }
}

struct FakeDesktop {
    _guard: std::sync::MutexGuard<'static, ()>,
    fallback_existed: bool,
    root: tempfile::TempDir,
    program: SshProgram,
    cmd: SshCommand,
}
impl FakeDesktop {
    fn new() -> Self {
        let guard = FAKE_DESKTOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let fallback_existed =
            Path::new(&format!("/tmp/pierlink-vnc-{}", unsafe { libc::getuid() })).exists();
        let (root, _program, mut cmd) = local();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        script_file(
            &bin.join("xauth"),
            "#!/bin/sh\nprintf '测试认证文件' > \"$2\"\n",
        );
        script_file(&bin.join("loginctl"), "#!/bin/sh\nexit 1\n");
        // 单测只验证会话生命周期，私有总线留给真实桌面夹具。
        script_file(
            &bin.join("dbus-run-session"),
            "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n",
        );
        script_file(
            &bin.join("Xvnc"),
            r#"#!/usr/bin/python3
import ctypes,os,signal,socket,sys,time
if '-version' in sys.argv:
    print('TigerVNC 1.16.2');sys.exit(0)
ctypes.CDLL(None).prctl(15,b'Xvnc',0,0,0)
path=sys.argv[sys.argv.index('-rfbunixpath')+1]
mode=os.environ.get('FAKE_MODE','')
if mode=='failed':
    print('password=绝密',flush=True);sys.exit(1)
if mode=='active':
    print('already active for display',flush=True);sys.exit(1)
if mode=='fallback' and not path.startswith('/tmp/pierlink-vnc-'):
    print('cannot bind unix socket',flush=True);sys.exit(1)
if mode!='timeout': open(path,'w').close()
signal.signal(signal.SIGTERM,signal.SIG_IGN if mode=='stubborn' else lambda *_:sys.exit(0))
while True: time.sleep(1)
"#,
        );
        let xsessions = root.path().join("xsessions");
        fs::create_dir(&xsessions).unwrap();
        fs::write(
            xsessions.join("test.desktop"),
            "[Desktop Entry]\nName=桌面含'引号\nExec=/bin/sh -c \"sleep 600\"\n",
        )
        .unwrap();
        cmd.env.extend([
            (
                "PIERLINK_VNC_XSESSIONS_DIR".into(),
                xsessions.to_str().unwrap().into(),
            ),
            ("SHELL".into(), "/bin/sh".into()),
            ("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())),
        ]);
        let tcp = root.path().join("tcp");
        fs::write(&tcp, "0: 0100007F:1766 0:0 0A 0:0 0:0 0 1000 0 123\n").unwrap();
        let empty = root.path().join("empty");
        fs::write(&empty, "").unwrap();
        let supervisor = include_str!("supervise.sh");
        let begin = supervisor.find("inodes=$(\n").unwrap();
        let end = begin + supervisor[begin..].find("\n)").unwrap() + 2;
        let block = &supervisor[begin..end];
        let quoted = sh_quote(block).unwrap();
        let runner = root.path().join("runner");
        script_file(
            &runner,
            &format!(
                r#"#!/usr/bin/python3
import os,subprocess,sys
s=sys.stdin.read()
s=s.replace({block},'inodes=123').replace({quoted},'inodes=123')
s=s.replace('-S "$socket"','-e "$socket"')
if os.environ.get('PIERLINK_TEST_XKB_NAMESPACE')!='1':
    s=s.replace('\nkeyboard_environment\n','\nxkb_args=\n')
s=s.replace('/proc/net/tcp6',{empty}).replace('/proc/net/tcp', {tcp} if os.environ.get('FAKE_MODE')=='tcp' else {empty})
sys.exit(subprocess.run(['/bin/sh'],input=s.encode()).returncode)
"#,
                block = serde_json::to_string(block).unwrap(),
                quoted = serde_json::to_string(&quoted[1..quoted.len() - 1]).unwrap(),
                empty = serde_json::to_string(empty.to_str().unwrap()).unwrap(),
                tcp = serde_json::to_string(tcp.to_str().unwrap()).unwrap()
            ),
        );
        let program = SshProgram { path: runner };
        Self {
            _guard: guard,
            fallback_existed,
            root,
            program,
            cmd,
        }
    }
    fn req(&self) -> CreateDesktop {
        let mut req = request();
        req.xvnc.path = self.root.path().join("bin/Xvnc").to_str().unwrap().into();
        req
    }
}
impl Drop for FakeDesktop {
    fn drop(&mut self) {
        let home = self
            .cmd
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == "HOME")
            .map(|(_, v)| Path::new(v))
            .unwrap();
        let dir = home.join(".local/state/pierlink/vnc");
        for display in 90..=99 {
            if dir.join(format!("{display}.json")).exists() {
                let _ = stop_desktop(&self.program, &self.cmd, display);
            }
        }
        let fallback = format!("/tmp/pierlink-vnc-{}", unsafe { libc::getuid() });
        if !self.fallback_existed {
            let _ = fs::remove_dir(fallback);
        }
    }
}

#[test]
fn 显示选择跳过占用且十次耗尽后返回错误() {
    let f = FakeDesktop::new();
    let dir = f.root.path().join(".local/state/pierlink/vnc");
    fs::create_dir_all(&dir).unwrap();
    for n in 90..100 {
        fs::create_dir(dir.join(format!(".{n}.launch"))).unwrap();
    }
    assert_eq!(
        create_desktop(&f.program, &f.cmd, f.req())
            .unwrap_err()
            .code,
        "vnc.display_exhausted"
    );
    fs::remove_dir(dir.join(".99.launch")).unwrap();
    let desktop = create_desktop(&f.program, &f.cmd, f.req()).unwrap();
    assert_eq!(desktop.display, 99);
    assert!(desktop.linger_failed);
    assert_eq!(cleanup_stale(&f.program, &f.cmd).unwrap(), 0);
    let state: Value = serde_json::from_slice(&fs::read(dir.join("99.json")).unwrap()).unwrap();
    let pid = state["xvnc_pid"].as_u64().unwrap();
    let script = detection::script().replace(
        "for proc in \"$proc_root\"/[0-9]*; do",
        &format!("for proc in /proc/{pid}; do"),
    );
    let value =
        crate::remote::run::run_script(&f.program, &f.cmd, &script, Default::default()).unwrap();
    let detected = detection::parse_detection(value).unwrap();
    assert_eq!(detected.pierlink_desktops, vec![desktop]);
    assert_eq!(detected.sessions.len(), 1);
    assert!(detected.sessions[0].pierlink);
    stop_desktop(&f.program, &f.cmd, 99).unwrap();
}

#[test]
fn 启动失败与_tcp_监听均回收自己的进程和状态() {
    for (mode, code) in [
        ("failed", "vnc.start_timeout"),
        ("tcp", "vnc.tcp_listen"),
        ("active", "vnc.display_exhausted"),
        ("timeout", "vnc.start_timeout"),
    ] {
        let mut f = FakeDesktop::new();
        f.cmd.env.push(("FAKE_MODE".into(), mode.into()));
        let e = create_desktop(&f.program, &f.cmd, f.req()).unwrap_err();
        assert_eq!(e.code, code);
        assert!(!format!("{e:?}").contains("绝密"));
        let dir = f.root.path().join(".local/state/pierlink/vnc");
        for entry in fs::read_dir(dir).unwrap().flatten() {
            assert_eq!(entry.path().extension().unwrap(), "log");
        }
    }
}

#[test]
fn 套接字路径已存在时拒绝覆盖并回收启动锁() {
    let f = FakeDesktop::new();
    let dir = f.root.path().join(".local/state/pierlink/vnc");
    fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("90.sock");
    assert!(socket.as_os_str().len() < 100);
    fs::write(&socket, "保留").unwrap();
    let e = create_desktop(&f.program, &f.cmd, f.req()).unwrap_err();
    assert_eq!(e.code, "vnc.socket_path");
    assert_eq!(fs::read_to_string(&socket).unwrap(), "保留");
    assert!(!dir.join(".90.launch").exists());
    assert!(!dir.join("90.xauth").exists());
}

#[test]
fn 强制关闭忽略终止信号的桌面仍回收认证文件与套接字() {
    let mut f = FakeDesktop::new();
    f.cmd.env.push(("FAKE_MODE".into(), "stubborn".into()));
    let d = create_desktop(&f.program, &f.cmd, f.req()).unwrap();
    let dir = f.root.path().join(".local/state/pierlink/vnc");
    let file = dir.join(format!("{}.json", d.display));
    let s: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    stop_desktop(&f.program, &f.cmd, d.display).unwrap();
    assert!(!file.exists());
    assert!(!Path::new(s["xauth"].as_str().unwrap()).exists());
    assert!(!Path::new(&d.socket).exists());
    let pid = s["xvnc_pid"].as_u64().unwrap();
    // 未由系统回收的僵尸同样已不能执行代码；只检查本夹具自己的 PID。
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        assert!(stat.rsplit_once(") ").unwrap().1.starts_with('Z'));
    }
}

#[test]
fn 套接字长路径与绑定失败都退到私有短目录() {
    // 两种情况串行运行，测试只清理自身状态所记的套接字。
    for long in [false, true] {
        let mut f = FakeDesktop::new();
        if long {
            let home = f.root.path().join("家目录很长".repeat(8));
            fs::create_dir(&home).unwrap();
            f.cmd
                .env
                .push(("HOME".into(), home.to_str().unwrap().into()));
        } else {
            f.cmd.env.push(("FAKE_MODE".into(), "fallback".into()));
        }
        let d = create_desktop(&f.program, &f.cmd, f.req()).unwrap();
        assert!(d.socket.starts_with("/tmp/pierlink-vnc-"));
        assert!(d.socket.len() < 100);
        stop_desktop(&f.program, &f.cmd, d.display).unwrap();
        assert!(!Path::new(&d.socket).exists());
    }
}

#[test]
fn 轻量桌面使用专用会话并恢复环境关闭时删除缓存() {
    let f = FakeDesktop::new();
    let prefix = f
        .root
        .path()
        .join(".local/share/pierlink/vnc-light/apt-test");
    fs::create_dir_all(prefix.join("etc/fonts")).unwrap();
    fs::write(prefix.join(".pierlink-created"), "true").unwrap();
    fs::write(prefix.join("etc/fonts/pierlink.conf"), "").unwrap();
    script_file(
        &prefix.join("session"),
        r#"#!/bin/sh
printf '%s\n' "$DESKTOP_SESSION" "$XDG_CURRENT_DESKTOP" "$PATH" "$FONTCONFIG_FILE" "$XDG_CACHE_HOME" > "$HOME/session-environment"
sleep 600
"#,
    );
    let mut req = f.req();
    req.desktop_id = LIGHT_DESKTOP_ID.into();
    let created = create_desktop(&f.program, &f.cmd, req).unwrap();
    assert_eq!(created.desktop_id, LIGHT_DESKTOP_ID);
    let env_file = f.root.path().join("session-environment");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !env_file.exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let contents = fs::read_to_string(&env_file).unwrap();
    assert!(contents.starts_with("pierlink-light\nOPENBOX\n"));
    assert!(contents.contains(prefix.join("usr/bin").to_str().unwrap()));
    assert!(contents.contains(prefix.join("etc/fonts/pierlink.conf").to_str().unwrap()));
    let cache = f.root.path().join(format!(
        ".local/state/pierlink/vnc/{}.cache",
        created.display
    ));
    assert!(cache.join("session.conf").exists());
    stop_desktop(&f.program, &f.cmd, created.display).unwrap();
    assert!(!cache.exists());
}

#[test]
fn 脚本默认原因及变化参数保留中文并传递翻译键() {
    let (_root, program, cmd) = local();
    let error = desktop::run(&program, &cmd, "fail download_failed", 5).unwrap_err();
    assert_eq!(error.params["reason"], "操作无法完成");
    assert_eq!(error.params["reason_key"], "vnc.operation_failed");
    assert_eq!(error.params["source"], "未知");
    assert_eq!(error.params["source_key"], "vnc.unknown_source");
    let error = desktop::run(&program, &cmd, "fail start_timeout", 5).unwrap_err();
    assert_eq!(error.params["log"], "操作无法完成");
    assert_eq!(error.params["log_key"], "vnc.operation_failed");

    let body = format!(
        "{}\napt_command() {{ printf '未知错误' >&2; return 23; }}\nwork=$HOME\napt_attempt || fail download_failed \"$apt_reason_key\" \"$apt_reason\"",
        include_str!("apt.sh")
    );
    let error = desktop::run(&program, &cmd, &body, 5).unwrap_err();
    assert_eq!(error.params["reason"], "软件源操作失败，退出码 23");
    assert_eq!(error.params["reason_key"], "vnc.apt_failed");
    assert_eq!(error.params["exit_code"], "23");

    let error = desktop::run(
        &program,
        &cmd,
        "libs=libExample.so; fail missing_libs vnc.missing_libraries \"缺少动态库：$libs\"",
        5,
    )
    .unwrap_err();
    assert_eq!(error.params["reason"], "缺少动态库：libExample.so");
    assert_eq!(error.params["reason_key"], "vnc.missing_libraries");
    assert_eq!(error.params["libs"], "libExample.so");
}
