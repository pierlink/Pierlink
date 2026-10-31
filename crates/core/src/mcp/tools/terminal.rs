//! 终端自动化输入及有界增量输出；权限和 AI 登记由分发层检查。
use crate::{
    error::AppError,
    mcp::{
        safety::looks_like_password_prompt,
        text::{ALIGN_LOOKBEHIND, aligned_start_settled, strip_ansi, token_len_settled},
    },
    terminal::{InputSource, TerminalEvent, TerminalManager},
};
use regex::Regex;
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const READ_LIMIT: usize = 64 * 1024;
const WAIT_LIMIT: usize = 256 * 1024;
#[derive(Debug, Serialize)]
pub struct SendOutcome {
    pub queued: bool,
    /// 入队前的原始输出末尾字节游标
    pub cursor: u64,
    pub reason: &'static str,
}
#[derive(Debug, Default, Serialize)]
pub struct TerminalRead {
    pub more: bool,
    pub text: String,
    pub cursor: u64,
    pub lost: bool,
}
#[derive(Debug, Default, Serialize)]
pub struct TerminalWait {
    pub matched: bool,
    pub text: String,
    pub cursor: u64,
    pub lost: bool,
    pub exited: bool,
    pub timed_out: bool,
}
/// 按键名（Enter、Ctrl+C、Up……）对应的字节；不认识的按键返回 mcp.invalid_argument
pub fn key_bytes(key: &str) -> Result<Vec<u8>, AppError> {
    let bytes: &[u8] = match key {
        "Enter" => b"\r",
        "Tab" => b"\t",
        "Esc" => b"\x1b",
        "Backspace" => b"\x7f",
        "Delete" => b"\x1b[3~",
        "Up" => b"\x1b[A",
        "Down" => b"\x1b[B",
        "Right" => b"\x1b[C",
        "Left" => b"\x1b[D",
        "Home" => b"\x1b[H",
        "End" => b"\x1b[F",
        "PageUp" => b"\x1b[5~",
        "PageDown" => b"\x1b[6~",
        _ => {
            if let Some(letter) = key
                .strip_prefix("Ctrl+")
                .filter(|v| v.len() == 1 && v.as_bytes()[0].is_ascii_uppercase())
            {
                return Ok(vec![letter.as_bytes()[0] - b'A' + 1]);
            }
            return Err(AppError::new("mcp.invalid_argument")
                .with_reason("mcp.unknown_terminal_key", "未知终端按键"));
        }
    };
    Ok(bytes.into())
}
/// 删掉 line 末尾的一个词（连同它后面的空白），和 shell 里 Ctrl+W 一样
fn delete_word(line: &mut String) {
    let kept = line
        .trim_end()
        .trim_end_matches(|c: char| !c.is_whitespace())
        .len();
    line.truncate(kept);
}

/// 按 text、keys 的顺序模拟自动化输入，返回这次按了回车提交的整行（含之前没提交的部分）和之后剩下没提交的部分。
/// Enter、Ctrl+M、Ctrl+J 算回车；Ctrl+C、Ctrl+U 清空这一行；Ctrl+W 删一个词；退格删一个字。
/// 上下方向键、PageUp/PageDown、Ctrl+P/Ctrl+N 会换成历史命令，内容无法知道，清空（判断是尽力而为）；
/// 左右方向键、Home、End、Esc 等只移动光标或不改内容的键保留这一行：光标挪动后再回车，提交的还是这一行，照样要判断。
///
/// :param pending: 之前自动化输入、还没按回车的部分
/// :param text: 这次发送的文字
/// :param keys: 这次发送的按键（在文字之后）
/// :return: (这次提交的各行, 剩下没提交的部分)
pub fn submitted_lines(
    pending: &str,
    text: Option<&str>,
    keys: &[String],
) -> (Vec<String>, String) {
    let mut line = pending.to_owned();
    let mut lines = Vec::new();
    let mut submit = |line: &mut String| {
        if !line.trim().is_empty() {
            lines.push(std::mem::take(line));
        }
        line.clear();
    };
    let mut chars = text.unwrap_or_default().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' | '\n' => submit(&mut line),
            '\x03' | '\x15' => line.clear(),
            '\x17' => delete_word(&mut line),
            '\x7f' | '\x08' => {
                line.pop();
            }
            // Ctrl+P、Ctrl+N：上一条、下一条历史
            '\x10' | '\x0e' => line.clear(),
            '\x1b' => match chars.peek() {
                // 控制序列：上下方向键（ESC [ A/B、ESC O A/B）和 PageUp/PageDown（ESC [ 5~/6~）翻历史，其余保留这一行
                Some('[') | Some('O') => {
                    chars.next();
                    let mut sequence = String::new();
                    while let Some(c) = chars.next_if(|c| !('@'..='~').contains(c)) {
                        sequence.push(c);
                    }
                    if let Some(end) = chars.next() {
                        sequence.push(end);
                    }
                    if matches!(sequence.as_str(), "A" | "B" | "5~" | "6~") {
                        line.clear();
                    }
                }
                _ => {}
            },
            c if c.is_control() => {}
            c => line.push(c),
        }
    }
    for key in keys {
        match key.as_str() {
            "Enter" | "Ctrl+M" | "Ctrl+J" => submit(&mut line),
            "Ctrl+C" | "Ctrl+U" => line.clear(),
            "Ctrl+W" => delete_word(&mut line),
            "Backspace" | "Ctrl+H" => {
                line.pop();
            }
            "Up" | "Down" | "PageUp" | "PageDown" | "Ctrl+P" | "Ctrl+N" => line.clear(),
            _ => {}
        }
    }
    (lines, line)
}

