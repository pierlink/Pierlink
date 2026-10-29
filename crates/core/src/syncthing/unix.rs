//! Unix 套接字传输仅供实例内部探活和退出，HTTP 编解码仍交给 ureq。
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, LazyBuffers, NextTimeout, Transport,
};
#[derive(Debug)]
pub(super) struct Connector(pub PathBuf);
#[derive(Debug)]
pub(super) struct Socket {
    stream: UnixStream,
    buffers: LazyBuffers,
}
impl ureq::unversioned::transport::Connector for Connector {
    type Out = Socket;
    fn connect(&self, _: &ConnectionDetails, _: Option<()>) -> Result<Option<Socket>, ureq::Error> {
        // 非阻塞连接避免套接字积压队列满时无限等待；启动方会在期限内重试。
        use std::os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::ffi::OsStrExt,
        };
        let bytes = self.0.as_os_str().as_bytes();
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
            return Err(
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "套接字路径无效").into(),
            );
        }
        address.sun_family = libc::AF_UNIX as _;
        for (to, from) in address.sun_path.iter_mut().zip(bytes) {
            *to = *from as _;
        }
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe {
            libc::connect(
                fd.as_raw_fd(),
                &address as *const _ as _,
                std::mem::size_of_val(&address) as _,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(false)?;
        Ok(Some(Socket {
            stream,
            buffers: LazyBuffers::new(8192, 8192),
        }))
    }
}
impl Transport for Socket {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        let timeout = timeout.for_write();
        if timeout.after
            == ureq::unversioned::transport::time::Duration::from(std::time::Duration::ZERO)
        {
            return Err(ureq::Error::Timeout(timeout.reason));
        }
        self.stream
            .set_write_timeout(timeout.not_zero().map(|d| *d))?;
        self.stream.write_all(&self.buffers.output()[..amount])?;
        Ok(())
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let timeout = timeout.for_read();
        if timeout.after
            == ureq::unversioned::transport::time::Duration::from(std::time::Duration::ZERO)
        {
            return Err(ureq::Error::Timeout(timeout.reason));
        }
        self.stream
            .set_read_timeout(timeout.not_zero().map(|d| *d))?;
        let n = self.stream.read(self.buffers.input_append_buf())?;
        self.buffers.input_appended(n);
        Ok(n > 0)
    }
    fn is_open(&mut self) -> bool {
        false
    }
}
