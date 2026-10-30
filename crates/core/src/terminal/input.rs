//! 用户输入优先；自动化等待用户空闲两秒，接管后丢弃未发送自动化输入。
use super::*;
use std::{borrow::Cow, collections::VecDeque};

#[cfg(test)]
mod write_tests;

const USER_IDLE: Duration = Duration::from_secs(2);
const ESC_KEY: &[u8] = b"\x1b[27;1;27;1;0;1_\x1b[27;1;27;0;0;1_";

/// 只记解析位置，不保存输入内容；识别被系统短写或分块拆开的按键序列。
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Win32Input {
    #[default]
    Plain,
    Escape,
    Parameters,
    Enabled,
}

impl Win32Input {
    fn after(mut self, data: &[u8]) -> Self {
        for &byte in data {
            self = match (self, byte) {
                (Self::Enabled, _) => return Self::Enabled,
                (_, 0x1b) => Self::Escape,
                (Self::Escape, b'[') => Self::Parameters,
                (Self::Parameters, b'0'..=b'9' | b';') => Self::Parameters,
                (Self::Parameters, b'_') => Self::Enabled,
                _ => Self::Plain,
            };
        }
        self
    }
}

/// 已使用 Windows 按键格式时，只替换整条输入末尾的一个 Esc。
fn escape_trailing_esc(data: &[u8], win32: bool) -> Cow<'_, [u8]> {
    if win32 && data.last() == Some(&0x1b) {
        let mut encoded = Vec::with_capacity(data.len() - 1 + ESC_KEY.len());
        encoded.extend_from_slice(&data[..data.len() - 1]);
        encoded.extend_from_slice(ESC_KEY);
        Cow::Owned(encoded)
    } else {
        Cow::Borrowed(data)
    }
}

/// 输入的来源；现有 write 入口始终按用户输入处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSource {
    User,
    Automation,
}

#[derive(Default)]
pub(super) struct Input {
    conpty: bool,
    user: VecDeque<Vec<u8>>,
    automation: VecDeque<(Vec<u8>, Option<u64>)>,
    bytes: usize,
    failed: bool,
    user_at: Option<Instant>,
    taken_over: bool,
    generation: u64,
}
impl Input {
    /// 按 conpty 标志创建空队列；只有 Windows 伪终端需要转换末尾 Esc。
    pub(super) fn new(conpty: bool) -> Self {
        Self {
            conpty,
            ..Default::default()
        }
    }

    fn push_user(&mut self, data: &[u8]) {
        // 终端自动应答照常转给程序，不作废 AI 的待提交行或暂停自动化。
        if !is_terminal_report(data) {
            self.generation = self.generation.wrapping_add(1);
            self.user_at = Some(Instant::now());
        }
        self.user.push_back(data.to_vec());
    }

    fn ready(&self, now: Instant) -> bool {
        !self.user.is_empty()
            || !self.taken_over && !self.paused(now) && !self.automation.is_empty()
    }
    fn paused(&self, now: Instant) -> bool {
        self.user_at
            .is_some_and(|at| now.saturating_duration_since(at) < USER_IDLE)
    }
    fn next(&mut self, now: Instant) -> Option<(InputSource, Vec<u8>, u64)> {
        if let Some(data) = self.user.pop_front() {
            return Some((InputSource::User, data, self.generation));
        }
        if !self.taken_over && !self.paused(now) {
            while let Some((data, expected)) = self.automation.pop_front() {
                if expected.is_some_and(|generation| generation != self.generation) {
                    self.bytes -= data.len();
                    continue;
                }
                return Some((InputSource::Automation, data, self.generation));
            }
        }
        None
    }
    fn clear(&mut self) {
        self.user.clear();
        self.automation.clear();
        self.bytes = 0;
    }
}

impl TerminalManager {
    /// 将 data 按 source 放入 id 的有界输入队列；成功表示已入队，不表示远端已执行。
    /// 用户输入立即排队；两秒内有用户按键时自动化延后，接管后返回 automation_rejected。
    pub fn write_from(&self, id: &str, data: &[u8], source: InputSource) -> Result<(), AppError> {
        self.enqueue(id, data, source, None)
    }

    /// 将 data 作为自动化输入放入 id 的队列；generation 必须与用户当前输入代数一致。
    /// 用户在审批期间或排队期间按键时拒绝或丢弃这次输入，避免执行未经审批的新内容。
    pub(crate) fn write_automation_at_generation(
        &self,
        id: &str,
        data: &[u8],
        generation: u64,
    ) -> Result<(), AppError> {
        self.enqueue(id, data, InputSource::Automation, Some(generation))
    }

