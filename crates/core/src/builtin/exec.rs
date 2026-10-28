//! 通用 exec：标准输入、双输出并发处理，截断后仍持续读出以免共享窗口停滞。
use super::*;
use crate::ssh::quote::sh_quote;
use std::{
    io::{Read, Write},
    sync::{
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// 命令参数逐个转义；Escaped 只接受调用方已经完成转义的命令字符串。
pub enum ExecCommand {
    Argv(Vec<String>),
    Escaped(String),
}
impl ExecCommand {
    fn render(&self, directory: Option<&str>) -> Result<String, AppError> {
        let command = match self {
            Self::Argv(args) if !args.is_empty() && !args[0].is_empty() => args
                .iter()
                .map(|s| sh_quote(s))
                .collect::<Result<Vec<_>, _>>()?
                .join(" "),
            Self::Escaped(command) if !command.is_empty() && !command.contains('\0') => {
                command.clone()
            }
            _ => return Err(AppError::new("ssh.invalid_argument").with("field", "命令")),
        };
        // 显式使用 POSIX sh 解释目录与已转义命令，避免 fish 等登录 shell 的语法差异。
        let script = match directory {
            Some(path) => format!("cd {} && {command}", sh_quote(path)?),
            None => command,
        };
        Ok(format!("/bin/sh -c {}", sh_quote(&script)?))
    }
}

/// 命令执行选项；None 超时允许无限等待，取消贯穿取连接、开会话和读写。
pub struct ExecOptions {
    pub stdin: Vec<u8>,
    /// 写完 stdin 后保持输入端打开，供常驻命令感知连接关闭；默认发送 EOF。
    pub keep_stdin_open: bool,
    pub env: Vec<(String, String)>,
    pub directory: Option<String>,
    pub timeout: Option<Duration>,
    /// stdout 和 stderr 合计保留或交付的最大字节数；超出部分持续读出并丢弃。
    pub output_limit: usize,
    pub cancelled: Arc<AtomicBool>,
}
impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            stdin: Vec::new(),
            keep_stdin_open: false,
            env: Vec::new(),
            directory: None,
            timeout: Some(Duration::from_secs(30)),
            output_limit: 1024 * 1024,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// 流式输出类别；回调按接收次序串行调用，每块至多 32 KiB。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// 原始输出和退出信息；超时或取消时状态可能缺失，已读输出仍返回。
#[derive(Debug, Default)]
pub struct ExecResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<u32>,
    pub signal: Option<String>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub truncated: bool,
}

struct Completion<'a>(&'a AtomicBool);
impl Drop for Completion<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl ConnectionManager {
    /// 在 request.class 指定的连接执行 command，按 options 返回双输出、退出状态和截断标记。
    /// timeout 覆盖取连接至退出的全过程；失败返回受控 AppError，不回显命令或输入。
    pub fn exec(
        &self,
        request: ConnectionRequest<'_>,
        command: ExecCommand,
        options: ExecOptions,
    ) -> Result<ExecResult, AppError> {
        self.execute(request, command, options, true, |_, _| Ok(()))
    }

    /// 在 request.class 指定的连接流式执行 command；callback 串行接收有界输出，须及时返回且不要阻塞等待本命令。
    /// 返回结果的 stdout/stderr 为空，其他字段与 exec 相同；回调失败会取消会话并返回固定错误。
    pub fn exec_streaming(
        &self,
        request: ConnectionRequest<'_>,
        command: ExecCommand,
        options: ExecOptions,
        callback: impl FnMut(OutputStream, &[u8]) -> Result<(), AppError>,
    ) -> Result<ExecResult, AppError> {
        self.execute(request, command, options, false, callback)
    }

