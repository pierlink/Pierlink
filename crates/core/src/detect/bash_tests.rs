use super::*;

fn shell(args: Vec<String>) -> LocalShell {
    LocalShell {
        id: "bash".into(),
        name: "Bash".into(),
        program: "/bin/bash".into(),
        args,
        env: vec![],
        kind: ShellKind::Other,
    }
}
#[test]
fn 固定脚本原子替换复用及写入错误() {
    let root = crate::test_support::tempdir();
    let mut first = shell(vec!["--login".into(), "-i".into()]);
    prepare(&mut first, root.path()).unwrap();
    assert_eq!(first.args[0], "--rcfile");
    assert!(!first.args.iter().any(|s| s == "--login"));
    let path = Path::new(&first.args[1]);
    let before = fs::metadata(path).unwrap().modified().unwrap();
    let mut second = shell(vec!["--login".into(), "-i".into()]);
    prepare(&mut second, root.path()).unwrap();
    assert_eq!(first.args, second.args);
    assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), before);
    fs::write(path, "旧版本").unwrap();
    prepare(&mut shell(vec!["--login".into()]), root.path()).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), script(true, false));
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    let mut git = shell(vec!["--login".into()]);
    git.kind = ShellKind::GitBash;
    prepare(&mut git, root.path()).unwrap();
    assert!(
        fs::read_to_string(&git.args[1])
            .unwrap()
            .contains("cygpath -w")
    );
    let blocked = root.path().join("不是目录");
    fs::write(&blocked, "").unwrap();
    assert_eq!(
        prepare(&mut shell(vec![]), &blocked).unwrap_err().code,
        "detect.shell_config_failed"
    );
    // 包装脚本写不进去：shell 列表照常返回，Bash 保持原有参数（不带 --rcfile）
    let original = shell(vec!["--login".into(), "-i".into()]);
    let mut kept = original.clone();
    assert!(prepare(&mut kept, &blocked).is_err());
    assert_eq!(kept, original);
    for s in super::super::detect_local_shells(&blocked) {
        assert!(!s.args.iter().any(|a| a == "--rcfile"));
    }
}

#[cfg(unix)]
#[test]
fn bash真实用户rc别名及双钩子() {
    use crate::terminal::{TerminalEvent, TerminalKind, TerminalManager, TerminalSpec};
    use std::{
        sync::{Arc, mpsc},
        time::{Duration, Instant},
    };
    let root = crate::test_support::tempdir();
    let home = root.path().join("用户 空格 '$() 目录");
    fs::create_dir(&home).unwrap();
    fs::write(
        home.join(".bashrc"),
        "PROMPT_COMMAND='printf USER-HOOK'\nalias pierlink_alias='printf ALIAS-OK'\n",
    )
    .unwrap();
    let mut shell = shell(vec![]);
    prepare(&mut shell, &root.path().join("包装 空格 '$() 目录")).unwrap();
    let manager = TerminalManager::new();
    let (tx, rx) = mpsc::channel();
    let id = manager
        .open(
            TerminalSpec {
                kind: TerminalKind::Local {
                    program: shell.program,
                    args: shell.args,
                    cwd: Some(home.clone()),
                    env: vec![("HOME".into(), home.to_str().unwrap().into())],
                },
                cols: 80,
                rows: 24,
            },
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
    let expected = format!("USER-HOOK\x1b]9;9;\"{}\"\x1b\\", home.display());
    let start = Instant::now();
    let mut output = vec![];
    let mut sent = false;
    let found = loop {
        if start.elapsed() > Duration::from_secs(10) {
            break false;
        }
        if let Ok(TerminalEvent::Output(bytes)) = rx.recv_timeout(Duration::from_millis(100)) {
            let _ = manager.ack(&id, bytes.len());
            output.extend(bytes);
        }
        if !sent
            && output
                .windows(expected.len())
                .any(|w| w == expected.as_bytes())
        {
            manager.write(&id, b"pierlink_alias\r").unwrap();
            sent = true;
        }
        if sent && output.windows(8).any(|w| w == b"ALIAS-OK") {
            break true;
        }
    };
    manager.close(&id).unwrap();
    assert!(
        found,
        "用户配置及钩子未全部生效：{:?}",
        String::from_utf8_lossy(&output)
    );
}

#[cfg(unix)]
#[test]
fn bash标量注释和数组钩子追加去重() {
    let root = crate::test_support::tempdir();
    let home = root.path().join("home");
    fs::create_dir(&home).unwrap();
    for original in [
        "PROMPT_COMMAND='printf USER-HOOK # 用户注释'",
        "PROMPT_COMMAND=('printf USER-HOOK' 'printf ARRAY-HOOK')",
    ] {
        fs::write(
            home.join(".bashrc"),
            format!(
                "if [[ -z ${{USER_RC_LOADED-}} ]]; then
{original}
USER_RC_LOADED=1
fi
"
            ),
        )
        .unwrap();
        let script = write_script(root.path(), "bash-cwd.rc", &script(false, false)).unwrap();
        let output = std::process::Command::new("/bin/bash")
            .args([
                "--rcfile",
                script.to_str().unwrap(),
                "-ic",
                ". \"$1\"; declare -p PROMPT_COMMAND; for cmd in \"${PROMPT_COMMAND[@]}\"; do eval \"$cmd\"; done",
                "bash",
                script.to_str().unwrap(),
            ])
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("USER-HOOK"));
        assert!(text.contains("\x1b]9;9;"), "{text}");
        // declare 只出现一个追加项；第二次 source 不会把目录钩子叠加两遍。
        assert_eq!(text.matches("__pierlink_report_cwd").count(), 1);
    }
}

