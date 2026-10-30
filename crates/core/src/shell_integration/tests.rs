use super::*;
use crate::{
    ssh::args::terminal_command,
    terminal::{TerminalEvent, TerminalKind, TerminalManager, TerminalSpec},
};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

const A: &str = "\x1b]133;A\x07";
const B: &str = "\x1b]133;B\x07";

struct Session {
    manager: TerminalManager,
    id: String,
    rx: mpsc::Receiver<TerminalEvent>,
}
impl Session {
    fn new(program: &str, args: Vec<String>, home: &Path, env: Vec<(String, String)>) -> Self {
        let mut vars = vec![
            ("HOME".into(), home.to_str().unwrap().into()),
            ("TMPDIR".into(), home.join("tmp").to_str().unwrap().into()),
            ("SHELL".into(), "/bin/bash".into()),
            ("HISTFILE".into(), "/dev/null".into()),
            ("TERM".into(), "xterm-256color".into()),
            // Debian/Ubuntu 的 /etc/zsh/zshrc 会跑 compinit，补全目录权限不对时停下来问 y/n（CI 机器上就是这样）
            ("skip_global_compinit".into(), "1".into()),
        ];
        vars.extend(env);
        Self::from_kind(TerminalKind::Local {
            program: program.into(),
            args,
            cwd: Some(home.into()),
            env: vars,
        })
    }
    fn from_kind(kind: TerminalKind) -> Self {
        let manager = TerminalManager::new();
        let (tx, rx) = mpsc::channel();
        let id = manager
            .open(
                TerminalSpec {
                    kind,
                    cols: 160,
                    rows: 24,
                },
                Arc::new(move |event| {
                    let _ = tx.send(event);
                }),
            )
            .unwrap();
        Self { manager, id, rx }
    }
    fn remote(home: &Path, env: Vec<(String, String)>) -> Self {
        Self::new(
            "/bin/dash",
            vec!["-c".into(), command(None).unwrap()],
            home,
            env,
        )
    }
    fn read(&self, needle: &str) -> String {
        let start = Instant::now();
        let mut output = Vec::new();
        loop {
            if let Ok(event) = self.rx.recv_timeout(Duration::from_millis(100)) {
                match event {
                    TerminalEvent::Output(bytes) => {
                        let _ = self.manager.ack(&self.id, bytes.len());
                        output.extend(bytes);
                    }
                    TerminalEvent::Exited { .. } => {
                        panic!("终端提前退出：{:?}", String::from_utf8_lossy(&output))
                    }
                }
            }
            let text = String::from_utf8_lossy(&output);
            if text.contains(needle) {
                return text.into_owned();
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "未收到 {needle:?}：{text:?}"
            );
        }
    }
    fn run(&self, line: &str) -> String {
        self.manager
            .write(&self.id, format!("{line}\r").as_bytes())
            .unwrap();
        self.read(B)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.manager.close(&self.id);
    }
}

fn home(rc: &str) -> tempfile::TempDir {
    let root = crate::test_support::tempdir();
    fs::create_dir(root.path().join("tmp")).unwrap();
    fs::write(
        root.path().join(".bash_profile"),
        "A=${A:-0}; A=$((A+1)); . ~/.bashrc\n",
    )
    .unwrap();
    fs::write(
        root.path().join(".bashrc"),
        format!("B=${{B:-0}}; B=$((B+1)); PS1='READY> '; HISTCONTROL=ignorespace\n{rc}\n"),
    )
    .unwrap();
    root
}

fn record(text: &str, code: u8, expected: &str) {
    let prefix = format!("\x1b]{PRIVATE_OSC};c;{code};");
    let encoded = text
        .split_once(&prefix)
        .unwrap_or_else(|| panic!("缺少命令记录：{text:?}"))
        .1
        .split('\x07')
        .next()
        .unwrap();
    assert_eq!(Base64::decode_vec(encoded).unwrap(), expected.as_bytes());
    assert!(text.find(&format!("\x1b]133;D;{code}\x07")).unwrap() < text.find(&prefix).unwrap());
    assert!(text.find(&prefix).unwrap() < text.find("\x1b]7;").unwrap());
    assert!(text.find("\x1b]7;").unwrap() < text.find(A).unwrap());
}

