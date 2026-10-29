//! 独立线程收发有界数据包；按请求编号匹配响应，并限制在途请求和等待时间。
use super::codec::{Decoder, MAX_PACKET, protocol};
use crate::{
    error::AppError,
    process::{Reader, SshProcess, SshProgram, join_bounded},
    ssh::args::SshCommand,
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(super) struct Transport {
    backend: Backend,
    input: Option<mpsc::SyncSender<Vec<u8>>>,
    output: Option<mpsc::Receiver<Result<Vec<u8>, AppError>>>,
    workers: Vec<JoinHandle<()>>,
    pending: BTreeMap<u32, Pending>,
    #[cfg(test)]
    pub max_pending: usize,
    stderr: Reader,
    timeout: Duration,
    broken: bool,
}
fn lost(reason: &str) -> AppError {
    AppError::new("sftp.connection_lost").with("reason", reason)
}
enum Backend {
    Process(SshProcess),
    Builtin(pierlink_ssh_engine::Session),
}
impl Backend {
    fn abort(&mut self) {
        match self {
            Self::Process(process) => {
                let _ = process.terminate();
            }
            Self::Builtin(session) => session.closer().cancel(),
        }
    }
}
pub(super) const WINDOW: usize = 8;
struct Pending {
    sent: Instant,
    reply: Option<Vec<u8>>,
}
fn read_packet(output: &mut impl Read) -> Result<Vec<u8>, AppError> {
    let mut length = [0; 4];
    output
        .read_exact(&mut length)
        .map_err(|_| lost("读取子系统管道失败"))?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_PACKET {
        return Err(protocol("响应包长度超限"));
    }
    let mut packet = vec![0; length];
    output
        .read_exact(&mut packet)
        .map_err(|_| lost("响应未完整到达，连接已断开"))?;
    Ok(packet)
}
impl Transport {
    pub fn open(
        program: &SshProgram,
        command: &SshCommand,
        timeout: Duration,
    ) -> Result<Self, AppError> {
        if timeout.is_zero() {
            return Err(protocol("超时必须大于零"));
        }
        if let Some(command) = &command.builtin {
            let mut request = command.request();
            request.class = crate::builtin::ConnectionClass::Bulk;
            let cancelled = request.cancelled.clone();
            let session = command.manager.open_session(request, |connection| {
                connection.sftp_subsystem(cancelled.clone(), timeout)
            })?;
            return Self::builtin(session, timeout);
        }
        let mut process = program.spawn(command)?;
        let stdin = process
            .child
            .stdin
            .take()
            .ok_or_else(|| lost("缺少子系统输入管道"))?;
        let stdout = process
            .child
            .stdout
            .take()
            .ok_or_else(|| lost("缺少子系统输出管道"))?;
        let stderr = process
            .child
            .stderr
            .take()
            .ok_or_else(|| lost("缺少错误管道"))?;
        Self::streams(Backend::Process(process), stdin, stdout, stderr, timeout)
    }
    fn builtin(
        mut session: pierlink_ssh_engine::Session,
        timeout: Duration,
    ) -> Result<Self, AppError> {
        let (stdin, stdout, stderr) = session.take_io();
        Self::streams(Backend::Builtin(session), stdin, stdout, stderr, timeout)
    }
    fn streams(
        mut backend: Backend,
        mut stdin: impl Write + Send + 'static,
        mut stdout: impl Read + Send + 'static,
        stderr: impl Read + Send + 'static,
        timeout: Duration,
    ) -> Result<Self, AppError> {
        let mut stderr =
            Reader::tail(stderr, 8192).map_err(|_| lost("创建错误管道读取线程失败"))?;
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(WINDOW);
        let (results, output) = mpsc::sync_channel(WINDOW);
        let errors = results.clone();
        let writer = match thread::Builder::new()
            .name("SFTP发送".into())
            .spawn(move || {
                while let Ok(packet) = rx.recv() {
                    if stdin
                        .write_all(&(packet.len() as u32).to_be_bytes())
                        .and_then(|_| stdin.write_all(&packet))
                        .and_then(|_| stdin.flush())
                        .is_err()
                    {
                        let _ = errors.send(Err(lost("写入子系统管道失败")));
                        break;
                    }
                }
            }) {
            Ok(writer) => writer,
            Err(_) => {
                backend.abort();
                stderr.join();
                return Err(lost("创建子系统发送线程失败"));
            }
        };
        let reader = match thread::Builder::new()
            .name("SFTP接收".into())
            .spawn(move || {
                loop {
                    let packet = read_packet(&mut stdout);
                    let failed = packet.is_err();
                    if results.send(packet).is_err() || failed {
                        break;
                    }
                }
            }) {
            Ok(reader) => reader,
            Err(_) => {
                drop(tx);
                drop(output);
                backend.abort();
                join_bounded(writer, Duration::from_secs(2));
                stderr.join();
                return Err(lost("创建子系统接收线程失败"));
            }
        };
        Ok(Self {
            backend,
            input: Some(tx),
            output: Some(output),
            workers: vec![writer, reader],
            pending: BTreeMap::new(),
            #[cfg(test)]
            max_pending: 0,
            stderr,
            timeout,
            broken: false,
        })
    }
    #[cfg(all(test, unix))]
    pub(super) fn disconnect(&self) -> Box<dyn Fn()> {
        match &self.backend {
            Backend::Process(process) => {
                let pid = process.child.id();
                Box::new(move || unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                })
            }
            Backend::Builtin(session) => {
                let closer = session.closer();
                Box::new(move || closer.cancel())
            }
        }
    }
    fn send_packet(&mut self, packet: Vec<u8>) -> Result<(), AppError> {
        if self.broken {
            return Err(lost("子系统连接已失效"));
        }
        if packet.is_empty() || packet.len() > MAX_PACKET {
            return Err(protocol("请求包长度超限"));
        }
        self.input
            .as_ref()
            .ok_or_else(|| lost("输入管道已关闭"))?
            .send(packet)
            .map_err(|_| lost("子系统已退出"))
    }
    fn receive_packet(&self, timeout: Duration) -> Result<Vec<u8>, AppError> {
        self.output
            .as_ref()
            .ok_or_else(|| lost("输出管道已关闭"))?
            .recv_timeout(timeout)
            .map_err(|_| lost("子系统请求超时或进程已退出"))?
    }
    pub fn send(&mut self, id: u32, packet: Vec<u8>) -> Result<(), AppError> {
        if self.pending.len() >= WINDOW || self.pending.contains_key(&id) {
            self.abort();
            return Err(protocol("在途请求超过窗口或请求编号重复"));
        }
        if let Err(error) = self.send_packet(packet) {
            self.abort();
            return Err(error);
        }
        self.pending.insert(
            id,
            Pending {
                sent: Instant::now(),
                reply: None,
            },
        );
        #[cfg(test)]
        {
            self.max_pending = self.max_pending.max(self.pending.len());
        }
        Ok(())
    }
    pub fn receive(&mut self, id: u32) -> Result<Vec<u8>, AppError> {
        let result = self.receive_matching(id);
        if result.is_err() {
            self.abort();
        }
        result
    }
    fn receive_matching(&mut self, id: u32) -> Result<Vec<u8>, AppError> {
        loop {
            let pending = self
                .pending
                .get_mut(&id)
                .ok_or_else(|| lost("请求不存在或子系统连接已失效"))?;
            if let Some(reply) = pending.reply.take() {
                self.pending.remove(&id);
                return Ok(reply);
            }
            // 以最早未响应请求的剩余时限等待，乱序响应不能无限延长请求寿命。
            let remaining = self
                .pending
                .values()
                .filter(|p| p.reply.is_none())
                .map(|p| self.timeout.saturating_sub(p.sent.elapsed()))
                .min()
                .unwrap_or(self.timeout);
            // 即使调用方处理输出较慢，也先取出已经到达的响应；零时限不会再阻塞。
            let packet = self.receive_packet(remaining)?;
            let mut input = Decoder(&packet);
            input.byte()?;
            let response_id = input.u32()?;
            let pending = self
                .pending
                .get_mut(&response_id)
                .ok_or_else(|| protocol("响应请求编号不匹配"))?;
            if pending.reply.is_some() {
                return Err(protocol("响应请求编号重复"));
            }
            pending.reply = Some(packet);
        }
    }
    pub fn drain(&mut self) -> Result<(), AppError> {
        while let Some(&id) = self.pending.keys().next() {
            self.receive(id)?;
        }
        Ok(())
    }
    pub fn abort(&mut self) {
        self.broken = true;
        self.input.take();
        self.output.take();
        self.pending.clear();
        self.backend.abort();
    }
    /// 握手失败时把 ssh 的错误输出转成具体错误（登录失败、连不上、服务器没开 SFTP 等）；
    /// 错误输出里没有可识别的内容时原样返回 error。
    fn explain(&mut self, error: AppError) -> AppError {
        if error.code != "sftp.connection_lost" {
            return error;
        }
        self.abort();
        self.stderr.join();
        let Backend::Process(process) = &mut self.backend else {
            // 内置通道的错误输出属于不可信内容，不用于推断认证结果或返回给前端。
            return error;
        };
        let text = match self.stderr.state.lock() {
            Ok(capture) => String::from_utf8_lossy(&capture.bytes).into_owned(),
            Err(_) => return error,
        };
        let classified = process.finish_error(crate::remote::run::classify_error(None, &text, &[]));
        if matches!(
            classified.code.as_str(),
            "ssh.password_needed"
                | "ssh.two_factor_required"
                | "ssh.auth_failed"
                | "ssh.too_many_auth_attempts"
        ) {
            return classified;
        }
        if text.contains("subsystem request failed") {
            return AppError::new("sftp.unavailable");
        }
        if classified.code == "ssh.failed" && text.trim().is_empty() {
            error
        } else {
            classified
        }
    }
    pub fn init(&mut self) -> Result<bool, AppError> {
        let result = self
            .send_packet(vec![1, 0, 0, 0, 3])
            .and_then(|()| self.receive_packet(self.timeout));
        let response = match result {
            Ok(response) => response,
            Err(e) => return Err(self.explain(e)),
        };
        let mut input = Decoder(&response);
        if input.byte()? != 2 || input.u32()? != 3 {
            return Err(protocol("服务器未协商 SFTP 第三版"));
        }
        let mut posix_rename = false;
        while !input.0.is_empty() {
            let name = input.string()?;
            let value = input.string()?;
            posix_rename |= name == b"posix-rename@openssh.com" && value == b"1";
        }
        Ok(posix_rename)
    }
}
impl Drop for Transport {
    fn drop(&mut self) {
        self.abort();
        for worker in self.workers.drain(..) {
            join_bounded(worker, Duration::from_secs(2));
        }
        self.stderr.join();
    }
}
