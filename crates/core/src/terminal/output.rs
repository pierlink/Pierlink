//! 屏幕和输出历史有界保留；附加订阅者不参与界面的确认背压。
use super::*;
use std::{collections::VecDeque, sync::Weak};

const HISTORY_LINES: usize = 10_000;
const HISTORY_BYTES: usize = 4 * 1024 * 1024;
const MAX_SUBSCRIBERS: usize = 32;

/// 订阅事件；lost 表示上次读取后曾因缓冲满而丢弃最旧字节或事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionEvent {
    pub event: TerminalEvent,
    pub lost: bool,
}

#[derive(Default)]
struct Queue {
    events: VecDeque<TerminalEvent>,
    bytes: usize,
    lost: bool,
    closed: bool,
}
struct Subscriber {
    queue: Mutex<Queue>,
    ready: Condvar,
    capacity: usize,
}
impl Subscriber {
    fn push(&self, event: &TerminalEvent) {
        let mut queue = lock(&self.queue);
        if queue.closed {
            return;
        }
        let mut event = event.clone();
        if let TerminalEvent::Output(bytes) = &mut event
            && bytes.len() > self.capacity
        {
            bytes.drain(..bytes.len() - self.capacity);
            queue.lost = true;
        }
        let bytes = weight(&event);
        while queue.bytes + bytes > self.capacity {
            if let Some(old) = queue.events.pop_front() {
                queue.bytes -= weight(&old);
                queue.lost = true;
            } else {
                break;
            }
        }
        queue.bytes += bytes;
        queue.closed = matches!(event, TerminalEvent::Exited { .. });
        queue.events.push_back(event);
        self.ready.notify_all();
    }
}
fn weight(event: &TerminalEvent) -> usize {
    match event {
        TerminalEvent::Output(bytes) => bytes.len().max(1),
        TerminalEvent::Exited { .. } => 1,
    }
}

/// 独立的有界订阅句柄；丢弃句柄等同取消订阅，不影响界面或其他订阅者。
pub struct OutputSubscription {
    subscriber: Arc<Subscriber>,
}
impl OutputSubscription {
    /// 最多等待 timeout 读取下一事件；超时或订阅已关闭且排空时返回 None。
    /// Duration::ZERO 表示立即查询；此接口不需要调用 TerminalManager::ack。
    pub fn recv_timeout(&self, timeout: Duration) -> Option<SubscriptionEvent> {
        let queue = lock(&self.subscriber.queue);
        let mut queue = self
            .subscriber
            .ready
            .wait_timeout_while(queue, timeout, |q| q.events.is_empty() && !q.closed)
            .unwrap_or_else(|p| p.into_inner())
            .0;
        let event = queue.events.pop_front()?;
        queue.bytes -= weight(&event);
        Some(SubscriptionEvent {
            event,
            lost: std::mem::take(&mut queue.lost),
        })
    }

    /// 无参数；立即取消订阅、清空缓冲并唤醒读取者；重复调用无操作。
    pub fn unsubscribe(&self) {
        let mut queue = lock(&self.subscriber.queue);
        queue.closed = true;
        queue.events.clear();
        queue.bytes = 0;
        self.subscriber.ready.notify_all();
    }
}
impl Drop for OutputSubscription {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

/// 原始输出的增量快照；位置为本会话累计字节偏移，不是 UTF-8 字符索引。
/// 下次查询传 next；lost 表示请求位置早于保留范围，start 是实际起点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRead {
    pub start: u64,
    pub next: u64,
    pub data: Vec<u8>,
    pub lost: bool,
    /// 最后一次非空输出至快照时刻的静止时长。
    pub quiet_for: Duration,
}