#[cfg(unix)]
#[test]
fn bash登录顺序及首个用户profile() {
    let root = crate::test_support::tempdir();
    let home = root.path().join("home");
    fs::create_dir(&home).unwrap();
    fs::write(home.join(".bashrc"), "printf USER-RC\nPROMPT_COMMAND='printf USER-HOOK'\nalias pierlink_alias='printf ALIAS-OK'\n").unwrap();
    let script = write_script(root.path(), "bash-login-cwd.rc", &script(true, false)).unwrap();
    for (index, profile) in [".bash_profile", ".bash_login", ".profile"]
        .iter()
        .enumerate()
    {
        fs::write(
            home.join(profile),
            format!("printf PROFILE-{index}\\n\n. ~/.bashrc\n"),
        )
        .unwrap();
    }
    for (index, profile) in [".bash_profile", ".bash_login", ".profile"]
        .iter()
        .enumerate()
    {
        let output = std::process::Command::new("/bin/bash")
            .args([
                "--rcfile",
                script.to_str().unwrap(),
                "-ic",
                "pierlink_alias; eval \"$PROMPT_COMMAND\"",
            ])
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains(&format!("PROFILE-{index}")), "{text}");
        assert_eq!(text.matches("PROFILE-").count(), 1);
        assert!(
            text.contains("USER-RC") && text.contains("ALIAS-OK") && text.contains("USER-HOOK")
        );
        assert!(text.contains("\x1b]9;9;"));
        fs::remove_file(home.join(profile)).unwrap();
    }
}

#[test]
fn wsl启动器不加包装脚本() {
    let root = crate::test_support::tempdir();
    for program in [
        r"C:\Windows\System32\bash.exe",
        "c:/windows/system32/bash.exe",
    ] {
        let mut s = shell(vec![]);
        s.program = program.into();
        prepare(&mut s, root.path()).unwrap();
        assert!(s.args.is_empty(), "{program}");
    }
    assert!(!super::is_wsl_launcher(std::path::Path::new(
        r"C:\Program Files\Git\bin\bash.exe"
    )));
}

#[cfg(unix)]
#[test]
fn 仅gitbash且环境显式启用时安装预测钩子() {
    for git in [false, true] {
        for predict in [None, Some("0"), Some("1"), Some("invalid")] {
            let root = crate::test_support::tempdir();
            fs::write(root.path().join(".bashrc"), "PS1='LOCAL> '\n").unwrap();
            let script = write_script(root.path(), "test.rc", &script(false, git)).unwrap();
            let mut command = std::process::Command::new("/bin/bash");
            command.args([
                    "--rcfile",
                    script.to_str().unwrap(),
                    "-ic",
                    "declare -F __pl_pre; declare -p PROMPT_COMMAND; printf 'PREDICT=%s\\n' \"${PIERLINK_PREDICT-unset}\"; /bin/bash --norc -c 'printf \"CHILD-PREDICT=%s\\n\" \"${PIERLINK_PREDICT-unset}\"'",
                ])
                .env("HOME", root.path())
                .env_remove("PIERLINK_PREDICT")
                .env("HISTFILE", "/dev/null");
            if let Some(predict) = predict {
                command.env("PIERLINK_PREDICT", predict);
            }
            let output = command.output().unwrap();
            assert!(output.status.success());
            let text = String::from_utf8_lossy(&output.stdout);
            let enabled = git && predict == Some("1");
            assert_eq!(text.contains("\x1b]7701;i;native=0\x07"), enabled);
            assert_eq!(text.contains("__pl_pre"), enabled);
            assert!(text.contains("__pierlink_report_cwd"));
            let expected = if git {
                "unset"
            } else {
                predict.unwrap_or("unset")
            };
            assert!(
                text.contains(&format!("\nPREDICT={expected}\n")),
                "{text:?}"
            );
            assert!(
                text.contains(&format!("\nCHILD-PREDICT={expected}\n")),
                "{text:?}"
            );
        }
    }
}
