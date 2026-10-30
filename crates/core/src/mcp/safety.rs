//! 危险命令和密码提示的尽力判断；不能识别历史展开或交互程序中的命令。
use regex::Regex;
use std::sync::LazyLock;

/// sh/bash/zsh 的 -c：可以和其他开关合写（-lc、-ec），前面可以有 -o pipefail、-euo pipefail、--login 这类开关
const SHELL_C: &str =
    r"(?:sh|bash|zsh)(?:\s+(?:-[a-zA-Z]*o\s+[\w-]+|-[a-zA-Z]+|--[\w-]+))*?\s+-[a-zA-Z]*c[a-zA-Z]*";
/// cmd 的 /c 或 /k：前面可以有 /d、/q 这类开关
const CMD_C: &str = r"cmd(?:\.exe)?(?:\s+/\w)*?\s+/[ck]";
/// powershell/pwsh 的 -Command（可缩写到 -c）：前面可以有 -NoProfile、-ExecutionPolicy Bypass 这类开关
const PWSH_C: &str = r#"(?:powershell|pwsh)(?:\.exe)?(?:\s+-\w+(?:\s+[^-\s"']\S*)?)*?\s+-c\w*"#;

/// 危险清单：整套不区分大小写（Windows 命令和参数本来就不区分；Unix 命令大写也照样算）。
/// 命令名必须在命令开头：行首或 ; & | ( 换行之后，可以套 sudo（带选项和选项的值）、上面三种包装、env、nohup 等
static DANGEROUS: LazyLock<Result<Vec<Regex>, regex::Error>> = LazyLock::new(|| {
    let start = format!(
        r"(?:^|[;&|(\r\n])\s*(?:(?:sudo(?:\s+-\S+(?:\s+[^-\s]\S*)?)*|{CMD_C}|{SHELL_C}|{PWSH_C}|nohup|exec|time|env(?:\s+\w+=\S*)*)\s+)*"
    );
    [
        r"rm\s+(?:-[a-zA-Z]*[rRf][a-zA-Z]*|--recursive|--force)\b",
        r"(?:reboot|shutdown|poweroff|halt)\b",
        r"init\s+[06]\b",
        r"mkfs(?:\.\w+)?\b",
        r"dd\b.*\bof=/dev/",
        r"(?:chmod|chown)\s+(?:-[a-zA-Z]*R[a-zA-Z]*|--recursive)\b.*\s/(?:\s|$)",
        r"git\s+push\b.*(?:\s-f\b|--force)",
        r"git\s+reset\s+--hard\b",
        r"kubectl\s+delete\b",
        r"(?:drop\s+(?:table|database|schema)\b|truncate\s+table\b)",
        // PowerShell 的删除和它的别名，-Recurse 可以缩写到 -r
        r"(?:Remove-Item|ri|rm|del|erase|rd|rmdir)\b.*\s-r(?:ecurse|ecurs|ecur|ecu|ec|e)?\b",
        r"(?:rmdir|rd|del|erase)\b.*\s/s\b",
        r"(?:format\s+[a-z]:|diskpart\b|Stop-Computer\b|Restart-Computer\b|Clear-Disk\b|Format-Volume\b|reg\s+delete\b|bcdedit\b|vssadmin\s+delete\b)",
    ]
    .into_iter()
    .map(|pattern| Regex::new(&format!("(?i){start}{pattern}")))
    .chain(
        [r">\s*/dev/(?:sd|nvme|vd|hd)", r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;\s*:"]
            .into_iter()
            .map(Regex::new),
    )
    .collect()
});

/// 引号前面是 sh -c、cmd /c、powershell -Command 这类包装：引号里的才是真正执行的命令
static WRAPPER_END: LazyLock<Result<Regex, regex::Error>> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(?:^|[\s;&|(])(?:{SHELL_C}|{CMD_C}|{PWSH_C})\s*$"
    ))
});

/// 一行命令按引号扫描的结果
struct Scan<'a> {
    /// 引号里的文字换成占位字符：引号里的 ; | 不是命令边界（例如 echo "x; reboot"），占位符也让两侧单词不会拼到一起
    masked: String,
    /// 顶层引号里的内容和它前面的文字：(前面的文字, 引号里的内容)
    segments: Vec<(&'a str, String)>,
    /// 到行尾引号还没闭合
    unterminated: bool,
}

