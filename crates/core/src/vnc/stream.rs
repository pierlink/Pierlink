use crate::{error::AppError, process::join_bounded};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const CAPACITY: usize = 256 * 1024;
const CHUNK: usize = 64 * 1024;
pub(super) fn closed() -> AppError {
    AppError::new("vnc.stream_closed").with("reason", "桌面数据流已关闭")
}
fn timeout() -> AppError {
    AppError::new("vnc.probe_timeout")
}

#[derive(Default)]
struct Buffer {
    bytes: VecDeque<u8>,
    error: Option<AppError>,
    queued: u64,
    completed: u64,
}
#[derive(Default)]
struct Pipe {
    buffer: Mutex<Buffer>,
    changed: Condvar,
}
impl Pipe {
    fn lock(&self) -> MutexGuard<'_, Buffer> {
        self.buffer.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn wait<'a>(
        &self,
        buffer: MutexGuard<'a, Buffer>,
        deadline: Option<Instant>,
    ) -> Result<MutexGuard<'a, Buffer>, AppError> {
        match deadline {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(timeout());
                }
                let (buffer, result) = self
                    .changed
                    .wait_timeout(buffer, left)
                    .unwrap_or_else(|p| p.into_inner());
                if result.timed_out() {
                    Err(timeout())
                } else {
                    Ok(buffer)
                }
            }
            None => Ok(self.changed.wait(buffer).unwrap_or_else(|p| p.into_inner())),
        }
    }
    fn fail(&self, error: AppError, discard: bool) {
        let mut buffer = self.lock();
        if discard {
            buffer.bytes.clear();
        }
        if buffer.error.is_none() {
            buffer.error = Some(error);
        }
        self.changed.notify_all();
    }
    fn read(&self, out: &mut [u8], deadline: Option<Instant>) -> Result<usize, AppError> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut buffer = self.lock();
        while buffer.bytes.is_empty() {
            if let Some(error) = &buffer.error {
                return Err(error.clone());
            }
            buffer = self.wait(buffer, deadline)?;
        }
        let n = out.len().min(buffer.bytes.len());
        for (slot, byte) in out.iter_mut().zip(buffer.bytes.drain(..n)) {
            *slot = byte;
        }
        self.changed.notify_all();
        Ok(n)
    }
    fn write(&self, bytes: &[u8], deadline: Option<Instant>) -> Result<(), AppError> {
        let mut rest = bytes;
        let mut buffer = self.lock();
        while !rest.is_empty() {
            if let Some(error) = &buffer.error {
                return Err(error.clone());
            }
            if buffer.bytes.len() == CAPACITY {
                buffer = self.wait(buffer, deadline)?;
                continue;
            }
            let n = rest.len().min(CAPACITY - buffer.bytes.len());
            buffer.bytes.extend(&rest[..n]);
            buffer.queued = buffer.queued.saturating_add(n as u64);
            rest = &rest[n..];
            self.changed.notify_all();
        }
        if let Some(error) = &buffer.error {
            return Err(error.clone());
        }
        Ok(())
    }
    fn complete(&self, n: usize) {
        let mut buffer = self.lock();
        buffer.completed += n as u64;
        self.changed.notify_all();
    }
    fn flush(&self, deadline: Option<Instant>) -> Result<(), AppError> {
        let mut buffer = self.lock();
        loop {
            if let Some(error) = &buffer.error {
                return Err(error.clone());
            }
            if buffer.completed == buffer.queued {
                return Ok(());
            }
            buffer = self.wait(buffer, deadline)?;
        }
    }
}

pub(super) trait Backend: Send {
    fn close(&mut self);
    fn failure(&mut self) -> AppError {
        closed()
    }
}

struct Control {
    closed: AtomicBool,
    incoming: Arc<Pipe>,
    outgoing: Arc<Pipe>,
    backend: Mutex<Box<dyn Backend>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}
impl Control {
    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.incoming.fail(closed(), true);
        self.outgoing.fail(closed(), true);
        self.backend
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .close();
    }
    fn failure(&self) -> AppError {
        self.backend
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .failure()
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        self.close();
        let start = Instant::now();
        for worker in self
            .workers
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
        {
            if worker.thread().id() == thread::current().id() {
                continue;
            }
            join_bounded(
                worker,
                Duration::from_secs(2).saturating_sub(start.elapsed()),
            );
        }
    }
}

/// 桌面字节流；两方向各最多缓存 256 KiB，最后一个句柄释放时关闭 SSH 通道或进程树。
pub struct VncStream {
    pub(super) reader: VncReader,
    pub(super) writer: VncWriter,
}
/// 单独交给读取线程的读端；每次返回已到达的字节，不等待填满缓冲区。
pub struct VncReader {
    control: Arc<Control>,
    prefix: VecDeque<u8>,
}
/// 可由另一线程持有的写端；缓冲满时施加背压。
pub struct VncWriter {
    control: Arc<Control>,
}
/// 可克隆的关闭句柄；不会关闭同一服务器的其他 SSH 通道。
#[derive(Clone)]
pub struct VncCloser {
    control: Arc<Control>,
}

