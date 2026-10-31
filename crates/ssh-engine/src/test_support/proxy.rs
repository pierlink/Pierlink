//! 本机测试代理；只将测试目标转发到指定回环地址，记录收到的目标。
use crate::proxy::{ProxyKind, ProxyServer};
use std::{
    io::{self, Read, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

/// 测试代理应答方式。
#[derive(Clone, Copy)]
pub enum Reply {
    Forward(SocketAddr),
    Refuse,
    AuthRequired,
    Stall,
    Banner,
    Malformed,
    Oversized,
}

/// 自动回收监听线程与活动连接的本机代理。
pub struct TestProxy {
    pub server: ProxyServer,
    pub targets: Arc<Mutex<Vec<(String, u16)>>>,
    stop: Arc<AtomicBool>,
    streams: Arc<Mutex<Vec<TcpStream>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl TestProxy {
    /// 按 kind 和 reply 启动回环代理，返回自动清理的夹具。
    pub fn start(kind: ProxyKind, reply: Reply) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let server = ProxyServer {
            kind,
            host: "127.0.0.1".into(),
            port,
        };
        let targets = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let streams = Arc::new(Mutex::new(Vec::new()));
        let (received, stopped, active) = (targets.clone(), stop.clone(), streams.clone());
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
                        if let Ok(copy) = stream.try_clone() {
                            active.lock().unwrap().push(copy);
                        }
                        let _ = serve(&mut stream, kind, reply, &received, &stopped);
                        active.lock().unwrap().clear();
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            server,
            targets,
            stop,
            streams,
            worker: Some(worker),
        })
    }
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for stream in self.streams.lock().unwrap().iter() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read<const N: usize>(stream: &mut TcpStream) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}
fn text(stream: &mut TcpStream, length: usize) -> io::Result<String> {
    let mut data = vec![0; length];
    stream.read_exact(&mut data)?;
    String::from_utf8(data).map_err(|_| io::Error::other("测试代理收到非 UTF-8 地址"))
}

fn serve(
    stream: &mut TcpStream,
    kind: ProxyKind,
    reply: Reply,
    targets: &Mutex<Vec<(String, u16)>>,
    stop: &AtomicBool,
) -> io::Result<()> {
    if matches!(reply, Reply::Stall) {
        while !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(10));
        }
        return Ok(());
    }
    match kind {
        ProxyKind::Http => {
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") && header.len() < 16 * 1024 {
                header.extend(read::<1>(stream)?);
            }
            let header = String::from_utf8_lossy(&header);
            if matches!(reply, Reply::AuthRequired) {
                stream.write_all(b"HTTP/1.1 407 Authentication required\r\n\r\n")?;
                return Ok(());
            }
            let address = header
                .split_whitespace()
                .nth(1)
                .ok_or_else(|| io::Error::other("缺少目标"))?;
            let (host, port) = address
                .rsplit_once(':')
                .ok_or_else(|| io::Error::other("缺少端口"))?;
            targets.lock().unwrap().push((
                host.trim_matches(['[', ']']).into(),
                port.parse().map_err(|_| io::Error::other("端口无效"))?,
            ));
            if matches!(reply, Reply::Refuse) {
                stream.write_all(b"HTTP/1.1 403 secret-response\r\n\r\nsecret-body")?;
                return Ok(());
            }
            if matches!(reply, Reply::Malformed) {
                stream.write_all(b"BAD secret-response\r\n\r\n")?;
                return Ok(());
            }
            if matches!(reply, Reply::Oversized) {
                stream.write_all(&vec![b'x'; 17000])?;
                return Ok(());
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nHeader: value\r\n\r\n")?;
        }
        ProxyKind::Socks5 => {
            let greeting = read::<2>(stream)?;
            let mut methods = vec![0; greeting[1] as usize];
            stream.read_exact(&mut methods)?;
            if greeting != [5, 1] || methods != [0] {
                return Err(io::Error::other("测试客户端应只提供无认证方式"));
            }
            if matches!(reply, Reply::AuthRequired) {
                stream.write_all(&[5, 0xff])?;
                return Ok(());
            }
            stream.write_all(&[5, 0])?;
            let request = read::<4>(stream)?;
            let host = match request[3] {
                1 => IpAddr::from(read::<4>(stream)?).to_string(),
                4 => IpAddr::from(read::<16>(stream)?).to_string(),
                3 => {
                    let size = read::<1>(stream)?[0];
                    text(stream, size as usize)?
                }
                _ => return Err(io::Error::other("测试代理收到无效地址类型")),
            };
            let port = u16::from_be_bytes(read::<2>(stream)?);
            targets.lock().unwrap().push((host, port));
            if matches!(reply, Reply::Refuse) {
                stream.write_all(&[5, 5, 0, 1])?;
                return Ok(());
            }
            if matches!(reply, Reply::Malformed | Reply::Oversized) {
                stream.write_all(&[4, 0, 0, 9])?;
                return Ok(());
            }
            stream.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 1])?;
        }
    }
    if let Reply::Forward(address) = reply {
        let mut remote = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
        remote.set_read_timeout(Some(Duration::from_secs(3)))?;
        remote.set_write_timeout(Some(Duration::from_secs(3)))?;
        let mut incoming = stream.try_clone()?;
        let mut outgoing = remote.try_clone()?;
        thread::scope(|scope| {
            scope.spawn(|| {
                let _ = io::copy(&mut incoming, &mut outgoing);
                let _ = outgoing.shutdown(Shutdown::Both);
            });
            let _ = io::copy(&mut remote, stream);
            let _ = stream.shutdown(Shutdown::Both);
        });
    } else {
        stream.write_all(b"SSH-2.0-test\r\n")?;
    }
    Ok(())
}
