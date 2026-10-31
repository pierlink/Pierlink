//! 原始字节终端会话；输出最多保留 1 MiB 未确认数据，回调不得阻塞。
use crate::{error::AppError, ssh::args::SshCommand};
use portable_pty::PtySize;
#[cfg(unix)]
type MasterHandle = Box<dyn portable_pty::MasterPty + Send>;
#[cfg(unix)]
type Pair = portable_pty::PtyPair;
// 专用密钥生成（keys::local）也用这套 ConPTY 启动方式，所以对 crate 内可见
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
type MasterHandle = Box<windows::Master>;
#[cfg(windows)]
type Pair = windows::Pair;
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
mod builtin;
mod busy;
mod input;
mod output;
pub(crate) mod progress;
mod ssh_log;
pub use builtin::{BuiltinTerminal, remote};
pub use input::InputSource;
pub use output::{OutputRead, OutputSubscription, SubscriptionEvent};
pub use progress::{ConnectProgress, ConnectStage};
mod child;
#[cfg(test)]
mod tests;

pub enum TerminalKind {
    BuiltinRemote(Box<BuiltinTerminal>),
    Local {
        program: PathBuf,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        env: Vec<(String, String)>,
    },
    Remote {
        ssh: PathBuf,
        command: SshCommand,
        config_dir: PathBuf,
    },
}
pub struct TerminalSpec {
    pub kind: TerminalKind,
    pub cols: u16,
    pub rows: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum TerminalEvent {
    Stage(ConnectStage),
    Output(Vec<u8>),
    Exited {
        /// 远程退出码；内置 SSH 连接断开时为 255，与系统 SSH 一致。
        code: Option<i32>,
        closed_by_user: bool,
        /// 本次会话是否使用带提示符钩子的命令启动；本机终端为 false。
        hooked: bool,
    },
}
const CHUNK: usize = 64 * 1024;
const WINDOW: usize = 1024 * 1024;
type Callback = Arc<dyn Fn(TerminalEvent) + Send + Sync>;
type Sessions = Arc<SessionStore>;
#[derive(Default)]
struct SessionStore(Mutex<HashMap<String, Arc<Session>>>);
impl std::ops::Deref for SessionStore {
    type Target = Mutex<HashMap<String, Arc<Session>>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Drop for SessionStore {
    fn drop(&mut self) {
        // 最后一个管理器释放时才关闭会话；MCP 释放自己的克隆不能结束用户终端。
        for session in std::mem::take(&mut *lock(&self.0)).into_values() {
            session.stop();
        }
    }
}
struct Flow {
    pending: usize,
    reading: usize,
    stopped: bool,
    closed: bool,
    last_ack: Instant,
}
impl Default for Flow {
    fn default() -> Self {
        Self {
            pending: 0,
            reading: 0,
            stopped: false,
            closed: false,
            last_ack: Instant::now(),
        }
    }
}
struct Session {
    hooked: bool,
    flow: Mutex<Flow>,
    wake: Condvar,
    master: Mutex<(Option<MasterHandle>, u16, u16)>,
    input: Mutex<input::Input>,
    output: Mutex<output::Output>,
    child: Mutex<builtin::Process>,
}
fn error(code: &str, text_key: &'static str, operation: &str) -> AppError {
    AppError::new(format!("terminal.{code}")).with_text("operation", text_key, operation)
}
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
fn size(cols: u16, rows: u16) -> Result<PtySize, AppError> {
    if cols == 0 || rows == 0 {
        return Err(error(
            "invalid_size",
            "terminal.terminal_dimensions_must_be_greater_than_zero",
            "终端尺寸必须大于零",
        ));
    }
    Ok(PtySize {
        cols,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    })
}
/// 克隆得到的管理器与原来的共享同一张会话表（便于把关闭操作交给后台线程）。
#[derive(Default, Clone)]
pub struct TerminalManager {
    sessions: Sessions,
}
impl TerminalManager {
    /// 无参数；返回空的会话管理器。
    pub fn new() -> Self {
        Self::default()
    }
    /// 按 spec 打开会话并返回 UUID；输出在后台回调，阶段也可能在打开期间同步回调，必须快速返回，可重入管理器。
    /// 输出保持字节及顺序；自然退出排空后发送 Exited，Linux 最多再排空两秒，背压期间仍须 ack。
    /// 主动关闭会放弃尚未读取的输出。回调可能早于 open 返回，调用方须先建立接收通道。
    /// 调用方在页面重载、窗口关闭时必须 close_all。满额输出超过五分钟没有 ack 将主动关闭。
    pub fn open(&self, spec: TerminalSpec, on_event: Callback) -> Result<String, AppError> {
        self.open_with_registration(spec, on_event)
            .map(|(id, _)| id)
    }
    /// 按 spec 启动终端并回调 on_event，返回会话 ID 和本次实际 askpass 守卫；失败时返回脱敏错误。
    /// 调用方持有守卫至 Exited 后，按终端输出归类错误并调用 finish_error，获得密码错误码和作废结果。
    pub fn open_with_registration(
        &self,
        spec: TerminalSpec,
        on_event: Callback,
    ) -> Result<(String, Option<Arc<crate::ssh::askpass_pipe::Registration>>), AppError> {
        let dimensions = size(spec.cols, spec.rows)?;
        if let TerminalKind::BuiltinRemote(terminal) = spec.kind {
            let progress = terminal.progress(on_event.clone());
            let (mut channel, hooked) = terminal.open(spec.cols, spec.rows, progress.clone())?;
            let (writer, reader, stderr) = channel.take_io();
            drop(stderr);
            let session = Arc::new(Session {
                hooked,
                flow: Mutex::new(Flow::default()),
                wake: Condvar::new(),
                master: Mutex::new((None, spec.cols, spec.rows)),
                input: Mutex::new(input::Input::new(false)),
                output: Mutex::new(output::Output::new(spec.cols, spec.rows)),
                child: Mutex::new(builtin::Process::Builtin(Box::new(channel))),
            });
            let id = self.start(
                session,
                Box::new(reader),
                Box::new(writer),
                on_event,
                None,
                Some(progress),
            )?;
            return Ok((id, None));
        }
        #[cfg(unix)]
        let pair = portable_pty::native_pty_system()
            .openpty(dimensions)
            .map_err(|_| {
                error(
                    "spawn_failed",
                    "terminal.create_pseudoterminal",
                    "创建伪终端",
                )
            })?;
        #[cfg(windows)]
        let pair = windows::open(dimensions)?;
        #[cfg(unix)]
        if let Some(fd) = pair.master.as_raw_fd() {
            // 克隆的读写句柄共享非阻塞标志，线程可定期检查关闭状态。
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                if flags == -1 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
                    return Err(error(
                        "spawn_failed",
                        "terminal.set_pseudoterminal_nonblocking_mode",
                        "设置伪终端非阻塞模式",
                    ));
                }
            }
        }
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|_| error("spawn_failed", "terminal.open_output", "打开输出"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|_| error("spawn_failed", "terminal.open_input", "打开输入"))?;
        let hooked = matches!(&spec.kind, TerminalKind::Remote { command, .. } if command.hooked);
        let mut process = child::spawn(spec.kind, &pair)?;
        let registration = process.askpass.clone();
        let log = process.log.take();
        #[cfg(unix)]
        drop(pair.slave);
        let session = Arc::new(Session {
            hooked,
            flow: Mutex::new(Flow::default()),
            wake: Condvar::new(),
            master: Mutex::new((Some(pair.master), spec.cols, spec.rows)),
            input: Mutex::new(input::Input::new(cfg!(windows))),
            output: Mutex::new(output::Output::new(spec.cols, spec.rows)),
            child: Mutex::new(builtin::Process::Local(process)),
        });
        self.start(session, reader, writer, on_event, log, None)
            .map(|id| (id, registration))
    }
    fn start(
        &self,
        session: Arc<Session>,
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        on_event: Callback,
        log: Option<ssh_log::Log>,
        ready: Option<ConnectProgress>,
    ) -> Result<String, AppError> {
        let writing = session.clone();
        if thread::Builder::new()
            .name("终端输入".into())
            .spawn(move || input::write_input(writer, writing))
            .is_err()
        {
            session.stop();
            return Err(error(
                "spawn_failed",
                "terminal.create_input_thread",
                "创建输入线程",
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        // 有界管道中的字节也计入 pending；没有额外无界输出队列。
        let (tx, rx) = mpsc::sync_channel(2);
        let reading = session.clone();
        let reader_thread = thread::Builder::new()
            .name("终端读取".into())
            .spawn(move || read_output(reader, reading, tx))
            .map_err(|_| {
                session.stop();
                error(
                    "spawn_failed",
                    "terminal.create_reader_thread",
                    "创建读取线程",
                )
            })?;
        let (started, start) = mpsc::channel();
        let active = session.clone();
        let sessions = Arc::downgrade(&self.sessions);
        let key = id.clone();
        lock(&self.sessions).insert(id.clone(), session.clone());
        if thread::Builder::new()
            .name("终端事件".into())
            .spawn(move || {
                // Ready 先于输出和退出；线程创建失败时不报告会话可用。
                let _ = start.recv();
                dispatch(&active, rx, on_event, log);
                // 先清理资源再移除条目；不会保留已退出会话的历史。
                lock(&active.child).finish();
                if let Some(sessions) = sessions.upgrade() {
                    lock(&sessions).remove(&key);
                }
                crate::process::join_bounded(reader_thread, Duration::from_secs(2));
            })
            .is_err()
        {
            lock(&self.sessions).remove(&id);
            session.stop();
            return Err(error(
                "spawn_failed",
                "terminal.create_event_thread",
                "创建事件线程",
            ));
        }
        if let Some(ready) = ready {
            ready(ConnectStage::Ready);
        }
        let _ = started.send(());
        Ok(id)
    }
    fn get(&self, id: &str) -> Result<Arc<Session>, AppError> {
        lock(&self.sessions).get(id).cloned().ok_or_else(|| {
            error(
                "not_found",
                "terminal.session_is_missing_or_has_ended",
                "会话不存在或已结束",
            )
        })
    }
    /// 将 data 原样加入 id 的输入队列并立即返回；含正在写入的数据最多 1 MiB，超额返回 input_overflow。
    /// 成功表示已入队；后台写入失败后后续调用返回 write_failed，不记录输入内容。
    /// ConPTY 使用过 Windows 按键格式后，写入时将末尾 Esc 转为对应按键事件；容量按原始字节计算。
    pub fn write(&self, id: &str, data: &[u8]) -> Result<(), AppError> {
        self.write_from(id, data, InputSource::User)
    }
    /// 立即设置 id 的列数和行数；拒绝零尺寸，重复尺寸不下发。
    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), AppError> {
        let dimensions = size(cols, rows)?;
        let session = self.get(id)?;
        let mut master = lock(&session.master);
        if (master.1, master.2) != (cols, rows) {
            if let Some(pty) = &master.0 {
                pty.resize(dimensions).map_err(|_| {
                    error("resize_failed", "terminal.resize_terminal", "调整终端尺寸")
                })?;
            } else {
                lock(&session.child).resize(cols, rows)?;
            }
            lock(&session.output).resize(cols, rows);
            master.1 = cols;
            master.2 = rows;
        }
        Ok(())
    }
    /// 查询 id 的本机 shell 是否有直接子进程；远程终端、进程已结束或系统查询失败返回 None。
    /// 不存在的 id 返回 terminal.not_found；Windows 忽略控制台宿主并校验子进程创建时间。
    pub fn busy(&self, id: &str) -> Result<Option<bool>, AppError> {
        let session = self.get(id)?;
        let result = lock(&session.child).busy();
        Ok(result)
    }
    /// 确认 id 已消费的 bytes 字节并恢复读取；超额确认报错。已移除会话的迟到确认无操作。
    pub fn ack(&self, id: &str, bytes: usize) -> Result<(), AppError> {
        let session = { lock(&self.sessions).get(id).cloned() };
        let Some(session) = session else {
            return Ok(());
        };
        let mut flow = lock(&session.flow);
        if bytes > flow.pending {
            return Err(error(
                "invalid_ack",
                "terminal.acknowledged_byte_count_exceeds_pending_bytes",
                "确认字节数超过未确认数",
            ));
        }
        flow.pending -= bytes;
        flow.last_ack = Instant::now();
        session.wake.notify_all();
        Ok(())
    }
    /// 结束 id 的进程树并回收直接子进程；不存在或重复关闭均成功。
    pub fn close(&self, id: &str) -> Result<(), AppError> {
        let session = { lock(&self.sessions).get(id).cloned() };
        if let Some(session) = session {
            session.stop();
        }
        Ok(())
    }
    /// 无参数、无返回值；关闭调用时存在的全部会话。
    pub fn close_all(&self) {
        let sessions: Vec<_> = lock(&self.sessions).values().cloned().collect();
        for session in sessions {
            session.stop();
        }
    }
}
impl Session {
    fn stop_if_stale(&self, now: Instant) {
        let stale = {
            let flow = lock(&self.flow);
            flow.pending + flow.reading >= WINDOW
                && now.saturating_duration_since(flow.last_ack) >= Duration::from_secs(300)
        };
        if stale {
            self.stop();
        }
    }

    fn stop(&self) {
        {
            let mut flow = lock(&self.flow);
            flow.closed = true;
            flow.stopped = true;
        }
        self.wake.notify_all();
        lock(&self.child).terminate();
        #[cfg(windows)]
        if let Some(master) = &lock(&self.master).0 {
            master.shutdown();
        }
    }
}
fn read_output(
    mut reader: Box<dyn Read + Send>,
    session: Arc<Session>,
    tx: mpsc::SyncSender<Vec<u8>>,
) {
    loop {
        {
            let mut flow = lock(&session.flow);
            while flow.pending >= WINDOW && !flow.stopped {
                flow = session.wake.wait(flow).unwrap_or_else(|e| e.into_inner());
            }
            if flow.stopped {
                #[cfg(windows)]
                {
                    drop(flow);
                    let _ = std::io::copy(&mut reader, &mut std::io::sink());
                }
                break;
            }
        }
        #[cfg(unix)]
        {
            let fd = lock(&session.master).0.as_ref().and_then(|m| m.as_raw_fd());
            if let Some(fd) = fd {
                let mut poll = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // 有输出立即唤醒；空闲时最多每 200 毫秒检查一次关闭状态。
                if unsafe { libc::poll(&mut poll, 1, 200) } <= 0 {
                    continue;
                }
            }
        }
        let capacity = {
            let mut flow = lock(&session.flow);
            let capacity = WINDOW.saturating_sub(flow.pending).min(CHUNK);
            if capacity == 0 || flow.stopped {
                continue;
            }
            // 为即将读取的字节保留窗口，日志回显不能占用这部分容量。
            flow.reading = capacity;
            capacity
        };
        let mut buffer = vec![0; capacity];
        let result = reader.read(&mut buffer);
        {
            let mut flow = lock(&session.flow);
            flow.reading = 0;
            if let Ok(n) = &result {
                flow.pending += n;
            }
        }
        match result {
            Ok(0) => break,
            Ok(n) => {
                buffer.truncate(n);
                if tx.send(buffer).is_err() {
                    #[cfg(windows)]
                    let _ = std::io::copy(&mut reader, &mut std::io::sink());
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let flow = lock(&session.flow);
                if !flow.stopped {
                    let _ = session.wake.wait_timeout(flow, Duration::from_millis(100));
                }
            }
            Err(_) => break,
        }
    }
}
fn dispatch(
    session: &Session,
    rx: mpsc::Receiver<Vec<u8>>,
    callback: Callback,
    mut log: Option<ssh_log::Log>,
) {
    if log.is_some() {
        session.emit(&callback, TerminalEvent::Stage(ConnectStage::Resolve));
    }
    let mut ended = false;
    let mut exited_at = None;
    loop {
        let mut output = Vec::new();
        match rx.recv_timeout(Duration::from_millis(if log.is_some() { 50 } else { 200 })) {
            Ok(chunk) => output = chunk,
            Err(mpsc::RecvTimeoutError::Disconnected) => ended = true,
            Err(mpsc::RecvTimeoutError::Timeout) => (),
        }
        let start = Instant::now();
        while !output.is_empty()
            && output.len() < CHUNK
            && start.elapsed() < Duration::from_millis(4)
        {
            // 每次回调最多 64 KiB；不能预取一个无法装入的块。
            match rx.recv_timeout(Duration::from_millis(4).saturating_sub(start.elapsed())) {
                Ok(chunk) => {
                    if output.len() + chunk.len() > CHUNK {
                        session.emit(
                            &callback,
                            TerminalEvent::Output(std::mem::take(&mut output)),
                        );
                    }
                    output.extend(chunk);
                }
                Err(_) => break,
            }
        }
        if !output.is_empty() {
            session.emit(&callback, TerminalEvent::Output(output));
        }
        session.stop_if_stale(Instant::now());
        let status = lock(&session.child).poll();
        if status.is_some() {
            exited_at.get_or_insert_with(Instant::now);
        }
        let drain_expired = cfg!(target_os = "linux")
            && exited_at.is_some_and(|t| t.elapsed() >= Duration::from_secs(2));
        #[cfg(windows)]
        if status.is_some()
            && let Some(master) = &lock(&session.master).0
        {
            master.shutdown();
        }
        let closed = lock(&session.flow).closed;
        let log_drained = log
            .as_mut()
            .is_none_or(|log| log.pump(session, &callback, status.is_some()));
        if status.is_some() && (ended || closed || drain_expired) && log_drained {
            // 进程已结束且日志已排空，先删除日志再通知退出。
            drop(log.take());
            lock(&session.flow).stopped = true;
            session.wake.notify_all();
            lock(&session.child).finish();
            // 已排队输出先于唯一退出事件；超时后放弃尚未读取的输出。
            while let Ok(chunk) = rx.try_recv() {
                session.emit(&callback, TerminalEvent::Output(chunk));
            }
            session.emit(
                &callback,
                TerminalEvent::Exited {
                    code: status.flatten(),
                    closed_by_user: closed,
                    hooked: session.hooked,
                },
            );
            break;
        }
        if ended && (status.is_none() || !log_drained) {
            // 读取端已关闭时 recv 不再阻塞；日志背压或等待进程退出仍须让出线程。
            thread::sleep(Duration::from_millis(if log.is_some() { 50 } else { 200 }));
        }
    }
}
fn emit(callback: &Callback, event: TerminalEvent) {
    // 调用方异常不能使会话资源无人清理。
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(event)));
}

/// 应用启动、打开任何终端之前调用；删除 config_dir 内残留的 terminal-*.conf 和 terminal-*.log，返回删除数量。
/// 目录不存在返回零；不会递归删除子目录，失败返回 config_failed。活动会话使用的目录不可调用。
pub fn cleanup_stale_configs(config_dir: &std::path::Path) -> Result<usize, AppError> {
    let entries = match std::fs::read_dir(config_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(_) => {
            return Err(error(
                "config_failed",
                "terminal.read_temporary_configuration_directory",
                "读取临时配置目录",
            ));
        }
    };
    let mut count = 0;
    for entry in entries {
        let entry = entry.map_err(|_| {
            error(
                "config_failed",
                "terminal.read_temporary_configuration_entry",
                "读取临时配置项",
            )
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("terminal-")
            && (name.ends_with(".conf") || name.ends_with(".log"))
            && entry
                .file_type()
                .map_err(|_| {
                    error(
                        "config_failed",
                        "terminal.read_configuration_type",
                        "读取配置类型",
                    )
                })?
                .is_file()
        {
            std::fs::remove_file(entry.path()).map_err(|_| {
                error(
                    "config_failed",
                    "terminal.remove_leftover_configuration",
                    "删除残留配置",
                )
            })?;
            count += 1;
        }
    }
    Ok(count)
}
