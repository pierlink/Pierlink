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
/// Windows 用无窗口 PowerShell，其他平台用 /bin/sh；取消时保留已读输出，timed_out 仅表示期限已到。
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
#[cfg(not(windows))]
const COMMAND_MAX: usize = 12000;

#[cfg(any(windows, test))]
fn windows_absolute(path: &str) -> bool {
    let path = path.replace('\\', "/");
    let path = path.strip_prefix("//?/").unwrap_or(&path);
    let path = if path
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case("UNC/"))
    {
        format!("//{}", &path[4..])
    } else {
        path.to_owned()
    };
    let bytes = path.as_bytes();
    (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/')
        || path.strip_prefix("//").is_some_and(|tail| {
            let mut parts = tail.split('/');
            parts.next().is_some_and(|p| !p.is_empty() && p != ".")
                && parts.next().is_some_and(|p| !p.is_empty())
        })
}
fn invalid_cwd() -> AppError {
    AppError::new("mcp.invalid_argument").with_reason(
        "mcp.cwd_must_be_windows_absolute",
        "cwd 必须是 Windows 绝对路径",
    )
}
fn too_long(max: usize) -> AppError {
    AppError::new("mcp.command_too_long")
        .with("max", max)
        .with_reason(
            "mcp.command_exceeds_the_length_limit",
            "命令超过长度限制（max 按 UTF-16 单元计）",
        )
}

