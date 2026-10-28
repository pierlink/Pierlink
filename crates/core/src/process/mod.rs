//! 最小环境、进程树生命周期和有界管道读取。
use crate::{error::AppError, ssh::args::SshCommand};
mod capture;
pub(crate) use capture::{HeadTail, collect_tree};
use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(windows)]
pub(crate) mod windows;

#[derive(Debug, Clone)]
pub struct SshProgram {
    pub path: PathBuf,
}

#[cfg(any(windows, test))]
const PIPE_CREATION_FLAGS: u32 = 4 | 0x08000000;

const ENVIRONMENT_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "SystemRoot",
    "TEMP",
    "TMP",
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "KRB5CCNAME",
    "ProgramData",
    "SystemDrive",
    "COMSPEC",
    "LOCALAPPDATA",
];
pub(crate) fn environment(cmd: &SshCommand) -> Vec<(OsString, OsString)> {
    environment_from(cmd, |key| std::env::var_os(key))
}
fn environment_from(
    cmd: &SshCommand,
    mut get: impl FnMut(&str) -> Option<OsString>,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<_> = ENVIRONMENT_KEYS
        .iter()
        .filter_map(|key| get(key).map(|value| ((*key).into(), value)))
        .collect();
    env.extend(cmd.env.iter().map(|(k, v)| (k.into(), v.into())));
    env
}

pub(crate) fn io_error(operation: &str) -> AppError {
    AppError::new("ssh.io_failed").with("operation", operation)
}

/// 持有进程树；释放时终止整个进程组或作业对象，并回收直接子进程。
pub struct TreeProcess {
    pub(crate) child: Child,
    terminated: bool,
    /// 本次启动的实际登记，进程结束前持续持有。
    pub askpass: Option<Arc<crate::ssh::askpass_pipe::Registration>>,
    #[cfg(windows)]
    _job: windows::Job,
}
impl SshProgram {
    /// 按 cmd 的参数数组及最小环境启动 SSH，返回自动清理的进程句柄；三个标准流均为管道。
    pub fn spawn(&self, cmd: &SshCommand) -> Result<SshProcess, AppError> {
        if cmd.builtin.is_some() {
            return Err(AppError::new("ssh.invalid_argument")
                .with("reason", "内置后台命令不能启动本地进程"));
        }
        let cmd = cmd.prepare()?;
        self.spawn_with_environment(&cmd, environment(&cmd))
    }
    pub(crate) fn spawn_with_environment(
        &self,
        cmd: &SshCommand,
        environment: Vec<(OsString, OsString)>,
    ) -> Result<SshProcess, AppError> {
        let mut process =
            spawn_tree_command(&self.path, &cmd.args, false, None, &environment, false)?;
        process.askpass = cmd.askpass.clone();
        Ok(process)
    }
}

/// SSH 进程沿用通用进程树生命周期，同时持有本次密码代理登记。
pub type SshProcess = TreeProcess;

/// 以参数数组启动 program，cwd 为工作目录，env 覆盖继承的用户环境；返回释放时结束整棵树的句柄。
/// 三个标准流均为管道；本机不经过 shell，Windows 不弹窗口。
pub fn spawn_tree(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    env: &[(String, String)],
) -> Result<TreeProcess, AppError> {
    let env = env
        .iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect::<Vec<_>>();
    spawn_tree_command(program, args, false, cwd, &env, true)
}

/// 同 spawn_tree，但 Windows 上把 args 原样拼在命令行里、不做引号转义（cmd.exe 不认 `\"`，按 Rust 文档要用原样参数）；
/// 其他平台和 spawn_tree 一样。
pub fn spawn_tree_raw(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
) -> Result<TreeProcess, AppError> {
    spawn_tree_command(program, args, true, cwd, &[], true)
}

