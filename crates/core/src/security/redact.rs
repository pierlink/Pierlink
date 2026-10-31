//! 在原文上合并所有敏感区间，避免替换顺序导致秘密部分残留。
use regex::Regex;
use std::sync::LazyLock;

static PATTERNS: LazyLock<Result<Vec<Regex>, regex::Error>> = LazyLock::new(|| {
    [
    r"(?i)authorization[ \t]*:[^\r\n]*",
    r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
    r#"(?i)(?:密码|口令|密钥|令牌|API密钥|pwd|[a-z_-]*(?:key|secret|token|pass(?:word|wd|phrase)?))["']?\s*[:=][ \t]*(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\r\n]*)"#,
    r"(?is)<apikey\s*>.*?(?:</apikey\s*>|\z)",
    r"[^\s/:@]+:[^\s/@]+@",
    r"(?i)proxycommand[ \t]*=[^\r\n]*",
].into_iter().map(Regex::new).collect()
});

/// 屏蔽 text 中已知非空秘密、密码、口令、令牌、秘密/API 字段与 Authorization 头与完整或截断的私钥块，返回适合日志的文本。
/// 无引号字段保守屏蔽到行尾；无法确定边界时宁可丢失日志上下文。
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut ranges = Vec::new();
    let mut forms = Vec::new();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        forms.push((*secret).to_owned());
        if let Ok(encoded) = serde_json::to_string(secret) {
            forms.push(encoded[1..encoded.len() - 1].to_owned());
        }
    }
    for secret in &forms {
        // 包括自重叠匹配，例如 aaa 中的 aa。
        for (start, _) in text.char_indices() {
            if text[start..].starts_with(secret.as_str()) {
                ranges.push((start, start + secret.len()));
            }
        }
    }
    let Ok(patterns) = &*PATTERNS else {
        return "***".into();
    };
    for regex in patterns {
        ranges.extend(regex.find_iter(text).map(|m| (m.start(), m.end())));
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    let mut output = String::new();
    let mut cursor = 0;
    for (start, end) in merged {
        output.push_str(&text[cursor..start]);
        output.push_str("***");
        cursor = end;
    }
    output.push_str(&text[cursor..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 补充口令令牌秘密和认证头() {
        for key in ["passphrase", "TOKEN", "Secret"] {
            for text in [
                format!("{key}=sensitive value"),
                format!("\"{key}\":\"sensitive\\\"value\""),
                format!("'{key}':'sensitive\nvalue'"),
            ] {
                let out = redact(&text, &[]);
                assert!(!out.contains("sensitive"));
                assert!(!out.contains("value"));
            }
        }
        for header in [
            "Authorization: Bearer sensitive",
            "authorization:\tBasic sensitive",
            "AUTHORIZATION: custom sensitive",
        ] {
            assert_eq!(
                redact(&format!("前\r\n{header}\r\n后"), &[]),
                "前\r\n***\r\n后"
            );
        }
    }
    #[test]
    fn 扩展字段和转义秘密() {
        for key in [
            "密码",
            "口令",
            "密钥",
            "令牌",
            "API密钥",
            "api_key",
            "secret_key",
            "passwd",
            "pwd",
            "service-auth-token",
        ] {
            assert!(!redact(&format!("{key}=sensitive-value"), &[]).contains("sensitive-value"));
        }
        for text in [
            "<apikey>sensitive-value</apikey>",
            "https://user:sensitive-value@host/path",
            "ProxyCommand=helper --auth sensitive-value",
        ] {
            assert!(!redact(text, &[]).contains("sensitive-value"));
        }
        let secret = "a\"b\\c\n中文";
        let json = serde_json::to_string(secret).unwrap();
        assert_eq!(redact(&json, &[secret]), "\"***\"");
    }
    #[test]
    fn 已知秘密和重叠值() {
        assert_eq!(
            redact("中文秘密 abcdef aaa", &["", "秘密", "abc", "abcdef", "aa"]),
            "中文*** *** ***"
        );
        assert_eq!(redact("普通日志", &[""]), "普通日志");
        assert_eq!(redact("", &[]), "");
    }
    #[test]
    fn 设计310_常见密码与密钥格式() {
        for text in [
            "password=secret",
            "PASSWORD = secret with spaces",
            "apikey=secret",
            "X-API-Key: secret",
            "x-api-key:\tsecret",
            "{\"password\":\"secret words\"}",
            "password='secret\nwords'",
            "apikey=\"secret\\\"words\"",
        ] {
            let out = redact(text, &[]);
            assert!(!out.contains("secret"));
            assert!(!out.contains("words"));
            assert!(out.contains("***"));
        }
    }
    #[test]
    fn 设计310_完整和截断私钥() {
        for kind in ["", "RSA ", "EC ", "OPENSSH ", "ENCRYPTED "] {
            let begin = format!("-----BEGIN {kind}PRIVATE KEY-----");
            let end = format!("-----END {kind}PRIVATE KEY-----");
            let text = format!("前\n{begin}\n敏感\r\n内容\n{end}\n后");
            assert_eq!(redact(&text, &["BEGIN", "敏感"]), "前\n***\n后");
            assert_eq!(redact(&format!("前\n{begin}\n敏感"), &[]), "前\n***");
        }
    }
}
