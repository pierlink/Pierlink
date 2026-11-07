//! 终端自动化输入及有界增量输出；权限和 AI 登记由分发层检查。
use crate::{
    error::AppError,
    mcp::{
        safety::looks_like_password_prompt,
        text::{ALIGN_LOOKBEHIND, aligned_start_settled, strip_ansi, token_len_settled},
    },
    terminal::{TerminalEvent, TerminalManager},
};
use regex::Regex;
use serde::Serialize;
use std::{
    collections::VecDeque,
    path::Path,
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
    /// 等待期间终端里的程序发来的通知（响铃为 bell），收到就立即返回
    pub notification: Option<String>,
}
/// 固定字节的按键名（Enter、Ctrl+C、Up……）对应的字节；不认识的按键返回 mcp.invalid_argument。
/// 要看终端状态的按键（Shift+Enter、Ctrl+Enter、Alt+Enter、WheelUp、WheelDown）用 key_sequence。
pub fn key_bytes(key: &str) -> Result<Vec<u8>, AppError> {
    let bytes: &[u8] = match key {
        "Enter" => b"\r",
        "Tab" => b"\t",
        "Shift+Tab" => b"\x1b[Z",
        "Shift+Up" => b"\x1b[1;2A",
        "Shift+Down" => b"\x1b[1;2B",
        "Shift+Right" => b"\x1b[1;2C",
        "Shift+Left" => b"\x1b[1;2D",
        "Shift+Home" => b"\x1b[1;2H",
        "Shift+End" => b"\x1b[1;2F",
        "Alt+Up" => b"\x1b[1;3A",
        "Alt+Down" => b"\x1b[1;3B",
        "Alt+Right" => b"\x1b[1;3C",
        "Alt+Left" => b"\x1b[1;3D",
        "Ctrl+Up" => b"\x1b[1;5A",
        "Ctrl+Down" => b"\x1b[1;5B",
        "Ctrl+Right" => b"\x1b[1;5C",
        "Ctrl+Left" => b"\x1b[1;5D",
        "Ctrl+Home" => b"\x1b[1;5H",
        "Ctrl+End" => b"\x1b[1;5F",
        // Ctrl+Backspace 和 Windows Terminal 一样发 Ctrl+H
        "Ctrl+Backspace" => b"\x08",
        "Alt+Backspace" => b"\x1b\x7f",
        "Ctrl+Delete" => b"\x1b[3;5~",
        // 符号组合键的控制字符：Ctrl+_ 和 Ctrl+/ 都是 0x1f（readline、emacs 的撤销）
        "Ctrl+_" | "Ctrl+/" => b"\x1f",
        "Ctrl+]" => b"\x1d",
        "Ctrl+\\" => b"\x1c",
        "Ctrl+^" => b"\x1e",
        "Esc" => b"\x1b",
        "Backspace" => b"\x7f",
        "Delete" => b"\x1b[3~",
        "Insert" => b"\x1b[2~",
        "F1" => b"\x1bOP",
        "F2" => b"\x1bOQ",
        "F3" => b"\x1bOR",
        "F4" => b"\x1bOS",
        "F5" => b"\x1b[15~",
        "F6" => b"\x1b[17~",
        "F7" => b"\x1b[18~",
        "F8" => b"\x1b[19~",
        "F9" => b"\x1b[20~",
        "F10" => b"\x1b[21~",
        "F11" => b"\x1b[23~",
        "F12" => b"\x1b[24~",
        "Ctrl+Space" => b"\0",
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
                .filter(|v| v.len() == 1 && v.as_bytes()[0].is_ascii_alphabetic())
            {
                return Ok(vec![letter.as_bytes()[0].to_ascii_uppercase() - b'A' + 1]);
            }
            if let Some(letter) = key
                .strip_prefix("Alt+")
                .filter(|v| v.len() == 1 && v.as_bytes()[0].is_ascii_alphabetic())
            {
                return Ok(vec![0x1b, letter.as_bytes()[0]]);
            }
            return Err(AppError::new("mcp.invalid_argument")
                .with_reason("mcp.unknown_terminal_key", "未知终端按键"));
        }
    };
    Ok(bytes.into())
}
/// Windows 控制台的按键格式（win32-input-mode）里的按下、松开：本机 PowerShell、命令提示符按它们自己的规则处理
const WIN32_SHIFT_ENTER: &[u8] = b"\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_";
const WIN32_CTRL_ENTER: &[u8] = b"\x1b[13;28;10;1;8;1_\x1b[13;28;10;0;8;1_";
const WIN32_CTRL_J: &[u8] = b"\x1b[74;36;10;1;8;1_\x1b[74;36;10;0;8;1_";

/// 本机终端里是哪种 shell，按程序文件名判断（和界面的 localShellFamily 一样）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalShell {
    PowerShell,
    Cmd,
    Wsl,
    /// Git Bash 等
    Other,
}

fn local_shell(program: &Path) -> LocalShell {
    let name = program
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match name.as_str() {
        "pwsh.exe" | "powershell.exe" => LocalShell::PowerShell,
        "cmd.exe" => LocalShell::Cmd,
        "wsl.exe" => LocalShell::Wsl,
        _ => LocalShell::Other,
    }
}

/// Shift+Enter、Ctrl+Enter（换行、不提交）要发的字节，规则和界面（kittyKeys.ts 的 newlineKey）一样：
/// - 本机 PowerShell、命令提示符，和本机 Git Bash 里的全屏程序（多半是 Windows 程序）：原样交给 Windows 控制台；
/// - 程序开了 kitty 键盘协议：按协议发 ESC[13;2u（Claude Code、Codex 当换行；Ctrl+Enter 也换行，发同样的）；
/// - 其他全屏程序（备用屏，含 screen、tmux 里的 Claude Code）：换行符 Ctrl+J；
/// - shell 的提示符：Ctrl+V 再接换行符，bash、zsh 插入换行、不执行。
///
/// :param shell: 本机终端的 shell；服务器终端为 None
/// :param kitty: 程序开着 kitty 键盘协议
/// :param alternate: 在备用屏（全屏程序）
/// :param shift: Shift+Enter 为 true，Ctrl+Enter 为 false
fn newline_bytes(
    shell: Option<LocalShell>,
    kitty: bool,
    alternate: bool,
    shift: bool,
) -> &'static [u8] {
    if native(shell, alternate) && !kitty {
        if shift {
            WIN32_SHIFT_ENTER
        } else {
            WIN32_CTRL_ENTER
        }
    } else if kitty {
        b"\x1b[13;2u"
    } else if alternate {
        b"\n"
    } else {
        b"\x16\n"
    }
}

