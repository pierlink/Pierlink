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
        TerminalEvent::Exited { .. } | TerminalEvent::Stage(_) => 1,
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
    /// 屏幕副本的尺寸：解析器出错要重建时用（出错的解析器自己报的尺寸不可信）
    size: (u16, u16),
    /// 测试用：下一次解析时让解析器出错
    #[cfg(test)]
    fail_next: bool,
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
            size: (rows, cols),
            #[cfg(test)]
            fail_next: false,
            history: VecDeque::new(),
            newlines: 0,
            position: 0,
            subscribers: Vec::new(),
            ended: false,
            last_output: Instant::now(),
        }
    }
    pub(super) fn resize(&mut self, cols: u16, rows: u16) {
        self.size = (rows, cols);
        let parser = &mut self.parser;
        let resized = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parser.screen_mut().set_size(rows, cols);
        }));
        if resized.is_err() {
            self.rebuild_parser("改尺寸");
        }
    }
    /// 把输出喂给屏幕副本。解析器（vt100）出错时换一个新的接着用：出错会让分发线程退出，
    /// 读输出、写输入、改尺寸跟着全停，整个终端卡死（1.2.1 用户遇到过）；副本丢了只影响读屏
    fn process(&mut self, bytes: &[u8]) {
        #[cfg(test)]
        let fail = std::mem::take(&mut self.fail_next);
        #[cfg(not(test))]
        let fail = false;
        let parser = &mut self.parser;
        let processed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if fail {
                panic!("测试：解析器出错");
            }
            parser.process(bytes);
        }));
        if processed.is_err() {
            self.rebuild_parser("解析输出");
        }
    }
    fn rebuild_parser(&mut self, action: &str) {
        let (rows, cols) = self.size;
        // 程序打开过的模式带到新副本上：程序不会再发一遍，丢了的话手机的括号粘贴、往上翻（全屏、鼠标）都会错。
        // 这几个只是读字段，出过错的解析器也能读
        let old = self.parser.screen();
        let mut modes = Vec::new();
        for (on, seq) in [
            (old.alternate_screen(), b"\x1b[?1049h".as_slice()),
            (old.bracketed_paste(), b"\x1b[?2004h"),
            (old.application_cursor(), b"\x1b[?1h"),
            (old.application_keypad(), b"\x1b="),
        ] {
            if on {
                modes.extend_from_slice(seq);
            }
        }
        modes.extend_from_slice(match old.mouse_protocol_mode() {
            vt100::MouseProtocolMode::None => b"".as_slice(),
            vt100::MouseProtocolMode::Press => b"\x1b[?9h",
            vt100::MouseProtocolMode::PressRelease => b"\x1b[?1000h",
            vt100::MouseProtocolMode::ButtonMotion => b"\x1b[?1002h",
            vt100::MouseProtocolMode::AnyMotion => b"\x1b[?1003h",
        });
        modes.extend_from_slice(match old.mouse_protocol_encoding() {
            vt100::MouseProtocolEncoding::Default => b"".as_slice(),
            vt100::MouseProtocolEncoding::Utf8 => b"\x1b[?1005h",
            vt100::MouseProtocolEncoding::Sgr => b"\x1b[?1006h",
        });
        eprintln!("终端屏幕副本出错，已重建: 动作={action}, 尺寸={cols}x{rows}");
        self.parser = vt100::Parser::new(rows, cols, HISTORY_LINES);
        self.parser.process(&modes);
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
            self.process(bytes);
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
        self.recent_rendered(count, false)
    }
    fn recent_rendered(&self, count: usize, formatted: bool) -> Vec<String> {
        if count == 0 {
            return Vec::new();
        }
        let mut screen = self.parser.screen().clone();
        let (rows, cols) = screen.size();
        let mut current: Vec<_> = screen.rows(0, cols).collect();
        while current.last().is_some_and(String::is_empty) {
            current.pop();
        }
        if formatted {
            current = (0..current.len())
                .map(|row| formatted_row(&screen, row as u16))
                .collect();
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
            if formatted {
                lines.extend((0..take).map(|row| formatted_row(&screen, row as u16)));
            } else {
                lines.extend(screen.rows(0, cols).take(take));
            }
            offset -= take;
        }
        lines.extend(current);
        lines
    }
}