pub(super) struct Output {
    parser: vt100::Parser,
    history: VecDeque<u8>,
    newlines: usize,
    position: u64,
    subscribers: Vec<Weak<Subscriber>>,
    ended: bool,
    last_output: Instant,
}
impl Output {
    pub(super) fn new(cols: u16, rows: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, HISTORY_LINES),
            history: VecDeque::new(),
            newlines: 0,
            position: 0,
            subscribers: Vec::new(),
            ended: false,
            last_output: Instant::now(),
        }
    }
    pub(super) fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }
    fn feed(&mut self, event: &TerminalEvent) {
        self.feed_at(event, Instant::now());
    }
    fn feed_at(&mut self, event: &TerminalEvent, now: Instant) {
        self.ended |= matches!(event, TerminalEvent::Exited { .. });
        if let TerminalEvent::Output(bytes) = event {
            if !bytes.is_empty() {
                self.last_output = now;
            }
            self.parser.process(bytes);
            self.position = self.position.saturating_add(bytes.len() as u64);
            self.newlines += bytes.iter().filter(|&&b| b == b'\n').count();
            self.history.extend(bytes);
            // 行数和字节数同时限额，单行无限刷屏也不会无限增长。
            while self.history.len() > HISTORY_BYTES || self.newlines > HISTORY_LINES {
                if let Some(b) = self.history.pop_front() {
                    self.newlines -= usize::from(b == b'\n');
                }
            }
        }
        self.subscribers.retain(|weak| {
            let Some(subscriber) = weak.upgrade() else {
                return false;
            };
            subscriber.push(event);
            !lock(&subscriber.queue).closed
        });
    }
    fn subscribe(&mut self, capacity: usize) -> Result<OutputSubscription, AppError> {
        if self.ended {
            return Err(error(
                "not_found",
                "terminal.session_has_ended",
                "会话已结束",
            ));
        }
        if capacity == 0 || capacity > WINDOW {
            return Err(error(
                "invalid_buffer",
                "terminal.subscription_buffer_must_be_between_1_byte_and_1_mib",
                "订阅缓冲必须为 1 字节到 1 MiB",
            ));
        }
        self.subscribers
            .retain(|s| s.upgrade().is_some_and(|s| !lock(&s.queue).closed));
        if self.subscribers.len() >= MAX_SUBSCRIBERS {
            return Err(error(
                "subscriber_limit",
                "terminal.each_terminal_allows_at_most_32_additional_subscribers",
                "每个终端最多允许 32 个附加订阅者",
            ));
        }
        let subscriber = Arc::new(Subscriber {
            queue: Mutex::new(Queue::default()),
            ready: Condvar::new(),
            capacity,
        });
        self.subscribers.push(Arc::downgrade(&subscriber));
        Ok(OutputSubscription { subscriber })
    }
    fn since(&self, position: u64) -> Result<OutputRead, AppError> {
        self.window(position, None, 0, usize::MAX, Instant::now())
            .map(|(output, _)| output)
    }
    fn window(
        &self,
        position: u64,
        tail_bytes: Option<usize>,
        lookbehind: usize,
        limit: usize,
        now: Instant,
    ) -> Result<(OutputRead, u64), AppError> {
        if position > self.position {
            return Err(error(
                "invalid_position",
                "terminal.output_position_is_beyond_the_current_end",
                "输出位置超过当前末尾",
            )
            .with("position", position)
            .with("end", self.position));
        }
        let position = tail_bytes.map_or(position, |tail| {
            position.max(self.position.saturating_sub(tail as u64))
        });
        let oldest = self.position - self.history.len() as u64;
        let requested = position.max(oldest);
        let start = requested.saturating_sub(lookbehind as u64).max(oldest);
        let end = requested.saturating_add(limit as u64).min(self.position);
        Ok((
            OutputRead {
                start,
                next: self.position,
                lost: position < oldest,
                quiet_for: now.saturating_duration_since(self.last_output),
                data: self
                    .history
                    .range((start - oldest) as usize..(end - oldest) as usize)
                    .copied()
                    .collect(),
            },
            position,
        ))
    }
    fn recent(&self, count: usize) -> Vec<String> {
        if count == 0 {
            return Vec::new();
        }
        let mut screen = self.parser.screen().clone();
        let (rows, cols) = screen.size();
        let mut current: Vec<_> = screen.rows(0, cols).collect();
        while current.last().is_some_and(String::is_empty) {
            current.pop();
        }
        let count = count.min(HISTORY_LINES);
        if current.len() >= count {
            return current.split_off(current.len() - count);
        }
        screen.set_scrollback(usize::MAX);
        let available = screen.scrollback().min(count - current.len());
        let mut offset = available;
        let mut lines = Vec::with_capacity(available + current.len());
        while offset > 0 {
            screen.set_scrollback(offset);
            let take = offset.min(rows as usize);
            lines.extend(screen.rows(0, cols).take(take));
            offset -= take;
        }
        lines.extend(current);
        lines
    }
}

impl Session {
    pub(super) fn emit(&self, callback: &Callback, event: TerminalEvent) {
        lock(&self.output).feed(&event);
        emit(callback, event);
    }
}