/// 按键原样交给 Windows 控制台的终端：本机 PowerShell、命令提示符，和本机 Git Bash 里的全屏程序（多半是 Windows 程序）
fn native(shell: Option<LocalShell>, alternate: bool) -> bool {
    matches!(shell, Some(LocalShell::PowerShell | LocalShell::Cmd))
        || (shell == Some(LocalShell::Other) && alternate)
}

/// 换行键（Shift/Ctrl/Alt+Enter）在这个终端里是不是真的提交：本机命令提示符把它们当回车（Alt+Enter 是 Esc 清行加回车）；
/// 别的程序（Claude Code、Codex、bash、zsh、PSReadLine）只插入换行，真正提交是之后的回车
///
/// :param m: 终端管理器
/// :param id: 终端 ID
/// :return: 换行键会提交为 true
pub(super) fn newline_submits(m: &TerminalManager, id: &str) -> Result<bool, AppError> {
    Ok(shell_of(m, id)? == Some(LocalShell::Cmd))
}

/// 本机终端的 shell；服务器终端和非 Windows 上的终端（都是 Unix 程序）为 None
fn shell_of(m: &TerminalManager, id: &str) -> Result<Option<LocalShell>, AppError> {
    if !cfg!(windows) {
        return Ok(None);
    }
    Ok(m.local_program(id)?.as_deref().map(local_shell))
}

/// 一个按键要写给 id 的字节，规则和 Pierlink 界面一样：多数按键是固定的（见 key_bytes）；
/// Shift+Enter、Ctrl+Enter、Alt+Enter 按 kitty 键盘协议、全屏和本机 shell 决定；
/// WheelUp、WheelDown 和电脑上滚一格滚轮一样（程序接管了鼠标发滚轮事件，否则发上下方向键），只用于全屏程序。
///
/// :param m: 终端管理器
/// :param id: 终端 ID
/// :param key: 按键名
/// :return: 要写的字节；滚轮键在终端不在全屏程序（备用屏）时为 None；不认识的按键返回 mcp.invalid_argument
pub fn key_sequence(m: &TerminalManager, id: &str, key: &str) -> Result<Option<Vec<u8>>, AppError> {
    let kitty = || m.kitty_flags(id).map(|flags| flags & 1 != 0);
    match key {
        "WheelUp" | "WheelDown" => m.wheel_bytes(id, key == "WheelUp"),
        "Shift+Enter" | "Ctrl+Enter" => Ok(Some(
            newline_bytes(
                shell_of(m, id)?,
                kitty()?,
                m.alternate_screen(id)?,
                key == "Shift+Enter",
            )
            .to_vec(),
        )),
        // 和界面一样：开了 kitty 协议按协议发，否则是 Esc 加回车
        "Alt+Enter" => Ok(Some(if kitty()? {
            b"\x1b[13;3u".to_vec()
        } else {
            b"\x1b\r".to_vec()
        })),
        // 和界面一样：原样交给 Windows 控制台时发 Ctrl+J 的按键（控制台把换行符当 Ctrl+Enter）
        "Ctrl+J" if native(shell_of(m, id)?, m.alternate_screen(id)?) => {
            Ok(Some(WIN32_CTRL_J.to_vec()))
        }
        _ => key_bytes(key).map(Some),
    }
}

/// 滚轮键在普通命令行里不发给程序：告诉 AI 用 terminal_read 读更早的输出
pub(super) fn wheel_unavailable() -> AppError {
    AppError::new("mcp.wheel_unavailable").with_reason(
        "mcp.wheel_only_works_in_fullscreen_programs",
        "WheelUp/WheelDown 只用于全屏程序（备用屏，比如 Codex、less、vim）；普通命令行里更早的输出用 terminal_read 读",
    )
}

pub(super) fn normalized_key(key: &str) -> String {
    if let Some(letter) = key
        .strip_prefix("Ctrl+")
        .filter(|v| v.len() == 1 && v.as_bytes()[0].is_ascii_alphabetic())
    {
        format!("Ctrl+{}", letter.to_ascii_uppercase())
    } else {
        key.into()
    }
}
/// 删掉 line 末尾的一个词（连同它后面的空白），和 shell 里 Ctrl+W 一样
fn delete_word(line: &mut String) {
    let kept = line
        .trim_end()
        .trim_end_matches(|c: char| !c.is_whitespace())
        .len();
    line.truncate(kept);
}

/// 提交的按键：回车（Ctrl+O 执行这一行再取出下一条历史，也算）
fn submit_key(key: &str) -> bool {
    matches!(key, "Enter" | "Ctrl+M" | "Ctrl+J" | "Ctrl+O")
}

/// 换行键：多数 AI 程序和 shell 里只插入换行，命令提示符会当回车（见 newline_submits）
fn newline_key(key: &str) -> bool {
    matches!(key, "Shift+Enter" | "Ctrl+Enter" | "Alt+Enter")
}

/// 翻历史的按键：之后这一行换成了历史命令，内容无法知道。滚轮在没接管鼠标的全屏程序里是上下方向键；
/// Ctrl+R、Ctrl+S 是搜历史（回车执行搜到的那条）
fn history_key(key: &str) -> bool {
    matches!(
        key,
        "Up" | "Down"
            | "PageUp"
            | "PageDown"
            | "Ctrl+P"
            | "Ctrl+N"
            | "Ctrl+R"
            | "Ctrl+S"
            | "WheelUp"
            | "WheelDown"
    )
}

/// 模拟自动化输入的结果
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Typed {
    /// 这次按回车提交的各行（含之前没提交的部分）
    pub lines: Vec<String>,
    /// 这次提交时这一行有看不到的内容：用户打过字（交给 AI 之前或刚刚打的），或撤销、粘回了删掉的文字
    pub unknown: bool,
    /// 剩下没提交的部分
    pub rest: String,
    /// 剩下的部分有看不到的内容
    pub rest_unknown: bool,
}

/// 正在模拟的这一行
struct Line {
    text: String,
    unknown: bool,
    typed: Typed,
}
impl Line {
    fn submit(&mut self) {
        self.typed.unknown |= self.unknown;
        if !self.text.trim().is_empty() {
            self.typed.lines.push(std::mem::take(&mut self.text));
        }
        self.text.clear();
        self.unknown = false;
    }
    /// 换行键：真提交时（命令提示符）同回车；否则按提交判断一次这一行（万一程序当回车），
    /// 但不清空、接上换行，之后的回车再把整段一起判断
    fn newline(&mut self, submits: bool) {
        if submits {
            self.submit();
            return;
        }
        self.typed.unknown |= self.unknown;
        if !self.text.trim().is_empty() {
            self.typed.lines.push(self.text.clone());
        }
        self.text.push('\n');
    }
    /// Ctrl+O：执行这一行，再取出下一条历史（内容无法知道）
    fn submit_and_recall(&mut self) {
        self.submit();
        self.unknown = true;
    }
}

