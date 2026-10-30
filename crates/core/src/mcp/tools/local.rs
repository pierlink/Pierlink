//! 本机命令继承用户环境，输出有界，结束时清理整棵进程树。
use super::super::{RunLimits, RunOutput};
use crate::{
    error::AppError,
    process::{collect_tree, spawn_tree},
};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

/// 按 command、cwd 和 limits 执行本机命令；返回原始退出码和双流头尾，超时或 cancelled 时清理进程树。
/// Windows 用无窗口 PowerShell，其他平台用 /bin/sh；取消也设置 timed_out，并保留已读输出。
pub fn run_local(
    command: &str,
    cwd: Option<&Path>,
    limits: RunLimits,
    cancelled: &AtomicBool,
) -> Result<RunOutput, AppError> {
    let prepared = prepare(command, cwd, None)?;
    run_prepared(&prepared, limits, cancelled, None)
}
pub(super) struct Prepared {
    program: std::path::PathBuf,
    args: Vec<String>,
    /// 参数原样拼进命令行（cmd.exe）
    raw: bool,
    pub cwd: std::path::PathBuf,
    pub shell: String,
}
pub(super) type OutputSink = std::sync::Arc<dyn Fn(bool, &[u8]) + Send + Sync>;
pub(super) fn home() -> Result<std::path::PathBuf, AppError> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
        .ok_or_else(|| {
            AppError::new("mcp.cwd_not_found").with_reason(
                "mcp.couldn_t_determine_the_user_directory",
                "无法确定用户目录",
            )
        })
}
pub(super) fn shells() -> Vec<(String, std::path::PathBuf)> {
    crate::detect::shells::available_shells()
        .into_iter()
        .filter_map(|s| {
            use crate::detect::shells::ShellKind;
            let name = match s.kind {
                ShellKind::WindowsPowerShell => "powershell",
                ShellKind::PowerShell7 => "pwsh",
                ShellKind::Cmd => "cmd",
                ShellKind::GitBash => "git-bash",
                _ => return None,
            };
            Some((name.to_owned(), s.program))
        })
        .collect()
}
const COMMAND_MAX: usize = 12000;
pub(super) fn prepare(
    command: &str,
    cwd: Option<&Path>,
    shell: Option<&str>,
) -> Result<Prepared, AppError> {
    if command.encode_utf16().count() > COMMAND_MAX {
        return Err(AppError::new("mcp.command_too_long")
            .with("max", COMMAND_MAX)
            .with_reason("mcp.command_exceeds_the_length_limit", "命令超过长度限制"));
    }
    if command.contains('\0') {
        return Err(AppError::new("mcp.invalid_argument").with_reason(
            "mcp.command_cannot_contain_null_characters",
            "命令不能包含空字符",
        ));
    }
    let cwd = match cwd {
        Some(p) => p.to_owned(),
        None => home()?,
    };
    let cwd = crate::fs_local::plain_canonical(&cwd).map_err(|_| {
        AppError::new("mcp.cwd_not_found").with_reason(
            "mcp.working_directory_is_missing_or_inaccessible",
            "工作目录不存在或无法访问",
        )
    })?;
    if !cwd.is_dir() {
        return Err(AppError::new("mcp.cwd_not_found").with_reason(
            "mcp.working_directory_is_not_a_directory",
            "工作目录不是目录",
        ));
    }
    #[cfg(windows)]
    {
        let shell = shell.unwrap_or("powershell");
        let program = shells()
            .into_iter()
            .find(|(name, _)| name == shell)
            .map(|(_, p)| p)
            .ok_or_else(|| {
                AppError::new("mcp.shell_unavailable").with_reason(
                    "mcp.selected_shell_is_unavailable_check_list_targets",
                    "所选 shell 不可用，请查看 list_targets",
                )
            })?;
        let args = match shell {
            "powershell" | "pwsh" => powershell(command),
            // cmd.exe 不认 `\"` 转义：整条命令用一对引号包起来原样交给 /s /c（/s 只去掉最外层的引号）
            "cmd" => {
                if command.contains(['\r', '\n']) {
                    return Err(AppError::new("mcp.process_failed").with_reason(
                        "mcp.command_prompt_cannot_run_multiline_commands_use_powershell_or_a",
                        "命令提示符不支持多行命令，请改用 PowerShell 或写成批处理文件",
                    ));
                }
                vec![
                    "/d".into(),
                    "/s".into(),
                    "/c".into(),
                    format!("\"chcp 65001 >nul & {command}\""),
                ]
            }
            "git-bash" => vec![
                "--noprofile".into(),
                "--norc".into(),
                "-c".into(),
                command.into(),
            ],
            _ => {
                return Err(AppError::new("mcp.shell_unavailable")
                    .with_reason("mcp.selected_shell_is_unavailable", "所选 shell 不可用"));
            }
        };
        Ok(Prepared {
            raw: shell == "cmd",
            program,
            args,
            cwd,
            shell: shell.into(),
        })
    }
    #[cfg(not(windows))]
    {
        if shell.is_some() {
            return Err(AppError::new("mcp.shell_unavailable").with_reason(
                "mcp.selected_windows_shell_is_unavailable_on_this_platform",
                "此平台没有所选 Windows shell",
            ));
        }
        Ok(Prepared {
            raw: false,
            program: "/bin/sh".into(),
            args: vec!["-c".into(), command.into()],
            cwd,
            shell: "sh".into(),
        })
    }
}
pub(super) fn run_prepared(
    prepared: &Prepared,
    limits: RunLimits,
    cancelled: &AtomicBool,
    sink: Option<OutputSink>,
) -> Result<RunOutput, AppError> {
    let started = Instant::now();
    if cancelled.load(Ordering::Acquire) || limits.timeout.is_zero() {
        return Ok(RunOutput {
            timed_out: true,
            ..Default::default()
        });
    }
    let mut process = if prepared.raw {
        crate::process::spawn_tree_raw(&prepared.program, &prepared.args, Some(&prepared.cwd))?
    } else {
        spawn_tree(&prepared.program, &prepared.args, Some(&prepared.cwd), &[])?
    };
    let limits = RunLimits {
        timeout: limits.timeout.saturating_sub(started.elapsed()),
        ..limits
    };
    let mut output = if let Some(sink) = sink {
        stream(&mut process, limits, cancelled, sink)?
    } else {
        collect_tree(
            &mut process,
            limits,
            || cancelled.load(Ordering::Acquire),
            None,
        )?
        .0
    };
    output.duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    Ok(output)
}
#[cfg(windows)]
fn powershell(command: &str) -> Vec<String> {
    use ssh_key::encoding::base64::{Base64, Encoding};
    let script = format!(
        "[Console]::InputEncoding=[Text.UTF8Encoding]::new($false); [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); $OutputEncoding=[Console]::OutputEncoding; {command}"
    );
    let bytes: Vec<_> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    vec![
        "-NoLogo".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-EncodedCommand".into(),
        Base64::encode_string(&bytes),
    ]
}
fn stream(
    process: &mut crate::process::TreeProcess,
    limits: RunLimits,
    cancelled: &AtomicBool,
    sink: OutputSink,
) -> Result<RunOutput, AppError> {
    use super::super::lock;
    use crate::process::{HeadTail, join_bounded};
    use std::{
        io::Read,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };
    let started = Instant::now();
    drop(process.child.stdin.take());
    let mut readers = Vec::new();
    let mut captures = Vec::new();
    let inputs: [Option<Box<dyn Read + Send>>; 2] = [
        process.child.stdout.take().map(|p| Box::new(p) as _),
        process.child.stderr.take().map(|p| Box::new(p) as _),
    ];
    let result = (|| {
        for (stderr, input) in [false, true].into_iter().zip(inputs) {
            let mut input = input.ok_or_else(|| {
                AppError::new("mcp.process_failed")
                    .with_reason("mcp.couldn_t_read_the_command_pipe", "无法读取命令管道")
            })?;
            let capture = Arc::new(Mutex::new((
                HeadTail::new(limits.output_bytes),
                false,
                false,
            )));
            let state = capture.clone();
            let sink = sink.clone();
            let reader = thread::Builder::new()
                .name("后台输出".into())
                .spawn(move || {
                    let mut chunk = [0; 8192];
                    loop {
                        match input.read(&mut chunk) {
                            Ok(0) => {
                                lock(&state).1 = true;
                                break;
                            }
                            Ok(n) => {
                                lock(&state).0.push(&chunk[..n]);
                                sink(stderr, &chunk[..n]);
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(_) => {
                                let mut state = lock(&state);
                                state.1 = true;
                                state.2 = true;
                                break;
                            }
                        }
                    }
                })
                .map_err(|_| {
                    AppError::new("mcp.process_failed").with_reason(
                        "mcp.couldn_t_create_the_output_reader_thread",
                        "无法创建输出读取线程",
                    )
                })?;
            readers.push(reader);
            captures.push(capture);
        }
        let mut output = RunOutput::default();
        let mut exited = None;
        loop {
            if cancelled.load(Ordering::Acquire) || started.elapsed() >= limits.timeout {
                output.timed_out = true;
                break;
            }
            if let Some(status) = process.try_wait()? {
                output.exit_code = status.code();
                if captures.iter().all(|c| lock(c).1)
                    || exited.get_or_insert_with(Instant::now).elapsed()
                        >= Duration::from_millis(100)
                {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok::<_, AppError>(output)
    })();
    let terminated = process.terminate();
    for reader in readers {
        join_bounded(reader, Duration::from_secs(2));
    }
    let mut output = result?;
    terminated?;
    for (index, state) in captures.iter().enumerate() {
        let state = lock(state);
        if state.2 {
            return Err(AppError::new("mcp.process_failed")
                .with_reason("mcp.couldn_t_read_command_output", "读取命令输出失败"));
        }
        let (bytes, truncated) = state.0.snapshot();
        output.truncated |= truncated;
        if index == 0 {
            output.stdout = bytes;
        } else {
            output.stderr = bytes;
        }
    }
    Ok(output)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{thread, time::Duration};
    fn limits() -> RunLimits {
        RunLimits {
            timeout: Duration::from_secs(5),
            output_bytes: 64,
        }
    }
    #[test]
    fn 本机退出码双流头尾工作目录及缺失目录() {
        let root = crate::test_support::tempdir();
        let output = run_local("printf head; head -c 100000 /dev/zero | tr '\\0' x; printf tail; printf error >&2; exit 7", Some(root.path()), limits(), &AtomicBool::new(false)).unwrap();
        assert_eq!(output.exit_code, Some(7));
        assert!(output.stdout.starts_with(b"head") && output.stdout.ends_with(b"tail"));
        assert_eq!(output.stderr, b"error");
        assert!(output.truncated && !output.timed_out);
        assert!(output.stdout.len() < 150);
        assert_eq!(
            run_local(
                "pwd",
                Some(&root.path().join("missing")),
                limits(),
                &AtomicBool::new(false)
            )
            .unwrap_err()
            .code,
            "mcp.cwd_not_found"
        );
        let output = run_local(
            "pwd",
            Some(root.path()),
            RunLimits {
                output_bytes: 4096,
                ..limits()
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            root.path().to_str().unwrap()
        );
    }
    #[test]
    fn 超时杀死孙进程且取消一秒内返回() {
        let root = crate::test_support::tempdir();
        let pidfile = root.path().join("pid");
        let command = format!(
            "sh -c 'sleep 30 & echo $! > {}; wait' & wait",
            pidfile.display()
        );
        let output = run_local(
            &command,
            None,
            RunLimits {
                timeout: Duration::from_millis(150),
                ..limits()
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(output.timed_out);
        let pid = std::fs::read_to_string(pidfile).unwrap();
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()));
        assert!(stat.is_err() || stat.unwrap().split_whitespace().nth(2) == Some("Z"));
        let cancelled = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(50));
                cancelled.store(true, Ordering::Release);
            });
            let before = Instant::now();
            assert!(
                run_local("sleep 30", None, limits(), &cancelled)
                    .unwrap()
                    .timed_out
            );
            assert!(before.elapsed() < Duration::from_secs(1));
        });
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;
    #[test]
    fn 默认目录与过长命令错误() {
        let prepared = prepare("echo ok", None, None).unwrap();
        assert_eq!(
            prepared.cwd,
            crate::fs_local::plain_canonical(&home().unwrap()).unwrap()
        );
        assert_eq!(
            prepare(&"x".repeat(COMMAND_MAX + 1), None, None)
                .err()
                .unwrap()
                .code,
            "mcp.command_too_long"
        );
        assert!(prepare("echo ok", None, Some("invalid")).is_err());
        assert!(shells().iter().all(|(_, path)| path.is_absolute()));
    }
    #[cfg(windows)]
    #[test]
    fn 命令提示符里的双引号原样传给命令() {
        let root = crate::test_support::tempdir();
        let windir = std::env::var("WINDIR").unwrap();
        let prepared = prepare(
            &format!("echo \"a b\" & dir /b \"{windir}\" >nul && echo DIR_OK"),
            Some(root.path()),
            Some("cmd"),
        )
        .unwrap();
        let output = run_prepared(
            &prepared,
            RunLimits {
                timeout: std::time::Duration::from_secs(20),
                output_bytes: 8192,
            },
            &AtomicBool::new(false),
            None,
        )
        .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("\"a b\""), "{stdout}");
        assert!(!stdout.contains("\\\""), "{stdout}");
        assert!(stdout.contains("DIR_OK"), "{stdout}");
        assert!(prepare("echo a\necho b", Some(root.path()), Some("cmd")).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn 两种powershell的中文原生程序双流输出() {
        let root = crate::test_support::tempdir();
        for shell in ["powershell", "pwsh"] {
            if !shells().iter().any(|(name, _)| name == shell) {
                continue;
            }
            // 先固定原生子进程代码页，再验证 .NET 和原生输出都经 UTF-8 管道返回。
            let script = "Write-Output '中文甲'; & cmd.exe /d /c 'chcp 65001 >nul & echo 中文乙 & echo 中文错 1>&2'";
            let prepared = prepare(script, Some(root.path()), Some(shell)).unwrap();
            let output = run_prepared(
                &prepared,
                RunLimits {
                    timeout: std::time::Duration::from_secs(20),
                    output_bytes: 8192,
                },
                &AtomicBool::new(false),
                None,
            )
            .unwrap();
            assert_eq!(output.exit_code, Some(0));
            let stdout = String::from_utf8(output.stdout).unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(stdout.contains("中文甲") && stdout.contains("中文乙"));
            assert!(stderr.contains("中文错"));
        }
    }
}
