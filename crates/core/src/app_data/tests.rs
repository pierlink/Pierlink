use super::*;
use crate::{
    storage,
    syncthing::{
        folder::FolderSpec,
        home::{GuiListen, HomeSpec, init_home},
        process::{SyncthingBin, start},
    },
};
use serde_json::json;
use std::{
    thread,
    time::{Duration, Instant},
};

fn fixture(root: &Path) -> (PathBuf, PathBuf) {
    let old = root.join("Pierlink 旧");
    let new = root.join("com.pierlink.desktop");
    fs::create_dir_all(&old).unwrap();
    let db = storage::open(&old.join("pierlink.db")).unwrap();
    db.set_setting("layout", &json!({"paths":[old.join("keys/key"), root.join("Pierlink 旧extra/keys/a")], "embedded": format!("前缀 {}", old.display())})).unwrap();
    drop(db);
    for name in ROOT_ITEMS.iter().filter(|n| !n.starts_with("pierlink.db")) {
        fs::create_dir_all(old.join(name)).unwrap();
        fs::write(old.join(name).join("sentinel"), name).unwrap();
    }
    home::private_dir(&old.join("syncthing")).unwrap();
    let config = format!(
        r#"<configuration version="52"><folder id="pl-1234567890123456" path="{}"><versioning type="trashcan"><fsPath>{}</fsPath></versioning></folder><device id="设备不变"/></configuration>"#,
        root.join("project").display(),
        old.join("versions/pl-1234567890123456").display()
    );
    fs::write(old.join("syncthing/config.xml"), config).unwrap();
    for name in ["cert.pem", "key.pem", "index-v2/db"] {
        let p = old.join("syncthing").join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, name).unwrap();
    }
    for name in [
        "Pierlink.exe",
        "uninstall.exe",
        "openssh/ssh.exe",
        "licenses/notice.txt",
        "syncthing/syncthing.exe",
        "syncthing/syncthing-linux-amd64.gz",
        "syncthing/syncthing-linux-arm64.gz",
        "syncthing/manifest.json",
    ] {
        let p = old.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "安装程序资源保持原样").unwrap();
    }
    fs::write(
        old.join("config/ui.json"),
        json!({"key": old.join("keys/key")}).to_string(),
    )
    .unwrap();
    fs::write(
        old.join(".ssh/config"),
        format!("IdentityFile \"{}\"\n", old.join("keys/key").display()),
    )
    .unwrap();
    (old, new)
}
fn assert_programs(old: &Path, new: &Path) {
    for name in [
        "Pierlink.exe",
        "uninstall.exe",
        "openssh/ssh.exe",
        "licenses/notice.txt",
        "syncthing/syncthing.exe",
        "syncthing/syncthing-linux-amd64.gz",
        "syncthing/syncthing-linux-arm64.gz",
        "syncthing/manifest.json",
    ] {
        assert_eq!(
            fs::read_to_string(old.join(name)).unwrap(),
            "安装程序资源保持原样"
        );
        assert!(!new.join(name).exists());
    }
}
fn assert_old(old: &Path, new: &Path) {
    for name in ROOT_ITEMS.iter().filter(|n| !n.starts_with("pierlink.db")) {
        assert!(old.join(name).join("sentinel").exists(), "{name}");
        assert!(!new.join(name).exists(), "{name}");
    }
    assert!(old.join("pierlink.db").exists());
    assert!(!new.join("pierlink.db").exists());
    assert!(
        fs::read_to_string(old.join("syncthing/config.xml"))
            .unwrap()
            .contains(&old.join("versions").display().to_string())
    );
    assert_eq!(fs::read(old.join("syncthing/key.pem")).unwrap(), b"key.pem");
    assert_programs(old, new);
}
#[test]
fn 完整数据搬运改写且保留全部安装资源() {
    let root = crate::test_support::tempdir();
    let (old, new) = fixture(root.path());
    fs::create_dir_all(new.join("EBWebView")).unwrap();
    fs::write(new.join("EBWebView/保留"), "WebView").unwrap();
    let report = migrate_data_dir(&old, &new).unwrap().unwrap();
    assert_eq!(report.rewritten, 4);
    for name in ROOT_ITEMS.iter().filter(|n| !n.starts_with("pierlink.db")) {
        assert!(new.join(name).join("sentinel").exists(), "{name}");
        assert!(!old.join(name).exists());
    }
    assert_programs(&old, &new);
    assert!(report.skipped.contains(&"syncthing/syncthing.exe".into()));
    let xml = fs::read_to_string(new.join("syncthing/config.xml")).unwrap();
    assert!(xml.contains(&new.join("versions").display().to_string()));
    assert!(xml.contains("pl-1234567890123456"));
    assert_eq!(fs::read(new.join("syncthing/key.pem")).unwrap(), b"key.pem");
    assert_eq!(
        fs::read(new.join("syncthing/cert.pem")).unwrap(),
        b"cert.pem"
    );
    let db = storage::open(&new.join("pierlink.db")).unwrap();
    let setting = db
        .get_setting::<serde_json::Value>("layout")
        .unwrap()
        .unwrap();
    // 改写会统一为本机分隔符；Windows 的 join("keys/key") 仍可能保留正斜杠。
    assert_eq!(setting["paths"][0], json!(new.join("keys").join("key")));
    assert_eq!(
        setting["paths"][1],
        json!(root.path().join("Pierlink 旧extra/keys/a"))
    );
    assert!(
        setting["embedded"]
            .as_str()
            .unwrap()
            .contains(&old.display().to_string())
    );
    assert!(!new.join(JOURNAL).exists());
    assert!(!new.join(".pierlink-migration-finished").exists());
    assert!(migrate_data_dir(&old, &new).unwrap().is_none());
}
#[test]
fn 每一项rename及改写失败都完整回滚() {
    let root = crate::test_support::tempdir();
    for phase in ["move", "rewrite", "commit"] {
        let attempt = root.path().join(phase);
        let (old, new) = fixture(&attempt);
        let p = plan(&old, &new).unwrap();
        let count = match phase {
            "move" => p.items.len(),
            "rewrite" => p.replacements.len(),
            _ => 1,
        };
        for fail_at in 0..count {
            let mut reached = false;
            let error = migrate(&old, &new, &mut |stage, i| {
                if stage == phase && i == fail_at {
                    reached = true;
                    Err(failure(
                        "app_data.file_operation_failed_during_data_migration",
                        "模拟改名失败",
                    ))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert!(reached, "未到达 {phase}-{fail_at} 注入点：{error:?}");
            assert_eq!(
                error.params.get("safe_to_use_old").map(String::as_str),
                Some("true"),
                "{error:?}"
            );
            assert_old(&old, &new);
            assert!(!new.join(JOURNAL).exists());
        }
    }
}
#[test]
fn 强杀窗口恢复含数据库已搬走和改写中断与已提交() {
    let root = crate::test_support::tempdir();
    for phase in ["move", "rewrite", "commit", "cleanup"] {
        let template = root.path().join(format!("{phase}-template"));
        let (old, new) = fixture(&template);
        let p = plan(&old, &new).unwrap();
        let count = match phase {
            "move" => p.items.len(),
            "rewrite" => p.replacements.len(),
            _ => 1,
        };
        for stop_at in 0..count {
            let (old, new) = fixture(&root.path().join(format!("{phase}-{stop_at}")));
            let mut reached = false;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                migrate(&old, &new, &mut |stage, i| {
                    if stage == phase && i == stop_at {
                        reached = true;
                        panic!("模拟进程被强杀");
                    }
                    Ok(())
                })
            }));
            assert!(reached, "未到达 {phase}-{stop_at} 注入点：{result:?}");
            assert!(result.is_err(), "{phase}-{stop_at} 应被中断");
            assert!(new.join(JOURNAL).exists());
            migrate_data_dir(&old, &new).unwrap().unwrap();
            assert!(!new.join(JOURNAL).exists());
            assert!(!old.join("pierlink.db").exists());
            assert_programs(&old, &new);
        }
    }
}
#[test]
fn 新安装已有数据库冲突和无法停止均不动旧数据() {
    let root = crate::test_support::tempdir();
    assert!(
        migrate_data_dir(&root.path().join("absent"), &root.path().join("new"))
            .unwrap()
            .is_none()
    );
    let (old, new) = fixture(root.path());
    fs::create_dir(&new).unwrap();
    fs::write(new.join("pierlink.db"), "已有数据库即使损坏也不能覆盖").unwrap();
    assert!(migrate_data_dir(&old, &new).unwrap().is_none());
    fs::rename(new.join("pierlink.db"), root.path().join("已有数据库")).unwrap();
    fs::write(new.join("keys"), "新目录同名内容").unwrap();
    assert!(migrate_data_dir(&old, &new).is_err());
    assert!(old.join("pierlink.db").exists());
    fs::rename(new.join("keys"), root.path().join("同名文件")).unwrap();
    fs::write(
        old.join("syncthing/pierlink.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    assert!(migrate_data_dir(&old, &new).is_err());
    assert_old(&old, &new);
    assert!(migrate_data_dir(&old, &old).is_err());
    assert!(migrate_data_dir(&old, &old.join("child")).is_err());
}
#[test]
fn 坏数据库改写失败不影响原数据() {
    let root = crate::test_support::tempdir();
    let (old, new) = fixture(root.path());
    fs::write(old.join("pierlink.db"), "损坏数据库").unwrap();
    assert_eq!(
        migrate_data_dir(&old, &new).unwrap_err().params["safe_to_use_old"],
        "true"
    );
    assert_old(&old, &new);
    assert_eq!(
        fs::read(old.join("pierlink.db")).unwrap(),
        "损坏数据库".as_bytes()
    );
}

#[test]
fn wal未合并的密钥路径改写及失败后原始字节恢复() {
    let root = crate::test_support::tempdir();
    let (old, new) = fixture(root.path());
    let source = root.path().join("source.db");
    drop(storage::open(&source).unwrap());
    let conn = rusqlite::Connection::open(&source).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    conn.execute("INSERT INTO servers(id,name,source,host,auth_method,identity_file,created_at,updated_at) VALUES ('a','甲','manual','host','dedicated_key',?1,0,0)", [old.join("keys/key").to_str().unwrap()]).unwrap();
    let identities =
        serde_json::to_string(&vec![old.join("keys/key").to_str().unwrap(), "none"]).unwrap();
    conn.execute(
        "UPDATE servers SET identity_files=?1 WHERE id='a'",
        [identities],
    )
    .unwrap();
    for suffix in ["", "-wal", "-shm"] {
        fs::copy(
            root.path().join(format!("source.db{suffix}")),
            old.join(format!("pierlink.db{suffix}")),
        )
        .unwrap();
    }
    let originals: Vec<_> = ["", "-wal", "-shm"]
        .iter()
        .map(|suffix| fs::read(old.join(format!("pierlink.db{suffix}"))).unwrap())
        .collect();
    let error = migrate(&old, &new, &mut |stage, _| {
        if stage == "commit" {
            Err(failure(
                "app_data.file_operation_failed_during_data_migration",
                "模拟提交前失败",
            ))
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert_eq!(error.params["safe_to_use_old"], "true");
    for (i, suffix) in ["", "-wal", "-shm"].iter().enumerate() {
        assert_eq!(
            fs::read(old.join(format!("pierlink.db{suffix}"))).unwrap(),
            originals[i]
        );
    }
    let report = migrate_data_dir(&old, &new).unwrap().unwrap();
    assert!(report.rewritten >= 1);
    let db = storage::open(&new.join("pierlink.db")).unwrap();
    assert_eq!(
        db.get_server("a")
            .unwrap()
            .unwrap()
            .identity_file
            .as_deref(),
        new.join("keys").join("key").to_str()
    );
    assert_eq!(
        db.get_server("a").unwrap().unwrap().identity_files,
        [new.join("keys").join("key").to_str().unwrap(), "none"]
    );
}

#[test]
fn 回滚本身再次中断也可以重复恢复() {
    let root = crate::test_support::tempdir();
    let (old, new) = fixture(root.path());
    let mut reached = false;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        migrate(&old, &new, &mut |phase, _| {
            if phase == "commit" {
                reached = true;
                panic!("模拟提交前中断");
            }
            Ok(())
        })
    }));
    assert!(reached, "未到达提交前注入点：{result:?}");
    assert!(result.is_err());
    let journal = new.join(JOURNAL);
    let p: Plan = load(&journal.join("plan.json")).unwrap();
    // 模拟回滚恢复文件后、记录恢复标记之前中断。
    for (i, name) in p.replacements.iter().enumerate() {
        restore_copy(
            &journal.join(format!("original-{i}")),
            &new.join(name),
            &journal.join(format!("restore-{i}")),
        )
        .unwrap();
    }
    for (i, name) in p.replacements.iter().enumerate() {
        restore_copy(
            &journal.join(format!("original-{i}")),
            &new.join(name),
            &journal.join(format!("restore-{i}")),
        )
        .unwrap();
        save(&journal.join(format!("restored-{i}")), &true).unwrap();
    }
    // 模拟部分目录已搬回旧位置时又被强杀。
    for name in p.items.iter().rev().take(3) {
        rename(&new.join(name), &old.join(name)).unwrap();
    }
    migrate_data_dir(&old, &new).unwrap().unwrap();
    assert!(!journal.exists());
    assert!(new.join("pierlink.db").exists());
    assert_programs(&old, &new);
}

#[test]
fn 恢复副本重复替换已有文件且保留原件() {
    let root = crate::test_support::tempdir();
    let original = root.path().join("original");
    let target = root.path().join("target");
    let temporary = root.path().join("restore");
    fs::write(&original, "原始字节").unwrap();
    fs::write(&target, "改写后的字节").unwrap();
    for _ in 0..2 {
        restore_copy(&original, &target, &temporary).unwrap();
        assert_eq!(fs::read(&target).unwrap(), fs::read(&original).unwrap());
        assert!(!temporary.exists());
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // 独占打开验证刷盘及替换没有留下句柄。
            drop(
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .share_mode(0)
                    .open(&target)
                    .unwrap(),
            );
        }
    }
}

#[test]
fn 文件和目录改名拒绝覆盖且目录同步可用() {
    let root = crate::test_support::tempdir();
    for directory in [false, true] {
        let attempt = root.path().join(if directory { "目录" } else { "文件" });
        fs::create_dir(&attempt).unwrap();
        let from = attempt.join("from");
        let to = attempt.join("to");
        if directory {
            fs::create_dir(&from).unwrap();
            fs::create_dir(&to).unwrap();
            fs::write(from.join("内容"), "原件").unwrap();
            fs::write(to.join("内容"), "不能覆盖").unwrap();
        } else {
            fs::write(&from, "原件").unwrap();
            fs::write(&to, "不能覆盖").unwrap();
        }
        assert!(rename(&from, &to).is_err());
        let content = |p: &Path| {
            fs::read(if directory {
                p.join("内容")
            } else {
                p.into()
            })
            .unwrap()
        };
        assert_eq!(content(&from), "原件".as_bytes());
        assert_eq!(content(&to), "不能覆盖".as_bytes());
        rename(&from, &attempt.join("moved")).unwrap();
        sync_dir(&attempt).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn windows_文件和目录占用时安全回滚释放后可迁移() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let root = crate::test_support::tempdir();
    for name in ["pierlink.db", "keys"] {
        let (old, new) = fixture(&root.path().join(name));
        // 允许读取预备副本，但阻止改名；目录句柄需要 BACKUP_SEMANTICS。
        let occupied = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(old.join(name))
            .unwrap();
        let error = migrate_data_dir(&old, &new).unwrap_err();
        assert_eq!(error.params["safe_to_use_old"], "true");
        assert_eq!(error.params["path"], name);
        assert!(
            matches!(error.params["os_code"].as_str(), "5" | "32"),
            "{error:?}"
        );
        assert_old(&old, &new);
        assert!(!new.join(JOURNAL).exists());
        drop(occupied);
        // 在第一次改名之前独占打开预备库，确认 SQLite 与刷盘句柄均已关闭。
        migrate(&old, &new, &mut |phase, index| {
            if phase == "move" && index == 0 {
                let prepared = new.join(JOURNAL).join("prepared");
                drop(
                    fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .share_mode(0)
                        .open(prepared.join("pierlink.db"))
                        .unwrap(),
                );
                for suffix in ["-wal", "-shm", "-journal"] {
                    assert!(!prepared.join(format!("pierlink.db{suffix}")).exists());
                }
            }
            Ok(())
        })
        .unwrap()
        .unwrap();
        assert_programs(&old, &new);
    }
}

#[test]
fn 空迁移目录及已结束清理残留会自动回收() {
    let root = crate::test_support::tempdir();
    let (old, new) = fixture(root.path());
    home::private_dir(&new.join(JOURNAL)).unwrap();
    migrate_data_dir(&old, &new).unwrap().unwrap();
    let garbage = new.join(".pierlink-migration-finished");
    fs::create_dir(&garbage).unwrap();
    fs::write(garbage.join("已完成工作区"), "临时数据").unwrap();
    assert!(migrate_data_dir(&old, &new).unwrap().is_none());
    assert!(!garbage.exists());
}

#[test]
fn 每日数据库备份中的路径也随迁移改写() {
    let root = crate::test_support::tempdir();
    let (old, new) = fixture(root.path());
    let db = storage::open(&old.join("pierlink.db")).unwrap();
    db.backup_daily(&old.join("backups")).unwrap();
    drop(db);
    let report = migrate_data_dir(&old, &new).unwrap().unwrap();
    assert_eq!(report.rewritten, 5);
    let backup = fs::read_dir(new.join("backups"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "db"))
        .unwrap();
    let db = storage::open(&backup).unwrap();
    let layout = db
        .get_setting::<serde_json::Value>("layout")
        .unwrap()
        .unwrap();
    assert_eq!(layout["paths"][0], json!(new.join("keys").join("key")));
}

#[test]
fn 损坏或越界的迁移记录保留现场并阻止启动() {
    let root = crate::test_support::tempdir();
    let (old, new) = fixture(root.path());
    home::private_dir(&new.join(JOURNAL)).unwrap();
    let file = new.join(JOURNAL).join("plan.json");
    fs::write(&file, "坏记录").unwrap();
    assert_eq!(
        migrate_data_dir(&old, &new).unwrap_err().code,
        "app.data_migration_recovery_required"
    );
    assert!(old.join("pierlink.db").exists());
    let mut p = plan(&old, &new).unwrap();
    p.items.push("../外部文件".into());
    save(&file, &p).unwrap();
    assert_eq!(
        migrate_data_dir(&old, &new).unwrap_err().code,
        "app.data_migration_recovery_required"
    );
    assert!(file.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn 迁移先核验并停止旧home进程() {
    use std::process::{Command, Stdio};
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = crate::test_support::tempdir();
    let (old, new) = fixture(root.path());
    let h = old.join("syncthing");
    let bin = SyncthingBin {
        path: "/usr/bin/python3".into(),
    };
    let mut child = Child(
        Command::new(&bin.path)
            .args([
                "-c",
                "import time; time.sleep(30)",
                "serve",
                "--no-restart",
                &format!("--home={}", h.display()),
                "--gui-address=127.0.0.1:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !process::record_is_running(&bin, &h, &GuiListen::Tcp { port: 1 }).unwrap() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    fs::write(
        h.join("pierlink-process.json"),
        serde_json::to_vec(&(&bin.path, "127.0.0.1:1")).unwrap(),
    )
    .unwrap();
    fs::write(h.join("pierlink.pid"), child.0.id().to_string()).unwrap();
    migrate_data_dir(&old, &new).unwrap().unwrap();
    assert!(!child.0.wait().unwrap().success());
    assert!(!new.join("syncthing/pierlink.pid").exists());
}
fn port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
#[test]
fn 真实syncthing无端口迁移后再次初始化保留证书和版本配置() {
    let Some(bin) = std::env::var_os("PIERLINK_TEST_SYNCTHING") else {
        eprintln!("跳过真实无端口迁移：未设置 PIERLINK_TEST_SYNCTHING");
        return;
    };
    let bin = SyncthingBin { path: bin.into() };
    let root = crate::test_support::tempdir();
    let old = root.path().join("old");
    let new = root.path().join("new");
    drop(storage::open(&old.join("pierlink.db")).unwrap());
    let mut spec = HomeSpec {
        home: old.join("syncthing"),
        gui: GuiListen::Tcp { port: 18384 },
        gui_user: "迁移测试".into(),
        listen_port: 22001,
    };
    let info = init_home(&bin, &spec, "测试口令").unwrap();
    let key = fs::read(spec.home.join("key.pem")).unwrap();
    let xml = fs::read_to_string(spec.home.join("config.xml")).unwrap();
    let folder = format!(
        "<folder id=\"pl-1234567890123456\" path=\"{}\" type=\"sendreceive\"><versioning type=\"trashcan\"><fsPath>{}</fsPath><fsType>basic</fsType></versioning></folder>",
        root.path().join("project").display(),
        old.join("versions/test").display()
    );
    fs::write(
        spec.home.join("config.xml"),
        xml.replace("</configuration>", &format!("{folder}</configuration>")),
    )
    .unwrap();
    migrate_data_dir(&old, &new).unwrap().unwrap();
    spec.home = new.join("syncthing");
    assert_eq!(
        init_home(&bin, &spec, "测试口令").unwrap().device_id,
        info.device_id
    );
    assert_eq!(fs::read(spec.home.join("key.pem")).unwrap(), key);
    let xml = fs::read_to_string(spec.home.join("config.xml")).unwrap();
    assert!(xml.contains("pl-1234567890123456"));
    assert!(xml.contains(&new.join("versions").join("test").display().to_string()));
}
#[test]
fn 真实syncthing迁移版本路径证书与文件夹标识保持() {
    let Some(bin) = std::env::var_os("PIERLINK_TEST_SYNCTHING") else {
        eprintln!("跳过真实迁移测试：未设置 PIERLINK_TEST_SYNCTHING");
        return;
    };
    let bin = SyncthingBin { path: bin.into() };
    let root = crate::test_support::tempdir();
    let old = root.path().join("old");
    let new = root.path().join("new");
    drop(storage::open(&old.join("pierlink.db")).unwrap());
    let spec = HomeSpec {
        home: old.join("syncthing"),
        gui: GuiListen::Tcp { port: port() },
        gui_user: "迁移测试".into(),
        listen_port: port(),
    };
    let info = init_home(&bin, &spec, "测试口令").unwrap();
    let cert = fs::read(spec.home.join("cert.pem")).unwrap();
    let key = fs::read(spec.home.join("key.pem")).unwrap();
    let instance = start(&bin, &spec.home, &spec.gui, "测试密钥").unwrap();
    let folder = FolderSpec {
        path: root.path().join("project"),
        versions_dir: Some(old.join("versions/folder")),
        devices: vec![info.device_id.clone()],
        paused: false,
        ..Default::default()
    };
    instance.client().put_folder(&folder).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !folder.path.join(".stfolder").exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(50));
    }
    instance.stop().unwrap();
    let report = migrate_data_dir(&old, &new).unwrap().unwrap();
    assert!(report.rewritten >= 1);
    assert_eq!(fs::read(new.join("syncthing/cert.pem")).unwrap(), cert);
    assert_eq!(fs::read(new.join("syncthing/key.pem")).unwrap(), key);
    let migrated = start(&bin, &new.join("syncthing"), &spec.gui, "测试密钥").unwrap();
    assert_eq!(migrated.client().my_device_id().unwrap(), info.device_id);
    let actual = migrated.client().get_folder(&folder.id).unwrap().unwrap();
    assert_eq!(actual["path"], json!(folder.path));
    assert_eq!(
        actual["versioning"]["fsPath"],
        json!(new.join("versions").join("folder"))
    );
    migrated.stop().unwrap();
}