/// 和电脑终端里滚一格滚轮一样（xterm.js 的做法）：不在备用屏时返回 None；
/// 程序接管了鼠标就按它要的编码发一个滚轮事件，位置在横向中间、纵向上三分之一
/// （Codex 的对话在输入框上面）；没接管鼠标就发一个上或下方向键（应用光标模式用 ESC O）。
fn wheel_sequence(screen: &vt100::Screen, up: bool) -> Option<Vec<u8>> {
    if !screen.alternate_screen() {
        return None;
    }
    let (rows, cols) = screen.size();
    // X10 模式（?9h）只报按键按下、没有滚轮事件，和 xterm.js 一样按没接管处理
    if matches!(
        screen.mouse_protocol_mode(),
        vt100::MouseProtocolMode::None | vt100::MouseProtocolMode::Press
    ) {
        let prefix = if screen.application_cursor() {
            b'O'
        } else {
            b'['
        };
        return Some(vec![0x1b, prefix, if up { b'A' } else { b'B' }]);
    }
    // 滚轮上、下是按键 64、65；坐标从 1 开始
    let button = if up { 64 } else { 65 };
    let (col, row) = (u32::from(cols / 2).max(1), u32::from(rows / 3).max(1));
    Some(match screen.mouse_protocol_encoding() {
        vt100::MouseProtocolEncoding::Sgr => format!("\x1b[<{button};{col};{row}M").into_bytes(),
        encoding => {
            // 默认编码每个值加 32 放进一个字节；UTF-8 编码大于 127 时写成 UTF-8 字符
            let mut out = b"\x1b[M".to_vec();
            for value in [button, col, row] {
                let value = value + 32;
                if encoding == vt100::MouseProtocolEncoding::Utf8 {
                    let ch = char::from_u32(value.min(2047)).unwrap_or(' ');
                    out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                } else {
                    out.push(value.min(255) as u8);
                }
            }
            out
        }
    })
}

