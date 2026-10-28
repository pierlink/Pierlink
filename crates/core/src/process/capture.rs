//! 持续排空双流，内存只保留固定大小的开头与尾部。
use super::*;
use crate::mcp::{RunLimits, RunOutput};
use std::collections::VecDeque;

pub(crate) struct HeadTail {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    limit: usize,
    total: u64,
}
impl HeadTail {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            limit,
            total: 0,
        }
    }
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        let count = bytes
            .len()
            .min((self.limit / 2).saturating_sub(self.head.len()));
        self.head.extend_from_slice(&bytes[..count]);
        let bytes = &bytes[count..];
        let capacity = self.limit - self.limit / 2;
        if bytes.len() >= capacity {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - capacity..]);
        } else {
            let discard = (self.tail.len() + bytes.len()).saturating_sub(capacity);
            self.tail.drain(..discard);
            self.tail.extend(bytes);
        }
    }
    pub(crate) fn snapshot(&self) -> (Vec<u8>, bool) {
        let truncated = self.total > self.limit as u64;
        let mut head = self.head.clone();
        let mut tail: Vec<_> = self.tail.iter().copied().collect();
        if truncated {
            // 丢掉截断处不完整的 UTF-8 字符，与 truncate_head_tail 的边界语义一致。
            if let Err(e) = std::str::from_utf8(&head)
                && e.error_len().is_none()
            {
                head.truncate(e.valid_up_to());
            }
            let skip = tail.iter().take_while(|b| **b & 0xc0 == 0x80).count();
            tail.drain(..skip);
            let omitted = self.total - head.len() as u64 - tail.len() as u64;
            head.extend_from_slice(format!("\n…（省略 {omitted} 字节）…\n").as_bytes());
        }
        head.extend(tail);
        (head, truncated)
    }
}

type Matcher = fn(&[u8]) -> Option<&'static str>;
impl Reader {
    pub(crate) fn head_tail(
        input: impl Read + Send + 'static,
        limit: usize,
        matcher: Option<Matcher>,
    ) -> Result<Self, AppError> {
        let state = Arc::new(Mutex::new(Capture::default()));
        let shared = state.clone();
        let handle = thread::Builder::new()
            .name("命令输出读取".into())
            .spawn(move || {
                let mut input = input;
                let mut buffer = HeadTail::new(limit);
                let mut chunk = [0; 8192];
                let mut overlap = Vec::new();
                loop {
                    let read = input.read(&mut chunk);
                    let Ok(mut state) = shared.lock() else { return };
                    match read {
                        Ok(0) => {
                            state.done = true;
                            return;
                        }
                        Ok(n) => {
                            buffer.push(&chunk[..n]);
                            (state.bytes, state.exceeded) = buffer.snapshot();
                            if let Some(matcher) = matcher {
                                overlap.extend_from_slice(&chunk[..n]);
                                state.matched_error =
                                    state.matched_error.or_else(|| matcher(&overlap));
                                overlap.drain(..overlap.len().saturating_sub(256));
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            state.failed = true;
                            state.done = true;
                            return;
                        }
                    }
                }
            })
            .map_err(|_| io_error("创建读取线程"))?;
        Ok(Self {
            state,
            handle: Some(handle),
        })
    }
}

pub(crate) fn collect_tree(
    process: &mut TreeProcess,
    limits: RunLimits,
    cancelled: impl Fn() -> bool,
    matcher: Option<Matcher>,
) -> Result<(RunOutput, Option<&'static str>), AppError> {
    let started = Instant::now();
    drop(process.child.stdin.take());
    let mut out = Reader::head_tail(
        process
            .child
            .stdout
            .take()
            .ok_or_else(|| io_error("获取标准输出"))?,
        limits.output_bytes,
        None,
    )?;
    let mut err = match Reader::head_tail(
        process
            .child
            .stderr
            .take()
            .ok_or_else(|| io_error("获取错误输出"))?,
        limits.output_bytes,
        matcher,
    ) {
        Ok(reader) => reader,
        Err(e) => {
            let _ = process.terminate();
            out.join();
            return Err(e);
        }
    };
    let result = (|| {
        let mut output = RunOutput::default();
        let mut exited = None;
        loop {
            if cancelled() || started.elapsed() >= limits.timeout {
                output.timed_out = true;
                break;
            }
            let stdout = out.state.lock().map_err(|_| io_error("读取标准输出"))?;
            let stderr = err.state.lock().map_err(|_| io_error("读取错误输出"))?;
            if stdout.failed || stderr.failed {
                return Err(io_error("读取命令管道"));
            }
            if let Some(status) = process.try_wait()? {
                output.exit_code = status.code();
                // 已退出的 shell 可能留下持有管道的后台进程；给予排空时间后清理整棵树。
                if (stdout.done && stderr.done)
                    || exited.get_or_insert_with(Instant::now).elapsed()
                        >= Duration::from_millis(100)
                {
                    break;
                }
            }
            drop(stdout);
            drop(stderr);
            thread::sleep(Duration::from_millis(10));
        }
        Ok(output)
    })();
    let cleanup = Instant::now();
    let terminated = process.terminate();
    for reader in [&mut out, &mut err] {
        if let Some(handle) = reader.handle.take() {
            join_bounded(
                handle,
                Duration::from_secs(5).saturating_sub(cleanup.elapsed()),
            );
        }
    }
    let mut output = result?;
    terminated?;
    let stdout = out.state.lock().map_err(|_| io_error("读取标准输出"))?;
    let stderr = err.state.lock().map_err(|_| io_error("读取错误输出"))?;
    if stdout.failed || stderr.failed {
        return Err(io_error("读取命令管道"));
    }
    output.stdout = stdout.bytes.clone();
    output.stderr = stderr.bytes.clone();
    output.truncated = stdout.exceeded || stderr.exceeded;
    output.duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    Ok((output, stderr.matched_error))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 逐块截断与整块截断相同且内存有界() {
        let bytes = "头甲乙𠀀丙丁尾".repeat(1000).into_bytes();
        for limit in 0..80 {
            let mut capture = HeadTail::new(limit);
            for chunk in bytes.chunks(7) {
                capture.push(chunk);
                assert!(capture.head.len() + capture.tail.len() <= limit);
            }
            assert_eq!(
                capture.snapshot(),
                crate::mcp::text::truncate_head_tail(&bytes, limit)
            );
        }
        let mut capture = HeadTail::new(10);
        capture.push(b"abc");
        assert_eq!(capture.snapshot(), (b"abc".to_vec(), false));
    }
}