    fn enqueue(
        &self,
        id: &str,
        data: &[u8],
        source: InputSource,
        expected: Option<u64>,
    ) -> Result<(), AppError> {
        let session = self.get(id)?;
        let flow = lock(&session.flow);
        if flow.stopped {
            return Err(error(
                "not_found",
                "terminal.session_has_ended",
                "会话已结束",
            ));
        }
        let mut input = lock(&session.input);
        if input.failed {
            return Err(error(
                "write_failed",
                "terminal.couldn_t_write_to_the_terminal",
                "写入终端失败",
            ));
        }
        if source == InputSource::Automation && input.taken_over {
            return Err(error(
                "automation_rejected",
                "terminal.the_user_has_taken_control_of_the_terminal",
                "用户已接管终端",
            ));
        }
        if expected.is_some_and(|generation| generation != input.generation) {
            return Err(error(
                "input_changed",
                "terminal.user_input_changed_read_the_terminal_again_and_retry",
                "用户输入已改变，请重新读取终端并重试",
            ));
        }
        if data.len() > WINDOW - input.bytes {
            return Err(error(
                "input_overflow",
                "terminal.input_queue_exceeds_1_mib",
                "输入队列超过 1 MiB",
            ));
        }
        if !data.is_empty() {
            input.bytes += data.len();
            match source {
                InputSource::User => {
                    input.push_user(data);
                }
                InputSource::Automation => input.automation.push_back((data.to_vec(), expected)),
            }
        }
        session.wake.notify_all();
        Ok(())
    }

    /// 返回 id 的用户输入代数；每次非空用户输入或接管加一，终端不存在返回错误。
    pub fn control_generation(&self, id: &str) -> Result<u64, AppError> {
        Ok(lock(&self.get(id)?.input).generation)
    }

    /// 设置 id 是否由用户接管；true 拒绝新自动化输入并丢弃尚未发送的自动化队列。
    /// false 重新允许自动化，仍等待用户空闲两秒；已经交给系统的字节无法撤回。
    pub fn set_user_control(&self, id: &str, taken_over: bool) -> Result<(), AppError> {
        let session = self.get(id)?;
        let mut input = lock(&session.input);
        input.taken_over = taken_over;
        if taken_over {
            input.generation = input.generation.wrapping_add(1);
            let discarded: usize = input.automation.drain(..).map(|(data, _)| data.len()).sum();
            input.bytes -= discarded;
        }
        session.wake.notify_all();
        Ok(())
    }
}

/// 将 session 的排队输入写入 writer；会话结束或写入失败时清空队列并返回。
pub(super) fn write_input(mut writer: Box<dyn Write + Send>, session: Arc<Session>) {
    write_queued_input(&mut writer, &session.flow, &session.input, &session.wake);
}

