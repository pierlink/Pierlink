//! 私有临时目录隔离 Unix 套接字；所有资源由服务所有者自动删除。
use super::*;
use std::os::unix::{
    fs::PermissionsExt,
    net::{UnixListener, UnixStream},
};
pub(super) struct Listener {
    listener: UnixListener,
    endpoint: String,
    _root: tempfile::TempDir,
}
impl Listener {
    /// Unix 只用于开发和测试：靠 0700 私有目录隔离，不校验客户端程序
    pub(super) fn bind(root: &Path, _client: Option<&Path>) -> Result<Self, AppError> {
        let root = tempfile::Builder::new()
            .prefix("pl-ask-")
            .tempdir_in(root)
            .map_err(|_| failed("创建私有目录失败"))?;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(|_| failed("设置目录权限失败"))?;
        let path = root.path().join("s");
        let endpoint = path
            .to_str()
            .ok_or_else(|| failed("套接字路径无效"))?
            .to_owned();
        let listener = UnixListener::bind(path).map_err(|_| failed("监听套接字失败"))?;
        listener
            .set_nonblocking(true)
            .map_err(|_| failed("设置套接字失败"))?;
        Ok(Self {
            listener,
            endpoint,
            _root: root,
        })
    }
    pub(super) fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub(super) fn accept(&mut self, stopped: &Arc<AtomicBool>) -> Result<Option<Stream>, AppError> {
        match self.listener.accept() {
            Ok((stream, _)) => configure(stream, Some(stopped.clone())).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(_) => Err(failed("接受连接失败")),
        }
    }
}
pub(super) struct Stream {
    stream: UnixStream,
    deadline: Instant,
    stopped: Option<Arc<AtomicBool>>,
}
impl Stream {
    fn check(&self) -> std::io::Result<()> {
        if Instant::now() >= self.deadline
            || self
                .stopped
                .as_ref()
                .is_some_and(|s| s.load(Ordering::Acquire))
        {
            Err(std::io::ErrorKind::TimedOut.into())
        } else {
            Ok(())
        }
    }
}
impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            self.check()?;
            match self.stream.read(bytes) {
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                result => return result,
            }
        }
    }
}
impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.check()?;
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}
fn configure(stream: UnixStream, stopped: Option<Arc<AtomicBool>>) -> Result<Stream, AppError> {
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(1))))
        .map_err(|_| failed("设置通信时限失败"))?;
    Ok(Stream {
        stream,
        deadline: Instant::now() + TIMEOUT,
        stopped,
    })
}
pub(super) fn connect(endpoint: &str) -> Result<Stream, AppError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if endpoint.is_empty() || endpoint.contains('\0') || endpoint.len() >= address.sun_path.len() {
        return Err(failed("套接字路径无效"));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (target, source) in address.sun_path.iter_mut().zip(endpoint.bytes()) {
        *target = source as _;
    }
    let descriptor = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if descriptor < 0 {
        return Err(failed("创建客户端套接字失败"));
    }
    let stream = unsafe { UnixStream::from_raw_fd(descriptor) };
    if unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(failed("设置套接字继承失败"));
    }
    stream
        .set_nonblocking(true)
        .map_err(|_| failed("设置连接时限失败"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let connected = unsafe {
            libc::connect(
                stream.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                std::mem::size_of_val(&address) as _,
            )
        };
        if connected == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => break,
            Some(libc::EAGAIN | libc::EINPROGRESS | libc::EALREADY | libc::EINTR)
                if Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(5))
            }
            _ => return Err(failed("连接服务失败或超时")),
        }
    }
    stream
        .set_nonblocking(false)
        .map_err(|_| failed("设置通信模式失败"))?;
    configure(stream, None)
}
