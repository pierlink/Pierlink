//! 子进程和临时 SSH 配置的所有权。
use super::*;
use std::fs::{self, OpenOptions};
struct Config(PathBuf);
impl Drop for Config {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
pub(super) struct Process {
    #[cfg(unix)]
    child: Box<dyn portable_pty::Child + Send + Sync>,
    #[cfg(windows)]
    child: super::windows::Child,
    status: Option<Option<i32>>,
    config: Option<Config>,
    remote: bool,
    pub(super) askpass: Option<Arc<crate::ssh::askpass_pipe::Registration>>,
}
impl Process {
    /// 测试专用：传入 Windows 子进程，返回尚未查询退出状态且不持有临时配置的进程所有者。
    #[cfg(all(test, windows))]
    pub(super) fn from_test_child(child: super::windows::Child) -> Self {
        Self {
            child,
            status: None,
            config: None,
            remote: false,
            askpass: None,
        }
    }
    pub(super) fn busy(&mut self) -> Option<bool> {
        if self.remote || self.poll().is_some() {
            return None;
        }
        super::busy::query(self.child.process_id()?)
    }
    pub(super) fn poll(&mut self) -> Option<Option<i32>> {
        if self.status.is_some() {
            return self.status;
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            self.status = Some(if status.signal().is_some() {
                None
            } else {
                Some(status.exit_code() as i32)
            });
            self.finish();
        }
        self.status
    }
    pub(super) fn terminate(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(pid) = self.child.process_id() {
            kill_session(pid, self.status.is_none());
        }
        if self.status.is_none() {
            #[cfg(all(unix, not(target_os = "linux")))]
            if let Some(pid) = self.child.process_id() {
                crate::process::kill_group(pid);
            }

            let _ = self.child.kill();
            #[cfg(unix)]
            let status = self.child.wait().ok();
            // Windows 的 kill 已最多等待五秒；失败后也只能非阻塞查询，不能继续无限等待。
            #[cfg(windows)]
            let status = self.child.try_wait_preserving_job().ok().flatten();
            self.status = Some(status.and_then(|s| {
                if s.signal().is_some() {
                    None
                } else {
                    Some(s.exit_code() as i32)
                }
            }));
        }
        self.config.take();
    }
    pub(super) fn finish(&mut self) {
        if self.status.is_none() {
            self.terminate();
        }
        self.config.take();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.finish();
    }
}
// 交互 shell 会为后台作业创建新的进程组，因此同时清理该终端会话的全部组。
#[cfg(target_os = "linux")]
fn kill_session(pid: u32, root_alive: bool) {
    if root_alive {
        unsafe {
            libc::kill(pid as i32, libc::SIGSTOP);
        }
    }
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(member) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            if unsafe { libc::getsid(member) } == pid as i32 {
                unsafe {
                    libc::kill(member, libc::SIGKILL);
                }
            }
        }
    }
    if root_alive {
        crate::process::kill_group(pid);
    }
}
pub(super) fn spawn(kind: TerminalKind, pair: &super::Pair) -> Result<Process, AppError> {
    let remote = matches!(&kind, TerminalKind::Remote { .. });
    let mut config = None;
    let mut askpass = None;
    let (program, args, cwd, mut env) = match kind {
        TerminalKind::BuiltinRemote(_) => {
            return Err(error(
                "spawn_failed",
                "terminal.built_in_terminals_cannot_start_local_processes",
                "内置终端不能启动本机进程",
            ));
        }
        TerminalKind::Local {
            program,
            args,
            cwd,
            env,
        } => (program, args, cwd, env),
        TerminalKind::Remote {
            ssh,
            mut command,
            config_dir,
        } => {
            command = command.prepare()?;
            askpass = command.askpass.clone();
            if let Some(text) = command.config_text.take().filter(|s| !s.is_empty()) {
                fs::create_dir_all(&config_dir).map_err(|_| {
                    error(
                        "config_failed",
                        "terminal.create_configuration_directory",
                        "创建配置目录",
                    )
                })?;
                let path = config_dir.join(format!("terminal-{}.conf", uuid::Uuid::new_v4()));
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(&path).map_err(|_| {
                    error(
                        "config_failed",
                        "terminal.create_temporary_ssh_configuration",
                        "创建临时 SSH 配置",
                    )
                })?;
                config = Some(Config(path.clone()));
                file.write_all(text.as_bytes()).map_err(|_| {
                    error(
                        "config_failed",
                        "terminal.write_temporary_ssh_configuration",
                        "写入临时 SSH 配置",
                    )
                })?;
                let path = path
                    .to_str()
                    .ok_or_else(|| {
                        error(
                            "config_failed",
                            "terminal.invalid_configuration_path_encoding",
                            "配置路径编码无效",
                        )
                    })?
                    .to_owned();
                if let Some(index) = command.args.iter().position(|s| s == "-F") {
                    let value = command.args.get_mut(index + 1).ok_or_else(|| {
                        error(
                            "config_failed",
                            "terminal.ssh_configuration_argument_is_missing",
                            "缺少 SSH 配置参数",
                        )
                    })?;
                    *value = path;
                } else {
                    command.args.splice(0..0, ["-F".into(), path]);
                }
            }
            (ssh, command.args, None, command.env)
        }
    };
    if cwd.as_ref().is_some_and(|path| !path.is_dir()) {
        return Err(error(
            "cwd_not_found",
            "terminal.working_directory_is_missing_or_is_not_a_directory",
            "工作目录不存在或不是目录",
        ));
    }
    env.retain(|(key, _)| !key.eq_ignore_ascii_case("TERM"));
    env.push(("TERM".into(), "xterm-256color".into()));
    #[cfg(unix)]
    let child: Box<dyn portable_pty::Child + Send + Sync> = {
        use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
        let path = pair
            .master
            .tty_name()
            .ok_or_else(|| error("spawn_failed", "terminal.get_pseudoterminal", "获取伪终端"))?;
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(path)
            .map_err(|_| error("spawn_failed", "terminal.open_pseudoterminal", "打开伪终端"))?;
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .env("TERM", "xterm-256color")
            .envs(env)
            .stdin(slave.try_clone().map_err(|_| {
                error(
                    "spawn_failed",
                    "terminal.clone_input_handle",
                    "复制输入句柄",
                )
            })?)
            .stdout(slave.try_clone().map_err(|_| {
                error(
                    "spawn_failed",
                    "terminal.clone_output_handle",
                    "复制输出句柄",
                )
            })?)
            .stderr(slave);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Box::new(command.spawn().map_err(|e| {
            error(
                if e.kind() == std::io::ErrorKind::NotFound {
                    "not_found"
                } else {
                    "spawn_failed"
                },
                "terminal.start_terminal_program",
                "启动终端程序",
            )
        })?)
    };
    #[cfg(windows)]
    let child = pair.master.spawn(&program, &args, cwd.as_deref(), &env)?;
    Ok(Process {
        askpass,
        child,
        status: None,
        config,
        remote,
    })
}
