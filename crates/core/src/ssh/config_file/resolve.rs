//! 有超时、取消和并发限制的别名解析。
use super::{HostList, validate_alias};
use crate::{
    error::AppError,
    process::{Reader, SshProgram},
    ssh::{
        args::SshCommand,
        config::{ResolvedConfig, host_key_check_weakened, parse_ssh_g},
    },
};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

/// 用 program 解析 alias；timeout 到期终止进程树，返回有效配置或不含配置值的错误。
pub fn resolve_alias(
    program: &SshProgram,
    alias: &str,
    timeout: Duration,
) -> Result<ResolvedConfig, AppError> {
    resolve(program, alias, timeout, &AtomicBool::new(false))
}
fn failed(reason: &str) -> AppError {
    AppError::new("ssh.resolve_failed").with("reason", reason)
}
fn resolve(
    program: &SshProgram,
    alias: &str,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<ResolvedConfig, AppError> {
    validate_alias(alias)?;
    let (output, _) = capture(
        program,
        vec!["-G".into(), "--".into(), alias.into()],
        timeout,
        cancel,
    )?;
    parse_ssh_g(&output)
}

// 仅转换预览开启诊断；旧解析入口保持参数及行为不变。
pub(super) fn capture(
    program: &SshProgram,
    args: Vec<String>,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<(String, String), AppError> {
    if cancel.load(Ordering::Acquire) {
        return Err(AppError::new("ssh.resolve_cancelled"));
    }
    let start = Instant::now();
    let mut process = program
        .spawn(&SshCommand {
            args,
            env: vec![],
            config_text: None,
            builtin: None,
            askpass: None,
        })
        .map_err(|_| failed("无法启动 SSH 程序"))?;
    drop(process.child.stdin.take());
    let mut out = Reader::start(
        process
            .child
            .stdout
            .take()
            .ok_or_else(|| failed("无法读取标准输出"))?,
        1024 * 1024,
    )?;
    let mut err = match Reader::start(
        process
            .child
            .stderr
            .take()
            .ok_or_else(|| failed("无法读取错误输出"))?,
        64 * 1024,
    ) {
        Ok(r) => r,
        Err(e) => {
            drop(process);
            out.join();
            return Err(e);
        }
    };
    let result = (|| loop {
        if cancel.load(Ordering::Acquire) {
            return Err(AppError::new("ssh.resolve_cancelled"));
        }
        if start.elapsed() >= timeout {
            return Err(AppError::new("ssh.resolve_timeout"));
        }
        let stdout = out.state.lock().map_err(|_| failed("输出读取状态不可用"))?;
        let stderr = err.state.lock().map_err(|_| failed("错误读取状态不可用"))?;
        if stdout.exceeded || stderr.exceeded || stdout.failed || stderr.failed {
            return Err(failed("SSH 输出过大或读取失败"));
        }
        if let Some(status) = process.try_wait()? {
            if !status.success() {
                // stderr 可能包含任意未标记的秘密；仅给出退出码，不回传原文。
                return Err(failed("SSH 配置解析退出失败").with(
                    "exit_code",
                    status
                        .code()
                        .map_or_else(|| "信号终止".into(), |c| c.to_string()),
                ));
            }
            if stdout.done && stderr.done {
                return Ok((
                    String::from_utf8_lossy(&stdout.bytes).into_owned(),
                    String::from_utf8_lossy(&stderr.bytes).into_owned(),
                ));
            }
        }
        drop(stdout);
        drop(stderr);
        thread::sleep(Duration::from_millis(10));
    })();
    drop(process);
    out.join();
    err.join();
    result
}

#[derive(Debug, Clone)]
pub struct ImportCandidate {
    pub alias: String,
    pub file: PathBuf,
    pub line: u32,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
    pub via_jump: bool,
    pub has_forwards: bool,
    pub host_key_check_weakened: bool,
    pub already_imported: bool,
    pub error: Option<AppError>,
}
/// 按 list 顺序返回预览；与 existing_aliases 区分大小写比较，cancel 为真时取消未完成解析。
/// 每个别名最多运行 5 秒，同时最多 4 个；单个失败不影响其他别名。
pub fn preview(
    program: &SshProgram,
    list: &HostList,
    existing_aliases: &HashSet<String>,
    cancel: &AtomicBool,
) -> Vec<ImportCandidate> {
    let mut candidates: Vec<_> = list
        .hosts
        .iter()
        .map(|host| ImportCandidate {
            alias: host.alias.clone(),
            file: host.file.clone(),
            line: host.line,
            hostname: None,
            user: None,
            port: None,
            identity_files: vec![],
            via_jump: false,
            has_forwards: false,
            host_key_check_weakened: false,
            already_imported: existing_aliases.contains(&host.alias),
            error: Some(if cancel.load(Ordering::Acquire) {
                AppError::new("ssh.resolve_cancelled")
            } else {
                failed("无法创建解析工作线程")
            }),
        })
        .collect();
    // 四个工作线程各自处理互不重叠的条目，无需共享可变结果或持有数据库连接。
    thread::scope(|scope| {
        let mut lanes = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for (index, candidate) in candidates.iter_mut().enumerate() {
            lanes[index % 4].push(candidate);
        }
        for lane in lanes {
            let _ = thread::Builder::new()
                .name("SSH导入预览".into())
                .spawn_scoped(scope, move || {
                    for candidate in lane {
                        match resolve(program, &candidate.alias, Duration::from_secs(5), cancel) {
                            Ok(cfg) => {
                                candidate.host_key_check_weakened = host_key_check_weakened(&cfg);
                                candidate.hostname = Some(cfg.hostname);
                                candidate.user = Some(cfg.user);
                                candidate.port = Some(cfg.port);
                                candidate.identity_files = cfg.identity_files;
                                candidate.via_jump =
                                    cfg.proxy_jump.is_some() || cfg.proxy_command.is_some();
                                candidate.has_forwards = cfg.has_forwards;
                                candidate.error = None;
                            }
                            Err(e) => candidate.error = Some(e),
                        }
                    }
                });
        }
    });
    candidates
}