    fn execute(
        &self,
        request: ConnectionRequest<'_>,
        command: ExecCommand,
        options: ExecOptions,
        collect: bool,
        mut callback: impl FnMut(OutputStream, &[u8]) -> Result<(), AppError>,
    ) -> Result<ExecResult, AppError> {
        let command = command.render(options.directory.as_deref())?;
        let started = Instant::now();
        let operation_cancel = Arc::new(AtomicBool::new(false));
        // 独立取消标记，超时不能污染调用方的长期取消句柄。
        let finished = AtomicBool::new(false);
        let reason = AtomicU8::new(0);
        std::thread::scope(|scope| {
            let operation_cancel = &operation_cancel;
            let options = &options;
            let finished = &finished;
            let reason = &reason;
            let _completion = Completion(finished);
            let caller_cancel = request.cancelled.clone();
            scope.spawn(move || {
                while !finished.load(Ordering::Acquire) {
                    let value = if caller_cancel.load(Ordering::Acquire)
                        || options.cancelled.load(Ordering::Acquire)
                    {
                        2
                    } else if options.timeout.is_some_and(|t| started.elapsed() >= t) {
                        1
                    } else {
                        0
                    };
                    if value != 0 {
                        reason.store(value, Ordering::Release);
                        operation_cancel.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
            let result = (|| {
                if request.cancelled.load(Ordering::Acquire)
                    || options.cancelled.load(Ordering::Acquire)
                {
                    reason.store(2, Ordering::Release);
                    return Ok(stopped(2));
                }
                if options.timeout == Some(Duration::ZERO) {
                    reason.store(1, Ordering::Release);
                    return Ok(stopped(1));
                }
                let env: Vec<_> = options
                    .env
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str()))
                    .collect();
                let session = self.open_session(
                    ConnectionRequest {
                        server: request.server,
                        class: request.class,
                        vault: request.vault,
                        callbacks: request.callbacks,
                        interactive: request.interactive,
                        cancelled: operation_cancel.clone(),
                    },
                    |connection| {
                        let remaining = options
                            .timeout
                            .map(|t| t.saturating_sub(started.elapsed()))
                            .unwrap_or(Duration::from_secs(30));
                        if operation_cancel.load(Ordering::Acquire) {
                            return Err(pierlink_ssh_engine::Error::Callback);
                        }
                        connection.exec_cancellable(
                            &command,
                            &env,
                            operation_cancel.clone(),
                            remaining.max(Duration::from_millis(1)),
                        )
                    },
                )?;
                run_session(
                    session,
                    options,
                    operation_cancel,
                    reason,
                    collect,
                    &mut callback,
                )
            })();
            finished.store(true, Ordering::Release);
            match (result, reason.load(Ordering::Acquire)) {
                (Err(_), value @ (1 | 2)) => Ok(stopped(value)),
                (result, _) => result,
            }
        })
    }
}

fn stopped(reason: u8) -> ExecResult {
    ExecResult {
        timed_out: reason == 1,
        cancelled: reason == 2,
        ..Default::default()
    }
}

enum Message {
    Data(OutputStream, Vec<u8>),
    ReadDone(std::io::Result<()>),
    WriteDone(std::io::Result<()>),
}

fn read_output(mut reader: impl Read, kind: OutputStream, sender: mpsc::SyncSender<Message>) {
    let result = (|| {
        let mut buffer = [0; 32 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Ok(());
            }
            if sender
                .send(Message::Data(kind, buffer[..count].to_vec()))
                .is_err()
            {
                return Ok(());
            }
        }
    })();
    let _ = sender.send(Message::ReadDone(result));
}

fn run_session(
    mut session: pierlink_ssh_engine::Session,
    options: &ExecOptions,
    cancelled: &Arc<AtomicBool>,
    reason: &AtomicU8,
    collect: bool,
    callback: &mut impl FnMut(OutputStream, &[u8]) -> Result<(), AppError>,
) -> Result<ExecResult, AppError> {
    let closer = session.closer();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let done = &done;
        let closer = &closer;
        let _completion = Completion(done);
        scope.spawn(move || {
            while !done.load(Ordering::Acquire) {
                if cancelled.load(Ordering::Acquire) {
                    closer.cancel();
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let mut output = ExecResult::default();
        let mut failure = None;
        std::thread::scope(|scope| {
            let (sender, receiver) = mpsc::sync_channel(8);
            let out = &mut session.stdout;
            let err = &mut session.stderr;
            let input = &mut session.stdin;
            let tx = sender.clone();
            scope.spawn(move || read_output(out, OutputStream::Stdout, tx));
            let tx = sender.clone();
            scope.spawn(move || read_output(err, OutputStream::Stderr, tx));
            scope.spawn(move || {
                let result = input.write_all(&options.stdin).and_then(|_| {
                    if options.keep_stdin_open {
                        Ok(())
                    } else {
                        input.shutdown().map_err(std::io::Error::other)
                    }
                });
                let _ = sender.send(Message::WriteDone(result));
            });
            let mut complete = 0;
            let mut retained = 0;
            while complete < 3 {
                match receiver.recv() {
                    Ok(Message::Data(kind, bytes)) => {
                        let count = bytes
                            .len()
                            .min(options.output_limit.saturating_sub(retained));
                        output.truncated |= count < bytes.len();
                        retained += count;
                        if count > 0 && failure.is_none() {
                            if callback(kind, &bytes[..count]).is_err() {
                                failure = Some(
                                    AppError::new("ssh.task_failed")
                                        .with("reason", "命令输出回调失败"),
                                );
                                closer.cancel();
                            }
                            if collect {
                                match kind {
                                    OutputStream::Stdout => &mut output.stdout,
                                    OutputStream::Stderr => &mut output.stderr,
                                }
                                .extend_from_slice(&bytes[..count]);
                            }
                        }
                    }
                    Ok(Message::ReadDone(result) | Message::WriteDone(result)) => {
                        complete += 1;
                        if result.is_err() && failure.is_none() {
                            failure = Some(
                                AppError::new("ssh.unreachable").with("reason", "命令通道读写中断"),
                            );
                            closer.cancel();
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let status = session.wait_status();
        done.store(true, Ordering::Release);
        let value = reason.load(Ordering::Acquire);
        output.timed_out = value == 1;
        output.cancelled = value == 2;
        if value == 0 {
            if let Some(error) = failure {
                return Err(error);
            }
            let status =
                status.map_err(|e| error::map_engine_error(&e, error::Operation::Other))?;
            output.exit_code = status.code;
            output.signal = status.signal;
        }
        Ok(output)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 参数与工作目录逐字节转义_不泄露无效输入() {
        // 渲染出的命令交给服务器上的 POSIX shell 执行；本机用 /bin/sh 验证，Windows 上没有，跳过这一半
        #[cfg(unix)]
        {
            let root = crate::test_support::tempdir();
            let directory = root.path().join("中文 ' $() 目录");
            std::fs::create_dir(&directory).unwrap();
            let arguments = ["", "a'b\"c", "$(id)`id`*?[]", "中文\n换行"];
            let mut argv = vec!["printf".into(), "%s\\0".into()];
            argv.extend(arguments.iter().map(|s| s.to_string()));
            let rendered = ExecCommand::Argv(argv).render(directory.to_str()).unwrap();
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", &rendered])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(
                output.stdout,
                arguments
                    .iter()
                    .flat_map(|s| s.as_bytes().iter().copied().chain([0]))
                    .collect::<Vec<_>>()
            );
        }
        for command in [
            ExecCommand::Argv(vec![]),
            ExecCommand::Argv(vec![String::new()]),
            ExecCommand::Escaped("秘密\0".into()),
        ] {
            let error = command.render(None).unwrap_err();
            assert!(!format!("{error:?}").contains("秘密"));
        }
        assert!(
            ExecCommand::Escaped("true".into())
                .render(Some("秘密\0"))
                .is_err()
        );
    }
}

#[cfg(test)]
mod raw_tests {
    use super::*;
    use crate::{
        mcp::RunLimits,
        process::SshProgram,
        remote::raw::run_raw,
        storage::{AuthMethod, ServerSource},
    };
    use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer, ScriptReply};

    struct Callbacks;
    impl ConnectionCallbacks for Callbacks {
        fn check_host_key(&self, _: &str, _: &HostKey, _: &AtomicBool) -> Result<bool, AppError> {
            Ok(true)
        }
        fn password(&self, _: &str, _: &AtomicBool) -> Result<Option<Zeroizing<String>>, AppError> {
            panic!("后台不能弹密码框")
        }
        fn passphrase(
            &self,
            _: &str,
            _: &Path,
            _: &AtomicBool,
        ) -> Result<Option<Zeroizing<String>>, AppError> {
            panic!("后台不能弹口令框")
        }
        fn dedicated_key(
            &self,
            _: &str,
            _: &AtomicBool,
        ) -> Result<Option<DedicatedCredential>, AppError> {
            Ok(None)
        }
        fn keyboard_interactive(
            &self,
            _: &str,
            _: Challenge,
            _: &AtomicBool,
        ) -> Result<Option<Vec<Zeroizing<String>>>, AppError> {
            panic!("后台不能弹验证码框")
        }
    }
    #[cfg(unix)]
    #[test]
    fn 系统命令替身保留退出码并跨截断识别错误及回收进程() {
        let root = crate::test_support::tempdir();
        let pidfile = root.path().join("pid");
        let manager = ConnectionManager::default();
        let mut server =
            crate::server_input::build_alias_server("系统", None, "system", 0).unwrap();
        server.connection_engine = crate::storage::Engine::OpenSsh;
        let request = || ConnectionRequest {
            server: &server,
            class: ConnectionClass::Main,
            vault: Arc::new(PasswordVault::default()),
            callbacks: Arc::new(Callbacks),
            interactive: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let program = SshProgram {
            path: "/bin/sh".into(),
        };
        // 替身把最后一个参数作为远程 exec 请求；不依赖监听端口。
        let openssh = || {
            Ok(crate::ssh::args::SshCommand {
                args: vec![
                    "-c".into(),
                    format!(
                        "printf '%s' $$ > {}; exec /bin/sh -c \"$0\"",
                        sh_quote(pidfile.to_str().unwrap()).unwrap()
                    ),
                    "/bin/sh -s".into(),
                ],
                env: vec![],
                builtin: None,
                askpass: None,
                config_text: None,
            })
        };
        let limits = RunLimits {
            timeout: Duration::from_secs(3),
            output_bytes: 16,
        };
        for code in [0, 7, 255] {
            let out = run_raw(
                &manager,
                request(),
                &program,
                openssh,
                &format!("printf out; printf err >&2; exit {code}"),
                None,
                limits,
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            assert_eq!(out.exit_code, Some(code));
            assert_eq!(out.stdout, b"out");
            assert_eq!(out.stderr, b"err");
        }
        let error = run_raw(
            &manager,
            request(),
            &program,
            openssh,
            "printf 'before Permission denied (publickey). secret after' >&2; exit 255",
            None,
            limits,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert_eq!(error.code, "ssh.auth_failed");
        assert!(!format!("{error:?}").contains("secret"));
        let cwd = root.path().join("中文 ' $() 目录");
        std::fs::create_dir(&cwd).unwrap();
        let out = run_raw(
            &manager,
            request(),
            &program,
            openssh,
            "pwd",
            cwd.to_str(),
            RunLimits {
                output_bytes: 4096,
                ..limits
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            cwd.to_str().unwrap()
        );
        let out = run_raw(
            &manager,
            request(),
            &program,
            openssh,
            "sleep 30",
            None,
            RunLimits {
                timeout: Duration::from_millis(50),
                ..limits
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(out.timed_out);
        let pid: i32 = std::fs::read_to_string(pidfile).unwrap().parse().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
    #[test]
    fn 内置原样执行退出码双流头尾超时及后台密码() {
        let manager = ConnectionManager::default();
        let mut server =
            crate::server_input::build_alias_server("内存", None, "memory", 0).unwrap();
        server.source = ServerSource::Manual;
        server.ssh_alias = None;
        server.host = Some("127.0.0.1".into());
        server.username = Some("tester".into());
        server.auth_method = AuthMethod::Password;
        server.connection_engine = crate::storage::Engine::Builtin;
        let vault = Arc::new(PasswordVault::default());
        let peer = LoginPeer::new(LoginMode::Password).with_scripts(|_| ScriptReply {
            stdout: format!("head{}tail", "o".repeat(200000)).into_bytes(),
            stderr: format!("err-head{}err-tail", "e".repeat(200000)).into_bytes(),
            exit_code: Some(7),
        });
        let engine = Engine::new().unwrap();
        *manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
            peer.connect(&engine, options, auth)
        }));
        let request = || ConnectionRequest {
            server: &server,
            class: ConnectionClass::Bulk,
            vault: vault.clone(),
            callbacks: Arc::new(Callbacks),
            interactive: true,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let program = SshProgram {
            path: "/不存在的程序".into(),
        };
        let limits = RunLimits {
            timeout: Duration::from_secs(5),
            output_bytes: 64,
        };
        assert_eq!(
            run_raw(
                &manager,
                request(),
                &program,
                || panic!("内置不能构造系统参数"),
                "exit 7",
                None,
                limits,
                Arc::new(AtomicBool::new(false))
            )
            .unwrap_err()
            .code,
            "ssh.password_needed"
        );
        vault
            .set(&server.id, Zeroizing::new("测试密码".into()))
            .unwrap();
        let out = run_raw(
            &manager,
            request(),
            &program,
            || panic!("内置不能构造系统参数"),
            "'/bin/sh' '-s'",
            None,
            limits,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(out.exit_code, Some(7));
        assert!(out.stdout.starts_with(b"head") && out.stdout.ends_with(b"tail"));
        assert!(out.stderr.starts_with(b"err-head") && out.stderr.ends_with(b"err-tail"));
        assert!(out.truncated && !out.timed_out);
        manager.close_server(&server.id).unwrap();
        let peer = LoginPeer::new(LoginMode::Password).with_scripts(|_| {
            std::thread::sleep(Duration::from_millis(300));
            ScriptReply {
                stdout: vec![],
                stderr: vec![],
                exit_code: Some(0),
            }
        });
        let engine = Engine::new().unwrap();
        *manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
            peer.connect(&engine, options, auth)
        }));
        let out = run_raw(
            &manager,
            request(),
            &program,
            || panic!("内置不能构造系统参数"),
            "'/bin/sh' '-s'",
            None,
            RunLimits {
                timeout: Duration::from_millis(100),
                ..limits
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(out.timed_out);
        manager.close_server(&server.id).unwrap();
        vault.mark_two_factor(&server.id);
        assert_eq!(
            run_raw(
                &manager,
                request(),
                &program,
                || panic!("内置不能构造系统参数"),
                "'/bin/sh' '-s'",
                None,
                limits,
                Arc::new(AtomicBool::new(false))
            )
            .unwrap_err()
            .code,
            "ssh.two_factor_required"
        );
        manager.close_all().unwrap();
    }
}
