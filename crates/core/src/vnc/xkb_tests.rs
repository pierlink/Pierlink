use super::{desktop, desktop_tests::*, detection};
use crate::ssh::quote::sh_quote;
use std::{fs, os::unix::fs::PermissionsExt};

const INSTALL: &str = "sudo apt-get install -y x11-xkb-utils";

#[test]
fn 系统服务端可复用已获取前缀并补充键盘数据() {
    let (root, program, cmd) = local();
    let prefix = root.path().join(".local/share/pierlink/vnc/apt-test");
    fs::create_dir_all(prefix.join("usr/bin")).unwrap();
    fs::create_dir_all(prefix.join("usr/share/X11/xkb/rules")).unwrap();
    fs::write(prefix.join(".pierlink-created"), "true").unwrap();
    fs::write(prefix.join("usr/share/X11/xkb/rules/evdev"), "").unwrap();
    script_file(&prefix.join("usr/bin/xkbcomp"), "#!/bin/sh\nexit 0\n");
    let functions = include_str!("environment.sh")
        .replace("[ ! -x /usr/bin/xkbcomp ]", "true")
        .replace("[ ! -r /usr/share/X11/xkb/rules/evdev ]", "true")
        .replace("[ -r /usr/share/X11/xkb/rules/evdev ]", "false");
    let body = format!(
        "{functions}\nxkb_namespace_tools() {{ xkb_unshare=/bin/true; }}\nkeyboard_environment\nprintf '{{\"compiler\":'; json \"$xkb_prefix_compiler\"; printf ',\"args\":'; json \"$xkb_args\"; printf '}}'"
    );
    let result = desktop::run(&program, &cmd, &body, 10).unwrap();
    assert_eq!(
        result["compiler"],
        prefix.join("usr/bin/xkbcomp").to_str().unwrap()
    );
    assert_eq!(
        result["args"],
        format!(
            "-xkbdir {}",
            sh_quote(prefix.join("usr/share/X11/xkb").to_str().unwrap()).unwrap()
        )
    );
}

#[test]
fn 私有目录创建失败和挂载工具缺失有明确原因() {
    let (root, program, cmd) = local();
    let body = format!(
        "{}\nxkb_namespace_tools || fail xkb_unavailable \"$xkb_reason\"",
        include_str!("xkb.sh")
            .replace("command -v unshare", "command -v true")
            .replace("command -v mount", "command -v 不存在的挂载工具")
    );
    let error = desktop::run(&program, &cmd, &body, 10).unwrap_err();
    assert!(error.params["reason"].contains("mount"));
    assert_eq!(error.params["install_command"], INSTALL);
    let existing = root.path().join("已有目录");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("保留"), "保留").unwrap();
    let body = format!(
        "xkb_namespace_prepare {} || fail xkb_unavailable \"$xkb_reason\"",
        sh_quote(existing.to_str().unwrap()).unwrap()
    );
    let error = desktop::run(&program, &cmd, &body, 10).unwrap_err();
    assert_eq!(error.code, "vnc.xkb_unavailable");
    assert!(error.params["reason"].contains("私有目录"));
    assert!(existing.join("保留").exists());
}

