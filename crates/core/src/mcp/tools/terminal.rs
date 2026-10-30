//! 终端自动化输入及有界增量输出；权限和 AI 登记由分发层检查。
use crate::{
    error::AppError,
    mcp::{safety::looks_like_password_prompt, text::strip_ansi},
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
    pub reason: &'static str,
}
#[derive(Debug, Default, Serialize)]
pub struct TerminalRead {
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
    let mut bytes = text.unwrap_or_default().as_bytes().to_vec();
    for key in keys {
        bytes.extend(key_bytes(key)?);
    }
    if text.is_some_and(|v| !v.is_empty()) && looks_like_password_prompt(&m.screen_text(id)?) {
        return Err(AppError::new("mcp.password_prompt").with_reason(
            "mcp.terminal_is_waiting_for_a_password_enter_it_in_pierlink",
            "终端正在等待密码，请用户在 Pierlink 里输入",
        ));
    }
    if let Some(generation) = generation {
        m.write_automation_at_generation(id, &bytes, generation)?;
    } else {
        m.write_from(id, &bytes, InputSource::Automation)?;
    }
    Ok(SendOutcome {
        queued: true,
        reason: "已入队，不表示已执行；请读取或等待终端输出确认",
    })
}

// 每个文字字节记录对应原始输出的结束位置，保证正则命中后游标不会越过未返回文字。
fn clean_prefix(bytes: &[u8], start: u64) -> (String, VecDeque<u64>, usize) {
    let mut text = String::new();
    let mut positions = VecDeque::new();
    let mut i = 0;
    while i < bytes.len() {
        let mut end = i + 1;
        if bytes[i] == 0x1b {
            let Some(&kind) = bytes.get(end) else { break };
            end += 1;
            match kind {
                b'[' => {
                    while end < bytes.len() {
                        let b = bytes[end];
                        end += 1;
                        if (0x40..=0x7e).contains(&b) {
                            break;
                        }
                    }
                }
                b']' | b'P' | b'X' | b'^' | b'_' => {
                    while end < bytes.len() {
                        if kind == b']' && bytes[end] == 7 {
                            end += 1;
                            break;
                        }
                        if bytes[end..].starts_with(b"\x1b\\") {
                            end += 2;
                            break;
                        }
                        end += 1;
                    }
                }
                b'O' | b'N' => {
                    end = (end + 1).min(bytes.len());
                }
                0x20..=0x2f => {
                    while bytes.get(end).is_some_and(|b| (0x20..=0x2f).contains(b)) {
                        end += 1;
                    }
                    end = (end + 1).min(bytes.len());
                }
                _ => {}
            }
        } else if bytes[i..].starts_with(b"\r\n") {
            end += 1;
        } else if bytes[i] >= 0xc2 {
            let width = match bytes[i] {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => 1,
            };
            if i + width <= bytes.len() && std::str::from_utf8(&bytes[i..i + width]).is_ok() {
                end = i + width;
            }
        }
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
) -> Result<(TerminalRead, VecDeque<u64>), AppError> {
    let output = m.output_since(id, since.unwrap_or(0))?;
    let (text, positions, consumed) = clean_prefix(&output.data, output.start);
    Ok((
        TerminalRead {
            text,
            cursor: output.start + consumed as u64,
            lost: output.lost,
        },
        positions,
    ))
}
/// 从 id 的 since（默认历史开头）读取至多 64 KiB 去 ANSI 文字，返回原始字节游标和历史丢失标记。
pub fn terminal_read(
    m: &TerminalManager,
    id: &str,
    since: Option<u64>,
) -> Result<TerminalRead, AppError> {
    read_chunk(m, id, since).map(|v| v.0)
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
    let start = Instant::now();
    let timeout = timeout.min(Duration::from_secs(1800));
    let subscription = match m.subscribe(id, READ_LIMIT) {
        Ok(s) => Some(s),
        Err(e) if e.code == "terminal.not_found" => None,
        Err(e) => return Err(e),
    };
    let mut result = TerminalWait {
        cursor: since.unwrap_or(m.output_since(id, 0).map(|v| v.next).unwrap_or(0)),
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
        match read_chunk(m, id, Some(previous)) {
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
            .output_since(id, result.cursor)
            .map(|v| !v.data.is_empty())
            .unwrap_or(false);
        if result.exited && !pending {
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
        terminal_send(
            &m,
            &id,
            Some("printf '\\033[31m甲乙\\033[0m余下\\n'\n"),
            &[],
        )
        .unwrap();
        let matched = wait(&m, &id, "甲乙", start);
        assert!(matched.matched);
        assert!(matched.text.ends_with("甲乙"));
        assert!(wait(&m, &id, "余下", matched.cursor).matched);
        assert!(terminal_screen(&m, &id).unwrap().contains("甲乙余下"));
        let read = terminal_read(&m, &id, Some(start)).unwrap();
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
    fn 输出限额宽字符及控制序列游标() {
        let bytes = format!("\x1b[31m{}\x1b[0m尾", "甲".repeat(40000));
        let (text, offsets, count) = clean_prefix(bytes.as_bytes(), 100);
        assert!(text.len() <= READ_LIMIT && std::str::from_utf8(text.as_bytes()).is_ok());
        assert_eq!(offsets.back().copied(), Some(100 + count as u64));
        assert!(count < bytes.len());
    }
}