fn write_queued_input(
    writer: &mut dyn Write,
    flow: &Mutex<Flow>,
    input: &Mutex<Input>,
    wake: &Condvar,
) {
    let conpty = lock(input).conpty;
    let mut win32 = Win32Input::default();
    loop {
        if lock(flow).stopped {
            lock(input).clear();
            break;
        }
        let data = lock(input).next(Instant::now());
        let Some((source, data, generation)) = data else {
            let flow = lock(flow);
            if !flow.stopped && !lock(input).ready(Instant::now()) {
                let _ = wake.wait_timeout(flow, Duration::from_millis(200));
            }
            continue;
        };
        // 先按整条输入判断末尾；仅成功写出的字节才推进会话的协议状态。
        let encoded =
            escape_trailing_esc(&data, conpty && win32.after(&data) == Win32Input::Enabled);
        let escaped = encoded.len() != data.len();
        let mut escape_written = 0;
        let mut remaining = data.as_slice();
        let mut requeued = 0;
        while !remaining.is_empty() && !lock(flow).stopped {
            // 一个 Esc 的按下和松开连续发完，避免暂停重排队拆开这一个按键。
            if source == InputSource::Automation && escape_written == 0 {
                let mut input = lock(input);
                if input.taken_over || generation != input.generation {
                    break;
                }
                if input.paused(Instant::now()) {
                    input
                        .automation
                        .push_front((remaining.to_vec(), Some(generation)));
                    requeued = remaining.len();
                    break;
                }
            }
            // 分块写入使用户活动和接管能够在下一块生效。
            let escaping = escaped && remaining.len() == 1;
            let count = remaining.len() - usize::from(escaped);
            let count = if source == InputSource::Automation {
                count.min(4096)
            } else {
                count
            };
            let chunk = if escaping {
                &encoded[data.len() - 1 + escape_written..]
            } else {
                &remaining[..count]
            };
            match writer.write(chunk) {
                Ok(0) => {
                    lock(input).failed = true;
                    break;
                }
                Ok(n) => {
                    if conpty {
                        win32 = win32.after(&chunk[..n]);
                    }
                    if escaping {
                        escape_written += n;
                        if escape_written == ESC_KEY.len() {
                            remaining = &[];
                        }
                    } else {
                        remaining = &remaining[n..];
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let flow = lock(flow);
                    if !flow.stopped {
                        let _ = wake.wait_timeout(flow, Duration::from_millis(100));
                    }
                }
                Err(_) => {
                    lock(input).failed = true;
                    break;
                }
            }
        }
        let mut input = lock(input);
        // 容量始终按原始输入计算；重排队仍放回原始末尾 Esc。
        input.bytes -= data.len() - requeued;
        if input.failed {
            input.clear();
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 用户优先且自动化在两秒边界恢复() {
        let now = Instant::now();
        let mut input = Input {
            user_at: Some(now),
            ..Default::default()
        };
        input.automation.push_back((b"auto".to_vec(), None));
        input.user.push_back(b"user".to_vec());
        assert_eq!(input.next(now).unwrap().0, InputSource::User);
        assert!(input.next(now + Duration::from_millis(1999)).is_none());
        assert_eq!(input.next(now + USER_IDLE).unwrap().1, b"auto");
        input.automation.push_back((b"next".to_vec(), None));
        input.taken_over = true;
        assert!(input.next(now + USER_IDLE).is_none());
    }

    #[test]
    fn 用户修改后丢弃绑定旧代数的自动化输入并释放队列容量() {
        let now = Instant::now();
        let mut input = Input {
            generation: 2,
            user_at: Some(now),
            bytes: 6,
            ..Default::default()
        };
        input.automation.push_back((b"old".to_vec(), Some(1)));
        input.automation.push_back((b"new".to_vec(), Some(2)));
        assert!(input.next(now).is_none());
        assert_eq!(input.next(now + USER_IDLE).unwrap().1, b"new");
        assert_eq!(input.bytes, 3);
        assert!(input.next(now + USER_IDLE).is_none());
    }
}

/// data 必须全部由焦点、设备属性、状态、光标位置、键盘协议、版本或颜色应答组成。
fn is_terminal_report(mut data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    while !data.is_empty() {
        let Some(length) = terminal_report_len(data) else {
            return false;
        };
        data = &data[length..];
    }
    true
}

fn terminal_report_len(data: &[u8]) -> Option<usize> {
    if let Some(body) = data.strip_prefix(b"\x1b[") {
        let end = body.iter().position(|b| (0x40..=0x7e).contains(b))?;
        let params = &body[..end];
        let numbers = |bytes: &[u8]| {
            bytes
                .split(|b| *b == b';')
                .all(|part| !part.is_empty() && part.iter().all(u8::is_ascii_digit))
        };
        let pair =
            |bytes: &[u8]| bytes.iter().filter(|b| **b == b';').count() == 1 && numbers(bytes);
        let valid = match body[end] {
            b'I' | b'O' => params.is_empty(),
            b'c' => {
                params.first().is_some_and(|b| matches!(b, b'?' | b'>')) && numbers(&params[1..])
            }
            b'u' => params.starts_with(b"?") && numbers(&params[1..]),
            b'R' => pair(params.strip_prefix(b"?").unwrap_or(params)),
            b'n' => params == b"0",
            b'y' => params
                .strip_prefix(b"?")
                .and_then(|p| p.strip_suffix(b"$"))
                .is_some_and(pair),
            _ => false,
        };
        return valid.then_some(end + 3);
    }
    if let Some(version) = data.strip_prefix(b"\x1bP>|") {
        let end = version.iter().position(|b| *b == 0x1b)?;
        return (!version[..end].is_empty()
            && version[..end].iter().all(|b| (0x20..=0x7e).contains(b))
            && version[end..].starts_with(b"\x1b\\"))
        .then_some(4 + end + 2);
    }
    let body = data.strip_prefix(b"\x1b]")?;
    let end = body.iter().position(|b| matches!(b, 7 | 0x1b))?;
    let terminator = if body[end] == 7 {
        1
    } else if body[end..].starts_with(b"\x1b\\") {
        2
    } else {
        return None;
    };
    let mut fields = body[..end].split(|b| *b == b';');
    let valid = match fields.next()? {
        b"10" | b"11" | b"12" => fields.next().is_some_and(is_rgb) && fields.next().is_none(),
        b"4" => {
            let mut count = 0;
            while let Some(index) = fields.next() {
                if index.is_empty()
                    || !index.iter().all(u8::is_ascii_digit)
                    || !fields.next().is_some_and(is_rgb)
                {
                    return None;
                }
                count += 1;
            }
            count > 0
        }
        _ => false,
    };
    valid.then_some(2 + end + terminator)
}

fn is_rgb(data: &[u8]) -> bool {
    data.strip_prefix(b"rgb:").is_some_and(|color| {
        color.split(|b| *b == b'/').count() == 3
            && color
                .split(|b| *b == b'/')
                .all(|part| (1..=4).contains(&part.len()) && part.iter().all(u8::is_ascii_hexdigit))
    })
}

#[cfg(test)]
mod focus_tests {
    use super::is_terminal_report;

    #[test]
    fn 焦点报告不算用户输入() {
        assert!(is_terminal_report(b"\x1b[I"));
        assert!(is_terminal_report(b"\x1b[O\x1b[I"));
        assert!(!is_terminal_report(b""));
        assert!(!is_terminal_report(b"a"));
        assert!(!is_terminal_report(b"\x1b[Ia"));
        assert!(!is_terminal_report(b"\x1b[A"));
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;

    fn reports() -> Vec<&'static [u8]> {
        vec![
            b"\x1b[?1;2c",
            b"\x1b[>0;276;0c",
            b"\x1b[12;40R",
            b"\x1b[?12;40R",
            b"\x1b[?2026;2$y",
            b"\x1b[0n",
            b"\x1bP>|XTerm(390)\x1b\\",
            b"\x1b[?1u",
            b"\x1b]10;rgb:ffff/eeee/dddd\x07",
            b"\x1b]11;rgb:00/01/02\x1b\\",
            b"\x1b]12;rgb:ffff/eeee/dddd\x07",
            b"\x1b]12;rgb:00/01/02\x1b\\",
            b"\x1b]4;255;rgb:aa/bb/cc\x07",
            b"\x1b]4;0;rgb:0000/1111/2222;15;rgb:f/e/d\x1b\\",
            b"\x1b[I",
            b"\x1b[O",
        ]
    }

    #[test]
    fn 自动应答可单条或拼接且混入按键不豁免() {
        for report in reports() {
            assert!(is_terminal_report(report), "{report:?}");
            assert!(!is_terminal_report(&[report, b"x"].concat()));
            assert!(!is_terminal_report(&[b"\r", report].concat()));
            assert!(!is_terminal_report(&report[..report.len() - 1]));
        }
        assert!(is_terminal_report(&reports().concat()));
        for invalid in [
            b"\x1b[?c".as_slice(),
            b"\x1b[1R",
            b"\x1b[1;2;3R",
            b"\x1b[?1R",
            b"\x1b[?1;2;3R",
            b"\x1b[?;2R",
            b"\x1b[?1;2y",
            b"\x1b[1;2$y",
            b"\x1b[?1;$y",
            b"\x1b[?1;2;3$y",
            b"\x1b[5n",
            b"\x1bP>|\x1b\\",
            b"\x1bP>|version\rcommand\x1b\\",
            b"\x1bP>|version\x07",
            b"\x1bP>version\x1b\\",
            b"\x1b[>1u",
            b"\x1b]10;rgb:gg/00/00\x07",
            b"\x1b]12;rgb:gg/00/00\x07",
            b"\x1b]12;rgb:00/00/00;extra\x07",
            b"\x1b]4;rgb:00/00/00\x07",
            b"\x1b]4;;rgb:00/00/00\x07",
            b"\x1b]4;x;rgb:00/00/00\x07",
            b"\x1b]4;0;rgb:00/00/00;1\x07",
            b"\x1b]4;0;rgb:00/00/00\x1bx",
        ] {
            assert!(!is_terminal_report(invalid));
        }
    }

    fn preserves_automation(conpty: bool) {
        let mut input = Input::new(conpty);
        input.automation.push_back((b"echo ok\r".to_vec(), Some(0)));
        let data = reports().concat();
        input.bytes = data.len() + 8;
        input.push_user(&data);
        assert_eq!(input.generation, 0);
        assert!(input.user_at.is_none());
        assert_eq!(input.next(Instant::now()).unwrap().1, data);
        assert_eq!(input.next(Instant::now()).unwrap().1, b"echo ok\r");
        input.push_user(b"x");
        assert_eq!(input.generation, 1);
        assert!(input.paused(Instant::now()));
    }

    #[test]
    fn 自动应答原样发送且不丢弃排队命令或暂停() {
        preserves_automation(false);
    }

    #[cfg(windows)]
    #[test]
    fn 控制台自动应答不改变用户代数() {
        preserves_automation(true);
    }
}