/// 按 text、keys 的顺序模拟自动化输入（换行键按真提交算，见 submitted_lines_with）
pub fn submitted_lines(pending: &str, unknown: bool, text: Option<&str>, keys: &[String]) -> Typed {
    submitted_lines_with(pending, unknown, text, keys, true)
}

/// 按 text、keys 的顺序模拟自动化输入，返回这次按了回车提交的整行（含之前没提交的部分）和之后剩下没提交的部分。
/// Enter、Ctrl+M、Ctrl+J、Ctrl+O 算回车（见 submit_key）；换行键按 newline_submits 算提交，或按提交判断一次再接上换行；
/// Ctrl+C、Ctrl+U 清空这一行；Ctrl+W、Alt+Backspace 删一个词；退格、Ctrl+Backspace 删一个字。
/// 上下方向键、PageUp/PageDown、Ctrl+P/Ctrl+N、Ctrl+R/Ctrl+S、滚轮会换成历史命令，内容无法知道，清空（判断是尽力而为）；
/// 撤销（Ctrl+_、Ctrl+/，和 Ctrl+X 开头的组合键）、粘回删掉的文字（Ctrl+Y）、Ctrl+O 之后这一行内容未知，提交时要按“内容未知”确认；
/// 左右方向键、Home、End、Esc 等只移动光标或不改内容的键保留这一行：光标挪动后再回车，提交的还是这一行，照样要判断。
///
/// :param pending: 之前自动化输入、还没按回车的部分
/// :param unknown: pending 里有看不到的内容（用户打过字）
/// :param text: 这次发送的文字
/// :param keys: 这次发送的按键（在文字之后）
/// :param newline_submits: 换行键在这个终端里真的提交（本机命令提示符）
/// :return: 这次提交的各行、剩下没提交的部分，以及它们里有没有看不到的内容
pub fn submitted_lines_with(
    pending: &str,
    unknown: bool,
    text: Option<&str>,
    keys: &[String],
    newline_submits: bool,
) -> Typed {
    let mut line = Line {
        text: pending.to_owned(),
        unknown,
        typed: Typed::default(),
    };
    let mut chars = text.unwrap_or_default().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' | '\n' => line.submit(),
            '\x03' | '\x15' => line.text.clear(),
            '\x17' => delete_word(&mut line.text),
            '\x7f' | '\x08' => {
                line.text.pop();
            }
            // Ctrl+P、Ctrl+N：上一条、下一条历史；Ctrl+R、Ctrl+S：搜历史
            '\x10' | '\x0e' | '\x12' | '\x13' => line.text.clear(),
            // Ctrl+_（撤销）、Ctrl+Y（粘回删掉的文字）、Ctrl+X（组合键开头，Ctrl+X Ctrl+U 也是撤销）
            '\x1f' | '\x19' | '\x18' => line.unknown = true,
            // Ctrl+O：执行这一行再取出下一条历史
            '\x0f' => line.submit_and_recall(),
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
                        line.text.clear();
                    }
                }
                _ => {}
            },
            c if c.is_control() => {}
            c => line.text.push(c),
        }
    }
    for key in keys {
        let key = normalized_key(key);
        match key.as_str() {
            "Ctrl+O" => line.submit_and_recall(),
            key if submit_key(key) => line.submit(),
            key if newline_key(key) => line.newline(newline_submits),
            key if history_key(key) => line.text.clear(),
            "Ctrl+C" | "Ctrl+U" => line.text.clear(),
            "Ctrl+W" | "Alt+Backspace" => delete_word(&mut line.text),
            "Backspace" | "Ctrl+H" | "Ctrl+Backspace" => {
                line.text.pop();
            }
            "Ctrl+_" | "Ctrl+/" | "Ctrl+Y" | "Ctrl+X" => line.unknown = true,
            _ => {}
        }
    }
    Typed {
        rest: line.text,
        rest_unknown: line.unknown,
        ..line.typed
    }
}

/// 这次输入里是不是翻了历史（上下键、PageUp/PageDown、Ctrl+P/N、滚轮）又按了回车（含换行键）：翻出来的命令内容不可知。
/// 只看这一次发送；翻历史和回车分两次发的认不出（尽力而为）。
///
/// :param text: 这次发送的文字
/// :param keys: 这次发送的按键
/// :return: 是否既翻了历史又提交了
pub fn submits_recalled_history(text: Option<&str>, keys: &[String]) -> bool {
    let text = text.unwrap_or_default();
    let recalled = keys.iter().any(|key| history_key(&normalized_key(key)))
        || [
            "\x1b[A", "\x1b[B", "\x1bOA", "\x1bOB", "\x1b[5~", "\x1b[6~", "\x10", "\x0e", "\x12",
            "\x13",
        ]
        .iter()
        .any(|sequence| text.contains(sequence));
    let submits = keys.iter().any(|key| {
        let key = normalized_key(key);
        submit_key(&key) || newline_key(&key)
    }) || text.contains(['\r', '\n', '\x0f']);
    recalled && submits
}

/// 将 text 中的 CRLF 和 LF 统一成终端提交用的 CR，返回转换后的文字。
pub(super) fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\r").replace('\n', "\r")
}

/// text 能不能作为一次粘贴发送：只能有普通字符、Tab 和换行。
/// Esc 会让程序提前认为粘贴结束，后面的字就当成按键（比如回车提交）了。
///
/// :param text: 要粘贴的文字
/// :return: 能粘贴为 true
pub fn pastable(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
}

/// 程序没开括号粘贴：不悄悄改成逐行提交，告诉 AI 改为逐行发送
pub(super) fn paste_unsupported() -> AppError {
    AppError::new("mcp.paste_unsupported").with_reason(
        "mcp.program_does_not_accept_paste",
        "这个程序不接受粘贴（没开括号粘贴），请逐行发送",
    )
}