// 与 Windows 参数数组的引号、反斜杠规则一致，包含程序路径和结尾 NUL。
#[cfg(any(windows, test))]
fn command_line_len(program: &Path, args: &[String], raw: bool) -> usize {
    let program = program.to_string_lossy().encode_utf16().count() + 2;
    program
        + 1
        + args
            .iter()
            .map(|arg| {
                let units = arg.encode_utf16().count();
                if raw || !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
                    return units + 1;
                }
                let mut extra = 2;
                let mut slashes = 0;
                for c in arg.chars() {
                    if c == '\\' {
                        slashes += 1;
                    } else {
                        if c == '"' {
                            extra += slashes + 1;
                        }
                        slashes = 0;
                    }
                }
                units + extra + slashes + 1
            })
            .sum::<usize>()
}
#[cfg(any(windows, test))]
fn windows_args(program: &Path, shell: &str, command: &str) -> Result<Vec<String>, AppError> {
    let args = match shell {
        "powershell" | "pwsh" => powershell(command),
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
    let limit = if shell == "cmd" { 8000 } else { 32700 };
    if command_line_len(program, &args, shell == "cmd") > limit {
        let max = if matches!(shell, "powershell" | "pwsh") {
            let empty = powershell("");
            let overhead =
                command_line_len(program, &empty, false) - empty.last().map_or(0, String::len);
            let wrapped = powershell_script("").encode_utf16().count();
            (limit.saturating_sub(overhead) / 4 * 3 / 2).saturating_sub(wrapped)
        } else {
            // 命令中需要额外转义的字符也占最终命令行空间。
            let overhead = command_line_len(program, &args, shell == "cmd")
                .saturating_sub(command.encode_utf16().count());
            limit.saturating_sub(overhead)
        };
        return Err(too_long(max));
    }
    Ok(args)
}
pub(super) fn prepare(
    command: &str,
    cwd: Option<&Path>,
    shell: Option<&str>,
) -> Result<Prepared, AppError> {
    #[cfg(not(windows))]
    if command.encode_utf16().count() > COMMAND_MAX {
        return Err(too_long(COMMAND_MAX));
    }
    if let Some(path) = cwd {
        #[cfg(windows)]
        let absolute = windows_absolute(&path.to_string_lossy());
        #[cfg(not(windows))]
        let absolute = path.is_absolute();
        if !absolute {
            return Err(invalid_cwd());
        }
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
        let args = windows_args(&program, shell, command)?;
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
            timed_out: !cancelled.load(Ordering::Acquire) && limits.timeout.is_zero(),
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
    output.timed_out &= !cancelled.load(Ordering::Acquire);
    output.duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    Ok(output)
}
#[cfg(any(windows, test))]
fn powershell_script(command: &str) -> String {
    format!(
        "[Console]::InputEncoding=[Text.UTF8Encoding]::new($false); [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); $OutputEncoding=[Console]::OutputEncoding; $LASTEXITCODE=0; {command}\n$pierlinkSucceeded=$?; $pierlinkExitCode=$LASTEXITCODE; if ($pierlinkSucceeded) {{ exit 0 }} elseif ($null -ne $pierlinkExitCode -and $pierlinkExitCode -ne 0) {{ exit $pierlinkExitCode }} else {{ exit 1 }}"
    )
}
#[cfg(any(windows, test))]
fn powershell(command: &str) -> Vec<String> {
    use ssh_key::encoding::base64::{Base64, Encoding};
    let bytes: Vec<_> = powershell_script(command)
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
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
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            if started.elapsed() >= limits.timeout {
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
                !run_local("sleep 30", None, limits(), &cancelled)
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
            prepare(&"x".repeat(40000), None, None).err().unwrap().code,
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

#[cfg(all(test, windows))]
mod exit_tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn 原生命令和显式退出码以及脚本失败均保留() {
        for (command, expected) in [
            ("cmd /c exit 3", 3),
            ("cmd /c exit 101", 101),
            ("Write-Error 'failed'", 1),
            ("exit 7", 7),
            ("cmd /c exit 3; Write-Output ok", 0),
            ("Write-Output ok # 注释", 0),
        ] {
            let result = run_local(
                command,
                None,
                RunLimits {
                    timeout: Duration::from_secs(15),
                    output_bytes: 4096,
                },
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(result.exit_code, Some(expected), "{command}");
        }
    }
}

#[cfg(test)]
mod length_tests {
    use super::*;
    #[test]
    fn 编码后命令行在边界内且上限可直接重试() {
        for shell in ["powershell", "pwsh", "cmd", "git-bash"] {
            for program in [
                r"C:\Windows\System32\shell.exe",
                r"C:\Program Files\中文😀\shell.exe",
            ] {
                let program = Path::new(program);
                let error = windows_args(program, shell, &"甲".repeat(40000)).unwrap_err();
                assert_eq!(error.code, "mcp.command_too_long");
                let max: usize = error.params["max"].parse().unwrap();
                let args = windows_args(program, shell, &"甲".repeat(max)).unwrap();
                let limit = if shell == "cmd" { 8000 } else { 32700 };
                assert!(command_line_len(program, &args, shell == "cmd") <= limit);
                assert_eq!(
                    windows_args(program, shell, &"甲".repeat(max + 1))
                        .unwrap_err()
                        .code,
                    "mcp.command_too_long"
                );
                assert!(windows_args(program, shell, &"😀".repeat(max / 2)).is_ok());
                assert!(windows_args(program, shell, &"😀".repeat(max / 2 + 1)).is_err());
            }
        }
        assert!(
            windows_args(
                Path::new("powershell.exe"),
                "powershell",
                &"x".repeat(12000)
            )
            .is_err()
        );
    }
    #[test]
    fn 工作目录拒绝相对路径并识别盘符和网络绝对路径() {
        for path in [".", "src", r"C:work", r"\work", "/work", ""] {
            assert!(!windows_absolute(path), "{path}");
        }
        for path in [
            r"C:\work",
            "d:/work",
            r"\\host\share\work",
            r"\\?\C:\work",
            r"\\?\UNC\host\share\work",
        ] {
            assert!(windows_absolute(path), "{path}");
        }
        for path in [".", "src", "C:work"] {
            let error = prepare("echo ok", Some(Path::new(path)), None)
                .err()
                .unwrap();
            assert_eq!(error.code, "mcp.invalid_argument");
            assert_eq!(error.params["reason"], "cwd 必须是 Windows 绝对路径");
        }
    }
    #[test]
    fn 最终长度包含程序引号参数转义和结束符() {
        assert_eq!(
            command_line_len(Path::new("x.exe"), &["a b".into()], false),
            7 + 1 + 5 + 1
        );
        assert_eq!(
            command_line_len(Path::new("x.exe"), &["a\"b".into()], false),
            7 + 1 + 6 + 1
        );
        assert!(windows_args(Path::new("cmd.exe"), "cmd", "echo a\necho b").is_err());
    }
}
