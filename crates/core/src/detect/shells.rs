//! 本机 shell 检测与默认项选择；不修改用户环境或尝试提权。
use std::path::{Path, PathBuf};
#[path = "bash.rs"]
pub(crate) mod bash;
#[path = "git.rs"]
mod git;
#[cfg(windows)]
#[path = "shells_windows.rs"]
mod windows;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellKind {
    PowerShell7,
    WindowsPowerShell,
    GitBash,
    Wsl { distro: String },
    Cmd,
    Other,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalShell {
    pub id: String,
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub kind: ShellKind,
}
const POWERSHELL_CWD: &str = r#"$global:__PierlinkOriginalPrompt = $function:prompt; function global:prompt { $p = $executionContext.SessionState.Path.CurrentFileSystemLocation.ProviderPath; [Console]::Write(([char]27).ToString() + ']9;9;"' + $p + '"' + [char]27 + '\'); & $global:__PierlinkOriginalPrompt }"#;

fn cwd_options(kind: &ShellKind, mut args: Vec<String>) -> (Vec<String>, Vec<(String, String)>) {
    let mut env = Vec::new();
    match kind {
        ShellKind::PowerShell7 | ShellKind::WindowsPowerShell => {
            // -Command 在配置文件加载之后执行，保留用户（含 oh-my-posh）定义的 prompt。
            args.extend(["-NoExit".into(), "-Command".into(), POWERSHELL_CWD.into()]);
        }
        ShellKind::Cmd => env.push(("PROMPT".into(), "$E]9;9;\"$P\"$E\\$P$G".into())),
        _ => (),
    }
    (args, env)
}
/// 解码 wsl -l -q 的 UTF-16LE 字节，返回去 BOM、空字符、空行及重复项的发行版名。
/// 不完整的末尾字节忽略；非法代理项用替代字符表示。
pub fn parse_wsl_list(bytes: &[u8]) -> Vec<String> {
    let words: Vec<_> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .filter(|&w| w != 0 && w != 0xfeff)
        .collect();
    let mut result = Vec::new();
    for line in String::from_utf16_lossy(&words).lines() {
        let name = line.trim();
        if !name.is_empty() && !result.iter().any(|s| s == name) {
            result.push(name.to_string());
        }
    }
    result
}
/// 从 shells 按产品默认优先级选取借用项；空列表返回 None，同级保持检测顺序。
pub fn pick_default(shells: &[LocalShell]) -> Option<&LocalShell> {
    shells.iter().min_by_key(|s| match s.kind {
        ShellKind::PowerShell7 => 0,
        ShellKind::WindowsPowerShell => 1,
        ShellKind::GitBash => 2,
        ShellKind::Other | ShellKind::Wsl { .. } => 3,
        ShellKind::Cmd => 4,
    })
}
/// 在本次检测的 shells 中找 preferred_id；已卸载时按默认顺序回退，空列表返回 None。
pub fn fallback_default<'a>(
    shells: &'a [LocalShell],
    preferred_id: &str,
) -> Option<&'a LocalShell> {
    shells
        .iter()
        .find(|s| s.id == preferred_id)
        .or_else(|| pick_default(shells))
}
fn add(
    shells: &mut Vec<LocalShell>,
    program: PathBuf,
    name: &str,
    kind: ShellKind,
    args: Vec<String>,
) {
    // 只接受绝对路径：相对路径会相对当前目录解析，可能被同目录下的同名程序劫持
    if !program.is_absolute() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !program
            .metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        {
            return;
        }
    }
    if !program.is_file() {
        return;
    }
    let normalized = crate::fs_local::plain_canonical(&program).unwrap_or_else(|_| program.clone());
    let path = normalized.to_string_lossy().to_string();
    let path = if cfg!(windows) {
        path.to_lowercase()
    } else {
        path
    };
    // 同一 wsl.exe 的不同发行版是不同入口，不按程序路径合并发行版。
    let id = match &kind {
        ShellKind::Wsl { distro } => format!("{path}#{distro}"),
        _ => path,
    };
    if !shells.iter().any(|s| s.id == id) {
        let (args, env) = cwd_options(&kind, args);
        shells.push(LocalShell {
            id,
            name: name.into(),
            program,
            args,
            env,
            kind,
        });
    }
}
/// 检测本机 shell 并按完整路径去重，返回可用于 Local 会话的参数和环境。
/// wrapper_dir 是调用方提供的包装脚本目录；Bash 脚本原子替换并按固定文件名复用。
/// 包装脚本写不进去时这个 Bash 照常启动，只是文件面板不能跟随它的目录。
pub fn detect_local_shells(wrapper_dir: &Path) -> Vec<LocalShell> {
    let mut shells = detect();
    for shell in &mut shells {
        let _ = bash::prepare(shell, wrapper_dir);
    }
    shells
}
/// 无参数；返回检测到的本机 shell，不生成终端包装脚本，供命令工具列出和选择程序。
pub fn available_shells() -> Vec<LocalShell> {
    detect()
}
fn detect() -> Vec<LocalShell> {
    #[cfg(windows)]
    {
        windows::detect()
    }
    #[cfg(not(windows))]
    {
        let mut shells = Vec::new();
        if let Some(shell) = std::env::var_os("SHELL") {
            add(
                &mut shells,
                shell.into(),
                "用户 Shell",
                ShellKind::Other,
                vec![],
            );
        }
        add(
            &mut shells,
            "/bin/bash".into(),
            "bash",
            ShellKind::Other,
            vec![],
        );
        add(
            &mut shells,
            "/bin/sh".into(),
            "sh",
            ShellKind::Other,
            vec![],
        );
        shells
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 当前目录钩子的参数与已有提示符保留() {
        for kind in [ShellKind::PowerShell7, ShellKind::WindowsPowerShell] {
            let (args, env) = cwd_options(&kind, vec![]);
            assert_eq!(&args[..2], ["-NoExit", "-Command"]);
            assert!(args[2].contains("= $function:prompt"));
            assert!(args[2].contains("CurrentFileSystemLocation.ProviderPath"));
            assert!(!args[2].contains("CurrentFileSystemLocation.Path"));
            assert!(args[2].contains("& $global:__PierlinkOriginalPrompt"));
            assert!(!args.iter().any(|a| a == "-NoProfile"));
            assert!(env.is_empty());
        }
        let (_, env) = cwd_options(&ShellKind::Cmd, vec![]);
        assert_eq!(env, [("PROMPT".into(), "$E]9;9;\"$P\"$E\\$P$G".into())]);
        let (_, env) = cwd_options(
            &ShellKind::Wsl {
                distro: "Ubuntu".into(),
            },
            vec![],
        );
        assert!(env.is_empty());
    }
    /// Windows 真实测试：检测到的 PowerShell、命令提示符、Git Bash 都要在 cd 之后报告新目录（OSC 9;9）。
    /// 验证参数在 Windows 命令行上的引号转义、用户配置文件加载后钩子仍然生效。
    #[cfg(windows)]
    #[test]
    fn windows_真实终端cd后报告当前目录() {
        use crate::terminal::{TerminalEvent, TerminalKind, TerminalManager, TerminalSpec};
        use std::{
            sync::{Arc, mpsc},
            time::{Duration, Instant},
        };
        let root = crate::test_support::tempdir();
        let shells: Vec<_> = detect_local_shells(root.path())
            .into_iter()
            .filter(|s| {
                matches!(
                    s.kind,
                    ShellKind::PowerShell7
                        | ShellKind::WindowsPowerShell
                        | ShellKind::Cmd
                        | ShellKind::GitBash
                )
            })
            .collect();
        assert!(!shells.is_empty(), "没有检测到可测试的本机终端");
        // CI 的 Windows 机器装了 Git for Windows：必须检测到 Git Bash，而且只出现一次
        assert_eq!(
            shells
                .iter()
                .filter(|s| s.kind == ShellKind::GitBash)
                .count(),
            1,
            "Git Bash 应恰好检测到一个：{:?}",
            shells.iter().map(|s| &s.program).collect::<Vec<_>>()
        );
        let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let expected = format!("]9;9;\"{windir}\"");
        for shell in shells {
            let manager = TerminalManager::new();
            let (tx, rx) = mpsc::channel();
            let id = manager
                .open(
                    TerminalSpec {
                        kind: TerminalKind::Local {
                            program: shell.program.clone(),
                            args: shell.args.clone(),
                            cwd: None,
                            env: shell.env.clone(),
                        },
                        cols: 120,
                        rows: 30,
                    },
                    Arc::new(move |event| {
                        let _ = tx.send(event);
                    }),
                )
                .unwrap();
            let command = match shell.kind {
                ShellKind::GitBash => format!("cd \"$(cygpath -u '{windir}')\"\r"),
                // 命令提示符跨盘符切换目录要加 /d（CI 的工作目录在 D: 盘）
                ShellKind::Cmd => format!("cd /d \"{windir}\"\r"),
                _ => format!("cd \"{windir}\"\r"),
            };
            let start = Instant::now();
            let mut output = Vec::new();
            let mut sent = false;
            let found = loop {
                if start.elapsed() > Duration::from_secs(60) {
                    break false;
                }
                // 第一次出现提示符（任意 9;9 报告）后再输入 cd
                if !sent && output.windows(5).any(|w| w == b"]9;9;") {
                    manager.write(&id, command.as_bytes()).unwrap();
                    sent = true;
                }
                if let Ok(TerminalEvent::Output(bytes)) =
                    rx.recv_timeout(Duration::from_millis(500))
                {
                    let _ = manager.ack(&id, bytes.len());
                    output.extend(bytes);
                }
                let text = String::from_utf8_lossy(&output);
                if sent && text.to_lowercase().contains(&expected.to_lowercase()) {
                    break true;
                }
            };
            let _ = manager.close(&id);
            assert!(
                found,
                "{} 没有报告当前目录：已输入 cd={sent}，输出末尾={:?}",
                shell.name,
                String::from_utf8_lossy(&output[output.len().saturating_sub(600)..])
            );
        }
    }
    #[test]
    fn w1_utf16及空字符() {
        let bytes: Vec<_> = "\u{feff}Ubuntu\0\r\nDebian 中文\r\nUbuntu\r\n\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(parse_wsl_list(&bytes), ["Ubuntu", "Debian 中文"]);
        assert!(parse_wsl_list(&[0]).is_empty());
        assert_eq!(parse_wsl_list(&[0, 0xd8]), ["�"]);
    }
    #[test]
    fn w3_默认顺序及卸载回退全分支() {
        let mut shells: Vec<_> = [
            ShellKind::PowerShell7,
            ShellKind::WindowsPowerShell,
            ShellKind::GitBash,
            ShellKind::Other,
            ShellKind::Wsl {
                distro: "Ubuntu".into(),
            },
            ShellKind::Cmd,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, kind)| LocalShell {
            id: i.to_string(),
            name: "测试".into(),
            program: "测试".into(),
            args: vec![],
            env: vec![],
            kind,
        })
        .collect();
        assert_eq!(fallback_default(&shells, "5").unwrap().id, "5");
        while !shells.is_empty() {
            assert_eq!(pick_default(&shells).unwrap().id, shells[0].id);
            assert_eq!(
                fallback_default(&shells, "已卸载").unwrap().id,
                shells[0].id
            );
            shells.remove(0);
        }
        assert!(pick_default(&shells).is_none());
        assert!(fallback_default(&shells, "空").is_none());
    }
    #[test]
    fn w5_完整路径去重及检测() {
        let root = crate::test_support::tempdir();
        let shells = detect_local_shells(root.path());
        assert!(!available_shells().is_empty());
        #[cfg(unix)]
        assert!(
            shells
                .iter()
                .any(|s| s.program == std::path::Path::new("/bin/sh"))
        );
        let ids: std::collections::HashSet<_> = shells.iter().map(|s| &s.id).collect();
        assert_eq!(ids.len(), shells.len());
        let dir = crate::test_support::tempdir();
        let path = dir.path().join("shell");
        std::fs::write(&path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut result = Vec::new();
        add(&mut result, path.clone(), "甲", ShellKind::Other, vec![]);
        add(&mut result, path.clone(), "乙", ShellKind::Other, vec![]);
        assert_eq!(result[0].program, path);
        assert_eq!(result.len(), 1);
        add(
            &mut result,
            dir.path().join("没有"),
            "无",
            ShellKind::Other,
            vec![],
        );
        assert_eq!(result.len(), 1);
    }
}
