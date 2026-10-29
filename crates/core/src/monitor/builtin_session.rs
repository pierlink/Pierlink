//! 内置采样的输入、输出分别处理；变更间隔只写入已有通道。
use super::*;
use crate::remote::command::BuiltinCommand;
use std::sync::atomic::{AtomicBool, Ordering};

pub(in crate::monitor) struct BuiltinSession {
    lines: Arc<Mutex<Lines>>,
    finished: Arc<Mutex<Option<AppError>>>,
    cancelled: Arc<AtomicBool>,
    updates: Arc<InputUpdates>,
    worker: Option<JoinHandle<()>>,
}
impl BuiltinSession {
    pub(super) fn start(mut command: BuiltinCommand, script: String) -> Result<Self, AppError> {
        command.interactive = false;
        let script = lifetime_script(&script)?;
        let lines = Arc::new(Mutex::new(Lines::default()));
        let finished = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(AtomicBool::new(false));
        let updates = Arc::new(InputUpdates::default());
        let shared = lines.clone();
        let done = finished.clone();
        let cancel = cancelled.clone();
        let input = updates.clone();
        let worker = thread::Builder::new()
            .name("内置资源采样".into())
            .spawn(move || {
                let error = run_channel(command, script, shared.clone(), cancel, input)
                    .err()
                    .unwrap_or_else(|| AppError::new("ssh.unreachable"));
                shared.lock().unwrap_or_else(|p| p.into_inner()).done = true;
                *done.lock().unwrap_or_else(|p| p.into_inner()) = Some(error);
            })
            .map_err(|_| io_error("创建内置采样线程"))?;
        Ok(Self {
            lines,
            finished,
            cancelled,
            updates,
            worker: Some(worker),
        })
    }
    pub(super) fn set_interval(&self, interval: Duration) {
        self.updates.set(crate::monitor::interval_line(interval));
    }
    pub(super) fn frames(&self) -> Result<Vec<Frame>, AppError> {
        let mut state = self.lines.lock().map_err(|_| io_error("读取资源帧"))?;
        if state.failed {
            return Err(io_error("读取资源输出"));
        }
        Ok(state.pending.drain(..).collect())
    }
    pub(super) fn poll(&self) -> Result<(), AppError> {
        if !self
            .lines
            .lock()
            .map_err(|_| io_error("读取资源状态"))?
            .pending
            .is_empty()
        {
            return Ok(());
        }
        match self
            .finished
            .lock()
            .map_err(|_| io_error("读取资源状态"))?
            .as_ref()
        {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}
impl Drop for BuiltinSession {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct ChannelResources {
    channel: pierlink_ssh_engine::Session,
    output: Option<LineReader>,
    error: Option<Reader>,
    input: Option<InputWriter>,
}
impl Drop for ChannelResources {
    fn drop(&mut self) {
        // 先关闭通道以唤醒可能被流控阻塞的写入，再回收各读取线程。
        let _ = self.channel.close();
        drop(self.input.take());
        if let Some(reader) = &mut self.output {
            reader.join();
        }
        if let Some(reader) = &mut self.error {
            reader.join();
        }
    }
}
fn run_channel(
    command: BuiltinCommand,
    script: String,
    lines: Arc<Mutex<Lines>>,
    cancelled: Arc<AtomicBool>,
    updates: Arc<InputUpdates>,
) -> Result<(), AppError> {
    let mut request = command.request();
    request.cancelled = cancelled.clone();
    // 与其他后台命令一样显式经过 POSIX sh，兼容非 POSIX 的登录 shell。
    let argv = ["/bin/sh", "-s"]
        .into_iter()
        .map(crate::ssh::quote::sh_quote)
        .collect::<Result<Vec<_>, _>>()?
        .join(" ");
    let remote = format!("/bin/sh -c {}", crate::ssh::quote::sh_quote(&argv)?);
    let channel = command.manager.open_session(request, |connection| {
        connection.exec_cancellable(&remote, &[], cancelled.clone(), Duration::from_secs(30))
    })?;
    let mut resources = ChannelResources {
        channel,
        output: None,
        error: None,
        input: None,
    };
    let (stdin, stdout, stderr) = resources.channel.take_io();
    resources.output = Some(LineReader::start_with_state(stdout, lines.clone())?);
    resources.error = Some(Reader::tail(stderr, 16 * 1024)?);
    resources.input = Some(InputWriter::start(stdin, script, updates)?);
    while !cancelled.load(Ordering::Acquire) {
        if let Some(result) = resources.channel.try_wait_status() {
            let status = result.map_err(|e| {
                crate::builtin::error::map_engine_error(&e, crate::builtin::error::Operation::Other)
            })?;
            // 读取器先收完最后输出，再根据退出状态分类。
            if let Some(reader) = &mut resources.output {
                reader.join();
            }
            if let Some(reader) = &mut resources.error {
                reader.join();
            }
            let stderr = resources
                .error
                .as_ref()
                .ok_or_else(|| io_error("读取资源错误"))?
                .state
                .lock()
                .map_err(|_| io_error("读取资源错误"))?;
            let text = String::from_utf8_lossy(&stderr.bytes);
            let code = status.code.and_then(|c| i32::try_from(c).ok());
            let mut error = classify_error(code, &text, &[]);
            if !lines.lock().unwrap_or_else(|p| p.into_inner()).saw_marker
                && code.is_some_and(|c| c != 0)
                && error.code != "ssh.unreachable"
            {
                error = AppError::new("monitor.no_posix_shell");
            }
            error.params.remove("stderr_tail");
            return Err(error);
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };

    #[test]
    fn 输入关闭后清理采样及后代进程() {
        let mut child = Command::new("/bin/sh")
            .arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        struct Guard<'a>(&'a mut std::process::Child);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                crate::process::kill_group(self.0.id());
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let guard = Guard(&mut child);
        let mut input = guard.0.stdin.take().unwrap();
        input
            .write_all(
                lifetime_script(
                    "sleep 300 & sub=$!\nprintf '%s %s\\n' \"$$\" \"$sub\"\nwait \"$sub\"",
                )
                .unwrap()
                .as_bytes(),
            )
            .unwrap();
        let mut line = String::new();
        BufReader::new(guard.0.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let pids: Vec<u32> = line
            .split_whitespace()
            .map(|pid| pid.parse().unwrap())
            .collect();
        assert_eq!(pids.len(), 2);
        assert!(guard.0.try_wait().unwrap().is_none());
        drop(input);
        let started = Instant::now();
        // 整套测试并行跑时机器很忙：给足时间，只验证最终会退出、会回收
        while guard.0.try_wait().unwrap().is_none() {
            assert!(started.elapsed() < Duration::from_secs(10));
            thread::sleep(Duration::from_millis(10));
        }
        let gone = |pid: u32| {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
                stat.rsplit_once(") ")
                    .is_some_and(|(_, rest)| rest.starts_with('Z'))
            })
        };
        for pid in pids {
            let started = Instant::now();
            while !gone(pid) {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "进程 {pid} 没有被回收"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn busybox采样控制可缩短睡眠且关闭输入回收进程() {
        if !std::path::Path::new("/usr/bin/busybox").is_file() {
            return;
        }
        let script = crate::monitor::sampling_script(Duration::from_secs(5)).unwrap();
        // 本机 /bin/sh 是 dash，替换采样解释器以实际验证 BusyBox 的 read 和信号行为。
        let script = lifetime_script(&script)
            .unwrap()
            .replace("| /bin/sh -c ", "| /usr/bin/busybox sh -c ");
        let mut child = Command::new("/usr/bin/busybox")
            .args(["sh", "-s"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        struct Guard<'a>(&'a mut std::process::Child);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                crate::process::kill_group(self.0.id());
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let guard = Guard(&mut child);
        let mut input = guard.0.stdin.take().unwrap();
        let mut output = LineReader::start(guard.0.stdout.take().unwrap()).unwrap();
        input.write_all(script.as_bytes()).unwrap();
        let wait_sample = || {
            let start = Instant::now();
            loop {
                if output
                    .state
                    .lock()
                    .unwrap()
                    .pending
                    .drain(..)
                    .any(|f| matches!(f, Frame::Sample(_)))
                {
                    break;
                }
                assert!(start.elapsed() < Duration::from_secs(3));
                thread::sleep(Duration::from_millis(10));
            }
        };
        wait_sample();
        input
            .write_all(crate::monitor::interval_line(Duration::from_millis(100)).as_bytes())
            .unwrap();
        let start = Instant::now();
        wait_sample();
        wait_sample();
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(input);
        let start = Instant::now();
        while guard.0.try_wait().unwrap().is_none() {
            assert!(start.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(10));
        }
        output.join();
    }
}