impl TerminalManager {
    /// 测试中向 id 注入 bytes 和接收时刻 now；返回不存在的终端错误。
    #[cfg(test)]
    pub(crate) fn feed_output_at(
        &self,
        id: &str,
        bytes: &[u8],
        now: Instant,
    ) -> Result<(), AppError> {
        lock(&self.get(id)?.output).feed_at(&TerminalEvent::Output(bytes.to_vec()), now);
        Ok(())
    }

    /// 从订阅时刻开始接收 id 的输出和退出事件；capacity 为缓冲字节上限（1 到 1 MiB）。
    /// 满时丢最旧数据并设置 lost，最多 32 个附加订阅；取消使用句柄的 unsubscribe 或 Drop。
    pub fn subscribe(&self, id: &str, capacity: usize) -> Result<OutputSubscription, AppError> {
        lock(&self.get(id)?.output).subscribe(capacity)
    }

    /// 返回 id 的当前可见屏幕文字，解析 ANSI、光标移动和备用屏幕；不触发控制序列副作用。
    pub fn screen_text(&self, id: &str) -> Result<String, AppError> {
        Ok(lock(&self.get(id)?.output).parser.screen().contents())
    }

    /// 返回 id 最近 count 行已渲染文字（回滚加当前屏幕），最多 10000 行；不包含末尾空屏行。
    pub fn recent_lines(&self, id: &str, count: usize) -> Result<Vec<String>, AppError> {
        Ok(lock(&self.get(id)?.output).recent(count))
    }

    /// 返回 id 当前累计输出字节数；不复制历史，不存在的终端返回错误。
    pub(crate) fn output_position(&self, id: &str) -> Result<u64, AppError> {
        Ok(lock(&self.get(id)?.output).position)
    }

