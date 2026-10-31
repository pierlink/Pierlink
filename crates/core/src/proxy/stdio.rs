use super::{ProxyKind, ProxyServer, connect};
use crate::error::AppError;
use std::{
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

fn invalid() -> AppError {
    super::map_error(&pierlink_ssh_engine::proxy::ProxyError::InvalidInput)
}

pub(super) fn from_env_values(value: Option<&str>) -> Result<Option<ProxyServer>, AppError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed = url::Url::parse(value).map_err(|_| invalid())?;
    if value
        .split_once("://")
        .is_some_and(|(_, s)| s.contains('@'))
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
    {
        return Err(invalid());
    }
    let kind = match parsed.scheme() {
        "http" => ProxyKind::Http,
        "socks5h" | "socks5" => ProxyKind::Socks5,
        _ => return Err(invalid()),
    };
    let host = parsed
        .host_str()
        .ok_or_else(invalid)?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let server = ProxyServer {
        kind,
        host,
        port: parsed.port_or_known_default().ok_or_else(invalid)?,
    };
    pierlink_ssh_engine::proxy::validate(&server, "example.com", 22)
        .map_err(|e| super::map_error(&e))?;
    Ok(Some(server))
}

/// 从 PIERLINK_PROXY 读取无认证代理并连接 host:port，并在标准流间转发，任一方向结束即收尾。
/// 仅供 --proxy-connect 子进程入口使用；失败同时向标准错误写中文原因，调用方须以非零状态退出。
pub fn pipe_stdio_from_env(host: &str, port: u16) -> Result<(), AppError> {
    let result = pipe(host, port);
    if let Err(error) = &result {
        eprintln!(
            "{}",
            error
                .params
                .get("reason")
                .map(String::as_str)
                .unwrap_or("代理转发失败")
        );
    }
    result
}

fn pipe(host: &str, port: u16) -> Result<(), AppError> {
    let value = std::env::var("PIERLINK_PROXY")
        .map(Some)
        .or_else(|e| match e {
            std::env::VarError::NotPresent => Ok(None),
            _ => Err(invalid()),
        })?;
    let timeout = Duration::from_secs(15);
    let stream = match from_env_values(value.as_deref())? {
        Some(proxy) => connect(&proxy, host, port, timeout)?,
        None => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| io_error())?;
            let result = runtime
                .block_on(async {
                    tokio::time::timeout(timeout, tokio::net::TcpStream::connect((host, port)))
                        .await
                })
                .map_err(|_| super::map_error(&pierlink_ssh_engine::proxy::ProxyError::Timeout))
                .and_then(|result| result.map_err(|_| io_error()))
                .and_then(|stream| stream.into_std().map_err(|_| io_error()));
            runtime.shutdown_background();
            let stream = result?;
            stream.set_nonblocking(false).map_err(|_| io_error())?;
            stream
        }
    };
    relay(stream)
}

fn io_error() -> AppError {
    super::map_error(&pierlink_ssh_engine::proxy::ProxyError::Io)
}

fn relay(mut stream: TcpStream) -> Result<(), AppError> {
    #[cfg(unix)]
    let _stdout_mode = NonblockingStdout::new().map_err(|_| io_error())?;
    #[cfg(windows)]
    let output_done = Arc::new(AtomicBool::new(false));
    #[cfg(windows)]
    let output_cancel = OutputCancellation::new(output_done.clone()).map_err(|_| io_error())?;
    let mut writer = stream.try_clone().map_err(|_| io_error())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|_| io_error())?;
    writer
        .set_write_timeout(Some(Duration::from_secs(15)))
        .map_err(|_| io_error())?;
    let stopped = Arc::new(AtomicBool::new(false));
    let input_stopped = stopped.clone();
    let input = std::thread::Builder::new()
        .name("代理输入".into())
        .spawn(move || {
            let result = (|| {
                let mut buffer = [0; 8192];
                while !input_stopped.load(Ordering::Acquire) {
                    match read_stdin(&mut buffer).map_err(|_| io_error())? {
                        Some(0) => break,
                        Some(n) => {
                            if writer.write_all(&buffer[..n]).is_err() {
                                if input_stopped.load(Ordering::Acquire) {
                                    break;
                                }
                                return Err(io_error());
                            }
                        }
                        None => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
                Ok(())
            })();
            input_stopped.store(true, Ordering::Release);
            let _ = writer.shutdown(Shutdown::Both);
            #[cfg(windows)]
            output_cancel.cancel_until_done();
            result
        })
        .map_err(|_| io_error())?;
    let result = (|| {
        let mut buffer = [0; 8192];
        while !stopped.load(Ordering::Acquire) {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    write_stdout(&buffer[..n], &stopped).map_err(|_| io_error())?;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) if stopped.load(Ordering::Acquire) => break,
                Err(_) => return Err(io_error()),
            }
        }
        Ok(())
    })();
    stopped.store(true, Ordering::Release);
    #[cfg(windows)]
    output_done.store(true, Ordering::Release);
    let _ = stream.shutdown(Shutdown::Both);
    let input_result = input.join().map_err(|_| io_error())?;
    result.and(input_result)
}