impl VncStream {
    /// 无参数；返回可分别交给不同线程的读端、写端和关闭句柄。
    pub fn split(self) -> (VncReader, VncWriter, VncCloser) {
        let closer = self.closer();
        (self.reader, self.writer, closer)
    }
    /// 无参数；返回可跨线程取消本流的句柄。
    pub fn closer(&self) -> VncCloser {
        VncCloser {
            control: self.reader.control.clone(),
        }
    }
    /// 无参数；立即取消两端等待并关闭底层通道或进程，重复调用安全。
    pub fn close(&self) {
        self.reader.control.close();
    }

    pub(super) fn from_io(
        backend: impl Backend + 'static,
        mut input: impl Write + Send + 'static,
        mut output: impl Read + Send + 'static,
    ) -> Result<Self, AppError> {
        let control = Arc::new(Control {
            closed: AtomicBool::new(false),
            incoming: Arc::new(Pipe::default()),
            outgoing: Arc::new(Pipe::default()),
            backend: Mutex::new(Box::new(backend)),
            workers: Mutex::new(Vec::new()),
        });
        let incoming = control.incoming.clone();
        let outgoing = control.outgoing.clone();
        let weak = Arc::downgrade(&control);
        let reader = thread::Builder::new()
            .name("桌面读取".into())
            .spawn(move || {
                let mut bytes = [0; CHUNK];
                loop {
                    match output.read(&mut bytes) {
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Ok(0) | Err(_) => {
                            let error = weak.upgrade().map_or_else(closed, |c| c.failure());
                            incoming.fail(error.clone(), false);
                            outgoing.fail(error, true);
                            break;
                        }
                        Ok(n) => {
                            if incoming.write(&bytes[..n], None).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|_| closed().with("reason", "无法创建桌面读取线程"))?;
        control
            .workers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(reader);
        let incoming = control.incoming.clone();
        let outgoing = control.outgoing.clone();
        let writer = thread::Builder::new()
            .name("桌面写入".into())
            .spawn(move || {
                let mut bytes = [0; CHUNK];
                while let Ok(n) = outgoing.read(&mut bytes, None) {
                    if input
                        .write_all(&bytes[..n])
                        .and_then(|_| input.flush())
                        .is_err()
                    {
                        outgoing.fail(closed(), true);
                        incoming.fail(closed(), false);
                        break;
                    }
                    outgoing.complete(n);
                }
            })
            .map_err(|_| closed().with("reason", "无法创建桌面写入线程"))?;
        control
            .workers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(writer);
        Ok(Self {
            reader: VncReader {
                control: control.clone(),
                prefix: VecDeque::new(),
            },
            writer: VncWriter { control },
        })
    }
}
impl VncReader {
    /// 将已到达的数据写入 out，返回字节数；关闭、断线返回 vnc.stream_closed，空缓冲返回零。
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, AppError> {
        self.read_until(out, None)
    }
    pub(super) fn read_until(
        &mut self,
        out: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<usize, AppError> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.control.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        if !self.prefix.is_empty() {
            let n = out.len().min(self.prefix.len());
            for (slot, byte) in out.iter_mut().zip(self.prefix.drain(..n)) {
                *slot = byte;
            }
            return Ok(n);
        }
        self.control.incoming.read(out, deadline)
    }
    pub(super) fn unread(&mut self, bytes: &[u8]) {
        self.prefix.extend(bytes);
    }
}
impl VncWriter {
    /// 将 bytes 全部写入有界队列；关闭或断线返回 AppError，返回成功不代表服务器已处理。
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), AppError> {
        self.write_until(bytes, None)
    }
    /// 无参数；等待此前写入的数据送到底层 SSH 字节流，关闭时返回 AppError。
    pub fn flush(&mut self) -> Result<(), AppError> {
        self.control.outgoing.flush(None)
    }
    pub(super) fn flush_until(&mut self, deadline: Instant) -> Result<(), AppError> {
        self.control.outgoing.flush(Some(deadline))
    }
    pub(super) fn write_until(
        &mut self,
        bytes: &[u8],
        deadline: Option<Instant>,
    ) -> Result<(), AppError> {
        self.control.outgoing.write(bytes, deadline)
    }
}
impl VncCloser {
    /// 无参数；立即取消两端等待并关闭底层通道或进程，重复调用安全。
    pub fn close(&self) {
        self.control.close();
    }
}