    /// 从 position 或 tail_bytes 限定的尾部起点读取 id 的历史窗口。
    /// 起点前最多附带 lookbehind 字节、起点后最多 limit 字节；返回窗口和实际请求位置。
    /// now 用于计算静止时长；next 仍是完整历史的末尾，lost 表示请求位置已过期，未来位置返回错误。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn output_window(
        &self,
        id: &str,
        position: u64,
        tail_bytes: Option<usize>,
        lookbehind: usize,
        limit: usize,
        now: Instant,
    ) -> Result<(OutputRead, u64), AppError> {
        lock(&self.get(id)?.output).window(position, tail_bytes, lookbehind, limit, now)
    }

    /// 从 position 字节偏移读取 id 的新输出；返回原始字节及下次位置，过期位置通过 lost 标记。
    /// 历史同时限为最近 10000 个换行及 4 MiB；未来位置报错，位置仅适用于本会话。
    pub fn output_since(&self, id: &str, position: u64) -> Result<OutputRead, AppError> {
        lock(&self.get(id)?.output).since(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn feed(output: &mut Output, bytes: &[u8]) {
        output.feed(&TerminalEvent::Output(bytes.to_vec()));
    }

    #[test]
    fn 历史快照静止时长只随非空输出重置() {
        let mut output = Output::new(80, 24);
        let now = Instant::now();
        output.feed_at(&TerminalEvent::Output(b"a".to_vec()), now);
        let later = now + Duration::from_secs(1);
        output.feed_at(&TerminalEvent::Output(Vec::new()), later);
        assert_eq!(
            output.window(0, None, 0, 10, later).unwrap().0.quiet_for,
            Duration::from_secs(1)
        );
        output.feed_at(&TerminalEvent::Output(b"b".to_vec()), later);
        assert_eq!(
            output.window(0, None, 0, 10, later).unwrap().0.quiet_for,
            Duration::ZERO
        );
    }

    #[test]
    fn 慢订阅丢最旧并标记且取消不影响其他订阅() {
        let mut output = Output::new(80, 24);
        let slow = output.subscribe(3).unwrap();
        let fast = output.subscribe(32).unwrap();
        feed(&mut output, b"ab");
        assert!(!fast.recv_timeout(Duration::ZERO).unwrap().lost);
        feed(&mut output, b"cd");
        assert_eq!(
            slow.recv_timeout(Duration::ZERO).unwrap(),
            SubscriptionEvent {
                event: TerminalEvent::Output(b"cd".to_vec()),
                lost: true
            }
        );
        assert!(!fast.recv_timeout(Duration::ZERO).unwrap().lost);
        feed(&mut output, b"12345");
        assert_eq!(
            slow.recv_timeout(Duration::ZERO).unwrap().event,
            TerminalEvent::Output(b"345".to_vec())
        );
        slow.unsubscribe();
        slow.unsubscribe();
        feed(&mut output, b"end");
        assert!(slow.recv_timeout(Duration::ZERO).is_none());
        output.feed(&TerminalEvent::Exited {
            code: Some(7),
            closed_by_user: false,
            hooked: false,
        });
        assert!(matches!(
            fast.recv_timeout(Duration::ZERO).unwrap().event,
            TerminalEvent::Output(_)
        ));
        assert!(matches!(
            fast.recv_timeout(Duration::ZERO).unwrap().event,
            TerminalEvent::Output(_)
        ));
        assert!(matches!(
            fast.recv_timeout(Duration::ZERO).unwrap().event,
            TerminalEvent::Exited { code: Some(7), .. }
        ));
        assert!(fast.recv_timeout(Duration::from_secs(1)).is_none());
        assert!(output.subscribe(1).is_err());
    }

    #[test]
    fn 屏幕解析跨块宽字符光标覆盖及备用屏幕() {
        let mut output = Output::new(20, 3);
        for byte in "中文😀\r\nnormal".as_bytes() {
            feed(&mut output, &[*byte]);
        }
        assert_eq!(output.parser.screen().contents(), "中文😀\nnormal");
        feed(
            &mut output,
            b"\x1b[?1049h\x1b[H\x1b[2Jtop\x1b[2;1Hload\rCPU\x1b[K",
        );
        assert_eq!(output.parser.screen().contents(), "top\nCPU");
        feed(&mut output, b"\x1b[?1049l");
        assert_eq!(output.parser.screen().contents(), "中文😀\nnormal");
        output.resize(30, 5);
        assert_eq!(output.parser.screen().size(), (5, 30));
        assert!(output.parser.screen().contents().contains("normal"));
    }

    #[test]
    fn 最近行与增量位置有界并报告过期() {
        let mut output = Output::new(20, 3);
        feed(&mut output, b"one\r\ntwo\r\nthree\r\nfour");
        assert_eq!(output.recent(3), ["two", "three", "four"]);
        assert_eq!(output.recent(20), ["one", "two", "three", "four"]);
        assert!(output.recent(0).is_empty());
        let read = output.since(0).unwrap();
        feed(&mut output, b"!");
        assert_eq!(output.since(read.next).unwrap().data, b"!");
        assert!(output.since(output.position + 1).is_err());
        for _ in 0..HISTORY_LINES + 1 {
            feed(&mut output, b"x\r\n");
        }
        assert!(output.recent(usize::MAX).len() <= HISTORY_LINES);
        assert!(output.since(0).unwrap().lost);
        feed(&mut output, &vec![b'x'; HISTORY_BYTES + 1]);
        assert_eq!(output.history.len(), HISTORY_BYTES);
        assert!(output.since(0).unwrap().lost);
        assert!(output.since(output.position).unwrap().data.is_empty());
    }

    #[test]
    fn 订阅数量和容量有上限且丢弃句柄释放名额() {
        let mut output = Output::new(80, 24);
        assert!(output.subscribe(0).is_err());
        assert!(output.subscribe(WINDOW + 1).is_err());
        let subscriptions: Vec<_> = (0..MAX_SUBSCRIBERS)
            .map(|_| output.subscribe(1).unwrap())
            .collect();
        assert!(output.subscribe(1).is_err());
        drop(subscriptions);
        assert!(output.subscribe(1).is_ok());
    }
    #[test]
    fn 历史窗口限制复制范围并区分过期和分页() {
        let mut output = Output::new(80, 24);
        feed(&mut output, b"0123456789");
        let (window, requested) = output.window(5, None, 2, 3, Instant::now()).unwrap();
        assert_eq!(requested, 5);
        assert_eq!(window.start, 3);
        assert_eq!(window.next, 10);
        assert_eq!(window.data, b"34567");
        assert!(!window.lost);
        let (window, requested) = output.window(0, Some(2), 1, 4, Instant::now()).unwrap();
        assert_eq!(requested, 8);
        assert_eq!(window.data, b"789");
        assert!(output.window(11, Some(0), 0, 1, Instant::now()).is_err());
        // 模拟滚动窗口已经丢弃前五个字节，校验附带上下文不掩盖 lost。
        output.history.drain(..5);
        let (window, requested) = output.window(0, None, 4, 2, Instant::now()).unwrap();
        assert_eq!(requested, 0);
        assert_eq!(window.start, 5);
        assert_eq!(window.data, b"56");
        assert!(window.lost);
        let (window, _) = output.window(7, None, 4, 2, Instant::now()).unwrap();
        assert!(!window.lost);
        assert_eq!(window.data, b"5678");
    }
}
