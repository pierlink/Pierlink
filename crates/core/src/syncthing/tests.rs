//! Syncthing 模块的纯函数、无端口子命令与可选真实实例测试。
use super::{client::Client, folder::*, home::*, process::*};
use crate::test_support::tempdir;
use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

const DEVICE: &str = "XHNZYTR-OUXF6L7-WXAMJUL-KZDWW4U-7TY5RU5-PWS3S6Z-FCSS725-6H5OXQD";
fn spec(home: PathBuf) -> HomeSpec {
    HomeSpec {
        home,
        gui: GuiListen::Tcp { port: 18384 },
        gui_user: "测试用户".into(),
        listen_port: 22001,
    }
}
#[test]
fn 文件夹设备编号和排除边界() {
    let mut ids = std::collections::HashSet::new();
    for _ in 0..1000 {
        let id = new_folder_id();
        assert_eq!(id.len(), 19);
        assert!(ids.insert(id));
    }
    validate_device_id(DEVICE).unwrap();
    for value in [
        DEVICE.to_lowercase(),
        DEVICE[..55].into(),
        DEVICE.replace('X', "0"),
        "AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA".into(),
    ] {
        assert!(validate_device_id(&value).is_err());
    }
    let rules = default_ignores();
    assert_eq!(rules.len(), 16);
    assert_eq!(rules.last().unwrap(), "(?d)*.pierlink-part");
    assert!(rules.contains(&"(?d).git".into()));
    assert!(rules.contains(&"(?d)node_modules".into()));
    assert!(!rules.iter().any(|r| r.ends_with('/')));
    validate_ignores(&rules).unwrap();
    validate_ignores(&vec!["a".into(); 10_000]).unwrap();
    validate_ignores(&["a".repeat(4096)]).unwrap();
    for rules in [
        vec!["a".into(); 10_001],
        vec!["中".repeat(1366)],
        vec!["x\0".into()],
        vec!["x\ny".into()],
    ] {
        assert!(validate_ignores(&rules).is_err());
    }
}
#[test]
fn y26_d18_旧版本外置和冲突保留() {
    let dir = tempdir();
    let mut f = FolderSpec {
        path: dir.path().join("project"),
        devices: vec![DEVICE.into()],
        ..Default::default()
    };
    let config = f.to_config().unwrap();
    assert_eq!(config["maxConflicts"], -1);
    assert_eq!(config["versioning"]["type"], "");
    assert_eq!(config["paused"], true);
    assert_eq!(config["fsWatcherDelayS"], 10);
    assert_eq!(config["rescanIntervalS"], 3600);
    assert_eq!(config["markerName"], ".stfolder");
    assert_eq!(config["ignorePerms"], false);
    f.versions_dir = Some(dir.path().join("project/child/../versions"));
    assert_eq!(
        f.to_config().unwrap_err().code,
        "syncthing.versions_inside_folder"
    );
    f.versions_dir = Some(dir.path().join("versions"));
    f.kind = FolderKind::ReceiveOnly;
    let config = f.to_config().unwrap();
    assert_eq!(config["type"], "receiveonly");
    assert_eq!(config["versioning"]["type"], "trashcan");
    assert_eq!(config["versioning"]["params"]["cleanoutDays"], "30");
    for delay in [0, 61] {
        f.watch_delay_s = delay;
        assert!(f.to_config().is_err());
    }
    f.watch_delay_s = 1;
    f.id = "pl-../../../../../../".into();
    assert!(f.to_config().is_err());
}
#[cfg(unix)]
#[test]
fn 旧版本符号链接和父目录绕过() {
    let dir = tempdir();
    let project = dir.path().join("project");
    fs::create_dir(&project).unwrap();
    std::os::unix::fs::symlink(&project, dir.path().join("alias")).unwrap();
    let f = FolderSpec {
        path: project.clone(),
        versions_dir: Some(dir.path().join("alias/versions")),
        ..Default::default()
    };
    assert_eq!(
        f.to_config().unwrap_err().code,
        "syncthing.versions_inside_folder"
    );
    fs::create_dir(project.join("child")).unwrap();
    std::os::unix::fs::symlink(project.join("child"), dir.path().join("link")).unwrap();
    let f = FolderSpec {
        versions_dir: Some(dir.path().join("link/../versions")),
        ..f
    };
    assert_eq!(
        f.to_config().unwrap_err().code,
        "syncthing.versions_inside_folder"
    );
}
#[test]
fn p4_d12_d16_xml加固保留未知字段且幂等() {
    let s = spec(PathBuf::from("/测试实例"));
    let input = r#"<?xml version="1.0"?><configuration version="52"><!--保留--><gui tls="true" custom="a&amp;b"><user>u</user><password>hash</password><apikey>secret</apikey><address>0.0.0.0:8384</address><unknown a="&amp;">keep</unknown></gui><folder id="default"/><folder id="pl-1234567890123456"/><options><listenAddress>default</listenAddress><listenAddress>quic://:22000</listenAddress><unknown>keep</unknown></options><defaults><folder><maxConflicts>10</maxConflicts></folder></defaults></configuration>"#;
    let out = super::xml::harden(input.as_bytes(), &s).unwrap();
    assert_eq!(super::xml::harden(&out, &s).unwrap(), out);
    let out = String::from_utf8(out).unwrap();
    for field in [
        "globalAnnounceEnabled",
        "localAnnounceEnabled",
        "relaysEnabled",
        "natEnabled",
        "crashReportingEnabled",
        "startBrowser",
        "announceLANAddresses",
        "insecureAdminAccess",
    ] {
        assert!(out.contains(&format!("<{field}>false</{field}>")));
    }
    for text in [
        "<autoUpgradeIntervalH>0",
        "<urAccepted>-1",
        "<urSeen>2147483647",
        "<maxConflicts>-1",
        "tcp://127.0.0.1:22001",
        "127.0.0.1:18384",
        "<!--保留-->",
        "<unknown a=\"&amp;\">keep",
        "pl-1234567890123456",
    ] {
        assert!(out.contains(text), "{text}");
    }
    assert!(!out.contains("secret"));
    assert!(!out.contains("id=\"default\""));
    assert_eq!(out.matches("<listenAddress>").count(), 1);
    for bad in [
        "",
        "<bad/>",
        "<configuration>",
        "<configuration/><configuration/>",
        "<configuration><gui/><gui/></configuration>",
        "<!DOCTYPE configuration><configuration/>",
    ] {
        assert!(super::xml::harden(bad.as_bytes(), &s).is_err(), "{bad}");
    }
}
#[test]
fn 目录原子替换和无证书查询() {
    let dir = tempdir();
    let bin = SyncthingBin {
        path: dir.path().join("不存在"),
    };
    assert!(read_device_id(&bin, dir.path()).unwrap().is_none());
    for cert in [
        "无效",
        "-----BEGIN CERTIFICATE-----MA==-----END CERTIFICATE-----",
        "-----BEGIN CERTIFICATE-----A===-----END CERTIFICATE-----",
    ] {
        fs::write(dir.path().join("cert.pem"), cert).unwrap();
        assert!(read_device_id(&bin, dir.path()).is_err());
    }
    let path = dir.path().join("config.xml");
    super::home::atomic_write(&path, b"first").unwrap();
    super::home::atomic_write(&path, b"second").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"second");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    let lock = super::home::lock(dir.path()).unwrap();
    assert_eq!(
        super::home::lock(dir.path()).unwrap_err().code,
        "syncthing.busy"
    );
    drop(lock);
    super::home::lock(dir.path()).unwrap();
    assert!(init_home(&bin, &spec(dir.path().join("home")), "").is_err());
    assert!(start(&bin, dir.path(), &GuiListen::Tcp { port: 0 }, "key").is_err());
    assert!(start(&bin, dir.path(), &GuiListen::Tcp { port: 1 }, "").is_err());
}
fn test_bin() -> Option<SyncthingBin> {
    match std::env::var_os("PIERLINK_TEST_SYNCTHING") {
        Some(path) => Some(SyncthingBin { path: path.into() }),
        None => {
            eprintln!(
                "跳过真实 Syncthing 测试：未设置 PIERLINK_TEST_SYNCTHING，需在允许监听端口的环境运行"
            );
            None
        }
    }
}
#[test]
fn 无端口真实初始化() {
    let path = std::env::var_os("PIERLINK_TEST_SYNCTHING_GENERATE")
        .or_else(|| std::env::var_os("PIERLINK_TEST_SYNCTHING"));
    let Some(path) = path else {
        eprintln!("跳过真实 generate：未设置 PIERLINK_TEST_SYNCTHING_GENERATE");
        return;
    };
    let bin = SyncthingBin { path: path.into() };
    let dir = tempdir();
    let s = spec(dir.path().join("实例"));
    let first = init_home(&bin, &s, "测试口令& 空格").unwrap();
    let config = fs::read(s.home.join("config.xml")).unwrap();
    let reported = super::process::run_short(
        &bin,
        vec!["device-id".into(), format!("--home={}", s.home.display())],
        None,
    )
    .unwrap();
    assert_eq!(first.device_id, reported.trim());
    assert_eq!(
        init_home(&bin, &s, "更新口令& 空格").unwrap().device_id,
        first.device_id
    );
    let updated = fs::read_to_string(s.home.join("config.xml")).unwrap();
    assert_ne!(updated.as_bytes(), config);
    assert!(updated.contains("<apikey></apikey>"));
    assert!(!updated.contains("更新口令"));
    assert_eq!(
        read_device_id(&bin, &s.home).unwrap(),
        Some(first.device_id)
    );
    let text = String::from_utf8(config).unwrap();
    assert!(text.contains("<maxConflicts>-1</maxConflicts>"));
    assert!(text.contains("<apikey></apikey>"));
    assert!(!text.contains("测试口令"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&s.home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(s.home.join("config.xml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
fn port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn runtime_spec(home: PathBuf) -> HomeSpec {
    let mut s = spec(home);
    s.gui = GuiListen::Tcp { port: port() };
    s.listen_port = port();
    s
}

#[test]
#[ignore = "需要本机随机端口及 PIERLINK_TEST_SYNCTHING"]
fn 管理页真实登录注销与无效口令() {
    let bin = test_bin().expect("需要指定 Syncthing 程序");
    let dir = tempdir();
    let spec = runtime_spec(dir.path().join("home"));
    init_home(&bin, &spec, "测试管理页口令").unwrap();
    let instance = start(&bin, &spec.home, &spec.gui, "测试接口密钥").unwrap();
    let GuiListen::Tcp { port } = spec.gui else {
        unreachable!()
    };
    let base = format!("http://127.0.0.1:{port}");
    let cookie = super::gui_session::login(&base, &spec.gui_user, "测试管理页口令").unwrap();
    assert!(cookie.name.starts_with("sessionid-"));
    assert_eq!(cookie.path, "/");
    assert!(super::gui_session::login(&base, &spec.gui_user, "错误口令").is_err());
    super::gui_session::logout(&base, &cookie).unwrap();
    instance.client().ping().unwrap();
}
// 两个真实 Syncthing 互相发现并同步，Windows CI 机器上偶尔超过 30 秒；失败时报调用处的行号
#[track_caller]
fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !condition() {
        assert!(Instant::now() < deadline, "60 秒内条件未满足");
        thread::sleep(Duration::from_millis(100));
    }
}
#[test]
fn p4_d12_d16_l1_l10_真实生命周期与全部管理接口() {
    let Some(bin) = test_bin() else {
        return;
    };
    let dir = tempdir();
    let s = runtime_spec(dir.path().join("home"));
    let info = init_home(&bin, &s, "runtime-password").unwrap();
    fs::write(s.home.join("pierlink.pid"), "4294967294").unwrap();
    let instance = start(&bin, &s.home, &s.gui, "runtime-key").unwrap();
    let c = instance.client();
    c.ping().unwrap();
    assert!(c.version().unwrap().starts_with('v'));
    assert_eq!(c.my_device_id().unwrap(), info.device_id);
    let options = c.get_options().unwrap();
    for key in [
        "globalAnnounceEnabled",
        "localAnnounceEnabled",
        "relaysEnabled",
        "natEnabled",
        "crashReportingEnabled",
        "startBrowser",
        "announceLANAddresses",
    ] {
        assert_eq!(options[key], false, "{key}");
    }
    assert_eq!(options["autoUpgradeIntervalH"], 0);
    assert_eq!(options["urAccepted"], -1);
    assert!(c.list_folders().unwrap().as_array().unwrap().is_empty());
    // Syncthing 对重连间隔有下限（小于 5 会被改成 5），这里用一个正常值
    c.patch_options(json!({"reconnectionIntervalS":7})).unwrap();
    assert_eq!(c.get_options().unwrap()["reconnectionIntervalS"], 7);
    let base = format!("http://{}", s.gui.address().unwrap());
    for key in ["", "wrong-key"] {
        assert_eq!(
            Client::new(&base, key).unwrap().ping().unwrap_err().code,
            "syncthing.unauthorized"
        );
    }
    let config = fs::read_to_string(s.home.join("config.xml")).unwrap();
    assert!(!config.contains("runtime-key"));
    assert!(!config.contains("runtime-password"));
    #[cfg(target_os = "linux")]
    {
        let args = fs::read(format!("/proc/{}/cmdline", instance.pid())).unwrap();
        let args = String::from_utf8_lossy(&args);
        assert!(!args.contains("runtime-key"));
        assert!(!args.contains("runtime-password"));
        assert!(args.contains("--no-restart"));
        assert_eq!(
            unsafe { libc::getpgid(instance.pid() as i32) },
            instance.pid() as i32
        );
        let pid = instance.pid();
        let children = children(pid);
        assert!(!children.is_empty(), "监控进程必须有内层服务进程");
        // 监控进程会把内层进程放进它自己的进程组（实测），所以不能靠“结束进程组”一起结束；
        // 两者同属一个会话，结束时按程序路径和命令行逐个匹配（见 process::terminate_group）
        for child in children {
            assert_eq!(unsafe { libc::getsid(child as i32) }, unsafe {
                libc::getsid(pid as i32)
            });
        }
    }
    let f = FolderSpec {
        path: dir.path().join("project"),
        devices: vec![info.device_id.clone()],
        ..Default::default()
    };
    c.put_folder(&f).unwrap();
    assert!(c.get_folder(&f.id).unwrap().is_some());
    c.patch_folder(&f.id, json!({"paused":false})).unwrap();
    until(|| f.path.join(".stfolder").exists());
    c.set_ignores(&f.id, &default_ignores()).unwrap();
    assert!(
        c.get_ignores(&f.id).unwrap()["ignore"]
            .as_array()
            .unwrap()
            .contains(&json!("(?d).git"))
    );
    fs::write(f.path.join("hello.txt"), "hello").unwrap();
    c.scan(&f.id, "").unwrap();
    until(|| c.db_status(&f.id).is_ok_and(|v| v["localFiles"] == 1));
    c.db_completion(&f.id, &info.device_id).unwrap();
    c.connections().unwrap();
    c.browse(&f.id, "", 2).unwrap();
    c.file_info(&f.id, "hello.txt").unwrap();
    c.folder_versions(&f.id).unwrap();
    let events = c.events(0, 1, &["StateChanged".into()]).unwrap();
    assert!(events.events.iter().all(|e| e.id <= events.last_id));
    c.remove_folder(&f.id).unwrap();
    assert!(c.get_folder(&f.id).unwrap().is_none());
    assert!(f.path.join("hello.txt").exists());
    let adopted = start(&bin, &s.home, &s.gui, "runtime-key").unwrap();
    assert_eq!(adopted.pid(), instance.pid());
    adopted.stop().unwrap();
    instance.stop().unwrap();
    assert!(!s.home.join("pierlink.pid").exists());
    fs::write(s.home.join("pierlink.pid"), std::process::id().to_string()).unwrap();
    let instance = start(&bin, &s.home, &s.gui, "runtime-key").unwrap();
    assert_ne!(instance.pid(), std::process::id());
    instance.client().shutdown().unwrap();
    instance.stop().unwrap();
}
#[test]
fn y4_y25_y26_双实例同步排除及关闭旧版本时保留冲突() {
    let Some(bin) = test_bin() else {
        return;
    };
    let dir = tempdir();
    let a = runtime_spec(dir.path().join("a"));
    let b = runtime_spec(dir.path().join("b"));
    let aid = init_home(&bin, &a, "password-a").unwrap().device_id;
    let bid = init_home(&bin, &b, "password-b").unwrap().device_id;
    let mut ia = start(&bin, &a.home, &a.gui, "key-a").unwrap();
    let mut ib = start(&bin, &b.home, &b.gui, "key-b").unwrap();
    let pair = |ia: &RunningInstance, ib: &RunningInstance| {
        ia.client()
            .add_device(&bid, "乙", &[format!("tcp://127.0.0.1:{}", b.listen_port)])
            .unwrap();
        ib.client()
            .add_device(&aid, "甲", &[format!("tcp://127.0.0.1:{}", a.listen_port)])
            .unwrap();
    };
    pair(&ia, &ib);
    for i in [&ia, &ib] {
        i.client()
            .patch_options(json!({"reconnectionIntervalS":1}))
            .unwrap();
    }
    let fa = FolderSpec {
        path: dir.path().join("pa"),
        devices: vec![aid.clone(), bid.clone()],
        watch_delay_s: 1,
        ..Default::default()
    };
    let fb = FolderSpec {
        path: dir.path().join("pb"),
        ..fa.clone()
    };
    for (i, f) in [(&ia, &fa), (&ib, &fb)] {
        fs::create_dir(&f.path).unwrap();
        i.client().put_folder(f).unwrap();
        i.client().set_ignores(&f.id, &default_ignores()).unwrap();
        assert_eq!(
            i.client().get_folder(&f.id).unwrap().unwrap()["paused"],
            true
        );
    }
    fs::create_dir_all(fa.path.join("nested/.git")).unwrap();
    fs::write(fa.path.join("nested/.git/config"), "不应同步").unwrap();
    fs::write(fa.path.join("hello.txt"), "初始内容").unwrap();
    for name in [
        "名字.pierlink-part",
        ".pierlink-1234.pierlink-part",
        "nested/.pierlink-5678.pierlink-part",
    ] {
        fs::write(fa.path.join(name), "不应同步").unwrap();
    }
    for i in [&ia, &ib] {
        i.client()
            .patch_folder(&fa.id, json!({"paused":false}))
            .unwrap();
    }
    until(|| fs::read_to_string(fb.path.join("hello.txt")).is_ok_and(|s| s == "初始内容"));
    assert!(!fb.path.join("nested/.git").exists());
    ia.client().scan(&fa.id, "").unwrap();
    until(|| {
        ia.client()
            .db_status(&fa.id)
            .is_ok_and(|v| v["state"] == "idle" && v["localFiles"] == 1)
    });
    for name in [
        "名字.pierlink-part",
        ".pierlink-1234.pierlink-part",
        "nested/.pierlink-5678.pierlink-part",
    ] {
        assert!(!fb.path.join(name).exists());
    }
    for i in [&ia, &ib] {
        let list = i.client().list_devices().unwrap();
        for d in list.as_array().unwrap() {
            assert_eq!(d["autoAcceptFolders"], false);
            assert_eq!(d["introducer"], false);
        }
    }
    // 停止后分别产生新内容；重启时暂用不可达地址，先确保两端本地索引均已更新。
    ia.client()
        .add_device(&bid, "乙", &["tcp://127.0.0.1:1".into()])
        .unwrap();
    ib.client()
        .add_device(&aid, "甲", &["tcp://127.0.0.1:1".into()])
        .unwrap();
    ia.stop().unwrap();
    ib.stop().unwrap();
    fs::write(fa.path.join("hello.txt"), "甲的并行变更内容").unwrap();
    fs::write(fb.path.join("hello.txt"), "乙的不同并行变更").unwrap();
    ia = start(&bin, &a.home, &a.gui, "key-a").unwrap();
    ib = start(&bin, &b.home, &b.gui, "key-b").unwrap();
    for i in [&ia, &ib] {
        i.client().scan(&fa.id, "").unwrap();
    }
    until(|| {
        let x = ia.client().file_info(&fa.id, "hello.txt").ok();
        let y = ib.client().file_info(&fa.id, "hello.txt").ok();
        matches!((x,y),(Some(x),Some(y)) if !x["local"]["blocksHash"].is_null() && x["local"]["blocksHash"]!=y["local"]["blocksHash"])
    });
    pair(&ia, &ib);
    until(|| {
        [&fa.path, &fb.path].iter().any(|p| {
            fs::read_dir(p)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains("sync-conflict"))
        })
    });
    let mut contents = std::collections::HashSet::new();
    for p in [&fa.path, &fb.path] {
        for entry in fs::read_dir(p).unwrap().flatten() {
            if entry.path().is_file() && entry.file_name().to_string_lossy().starts_with("hello") {
                contents.insert(fs::read_to_string(entry.path()).unwrap());
            }
        }
    }
    assert!(contents.contains("甲的并行变更内容"));
    assert!(contents.contains("乙的不同并行变更"));
    for i in [&ia, &ib] {
        let f = i.client().get_folder(&fa.id).unwrap().unwrap();
        assert_eq!(f["maxConflicts"], -1);
        assert_eq!(f["versioning"]["type"], "");
    }
    ia.client().remove_device(&bid).unwrap();
    ib.client().remove_device(&aid).unwrap();
    ia.stop().unwrap();
    ib.stop().unwrap();
}
#[test]
fn p4_p6_管理端口及同步端口被占用() {
    let Some(bin) = test_bin() else {
        return;
    };
    for gui in [true, false] {
        let dir = tempdir();
        let mut s = runtime_spec(dir.path().join("home"));
        let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p = occupied.local_addr().unwrap().port();
        if gui {
            s.gui = GuiListen::Tcp { port: p };
        } else {
            s.listen_port = p;
        }
        init_home(&bin, &s, "password").unwrap();
        match start(&bin, &s.home, &s.gui, "secret-key") {
            Err(e) => assert_eq!(e.code, "syncthing.listen_port_in_use", "{e:?}"),
            Ok(i) => {
                i.stop().unwrap();
                panic!("未识别端口占用");
            }
        }
        assert!(!s.home.join("pierlink.pid").exists());
    }
}
#[cfg(unix)]
#[test]
fn p5_l10_unix套接字权限和认证() {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let Some(bin) = test_bin() else {
        return;
    };
    let dir = tempdir();
    let mut s = runtime_spec(dir.path().join("h"));
    let socket = dir.path().join("s/g");
    s.gui = GuiListen::Unix {
        path: socket.clone(),
    };
    init_home(&bin, &s, "password").unwrap();
    let instance = start(&bin, &s.home, &s.gui, "key").unwrap();
    assert!(fs::metadata(&socket).unwrap().file_type().is_socket());
    assert_eq!(
        fs::metadata(socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    instance.client().ping().unwrap();
    assert_eq!(
        Client::unix(&socket, "wrong")
            .unwrap()
            .ping()
            .unwrap_err()
            .code,
        "syncthing.unauthorized"
    );
    instance.stop().unwrap();
}

#[cfg(target_os = "linux")]
fn children(pid: u32) -> Vec<u32> {
    fs::read_dir(format!("/proc/{pid}/task"))
        .unwrap()
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path().join("children")).ok())
        .flat_map(|s| {
            s.split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}
#[test]
fn 版本祖先与权限显式指定() {
    let dir = tempdir();
    let mut f = FolderSpec {
        path: dir.path().join("MixedCase/Project"),
        ..Default::default()
    };
    for version in [
        dir.path().to_owned(),
        f.path.clone(),
        PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" }),
    ] {
        f.versions_dir = Some(version);
        assert_eq!(
            f.to_config().unwrap_err().code,
            "syncthing.versions_contains_folder"
        );
    }
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        f.versions_dir = Some(home.into());
        assert_eq!(
            f.to_config().unwrap_err().code,
            "syncthing.versions_contains_folder"
        );
    }
    f.versions_dir = Some(dir.path().join("Versions"));
    for ignore in [true, false] {
        f.ignore_perms = ignore;
        let v = f.to_config().unwrap();
        assert_eq!(v["ignorePerms"], ignore);
        assert!(v["path"].as_str().unwrap().contains("MixedCase"));
        assert!(
            v["versioning"]["fsPath"]
                .as_str()
                .unwrap()
                .contains("Versions")
        );
    }
}
#[test]
fn xml命名空间与重复默认元素() {
    let s = spec("/实例".into());
    for fragment in [
        r#"<x:options xmlns:x="urn:x"><x:listenAddress>default</x:listenAddress></x:options>"#,
        r#"<options xmlns="urn:x"/>"#,
        r#"<options x:field="value"/>"#,
        "<defaults><folder/><folder/></defaults>",
        "<defaults><device/><device/></defaults>",
    ] {
        let xml = format!(
            "<configuration><gui><password>hash</password></gui>{fragment}</configuration>"
        );
        assert_eq!(
            super::xml::harden(xml.as_bytes(), &s).unwrap_err().code,
            "syncthing.invalid_config"
        );
    }
}
#[test]
fn 套接字按字节限长() {
    for (path, valid) in [
        (format!("/{}", "a".repeat(98)), true),
        (format!("/{}", "a".repeat(99)), false),
        (format!("/{}", "中".repeat(33)), false),
    ] {
        let result = GuiListen::Unix { path: path.into() }.address();
        if valid {
            assert!(result.is_ok());
        } else {
            assert_eq!(result.unwrap_err().code, "syncthing.socket_path_too_long");
        }
    }
}
#[cfg(unix)]
#[test]
fn 拒绝更改已有目录权限() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir();
    let shared = dir.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        super::home::private_dir(&shared).unwrap_err().code,
        "syncthing.socket_dir_insecure"
    );
    assert_eq!(
        fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        super::home::socket_dir(dir.path()).unwrap_err().code,
        "syncthing.socket_dir_insecure"
    );
    let dedicated = shared.join("pierlink");
    super::home::socket_dir(&dedicated).unwrap();
    super::home::socket_dir(&dedicated).unwrap();
    assert_eq!(
        fs::metadata(&dedicated).unwrap().permissions().mode() & 0o777,
        0o700
    );
    std::os::unix::fs::symlink(&dedicated, shared.join("link")).unwrap();
    assert!(super::home::private_dir(&shared.join("link")).is_err());
}
#[cfg(target_os = "linux")]
#[test]
fn 监控崩溃恢复及认证失败强制停止() {
    let Some(bin) = test_bin() else {
        return;
    };
    let dir = tempdir();
    let s = runtime_spec(dir.path().join("home"));
    init_home(&bin, &s, "password").unwrap();
    let original = start(&bin, &s.home, &s.gui, "key").unwrap();
    let inner = children(original.pid());
    assert!(!inner.is_empty());
    assert_eq!(
        unsafe { libc::kill(original.pid() as i32, libc::SIGKILL) },
        0
    );
    let adopted = start(&bin, &s.home, &s.gui, "key").unwrap();
    assert!(inner.contains(&adopted.pid()));
    match start(&bin, &s.home, &s.gui, "wrong") {
        Err(e) => {
            assert_eq!(e.code, "syncthing.adopt_failed");
            assert!(e.params.contains_key("pid"));
        }
        Ok(_) => panic!("错误密钥不应接管"),
    }
    force_stop(&s.home).unwrap();
    force_stop(&s.home).unwrap();
    adopted.stop().unwrap();
    original.stop().unwrap();
    let restarted = start(&bin, &s.home, &s.gui, "key").unwrap();
    restarted.stop().unwrap();
}

#[test]
fn 已有配置先验证再调用generate() {
    let dir = tempdir();
    let s = spec(dir.path().join("home"));
    super::home::private_dir(&s.home).unwrap();
    let xml = r#"<configuration><gui><password>hash</password></gui><x:options xmlns:x="urn:x"/></configuration>"#;
    fs::write(s.home.join("config.xml"), xml).unwrap();
    let bin = SyncthingBin {
        path: dir.path().join("不存在的程序"),
    };
    assert_eq!(
        init_home(&bin, &s, "password").unwrap_err().code,
        "syncthing.invalid_config"
    );
    assert_eq!(fs::read_to_string(s.home.join("config.xml")).unwrap(), xml);
}
#[cfg(windows)]
#[test]
fn windows大小写仅用于比较() {
    let dir = tempdir();
    let project = dir.path().join("ProjectCase");
    fs::create_dir(&project).unwrap();
    let mut f = FolderSpec {
        path: project.clone(),
        versions_dir: Some(project.join("VERSIONS")),
        ..Default::default()
    };
    assert_eq!(
        f.to_config().unwrap_err().code,
        "syncthing.versions_inside_folder"
    );
    f.versions_dir = Some(dir.path().join("VersionCase"));
    let config = f.to_config().unwrap();
    for value in [&config["path"], &config["versioning"]["fsPath"]] {
        assert!(!value.as_str().unwrap().starts_with(r"\\?\"));
    }
    assert!(config["path"].as_str().unwrap().ends_with("ProjectCase"));
}