// 只按已渲染单元格生成 SGR，不转发原始终端控制序列。
fn formatted_row(screen: &vt100::Screen, row: u16) -> String {
    let cols = screen.size().1;
    let end = (0..cols)
        .rfind(|&col| {
            screen
                .cell(row, col)
                .is_some_and(|cell| cell.has_contents())
        })
        .map_or(0, |col| col + 1);
    let mut result = String::from("\x1b[0m");
    let mut previous = String::from("0");
    for col in 0..end {
        let Some(cell) = screen.cell(row, col) else {
            continue;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let mut style = String::from("0");
        for (enabled, code) in [
            (cell.bold(), ";1"),
            (cell.dim(), ";2"),
            (cell.italic(), ";3"),
            (cell.underline(), ";4"),
            (cell.inverse(), ";7"),
        ] {
            if enabled {
                style.push_str(code);
            }
        }
        append_color(&mut style, cell.fgcolor(), 38);
        append_color(&mut style, cell.bgcolor(), 48);
        if style != previous {
            result.push_str("\x1b[");
            result.push_str(&style);
            result.push('m');
            previous = style;
        }
        if cell.has_contents() {
            result.extend(cell.contents().chars().filter(|c| !c.is_control()));
        } else {
            result.push(' ');
        }
    }
    result.push_str("\x1b[0m");
    result
}

fn append_color(style: &mut String, color: vt100::Color, code: u8) {
    match color {
        vt100::Color::Default => {}
        vt100::Color::Idx(index) => style.push_str(&format!(";{code};5;{index}")),
        vt100::Color::Rgb(r, g, b) => style.push_str(&format!(";{code};2;{r};{g};{b}")),
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

    /// 返回 id 最近 count 行含 SGR 颜色和样式的文字，选行规则与 recent_lines 相同，最多 10000 行。
    /// 每行从默认样式开始并以复位结束，只包含 ESC [ … m 控制序列；不存在的终端返回错误。
    pub fn recent_lines_formatted(&self, id: &str, count: usize) -> Result<Vec<String>, AppError> {
        Ok(lock(&self.get(id)?.output).recent_rendered(count, true))
    }

    /// 返回 id 当前是否启用括号粘贴模式；不存在的终端返回错误。
    pub fn bracketed_paste(&self, id: &str) -> Result<bool, AppError> {
        Ok(lock(&self.get(id)?.output)
            .parser
            .screen()
            .bracketed_paste())
    }

    /// 返回 id 现在是否在备用屏（Codex、less、vim 这类全屏程序）；不存在的终端返回错误。
    pub fn alternate_screen(&self, id: &str) -> Result<bool, AppError> {
        Ok(lock(&self.get(id)?.output)
            .parser
            .screen()
            .alternate_screen())
    }

    /// 手机上往上翻（up）或往下翻一次要写给 id 的字节，见 wheel_sequence；
    /// 不在备用屏时返回 None（历史行由手机自己翻）；不存在的终端返回错误。
    pub fn wheel_bytes(&self, id: &str, up: bool) -> Result<Option<Vec<u8>>, AppError> {
        Ok(wheel_sequence(
            lock(&self.get(id)?.output).parser.screen(),
            up,
        ))
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

#[cfg(test)]
mod formatted_tests {
    use super::*;

    fn check_lines(output: &Output, count: usize) -> Vec<String> {
        let lines = output.recent_rendered(count, true);
        let sgr = regex::Regex::new("\x1b\\[[0-9;]*m").unwrap();
        let plain: Vec<_> = lines
            .iter()
            .map(|line| {
                assert!(line.starts_with("\x1b[0m"));
                assert!(line.ends_with("\x1b[0m"));
                let text = sgr.replace_all(line, "").into_owned();
                assert!(!text.chars().any(char::is_control));
                text
            })
            .collect();
        assert_eq!(plain, output.recent(count));
        lines
    }

    #[test]
    fn 彩色行保留全部样式索引色真彩色和宽字符且不输出其他控制序列() {
        let mut output = Output::new(40, 4);
        output.parser.process(
            concat!(
                "\x1b[31;44;1;3;4;7m中😀e\u{301}",
                "\x1b[0;2;38;5;202;48;5;234m暗",
                "\x1b[0;38;2;1;2;3;48;2;4;5;6m真",
                "\x1b[0m\x1b[2C后\x1b]52;c;c2VjcmV0\x07",
                "\x1b]8;;https://example.com\x1b\\链接\x1b]8;;\x1b\\",
                "\r\n默认"
            )
            .as_bytes(),
        );
        let lines = check_lines(&output, 10);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(";1;3;4;7;38;5;1;48;5;4m中😀e\u{301}"));
        assert!(lines[0].contains(";2;38;5;202;48;5;234m暗"));
        assert!(lines[0].contains(";38;2;1;2;3;48;2;4;5;6m真"));
        assert_eq!(lines[1], "\x1b[0m默认\x1b[0m");
        assert!(!lines[0].contains("example.com"));
        // 每行单独解析，验证宽字符不重复且每个单元格的样式可还原。
        let mut parsed = vt100::Parser::new(4, 40, 0);
        parsed.process(lines[0].as_bytes());
        for col in 0..20 {
            let original = output.parser.screen().cell(0, col).unwrap();
            let actual = parsed.screen().cell(0, col).unwrap();
            // 光标跳过的空单元格需要输出空格，重新解析后仍应呈现为空白。
            if original.contents().is_empty() {
                assert!(actual.contents().trim().is_empty());
            } else {
                assert_eq!(actual.contents(), original.contents());
            }
            assert_eq!(actual.fgcolor(), original.fgcolor());
            assert_eq!(actual.bgcolor(), original.bgcolor());
            assert_eq!(actual.bold(), original.bold());
            assert_eq!(actual.dim(), original.dim());
            assert_eq!(actual.italic(), original.italic());
            assert_eq!(actual.underline(), original.underline());
            assert_eq!(actual.inverse(), original.inverse());
        }
    }

    #[test]
    fn 彩色回滚选行空行截断及备用屏与纯文本一致() {
        let mut output = Output::new(6, 3);
        output
            .parser
            .process("\x1b[32m一\r\n\r\n三\r\n四\r\n五\r\n\r\n".as_bytes());
        for count in [0, 1, 2, 3, 4, 20] {
            check_lines(&output, count);
        }
        let original = output.recent_rendered(20, true);
        output
            .parser
            .process(b"\x1b[?1049h\x1b[H\x1b[2J\x1b[35malt");
        check_lines(&output, 20);
        output.parser.process(b"\x1b[?1049l");
        assert_eq!(output.recent_rendered(20, true), original);
        for _ in 0..HISTORY_LINES + 5 {
            output.parser.process(b"x\r\n");
        }
        assert_eq!(check_lines(&output, usize::MAX).len(), HISTORY_LINES);
    }
}

#[cfg(test)]
mod wheel_tests {
    use super::*;

    fn wheel(cols: u16, setup: &str, up: bool) -> Option<Vec<u8>> {
        let mut output = Output::new(cols, 24);
        output.parser.process(setup.as_bytes());
        wheel_sequence(output.parser.screen(), up)
    }

    #[test]
    fn 普通屏不发_全屏程序按它的鼠标格式发滚轮或方向键() {
        assert_eq!(wheel(80, "", true), None);
        // 没接管鼠标：方向键，应用光标模式用 ESC O
        assert_eq!(wheel(80, "\x1b[?1049h", true).unwrap(), b"\x1b[A");
        assert_eq!(wheel(80, "\x1b[?1049h", false).unwrap(), b"\x1b[B");
        assert_eq!(wheel(80, "\x1b[?1049h\x1b[?1h", true).unwrap(), b"\x1bOA");
        assert_eq!(wheel(80, "\x1b[?1049h\x1b[?9h", true).unwrap(), b"\x1b[A");
        // 接管了鼠标：SGR 编码，横向中间、纵向上三分之一
        assert_eq!(
            wheel(80, "\x1b[?1049h\x1b[?1000h\x1b[?1006h", true).unwrap(),
            b"\x1b[<64;40;8M"
        );
        assert_eq!(
            wheel(80, "\x1b[?1049h\x1b[?1003h\x1b[?1006h", false).unwrap(),
            b"\x1b[<65;40;8M"
        );
        // 默认编码：每个值加 32；UTF-8 编码大于 127 时写成两个字节
        assert_eq!(
            wheel(80, "\x1b[?1049h\x1b[?1000h", true).unwrap(),
            [b"\x1b[M".as_slice(), &[96, 72, 40]].concat()
        );
        assert_eq!(
            wheel(300, "\x1b[?1049h\x1b[?1000h", true).unwrap(),
            [b"\x1b[M".as_slice(), &[96, 182, 40]].concat()
        );
        assert_eq!(
            wheel(300, "\x1b[?1049h\x1b[?1000h\x1b[?1005h", true).unwrap(),
            [b"\x1b[M".as_slice(), &[96, 0xc2, 0xb6, 40]].concat()
        );
        // 退出全屏后不再发
        assert_eq!(wheel(80, "\x1b[?1049h\x1b[?1000h\x1b[?1049l", true), None);
    }
}

#[cfg(test)]
mod parser_failure_tests {
    use super::*;

    #[test]
    fn 屏幕副本出错时换新的接着用_历史和位置照常() {
        let mut output = Output::new(20, 4);
        output.feed(&TerminalEvent::Output(b"first\r\n".to_vec()));
        output.fail_next = true;
        output.feed(&TerminalEvent::Output(b"lost\r\n".to_vec()));
        output.feed(&TerminalEvent::Output(b"after".to_vec()));
        // 原始历史和位置一个字节都不少，读输出、等输出不受影响
        assert_eq!(output.position, 18);
        assert_eq!(
            output.history.iter().copied().collect::<Vec<_>>(),
            b"first\r\nlost\r\nafter"
        );
        // 新的副本从出错之后的输出开始
        assert_eq!(output.recent(4), ["after"]);
        assert_eq!(output.parser.screen().size(), (4, 20));
    }
}

#[cfg(test)]
mod parser_mode_tests {
    use super::*;

    #[test]
    fn 改窄切开汉字后清行尾_副本重建而不是让分发线程退出() {
        // vt100 0.16.2：改窄时新的最后一列是汉字的前半，之后清行尾会越界（1.2.1 用户的终端就这样卡死）
        let mut output = Output::new(80, 4);
        output.feed(&TerminalEvent::Output("中文".as_bytes().to_vec()));
        output.resize(3, 4);
        output.feed(&TerminalEvent::Output(b"\x1b[K".to_vec()));
        assert_eq!(output.position, "中文".len() as u64 + 3);
        // 出错后换了新的副本（旧画面没了），尺寸是改过的
        assert!(output.recent(4).is_empty());
        assert_eq!(output.parser.screen().size(), (4, 3));
        output.feed(&TerminalEvent::Output(b"ok".to_vec()));
        assert_eq!(output.recent(4), ["ok"]);
    }

    #[test]
    fn 重建后保留全屏_括号粘贴和鼠标模式() {
        let mut output = Output::new(80, 24);
        output.feed(&TerminalEvent::Output(
            b"\x1b[?1049h\x1b[?2004h\x1b[?1h\x1b[?1000h\x1b[?1006h".to_vec(),
        ));
        output.fail_next = true;
        output.feed(&TerminalEvent::Output(b"x".to_vec()));
        let screen = output.parser.screen();
        assert!(screen.alternate_screen());
        assert!(screen.bracketed_paste());
        assert!(screen.application_cursor());
        assert_eq!(wheel_sequence(screen, true).unwrap(), b"\x1b[<64;40;8M");
    }
}
