//! 用真实伪终端逐级确认 OpenSSH 指纹，不解析或改写 known_hosts。
use crate::{
    error::AppError,
    process::{Reader, SshProgram, environment, io_error},
    remote::run::classify_error,
    ssh::args::SshCommand,
};
use portable_pty::PtySize;
#[cfg(unix)]
use portable_pty::native_pty_system;
use serde::Serialize;
use std::{
    io::Write,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(30);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(90);
const OUTPUT_LIMIT: usize = 64 * 1024;
const SUCCESS_MARKER: &str = "pierlink-hostkey-ok";
const PROBE_COMMAND: &str = "echo pierlink-hostkey-ok";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostKeyInfo {
    pub host: String,
    /// 从 1 开始的确认层级；重复提问也递增，供界面明确展示来源。
    pub level: usize,
    pub key_type: String,
    pub fingerprint: String,
}

/// Trusted 表示 SSH 已完成校验；Confirm 包含当前一级提示，必须逐级显式回答。
pub enum HostKeyPrompt {
    Trusted,
    Confirm(Confirmation),
}
pub struct Confirmation {
    pub info: HostKeyInfo,
    session: Session,
}
struct Session {
    answers: Sender<Answer>,
    updates: Receiver<Result<Option<HostKeyInfo>, AppError>>,
    worker: Option<JoinHandle<()>>,
    timeout: Duration,
    diagnostic: Arc<Mutex<AppError>>,
}
enum Answer {
    Confirm(bool),
    Cancel,
}
impl Drop for Session {
    fn drop(&mut self) {
        // 取消必须直接结束探测，不能等待 ssh 处理 no 或等到连接超时。
        let _ = self.answers.send(Answer::Cancel);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
impl Session {
    fn next(self) -> Result<HostKeyPrompt, AppError> {
        match self
            .updates
            .recv_timeout(self.timeout * 2 + Duration::from_secs(2))
        {
            Ok(Ok(Some(info))) => Ok(HostKeyPrompt::Confirm(Confirmation {
                info,
                session: self,
            })),
            Ok(Ok(None)) => Ok(HostKeyPrompt::Trusted),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(timeout_snapshot(&self.diagnostic)),
        }
    }
}
impl HostKeyPrompt {
    /// 消耗当前提示并写入所确认的指纹或 no；返回下一级提示或已信任；拒绝、超时或指纹变化返回错误。Trusted 无需回答。
    pub fn answer(self, yes: bool) -> Result<Self, AppError> {
        match self {
            Self::Trusted => Ok(Self::Trusted),
            Self::Confirm(c) => {
                c.session
                    .answers
                    .send(Answer::Confirm(yes))
                    .map_err(|_| timeout_snapshot(&c.session.diagnostic))?;
                c.session.next()
            }
        }
    }
}

/// 手动跳板须传 HostKeyProbe 用途并预先写入 config_text；其他目标可传后台用途。
/// 用探测用途 target_cmd 启动 program 的隐藏伪终端探测；连接 30 秒、用户回答 90 秒时限，输出最多 64 KiB，释放提示会取消连接。
/// 禁止密码和键盘交互，避免把凭据提问当主机密钥确认；每个跳板提示单独返回。
/// 远端输出完整校验标记行即成功，随后结束并回收探测进程树，无需等待 ssh 自行退出。
pub fn probe(program: &SshProgram, target_cmd: &SshCommand) -> Result<HostKeyPrompt, AppError> {
    probe_with_limits(program, target_cmd, TIMEOUT, ANSWER_TIMEOUT)
}
#[cfg(all(test, unix))]
fn probe_with_timeout(
    program: &SshProgram,
    target_cmd: &SshCommand,
    timeout: Duration,
) -> Result<HostKeyPrompt, AppError> {
    probe_with_limits(program, target_cmd, timeout, timeout)
}
fn probe_with_limits(
    program: &SshProgram,
    target_cmd: &SshCommand,
    timeout: Duration,
    answer_timeout: Duration,
) -> Result<HostKeyPrompt, AppError> {
    let cmd = probe_command(target_cmd)?;
    let program = program.clone();
    let (answers_tx, answers_rx) = mpsc::channel();
    let (updates_tx, updates_rx) = mpsc::channel();
    let diagnostic = Arc::new(Mutex::new(timeout_diagnostic(&[], &cmd).with_text(
        "phase",
        "ssh.start_fingerprint_probe",
        "启动指纹探测",
    )));
    let worker_diagnostic = diagnostic.clone();
    let worker = thread::Builder::new()
        .name("ssh指纹确认".into())
        .spawn(move || {
            let mut state = ProbeState {
                accepted: false,
                level: 0,
                diagnostic: worker_diagnostic,
            };
            let mut result = interact(
                &program,
                &cmd,
                &answers_rx,
                &updates_tx,
                timeout,
                answer_timeout,
                &mut state,
            );
            if state.accepted && result.as_ref().is_err_and(|e| e.code == "ssh.unreachable") {
                // 仅重试一次；新的确认仍必须由用户逐级回答，绝不自动同意。
                result = interact(
                    &program,
                    &cmd,
                    &answers_rx,
                    &updates_tx,
                    timeout,
                    answer_timeout,
                    &mut state,
                );
            }
            let _ = updates_tx.send(result.map(|()| None));
        })
        .map_err(|_| io_error("ssh.create_fingerprint_thread", "创建指纹线程"))?;
    Session {
        answers: answers_tx,
        updates: updates_rx,
        worker: Some(worker),
        timeout,
        diagnostic,
    }
    .next()
}
fn probe_command(cmd: &SshCommand) -> Result<SshCommand, AppError> {
    if cmd.args.last().map(String::as_str) == Some("exit") {
        // 参数构造模块仍用 exit 标识探测用途；仅替换远端命令，保留逐级确认配置。
        let mut probe = cmd.clone();
        if let Some(command) = probe.args.last_mut() {
            *command = PROBE_COMMAND.into();
        }
        return Ok(probe);
    }
    if cmd.config_text.is_some() {
        return Err(AppError::new("ssh.host_key_probe_required"));
    }
    if cmd.args.last().map(String::as_str) != Some("/bin/sh -s") {
        return Err(AppError::new("ssh.background_command_required"));
    }
    let mut args = Vec::new();
    // OpenSSH 使用首次出现的值；这些选项必须置于原参数之前。
    for setting in [
        "StrictHostKeyChecking=ask",
        "BatchMode=no",
        "PasswordAuthentication=no",
        "KbdInteractiveAuthentication=no",
        "NumberOfPasswordPrompts=0",
        "FingerprintHash=sha256",
        "LogLevel=ERROR",
        "ClearAllForwardings=yes",
    ] {
        args.extend(["-o".into(), setting.into()]);
    }
    args.extend_from_slice(&cmd.args[..cmd.args.len() - 1]);
    args.push(PROBE_COMMAND.into());
    Ok(SshCommand {
        config_text: None,
        builtin: None,
        askpass: None,
        args,
        env: cmd
            .env
            .iter()
            .filter(|(k, _)| !k.starts_with("SSH_ASKPASS"))
            .cloned()
            .collect(),
    })
}

fn has_success_marker(normalized: &str) -> bool {
    // echo 可用于 sh、cmd 和 PowerShell；命令回显包含 echo 前缀，不等于标记行。
    // 必须整行精确匹配且已收到换行，不能把命令中的子串或未收全的一行当成结果。
    normalized
        .split_inclusive('\n')
        .any(|line| line.strip_suffix('\n') == Some(SUCCESS_MARKER))
}

fn parse_prompt(text: &str) -> Result<Option<HostKeyInfo>, AppError> {
    let normalized = normalize_terminal(text);
    let text = normalized.as_str();
    if text.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || text.contains("REVOKED HOST KEY DETECTED")
    {
        return Err(AppError::new("ssh.host_key_changed"));
    }
    if !text.contains("Are you sure you want to continue connecting")
        || !text.trim_end().ends_with('?')
    {
        return Ok(None);
    }
    let text = text
        .rfind("The authenticity of host '")
        .map_or(text, |start| &text[start..]);
    let host = text
        .rsplit_once("The authenticity of host '")
        .and_then(|(_, s)| s.split_once("' "))
        .map(|(s, _)| s);
    let key = text.lines().find_map(|line| {
        let (kind, fingerprint) = line.trim().split_once(" key fingerprint is SHA256:")?;
        let fingerprint = fingerprint.strip_suffix('.')?;
        if !kind.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || fingerprint.len() != 43
            || !fingerprint
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/".contains(&b))
        {
            return None;
        }
        Some((kind, format!("SHA256:{fingerprint}")))
    });
    match (host, key) {
        (Some(host), Some((kind, fingerprint))) if !host.chars().any(char::is_control) => {
            Ok(Some(HostKeyInfo {
                host: host.into(),
                level: 1,
                key_type: kind.into(),
                fingerprint,
            }))
        }
        _ => Err(AppError::new("ssh.host_key_prompt_invalid")),
    }
}
// CSI、OSC 和普通 ESC 序列不可参与提示解析；未完整输出的序列留待下次整段解析。
fn normalize_terminal(text: &str) -> String {
    let mut chars = text.chars().peekable();
    let mut output = String::new();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.peek() == Some(&'\\')) {
                            if c == '\x1b' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(c) if (' '..='/').contains(&c) => {
                    for c in chars.by_ref() {
                        if ('0'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            output.push('\n');
        } else {
            output.push(c);
        }
    }
    output
}
fn answer_bytes(fingerprint: &str, yes: bool) -> Vec<u8> {
    let answer = if yes { fingerprint } else { "no" };
    format!("{answer}{}", if cfg!(windows) { "\r\n" } else { "\n" }).into_bytes()
}

fn timeout_snapshot(diagnostic: &Mutex<AppError>) -> AppError {
    diagnostic.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn timeout_diagnostic(bytes: &[u8], cmd: &SshCommand) -> AppError {
    let text = normalize_terminal(&String::from_utf8_lossy(bytes));
    let secrets: Vec<_> = cmd.env.iter().map(|(_, value)| value.as_str()).collect();
    // 必须先对全文脱敏再截尾，避免切掉 password= 或私钥头后泄漏秘密。
    let safe = crate::security::redact(&text, &secrets);
    let tail: String = safe
        .chars()
        .rev()
        .take(300)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    AppError::new("ssh.host_key_timeout")
        .with("received_bytes", bytes.len())
        .with_text("pid", "ssh.not_started_yet", "尚未启动")
        .with_text("exited", "ssh.not_queried_yet", "尚未查询")
        .with("tail", tail)
}

#[derive(Default)]
struct TerminalQueries {
    pending: Vec<u8>,
    cursor: usize,
    attributes: usize,
}
impl TerminalQueries {
    fn replies(&mut self, bytes: &[u8]) -> Vec<u8> {
        const QUERIES: &[(&[u8], &[u8])] = &[
            (b"\x1b[6n", b"\x1b[1;1R"),
            (b"\x1b[c", b"\x1b[?1;0c"),
            (b"\x1b[0c", b"\x1b[?1;0c"),
        ];
        let mut replies = Vec::new();
        for &byte in bytes {
            if byte == 0x1b {
                self.pending.clear();
            } else if self.pending.is_empty() {
                continue;
            }
            self.pending.push(byte);
            if let Some((query, reply)) = QUERIES.iter().find(|(q, _)| *q == self.pending) {
                replies.extend_from_slice(reply);
                if *query == b"\x1b[6n" {
                    self.cursor += 1;
                } else {
                    self.attributes += 1;
                }
                self.pending.clear();
            } else if !QUERIES.iter().any(|(q, _)| q.starts_with(&self.pending)) {
                self.pending.clear();
            }
        }
        replies
    }
}

/// 探测用的 ssh 子进程。Windows 上用终端模块的 ConPTY 启动（创建时即加入作业对象）：
/// portable_pty 在 Windows 上启动的子进程会继承父进程的标准句柄，输出不进伪终端，
/// 表现为 30 秒收不到任何输出后报“确认服务器指纹超时”。
struct PtyChild {
    #[cfg(unix)]
    child: Box<dyn portable_pty::Child + Send + Sync>,
    #[cfg(windows)]
    child: crate::terminal::windows::Child,
    #[cfg(unix)]
    reaped: bool,
    cleaned: bool,
}
impl PtyChild {
    fn status(&mut self) -> Result<Option<portable_pty::ExitStatus>, AppError> {
        #[cfg(target_os = "linux")]
        {
            let pid = self
                .child
                .process_id()
                .ok_or_else(|| io_error("ssh.get_fingerprint_process_id", "获取指纹进程编号"))?;
            crate::process::exit_status_unreaped(pid).map(|s| s.map(portable_pty::ExitStatus::from))
        }
        #[cfg(windows)]
        {
            // 探测结束后还要清理跳板等后代；不能采用交互终端保留 GUI 程序的策略。
            self.child
                .try_wait_preserving_job()
                .map_err(|_| io_error("ssh.query_fingerprint_process", "查询指纹进程"))
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            let status = self
                .child
                .try_wait()
                .map_err(|_| io_error("ssh.query_fingerprint_process", "查询指纹进程"))?;
            self.reaped |= status.is_some();
            Ok(status)
        }
    }
    fn terminate(&mut self) -> Result<(), AppError> {
        if self.cleaned {
            return Ok(());
        }
        #[cfg(windows)]
        self.child.terminate_and_wait().map_err(|_| {
            io_error(
                "ssh.terminate_and_reap_fingerprint_process_tree",
                "结束并回收指纹进程树",
            )
        })?;
        #[cfg(unix)]
        if !self.reaped {
            if let Some(pid) = self.child.process_id() {
                crate::process::kill_group(pid);
            }
            let _ = self.child.kill();
            self.child
                .wait()
                .map_err(|_| io_error("ssh.reap_fingerprint_process", "回收指纹进程"))?;
            self.reaped = true;
        }
        self.cleaned = true;
        Ok(())
    }
}
impl Drop for PtyChild {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}
#[cfg(unix)]
fn spawn_pty(
    program: &SshProgram,
    cmd: &SshCommand,
    pair: &portable_pty::PtyPair,
) -> Result<PtyChild, AppError> {
    #[cfg(unix)]
    let child: Box<dyn portable_pty::Child + Send + Sync> = {
        use std::{fs::OpenOptions, os::unix::process::CommandExt, process::Command};
        let path = pair
            .master
            .tty_name()
            .ok_or_else(|| io_error("ssh.get_pseudoterminal_path", "获取伪终端路径"))?;
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| io_error("ssh.open_pseudoterminal_slave", "打开伪终端从端"))?;
        let mut command = Command::new(&program.path);
        command
            .args(&cmd.args)
            .env_clear()
            .envs(environment(cmd))
            .stdin(
                slave
                    .try_clone()
                    .map_err(|_| io_error("ssh.clone_pseudoterminal", "复制伪终端"))?,
            )
            .stdout(
                slave
                    .try_clone()
                    .map_err(|_| io_error("ssh.clone_pseudoterminal", "复制伪终端"))?,
            )
            .stderr(slave);
        // portable-pty 负责分配终端；自行启动避免其 Unix 启动器额外注入 SHELL，
        // 同时保留标准库的 exec 错误管道，使启动失败能正常返回错误。
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Box::new(
            command
                .spawn()
                .map_err(|_| io_error("ssh.start_fingerprint_probe", "启动指纹探测"))?,
        )
    };
    Ok(PtyChild {
        child,
        reaped: false,
        cleaned: false,
    })
}
#[cfg(windows)]
fn spawn_pty(
    program: &SshProgram,
    cmd: &SshCommand,
    master: &crate::terminal::windows::Master,
) -> Result<PtyChild, AppError> {
    let child = master
        .spawn_clean(&program.path, &cmd.args, &environment(cmd))
        .map_err(|_| io_error("ssh.start_fingerprint_probe", "启动指纹探测"))?;
    Ok(PtyChild {
        child,
        cleaned: false,
    })
}

struct ProbeState {
    accepted: bool,
    level: usize,
    diagnostic: Arc<Mutex<AppError>>,
}

fn interact(
    program: &SshProgram,
    cmd: &SshCommand,
    answers: &Receiver<Answer>,
    updates: &Sender<Result<Option<HostKeyInfo>, AppError>>,
    timeout: Duration,
    answer_timeout: Duration,
    state: &mut ProbeState,
) -> Result<(), AppError> {
    let size = PtySize {
        cols: 512,
        ..PtySize::default()
    };
    #[cfg(unix)]
    let pair = native_pty_system()
        .openpty(size)
        .map_err(|_| io_error("ssh.create_pseudoterminal", "创建伪终端"))?;
    #[cfg(windows)]
    let pair = crate::terminal::windows::open(size)
        .map_err(|_| io_error("ssh.create_pseudoterminal", "创建伪终端"))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|_| io_error("ssh.get_pseudoterminal_input", "获取伪终端输入"))?;
    let input = pair
        .master
        .try_clone_reader()
        .map_err(|_| io_error("ssh.get_pseudoterminal_output", "获取伪终端输出"))?;
    // 子进程附着时即可输出终端查询；先启动读取，避免启动阶段被输出管道反压。
    let mut reader = Reader::start(input, OUTPUT_LIMIT)?;
    #[cfg(unix)]
    let child = spawn_pty(program, cmd, &pair);
    #[cfg(windows)]
    let child = spawn_pty(program, cmd, &pair.master);
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            drop(writer);
            #[cfg(windows)]
            let _ = pair.master.shutdown_and_wait();
            drop(pair);
            reader.join();
            return Err(error);
        }
    };
    let pid = child
        .child
        .process_id()
        .map_or_else(|| "未知".into(), |pid| pid.to_string());
    state
        .diagnostic
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .params
        .insert("pid".into(), pid.clone());
    if child.child.process_id().is_none() {
        let mut diagnostic = state.diagnostic.lock().unwrap_or_else(|e| e.into_inner());
        *diagnostic = diagnostic
            .clone()
            .with_text("pid", "ssh.unknown_pid", "未知");
    }
    #[cfg(unix)]
    drop(pair.slave);
    let result = (|| {
        let mut since = Instant::now();
        let mut awaiting = false;
        let mut rejected = false;
        let mut answered = false;
        let mut pending = std::collections::VecDeque::<HostKeyInfo>::new();
        let mut queries = TerminalQueries::default();
        let mut scanned = 0;
        let mut consumed = 0;
        let mut exited = None;
        loop {
            // 只在复制快照时持有读锁；任何管道写入和进程查询都不能阻塞读取线程。
            let (bytes, done, failed, exceeded) = {
                let data = reader
                    .state
                    .lock()
                    .map_err(|_| io_error("ssh.read_fingerprint", "读取指纹"))?;
                (data.bytes.clone(), data.done, data.failed, data.exceeded)
            };
            let status = child.status()?;
            let replies = queries.replies(&bytes[scanned..]);
            scanned = bytes.len();
            let (phase_key, phase) = if awaiting {
                ("ssh.waiting_for_answer", "等待用户回答")
            } else if answered {
                ("ssh.waiting_for_verification", "等待回答后校验标记")
            } else {
                ("ssh.waiting_for_fingerprint", "等待指纹提问或已信任结果")
            };
            let mut diagnostic = timeout_diagnostic(&bytes, cmd)
                .with("pid", &pid)
                .with("exited", status.is_some())
                .with("reader_done", done)
                .with("reader_failed", failed)
                .with("cursor_queries", queries.cursor)
                .with("device_queries", queries.attributes)
                .with_text("phase", phase_key, phase);
            if child.child.process_id().is_none() {
                diagnostic = diagnostic.with_text("pid", "ssh.unknown_pid", "未知");
            }
            if let Some(status) = &status {
                diagnostic = diagnostic.with("exit_code", status.exit_code());
            }
            *state.diagnostic.lock().unwrap_or_else(|e| e.into_inner()) = diagnostic;
            if exceeded {
                return Err(AppError::new("remote.output_too_large"));
            }
            if failed {
                return Err(io_error("ssh.read_fingerprint", "读取指纹"));
            }
            // 隐藏探测没有 xterm.js；在首次提问、等用户操作及确认后都要回答查询。
            // 仅回复光标位置和最小终端属性，不代替用户确认任何指纹。
            if !replies.is_empty() {
                {
                    let mut diagnostic = state.diagnostic.lock().unwrap_or_else(|e| e.into_inner());
                    *diagnostic = diagnostic.clone().with_text(
                        "phase",
                        "ssh.answer_terminal_query",
                        "回答终端查询",
                    );
                }
                writer
                    .write_all(&replies)
                    .map_err(|_| io_error("ssh.answer_terminal_query", "回答终端查询"))?;
                writer.flush().map_err(|_| {
                    io_error("ssh.send_terminal_query_response", "发送终端查询回答")
                })?;
            }
            match answers.try_recv() {
                Ok(Answer::Confirm(yes)) if awaiting => {
                    {
                        let mut diagnostic =
                            state.diagnostic.lock().unwrap_or_else(|e| e.into_inner());
                        *diagnostic = diagnostic.clone().with_text(
                            "phase",
                            "ssh.send_host_key_response",
                            "发送主机密钥回答",
                        );
                    }
                    writer
                        .write_all(&answer_bytes(
                            &pending
                                .pop_front()
                                .ok_or_else(|| {
                                    io_error(
                                        "ssh.read_fingerprint_awaiting_confirmation",
                                        "读取待确认指纹",
                                    )
                                })?
                                .fingerprint,
                            yes,
                        ))
                        .map_err(|_| io_error("ssh.answer_host_key_prompt", "回答主机密钥"))?;
                    writer
                        .flush()
                        .map_err(|_| io_error("ssh.send_host_key_response", "发送主机密钥回答"))?;
                    rejected = !yes;
                    answered = true;
                    state.accepted |= yes;
                    awaiting = !pending.is_empty();
                    since = Instant::now();
                }
                Ok(Answer::Cancel) | Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(AppError::new("ssh.host_key_rejected"));
                }
                _ => {}
            }
            let normalized = normalize_terminal(&String::from_utf8_lossy(&bytes));
            let text = &normalized[consumed..];
            let prompt_end = text
                .find("Are you sure you want to continue connecting")
                .and_then(|start| text[start..].find('?').map(|end| start + end + 1));
            let current = prompt_end.map_or(text, |end| &text[..end]);
            if !rejected && let Some(mut info) = parse_prompt(current)? {
                state.level += 1;
                let level = state.level;
                if level > 64 {
                    return Err(AppError::new("ssh.host_key_prompt_invalid")
                        .with_reason("ssh.confirmation_limit_exceeded", "确认次数超过上限"));
                }
                info.level = level;
                if level > 1 {
                    // 只记录验证后的主机名和层级，不记录服务器输出或凭据。
                    eprintln!(
                        "主机密钥确认：第 {} 级，主机 {}，等待回答期间新增提问：{}",
                        level,
                        crate::security::redact(
                            &info.host,
                            &cmd.env
                                .iter()
                                .map(|(_, value)| value.as_str())
                                .collect::<Vec<_>>()
                        ),
                        awaiting
                    );
                }
                pending.push_back(info.clone());
                updates
                    .send(Ok(Some(info)))
                    .map_err(|_| io_error("ssh.return_fingerprint", "返回指纹"))?;
                // 保留完整原始字节供查询匹配、总量限制和超时诊断，只推进提示解析位置。
                consumed += prompt_end.unwrap_or(text.len());
                awaiting = true;
                since = Instant::now();
            }
            // 先处理指纹变化和待回答的各级提问，不能用标记越过用户确认或拒绝。
            // 先检查本轮收到的标记再判断超时，避免 ssh 卡在退出阶段时误报超时。
            if !rejected && !awaiting && has_success_marker(text) {
                return Ok(());
            }
            if since.elapsed() >= if awaiting { answer_timeout } else { timeout } {
                return Err(timeout_snapshot(&state.diagnostic));
            }
            if let Some(status) = status
                && (done
                    || exited.get_or_insert_with(Instant::now).elapsed()
                        >= Duration::from_millis(500))
            {
                if rejected {
                    return Err(AppError::new("ssh.host_key_rejected"));
                }
                let secrets: Vec<_> = cmd.env.iter().map(|(_, v)| v.as_str()).collect();
                let error = classify_error(Some(status.exit_code() as i32), text, &secrets);
                return Err(if status.success() {
                    error.with_reason(
                        "ssh.remote_fingerprint_verification_marker_was_not_received",
                        "未收到远端指纹校验标记",
                    )
                } else {
                    error
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    // 所有正常、拒绝、取消、超时和错误出口汇合于此，清理完成后才能通知调用方。
    let cleanup = child.terminate().map_err(|e| e.with("pid", &pid));
    drop(child);
    drop(writer);
    #[cfg(windows)]
    let cleanup = cleanup.and(pair.master.shutdown_and_wait().map_err(|_| {
        io_error(
            "ssh.wait_for_fingerprint_conpty_to_close",
            "等待指纹 ConPTY 关闭",
        )
        .with("pid", &pid)
    }));
    drop(pair.master);
    reader.join();
    finish_probe(result, cleanup, &pid)
}

fn finish_probe(
    result: Result<(), AppError>,
    cleanup: Result<(), AppError>,
    pid: &str,
) -> Result<(), AppError> {
    if let Err(error) = cleanup {
        // 只记录进程编号和固定错误码，不输出可能含服务器内容或凭据的错误参数。
        eprintln!("指纹探测清理失败：pid={pid}，错误码={}", error.code);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 只接受完整独立标记行而非命令回显() {
        assert_eq!(PROBE_COMMAND, format!("echo {SUCCESS_MARKER}"));
        for text in [
            format!("{SUCCESS_MARKER}\n"),
            format!("欢迎\r\n{SUCCESS_MARKER}\r\n"),
            format!("\x1b[32m{SUCCESS_MARKER}\x1b[0m\r\n后续输出"),
        ] {
            assert!(has_success_marker(&normalize_terminal(&text)), "{text:?}");
        }
        for text in [
            PROBE_COMMAND.into(),
            format!("{PROBE_COMMAND}\r\n"),
            format!("C:\\>ssh host {PROBE_COMMAND}\r\n"),
            format!("前缀{SUCCESS_MARKER}\n"),
            format!("{SUCCESS_MARKER}后缀\n"),
            format!(" {SUCCESS_MARKER}\n"),
            format!("{SUCCESS_MARKER} \n"),
            format!("\x1b]0;{SUCCESS_MARKER}\x07\r\n"),
        ] {
            assert!(!has_success_marker(&normalize_terminal(&text)), "{text:?}");
        }
        let line = format!("{SUCCESS_MARKER}\n");
        for split in 0..line.len() {
            assert!(!has_success_marker(&line[..split]));
            let mut received = line[..split].to_owned();
            received.push_str(&line[split..]);
            assert!(has_success_marker(&received));
        }
    }

    #[test]
    fn 清理失败不掩盖指纹校验成功() {
        assert!(finish_probe(Ok(()), Ok(()), "123").is_ok());
        for operation in ["结束并回收指纹进程树", "等待指纹 ConPTY 关闭"] {
            assert!(
                finish_probe(
                    Ok(()),
                    Err(io_error(
                        "ssh.terminate_and_reap_fingerprint_process_tree",
                        operation
                    )),
                    "123"
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn 清理失败保留原探测错误及参数() {
        for code in [
            "ssh.host_key_changed",
            "ssh.host_key_rejected",
            "ssh.host_key_timeout",
            "ssh.unreachable",
        ] {
            let original = AppError::new(code).with("phase", "等待回答后校验和退出");
            for cleanup in [
                Ok(()),
                Err(io_error(
                    "ssh.terminate_and_reap_fingerprint_process_tree",
                    "结束并回收指纹进程树",
                )),
            ] {
                let error = finish_probe(Err(original.clone()), cleanup, "123").unwrap_err();
                assert_eq!(error.code, original.code);
                assert_eq!(error.params, original.params);
            }
        }
    }

    #[test]
    fn 清理日志只含进程编号和错误码() {
        const MARKER: &str = "PIERLINK_TEST_CLEANUP_LOG";
        const SECRET: &str = "不可写入清理日志的秘密";
        if std::env::var_os(MARKER).is_some() {
            let cleanup = io_error(
                "ssh.terminate_and_reap_fingerprint_process_tree",
                "结束并回收指纹进程树",
            )
            .with("tail", SECRET);
            assert!(finish_probe(Ok(()), Err(cleanup), "123").is_ok());
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ssh::host_key::tests::清理日志只含进程编号和错误码",
                "--nocapture",
            ])
            .env(MARKER, "1")
            .output()
            .unwrap();
        assert!(output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("pid=123"));
        assert!(stderr.contains("错误码=ssh.io_failed"));
        assert!(!stderr.contains(SECRET));
        assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    }

    #[test]
    fn 终端查询跨块且只回答一次() {
        let mut queries = TerminalQueries::default();
        assert!(
            queries
                .replies("\x1b[31m普通输出\x1b[".as_bytes())
                .is_empty()
        );
        assert_eq!(queries.replies(b"6n\x1b[c\x1b[0"), b"\x1b[1;1R\x1b[?1;0c");
        assert_eq!(queries.replies(b"c\x1b[6n"), b"\x1b[?1;0c\x1b[1;1R");
        assert!(queries.replies(b"\x1b[?6n\x1b[5n\x1b[12;3R").is_empty());
        assert!(queries.replies(b"").is_empty());
        assert_eq!(queries.cursor, 2);
        assert_eq!(queries.attributes, 2);
        for split in 0..=4 {
            let mut queries = TerminalQueries::default();
            let mut replies = queries.replies(&b"\x1b[6n"[..split]);
            replies.extend(queries.replies(&b"\x1b[6n"[split..]));
            assert_eq!(replies, b"\x1b[1;1R");
        }
    }

    #[test]
    fn 超时诊断先规范化脱敏再按字符截尾() {
        let cmd = SshCommand {
            config_text: None,
            builtin: None,
            askpass: None,
            args: vec![],
            env: vec![("测试秘密".into(), "已知秘密".into())],
        };
        let text = format!(
            "\x1b[31m{}\x1b[0m\r\npassword={}\r\n-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n已知秘密\n尾部中文",
            "中".repeat(400),
            "不可泄漏".repeat(100),
            "私钥内容".repeat(100)
        );
        let error = timeout_diagnostic(text.as_bytes(), &cmd);
        assert_eq!(error.params["received_bytes"], text.len().to_string());
        let tail = &error.params["tail"];
        assert_eq!(tail.chars().count(), 300);
        assert!(tail.ends_with("***\n***\n***\n尾部中文"));
        for secret in ["不可泄漏", "私钥内容", "已知秘密", "\x1b", "\r"] {
            assert!(!tail.contains(secret), "诊断中残留敏感内容或控制序列");
        }
    }

    /// Windows 真实探测：在 CI（有网络）上用系统自带 ssh 连 github.com，必须在时限内拿到指纹提问。
    /// 这条覆盖“伪终端里收不到 ssh 输出、30 秒后超时”的问题（只在设置了 CI 环境变量时运行）。
    #[cfg(windows)]
    #[test]
    fn windows_真实探测能拿到指纹() {
        use crate::ssh::{
            args::{CommonOpts, Purpose, build_args},
            target::SshTarget,
        };
        if std::env::var_os("CI").is_none() {
            eprintln!("跳过：只在 CI 上运行（需要访问 github.com:22）");
            return;
        }
        let root = crate::test_support::tempdir();
        let target = SshTarget::Manual {
            host: "github.com".into(),
            port: 22,
            user: Some("git".into()),
            identity_file: None,
            jump: None,
        };
        let opts = CommonOpts {
            known_hosts: Some(root.path().join("known_hosts")),
            ..CommonOpts::default()
        };
        let mut cmd = build_args(&target, &Purpose::HostKeyProbe, &opts).unwrap();
        // CI 机器的全局 known_hosts 里预置了 github.com 的指纹，不忽略它就不会出现确认提问
        let global = root.path().join("global_known_hosts");
        cmd.args.splice(
            0..0,
            [
                "-o".to_owned(),
                format!("GlobalKnownHostsFile={}", global.display()),
            ],
        );
        // 系统自带的 ssh，以及安装包自带的 OpenSSH 10.0（CI 在测试前会下载到 src-tauri/resources）
        let system = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/OpenSSH/ssh.exe");
        let bundled = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../src-tauri/resources/openssh/ssh.exe");
        let mut tested = 0;
        for path in [system, bundled] {
            if !path.is_file() {
                eprintln!("跳过不存在的 ssh：{}", path.display());
                continue;
            }
            tested += 1;
            let program = SshProgram { path: path.clone() };
            let started = Instant::now();
            let prompt = probe(&program, &cmd)
                .unwrap_or_else(|e| panic!("{} 探测失败：{e:?}", path.display()));
            let HostKeyPrompt::Confirm(c) = prompt else {
                panic!("{}：全新的 known_hosts 应当要求确认指纹", path.display());
            };
            assert!(c.info.fingerprint.starts_with("SHA256:"), "{:?}", c.info);
            assert!(
                started.elapsed() < Duration::from_secs(25),
                "{} 探测太慢：{:?}",
                path.display(),
                started.elapsed()
            );
            // 拒绝后应当结束而不是挂住
            let err = HostKeyPrompt::Confirm(c).answer(false).err().unwrap();
            assert_eq!(err.code, "ssh.host_key_rejected", "{}", path.display());
        }
        assert!(tested > 0, "没有可用的 ssh");
    }
    #[test]
    fn 修订12_控制序列和换行规范化() {
        assert_eq!(
            normalize_terminal("\x1b[31m红\x1b[0m\r\n行\r尾\x1b]0;窗口\x07\x1b(B"),
            "红\n行\n尾"
        );
        assert_eq!(normalize_terminal("a\x1b]标题\x1b\\b"), "ab");
        assert_eq!(normalize_terminal("a\x1b[31"), "a");
        assert_eq!(
            answer_bytes("SHA256:abc", true),
            if cfg!(windows) {
                b"SHA256:abc\r\n".to_vec()
            } else {
                b"SHA256:abc\n".to_vec()
            }
        );
        assert_eq!(TIMEOUT, Duration::from_secs(30));
        assert_eq!(ANSWER_TIMEOUT, Duration::from_secs(90));
    }
    #[test]
    fn 手动跳板必须预先生成探测配置() {
        use crate::ssh::{
            args::{CommonOpts, Purpose, build_args},
            target::SshTarget,
        };
        let jump = SshTarget::Manual {
            host: "jump".into(),
            port: 22,
            user: None,
            identity_file: None,
            jump: None,
        };
        let target = SshTarget::Manual {
            host: "target".into(),
            port: 22,
            user: None,
            identity_file: None,
            jump: Some(Box::new(jump)),
        };
        let opts = CommonOpts {
            config_file: Some("config".into()),
            known_hosts: Some("hosts".into()),
            ..Default::default()
        };
        let background = build_args(&target, &Purpose::Background, &opts).unwrap();
        assert_eq!(
            probe_command(&background).unwrap_err().code,
            "ssh.host_key_probe_required"
        );
        let command = build_args(&target, &Purpose::HostKeyProbe, &opts).unwrap();
        let mut expected = command.clone();
        *expected.args.last_mut().unwrap() = PROBE_COMMAND.into();
        assert_eq!(probe_command(&command).unwrap(), expected);
        assert_eq!(
            command
                .config_text
                .as_ref()
                .unwrap()
                .matches("StrictHostKeyChecking ask")
                .count(),
            2
        );
        assert!(!command.config_text.as_ref().unwrap().contains("BatchMode"));
        let direct = SshCommand {
            args: vec!["host".into(), "/bin/sh -s".into()],
            env: vec![],
            config_text: None,
            builtin: None,
            askpass: None,
        };
        assert!(
            probe_command(&direct)
                .unwrap()
                .args
                .contains(&"StrictHostKeyChecking=ask".into())
        );
        assert_eq!(
            probe_command(&direct).unwrap().args.last().unwrap(),
            PROBE_COMMAND
        );
        assert!(
            probe_command(&SshCommand {
                args: vec![],
                ..direct
            })
            .is_err()
        );
    }
    #[test]
    fn 解析指纹及变化拒绝() {
        let text = format!(
            "The authenticity of host '[host]:2222 (127.0.0.1)' can't be established.\r\nED25519 key fingerprint is SHA256:{}.\r\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ",
            "a".repeat(43)
        );
        let info = parse_prompt(&text).unwrap().unwrap();
        assert_eq!(info.host, "[host]:2222 (127.0.0.1)");
        assert_eq!(info.key_type, "ED25519");
        assert_eq!(
            parse_prompt("REMOTE HOST IDENTIFICATION HAS CHANGED")
                .unwrap_err()
                .code,
            "ssh.host_key_changed"
        );
        assert!(parse_prompt("Are you sure you want to continue connecting?").is_err());
        assert!(parse_prompt("Password:").unwrap().is_none());
        assert!(matches!(
            HostKeyPrompt::Trusted.answer(true).unwrap(),
            HostKeyPrompt::Trusted
        ));
    }
}

#[cfg(all(test, unix))]
mod pty_tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn fake_ssh(root: &std::path::Path, body: &str) -> (SshProgram, SshCommand) {
        let path = root.join("fake-ssh");
        fs::write(
            &path,
            format!("#!/usr/bin/python3\nimport sys,time,os,tty\n{body}\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        (
            SshProgram { path },
            SshCommand {
                config_text: None,
                builtin: None,
                askpass: None,
                args: vec!["/bin/sh -s".into()],
                env: vec![],
            },
        )
    }

    #[test]
    fn 分块标记结束即成功并回收仍在运行的ssh() {
        let root = crate::test_support::tempdir();
        for confirm in [false, true] {
            let dir = tempfile::tempdir_in(root.path()).unwrap();
            let pid_file = dir.path().join("pid");
            let (program, mut cmd) = fake_ssh(
                dir.path(),
                &format!(
                    r#"
with open({pid_file:?}, 'w') as f: f.write(str(os.getpid()))
assert sys.argv[-1] == {PROBE_COMMAND:?}
if {confirm}:
 print("The authenticity of host 'target' can't be established.")
 print('ED25519 key fingerprint is SHA256:' + 'a'*43 + '.')
 print('Are you sure you want to continue connecting (yes/no/[fingerprint])? ', end='', flush=True)
 assert input() == 'SHA256:' + 'a'*43
print(sys.argv[-1], flush=True)
sys.stdout.write('\x1b[32mpierlink-hostkey-')
sys.stdout.flush()
time.sleep(0.05)
sys.stdout.write('ok\x1b[0m')
sys.stdout.flush()
time.sleep(0.05)
print('', flush=True)
time.sleep(30)
"#,
                    pid_file = pid_file.to_str().unwrap(),
                    confirm = if confirm { "True" } else { "False" },
                ),
            );
            // 同时覆盖现成 HostKeyProbe 参数与由后台用途转换的参数。
            if confirm {
                cmd.args = vec!["target".into(), "exit".into()];
            }
            let started = Instant::now();
            let prompt = probe_with_timeout(&program, &cmd, Duration::from_secs(2)).unwrap();
            let result = if confirm {
                assert!(matches!(&prompt, HostKeyPrompt::Confirm(_)));
                prompt.answer(true).unwrap()
            } else {
                prompt
            };
            assert!(matches!(result, HostKeyPrompt::Trusted));
            assert!(started.elapsed() < Duration::from_secs(1));
            let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "残留 PID {pid}");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
            assert_eq!(
                unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
    }

    #[test]
    fn 命令回显及不完整标记不能成功且保留错误分类() {
        let root = crate::test_support::tempdir();
        for (body, code) in [
            (
                "print(sys.argv[-1], flush=True)\ntime.sleep(30)",
                "ssh.host_key_timeout",
            ),
            (
                "sys.stdout.write('pierlink-hostkey-ok')\nsys.stdout.flush()\ntime.sleep(30)",
                "ssh.host_key_timeout",
            ),
            ("print(sys.argv[-1], flush=True)", "ssh.failed"),
            (
                "print(sys.argv[-1])\nprint('Permission denied (publickey).', flush=True)\nsys.exit(255)",
                "ssh.auth_failed",
            ),
            (
                "print(sys.argv[-1])\nprint('Host key verification failed.', flush=True)\nsys.exit(255)",
                "ssh.host_key_unknown",
            ),
            (
                "print('REMOTE HOST IDENTIFICATION HAS CHANGED')\nprint('pierlink-hostkey-ok', flush=True)\ntime.sleep(30)",
                "ssh.host_key_changed",
            ),
            (
                "print('REVOKED HOST KEY DETECTED')\nprint('pierlink-hostkey-ok', flush=True)\ntime.sleep(30)",
                "ssh.host_key_changed",
            ),
        ] {
            let (program, cmd) = fake_ssh(root.path(), body);
            let error = probe_with_timeout(&program, &cmd, Duration::from_secs(1))
                .err()
                .unwrap();
            assert_eq!(error.code, code, "{body}");
            if code == "ssh.host_key_timeout" {
                assert_eq!(error.params["exited"], "false");
                assert!(!error.params["tail"].is_empty());
            }
        }
    }

    #[test]
    fn 每种最终返回与取消都已结束并回收进程() {
        let root = crate::test_support::tempdir();
        let prompt = "print(\"The authenticity of host 'target' can't be established.\")\nprint('ED25519 key fingerprint is SHA256:' + 'a'*43 + '.')\nprint('Are you sure you want to continue connecting (yes/no/[fingerprint])? ', end='', flush=True)\n";
        for case in [
            "直接已信任",
            "确认",
            "标记后不退出",
            "信任后标记不退出",
            "拒绝",
            "用户超时",
            "取消",
            "信任后超时",
            "连接超时",
            "指纹变化",
            "输出超限",
            "信任后重试",
        ] {
            let dir = tempfile::tempdir_in(root.path()).unwrap();
            let pid_file = dir.path().join("pid");
            let body = match case {
                "直接已信任" => format!("print({SUCCESS_MARKER:?}, flush=True)"),
                "确认" => format!(
                    "{prompt}assert input() == 'SHA256:' + 'a'*43\nprint({SUCCESS_MARKER:?}, flush=True)"
                ),
                "标记后不退出" | "信任后标记不退出" => format!(
                    "{}print(sys.argv[-1], flush=True)\nassert sys.argv[-1] == {PROBE_COMMAND:?}\nprint({SUCCESS_MARKER:?}, flush=True)\ntime.sleep(30)",
                    if case == "信任后标记不退出" {
                        format!("{prompt}assert input() == 'SHA256:' + 'a'*43\n")
                    } else {
                        String::new()
                    }
                ),
                "拒绝" => format!(
                    "{prompt}assert input() == 'no'\nprint({SUCCESS_MARKER:?}, flush=True)\nsys.exit(255)"
                ),
                // 取消时故意不读取 no，验证不会遗留等待回答的工作线程。
                "用户超时" | "取消" => format!("{prompt}time.sleep(30)"),
                "信任后超时" => {
                    format!("{prompt}assert input() == 'SHA256:' + 'a'*43\ntime.sleep(30)")
                }
                "连接超时" => "time.sleep(30)".into(),
                "指纹变化" => {
                    "print('REMOTE HOST IDENTIFICATION HAS CHANGED', flush=True)\ntime.sleep(30)"
                        .into()
                }
                "输出超限" => "print('x'*70000, flush=True)\ntime.sleep(30)".into(),
                "信任后重试" => format!(
                    "marker={:?}\nif os.path.exists(marker):\n print({SUCCESS_MARKER:?}, flush=True)\n time.sleep(30)\n{prompt}assert input() == 'SHA256:' + 'a'*43\nwith open(marker, 'w'): pass\nprint('Connection closed', flush=True)\nsys.exit(255)",
                    dir.path().join("retried").to_str().unwrap()
                ),
                _ => unreachable!(),
            };
            let (program, cmd) = fake_ssh(
                dir.path(),
                &format!(
                    "with open({:?}, 'a') as f: f.write(str(os.getpid()) + '\\n')\n{body}",
                    pid_file.to_str().unwrap()
                ),
            );
            let probe = || {
                probe_with_limits(
                    &program,
                    &cmd,
                    Duration::from_millis(300),
                    Duration::from_millis(300),
                )
            };
            let result = match case {
                "确认" | "信任后超时" | "信任后重试" | "信任后标记不退出" => {
                    probe().unwrap().answer(true)
                }
                "拒绝" => probe().unwrap().answer(false),
                "用户超时" => {
                    let prompt = probe().unwrap();
                    thread::sleep(Duration::from_millis(450));
                    prompt.answer(true)
                }
                "取消" => {
                    let prompt = probe_with_limits(
                        &program,
                        &cmd,
                        Duration::from_secs(30),
                        Duration::from_secs(90),
                    )
                    .unwrap();
                    let started = Instant::now();
                    drop(prompt);
                    assert!(started.elapsed() < Duration::from_secs(1));
                    Ok(HostKeyPrompt::Trusted)
                }
                _ => probe(),
            };
            let pids: Vec<i32> = fs::read_to_string(&pid_file)
                .unwrap()
                .lines()
                .map(|pid| pid.parse().unwrap())
                .collect();
            let expected = match case {
                "拒绝" => Some("ssh.host_key_rejected"),
                "用户超时" | "信任后超时" | "连接超时" => Some("ssh.host_key_timeout"),
                "指纹变化" => Some("ssh.host_key_changed"),
                "输出超限" => Some("remote.output_too_large"),
                _ => None,
            };
            if let Some(code) = expected {
                let error = result.err().unwrap();
                assert_eq!(error.code, code, "{case}");
                if code == "ssh.host_key_timeout" {
                    assert_eq!(error.params["pid"], pids[0].to_string(), "{case}");
                }
            } else {
                assert!(matches!(result.unwrap(), HostKeyPrompt::Trusted), "{case}");
            }
            assert_eq!(pids.len(), if case == "信任后重试" { 2 } else { 1 });
            for pid in pids {
                assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "{case}残留 PID {pid}");
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ESRCH)
                );
                assert_eq!(
                    unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
                    -1
                );
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ECHILD)
                );
            }
        }
    }

    #[test]
    fn 提问前等待用户及信任后持续回答终端查询() {
        let root = crate::test_support::tempdir();
        let (program, cmd) = fake_ssh(
            root.path(),
            r#"
tty.setraw(0)
def query(request, reply):
 for byte in request:
  os.write(1, bytes([byte]))
  time.sleep(0.01)
 received = b''
 while len(received) < len(reply): received += os.read(0, 1)
 assert received == reply, repr(received)
query(b'\x1b[c', b'\x1b[?1;0c')
query(b'\x1b[6n', b'\x1b[1;1R')
os.write(1, ("The authenticity of host 'target' can't be established.\r\nED25519 key fingerprint is SHA256:" + 'a'*43 + ".\r\nAre you sure you want to continue connecting (yes/no/[fingerprint])? \x1b[6").encode())
time.sleep(0.04)
os.write(1, b'n')
assert os.read(0, 6) == b'\x1b[1;1R'
answer = b''
while not answer.endswith(b'\n'): answer += os.read(0, 1)
assert answer.strip() == b'SHA256:' + b'a'*43
query(b'\x1b[6n', b'\x1b[1;1R')
query(b'\x1b[0c', b'\x1b[?1;0c')
os.write(1, b'\r\npierlink-hostkey-ok\r\n')
"#,
        );
        let prompt = probe_with_limits(
            &program,
            &cmd,
            Duration::from_secs(2),
            Duration::from_secs(3),
        )
        .unwrap();
        assert!(matches!(&prompt, HostKeyPrompt::Confirm(c) if c.info.level == 1));
        thread::sleep(Duration::from_millis(150));
        assert!(matches!(
            prompt.answer(true).unwrap(),
            HostKeyPrompt::Trusted
        ));
    }

    #[test]
    fn 首次及信任后超时含原始累计字节和读进程状态() {
        let root = crate::test_support::tempdir();
        let (program, cmd) = fake_ssh(root.path(), "print('等待连接', flush=True)\ntime.sleep(30)");
        let error = probe_with_timeout(&program, &cmd, Duration::from_millis(300))
            .err()
            .unwrap();
        assert_eq!(error.code, "ssh.host_key_timeout");
        assert_eq!(
            error.params["received_bytes"],
            "等待连接\r\n".len().to_string()
        );
        assert_eq!(error.params["exited"], "false");
        assert_eq!(error.params["reader_done"], "false");
        assert_eq!(error.params["tail"], "等待连接\n");
        assert_eq!(error.params["phase"], "等待指纹提问或已信任结果");
        let (program, cmd) = fake_ssh(
            root.path(),
            r#"
print("The authenticity of host 'target' can't be established.")
print('ED25519 key fingerprint is SHA256:' + 'a'*43 + '.')
print('Are you sure you want to continue connecting (yes/no/[fingerprint])? ', end='', flush=True)
assert input() == 'SHA256:' + 'a'*43
print('确认后继续输出 password=不可泄漏', flush=True)
time.sleep(30)
"#,
        );
        let prompt = probe_with_limits(
            &program,
            &cmd,
            Duration::from_millis(300),
            Duration::from_secs(2),
        )
        .unwrap();
        let error = prompt.answer(true).err().unwrap();
        assert_eq!(error.code, "ssh.host_key_timeout");
        assert!(error.params["received_bytes"].parse::<usize>().unwrap() > 250);
        assert_eq!(error.params["exited"], "false");
        assert_eq!(error.params["reader_done"], "false");
        assert_eq!(error.params["phase"], "等待回答后校验标记");
        assert!(error.params["tail"].contains("Are you sure"));
        assert!(error.params["tail"].contains("确认后继续输出 ***"));
        assert!(!error.params["tail"].contains("不可泄漏"));
        // 用户迟答导致工作线程已经退出时，发送失败也必须保留同一份诊断。
        let prompt = probe_with_timeout(&program, &cmd, Duration::from_millis(100)).unwrap();
        thread::sleep(Duration::from_millis(200));
        let error = prompt.answer(true).err().unwrap();
        assert_eq!(error.params["phase"], "等待用户回答");
        assert!(error.params["tail"].contains("Are you sure"));
    }
    #[test]
    fn 真实伪终端多级确认_拒绝_超时_取消() {
        let root = crate::test_support::tempdir();
        let path = root.path().join("fake-ssh");
        let cmd = SshCommand {
            config_text: None,
            builtin: None,
            askpass: None,
            args: vec!["/bin/sh -s".into()],
            env: vec![],
        };
        let program = SshProgram { path: path.clone() };
        let install = |body: &str| {
            fs::write(
                &path,
                format!("#!/usr/bin/python3\nimport sys,time,os\nassert os.get_terminal_size().columns == 512\n{body}\n"),
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        };
        install(
            "for host in ['jump','target']:\n print(\"The authenticity of host '%s' can't be established.\" % host)\n print('ED25519 key fingerprint is SHA256:' + 'a'*43 + '.')\n print('Are you sure you want to continue connecting (yes/no/[fingerprint])? ', end='', flush=True)\n if input() != 'SHA256:' + 'a'*43: sys.exit(255)\nprint('pierlink-hostkey-ok', flush=True)",
        );
        let p = probe(&program, &cmd).unwrap();
        assert!(
            matches!(&p, HostKeyPrompt::Confirm(c) if c.info.host == "jump" && c.info.level == 1)
        );
        let p = p.answer(true).unwrap();
        assert!(
            matches!(&p, HostKeyPrompt::Confirm(c) if c.info.host == "target" && c.info.level == 2)
        );
        assert!(matches!(p.answer(true).unwrap(), HostKeyPrompt::Trusted));
        let p = probe(&program, &cmd).unwrap();
        assert_eq!(p.answer(false).err().unwrap().code, "ssh.host_key_rejected");
        let p = probe_with_limits(
            &program,
            &cmd,
            Duration::from_millis(100),
            Duration::from_secs(1),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(200));
        assert!(matches!(
            p.answer(true).unwrap().answer(true).unwrap(),
            HostKeyPrompt::Trusted
        ));
        let p = probe_with_timeout(&program, &cmd, Duration::from_millis(150)).unwrap();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(p.answer(true).err().unwrap().code, "ssh.host_key_timeout");
        let p = probe(&program, &cmd).unwrap();
        let before = Instant::now();
        drop(p);
        assert!(before.elapsed() < Duration::from_secs(1));
        install("print('REMOTE HOST IDENTIFICATION HAS CHANGED', flush=True)\nsys.exit(255)");
        assert_eq!(
            probe(&program, &cmd).err().unwrap().code,
            "ssh.host_key_changed"
        );
        install("time.sleep(30)");
        assert_eq!(
            probe_with_timeout(&program, &cmd, Duration::from_millis(100))
                .err()
                .unwrap()
                .code,
            "ssh.host_key_timeout"
        );
        install("print('x'*70000, flush=True)");
        assert_eq!(
            probe(&program, &cmd).err().unwrap().code,
            "remote.output_too_large"
        );
        let marker = root.path().join("retried");
        install(&format!(
            "marker={:?}\nif os.path.exists(marker):\n print({SUCCESS_MARKER:?}, flush=True)\n sys.exit(0)\nprint(\"The authenticity of host 'target' can't be established.\")\nprint('ED25519 key fingerprint is SHA256:' + 'a'*43 + '.')\nprint('Are you sure you want to continue connecting (yes/no/[fingerprint])? ', end='', flush=True)\nassert input() == 'SHA256:' + 'a'*43\nopen(marker,'w').close()\nprint('Connection closed', flush=True)\nsys.exit(255)",
            marker.to_str().unwrap()
        ));
        assert!(matches!(
            probe(&program, &cmd).unwrap().answer(true).unwrap(),
            HostKeyPrompt::Trusted
        ));
        install(
            "for host,fp in [('jump','a'),('target','b')]:\n print(\"The authenticity of host '%s' can't be established.\" % host)\n print('ED25519 key fingerprint is SHA256:' + fp*43 + '.')\n print('Are you sure you want to continue connecting (yes/no/[fingerprint])? ', end='', flush=True)\nassert input() == 'SHA256:' + 'a'*43\nassert input() == 'SHA256:' + 'b'*43\nprint('pierlink-hostkey-ok', flush=True)",
        );
        let first = probe(&program, &cmd).unwrap();
        thread::sleep(Duration::from_millis(50));
        let second = first.answer(true).unwrap();
        assert!(
            matches!(&second, HostKeyPrompt::Confirm(c) if c.info.level == 2 && c.info.host == "target" && c.info.fingerprint.ends_with(&"b".repeat(43)))
        );
        assert!(matches!(
            second.answer(true).unwrap(),
            HostKeyPrompt::Trusted
        ));
        install("print('pierlink-hostkey-ok', flush=True)");
        assert!(matches!(
            probe(&program, &cmd).unwrap(),
            HostKeyPrompt::Trusted
        ));
    }
}