/// 按引号扫描 line。escapes：反斜杠是否转义下一个字符（sh 的规则；PowerShell 和 cmd 里反斜杠不转义）
fn scan(line: &str, escapes: bool) -> Scan<'_> {
    let mut masked = String::with_capacity(line.len());
    let mut segments = Vec::new();
    let mut quote: Option<(char, usize)> = None;
    let mut inner = String::new();
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped {
            masked.push('x');
            if quote.is_some() {
                inner.push(c);
            }
            escaped = false;
            continue;
        }
        match quote {
            None => {
                if escapes && c == '\\' {
                    escaped = true;
                    masked.push('x');
                } else if c == '\'' || c == '"' {
                    quote = Some((c, i));
                    inner.clear();
                    masked.push('x');
                } else {
                    masked.push(c);
                }
            }
            Some((q, start)) => {
                masked.push('x');
                if escapes && c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    segments.push((&line[..start], std::mem::take(&mut inner)));
                    quote = None;
                } else {
                    inner.push(c);
                }
            }
        }
    }
    Scan {
        masked,
        segments,
        unterminated: quote.is_some(),
    }
}

fn dangerous_scan(scan: &Scan, depth: u8) -> bool {
    let direct = match &*DANGEROUS {
        Ok(patterns) => patterns
            .iter()
            .any(|pattern| pattern.is_match(&scan.masked)),
        Err(_) => !scan.masked.is_empty(),
    };
    if direct || depth >= 3 {
        return direct;
    }
    let Ok(wrapper) = &*WRAPPER_END else {
        return false;
    };
    scan.segments
        .iter()
        .any(|(before, inner)| wrapper.is_match(before) && dangerous_at(inner, depth + 1))
}

fn dangerous_at(line: &str, depth: u8) -> bool {
    let line = line.trim();
    let shell = scan(line, true);
    // 按 sh 的规则引号到行尾都没闭合：可能是 PowerShell、cmd 里以 \ 结尾的路径（"D:\work\tmp\"），按反斜杠不转义再看一遍
    dangerous_scan(&shell, depth) || shell.unterminated && dangerous_scan(&scan(line, false), depth)
}

/// 判断 line 中直接输入的命令是否命中危险清单，返回是否需要用户确认。
/// 套在 sh -c、cmd /c、powershell -Command 引号里的命令也会取出来判断（最多三层）。
pub fn is_dangerous(line: &str) -> bool {
    dangerous_at(line, 0)
}

static PASSWORD: LazyLock<Result<Regex, regex::Error>> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^\[sudo\] password for |(?:password|passphrase|passcode|verification code|authenticator code|one-time code|(?:2fa|mfa) code|密码|口令|验证码|令牌)[^\n]{0,40}[:：]\s*$|(?:\b(?:enter|input|type|paste|your|2fa|mfa)\b|请输入)[^\n]{0,30}\b(?:pin|otp|token)\b[^\n]{0,20}[:：]\s*$|^(?:pin|otp|token)\s*[:：]\s*$|^Enter passphrase)",
    )
});
static ECHO: LazyLock<Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"(?:^|[$#>]\s+)echo\s"));

/// 清理 tail 的控制序列，仅检查最后一行非空文字，返回是否像密码或验证码提示。
pub fn looks_like_password_prompt(tail: &str) -> bool {
    let plain = super::text::strip_ansi(tail.as_bytes());
    let last = plain
        .split(['\r', '\n'])
        .map(str::trim_end)
        .rfind(|s| !s.is_empty())
        .unwrap_or_default();
    if ECHO.as_ref().is_ok_and(|echo| echo.is_match(last)) {
        return false;
    }
    PASSWORD
        .as_ref()
        .map_or(!last.is_empty(), |pattern| pattern.is_match(last))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 共用安全判断向量() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("safety_vectors.json")).unwrap();
        for vector in vectors["dangerous"].as_array().unwrap() {
            let line = vector["line"].as_str().unwrap();
            assert_eq!(
                is_dangerous(line),
                vector["expected"].as_bool().unwrap(),
                "{line}"
            );
        }
        for vector in vectors["password_prompt"].as_array().unwrap() {
            let tail = vector["tail"].as_str().unwrap();
            assert_eq!(
                looks_like_password_prompt(tail),
                vector["expected"].as_bool().unwrap(),
                "{tail}"
            );
        }
    }
}
