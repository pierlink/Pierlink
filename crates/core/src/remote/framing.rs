//! 使用调用方提供的随机 nonce 隔离远程启动噪声。
use crate::error::AppError;

/// 无参数；返回 16 字节随机数的小写十六进制字符串，每次远程调用都要使用新的 nonce。
pub fn new_nonce() -> String {
    rand::random::<[u8; 16]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn markers(nonce: &str) -> (String, String) {
    // 字节十六进制编码是单射，既隔离换行也防止 nonce 成为 shell 代码。
    let encoded: String = nonce.bytes().map(|b| format!("{b:02x}")).collect();
    (
        format!("__PIERLINK_BEGIN_{encoded}__"),
        format!("__PIERLINK_END_{encoded}__"),
    )
}

/// 包装可信 POSIX 脚本正文（须输出一行 JSON），返回可从 SSH 标准输入发送的脚本。
/// nonce 必须由调用方随机生成且每次不同；正文失败时不输出结束标记并保留退出码。
/// 定界符只能隔离噪声，不能认证恶意服务器。
/// 正文标准输入为 /dev/null；调用方写完脚本后必须关闭 SSH 标准输入。
pub fn wrap_script(body: &str, nonce: &str) -> String {
    let (begin, end) = markers(nonce);
    format!(
        "printf '\\n%s\\n' '{begin}'\n(\n{body}\n) </dev/null\npierlink_status=$?\nif [ \"$pierlink_status\" -ne 0 ]; then exit \"$pierlink_status\"; fi\nprintf '\\n%s\\n' '{end}'\n"
    )
}

/// max_payload_bytes 限制帧内 JSON 的 UTF-8 字节数，超限返回 remote.payload_too_large。
/// 提取匹配 nonce 的唯一完整帧并解析 JSON；忽略帧外噪声，缺失、重复、次序或 JSON 错误分别报错。
pub fn parse_framed(
    stdout: &str,
    nonce: &str,
    max_payload_bytes: usize,
) -> Result<serde_json::Value, AppError> {
    let (begin, end) = markers(nonce);
    let lines: Vec<_> = stdout.lines().collect();
    let starts: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| **l == begin)
        .map(|(i, _)| i)
        .collect();
    let ends: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| **l == end)
        .map(|(i, _)| i)
        .collect();
    if starts.len() > 1 || ends.len() > 1 {
        return Err(AppError::new("remote.frame_duplicate"));
    }
    let (Some(&start), Some(&end)) = (starts.first(), ends.first()) else {
        return Err(AppError::new("remote.frame_missing"));
    };
    if start >= end {
        return Err(AppError::new("remote.frame_order"));
    }
    let payload_lines = &lines[start + 1..end];
    let size = payload_lines
        .iter()
        .try_fold(payload_lines.len().saturating_sub(1), |size, line| {
            size.checked_add(line.len())
        });
    if size.is_none_or(|size| size > max_payload_bytes) {
        return Err(
            AppError::new("remote.payload_too_large").with("max_payload_bytes", max_payload_bytes)
        );
    }
    let payload = payload_lines.join("\n");
    serde_json::from_str(&payload).map_err(|e| {
        AppError::new("remote.invalid_json")
            .with("line", e.line())
            .with("column", e.column())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 随机_nonce_格式唯一且可用于帧解析() {
        let mut values = std::collections::HashSet::new();
        for _ in 0..256 {
            let nonce = new_nonce();
            assert_eq!(nonce.len(), 32);
            assert!(
                nonce
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            );
            let (begin, end) = markers(&nonce);
            assert_eq!(
                parse_framed(&format!("{begin}\ntrue\n{end}"), &nonce, 1024).unwrap(),
                true
            );
            assert!(values.insert(nonce));
        }
    }
    #[test]
    fn s27_载荷字节上限() {
        let (b, e) = markers("n");
        let data = format!("{b}\n\"中\"\n{e}");
        assert_eq!(parse_framed(&data, "n", 5).unwrap(), "中");
        assert_eq!(
            parse_framed(&data, "n", 4).unwrap_err().code,
            "remote.payload_too_large"
        );
        assert_eq!(
            parse_framed(&data, "n", 0).unwrap_err().code,
            "remote.payload_too_large"
        );
    }
    #[test]
    #[cfg(unix)]
    fn s27_正文不能吞掉后续脚本() {
        use std::io::Write;
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-s")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(wrap_script("cat\nprintf true", "n").as_bytes())
            .unwrap();
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert_eq!(
            parse_framed(std::str::from_utf8(&out.stdout).unwrap(), "n", 10).unwrap(),
            true
        );
    }
    #[test]
    fn s11_噪声和伪造定界符() {
        let (b, e) = markers("随机");
        let (fake_b, fake_e) = markers("伪造");
        let data = format!("MOTD\n{fake_b}\nfalse\n{fake_e}\n{b}\n{{\"ok\":true}}\n{e}\n.bashrc\n");
        assert_eq!(
            parse_framed(&data, "随机", 1024).unwrap(),
            serde_json::json!({"ok":true})
        );
        assert!(parse_framed(&data, "另一随机数", 1024).is_err());
    }
    #[test]
    fn s11_明确的协议错误() {
        let (b, e) = markers("n");
        for (text, code) in [
            ("noise".into(), "remote.frame_missing"),
            (format!("{b}\ntrue"), "remote.frame_missing"),
            (format!("{b}\n{b}\ntrue\n{e}"), "remote.frame_duplicate"),
            (format!("{b}\ntrue\n{e}\n{e}"), "remote.frame_duplicate"),
            (format!("{e}\n{b}"), "remote.frame_order"),
            (format!("{b}\nsecret\n{e}"), "remote.invalid_json"),
            (format!("{b}\ntrue false\n{e}"), "remote.invalid_json"),
        ] {
            let err = parse_framed(&text, "n", 1024).unwrap_err();
            assert_eq!(err.code, code);
            assert!(!format!("{err:?}").contains("secret"));
        }
    }
    #[test]
    #[cfg(unix)]
    fn 包装脚本在真实_posix_shell_运行() {
        for nonce in ["normal", "'\n$(id)\0中文", ""] {
            let out = std::process::Command::new("/bin/sh")
                .args(["-c", &wrap_script("printf '%s' '{\"结果\":1}'", nonce)])
                .output()
                .unwrap();
            assert!(out.status.success());
            assert_eq!(
                parse_framed(std::str::from_utf8(&out.stdout).unwrap(), nonce, 1024).unwrap(),
                serde_json::json!({"结果":1})
            );
        }
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &wrap_script("exit 7", "n")])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(7));
        assert_eq!(
            parse_framed(std::str::from_utf8(&out.stdout).unwrap(), "n", 1024)
                .unwrap_err()
                .code,
            "remote.frame_missing"
        );
    }
}