/// 这次输入里是不是翻了历史（上下键、PageUp/PageDown、Ctrl+P/N）又按了回车：翻出来的命令内容不可知。
/// 只看这一次发送；翻历史和回车分两次发的认不出（尽力而为）。
///
/// :param text: 这次发送的文字
/// :param keys: 这次发送的按键
/// :return: 是否既翻了历史又提交了
pub fn submits_recalled_history(text: Option<&str>, keys: &[String]) -> bool {
    let text = text.unwrap_or_default();
    let recalled = keys.iter().any(|key| {
        matches!(
            key.as_str(),
            "Up" | "Down" | "PageUp" | "PageDown" | "Ctrl+P" | "Ctrl+N"
        )
    }) || [
        "\x1b[A", "\x1b[B", "\x1bOA", "\x1bOB", "\x1b[5~", "\x1b[6~", "\x10", "\x0e",
    ]
    .iter()
    .any(|sequence| text.contains(sequence));
    let submits = keys
        .iter()
        .any(|key| matches!(key.as_str(), "Enter" | "Ctrl+M" | "Ctrl+J"))
        || text.contains(['\r', '\n']);
    recalled && submits
}

/// 将 text 中的 CRLF 和 LF 统一成终端提交用的 CR，返回转换后的文字。
pub(super) fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\r").replace('\n', "\r")
}

/// 将 text 和 keys 按自动化来源放入 id 的输入队列；返回值只说明已入队，不表示已执行。
/// 密码提示拒绝非空文字；用户接管返回 terminal.automation_rejected，未知按键返回 mcp.invalid_argument。
pub fn terminal_send(
    m: &TerminalManager,
    id: &str,
    text: Option<&str>,
    keys: &[String],
) -> Result<SendOutcome, AppError> {
    send_at_generation(m, id, text, keys, None)
}
pub(super) fn send_at_generation(
    m: &TerminalManager,
    id: &str,
    text: Option<&str>,
    keys: &[String],
    generation: Option<u64>,
) -> Result<SendOutcome, AppError> {
    let mut bytes = normalize_text(text.unwrap_or_default()).into_bytes();
    for key in keys {
        bytes.extend(key_bytes(key)?);
    }
    if text.is_some_and(|v| !v.is_empty()) && looks_like_password_prompt(&m.screen_text(id)?) {
        return Err(AppError::new("mcp.password_prompt").with_reason(
            "mcp.terminal_is_waiting_for_a_password_enter_it_in_pierlink",
            "终端正在等待密码，请用户在 Pierlink 里输入",
        ));
    }
    let cursor = m.output_position(id)?;
    if let Some(generation) = generation {
        m.write_automation_at_generation(id, &bytes, generation)?;
    } else {
        m.write_from(id, &bytes, InputSource::Automation)?;
    }
    Ok(SendOutcome {
        queued: true,
        cursor,
        reason: "已入队，不表示已执行；请读取或等待终端输出确认",
    })
}

// 每个文字字节记录对应原始输出的结束位置，保证正则命中后游标不会越过未返回文字。
fn clean_prefix(bytes: &[u8], start: u64, settled: bool) -> (String, VecDeque<u64>, usize) {
    let mut text = String::new();
    let mut positions = VecDeque::new();
    let mut i = 0;
    while i < bytes.len() {
        let Some(size) = token_len_settled(&bytes[i..], settled) else {
            break;
        };
        let end = i + size;
        let token = strip_ansi(&bytes[i..end]);
        if text.len() + token.len() > READ_LIMIT {
            break;
        }
        positions.extend(std::iter::repeat_n(start + end as u64, token.len()));
        text.push_str(&token);
        i = end;
    }
    (text, positions, i)
}
fn read_chunk(
    m: &TerminalManager,
    id: &str,
    since: Option<u64>,
    tail_bytes: Option<u64>,
) -> Result<(TerminalRead, VecDeque<u64>), AppError> {
    read_chunk_at(m, id, since, tail_bytes, Instant::now())
}
fn read_chunk_at(
    m: &TerminalManager,
    id: &str,
    since: Option<u64>,
    tail_bytes: Option<u64>,
    now: Instant,
) -> Result<(TerminalRead, VecDeque<u64>), AppError> {
    // 在历史锁内确定尾部和窗口，只复制本次需要的字节及少量边界上下文。
    read_window(since, |limit| {
        m.output_window(
            id,
            since.unwrap_or(0),
            tail_bytes.map(|tail| tail.min(READ_LIMIT as u64) as usize),
            ALIGN_LOOKBEHIND,
            limit,
            now,
        )
    })
}