/// 粘贴进输入行的文字，记待提交的行用：换行统一成 \n，粘贴里的换行不算提交。
///
/// :param text: 粘贴的文字
/// :return: 插进输入行的文字
pub(super) fn pasted_line(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 包成一次括号粘贴：里面的换行和 xterm.js 一样换成回车，程序按粘贴插入、不逐行提交。
/// 程序没开括号粘贴时返回 mcp.paste_unsupported。
fn paste_bytes(m: &TerminalManager, id: &str, text: &str) -> Result<Vec<u8>, AppError> {
    if !m.bracketed_paste(id)? {
        return Err(paste_unsupported());
    }
    Ok(format!("\x1b[200~{}\x1b[201~", normalize_text(text)).into_bytes())
}

/// 把 input 变成要写的几段字节：(字节, 末尾的回车是否隔 SUBMIT_GAP 再写)。
/// 图片路径先粘贴进去（程序开了括号粘贴就按粘贴发）；
/// Alt+Enter 发 Esc 加回车时自成一段、回车不隔开（隔开了程序会当成先按 Esc、再按回车提交）。
fn input_parts(
    m: &TerminalManager,
    id: &str,
    input: Input<'_>,
) -> Result<Vec<(Vec<u8>, bool)>, AppError> {
    let mut current = match input.image {
        Some(path) if m.bracketed_paste(id)? => format!("\x1b[200~{path}\x1b[201~").into_bytes(),
        Some(path) => path.as_bytes().to_vec(),
        None => Vec::new(),
    };
    current.extend(match input.text {
        Some(text) if input.paste => paste_bytes(m, id, text)?,
        text => normalize_text(text.unwrap_or_default()).into_bytes(),
    });
    let mut parts = Vec::new();
    for key in input.keys {
        let bytes = key_sequence(m, id, key)?.ok_or_else(wheel_unavailable)?;
        let joined = key == "Alt+Enter" && bytes == b"\x1b\r";
        current.extend(bytes);
        if joined {
            parts.push((std::mem::take(&mut current), false));
        }
    }
    if !current.is_empty() {
        parts.push((current, true));
    }
    Ok(parts)
}

/// 一次发送要写进终端的内容，按 image、text、keys 的顺序
#[derive(Debug, Clone, Copy, Default)]
pub struct Input<'a> {
    /// 已经传到终端所在机器的图片路径：粘贴进去（Claude Code 显示为 [Image #1]）
    pub image: Option<&'a str>,
    pub text: Option<&'a str>,
    /// text 作为一次括号粘贴发送（换行保留、不逐行提交）
    pub paste: bool,
    pub keys: &'a [String],
}

