use super::{
    desktop_tests::{local, script_file},
    *,
};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Mutex, atomic::AtomicBool},
};

fn fake_apt(home: &Path, cmd: &mut crate::ssh::args::SshCommand) {
    let bin = home.join("tools");
    fs::create_dir(&bin).unwrap();
    cmd.env
        .push(("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())));
    script_file(
        &bin.join("apt-get"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/apt-calls"
case "$*" in
  *update*) exit 0;;
esac
for arg do package=$arg; done
case "$*" in
  *--print-uris*) printf "'https://发行版源/包' 包.deb %s SHA256:测试\n" "${FAKE_SIZE:-1}";;
  *download*)
    if [ "$package" = "${FAKE_FAIL:-}" ]; then
      printf '%s\n' 'Temporary failure resolving https://user:绝不回显@软件源/' >&2; exit 1
    fi
    printf x > "$package.deb";;
  *) exit 1;;
esac
"#,
    );
    script_file(
        &bin.join("apt-cache"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/cache-calls"
for arg do
  case "$arg" in openbox|xterm|dbus|dbus-bin|dbus-daemon|fontconfig|fonts-dejavu-core|fonts-wqy-microhei|xauth|x11-xkb-utils|xkb-data|tigervnc-common|libgtk-3-bin|libdbus-glib-1-2|libnss3) printf '%s\n  Depends: libmissing:amd64\n' "$arg";; esac
done
cat <<'EOF'
tigervnc-standalone-server
  Depends: <虚包>
    libc6:amd64
  Depends: libmissing:amd64
  Depends: libc6:amd64
libmissing:amd64
libc6:amd64
<虚包>
  已有提供者
EOF
"#,
    );
    script_file(
        &bin.join("dpkg-query"),
        r#"#!/bin/sh
for arg do package=$arg; done
[ "$package" = libc6:amd64 ] || exit 1
printf 'install ok installed'
"#,
    );
    script_file(
        &bin.join("dpkg-deb"),
        r#"#!/bin/sh
if [ "$1" = -f ]; then
  if [ "$3" = Version ]; then printf %s "${FAKE_VERSION:-1.12.0-1}";
  else name=${2##*/}; name=${name%.deb}; printf %s "${name%%:*}"; fi
else
  printf '%s\n' "$2" >> "$HOME/extracted"
  mkdir -p "$3/usr/bin" "$3/usr/lib/x86_64-linux-gnu" "$3/usr/share/fonts" "$3/usr/share/X11/xkb/rules"
  for name in Xtigervnc openbox xterm dbus-run-session dbus-daemon; do
    printf '#!/bin/sh\nprintf "TigerVNC 1.12.0"\n' > "$3/usr/bin/$name"
    chmod 700 "$3/usr/bin/$name"
  done
  printf x > "$3/usr/lib/x86_64-linux-gnu/libmissing.so.0"
  printf x > "$3/usr/share/X11/xkb/rules/evdev"
fi
"#,
    );
    script_file(&bin.join("ldd"), "#!/bin/sh\nexit 0\n");
}

#[test]
fn 依赖闭包过滤虚包已装包并保留多架构名称() {
    let (root, program, mut cmd) = local();
    fake_apt(root.path(), &mut cmd);
    let sample = "openbox\n  Depends: libc6:amd64\n |Depends: <x-terminal-emulator>\n    xterm\n<virtual>\nlibc6:amd64\nlibmissing:amd64\nlibmissing:i386\nlibmissing:amd64\n  Suggests: bad\n--option\npkg;bad\n";
    let body = format!(
        "{}\npackages=$(printf %s {} | dependency_names | missing_packages)\nprintf '{{\"packages\":'; json \"$packages\"; printf '}}'",
        include_str!("apt.sh"),
        crate::ssh::quote::sh_quote(sample).unwrap()
    );
    let value = desktop::run(&program, &cmd, &body, 10).unwrap();
    assert_eq!(
        value["packages"],
        "openbox\nlibmissing:amd64\nlibmissing:i386"
    );
}