#[test]
fn 系统键盘工具优先且前缀回退需要命名空间工具() {
    for system in [false, true] {
        for prefix_present in [false, true] {
            for unshare in [false, true] {
                let (root, program, mut cmd) = local();
                let bin = root.path().join("usr/bin");
                fs::create_dir_all(&bin).unwrap();
                if prefix_present {
                    script_file(&bin.join("xkbcomp"), "#!/bin/sh\nexit 0\n");
                }
                if unshare {
                    script_file(&bin.join("test-unshare"), "#!/bin/sh\nexit 0\n");
                }
                cmd.env
                    .push(("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())));
                let environment = include_str!("environment.sh")
                    .replace(
                        "[ ! -x /usr/bin/xkbcomp ]",
                        if system { "false" } else { "true" },
                    )
                    .replace("[ -r /usr/share/X11/xkb/rules/evdev ]", "true");
                let helpers =
                    include_str!("xkb.sh").replace("command -v unshare", "command -v test-unshare");
                let body = format!(
                    "{environment}\n{helpers}\nprefix_environment {}\nkeyboard_environment\nprintf '{{\"namespace\":%s}}' \"$xkb_namespace\"",
                    sh_quote(root.path().to_str().unwrap()).unwrap()
                );
                let result = desktop::run(&program, &cmd, &body, 10);
                if system || (prefix_present && unshare) {
                    assert_eq!(result.unwrap()["namespace"], !system);
                } else {
                    let error = result.unwrap_err();
                    assert_eq!(error.code, "vnc.xkb_unavailable");
                    assert_eq!(error.params["install_command"], INSTALL);
                    assert!(error.params["reason"].contains(if prefix_present {
                        "unshare"
                    } else {
                        "前缀"
                    }));
                }
            }
        }
    }
}

pub(super) fn mock_tools(bin: &std::path::Path) {
    fs::create_dir_all(bin).unwrap();
    script_file(
        &bin.join("unshare"),
        r#"#!/usr/bin/python3
import os,sys
assert sys.argv[1:5] == ['-Ur','-m','--propagation','private']
if os.environ.get('MOCK_FAILURE') == 'namespace': sys.exit(1)
os.execv(sys.argv[5],sys.argv[5:])
"#,
    );
    script_file(
        &bin.join("mount"),
        r#"#!/usr/bin/python3
import json,os,sys,time
with open(os.environ['HOME']+'/mount-args','w') as f:
    json.dump({'args':sys.argv[1:],'cwd':os.getcwd()},f)
if os.environ.get('MOCK_FAILURE') == 'mount': sys.exit(1)
if os.environ.get('MOCK_FAILURE') == 'timeout': time.sleep(60)
"#,
    );
}

#[test]
fn 命名空间命令保留进程号且路径不会进入挂载选项() {
    let (root, program, mut cmd) = local();
    let bin = root.path().join("bin");
    mock_tools(&bin);
    cmd.env
        .push(("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())));
    let dir = root
        .path()
        .join("私有 ' \" 空格,冒号:反斜杠\\$(false)\n目录");
    let compiler = root.path().join("键盘 ' ,\\$(false)");
    script_file(&compiler, "#!/bin/sh\nexit 0\n");
    let argument = "参数 ' \" , : \\ $(false)\n结尾";
    let body = format!(
        r#"xkb_prefix_compiler={compiler}
xkb_namespace_tools
xkb_namespace_prepare {dir}
(xkb_namespace_exec {dir} start /usr/bin/python3 -c {python} {argument}) &
pid=$!; printf '%s' "$pid" > "$HOME/pid"
wait "$pid""#,
        compiler = sh_quote(compiler.to_str().unwrap()).unwrap(),
        dir = sh_quote(dir.to_str().unwrap()).unwrap(),
        python = sh_quote(
            "import os,json,sys; print(json.dumps({'pid':os.getpid(),'arg':sys.argv[1]}))"
        )
        .unwrap(),
        argument = sh_quote(argument).unwrap(),
    );
    let result = desktop::run(&program, &cmd, &body, 10).unwrap();
    assert_eq!(
        result["pid"].to_string(),
        fs::read_to_string(root.path().join("pid")).unwrap()
    );
    assert_eq!(result["arg"], argument);
    let mount: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("mount-args")).unwrap()).unwrap();
    assert_eq!(mount["cwd"], dir.to_str().unwrap());
    assert_eq!(
        mount["args"],
        serde_json::json!([
            "-t",
            "overlay",
            "overlay",
            "-o",
            "lowerdir=/usr/bin,upperdir=upper,workdir=work",
            "/usr/bin"
        ])
    );
    assert_eq!(fs::read_link(dir.join("upper/xkbcomp")).unwrap(), compiler);
    for path in [&dir, &dir.join("upper"), &dir.join("work")] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    desktop::run(
        &program,
        &cmd,
        &format!(
            "xkb_namespace_remove {}\nprintf '{{}}'",
            sh_quote(dir.to_str().unwrap()).unwrap()
        ),
        10,
    )
    .unwrap();
    assert!(!dir.exists());
}

#[test]
fn 检测报告挂载能力和失败原因并清理探测目录() {
    for (failure, reason) in [
        ("", None),
        ("namespace", Some("AppArmor")),
        ("mount", Some("overlay")),
        ("timeout", Some("overlay")),
    ] {
        let (root, program, mut cmd) = local();
        let bin = root.path().join("bin");
        mock_tools(&bin);
        cmd.env.extend([
            ("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())),
            ("MOCK_FAILURE".into(), failure.into()),
        ]);
        let script = detection::script().replace(
            "for proc in \"$proc_root\"/[0-9]*; do",
            "for proc in /不存在的测试进程; do",
        );
        let result =
            detection::parse_detection(desktop::run(&program, &cmd, &script, 20).unwrap()).unwrap();
        let check = result.xkb_namespace.unwrap();
        assert_eq!(check.available, failure.is_empty());
        assert_eq!(check.install_command, INSTALL);
        match reason {
            Some(reason) => assert!(check.reason.unwrap().contains(reason)),
            None => assert!(check.reason.is_none()),
        }
        assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".xkb-probe.")
        }));
        assert!(!root.path().join(".local").exists());
    }
}
