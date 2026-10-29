//! 旧版密钥生成方式，仅供格式兼容性测试使用。
use super::*;
use crate::process::Reader;
use portable_pty::PtySize;
#[cfg(unix)]
use portable_pty::{CommandBuilder, native_pty_system};
/// ssh-keygen 子进程。Windows 上用终端模块的 ConPTY 启动方式：portable_pty 在 Windows 上
/// 启动的子进程会继承父进程被重定向的标准句柄，输出不进伪终端（CI 上 30 秒内一个字都收不到）。
enum Child {
    #[cfg(unix)]
    Pty(Box<dyn portable_pty::Child + Send + Sync>),
    #[cfg(windows)]
    Native(crate::terminal::windows::Child),
}
impl Child {
    fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        match self {
            #[cfg(unix)]
            Child::Pty(c) => c.try_wait(),
            #[cfg(windows)]
            Child::Native(c) => c.try_wait(),
        }
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        match self {
            #[cfg(unix)]
            Child::Pty(c) => {
                let _ = c.kill();
                let _ = c.wait();
            }
            #[cfg(windows)]
            Child::Native(c) => {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

// 不经 shell，不传秘密环境变量；只在 ssh-keygen 关闭回显并发出预期提示后写入口令。
pub(super) fn keygen(
    program: &Path,
    args: &[String],
    secret: &str,
    prompts: &[Vec<String>],
    timeout: Duration,
) -> Result<Option<String>, AppError> {
    let size = PtySize {
        cols: 512,
        ..PtySize::default()
    };
    let env = [
        ("LC_ALL".to_owned(), "C".to_owned()),
        ("SSH_ASKPASS_REQUIRE".to_owned(), "never".to_owned()),
    ];
    #[cfg(unix)]
    let pair = native_pty_system()
        .openpty(size)
        .map_err(|_| failed("无法创建密钥生成终端"))?;
    #[cfg(unix)]
    let mut child = {
        let mut command = CommandBuilder::new(program);
        command.args(args);
        for (key, value) in &env {
            command.env(key, value);
        }
        Child::Pty(
            pair.slave
                .spawn_command(command)
                .map_err(|_| failed("无法启动 ssh-keygen"))?,
        )
    };
    #[cfg(unix)]
    drop(pair.slave);
    #[cfg(windows)]
    let pair = crate::terminal::windows::open(size).map_err(|_| failed("无法创建密钥生成终端"))?;
    #[cfg(windows)]
    let mut child = Child::Native(
        pair.master
            .spawn(program, args, None, &env)
            .map_err(|_| failed("无法启动 ssh-keygen"))?,
    );
    let mut reader = Reader::start(
        pair.master
            .try_clone_reader()
            .map_err(|_| failed("无法读取密钥生成终端"))?,
        64 * 1024,
    )?;
    let result = (|| {
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|_| failed("无法写入密钥生成终端"))?;
        let start = Instant::now();
        let mut answered = 0;
        let mut consumed = 0;
        let mut exited = None;
        loop {
            let state = reader
                .state
                .lock()
                .map_err(|_| failed("密钥生成输出不可用"))?;
            if start.elapsed() > timeout {
                // 附上已回答的提示数和终端最后一段输出（去掉控制序列和口令），便于在 Windows 上定位原因
                let text = normalize_terminal(&String::from_utf8_lossy(&state.bytes))
                    .replace(secret, "***");
                let tail: String = text
                    .chars()
                    .rev()
                    .take(160)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                return Err(failed("ssh-keygen 超时")
                    .with("answered", answered)
                    .with("output", format!("{tail:?}")));
            }
            if state.exceeded {
                return Err(failed("ssh-keygen 输出超过限制"));
            }
            if state.failed {
                return Err(failed("无法读取密钥生成输出"));
            }
            if answered < prompts.len()
                && prompts[answered]
                    .iter()
                    .any(|p| prompt_matches(&state.bytes[consumed..], p))
            {
                #[cfg(unix)]
                {
                    let fd = pair
                        .master
                        .as_raw_fd()
                        .ok_or_else(|| failed("无法检查终端回显"))?;
                    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
                    if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } != 0
                        || unsafe { termios.assume_init() }.c_lflag & libc::ECHO != 0
                    {
                        return Err(failed("密钥生成终端未关闭回显"));
                    }
                }
                io(writer.write_all(secret.as_bytes()))?;
                io(writer.write_all(if cfg!(windows) { b"\r" } else { b"\n" }))?;
                io(writer.flush())?;
                consumed = state.bytes.len();
                answered += 1;
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|_| failed("无法等待 ssh-keygen"))?
            {
                // ConPTY 在关闭主端前可能不返回 EOF；给输出留出有界排空时间。
                if state.done
                    || exited.get_or_insert_with(Instant::now).elapsed()
                        >= Duration::from_millis(500)
                {
                    let output = normalize_terminal(&String::from_utf8_lossy(&state.bytes));
                    let inspecting = args.first().is_some_and(|arg| arg == "-y");
                    if inspecting
                        && ((!status.success() && invalid_private_key(&output))
                            || (status.success() && answered == 0))
                    {
                        return Ok(None);
                    }
                    if !status.success() || answered != prompts.len() {
                        return Err(failed("ssh-keygen 执行失败或提示不匹配"));
                    }
                    return Ok(Some(output));
                }
            }
            drop(state);
            thread::sleep(Duration::from_millis(10));
        }
    })();
    drop(child);
    drop(pair.master);
    reader.join();
    result
}
/// 输出（去掉终端控制序列后）是否以提示结尾。比较时忽略结尾空白：
/// Windows ConPTY 重绘时会省略行尾空格，提示末尾的 ": " 只剩 ":"。
fn prompt_matches(bytes: &[u8], prompt: &str) -> bool {
    normalize_terminal(&String::from_utf8_lossy(bytes))
        .trim_end()
        .ends_with(prompt.trim_end())
}

/// ssh-keygen 解开私钥时的提示：OpenSSH 10.0 起带文件名（`Enter passphrase for "路径": `）
pub(super) fn decrypt_prompts(path: &str) -> Vec<String> {
    vec![
        "Enter passphrase: ".into(),
        format!("Enter passphrase for \"{path}\": "),
    ]
}

/// ssh-keygen 生成密钥时两次输入口令的提示（10.0 起第一次带文件名）
pub(super) fn generate_prompts(path: &str) -> Vec<Vec<String>> {
    vec![
        vec![
            "Enter passphrase (empty for no passphrase): ".into(),
            format!("Enter passphrase for \"{path}\" (empty for no passphrase): "),
        ],
        vec!["Enter same passphrase again: ".into()],
    ]
}
// 去掉伪终端控制序列，便于识别旧生成器的提示。
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
fn invalid_private_key(output: &str) -> bool {
    [
        "incorrect passphrase supplied to decrypt private key",
        "invalid format",
        "error in libcrypto",
    ]
    .iter()
    .any(|reason| output.contains(reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conpty_提示去除控制序列并兼容整行重绘() {
        let prompt = "Enter passphrase (empty for no passphrase): ";
        for output in [
            prompt.to_owned(),
            format!("\x1b[?25l{prompt}\x1b[?25h"),
            format!("\x1b[2J\x1b[1;1H    {prompt}\x1b[1;1H    {prompt}\x1b[?25l"),
            format!("{prompt}\r\x1b[2K        {prompt}\x1b[1;46H"),
        ] {
            assert!(prompt_matches(output.as_bytes(), prompt));
        }
        assert!(!prompt_matches(
            format!("{prompt}其他输出").as_bytes(),
            prompt
        ));
        assert!(!prompt_matches(b"Enter passphrase (empty", prompt));
        // ConPTY 省略行尾空格
        assert!(prompt_matches(
            b"Enter passphrase (empty for no passphrase):\x1b[1;46H",
            prompt
        ));
        // OpenSSH 10.0 的新提示
        let path = r"C:\Users\张三\AppData\Local\Pierlink\keys\pierlink_ed25519";
        let gen_ = generate_prompts(path);
        let shown = format!("Enter passphrase for \"{path}\" (empty for no passphrase): ");
        assert!(gen_[0].iter().any(|p| prompt_matches(shown.as_bytes(), p)));
        let dec = decrypt_prompts(path);
        let shown = format!("Enter passphrase for \"{path}\":\x1b[?25h");
        assert!(dec.iter().any(|p| prompt_matches(shown.as_bytes(), p)));
        assert!(
            !dec.iter()
                .any(|p| prompt_matches(b"Enter passphrase for \"C:\\other\":", p))
        );
    }
}