#[test]
fn 私有索引先更新依赖统一解包并清理下载与索引() {
    let (root, program, mut cmd) = local();
    fake_apt(root.path(), &mut cmd);
    let binary = acquire::remote_package(&program, &cmd, true).unwrap();
    assert!(binary.path.ends_with("/apt-1.12.0-1/usr/bin/Xtigervnc"));
    let prefix = Path::new(&binary.path)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    assert!(
        prefix
            .join("usr/lib/x86_64-linux-gnu/libmissing.so.0")
            .exists()
    );
    let calls = fs::read_to_string(root.path().join("apt-calls")).unwrap();
    assert!(calls.lines().next().unwrap().contains("update"));
    for line in calls.lines() {
        assert!(line.contains("Dir::State::Lists="));
        assert!(line.contains("Dir::Cache="));
        assert!(line.contains("AllowUnauthenticated=false"));
    }
    assert!(calls.contains("download libmissing:amd64"));
    assert!(!calls.contains("libc6:amd64"));
    assert!(!calls.contains("虚包"));
    let cache = fs::read_to_string(root.path().join("cache-calls")).unwrap();
    assert!(cache.contains("--recurse --no-recommends --no-suggests --no-conflicts --no-breaks --no-replaces --no-enhances"));
    assert_eq!(
        fs::read_dir(root.path().join(".cache/pierlink/apt"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        fs::read_dir(root.path().join(".local/share/pierlink/vnc"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn 下载总量超限时不下载任何包并清理临时目录() {
    let (root, program, mut cmd) = local();
    fake_apt(root.path(), &mut cmd);
    cmd.env.push(("FAKE_SIZE".into(), "200000000".into()));
    let e = acquire::remote_package(&program, &cmd, true).unwrap_err();
    assert_eq!(e.code, "vnc.download_limit");
    assert_eq!(e.params["limit_bytes"], "314572800");
    assert_eq!(e.params["total_bytes"], "400000000");
    assert_eq!(e.params["reason_key"], "vnc.apt_total_limit");
    let calls = fs::read_to_string(root.path().join("apt-calls")).unwrap();
    assert!(
        calls
            .lines()
            .all(|line| line.contains("update") || line.contains("--print-uris"))
    );
    assert_eq!(
        fs::read_dir(root.path().join(".local/share/pierlink/vnc"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        fs::read_dir(root.path().join(".cache/pierlink/apt"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn 依赖包下载失败保留包名和原因且不破坏已有版本() {
    let (root, program, mut cmd) = local();
    fake_apt(root.path(), &mut cmd);
    let old = root.path().join(".local/share/pierlink/vnc/old");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join(".pierlink-created"), "true").unwrap();
    cmd.env
        .push(("FAKE_FAIL".into(), "libmissing:amd64".into()));
    let e = acquire::remote_package(&program, &cmd, true).unwrap_err();
    assert_eq!(e.code, "vnc.download_failed");
    assert_eq!(e.params["package"], "libmissing:amd64");
    assert_eq!(e.params["reason"], "无法解析软件源域名");
    assert_eq!(e.params["reason_key"], "vnc.apt_dns");
    assert!(!format!("{e:?}").contains("绝不回显"));
    assert!(old.exists());
    assert_eq!(fs::read_dir(old.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn 软件包版本含_epoch_时目录仍可用于环境搜索路径() {
    let (root, program, mut cmd) = local();
    fake_apt(root.path(), &mut cmd);
    cmd.env.push(("FAKE_VERSION".into(), "1:1.12.0-1".into()));
    let binary = acquire::remote_package(&program, &cmd, true).unwrap();
    assert!(binary.path.ends_with("/apt-1_1.12.0-1/usr/bin/Xtigervnc"));
}

#[test]
fn 轻量桌面获取带字体菜单和字节进度并复用完整版本() {
    let (root, program, mut cmd) = local();
    fake_apt(root.path(), &mut cmd);
    let events = Mutex::new(Vec::new());
    let prefix = acquire_light_desktop(
        &program,
        &cmd,
        &|e| events.lock().unwrap().push(e),
        &AtomicBool::new(false),
    )
    .unwrap();
    let prefix = Path::new(&prefix);
    assert!(prefix.join("session").exists());
    let menu = fs::read_to_string(prefix.join("etc/xdg/openbox/pierlink-menu.xml")).unwrap();
    assert!(menu.contains("终端"));
    assert!(menu.contains("xterm -u8"));
    let config = fs::read_to_string(prefix.join("etc/fonts/pierlink.conf")).unwrap();
    assert!(config.contains(prefix.to_str().unwrap()));
    let calls = fs::read_to_string(root.path().join("apt-calls")).unwrap();
    assert!(calls.contains("download fonts-wqy-microhei"));
    assert!(calls.contains("download fonts-dejavu-core"));
    assert!(calls.contains("download dbus-daemon"));
    assert!(calls.contains("download libgtk-3-bin"));
    assert!(calls.contains("download libnss3"));
    assert!(events.lock().unwrap().iter().any(|e| matches!(e, AcquireStep::Download {source: AcquireSource::Apt, bytes, total: Some(total)} if *bytes > 0 && bytes == total)));
    acquire_light_desktop(&program, &cmd, &|_| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("apt-calls")).unwrap(),
        calls
    );
    assert_eq!(
        acquire_light_desktop(
            &program,
            &cmd,
            &|_| panic!("取消后不应下载"),
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .code,
        "vnc.cancelled"
    );
    assert_eq!(fs::read_dir(prefix.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn 前缀环境系统优先并在登录后安全恢复带引号的路径() {
    let (root, program, mut cmd) = local();
    let prefix = root.path().join("前缀 ' 空格");
    for dir in ["usr/bin", "usr/lib/x86_64-linux-gnu", "etc/fonts"] {
        fs::create_dir_all(prefix.join(dir)).unwrap();
    }
    fs::write(prefix.join("etc/fonts/pierlink.conf"), "").unwrap();
    cmd.env.push(("LD_LIBRARY_PATH".into(), "/existing".into()));
    let body = format!(
        r#"
prefix_environment {}
restore=$(session_environment)
PATH=/bin
eval "$restore"
printf '{{"path":'; json "$PATH"
printf ',"libraries":'; json "$LD_LIBRARY_PATH"
printf ',"data":'; json "$XDG_DATA_DIRS"
printf ',"config":'; json "$XDG_CONFIG_DIRS"
printf ',"fonts":'; json "$FONTCONFIG_FILE"; printf '}}'
"#,
        crate::ssh::quote::sh_quote(prefix.to_str().unwrap()).unwrap()
    );
    let value = desktop::run(&program, &cmd, &body, 10).unwrap();
    assert!(
        value["path"]
            .as_str()
            .unwrap()
            .starts_with("/usr/bin:/bin:")
    );
    let libs = value["libraries"].as_str().unwrap();
    assert!(libs.find("/usr/lib").unwrap() < libs.find(prefix.to_str().unwrap()).unwrap());
    assert!(libs.contains("/existing"));
    assert!(
        value["data"]
            .as_str()
            .unwrap()
            .starts_with("/usr/local/share:/usr/share:")
    );
    assert!(value["config"].as_str().unwrap().starts_with("/etc/xdg:"));
    assert_eq!(
        value["fonts"],
        prefix.join("etc/fonts/pierlink.conf").to_str().unwrap()
    );
}

#[test]
fn 私有总线配置替换默认_session_并保留地址描述符() {
    let (root, program, mut cmd) = local();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    script_file(
        &bin.join("dbus-daemon"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\"\n",
    );
    script_file(&root.path().join("daemon"), include_str!("light_dbus.sh"));
    cmd.env
        .push(("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())));
    let cache = root.path().join("缓存 ' 空格");
    cmd.env
        .push(("XDG_CACHE_HOME".into(), cache.to_str().unwrap().into()));
    let value = desktop::run(
        &program,
        &cmd,
        r#"args=$("$HOME/daemon" --nofork --print-address 7 --session)
printf '{"args":'; json "$args"; printf '}'"#,
        10,
    )
    .unwrap();
    assert_eq!(
        value["args"],
        format!(
            "--config-file={}/session.conf\n--nofork\n--print-address\n7",
            cache.display()
        )
    );
}

#[test]
fn 无系统键盘工具明确报错且缺失数据使用前缀参数() {
    let (root, program, cmd) = local();
    let functions = include_str!("environment.sh")
        .replace("/usr/bin/xkbcomp", "/不存在的系统/xkbcomp")
        .replace("/usr/share/X11/xkb", "/不存在的系统/xkb");
    let e = desktop::run(
        &program,
        &cmd,
        &format!("{functions}\nkeyboard_environment"),
        10,
    )
    .unwrap_err();
    assert_eq!(e.code, "vnc.xkb_unavailable");
    let prefix = root.path().join("prefix");
    fs::create_dir_all(prefix.join("usr/share/X11/xkb/rules")).unwrap();
    fs::write(prefix.join("usr/share/X11/xkb/rules/evdev"), "").unwrap();
    let functions = include_str!("environment.sh")
        .replace(
            "/usr/share/X11/xkb/rules/evdev ]",
            "/不存在的系统/xkb/rules/evdev ]",
        )
        .replace("[ ! -x /usr/bin/xkbcomp ]", "false");
    let body = format!(
        "{functions}\nprefix_environment {}\nkeyboard_environment\nprintf '{{\"args\":'; json \"$xkb_args\"; printf '}}'",
        crate::ssh::quote::sh_quote(prefix.to_str().unwrap()).unwrap()
    );
    let value = desktop::run(&program, &cmd, &body, 10).unwrap();
    assert!(value["args"].as_str().unwrap().starts_with("-xkbdir '"));
    assert!(
        value["args"]
            .as_str()
            .unwrap()
            .contains(prefix.to_str().unwrap())
    );
}

#[test]
fn 仅无可用会话且_apt_齐全时报告轻量桌面能力() {
    for (apt, pm, desktops, expected) in [
        (true, "Apt", json!([]), true),
        (false, "Apt", json!([]), false),
        (true, "Dnf", json!([]), false),
        (
            true,
            "Apt",
            json!([{"id":"openbox","executable":true,"content":"[Desktop Entry]\nName=Openbox\nExec=openbox"}]),
            false,
        ),
    ] {
        let d = detection::parse_detection(json!({"arch":"x86_64","glibc":"2.35","os_release":"","package_manager":pm,"apt_available":apt,"binary_path":"","binary_version":"","libraries":"","tools":Tools::default(),"kill_user_processes":"","linger":"","sessions":[],"desktops":desktops})).unwrap();
        assert_eq!(d.light_desktop_available, expected);
    }
    let script = desktop::create_script(&CreateDesktop {
        desktop_id: LIGHT_DESKTOP_ID.into(),
        width: 800,
        height: 600,
        xvnc: XvncBinary {
            path: "/usr/bin/Xtigervnc".into(),
            version: (1, 12, 0),
            flavor: Flavor::TigerVnc,
        },
    })
    .unwrap();
    assert!(script.contains("desktop_id='pierlink-light'"));
    assert!(script.contains("desktop_names=OPENBOX"));
    assert!(script.contains("--dbus-daemon="));
    assert!(!script.contains("entry=$xsessions/$desktop_id.desktop"));
}

#[test]
fn 候选依赖只选首个可用项且复用已安装或已在闭包中的项() {
    for (first, installed, roots, expected) in [
        (true, false, "root", "root\na\na-child"),
        (false, false, "root", "root\nb\nb-child"),
        (true, true, "root", "root"),
        (true, false, "root b", "root\nb\nb-child"),
    ] {
        let (root, program, mut cmd) = local();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        script_file(
            &bin.join("dpkg-query"),
            if installed {
                "#!/bin/sh\nfor arg do p=$arg; done\n[ \"$p\" = b ] && printf 'install ok installed'\n"
            } else {
                "#!/bin/sh\nexit 1\n"
            },
        );
        cmd.env
            .push(("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())));
        let first_name = if first { "a" } else { "<a>" };
        let graph = format!(
            "root\n |Depends: {first_name}\n  Depends: b\na\n  Depends: a-child\na-child\nb\n  Depends: b-child\nb-child\n"
        );
        fs::write(root.path().join("depends"), graph).unwrap();
        let script = format!(
            "{}\nwork=$HOME\npackages=$(dependency_closure {roots}) || fail download_failed\nprintf '{{\"packages\":'; json \"$packages\"; printf '}}'",
            include_str!("apt.sh")
        );
        let result = desktop::run(&program, &cmd, &script, 10).unwrap();
        assert_eq!(result["packages"], expected);
    }
}

#[test]
fn 终端资源在会话前生成且拒绝链接路径() {
    let (root, program, mut cmd) = local();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for name in ["xterm", "openbox"] {
        script_file(
            &bin.join(name),
            &format!(
                "#!/bin/sh\n[ -r \"$XENVIRONMENT\" ] || exit 1\nprintf '%s' \"$XENVIRONMENT\" > \"$HOME/{name}-environment\"\n"
            ),
        );
    }
    script_file(
        &bin.join("openbox"),
        r#"#!/bin/sh
n=0
while [ ! -s "$HOME/xterm-environment" ] && [ "$n" -lt 100 ]; do sleep .01; n=$((n+1)); done
printf '%s' "$XENVIRONMENT" > "$HOME/openbox-environment"
"#,
    );
    cmd.env
        .push(("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())));
    let cache = root.path().join("会话 ' 空格");
    fs::create_dir(&cache).unwrap();
    script_file(
        &root.path().join("session"),
        include_str!("light_session.sh"),
    );
    cmd.env
        .push(("XDG_CACHE_HOME".into(), cache.to_str().unwrap().into()));
    let body = r#"light_xresources
saved=$(session_environment)
unset XENVIRONMENT
eval "$saved"
"$HOME/session"
printf '{"path":'; json "$XENVIRONMENT"; printf '}'"#;
    let result = desktop::run(&program, &cmd, body, 10).unwrap();
    let resource = cache.join("Xresources");
    assert_eq!(result["path"], resource.to_str().unwrap());
    // 首个终端是异步启动的，等待它完成写入后再核对继承环境。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !root.path().join("xterm-environment").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    for name in ["xterm", "openbox"] {
        assert_eq!(
            fs::read_to_string(root.path().join(format!("{name}-environment"))).unwrap(),
            resource.to_str().unwrap()
        );
    }
    assert_eq!(
        fs::metadata(&resource).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let text = fs::read_to_string(&resource).unwrap();
    assert!(text.contains("XTerm*selectToClipboard: true"));
    assert!(text.contains("Ctrl Shift <Key>C: copy-selection(CLIPBOARD)"));
    assert!(text.find("Ctrl Shift <Key>V").unwrap() < text.find("Ctrl ~Shift <Key>v").unwrap());
    assert!(!text.contains("<Key>c:"));
    let outside = root.path().join("outside");
    fs::write(&outside, "保持原文").unwrap();
    // 保留已有资源文件，以另一缓存目录检查符号链接拒绝。
    let linked = root.path().join("linked");
    fs::create_dir(&linked).unwrap();
    std::os::unix::fs::symlink(&outside, linked.join("Xresources")).unwrap();
    cmd.env.retain(|(key, _)| key != "XDG_CACHE_HOME");
    cmd.env
        .push(("XDG_CACHE_HOME".into(), linked.to_str().unwrap().into()));
    assert_eq!(
        desktop::run(&program, &cmd, "light_xresources", 10)
            .unwrap_err()
            .code,
        "vnc.unsafe_path"
    );
    assert_eq!(fs::read_to_string(outside).unwrap(), "保持原文");
}