/// 将 text 和 keys 按自动化来源放入 id 的输入队列；返回值只说明已入队，不表示已执行。
/// 密码提示拒绝非空文字；用户接管返回 terminal.automation_rejected，未知按键返回 mcp.invalid_argument。
pub fn terminal_send(
    m: &TerminalManager,
    id: &str,
    text: Option<&str>,
    keys: &[String],
) -> Result<SendOutcome, AppError> {
    send_at_generation(
        m,
        id,
        Input {
            image: None,
            text,
            paste: false,
            keys,
        },
        None,
    )
}
/// 按 input 写进 id 的自动化输入队列；generation 给出时用户输入代数变了就拒绝（见 write_automation_parts）。
/// 返回入队前的输出游标。下面这些情况什么都不写、返回错误：粘贴时程序没开括号粘贴（mcp.paste_unsupported），
/// 滚轮键时不在全屏程序里（mcp.wheel_unavailable），不认识的按键（mcp.invalid_argument）。
pub(super) fn send_at_generation(
    m: &TerminalManager,
    id: &str,
    input: Input<'_>,
    generation: Option<u64>,
) -> Result<SendOutcome, AppError> {
    let parts = input_parts(m, id, input)?;
    if (input.image.is_some() || input.text.is_some_and(|v| !v.is_empty()))
        && looks_like_password_prompt(&m.screen_text(id)?)
    {
        return Err(AppError::new("mcp.password_prompt").with_reason(
            "mcp.terminal_is_waiting_for_a_password_enter_it_in_pierlink",
            "终端正在等待密码，请用户在 Pierlink 里输入",
        ));
    }
    let cursor = m.output_position(id)?;
    m.write_automation_parts(id, &parts, generation)?;
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
    // 等待开始的位置之后，程序响铃或发通知（Codex、Claude Code 等人回答时会发）就立即返回
    let begin = result.cursor;
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
        if let Ok(Some((_, text))) = m.notice_after(id, begin) {
            result.notification = Some(text);
            return Ok(result);
        }
        // 还有输入没写出（例如隔 SUBMIT_GAP 才写的回车）时不算安静：命令还没提交
        if m.input_pending(id) {
            last_output = Instant::now();
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
    use super::{Typed, submitted_lines};

    fn keys(k: &[&str]) -> Vec<String> {
        k.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn 换行统一为回车且提交内容一致() {
        let normalized = super::normalize_text("echo a\r\necho b\necho c\r");
        assert_eq!(normalized, "echo a\recho b\recho c\r");
        assert_eq!(
            submitted_lines("", false, Some(&normalized), &[]).lines,
            ["echo a", "echo b", "echo c"]
        );
        assert_eq!(super::key_bytes("Delete").unwrap(), b"\x1b[3~");
        assert_eq!(super::key_bytes("Shift+Tab").unwrap(), b"\x1b[Z");
        assert_eq!(super::key_bytes("Shift+Left").unwrap(), b"\x1b[1;2D");
        assert_eq!(super::key_bytes("Shift+Up").unwrap(), b"\x1b[1;2A");
    }

    #[test]
    fn 自动化输入按整行判断提交_文字和回车可以分开发() {
        // 只发文字不回车：没有提交，整行留着
        let Typed { lines, rest, .. } = submitted_lines("", false, Some("rm -rf /tmp/x"), &[]);
        assert!(lines.is_empty());
        assert_eq!(rest, "rm -rf /tmp/x");
        // 之后单独发回车（Enter、Ctrl+M、Ctrl+J 都算）
        for enter in ["Enter", "Ctrl+M", "Ctrl+J"] {
            let Typed { lines, rest, .. } =
                submitted_lines("rm -rf /tmp/x", false, None, &keys(&[enter]));
            assert_eq!(lines, ["rm -rf /tmp/x"]);
            assert_eq!(rest, "");
        }
        // 文字里带换行：每行各算一次，最后没换行的留着
        let Typed { lines, rest, .. } =
            submitted_lines("sudo ", false, Some("reboot\nls\r\necho x"), &[]);
        assert_eq!(lines, ["sudo reboot", "ls"]);
        assert_eq!(rest, "echo x");
        // Ctrl+C、Ctrl+U 清空，退格删一个字，Ctrl+W 删一个词
        assert_eq!(
            submitted_lines("rm -rf /", false, None, &keys(&["Ctrl+C", "Enter"])).lines,
            Vec::<String>::new()
        );
        assert_eq!(
            submitted_lines("", false, Some("rm -rf /x\x15ls"), &keys(&["Enter"])).lines,
            ["ls"]
        );
        assert_eq!(
            submitted_lines("", false, Some("lss\x7f"), &keys(&["Enter"])).lines,
            ["ls"]
        );
        assert_eq!(
            submitted_lines("ls /tmp/x  ", false, None, &keys(&["Ctrl+W", "Enter"])).lines,
            ["ls "]
        );
        assert_eq!(
            submitted_lines("", false, Some("ls /tmp/x\x17"), &keys(&["Enter"])).lines,
            ["ls "]
        );
    }

    #[test]
    fn 移动光标的键不清空这一行_先按_end_再回车照样要判断() {
        for key in [
            "End", "Home", "Left", "Right", "Esc", "Tab", "Ctrl+A", "Ctrl+E",
        ] {
            assert_eq!(
                submitted_lines("rm -rf /x", false, None, &keys(&[key, "Enter"])).lines,
                ["rm -rf /x"],
                "{key}"
            );
        }
        // 文字里的控制序列：左（ESC [ D）、End（ESC [ F、ESC O F）、Delete（ESC [ 3~）保留
        for sequence in ["\x1b[D", "\x1b[F", "\x1bOF", "\x1b[3~"] {
            assert_eq!(
                submitted_lines("", false, Some(&format!("rm -rf /x{sequence}\r")), &[]).lines,
                ["rm -rf /x"],
                "{sequence:?}"
            );
        }
    }

    #[test]
    fn 粘贴只收普通字符tab和换行_换行不算提交() {
        use super::{pastable, pasted_line};
        assert!(pastable("第一行\n\tcd x\r\nls\r"));
        for text in ["a\x1b[201~\rrm -rf /", "a\x03", "a\x7f", "\u{9b}201~"] {
            assert!(!pastable(text), "{text:?}");
        }
        assert_eq!(pasted_line("a\r\nb\rc\nd"), "a\nb\nc\nd");
        // 粘贴进来的多行和之前没提交的部分一起，回车时整段算一次提交
        let pending = format!("git status; {}", pasted_line("rm -rf /tmp/x\nls"));
        let Typed { lines, rest, .. } = submitted_lines(&pending, false, None, &keys(&["Enter"]));
        assert_eq!(lines, ["git status; rm -rf /tmp/x\nls"]);
        assert!(crate::mcp::safety::is_dangerous(&lines[0]));
        assert_eq!(rest, "");
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
                submitted_lines("rm -rf /x", false, None, &keys(&[key])).rest,
                "",
                "{key}"
            );
        }
        for sequence in ["\x1b[A", "\x1bOB", "\x1b[5~", "\x10"] {
            assert_eq!(
                submitted_lines("", false, Some(&format!("rm -rf /x{sequence}")), &[]).rest,
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
    fn 程序发通知时等待立即返回() {
        let m = TerminalManager::new();
        let id = shell(&m);
        let sent = terminal_send(
            &m,
            &id,
            Some("sleep 1; printf '\\033]9;Question: %s\\007' pl-q; sleep 30\n"),
            &[],
        )
        .unwrap();
        let started = Instant::now();
        let waited = terminal_wait(
            &m,
            &id,
            Some(&Regex::new("never-matches").unwrap()),
            None,
            Some(sent.cursor),
            Duration::from_secs(20),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(!waited.timed_out && !waited.matched, "{waited:?}");
        assert_eq!(waited.notification.as_deref(), Some("Question: pl-q"));
        assert!(started.elapsed() < Duration::from_secs(10));
        m.close(&id).unwrap();
    }
    /// 原样显示收到的每个字节的终端（cat -v：Esc 显示为 ^[，回车显示为 ^M）；bracketed 时程序先开括号粘贴
    fn raw_echo(m: &TerminalManager, bracketed: bool) -> String {
        let setup = if bracketed {
            "printf '\\033[?2004h'; "
        } else {
            ""
        };
        let id = m
            .open(
                TerminalSpec {
                    kind: TerminalKind::Local {
                        program: "/bin/sh".into(),
                        args: vec![
                            "-c".into(),
                            format!("{setup}stty raw -echo; printf READY; exec cat -v"),
                        ],
                        cwd: None,
                        env: vec![],
                    },
                    cols: 100,
                    rows: 24,
                },
                Arc::new(|_| {}),
            )
            .unwrap();
        wait(m, &id, "READY", 0);
        id
    }
    #[test]
    fn 粘贴按括号粘贴整段发出_换行写成回车_之后的回车隔开再写() {
        let m = TerminalManager::new();
        let id = raw_echo(&m, true);
        assert!(m.bracketed_paste(&id).unwrap());
        let started = Instant::now();
        let input = Input {
            image: None,
            text: Some("echo a\nls\r\n"),
            paste: true,
            keys: &["Enter".into()],
        };
        let sent = send_at_generation(&m, &id, input, None).unwrap();
        let echoed = wait(&m, &id, r"\^\[\[201~\^M", sent.cursor);
        assert!(
            echoed.text.ends_with("^[[200~echo a^Mls^M^[[201~^M"),
            "{echoed:?}"
        );
        // 粘贴之后的回车照样隔 SUBMIT_GAP 再写
        assert!(started.elapsed() >= crate::terminal::SUBMIT_GAP);
        m.close(&id).unwrap();
    }
    #[test]
    fn 程序没开括号粘贴时报错且什么都不写() {
        let m = TerminalManager::new();
        let id = raw_echo(&m, false);
        let before = m.output_position(&id).unwrap();
        let input = Input {
            image: None,
            text: Some("rm -rf /tmp/x\nls"),
            paste: true,
            keys: &["Enter".into()],
        };
        let error = send_at_generation(&m, &id, input, None).unwrap_err();
        assert_eq!(error.code, "mcp.paste_unsupported");
        assert!(!m.input_pending(&id));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(m.output_position(&id).unwrap(), before);
        m.close(&id).unwrap();
    }
    #[test]
    fn 回车还没写出时安静等待不返回() {
        let m = TerminalManager::new();
        let id = shell(&m);
        let started = Instant::now();
        let sent = terminal_send(&m, &id, Some("true\n"), &[]).unwrap();
        let quiet = terminal_wait(
            &m,
            &id,
            None,
            Some(Duration::from_millis(100)),
            Some(sent.cursor),
            Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(!quiet.timed_out);
        assert!(started.elapsed() >= crate::terminal::SUBMIT_GAP);
        assert!(!m.input_pending(&id));
        m.close(&id).unwrap();
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
        // 回车在文字写出 SUBMIT_GAP 后才写，等 stty 生效再发下一条
        std::thread::sleep(crate::terminal::SUBMIT_GAP + Duration::from_millis(300));
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
    fn 管理器静止后消费残缺转义且后续字节从原游标读取() {
        for (fragment, tail) in [
            (b"\x1b".as_slice(), b"[31mred".as_slice()),
            (b"\x1b[31;", b"1mred"),
            (b"\x1b( ", b"Bred"),
            (b"\x1bO", b"Ared"),
            (b"\x1bN", b"Ared"),
        ] {
            let (manager, id) = manager();
            let now = Instant::now();
            let data = [b"$ ", fragment].concat();
            manager.feed_output_at(&id, &data, now).unwrap();
            let first = read_chunk_at(
                &manager,
                &id,
                Some(0),
                None,
                now + Duration::from_millis(999),
            )
            .unwrap()
            .0;
            assert_eq!(first.text, "$ ");
            assert_eq!(first.cursor, 2);
            assert!(first.more);
            let later = now + Duration::from_secs(1);
            let ready = read_chunk_at(&manager, &id, Some(first.cursor), None, later)
                .unwrap()
                .0;
            assert_eq!(ready.text, "");
            assert_eq!(ready.cursor, data.len() as u64);
            assert!(!ready.more);
            manager.feed_output_at(&id, tail, later).unwrap();
            let next = read_chunk_at(&manager, &id, Some(ready.cursor), None, later)
                .unwrap()
                .0;
            assert_eq!(next.text, String::from_utf8_lossy(tail));
            assert_eq!(next.cursor, (data.len() + tail.len()) as u64);
            assert!(!next.more && !next.lost);
            manager.close(&id).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn 管理器静止后替换半个中文且等待命中与后续续读不丢字节() {
        let bytes = "中".as_bytes();
        let pattern = Regex::new("�").unwrap();
        for split in 1..bytes.len() {
            let (manager, id) = manager();
            let now = Instant::now();
            let data = [b"$ ", &bytes[..split]].concat();
            manager.feed_output_at(&id, &data, now).unwrap();
            let first = read_chunk_at(
                &manager,
                &id,
                Some(0),
                None,
                now + Duration::from_millis(999),
            )
            .unwrap()
            .0;
            assert_eq!(first.text, "$ ");
            assert_eq!(first.cursor, 2);
            assert!(first.more);
            let later = now + Duration::from_secs(1);
            let ready = read_chunk_at(&manager, &id, Some(first.cursor), None, later)
                .unwrap()
                .0;
            assert_eq!(ready.text, "�");
            assert_eq!(ready.cursor, data.len() as u64);
            assert!(!ready.more);
            let waited = wait_with_clock(
                &manager,
                &id,
                Some(&pattern),
                None,
                Some(first.cursor),
                Duration::ZERO,
                || false,
                || later,
            )
            .unwrap();
            assert!(waited.matched && !waited.timed_out);
            assert_eq!(waited.text, "�");
            assert_eq!(waited.cursor, ready.cursor);
            let tail = [&bytes[split..], b"red"].concat();
            manager.feed_output_at(&id, &tail, later).unwrap();
            let next = read_chunk_at(&manager, &id, Some(ready.cursor), None, later)
                .unwrap()
                .0;
            assert_eq!(next.text, String::from_utf8_lossy(&tail));
            assert_eq!(next.cursor, (data.len() + tail.len()) as u64);
            assert!(!next.more && !next.lost);
            manager.close(&id).unwrap();
        }
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

#[cfg(test)]
mod extended_key_tests {
    use super::*;
    #[test]
    fn 扩展按键编码与小写控制键提交保持一致() {
        for (key, bytes) in [
            ("F1", "\x1bOP"),
            ("F2", "\x1bOQ"),
            ("F3", "\x1bOR"),
            ("F4", "\x1bOS"),
            ("F5", "\x1b[15~"),
            ("F6", "\x1b[17~"),
            ("F7", "\x1b[18~"),
            ("F8", "\x1b[19~"),
            ("F9", "\x1b[20~"),
            ("F10", "\x1b[21~"),
            ("F11", "\x1b[23~"),
            ("F12", "\x1b[24~"),
            ("Insert", "\x1b[2~"),
            ("PageUp", "\x1b[5~"),
            ("PageDown", "\x1b[6~"),
            ("Ctrl+Space", "\0"),
        ] {
            assert_eq!(key_bytes(key).unwrap(), bytes.as_bytes());
        }
        for letter in 'a'..='z' {
            assert_eq!(
                key_bytes(&format!("Ctrl+{letter}")).unwrap(),
                vec![letter as u8 - b'a' + 1]
            );
            for letter in [letter, letter.to_ascii_uppercase()] {
                assert_eq!(
                    key_bytes(&format!("Alt+{letter}")).unwrap(),
                    vec![0x1b, letter as u8]
                );
            }
        }
        for key in ["F13", "Alt+1", "Alt+中文", "Ctrl+aa"] {
            assert!(key_bytes(key).is_err());
        }
        assert_eq!(
            submitted_lines("git clean -fd", false, None, &["Ctrl+m".into()]).lines,
            ["git clean -fd"]
        );
        assert!(
            submitted_lines("pending", false, None, &["Ctrl+u".into(), "Enter".into()])
                .lines
                .is_empty()
        );
        assert!(submits_recalled_history(
            None,
            &["Ctrl+p".into(), "Ctrl+j".into()]
        ));
    }
}

#[cfg(test)]
mod newline_key_tests {
    use super::*;

    #[test]
    fn 换行键按本机_shell_kitty_协议和全屏决定_和界面规则一样() {
        use LocalShell::{Cmd, Other, PowerShell, Wsl};
        for shift in [true, false] {
            let native = if shift {
                WIN32_SHIFT_ENTER
            } else {
                WIN32_CTRL_ENTER
            };
            // 本机 PowerShell、命令提示符：原样交给 Windows 控制台，全屏程序里也一样
            for shell in [PowerShell, Cmd] {
                for alternate in [false, true] {
                    assert_eq!(newline_bytes(Some(shell), false, alternate, shift), native);
                }
                // 程序自己开了 kitty 协议时照协议发
                assert_eq!(newline_bytes(Some(shell), true, true, shift), b"\x1b[13;2u");
            }
            // Git Bash 里的全屏程序多半是 Windows 程序；它的提示符是 Unix 的
            assert_eq!(newline_bytes(Some(Other), false, true, shift), native);
            assert_eq!(newline_bytes(Some(Other), false, false, shift), b"\x16\n");
            // 服务器上、WSL 里：kitty 协议、全屏程序的换行符、shell 的 Ctrl+V 换行
            for shell in [None, Some(Wsl)] {
                assert_eq!(newline_bytes(shell, true, false, shift), b"\x1b[13;2u");
                assert_eq!(newline_bytes(shell, false, true, shift), b"\n");
                assert_eq!(newline_bytes(shell, false, false, shift), b"\x16\n");
            }
        }
        for (program, shell) in [
            (
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                PowerShell,
            ),
            (r"C:\Program Files\PowerShell\7\PWSH.EXE", PowerShell),
            (r"C:\Windows\System32\cmd.exe", Cmd),
            (r"C:\Windows\System32\wsl.exe", Wsl),
            (r"C:\Program Files\Git\bin\bash.exe", Other),
            ("/bin/sh", Other),
        ] {
            // Linux 上的 Path 不认反斜杠，取最后一段文件名
            let name = program.rsplit(['\\', '/']).next().unwrap();
            assert_eq!(local_shell(Path::new(name)), shell, "{program}");
        }
    }

    #[test]
    fn 新增的固定按键() {
        for (key, bytes) in [
            ("Ctrl+Left", "\x1b[1;5D"),
            ("Ctrl+Right", "\x1b[1;5C"),
            ("Ctrl+Up", "\x1b[1;5A"),
            ("Ctrl+Down", "\x1b[1;5B"),
            ("Alt+Left", "\x1b[1;3D"),
            ("Alt+Right", "\x1b[1;3C"),
            ("Alt+Up", "\x1b[1;3A"),
            ("Alt+Down", "\x1b[1;3B"),
            ("Ctrl+Home", "\x1b[1;5H"),
            ("Ctrl+End", "\x1b[1;5F"),
            ("Shift+Home", "\x1b[1;2H"),
            ("Shift+End", "\x1b[1;2F"),
            ("Ctrl+Backspace", "\x08"),
            ("Alt+Backspace", "\x1b\x7f"),
            ("Ctrl+Delete", "\x1b[3;5~"),
            ("Ctrl+_", "\x1f"),
            ("Ctrl+/", "\x1f"),
            ("Ctrl+]", "\x1d"),
            ("Ctrl+\\", "\x1c"),
            ("Ctrl+^", "\x1e"),
        ] {
            assert_eq!(key_bytes(key).unwrap(), bytes.as_bytes(), "{key}");
        }
        // 要看终端状态的键不在固定表里
        for key in [
            "Shift+Enter",
            "Ctrl+Enter",
            "Alt+Enter",
            "WheelUp",
            "WheelDown",
        ] {
            assert!(key_bytes(key).is_err(), "{key}");
        }
        // schema 的可选值和实现一一对应
        let tool = super::super::schema::TOOLS
            .iter()
            .find(|tool| tool.name == "terminal_send")
            .unwrap();
        for key in tool.input_schema["properties"]["keys"]["items"]["enum"]
            .as_array()
            .unwrap()
        {
            let key = key.as_str().unwrap();
            let context = matches!(
                key,
                "Shift+Enter" | "Ctrl+Enter" | "Alt+Enter" | "WheelUp" | "WheelDown"
            );
            assert!(context || key_bytes(key).is_ok(), "{key}");
        }
    }

    #[test]
    fn 换行键不提交时这一行按提交判断一次但不清空_回车时整段再判断() {
        let keys = |k: &[&str]| k.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for key in ["Shift+Enter", "Ctrl+Enter", "Alt+Enter"] {
            let typed = submitted_lines_with("rm -rf /tmp/x", false, None, &keys(&[key]), false);
            assert_eq!(typed.lines, ["rm -rf /tmp/x"], "{key}");
            assert_eq!(typed.rest, "rm -rf /tmp/x\n", "{key}");
            let typed =
                submitted_lines_with(&typed.rest, false, Some("ls"), &keys(&["Enter"]), false);
            assert_eq!(typed.lines, ["rm -rf /tmp/x\nls"], "{key}");
            assert!(typed.rest.is_empty());
            // 命令提示符里换行键真的提交：清空
            let typed = submitted_lines_with("rm -rf /tmp/x", false, None, &keys(&[key]), true);
            assert_eq!(typed.lines, ["rm -rf /tmp/x"], "{key}");
            assert!(typed.rest.is_empty(), "{key}");
        }
        // 一次发完：每段判断一次，回车时整段
        let typed = submitted_lines_with(
            "for f in a b; do",
            false,
            None,
            &keys(&["Shift+Enter"]),
            false,
        );
        let typed = submitted_lines_with(
            &typed.rest,
            false,
            Some("rm -rf $f"),
            &keys(&["Shift+Enter"]),
            false,
        );
        assert_eq!(typed.lines, ["for f in a b; do\nrm -rf $f"]);
        let typed =
            submitted_lines_with(&typed.rest, false, Some("done"), &keys(&["Enter"]), false);
        assert_eq!(typed.lines, ["for f in a b; do\nrm -rf $f\ndone"]);
    }

    #[test]
    fn ctrl_x_组合键和_ctrl_o_之后内容未知_搜历史清空() {
        let keys = |k: &[&str]| k.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // 清掉后 Ctrl+X Ctrl+U（撤销）再回车：按内容未知确认
        let typed = submitted_lines(
            "rm -rf /",
            false,
            None,
            &keys(&["Ctrl+U", "Ctrl+X", "Ctrl+U", "Enter"]),
        );
        assert!(typed.lines.is_empty() && typed.unknown);
        let typed = submitted_lines("", false, Some("rm -rf /\x15\x18\x15"), &keys(&["Enter"]));
        assert!(typed.lines.is_empty() && typed.unknown);
        // Ctrl+O 执行这一行，剩下的是取出来的历史
        let typed = submitted_lines("ls", false, None, &keys(&["Ctrl+O"]));
        assert_eq!(typed.lines, ["ls"]);
        assert!(typed.rest_unknown);
        // Ctrl+R、Ctrl+S 搜历史：这一行换掉了，回车提交的内容不知道
        for search in ["Ctrl+R", "Ctrl+S"] {
            let typed = submitted_lines("echo hi", false, None, &keys(&[search, "Enter"]));
            assert!(typed.lines.is_empty(), "{search}");
            assert!(
                submits_recalled_history(None, &keys(&[search, "Enter"])),
                "{search}"
            );
        }
        assert!(submits_recalled_history(Some("\x12rm\r"), &[]));
    }

    #[test]
    fn 换行键算提交_撤销和粘回让这一行内容未知_删词删字() {
        let keys = |k: &[&str]| k.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for key in ["Shift+Enter", "Ctrl+Enter", "Alt+Enter"] {
            let typed = submitted_lines("rm -rf /tmp/x", false, None, &keys(&[key]));
            assert_eq!(typed.lines, ["rm -rf /tmp/x"], "{key}");
            assert!(!typed.unknown);
            assert!(submits_recalled_history(None, &keys(&["Up", key])), "{key}");
        }
        // Ctrl+U 清空后撤销能把删掉的文字找回来：之后的回车按“内容未知”确认
        for undo in ["Ctrl+_", "Ctrl+/", "Ctrl+Y", "Ctrl+y"] {
            let typed = submitted_lines("rm -rf /", false, None, &keys(&["Ctrl+U", undo, "Enter"]));
            assert!(typed.lines.is_empty() && typed.unknown, "{undo}");
            assert!(!typed.rest_unknown);
            // 撤销之后还没回车：剩下的部分内容未知，留给之后的回车
            let typed = submitted_lines("rm -rf /", false, None, &keys(&["Ctrl+U", undo]));
            assert!(!typed.unknown && typed.rest_unknown, "{undo}");
        }
        let typed = submitted_lines("", false, Some("rm -rf /\x15\x1f\r"), &[]);
        assert!(typed.unknown);
        // 回车之后才撤销：提交的那行是知道的，剩下的不知道
        let typed = submitted_lines("ls", false, None, &keys(&["Enter", "Ctrl+Y"]));
        assert_eq!(typed.lines, ["ls"]);
        assert!(!typed.unknown && typed.rest_unknown);
        // 用户打过字：第一次提交内容未知，之后就知道了
        let typed = submitted_lines("", true, Some("ls\rpwd"), &[]);
        assert!(typed.unknown && !typed.rest_unknown);
        assert_eq!(typed.rest, "pwd");
        assert_eq!(
            submitted_lines("ls /tmp/x", false, None, &keys(&["Alt+Backspace", "Enter"])).lines,
            ["ls "]
        );
        assert_eq!(
            submitted_lines("lss", false, None, &keys(&["Ctrl+Backspace", "Enter"])).lines,
            ["ls"]
        );
        // 滚轮在没接管鼠标的全屏程序里是方向键，可能翻出历史
        for key in ["WheelUp", "WheelDown"] {
            assert_eq!(
                submitted_lines("rm -rf /x", false, None, &keys(&[key])).rest,
                ""
            );
            assert!(submits_recalled_history(None, &keys(&[key, "Enter"])));
        }
        // 光标移动和选择不改这一行
        for key in [
            "Ctrl+Left",
            "Alt+Right",
            "Ctrl+Up",
            "Alt+Down",
            "Ctrl+Home",
            "Shift+End",
            "Ctrl+Delete",
            "Ctrl+]",
        ] {
            assert_eq!(
                submitted_lines("rm -rf /x", false, None, &keys(&[key, "Enter"])).lines,
                ["rm -rf /x"],
                "{key}"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod key_state_tests {
    use super::*;
    use crate::terminal::{TerminalKind, TerminalSpec};
    use std::sync::Arc;

    fn terminal(m: &TerminalManager) -> String {
        m.open(
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
            Arc::new(|_| {}),
        )
        .unwrap()
    }
    fn feed(m: &TerminalManager, id: &str, bytes: &[u8]) {
        m.feed_output_at(id, bytes, Instant::now()).unwrap();
    }
    fn key(m: &TerminalManager, id: &str, key: &str) -> Option<Vec<u8>> {
        key_sequence(m, id, key).unwrap()
    }

    #[test]
    fn 换行键_alt_enter_和滚轮跟着程序开的模式变() {
        let m = TerminalManager::new();
        let id = terminal(&m);
        // 普通命令行：shell 的 Ctrl+V 换行；Alt+Enter 是 Esc 加回车；滚轮不发
        assert_eq!(key(&m, &id, "Shift+Enter").unwrap(), b"\x16\n");
        assert_eq!(key(&m, &id, "Ctrl+Enter").unwrap(), b"\x16\n");
        assert_eq!(key(&m, &id, "Alt+Enter").unwrap(), b"\x1b\r");
        assert_eq!(key(&m, &id, "WheelUp"), None);
        // 程序开了 kitty 协议
        feed(&m, &id, b"\x1b[>1u");
        assert_eq!(key(&m, &id, "Shift+Enter").unwrap(), b"\x1b[13;2u");
        assert_eq!(key(&m, &id, "Ctrl+Enter").unwrap(), b"\x1b[13;2u");
        assert_eq!(key(&m, &id, "Alt+Enter").unwrap(), b"\x1b[13;3u");
        // 全屏程序（备用屏有自己的一套 kitty 状态）：换行符；滚轮发方向键或滚轮事件
        feed(&m, &id, b"\x1b[?1049h");
        assert_eq!(key(&m, &id, "Shift+Enter").unwrap(), b"\n");
        assert_eq!(key(&m, &id, "WheelUp").unwrap(), b"\x1b[A");
        feed(&m, &id, b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(key(&m, &id, "WheelDown").unwrap(), b"\x1b[<65;40;8M");
        // 固定按键照常
        assert_eq!(key(&m, &id, "Ctrl+Left").unwrap(), b"\x1b[1;5D");
        assert_eq!(
            key_sequence(&m, &id, "Ctrl+Shift+Q").unwrap_err().code,
            "mcp.invalid_argument"
        );
        m.close(&id).unwrap();
    }

    #[test]
    fn alt_enter_的_esc_和回车单独一段连着写_滚轮不在全屏时什么都不写() {
        let m = TerminalManager::new();
        let id = terminal(&m);
        let keys: Vec<String> = ["Alt+Enter", "Ctrl+Left", "Enter"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let input = Input {
            image: None,
            text: Some("hi"),
            paste: false,
            keys: &keys,
        };
        assert_eq!(
            input_parts(&m, &id, input).unwrap(),
            [
                (b"hi\x1b\r".to_vec(), false),
                (b"\x1b[1;5D\r".to_vec(), true)
            ]
        );
        // 开了 kitty 协议时 Alt+Enter 不带回车，不用单独一段
        feed(&m, &id, b"\x1b[>1u");
        assert_eq!(
            input_parts(&m, &id, input).unwrap(),
            [(b"hi\x1b[13;3u\x1b[1;5D\r".to_vec(), true)]
        );
        let wheel = ["WheelUp".to_owned()];
        let error = send_at_generation(
            &m,
            &id,
            Input {
                image: None,
                text: None,
                paste: false,
                keys: &wheel,
            },
            None,
        )
        .unwrap_err();
        assert_eq!(error.code, "mcp.wheel_unavailable");
        assert!(!m.input_pending(&id));
        m.close(&id).unwrap();
    }
}