#[test]
fn 关闭逐字节兼容且启用命令仅含安全字符() {
    assert_eq!(terminal_command(&None, false).unwrap(), None);
    assert_eq!(
        terminal_command(&Some("/opt/bin/bash".into()), false).unwrap(),
        Some("exec '/opt/bin/bash' -l".into())
    );
    for shell in [None, Some("/opt/bin/bash".into()), Some("zsh".into())] {
        let text = terminal_command(&shell, true).unwrap().unwrap();
        assert_eq!(text.bytes().filter(|c| *c == b'\'').count(), 2);
        assert!(
            text.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"+/= $(){}\".;&|>:-_'".contains(&c)),
            "{text}"
        );
        let body = text
            .strip_prefix("exec /bin/sh -c '")
            .unwrap()
            .strip_suffix('\'')
            .unwrap();
        let encoded = body
            .split_once("printf ")
            .unwrap()
            .1
            .split_whitespace()
            .next()
            .unwrap();
        assert_eq!(
            Base64::decode_vec(encoded).unwrap(),
            loader(shell.as_deref()).unwrap().as_bytes()
        );
        if let Some(shell) = shell {
            assert!(text.ends_with(&format!("exec {shell} -l'")));
        }
    }
    for shell in [
        "", "-bash", "/a b", "bash!", "$(id)", "a'b", "bash\n", "bash\0",
    ] {
        for enabled in [false, true] {
            assert_eq!(
                terminal_command(&Some(shell.into()), enabled)
                    .unwrap_err()
                    .code,
                "ssh.invalid_argument"
            );
        }
    }
}