#[cfg(unix)]
struct NonblockingStdout(i32);

#[cfg(unix)]
impl NonblockingStdout {
    fn new() -> std::io::Result<Self> {
        let flags = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(Self(flags))
        }
    }
}

#[cfg(unix)]
impl Drop for NonblockingStdout {
    fn drop(&mut self) {
        unsafe {
            libc::fcntl(libc::STDOUT_FILENO, libc::F_SETFL, self.0);
        }
    }
}

#[cfg(unix)]
fn write_stdout(mut bytes: &[u8], stopped: &AtomicBool) -> std::io::Result<()> {
    while !bytes.is_empty() && !stopped.load(Ordering::Acquire) {
        let n = unsafe { libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len()) };
        if n > 0 {
            bytes = &bytes[n as usize..];
        } else {
            let error = std::io::Error::last_os_error();
            if !matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) {
                return Err(error);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_stdout(bytes: &[u8], stopped: &AtomicBool) -> std::io::Result<()> {
    let mut output = std::io::stdout().lock();
    let result = output.write_all(bytes).and_then(|_| output.flush());
    if stopped.load(Ordering::Acquire) {
        Ok(())
    } else {
        result
    }
}

// 标准输出可能因 SSH 停止读取而阻塞；输入 EOF 后持续取消本线程的同步 I/O，直到输出收尾。
#[cfg(windows)]
struct OutputCancellation {
    thread: usize,
    done: Arc<AtomicBool>,
}

#[cfg(windows)]
impl OutputCancellation {
    fn new(done: Arc<AtomicBool>) -> std::io::Result<Self> {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThreadId, OpenThread, THREAD_TERMINATE,
        };
        let thread = unsafe { OpenThread(THREAD_TERMINATE, 0, GetCurrentThreadId()) };
        if thread.is_null() {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(Self {
                thread: thread as usize,
                done,
            })
        }
    }
    fn cancel_until_done(&self) {
        while !self.done.load(Ordering::Acquire) {
            unsafe {
                windows_sys::Win32::System::IO::CancelSynchronousIo(self.thread as _);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(windows)]
impl Drop for OutputCancellation {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.thread as _);
        }
    }
}

// 先检查可读数据，再读取，保证远端 EOF 时不会遗留阻塞在 stdin 的线程。
#[cfg(unix)]
fn read_stdin(buffer: &mut [u8]) -> std::io::Result<Option<usize>> {
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut fd, 1, 0) };
    if ready < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if ready == 0 {
        return Ok(None);
    }
    let n = unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(Some(n as usize))
    }
}

#[cfg(windows)]
fn read_stdin(buffer: &mut [u8]) -> std::io::Result<Option<usize>> {
    use windows_sys::Win32::{
        Foundation::ERROR_BROKEN_PIPE,
        Storage::FileSystem::ReadFile,
        System::{
            Console::{GetStdHandle, STD_INPUT_HANDLE},
            Pipes::PeekNamedPipe,
        },
    };
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut available = 0;
    if unsafe {
        PeekNamedPipe(
            handle,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
            Ok(Some(0))
        } else {
            Err(error)
        };
    }
    if available == 0 {
        return Ok(None);
    }
    let mut read = 0;
    if unsafe {
        ReadFile(
            handle,
            buffer.as_mut_ptr(),
            available.min(buffer.len() as u32),
            &mut read,
            std::ptr::null_mut(),
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(Some(read as usize))
    }
}