fn spawn_tree_command(
    program: &Path,
    args: &[String],
    raw: bool,
    cwd: Option<&Path>,
    env: &[(OsString, OsString)],
    inherit: bool,
) -> Result<TreeProcess, AppError> {
    let mut command = Command::new(program);
    #[cfg(windows)]
    if raw {
        use std::os::windows::process::CommandExt;
        for arg in args {
            command.raw_arg(arg);
        }
    } else {
        command.args(args);
    }
    #[cfg(not(windows))]
    {
        let _ = raw;
        command.args(args);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !inherit {
        command.env_clear();
    }
    command.envs(env.iter().map(|(k, v)| (k, v)));
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(PIPE_CREATION_FLAGS);
    }
    let child = spawn_retrying(&mut command)?;
    #[cfg(windows)]
    let (child, job) = {
        use std::os::windows::io::AsRawHandle;
        let mut child = child;
        let job = match windows::Job::attach(child.as_raw_handle()) {
            Ok(job) => job,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        if let Err(e) = windows::resume(child.as_raw_handle()) {
            drop(job);
            let _ = child.wait();
            return Err(e);
        }
        (child, job)
    };
    Ok(TreeProcess {
        child,
        terminated: false,
        askpass: None,
        #[cfg(windows)]
        _job: job,
    })
}

impl TreeProcess {
    /// 将 error 按实际登记的结果映射，必要时作废本次密码；无密码登记时原样返回。
    pub fn finish_error(&self, error: AppError) -> AppError {
        self.askpass
            .as_ref()
            .map_or_else(|| error.clone(), |guard| guard.finish_error(error.clone()))
    }
    /// 无参数；查询直接子进程状态，返回 None 表示仍在运行；资源由 terminate 或 Drop 回收。
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, AppError> {
        #[cfg(target_os = "linux")]
        {
            // 保留僵尸子进程占住 PID；调用方处理完 EOF 后由 terminate 先清理进程组再回收。
            if !self.terminated {
                return exit_status_unreaped(self.child.id());
            }
        }
        let status = self.child.try_wait().map_err(|_| io_error("查询进程"))?;
        if status.is_some() {
            self.terminated = true;
        }
        Ok(status)
    }
    /// 无参数；终止进程树并等待直接子进程被回收，重复调用安全。
    pub fn terminate(&mut self) -> Result<(), AppError> {
        #[cfg(windows)]
        self._job.terminate();
        if self.terminated {
            return Ok(());
        }
        #[cfg(unix)]
        kill_group(self.child.id());
        let _ = self.child.kill();
        self.terminated = true;
        let started = Instant::now();
        loop {
            if self
                .child
                .try_wait()
                .map_err(|_| io_error("回收进程"))?
                .is_some()
            {
                return Ok(());
            }
            if started.elapsed() >= Duration::from_secs(5) {
                return Err(io_error("等待进程树退出超时"));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for TreeProcess {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}
#[cfg(unix)]
pub(crate) fn kill_group(pid: u32) {
    // 子进程创建了独立进程组，负 PID 只作用于该组。
    if let Ok(pid) = i32::try_from(pid) {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[derive(Default)]
pub(crate) struct Capture {
    pub bytes: Vec<u8>,
    pub done: bool,
    pub exceeded: bool,
    pub failed: bool,
    pub prohibited: bool,
    pub authenticated: bool,
    pub matched_error: Option<&'static str>,
}
pub(crate) struct Reader {
    pub state: Arc<Mutex<Capture>>,
    handle: Option<JoinHandle<()>>,
}
impl Reader {
    pub fn start(input: impl Read + Send + 'static, limit: usize) -> Result<Self, AppError> {
        Self::capture(input, limit, false, None)
    }
    pub fn tail(input: impl Read + Send + 'static, limit: usize) -> Result<Self, AppError> {
        Self::capture(input, limit, true, None)
    }
    /// 读取 input 的有界尾部，按 ASCII 忽略大小写识别行首认证标记；返回读取器或线程错误。
    pub fn tail_authenticated(
        input: impl Read + Send + 'static,
        limit: usize,
    ) -> Result<Self, AppError> {
        Self::capture(input, limit, true, Some(b"\nAuthenticated to ".to_vec()))
    }
    fn capture(
        mut input: impl Read + Send + 'static,
        limit: usize,
        tail: bool,
        authenticated_marker: Option<Vec<u8>>,
    ) -> Result<Self, AppError> {
        let state = Arc::new(Mutex::new(Capture::default()));
        let shared = state.clone();
        let handle = thread::Builder::new()
            .name("ssh读取".into())
            .spawn(move || {
                let mut chunk = [0; 8192];
                let mut overlap = vec![b'\n'];
                let mut ring = std::collections::VecDeque::<u8>::new();
                loop {
                    let read = input.read(&mut chunk);
                    let Ok(mut data) = shared.lock() else {
                        return;
                    };
                    match read {
                        Ok(0) => {
                            data.done = true;
                            return;
                        }
                        Ok(n) => {
                            if tail {
                                overlap.extend_from_slice(&chunk[..n]);
                                data.prohibited |= overlap
                                    .windows(b"administratively prohibited".len())
                                    .any(|w| w == b"administratively prohibited");
                                if let Some(marker) = &authenticated_marker {
                                    data.authenticated |= overlap
                                        .windows(marker.len())
                                        .any(|w| w.eq_ignore_ascii_case(marker));
                                }
                                let keep = overlap.len().saturating_sub(
                                    authenticated_marker
                                        .as_ref()
                                        .map_or(26, |m| (m.len() - 1).max(26)),
                                );
                                overlap.drain(..keep);
                                // 环形队列只保留最后 limit 字节；快照供调度线程读取。
                                ring.extend(&chunk[..n]);
                                let excess = ring.len().saturating_sub(limit);
                                ring.drain(..excess);
                                data.bytes.clear();
                                data.bytes.extend(ring.iter().copied());
                                continue;
                            }
                            let remaining = limit.saturating_sub(data.bytes.len());
                            data.bytes.extend_from_slice(&chunk[..n.min(remaining)]);
                            if n > remaining {
                                data.exceeded = true;
                                data.done = true;
                                return;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        // Linux PTY 在最后一个从端关闭后返回 EIO，等价于 EOF。
                        Err(e) if cfg!(unix) && e.raw_os_error() == Some(5) => {
                            data.done = true;
                            return;
                        }
                        Err(_) => {
                            data.failed = true;
                            data.done = true;
                            return;
                        }
                    }
                }
            })
            .map_err(|_| io_error("创建读取线程"))?;
        Ok(Self {
            state,
            handle: Some(handle),
        })
    }
    pub fn join(&mut self) {
        if let Some(h) = self.handle.take() {
            join_bounded(h, Duration::from_secs(2));
        }
    }
}

pub(crate) fn join_bounded<T>(handle: JoinHandle<T>, timeout: Duration) {
    let start = Instant::now();
    while !handle.is_finished() {
        if start.elapsed() >= timeout {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = handle.join();
}

#[cfg(target_os = "linux")]
pub(crate) fn exit_status_unreaped(pid: u32) -> Result<Option<std::process::ExitStatus>, AppError> {
    use std::os::unix::process::ExitStatusExt;
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) == -1
        {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
                return Ok(None);
            }
            return Err(io_error("查询未回收进程"));
        }
        if info.si_pid() == 0 {
            return Ok(None);
        }
        let raw = if info.si_code == libc::CLD_EXITED {
            info.si_status() << 8
        } else {
            info.si_status()
        };
        Ok(Some(std::process::ExitStatus::from_raw(raw)))
    }
}

/// 启动进程。Unix 上刚写好的可执行文件可能正被别的线程 fork 出、还没 exec 的子进程短暂持有写句柄，
/// 这时 exec 会得到“文本文件忙”（ETXTBSY）：稍等重试几次，其他错误直接返回
fn spawn_retrying(command: &mut Command) -> Result<std::process::Child, AppError> {
    #[cfg(unix)]
    {
        for attempt in 1..=5u64 {
            match command.spawn() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(std::time::Duration::from_millis(20 * attempt));
                }
                other => return other.map_err(|_| io_error("启动进程")),
            }
        }
    }
    command.spawn().map_err(|_| io_error("启动进程"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn 最小环境和显式环境() {
        let cmd = SshCommand {
            config_text: None,
            builtin: None,
            askpass: None,
            args: vec![],
            env: vec![("PIERLINK_TEST".into(), "指定值".into())],
        };
        let mut process = SshProgram {
            path: "/usr/bin/env".into(),
        }
        .spawn(&cmd)
        .unwrap();
        drop(process.child.stdin.take());
        let mut output = String::new();
        process
            .child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        while process.try_wait().unwrap().is_none() {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(process.try_wait().unwrap().unwrap().success());
        assert!(output.contains("PIERLINK_TEST=指定值"));
        for line in output.lines() {
            let key = line.split_once('=').unwrap().0;
            assert!(
                [
                    "PATH",
                    "HOME",
                    "USERPROFILE",
                    "SystemRoot",
                    "TEMP",
                    "TMP",
                    "SSH_AUTH_SOCK",
                    "SSH_AGENT_PID",
                    "KRB5CCNAME",
                    "ProgramData",
                    "SystemDrive",
                    "COMSPEC",
                    "LOCALAPPDATA",
                    "PIERLINK_TEST"
                ]
                .contains(&key)
            );
        }
    }
    #[test]
    fn l1_进程组终止并回收() {
        let program = SshProgram {
            path: "/usr/bin/sleep".into(),
        };
        let mut process = program
            .spawn(&SshCommand {
                config_text: None,
                builtin: None,
                askpass: None,
                args: vec!["60".into()],
                env: vec![],
            })
            .unwrap();
        let pid = process.child.id();
        assert_eq!(unsafe { libc::getpgid(pid as i32) }, pid as i32);
        process.terminate().unwrap();
        process.terminate().unwrap();
        assert!(process.child.try_wait().unwrap().is_some());
        assert!(
            SshProgram {
                path: "/没有此程序".into()
            }
            .spawn(&SshCommand {
                config_text: None,
                builtin: None,
                askpass: None,
                args: vec![],
                env: vec![]
            })
            .is_err()
        );
    }
}

#[cfg(test)]
mod revision_tests {
    use super::*;
    #[test]
    fn 修订13_新增环境变量仍遵守白名单() {
        let cmd = SshCommand {
            config_text: None,
            builtin: None,
            askpass: None,
            args: vec![],
            env: vec![],
        };
        let values = environment_from(&cmd, |key| Some(format!("值:{key}").into()));
        for key in [
            "ProgramData",
            "SystemDrive",
            "COMSPEC",
            "LOCALAPPDATA",
            "KRB5CCNAME",
        ] {
            assert!(values.contains(&(key.into(), format!("值:{key}").into())));
        }
        assert!(!values.iter().any(|(key, _)| key == "AWS_SECRET_ACCESS_KEY"));
        assert_eq!(values.len(), ENVIRONMENT_KEYS.len());
    }
    #[test]
    fn 修订5_管道窗口与挂起标志() {
        assert_eq!(PIPE_CREATION_FLAGS & 4, 4);
        assert_eq!(PIPE_CREATION_FLAGS & 0x08000000, 0x08000000);
    }
    #[test]
    fn 修订7_尾部有界且跨块识别禁止转发() {
        let mut bytes = vec![b'x'; 8180];
        bytes.extend_from_slice(b"administratively prohibited");
        bytes.extend(vec![b'y'; 100_000]);
        bytes.extend_from_slice(b"Connection closed");
        let mut reader = Reader::tail(std::io::Cursor::new(bytes), 16 * 1024).unwrap();
        reader.join();
        let data = reader.state.lock().unwrap();
        assert!(data.done && data.prohibited && !data.exceeded);
        assert_eq!(data.bytes.len(), 16 * 1024);
        assert!(data.bytes.ends_with(b"Connection closed"));
    }
    #[test]
    fn 修订6_滞留读取线程等待有界() {
        struct Blocked(std::sync::mpsc::Receiver<()>);
        impl Read for Blocked {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                let _ = self.0.recv();
                Ok(0)
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let mut reader = Reader::start(Blocked(rx), 10).unwrap();
        let before = Instant::now();
        reader.join();
        assert!(before.elapsed() < Duration::from_secs(3));
        tx.send(()).unwrap();
        let deadline = Instant::now();
        while !reader.state.lock().unwrap().done {
            assert!(deadline.elapsed() < Duration::from_secs(1));
            thread::sleep(Duration::from_millis(1));
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn 修订9_查询不回收且终止后不重复发信号() {
        let mut process = SshProgram {
            path: "/bin/true".into(),
        }
        .spawn(&SshCommand {
            config_text: None,
            builtin: None,
            askpass: None,
            args: vec![],
            env: vec![],
        })
        .unwrap();
        while process.try_wait().unwrap().is_none() {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!process.terminated);
        assert!(exit_status_unreaped(process.child.id()).unwrap().is_some());
        process.terminate().unwrap();
        assert!(process.terminated);
        assert!(exit_status_unreaped(process.child.id()).unwrap().is_none());
        process.terminate().unwrap();
    }
}

#[cfg(test)]
mod authentication_tests {
    use super::*;
    struct Chunks {
        bytes: std::io::Cursor<Vec<u8>>,
        size: usize,
    }
    impl Read for Chunks {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let len = out.len().min(self.size);
            self.bytes.read(&mut out[..len])
        }
    }

    #[test]
    fn 认证标记跨块_仅匹配行首且不比主机名_尾部始终有界() {
        for size in 1..65 {
            for (line, expected) in [
                (
                    "Authenticated to DESTination ([1.2.3.4]:22) using publickey.\n",
                    true,
                ),
                (
                    "Authenticated to jump ([1.2.3.4]:22) using publickey.\n",
                    true,
                ),
                (
                    "Authenticated to destination.evil ([1.2.3.4]:22) using publickey.\n",
                    true,
                ),
                (
                    "Banner\r\naUtHeNtIcAtEd TO resolved.internal (via proxy) using publickey.\r\n",
                    true,
                ),
                (
                    "debug: Authenticated to destination ([1.2.3.4]:22) using publickey.\n",
                    false,
                ),
                ("Authenticated to destination", true),
                ("Authenticated to ", true),
                ("Authenticated to", false),
                ("Authenticated to\tdestination\n", false),
                (" Authenticated to destination\n", false),
            ] {
                let bytes = format!(
                    "{line}\nadministratively prohibited\n{}",
                    "x".repeat(20_000)
                )
                .into_bytes();
                let mut reader = Reader::tail_authenticated(
                    Chunks {
                        bytes: std::io::Cursor::new(bytes),
                        size,
                    },
                    128,
                )
                .unwrap();
                reader.join();
                let captured = reader.state.lock().unwrap();
                assert_eq!(
                    captured.authenticated, expected,
                    "分块大小={size}，行={line}"
                );
                assert!(captured.prohibited && captured.done && !captured.exceeded);
                assert_eq!(captured.bytes.len(), 128);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod password_tests {
    use super::*;
    use crate::{
        security::vault::Zeroizing,
        ssh::askpass_pipe::{PromptKind, test_answer, test_registration_mode},
    };
    #[test]
    fn 后台启动前拒绝缺密码_实际进程守卫独立_失败作废() {
        let template = test_registration_mode("user@host", false);
        let command = SshCommand {
            args: vec!["-c".into(), "exit 255".into()],
            env: vec![],
            config_text: None,
            builtin: None,
            askpass: Some(template.clone()),
        };
        let missing = SshProgram {
            path: "/不存在的SSH".into(),
        };
        assert_eq!(
            missing.spawn(&command).err().unwrap().code,
            "ssh.password_needed"
        );
        template
            .set_password(Zeroizing::new("测试密码".into()))
            .unwrap();
        let program = SshProgram {
            path: "/bin/sh".into(),
        };
        let mut process = program.spawn(&command).unwrap();
        let actual = process.askpass.as_ref().unwrap();
        assert_ne!(actual.nonce(), template.nonce());
        test_answer(actual, "user@host's password: ", PromptKind::Password).unwrap();
        assert_eq!(
            process.finish_error(AppError::new("ssh.auth_failed")).code,
            "ssh.auth_failed"
        );
        assert!(!template.has_password());
        process.terminate().unwrap();
    }
}

#[cfg(all(test, unix))]
mod tree_tests {
    use super::*;
    #[test]
    fn 通用启动参数逐字传递且继承环境并允许覆盖() {
        let mut process = spawn_tree(
            Path::new("/usr/bin/printf"),
            &["%s".into(), "$(id) ' 中文".into()],
            None,
            &[],
        )
        .unwrap();
        drop(process.child.stdin.take());
        let mut output = String::new();
        process
            .child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        assert_eq!(output, "$(id) ' 中文");
        process.terminate().unwrap();
        let mut process = spawn_tree(
            Path::new("/usr/bin/env"),
            &[],
            None,
            &[("PIERLINK_TREE_TEST".into(), "显式覆盖".into())],
        )
        .unwrap();
        drop(process.child.stdin.take());
        let mut output = String::new();
        process
            .child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        assert!(output.lines().any(|s| s == "PIERLINK_TREE_TEST=显式覆盖"));
        if let Ok(value) = std::env::var("PATH") {
            assert!(output.lines().any(|line| line == format!("PATH={value}")));
        }
        process.terminate().unwrap();
    }
}