fn syntax(program: &str, args: &[&str], source: &str) {
    use std::io::Write;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{program}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 测试用的 zsh：环境变量 PIERLINK_TEST_ZSH 指定的程序，没有时用系统的 /usr/bin/zsh；都没有时返回 None。
fn zsh() -> Option<String> {
    std::env::var("PIERLINK_TEST_ZSH")
        .ok()
        .filter(|path| !path.is_empty())
        .or_else(|| Some("/usr/bin/zsh".into()))
        .filter(|path| Path::new(path).exists())
}

#[test]
fn 各启动片段通过对应shell语法检查() {
    for shell in [None, Some("/bin/bash"), Some("/bin/zsh")] {
        let script = loader(shell).unwrap();
        syntax("/bin/bash", &["-n"], &script);
        syntax("/bin/dash", &["-n"], &script);
        if Path::new("/usr/bin/busybox").exists() {
            syntax("/usr/bin/busybox", &["sh", "-n"], &script);
        }
    }
    syntax("/bin/bash", &["-n"], &bash_rc());
    if let Some(zsh) = zsh() {
        for name in [".zshenv", ".zprofile", ".zshrc", ".zlogin"] {
            syntax(&zsh, &["-n"], &zsh_rc(name));
        }
    } else {
        eprintln!("未安装 zsh，跳过 zsh 语法检查");
    }
}

#[test]
fn 真实bash加载配置一次并报告原文退出码目录及清理() {
    let root = home("set -u; set -o vi");
    let session = Session::remote(root.path(), vec![]);
    let first = session.read(B);
    assert!(first.contains(A) && first.contains("\x1b]7701;i;native=0\x07"));
    assert!(!first.contains("\x1b]133;D;") && !first.contains("\x1b]7701;c;"));
    assert_eq!(fs::read_dir(root.path().join("tmp")).unwrap().count(), 0);
    let text = session.run("printf 'LOAD=%s,%s\\n' \"$A\" \"$B\"");
    assert!(text.contains("LOAD=1,1"), "{text:?}");
    record(&session.run("cd /tmp; true"), 0, "cd /tmp; true");
    let text = session.run("false");
    record(&text, 1, "false");
    assert!(text.contains("/tmp\x07"));
    for line in [" true", "", " false"] {
        let text = session.run(line);
        assert!(
            !text.contains("\x1b]7701;c;") && !text.contains("\x1b]133;D;"),
            "{text:?}"
        );
    }
    record(&session.run("(exit 130)"), 130, "(exit 130)");
    record(&session.run("(exit 143)"), 143, "(exit 143)");
    session.run("set +o history");
    assert!(!session.run("echo ignored").contains("\x1b]7701;c;"));
}

#[test]
fn 字符串数组及重设提示符保留用户钩子和退出码() {
    for hook in [
        "PROMPT_COMMAND='printf USER-HOOK # 注释'",
        "PROMPT_COMMAND=('printf USER-HOOK' 'printf ARRAY-HOOK')",
        "PROMPT_COMMAND=('printf USER-HOOK'); export PROMPT_COMMAND",
        "PROMPT_COMMAND='PS1=STARSHIP; printf USER-HOOK'",
    ] {
        let root = home(hook);
        let session = Session::remote(root.path(), vec![]);
        assert!(session.read(B).contains("USER-HOOK"));
        let text = session.run("false");
        assert_eq!(text.matches(A).count(), 1);
        assert_eq!(text.matches(B).count(), 1);
        assert!(text.contains("USER-HOOK"));
        record(&text, 1, "false");
    }
}

#[test]
fn bash导出的提示符钩子不传入交互子shell() {
    for hook in [
        "export PROMPT_COMMAND=\"history -a${PROMPT_COMMAND:+; $PROMPT_COMMAND}\"",
        "PROMPT_COMMAND=('history -a'); export PROMPT_COMMAND",
    ] {
        let root = home(hook);
        let session = Session::remote(root.path(), vec![("LC_ALL".into(), "C".into())]);
        session.read(B);
        let declaration = session.run("declare -p PROMPT_COMMAND");
        assert!(
            declaration.contains("declare -- PROMPT_COMMAND=")
                || declaration.contains("declare -a PROMPT_COMMAND="),
            "{declaration:?}"
        );
        let text = session
            .run("/bin/bash --norc -i -c 'printf \"INHERITED=%s\\n\" \"${PROMPT_COMMAND-unset}\"'");
        assert!(text.contains("INHERITED=unset\r\n"), "{text:?}");
        assert!(!text.contains("command not found"), "{text:?}");
        session
            .manager
            .write(&session.id, b"/bin/bash -i\r")
            .unwrap();
        let first = session.read("READY> ");
        assert!(!first.contains("command not found"), "{first:?}");
        session.manager.write(&session.id, b"false\r").unwrap();
        let next = session.read("READY> ");
        assert!(!next.contains("command not found"), "{next:?}");
        session.run("exit");
        record(&session.run("false"), 1, "false");
    }
}

#[test]
fn bash提示符展开保留用户命令退出码() {
    for hook in ["PROMPT_COMMAND='true'", "PROMPT_COMMAND=('true' 'true')"] {
        let root = home(&format!(
            "PS1='STATUS=$? SUB=$(printf \"%s\" \"$?\")> '\n{hook}"
        ));
        let session = Session::remote(root.path(), vec![]);
        session.read(B);
        for (line, code) in [("false", 1), ("true", 0), ("(exit 42)", 42)] {
            let text = session.run(line);
            record(&text, code, line);
            assert!(
                text.contains(&format!("STATUS={code} SUB={code}> ")),
                "{text:?}"
            );
        }
    }
}

#[test]
fn 缺工具只读临时目录及配置exec均回退并清理() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for failure in [
        "base64",
        "mktemp",
        "cat",
        "只读",
        "写入",
        "exec",
        "其他shell",
    ] {
        let root = home(if failure == "exec" {
            "exec /bin/bash --norc -i"
        } else {
            ""
        });
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for tool in ["mktemp", "base64", "cat", "rm", "rmdir"] {
            if tool != failure {
                symlink(format!("/usr/bin/{tool}"), bin.join(tool)).unwrap();
            }
        }
        if failure == "写入" {
            fs::remove_file(bin.join("cat")).unwrap();
            fs::write(bin.join("cat"), "#!/bin/dash\nprintf partial\nexit 1\n").unwrap();
            fs::set_permissions(bin.join("cat"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut env = vec![("PATH".into(), bin.to_str().unwrap().into())];
        if failure == "只读" {
            fs::set_permissions(root.path().join("tmp"), fs::Permissions::from_mode(0o500))
                .unwrap();
        }
        if failure == "其他shell" {
            env.push(("SHELL".into(), "/bin/dash".into()));
        }
        let session = Session::remote(root.path(), env);
        session
            .manager
            .write(&session.id, b"printf 'FALLBACK-%s\\n' OK\r")
            .unwrap();
        let text = session.read("FALLBACK-OK");
        assert!(
            !text.contains(A) && !text.contains(B),
            "{failure}: {text:?}"
        );
        drop(session);
        fs::set_permissions(root.path().join("tmp"), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            fs::read_dir(root.path().join("tmp")).unwrap().count(),
            0,
            "{failure}"
        );
    }
}

#[test]
fn 超长控制字符与payload上限及中文目录编码() {
    let root = home("BLE_VERSION=1");
    let directory = root.path().join("中文 空格%#");
    fs::create_dir(&directory).unwrap();
    let session = Session::remote(root.path(), vec![]);
    assert!(session.read(B).contains("\x1b]7701;i;native=1\x07"));
    let line = format!("cd {}", sh_quote(directory.to_str().unwrap()).unwrap());
    let text = session.run(&line);
    record(&text, 0, &line);
    assert!(
        text.contains("/%E4%B8%AD%E6%96%87%20%E7%A9%BA%E6%A0%BC%25%23\x07"),
        "{text:?}"
    );
    let limit = format!(": {}", "a".repeat(998));
    record(&session.run(&limit), 0, &limit);
    for line in [
        format!(": {}", "a".repeat(999)),
        format!(": {}", "😀".repeat(998)),
    ] {
        let text = session.run(&line);
        assert!(text.contains("\x1b]133;D;0\x07"));
        assert!(!text.contains("\x1b]7701;c;"));
    }
    // 直接调用协议函数，避免 readline 把控制字节当编辑快捷键。
    for command in ["a\tb", "a\nb", "a\x7fb", "a\x1bb", "a\u{85}b"] {
        let script = format!(
            "__pl_osc={PRIVATE_OSC}\n{COMMON}\n__pl_record 0 {}",
            sh_quote(command).unwrap()
        );
        let out = Command::new("/bin/bash")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn 包装后的交互shell仍能查询目录与忙碌状态() {
    use crate::{cwd::remote_shell_status, process::SshProgram, ssh::args::SshCommand};
    for shell in [
        "/bin/bash".to_owned(),
        zsh().unwrap_or_else(|| "/usr/bin/zsh".into()),
    ] {
        let shell = shell.as_str();
        if !Path::new(shell).exists() {
            eprintln!("未安装 {shell}，跳过目录与忙碌状态检查");
            continue;
        }
        let root = home("");
        let marker = uuid::Uuid::new_v4().to_string();
        let session = Session::remote(
            root.path(),
            vec![
                ("LC_PIERLINK_TERM".into(), marker.clone()),
                ("SHELL".into(), shell.into()),
            ],
        );
        session.read(B);
        let program = SshProgram {
            path: "/bin/sh".into(),
        };
        let cmd = SshCommand {
            args: vec!["-s".into()],
            env: vec![],
            config_text: None,
            builtin: None,
            askpass: None,
        };
        let status = remote_shell_status(&program, &cmd, &marker)
            .unwrap()
            .unwrap();
        assert_eq!(status.cwd, root.path().to_str().unwrap());
        assert!(!status.busy);
        session.manager.write(&session.id, b"sleep 30\r").unwrap();
        let start = Instant::now();
        while !remote_shell_status(&program, &cmd, &marker)
            .unwrap()
            .unwrap()
            .busy
        {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(20));
        }
        session.manager.write(&session.id, b"\x03").unwrap();
        session.read(B);
        assert!(
            !remote_shell_status(&program, &cmd, &marker)
                .unwrap()
                .unwrap()
                .busy
        );
    }
}

#[test]
fn zsh转发保留用户目录及命令原文() {
    let Some(zsh) = zsh() else {
        eprintln!("未安装 zsh，跳过真实 zsh 集成测试");
        return;
    };
    for configured in [false, true] {
        let root = home("");
        // zsh 读启动文件时会把 ZDOTDIR 的值原地解码，值里有 0x83–0x9f 的字节（如“配置 目录”）时之后再读就是乱码，
        // 不用包装也一样（zsh 自身的问题）；这里选 UTF-8 字节不落在这一段的汉字
        let zdotdir = root.path().join("中 设置");
        fs::create_dir(&zdotdir).unwrap();
        let userdir = if configured { &zdotdir } else { root.path() };
        for name in [".zshenv", ".zprofile", ".zshrc", ".zlogin"] {
            fs::write(
                userdir.join(name),
                format!(
                    "print -r -- 'LOADED-{name}'; print -r -- \"USER-DIR=${{ZDOTDIR-unset}}\"\n"
                ),
            )
            .unwrap();
        }
        let mut env = vec![("SHELL".into(), zsh.clone())];
        if configured {
            env.push(("ZDOTDIR".into(), zdotdir.to_str().unwrap().into()));
        }
        let session = Session::remote(root.path(), env);
        let first = session.read(B);
        for name in [".zshenv", ".zprofile", ".zshrc", ".zlogin"] {
            assert_eq!(
                first.matches(&format!("LOADED-{name}")).count(),
                1,
                "{first:?}"
            );
        }
        let expected = if configured {
            zdotdir.to_str().unwrap()
        } else {
            "unset"
        };
        assert_eq!(
            first.matches(&format!("USER-DIR={expected}\r\n")).count(),
            4,
            "{first:?}"
        );
        assert_eq!(fs::read_dir(root.path().join("tmp")).unwrap().count(), 0);
        record(
            &session.run("printf '%s' '中文 原文'; false"),
            1,
            "printf '%s' '中文 原文'; false",
        );
        session.run("setopt histignorespace");
        assert!(!session.run(" true").contains("\x1b]7701;c;"));
    }
}

#[test]
fn 普通zsh首个提示符报告初始化和目录且不重复初始化() {
    let Some(zsh) = zsh() else {
        eprintln!("未安装 zsh，跳过真实 zsh 集成测试");
        return;
    };
    for native in [false, true] {
        let root = home("");
        fs::write(
            root.path().join(".zshrc"),
            if native {
                "PS1='READY> '; _zsh_autosuggest_start() { :; }\n"
            } else {
                "PS1='READY> '\n"
            },
        )
        .unwrap();
        let session = Session::remote(root.path(), vec![("SHELL".into(), zsh.clone())]);
        let first = session.read(B);
        let init = format!("\x1b]7701;i;native={}\x07", u8::from(native));
        assert_eq!(first.matches(&init).count(), 1, "{first:?}");
        assert!(first.contains("\x1b]7;file://"), "{first:?}");
        assert_eq!(first.matches(A).count(), 1, "{first:?}");
        assert_eq!(first.matches(B).count(), 1, "{first:?}");
        assert!(!first.contains("\x1b]133;D;") && !first.contains("\x1b]7701;c;"));
        let text = session.run("false");
        record(&text, 1, "false");
        assert!(!text.contains("\x1b]7701;i;"), "{text:?}");
    }
}

#[test]
fn zsh即时提示符捕获为空且重建提示符仍有标记() {
    let Some(zsh) = zsh() else {
        eprintln!("未安装 zsh，跳过真实 zsh 集成测试");
        return;
    };
    let root = home("");
    fs::write(
        root.path().join(".zshrc"),
        r#"exec {saved_stdout}>&1 {saved_stderr}>&2
exec >"$HOME/captured" 2>&1
typeset -gi __p9k_instant_prompt_active=1
# 在用户配置加载时读取开关，模拟主题初始化。
theme_integration=${POWERLEVEL9K_TERM_SHELL_INTEGRATION-false}
precmd() {
    precmd_functions=(${precmd_functions:#theme_precmd} theme_precmd)
}
theme_precmd() {
    if (( ${+__p9k_instant_prompt_active} )); then
        exec 1>&$saved_stdout 2>&$saved_stderr
        exec {saved_stdout}>&- {saved_stderr}>&-
        unset __p9k_instant_prompt_active
        if [[ -s $HOME/captured ]]; then print -r -- CAPTURE-NOT-EMPTY; fi
    fi
    PROMPT='THEME> '
    if [[ $theme_integration == true ]]; then
        PROMPT=$'%{\e]133;A\a%}'$PROMPT$'%{\e]133;B\a%}'
    fi
}
precmd_functions=(theme_precmd)
"#,
    )
    .unwrap();
    let session = Session::remote(root.path(), vec![("SHELL".into(), zsh)]);
    let first = session.read(B);
    assert!(fs::read(root.path().join("captured")).unwrap().is_empty());
    assert!(!first.contains("CAPTURE-NOT-EMPTY"), "{first:?}");
    assert!(
        !first.contains("\x1b]7701;i;") && !first.contains("\x1b]7;"),
        "{first:?}"
    );
    assert!(!first.contains("\x1b]133;D;") && !first.contains("\x1b]7701;c;"));
    assert_eq!(first.matches(A).count(), 1, "{first:?}");
    let second = session.run("false");
    assert_eq!(
        second.matches("\x1b]7701;i;native=0\x07").count(),
        1,
        "{second:?}"
    );
    record(&second, 1, "false");
    for line in ["true", "printf hello"] {
        let text = session.run(line);
        record(&text, 0, line);
        assert_eq!(text.matches(A).count(), 1, "{text:?}");
        assert_eq!(text.matches(B).count(), 1, "{text:?}");
        assert!(!text.contains("\x1b]7701;i;"), "{text:?}");
    }
    let text = session.run(
        "/bin/sh -c 'printf \"EXPORTED=%s\\n\" \"${POWERLEVEL9K_TERM_SHELL_INTEGRATION-unset}\"'",
    );
    assert!(text.contains("EXPORTED=unset\r\n"), "{text:?}");
}

#[test]
fn zsh保留用户显式关闭主题集成的配置() {
    let Some(zsh) = zsh() else {
        eprintln!("未安装 zsh，跳过真实 zsh 集成测试");
        return;
    };
    for name in [".zshenv", ".zshrc"] {
        let root = home("");
        fs::write(
            root.path().join(name),
            "typeset -g POWERLEVEL9K_TERM_SHELL_INTEGRATION=false\n",
        )
        .unwrap();
        fs::write(
            root.path().join(".zlogin"),
            "print -r -- INTEGRATION=$POWERLEVEL9K_TERM_SHELL_INTEGRATION\n",
        )
        .unwrap();
        let session = Session::remote(root.path(), vec![("SHELL".into(), zsh.clone())]);
        let first = session.read(B);
        assert!(first.contains("INTEGRATION=false\r\n"), "{first:?}");
    }
}

#[test]
fn 真实zsh中文目录的osc按utf8字节编码() {
    let Some(zsh) = zsh() else {
        eprintln!("未安装 zsh，跳过真实 zsh 集成测试");
        return;
    };
    let root = home("");
    let directory = root.path().join("中文 目录");
    fs::create_dir(&directory).unwrap();
    let session = Session::remote(
        root.path(),
        vec![
            ("SHELL".into(), zsh),
            ("LC_ALL".into(), "C.UTF-8".into()),
            ("TEST_DIRECTORY".into(), directory.to_str().unwrap().into()),
        ],
    );
    session.read(B);
    // 路径经环境变量传入，避免终端行编辑的字符解码影响 URL 编码断言。
    let line = "cd -- \"$TEST_DIRECTORY\"";
    let text = session.run(line);
    record(&text, 0, line);
    assert!(
        text.contains("/%E4%B8%AD%E6%96%87%20%E7%9B%AE%E5%BD%95\x07"),
        "{text:?}"
    );
}

#[test]
fn 其他登录shell可原样解析外层包装() {
    for shell in [
        "/bin/bash",
        "/bin/dash",
        "/usr/bin/busybox",
        "/usr/bin/tcsh",
        "/usr/bin/fish",
    ] {
        if !Path::new(shell).exists() {
            eprintln!("未安装 {shell}，跳过登录 shell 解析检查");
            continue;
        }
        let root = home("");
        let mut args = vec![];
        if shell.ends_with("busybox") {
            args.push("sh".into());
        }
        args.extend(["-c".into(), command(Some("/bin/bash")).unwrap()]);
        let session = Session::new(shell, args, root.path(), vec![]);
        session.read(B);
        record(&session.run("true"), 0, "true");
    }
}

fn ssh_fixture() -> crate::builtin_test_support::Fixture {
    use crate::builtin_test_support::{Fixture, sshd};
    let root = crate::test_support::tempdir();
    let temp = root.path().join("tmp");
    fs::create_dir(&temp).unwrap();
    let server =
        sshd::Server::with_root(root, &format!("SetEnv TMPDIR={}\n", temp.display()), true);
    fs::write(
        server.root.path().join("h/.bash_profile"),
        "HISTFILE=/dev/null\n",
    )
    .unwrap();
    let mut fixture = Fixture::from_sshd(server);
    fixture.server.login_shell = Some("/bin/bash".into());
    fixture
}

#[test]
fn 内置引擎经本机sshd收到提示符标记() {
    let f = ssh_fixture();
    let command = f.command();
    let kind = crate::terminal::remote(
        &f.manager,
        command.builtin.as_ref().unwrap().request(),
        None,
        true,
        || panic!("不能回退到系统引擎"),
    )
    .unwrap();
    let session = Session::from_kind(kind);
    assert!(session.read(B).contains(A));
    record(&session.run("false"), 1, "false");
    assert_eq!(
        fs::read_dir(f.sshd.root.path().join("tmp"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn 系统引擎经本机sshd收到提示符标记() {
    use crate::ssh::{
        args::{CommonOpts, Purpose, build_args},
        target::SshTarget,
    };
    let f = ssh_fixture();
    let hosts = f.sshd.root.path().join("known_hosts");
    fs::write(
        &hosts,
        format!(
            "[127.0.0.1]:{} {}",
            f.sshd.port,
            fs::read_to_string(f.sshd.root.path().join("host.pub")).unwrap()
        ),
    )
    .unwrap();
    let command = build_args(
        &SshTarget::Manual {
            host: "127.0.0.1".into(),
            port: f.sshd.port,
            user: Some(f.sshd.user.clone()),
            identity_file: Some(f.sshd.root.path().join("client")),
            jump: None,
        },
        &Purpose::Terminal {
            login_shell: Some("/bin/bash".into()),
            cwd_marker: None,
            integration: true,
        },
        &CommonOpts {
            known_hosts: Some(hosts),
            ..Default::default()
        },
    )
    .unwrap();
    let session = Session::from_kind(TerminalKind::Remote {
        ssh: "/usr/bin/ssh".into(),
        command,
        config_dir: f.sshd.root.path().into(),
    });
    assert!(session.read(B).contains(A));
    record(&session.run("false"), 1, "false");
    assert_eq!(
        fs::read_dir(f.sshd.root.path().join("tmp"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn 登录shell绝对路径按原路径执行() {
    use std::os::unix::fs::symlink;
    let root = home("");
    let shell = root.path().join("bash");
    symlink("/bin/bash", &shell).unwrap();
    let wrapper = terminal_command(&Some(shell.to_str().unwrap().into()), true)
        .unwrap()
        .unwrap();
    let session = Session::new("/bin/dash", vec!["-c".into(), wrapper], root.path(), vec![]);
    session.read(B);
    let output = session.run("printf 'SHELL-PATH=%s\\n' \"$BASH\"");
    assert!(output.contains(&format!("SHELL-PATH={}\r\n", shell.display())));
}