fn read_window(
    since: Option<u64>,
    mut window: impl FnMut(usize) -> Result<(crate::terminal::OutputRead, u64), AppError>,
) -> Result<(TerminalRead, VecDeque<u64>), AppError> {
    let mut limit = READ_LIMIT + ALIGN_LOOKBEHIND;
    loop {
        let (output, requested) = window(limit)?;
        let result = read_output(&output, requested, since != Some(requested));
        let consumed = (result.0.cursor - output.start) as usize;
        // 窗口切到控制序列时只扩大重读一次；字符串序列有 4 KiB 上限，
        // 即使历史全是控制序列，累计复制也不超过 3 × READ_LIMIT + 5 × ALIGN_LOOKBEHIND。
        if limit == READ_LIMIT + ALIGN_LOOKBEHIND
            && output.start + (output.data.len() as u64) < output.next
            && consumed < output.data.len()
            && token_len_settled(&output.data[consumed..], settled(&output)).is_none()
        {
            limit = limit.saturating_mul(2);
            continue;
        }
        return Ok(result);
    }
}
fn settled(output: &crate::terminal::OutputRead) -> bool {
    output.quiet_for >= Duration::from_secs(1)
        && output.start + output.data.len() as u64 == output.next
}
fn read_output(
    output: &crate::terminal::OutputRead,
    since: u64,
    align: bool,
) -> (TerminalRead, VecDeque<u64>) {
    let requested = (since.saturating_sub(output.start) as usize).min(output.data.len());
    let skip = if align || output.lost || since < output.start {
        aligned_start_settled(&output.data, requested, settled(output))
    } else {
        requested
    };
    let start = output.start + skip as u64;
    let (text, positions, consumed) = clean_prefix(&output.data[skip..], start, settled(output));
    (
        TerminalRead {
            text,
            cursor: start + consumed as u64,
            lost: output.lost || since < output.start,
            more: start + (consumed as u64) < output.next,
        },
        positions,
    )
}
/// 从 id 的 since（默认历史开头）读取至多 64 KiB 去 ANSI 文字；tail_bytes 限定最近字节范围，最多 64 KiB。
/// 返回原始字节游标、历史丢失标记和后面是否仍有输出；保留范围内的显式游标原样使用，计算出的起点向后对齐。
pub fn terminal_read(
    m: &TerminalManager,
    id: &str,
    since: Option<u64>,
    tail_bytes: Option<u64>,
) -> Result<TerminalRead, AppError> {
    read_chunk(m, id, since, tail_bytes).map(|v| v.0)
}
/// 返回 id 当前渲染后的屏幕文字；无效终端返回 terminal.not_found。
pub fn terminal_screen(m: &TerminalManager, id: &str) -> Result<String, AppError> {
    m.screen_text(id)
}
/// 从 since（默认当前位置）等待 id 的 pattern 命中或 quiet 安静期；timeout 至多 1800 秒，取消返回 mcp.cancelled。
/// 返回至命中为止的文字和游标；最多保留最近 256 KiB 并标记 lost，已退出和超时均作为正常结果返回。
#[allow(clippy::too_many_arguments)]
pub fn terminal_wait(
    m: &TerminalManager,
    id: &str,
    pattern: Option<&Regex>,
    quiet: Option<Duration>,
    since: Option<u64>,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<TerminalWait, AppError> {
    wait_cancellable(m, id, pattern, quiet, since, timeout, || {
        cancelled.load(Ordering::Acquire)
    })
}
pub(super) fn wait_cancellable(
    m: &TerminalManager,
    id: &str,
    pattern: Option<&Regex>,
    quiet: Option<Duration>,
    since: Option<u64>,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<TerminalWait, AppError> {
    wait_with_clock(
        m,
        id,
        pattern,
        quiet,
        since,
        timeout,
        cancelled,
        Instant::now,
    )
}
#[allow(clippy::too_many_arguments)]
fn wait_with_clock(
    m: &TerminalManager,
    id: &str,
    pattern: Option<&Regex>,
    quiet: Option<Duration>,
    since: Option<u64>,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
    now: impl Fn() -> Instant,
) -> Result<TerminalWait, AppError> {
    let start = Instant::now();
    let timeout = timeout.min(Duration::from_secs(1800));
    let quiet = quiet.or_else(|| pattern.is_none().then_some(Duration::from_secs(2)));
    let subscription = match m.subscribe(id, READ_LIMIT) {
        Ok(s) => Some(s),
        Err(e) if e.code == "terminal.not_found" => None,
        Err(e) => return Err(e),
    };
    let mut result = TerminalWait {
        cursor: since.unwrap_or_else(|| m.output_position(id).unwrap_or(0)),
        exited: subscription.is_none(),
        ..Default::default()
    };
    let mut positions = VecDeque::new();
    let mut last_output = Instant::now();
    loop {
        if cancelled() {
            return Err(
                AppError::new("mcp.cancelled").with_reason("mcp.operation_cancelled", "操作已取消")
            );
        }
        let previous = result.cursor;
        match read_chunk_at(m, id, Some(previous), None, now()) {
            Ok((read, offsets)) => {
                if read.cursor != previous {
                    last_output = Instant::now();
                }
                result.cursor = read.cursor;
                if read.lost {
                    result.text.clear();
                    positions.clear();
                }
                result.lost |= read.lost;
                result.text.push_str(&read.text);
                positions.extend(offsets);
                if result.text.len() > WAIT_LIMIT {
                    let mut discard = result.text.len() - WAIT_LIMIT;
                    while !result.text.is_char_boundary(discard) {
                        discard += 1;
                    }
                    result.text.drain(..discard);
                    positions.drain(..discard);
                    result.lost = true;
                }
                if let Some(found) = pattern.and_then(|p| p.find(&result.text)) {
                    result.matched = true;
                    result.cursor = found
                        .end()
                        .checked_sub(1)
                        .and_then(|n| positions.get(n).copied())
                        .unwrap_or(previous);
                    result.text.truncate(found.end());
                    return Ok(result);
                }
            }
            Err(e) if e.code == "terminal.not_found" => result.exited = true,
            Err(e) => return Err(e),
        }
        if let Some(s) = &subscription {
            while let Some(event) = s.recv_timeout(Duration::ZERO) {
                if matches!(event.event, TerminalEvent::Exited { .. }) {
                    result.exited = true;
                }
            }
        }
        // 先排空退出前的历史，不能因收到退出事件而漏掉末尾的匹配。
        let pending = m
            .output_position(id)
            .map(|position| position > result.cursor)
            .unwrap_or(false);
        if result.exited && (!pending || result.cursor == previous) {
            return Ok(result);
        }
        if quiet.is_some_and(|q| last_output.elapsed() >= q) {
            return Ok(result);
        }
        if start.elapsed() >= timeout {
            result.timed_out = true;
            return Ok(result);
        }
        if result.cursor == previous || !pending {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod line_tests {
    use super::submitted_lines;

    fn keys(k: &[&str]) -> Vec<String> {
        k.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn 换行统一为回车且提交内容一致() {
        let normalized = super::normalize_text("echo a\r\necho b\necho c\r");
        assert_eq!(normalized, "echo a\recho b\recho c\r");
        assert_eq!(
            submitted_lines("", Some(&normalized), &[]).0,
            ["echo a", "echo b", "echo c"]
        );
        assert_eq!(super::key_bytes("Delete").unwrap(), b"\x1b[3~");
    }

    #[test]
    fn 自动化输入按整行判断提交_文字和回车可以分开发() {
        // 只发文字不回车：没有提交，整行留着
        let (lines, rest) = submitted_lines("", Some("rm -rf /tmp/x"), &[]);
        assert!(lines.is_empty());
        assert_eq!(rest, "rm -rf /tmp/x");
        // 之后单独发回车（Enter、Ctrl+M、Ctrl+J 都算）
        for enter in ["Enter", "Ctrl+M", "Ctrl+J"] {
            let (lines, rest) = submitted_lines("rm -rf /tmp/x", None, &keys(&[enter]));
            assert_eq!(lines, ["rm -rf /tmp/x"]);
            assert_eq!(rest, "");
        }
        // 文字里带换行：每行各算一次，最后没换行的留着
        let (lines, rest) = submitted_lines("sudo ", Some("reboot\nls\r\necho x"), &[]);
        assert_eq!(lines, ["sudo reboot", "ls"]);
        assert_eq!(rest, "echo x");
        // Ctrl+C、Ctrl+U 清空，退格删一个字，Ctrl+W 删一个词
        assert_eq!(
            submitted_lines("rm -rf /", None, &keys(&["Ctrl+C", "Enter"])).0,
            Vec::<String>::new()
        );
        assert_eq!(
            submitted_lines("", Some("rm -rf /x\x15ls"), &keys(&["Enter"])).0,
            ["ls"]
        );
        assert_eq!(
            submitted_lines("", Some("lss\x7f"), &keys(&["Enter"])).0,
            ["ls"]
        );
        assert_eq!(
            submitted_lines("ls /tmp/x  ", None, &keys(&["Ctrl+W", "Enter"])).0,
            ["ls "]
        );
        assert_eq!(
            submitted_lines("", Some("ls /tmp/x\x17"), &keys(&["Enter"])).0,
            ["ls "]
        );
    }

    #[test]
    fn 移动光标的键不清空这一行_先按_end_再回车照样要判断() {
        for key in [
            "End", "Home", "Left", "Right", "Esc", "Tab", "Ctrl+A", "Ctrl+E",
        ] {
            assert_eq!(
                submitted_lines("rm -rf /x", None, &keys(&[key, "Enter"])).0,
                ["rm -rf /x"],
                "{key}"
            );
        }
        // 文字里的控制序列：左（ESC [ D）、End（ESC [ F、ESC O F）、Delete（ESC [ 3~）保留
        for sequence in ["\x1b[D", "\x1b[F", "\x1bOF", "\x1b[3~"] {
            assert_eq!(
                submitted_lines("", Some(&format!("rm -rf /x{sequence}\r")), &[]).0,
                ["rm -rf /x"],
                "{sequence:?}"
            );
        }
    }

    #[test]
    fn 翻历史又回车的输入能认出来() {
        use super::submits_recalled_history;
        assert!(submits_recalled_history(None, &keys(&["Up", "Enter"])));
        assert!(submits_recalled_history(Some("\x1b[A\r"), &[]));
        assert!(submits_recalled_history(Some("\x10"), &keys(&["Ctrl+M"])));
        assert!(!submits_recalled_history(None, &keys(&["Up"])));
        assert!(!submits_recalled_history(Some("ls\r"), &[]));
        assert!(!submits_recalled_history(None, &keys(&["End", "Enter"])));
    }

    #[test]
    fn 翻历史之后内容不可知_清空() {
        for key in ["Up", "Down", "PageUp", "PageDown", "Ctrl+P", "Ctrl+N"] {
            assert_eq!(
                submitted_lines("rm -rf /x", None, &keys(&[key])).1,
                "",
                "{key}"
            );
        }
        for sequence in ["\x1b[A", "\x1bOB", "\x1b[5~", "\x10"] {
            assert_eq!(
                submitted_lines("", Some(&format!("rm -rf /x{sequence}")), &[]).1,
                "",
                "{sequence:?}"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::terminal::{TerminalKind, TerminalSpec};
    use std::sync::Arc;
    fn shell(m: &TerminalManager) -> String {
        m.open(
            TerminalSpec {
                kind: TerminalKind::Local {
                    program: "/bin/sh".into(),
                    args: vec!["-i".into()],
                    cwd: None,
                    env: vec![],
                },
                cols: 100,
                rows: 24,
            },
            Arc::new(|_| {}),
        )
        .unwrap()
    }
    fn wait(m: &TerminalManager, id: &str, pattern: &str, since: u64) -> TerminalWait {
        terminal_wait(
            m,
            id,
            Some(&Regex::new(pattern).unwrap()),
            None,
            Some(since),
            Duration::from_secs(3),
            &AtomicBool::new(false),
        )
        .unwrap()
    }
    #[test]
    fn 按键映射拒绝未知键并保持原子入队() {
        for (key, expected) in [
            ("Enter", "\r"),
            ("Tab", "\t"),
            ("Esc", "\x1b"),
            ("Backspace", "\x7f"),
            ("Delete", "\x1b[3~"),
            ("Up", "\x1b[A"),
            ("Down", "\x1b[B"),
            ("Left", "\x1b[D"),
            ("Right", "\x1b[C"),
            ("Home", "\x1b[H"),
            ("End", "\x1b[F"),
            ("PageUp", "\x1b[5~"),
            ("PageDown", "\x1b[6~"),
        ] {
            assert_eq!(key_bytes(key).unwrap(), expected.as_bytes());
        }
        for c in b'A'..=b'Z' {
            assert_eq!(
                key_bytes(&format!("Ctrl+{}", c as char)).unwrap(),
                [c - b'A' + 1]
            );
        }
        assert!(key_bytes("unknown").is_err());
    }
    #[test]
    fn 本机终端收发正则命中游标安静超时及退出() {
        let m = TerminalManager::new();
        let id = shell(&m);
        terminal_send(&m, &id, Some("stty -echo\n"), &[]).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let start = m.output_since(&id, 0).unwrap().next;
        let sent = terminal_send(
            &m,
            &id,
            Some("printf '\\033[31m甲乙\\033[0m余下\\n'\n"),
            &[],
        )
        .unwrap();
        let matched = wait(&m, &id, "甲乙", sent.cursor);
        assert!(matched.matched);
        assert!(matched.text.ends_with("甲乙"));
        assert!(wait(&m, &id, "余下", matched.cursor).matched);
        assert!(terminal_screen(&m, &id).unwrap().contains("甲乙余下"));
        let read = terminal_read(&m, &id, Some(start), None).unwrap();
        assert!(!read.text.contains('\x1b'));
        let quiet = terminal_wait(
            &m,
            &id,
            None,
            Some(Duration::from_millis(20)),
            None,
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(!quiet.matched && !quiet.timed_out);
        let default_quiet = terminal_wait(
            &m,
            &id,
            None,
            None,
            None,
            Duration::from_secs(3),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(!default_quiet.timed_out);
        let tail = terminal_read(&m, &id, None, Some(1)).unwrap();
        assert!(tail.text.len() <= 1);
        assert!(!tail.more);
        let timeout = terminal_wait(
            &m,
            &id,
            None,
            None,
            None,
            Duration::from_millis(30),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(timeout.timed_out);
        assert_eq!(
            terminal_wait(
                &m,
                &id,
                None,
                None,
                None,
                Duration::from_secs(1),
                &AtomicBool::new(true)
            )
            .unwrap_err()
            .code,
            "mcp.cancelled"
        );
        terminal_send(&m, &id, Some("exit\n"), &[]).unwrap();
        assert!(
            terminal_wait(
                &m,
                &id,
                None,
                None,
                None,
                Duration::from_secs(2),
                &AtomicBool::new(false)
            )
            .unwrap()
            .exited
        );
        assert!(
            terminal_wait(
                &m,
                &id,
                None,
                None,
                None,
                Duration::from_secs(2),
                &AtomicBool::new(false)
            )
            .unwrap()
            .exited
        );
    }
    #[test]
    fn 密码提示拒绝文字放行按键且用户接管拒绝自动化() {
        let m = TerminalManager::new();
        let id = shell(&m);
        terminal_send(
            &m,
            &id,
            Some("stty -echo; printf '\\nPassword: '; read value\n"),
            &[],
        )
        .unwrap();
        wait(&m, &id, "Password:", 0);
        let deadline = Instant::now();
        while !looks_like_password_prompt(&terminal_screen(&m, &id).unwrap()) {
            assert!(deadline.elapsed() < Duration::from_secs(2));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            terminal_send(&m, &id, Some("秘密"), &[]).unwrap_err().code,
            "mcp.password_prompt"
        );
        assert!(
            terminal_send(&m, &id, None, &["Ctrl+C".into()])
                .unwrap()
                .queued
        );
        m.set_user_control(&id, true).unwrap();
        assert_eq!(
            terminal_send(&m, &id, None, &["Enter".into()])
                .unwrap_err()
                .code,
            "terminal.automation_rejected"
        );
        m.close(&id).unwrap();
    }
    #[test]
    fn 四兆历史下千次增量和尾部读取保持有界耗时() {
        let manager = TerminalManager::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let id = manager
            .open(
                TerminalSpec {
                    kind: TerminalKind::Local {
                        program: "/bin/sh".into(),
                        args: vec![
                            "-c".into(),
                            "head -c 4194304 /dev/zero | tr '\\000' x; printf '\\033]0;'; head -c 102400 /dev/zero | tr '\\000' x; printf '\\007DONE'; read line"
                                .into(),
                        ],
                        cwd: None,
                        env: vec![],
                    },
                    cols: 100,
                    rows: 24,
                },
                Arc::new(move |event| {
                    let _ = tx.send(event);
                }),
            )
            .unwrap();
        let mut received = 0;
        while received < 4 * 1024 * 1024 + 102400 + 9 {
            let TerminalEvent::Output(bytes) = rx.recv_timeout(Duration::from_secs(15)).unwrap()
            else {
                panic!("输出历史填满前终端已退出");
            };
            manager.ack(&id, bytes.len()).unwrap();
            received += bytes.len();
        }
        let end = manager.output_position(&id).unwrap();
        assert_eq!(
            manager.output_since(&id, 0).unwrap().data.len(),
            4 * 1024 * 1024
        );
        let started = Instant::now();
        for _ in 0..1000 {
            let read = read_chunk(&manager, &id, Some(end), None).unwrap().0;
            assert_eq!(read.cursor, end);
            assert!(read.text.is_empty() && !read.more && !read.lost);
            let tail = read_chunk(&manager, &id, None, Some(4)).unwrap().0;
            assert_eq!(tail.text, "DONE");
            assert_eq!(tail.cursor, end);
            assert!(!tail.more && !tail.lost);
        }
        let elapsed = started.elapsed();
        eprintln!("四兆历史下各一千次增量和尾部读取耗时：{elapsed:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "增量读取耗时过长：{elapsed:?}"
        );
        assert_eq!(
            read_chunk(&manager, &id, Some(end + 1), None)
                .unwrap_err()
                .code,
            "terminal.invalid_position"
        );
        let first = read_chunk(&manager, &id, Some(0), None).unwrap().0;
        assert!(first.lost && first.more);
        assert_eq!(first.text.len(), READ_LIMIT);
        for (since, prefix, consumed_control) in [
            (4194304, "", crate::mcp::text::STRING_SEQUENCE_LIMIT),
            (4194306, "0;", 0),
        ] {
            let read = read_chunk(&manager, &id, Some(since), None).unwrap().0;
            // 从引导符开始时最多消费 4 KiB 控制序列；显式给出序列内部游标时保留原始起点。
            assert_eq!(
                read.text,
                format!("{prefix}{}", "x".repeat(READ_LIMIT - prefix.len()))
            );
            assert_eq!(read.cursor, since + (consumed_control + READ_LIMIT) as u64);
            assert!(read.more && !read.lost);
            let next = read_chunk(&manager, &id, Some(read.cursor), None)
                .unwrap()
                .0;
            assert_eq!(
                next.text,
                format!("{}\x07DONE", "x".repeat((end - read.cursor - 5) as usize))
            );
            assert_eq!(next.cursor, end);
            assert!(!next.more && !next.lost);
        }
        manager.close(&id).unwrap();
        assert_eq!(
            manager.output_position("不存在").unwrap_err().code,
            "terminal.not_found"
        );
        assert_eq!(
            read_chunk(&manager, "不存在", None, None).unwrap_err().code,
            "terminal.not_found"
        );
    }

    #[test]
    fn 未终止字符串控制序列后的正常输出可读取和等待() {
        let manager = TerminalManager::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let id = manager
            .open(
                TerminalSpec {
                    kind: TerminalKind::Local {
                        program: "/bin/sh".into(),
                        args: vec![
                            "-c".into(),
                            "printf '\\033]'; head -c 4094 /dev/zero | tr '\\000' x; printf READY; read line".into(),
                        ],
                        cwd: None,
                        env: vec![],
                    },
                    cols: 100,
                    rows: 24,
                },
                Arc::new(move |event| {
                    let _ = tx.send(event);
                }),
            )
            .unwrap();
        let end = crate::mcp::text::STRING_SEQUENCE_LIMIT + 5;
        let mut received = 0;
        while received < end {
            let TerminalEvent::Output(bytes) = rx.recv_timeout(Duration::from_secs(5)).unwrap()
            else {
                panic!("输出完成前终端已退出");
            };
            manager.ack(&id, bytes.len()).unwrap();
            received += bytes.len();
        }
        let read = terminal_read(&manager, &id, Some(0), None).unwrap();
        assert_eq!(read.text, "READY");
        assert_eq!(read.cursor, end as u64);
        assert!(!read.more);
        let wait = terminal_wait(
            &manager,
            &id,
            Some(&Regex::new("READY").unwrap()),
            None,
            Some(0),
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(wait.matched && !wait.timed_out);
        assert_eq!(wait.text, "READY");
        assert_eq!(wait.cursor, end as u64);
        manager.close(&id).unwrap();
    }

    #[test]
    fn 输出限额宽字符及控制序列游标() {
        let bytes = format!("\x1b[31m{}\x1b[0m尾", "甲".repeat(40000));
        let (text, offsets, count) = clean_prefix(bytes.as_bytes(), 100, false);
        assert!(text.len() <= READ_LIMIT && std::str::from_utf8(text.as_bytes()).is_ok());
        assert_eq!(offsets.back().copied(), Some(100 + count as u64));
        assert!(count < bytes.len());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use crate::terminal::{TerminalKind, TerminalSpec};
    use std::sync::Arc;

    #[test]
    fn 控制台换行提交能执行命令() {
        let manager = TerminalManager::new();
        let id = manager
            .open(
                TerminalSpec {
                    kind: TerminalKind::Local {
                        program: std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                            .join("System32/WindowsPowerShell/v1.0/powershell.exe"),
                        args: vec!["-NoLogo".into(), "-NoProfile".into()],
                        cwd: None,
                        env: vec![],
                    },
                    cols: 100,
                    rows: 24,
                },
                Arc::new(|_| {}),
            )
            .unwrap();
        let pattern = Regex::new("LF_OK").unwrap();
        let sent =
            terminal_send(&manager, &id, Some("Write-Output ('LF_' + 'OK')\n"), &[]).unwrap();
        let output = terminal_wait(
            &manager,
            &id,
            Some(&pattern),
            None,
            Some(sent.cursor),
            Duration::from_secs(15),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(output.matched, "{output:?}");
        manager.close(&id).unwrap();
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn 历史窗口遇到未终止序列最多重读一次且复制总量有界() {
        use crate::mcp::text::STRING_SEQUENCE_LIMIT;
        let initial = READ_LIMIT + ALIGN_LOOKBEHIND;
        for prefix in [0, initial - 1, 2 * initial - 1] {
            // 前缀只有可完整消费的控制序列，覆盖在首轮和重读窗口末尾截断的情形。
            let mut history = b"\x1b7".repeat(prefix / 2);
            if prefix % 2 == 1 {
                history.push(b'a');
            }
            let sequence = history.len();
            history.extend_from_slice(b"\x1b]");
            history.resize(sequence + STRING_SEQUENCE_LIMIT, b'x');
            history.extend_from_slice(b"READY");
            history.resize(4 * 1024 * 1024, b'x');
            let mut since = 0;
            let mut found = false;
            for _ in 0..3 {
                let mut copies = Vec::new();
                let read = read_window(Some(since), |limit| {
                    let start = (since as usize).saturating_sub(ALIGN_LOOKBEHIND);
                    let end = (since as usize + limit).min(history.len());
                    let data = history[start..end].to_vec();
                    copies.push(data.len());
                    Ok((
                        crate::terminal::OutputRead {
                            start: start as u64,
                            next: history.len() as u64,
                            data,
                            lost: false,
                            quiet_for: Duration::ZERO,
                        },
                        since,
                    ))
                })
                .unwrap()
                .0;
                assert!(copies.len() <= 2);
                assert!(copies.iter().sum::<usize>() <= 3 * READ_LIMIT + 5 * ALIGN_LOOKBEHIND);
                assert!(
                    copies
                        .iter()
                        .all(|n| *n <= 2 * READ_LIMIT + 3 * ALIGN_LOOKBEHIND)
                );
                assert!(read.cursor > since);
                since = read.cursor;
                found |= read.text.contains("READY");
                if found {
                    break;
                }
            }
            assert!(found, "截断位置：{prefix}");
        }
    }

    #[test]
    fn 分块中文和控制序列未完整前不推进游标() {
        for data in [
            "甲".as_bytes(),
            b"\x1b[31m".as_slice(),
            b"\x1b]0;title\x1b\\",
        ] {
            for split in 1..data.len() {
                let bytes = [b"a", &data[..split]].concat();
                let (text, _, count) = clean_prefix(&bytes, 0, false);
                assert_eq!(text, "a");
                let expected = if bytes.ends_with(b"\x1b") && data.starts_with(b"\x1b]") {
                    bytes.len() - 1
                } else {
                    1
                };
                assert_eq!(count, expected, "{data:?}/{split}");
                let (tail, _, consumed) = clean_prefix(data, count as u64, false);
                assert_eq!(consumed, data.len());
                assert_eq!(tail, if data[0] == 0x1b { "" } else { "甲" });
            }
        }
    }

    #[test]
    fn 分页和尾部起点均对齐且返回更多标志() {
        let data = format!("{}甲\x1b[31m尾\x1b[0m", "x".repeat(READ_LIMIT - 1)).into_bytes();
        let output = crate::terminal::OutputRead {
            start: 0,
            next: data.len() as u64,
            data,
            lost: false,
            quiet_for: Duration::ZERO,
        };
        let first = read_output(&output, 0, false).0;
        assert!(first.more);
        assert_eq!(first.cursor, (READ_LIMIT - 1) as u64);
        let second = read_output(&output, first.cursor, false).0;
        assert_eq!(second.text, "甲尾");
        assert!(!second.more);
        assert_eq!(second.cursor, output.next);
        assert_eq!(read_output(&output, READ_LIMIT as u64, true).0.text, "尾");
        assert_eq!(
            read_output(&output, (READ_LIMIT + 3) as u64, true).0.text,
            "尾"
        );
    }
    #[test]
    fn 有界回看对齐多字节字符和各类控制序列内部起点() {
        for token in [
            "甲😀".as_bytes(),
            b"\x1b[38;2;255;0;0m",
            b"\x1b]0;title\x07",
            b"\x1b]0;title\x1b\\",
            b"\x1bPpayload\x1b\\",
            b"\x1b(B",
            b"\x1bOA",
            b"\r\n",
        ] {
            let mut data = vec![b'x'; 2 * ALIGN_LOOKBEHIND];
            let start = data.len();
            data.extend_from_slice(token);
            data.extend_from_slice(b"tail");
            let output = crate::terminal::OutputRead {
                start: 100,
                next: 100 + data.len() as u64,
                data,
                lost: false,
                quiet_for: Duration::ZERO,
            };
            for split in 1..token.len() {
                let read = read_output(&output, 100 + (start + split) as u64, true).0;
                let expected = if token == "甲😀".as_bytes() && split <= 3 {
                    "😀tail"
                } else {
                    "tail"
                };
                assert_eq!(read.text, expected, "{token:?}/{split}");
                assert_eq!(read.cursor, output.next);
                assert!(!read.more && !read.lost);
            }
        }
    }
}

#[cfg(test)]
mod reading_tests {
    use super::*;
    use crate::terminal::OutputRead;

    fn snapshot(data: Vec<u8>, quiet_for: Duration) -> OutputRead {
        OutputRead {
            start: 0,
            next: data.len() as u64,
            data,
            lost: false,
            quiet_for,
        }
    }

    #[test]
    fn 字符串静止后仅丢引导符并保留后续游标() {
        for kind in *b"]PX^_" {
            let mut data = vec![0x1b, kind];
            data.extend(vec![b'x'; 2048]);
            data.extend_from_slice(b"$ ");
            let mut output = snapshot(data, Duration::from_millis(999));
            let first = read_output(&output, 0, false).0;
            assert_eq!(first.text, "");
            assert_eq!(first.cursor, 0);
            output.quiet_for = Duration::from_secs(1);
            let ready = read_output(&output, 0, false).0;
            assert_eq!(ready.text, format!("{}$ ", "x".repeat(2048)));
            assert_eq!(ready.cursor, output.next);
            assert_eq!(read_output(&output, output.next - 2, true).0.text, "$ ");
            output.data.extend_from_slice(b"new\x07\x1b[32m$ \x1b[0m");
            output.next = output.data.len() as u64;
            output.quiet_for = Duration::ZERO;
            let next = read_output(&output, ready.cursor, false).0;
            assert_eq!(next.text, "new\u{7}$ ");
            assert_eq!(next.cursor, output.next);
        }
    }

    #[test]
    fn 字符串被转义或取消终止时一次读取到提示符() {
        for kind in *b"]PX^_" {
            for ending in [
                b"\x1b[32m$ \x1b[0m".as_slice(),
                b"\x1b\\$ ",
                b"\x18$ ",
                b"\x1a$ ",
            ] {
                let data = [vec![0x1b, kind], vec![b'x'; 2048], ending.to_vec()].concat();
                let output = snapshot(data, Duration::ZERO);
                let read = read_output(&output, 0, false).0;
                assert_eq!(read.text, "$ ");
                assert_eq!(read.cursor, output.next);
            }
        }
    }

    #[test]
    fn 显式游标保留跨块换行且计算起点仍向后对齐() {
        let mut output = snapshot(b"a\r".to_vec(), Duration::ZERO);
        let first = read_output(&output, 0, false).0;
        assert_eq!(first.text, "a\r");
        output.data.extend_from_slice(b"\nb");
        output.next = output.data.len() as u64;
        assert_eq!(read_output(&output, first.cursor, false).0.text, "\nb");
        let mut output = snapshot("甲\x1b[32m尾".as_bytes().to_vec(), Duration::ZERO);
        assert_eq!(read_output(&output, 1, true).0.text, "尾");
        assert_eq!(read_output(&output, 5, true).0.text, "尾");
        assert_eq!(read_output(&output, 1, false).0.text, "��尾");
        output.data.drain(..1);
        output.start = 1;
        output.lost = true;
        assert_eq!(read_output(&output, 0, false).0.text, "尾");
    }

    #[test]
    fn 窗口读完只复制一次且静止规则只作用于真正末尾() {
        for (data, quiet_for, hidden_tail) in [
            (b"\x1b7".repeat(READ_LIMIT), Duration::ZERO, true),
            (b"\x1b]unfinished$ ".to_vec(), Duration::from_secs(1), false),
            (b"\x1b]unfinished$ ".to_vec(), Duration::from_secs(1), true),
        ] {
            let mut copies = 0;
            let mut output = snapshot(data, quiet_for);
            output.next += u64::from(hidden_tail);
            let read = read_window(Some(0), |_| {
                copies += 1;
                Ok((output.clone(), 0))
            })
            .unwrap()
            .0;
            if output.data.starts_with(b"\x1b]") && hidden_tail {
                assert_eq!(copies, 2);
                assert_eq!(read.cursor, 0);
            } else {
                assert_eq!(copies, 1);
                assert_eq!(read.cursor, output.data.len() as u64);
            }
        }
    }

    #[cfg(unix)]
    fn manager() -> (TerminalManager, String) {
        use crate::terminal::{TerminalKind, TerminalSpec};
        let manager = TerminalManager::new();
        let id = manager
            .open(
                TerminalSpec {
                    kind: TerminalKind::Local {
                        program: "/bin/cat".into(),
                        args: vec![],
                        cwd: None,
                        env: vec![],
                    },
                    cols: 80,
                    rows: 24,
                },
                std::sync::Arc::new(|_| {}),
            )
            .unwrap();
        (manager, id)
    }

    #[cfg(unix)]
    #[test]
    fn 管理器读取未终止字符串注入时间后等待命中且新增输出不丢失() {
        let pattern = Regex::new(r"\$ ").unwrap();
        for kind in *b"]PX^_" {
            let (manager, id) = manager();
            let now = Instant::now();
            let data = [vec![0x1b, kind], vec![b'x'; 2048], b"$ ".to_vec()].concat();
            manager.feed_output_at(&id, &data, now).unwrap();
            assert_eq!(
                read_chunk_at(
                    &manager,
                    &id,
                    Some(0),
                    None,
                    now + Duration::from_millis(999)
                )
                .unwrap()
                .0
                .cursor,
                0
            );
            let later = now + Duration::from_secs(1);
            assert_eq!(
                read_chunk_at(&manager, &id, None, Some(2), later)
                    .unwrap()
                    .0
                    .text,
                "$ "
            );
            let ready = read_chunk_at(&manager, &id, Some(0), None, later)
                .unwrap()
                .0;
            assert_eq!(ready.text, format!("{}$ ", "x".repeat(2048)));
            let wait = wait_with_clock(
                &manager,
                &id,
                Some(&pattern),
                None,
                Some(0),
                Duration::ZERO,
                || false,
                || later,
            )
            .unwrap();
            assert!(wait.matched && !wait.timed_out);
            assert_eq!(wait.cursor, ready.cursor);
            let tail = b"new\x07\x1b[32m$ \x1b[0m";
            manager.feed_output_at(&id, tail, later).unwrap();
            let next = terminal_read(&manager, &id, Some(ready.cursor), None).unwrap();
            assert_eq!(next.text, "new\u{7}$ ");
            assert_eq!(next.cursor, (data.len() + tail.len()) as u64);
            manager.close(&id).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn 管理器字符串终止与换行游标及尾部对齐() {
        for kind in *b"]PX^_" {
            for ending in [
                b"\x1b[32m$ \x1b[0m".as_slice(),
                b"\x1b\\$ ",
                b"\x18$ ",
                b"\x1a$ ",
            ] {
                let (manager, id) = manager();
                let data = [vec![0x1b, kind], vec![b'x'; 2048], ending.to_vec()].concat();
                manager.feed_output_at(&id, &data, Instant::now()).unwrap();
                assert_eq!(
                    terminal_read(&manager, &id, Some(0), None).unwrap().text,
                    "$ "
                );
                manager.close(&id).unwrap();
            }
        }
        let (manager, id) = manager();
        manager.feed_output_at(&id, b"a\r", Instant::now()).unwrap();
        let first = terminal_read(&manager, &id, None, None).unwrap();
        assert_eq!(first.text, "a\r");
        manager.feed_output_at(&id, b"\nb", Instant::now()).unwrap();
        assert_eq!(
            terminal_read(&manager, &id, Some(first.cursor), None)
                .unwrap()
                .text,
            "\nb"
        );
        // tail_bytes 未改变显式起点时，也必须保留原来的游标。
        assert_eq!(
            terminal_read(&manager, &id, Some(first.cursor), Some(100))
                .unwrap()
                .text,
            "\nb"
        );
        manager
            .feed_output_at(&id, "甲\x1b[32m尾".as_bytes(), Instant::now())
            .unwrap();
        for tail_bytes in [10, 6] {
            assert_eq!(
                terminal_read(&manager, &id, None, Some(tail_bytes))
                    .unwrap()
                    .text,
                "尾"
            );
        }
        manager.close(&id).unwrap();
    }
}
