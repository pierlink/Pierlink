//! OpenSSH 的私有临时日志；仅保留活动进程的文件，退出和启动清理时删除。
//! 日志内容只能进入终端输出，不进入应用日志或错误参数。
use super::*;
use std::{
    collections::VecDeque,
    fs::File,
    io::{Seek, SeekFrom},
    path::Path,
};

pub(super) struct Log {
    path: tempfile::TempPath,
    offset: u64,
    parser: Parser,
    events: VecDeque<TerminalEvent>,
}

impl Log {
    pub(super) fn create(directory: &Path) -> Option<Self> {
        std::fs::create_dir_all(directory).ok()?;
        let file = tempfile::Builder::new()
            .prefix("terminal-")
            .suffix(".log")
            .tempfile_in(directory)
            .ok()?;
        // 在 ssh 开始写入前限制权限；失败时丢弃空文件并照常连接。
        #[cfg(windows)]
        crate::keys::windows::permissions(file.path(), false).ok()?;
        file.path().to_str()?;
        Some(Self {
            path: file.into_temp_path(),
            offset: 0,
            parser: Parser::default(),
            events: VecDeque::new(),
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    fn read(&mut self, finished: bool) -> bool {
        // 每次重新打开，允许 Windows 启动初期文件暂时不存在，并及时释放共享句柄。
        let mut bytes = [0; CHUNK];
        let read = File::open(&self.path).and_then(|mut file| {
            file.seek(SeekFrom::Start(self.offset))?;
            file.read(&mut bytes)
        });
        match read {
            Ok(n) if n > 0 => {
                self.offset += n as u64;
                self.parser.feed(&bytes[..n], &mut self.events);
                false
            }
            _ => {
                if finished {
                    self.parser.finish(&mut self.events);
                }
                true
            }
        }
    }

    pub(super) fn pump(&mut self, session: &Session, callback: &Callback, finished: bool) -> bool {
        let eof = self.events.is_empty() && self.read(finished);
        while let Some(event) = self.events.pop_front() {
            let event = match event {
                TerminalEvent::Output(mut bytes) => {
                    let mut flow = lock(&session.flow);
                    let capacity = if flow.closed {
                        CHUNK
                    } else {
                        WINDOW.saturating_sub(flow.pending + flow.reading)
                    };
                    if capacity == 0 {
                        self.events.push_front(TerminalEvent::Output(bytes));
                        return false;
                    }
                    if bytes.len() > capacity {
                        let tail = bytes.split_off(capacity);
                        self.events.push_front(TerminalEvent::Output(tail));
                    }
                    flow.pending += bytes.len();
                    TerminalEvent::Output(bytes)
                }
                stage => stage,
            };
            session.emit(callback, event);
        }
        finished && eof
    }
}

#[derive(Default)]
struct Parser {
    line: Vec<u8>,
    last: Option<ConnectStage>,
    // 超长行分块回显，调试行整行丢弃；内存不随日志增长。
    continued: Option<bool>,
}

impl Parser {
    fn feed(&mut self, bytes: &[u8], events: &mut VecDeque<TerminalEvent>) {
        for &byte in bytes {
            if byte == b'\n' {
                self.line(events, true);
                self.continued = None;
            } else {
                self.line.push(byte);
                if self.line.len() == CHUNK {
                    // 留下可能属于 CRLF 的末尾 CR，下一块才能准确规范化行尾。
                    let cr = self.line.last() == Some(&b'\r');
                    if cr {
                        self.line.pop();
                    }
                    self.line(events, false);
                    if cr {
                        self.line.push(b'\r');
                    }
                }
            }
        }
    }

    fn finish(&mut self, events: &mut VecDeque<TerminalEvent>) {
        if !self.line.is_empty() || self.continued.is_some() {
            self.line(events, true);
            self.continued = None;
        }
    }

    fn stage(&mut self, stage: ConnectStage, events: &mut VecDeque<TerminalEvent>) {
        if self.last.is_none_or(|last| stage > last) {
            self.last = Some(stage);
            events.push_back(TerminalEvent::Stage(stage));
        }
    }

    fn line(&mut self, events: &mut VecDeque<TerminalEvent>, complete: bool) {
        let mut line = std::mem::take(&mut self.line);
        if complete && line.last() == Some(&b'\r') {
            line.pop();
        }
        let debug = self.continued.unwrap_or_else(|| {
            [b"debug1:".as_slice(), b"debug2:", b"debug3:"]
                .iter()
                .any(|p| line.starts_with(p))
        });
        if debug && self.continued.is_none() && self.last != Some(ConnectStage::Ready) {
            // 只识别调试前缀后的消息，普通诊断中的相同文字不能推进阶段。
            let message = String::from_utf8_lossy(&line[7..]);
            let message = message.trim_start();
            if message.starts_with("Connecting to ") && message.contains(" port ")
                || message.starts_with("Executing proxy command")
            {
                self.stage(ConnectStage::Connect, events);
            } else if message.starts_with("Connection established.")
                || message.starts_with("Authenticating to ")
            {
                self.stage(ConnectStage::Auth, events);
            } else if message.starts_with("Authentication succeeded")
                || message.starts_with("Authenticated to ")
            {
                self.stage(ConnectStage::Session, events);
            } else if message.starts_with("Entering interactive session.") {
                self.stage(ConnectStage::Ready, events);
            } else if message.starts_with("mux_client_request_session: master session id:") {
                // 请求成功后才算就绪；发送请求或复用失败不能提前点亮。
                self.stage(ConnectStage::Session, events);
                self.stage(ConnectStage::Ready, events);
            } else if message.starts_with("mux_client_request_session: entering") {
                self.stage(ConnectStage::Session, events);
            }
        }
        if !debug {
            // 新版 OpenSSH 的认证成功消息也可能是非调试级别，仍须原样回显。
            if self.last != Some(ConnectStage::Ready)
                && self.continued.is_none()
                && (line.starts_with(b"Authenticated to ")
                    || line.starts_with(b"Authentication succeeded"))
            {
                self.stage(ConnectStage::Session, events);
            }
            if complete {
                line.extend_from_slice(b"\r\n");
            }
            if !line.is_empty() {
                events.push_back(TerminalEvent::Output(line));
            }
        }
        if !complete {
            self.continued = Some(debug);
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, unix))]
mod real_tests;
