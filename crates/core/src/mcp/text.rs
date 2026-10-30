//! 工具输出的控制序列清理和头尾截断。

/// 清理 bytes 中的 CSI、OSC 和其他 ESC 序列，宽松解码 UTF-8，并将 CRLF 转成 LF。
pub fn strip_ansi(bytes: &[u8]) -> String {
    let mut plain = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            plain.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&kind) = bytes.get(i) else { break };
        i += 1;
        match kind {
            b'[' => {
                while let Some(&byte) = bytes.get(i) {
                    i += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            b']' | b'P' | b'X' | b'^' | b'_' => {
                while i < bytes.len() {
                    if kind == b']' && bytes[i] == 7 {
                        i += 1;
                        break;
                    }
                    if bytes[i..].starts_with(b"\x1b\\") {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'O' | b'N' => {
                if i < bytes.len() {
                    i += 1;
                }
            }
            0x20..=0x2f => {
                while bytes.get(i).is_some_and(|b| (0x20..=0x2f).contains(b)) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
            _ => {}
        }
    }
    String::from_utf8_lossy(&plain).replace("\r\n", "\n")
}

/// 返回首个完整 UTF-8 字符或控制序列的字节长度；末尾未接收完整时返回 None。
pub(super) fn token_len(bytes: &[u8]) -> Option<usize> {
    let first = *bytes.first()?;
    if first != 0x1b {
        if bytes.starts_with(b"\r\n") {
            return Some(2);
        }
        let width = match first {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 1,
        };
        if bytes.len() < width
            && std::str::from_utf8(bytes)
                .err()
                .is_some_and(|e| e.error_len().is_none())
        {
            return None;
        }
        return Some(
            if bytes.len() >= width && std::str::from_utf8(&bytes[..width]).is_ok() {
                width
            } else {
                1
            },
        );
    }
    let kind = *bytes.get(1)?;
    match kind {
        b'[' => bytes[2..]
            .iter()
            .position(|b| (0x40..=0x7e).contains(b))
            .map(|end| end + 3),
        b']' | b'P' | b'X' | b'^' | b'_' => {
            let mut end = 2;
            while end < bytes.len() {
                if kind == b']' && bytes[end] == 7 {
                    return Some(end + 1);
                }
                if bytes[end..].starts_with(b"\x1b\\") {
                    return Some(end + 2);
                }
                end += 1;
            }
            None
        }
        b'O' | b'N' => (bytes.len() >= 3).then_some(3),
        0x20..=0x2f => bytes[2..]
            .iter()
            .position(|b| !(0x20..=0x2f).contains(b))
            .map(|end| end + 3),
        _ => Some(2),
    }
}

/// 把请求的字节起点向后移到完整字符或控制序列的边界；缓冲开头残留的续字节跳过。
pub(super) fn aligned_start(bytes: &[u8], requested: usize) -> usize {
    let mut offset = bytes.iter().take_while(|b| **b & 0xc0 == 0x80).count();
    while offset < requested.min(bytes.len()) {
        match token_len(&bytes[offset..]) {
            Some(size) => offset += size,
            None => return bytes.len(),
        }
    }
    offset
}

/// bytes 超过 limit 时保留头尾各半的字节预算，另插入省略提示；返回结果与是否截断。
/// UTF-8 多字节字符不会被切开；提示中的字节数包含边界调整省略的部分。
pub fn truncate_head_tail(bytes: &[u8], limit: usize) -> (Vec<u8>, bool) {
    if bytes.len() <= limit {
        return (bytes.to_vec(), false);
    }
    let mut head = limit / 2;
    let mut tail = bytes.len() - (limit - head);
    while head > 0 && bytes[head] & 0xc0 == 0x80 {
        head -= 1;
    }
    while tail < bytes.len() && bytes[tail] & 0xc0 == 0x80 {
        tail += 1;
    }
    let mut output = bytes[..head].to_vec();
    output.extend_from_slice(format!("\n…（省略 {} 字节）…\n", tail - head).as_bytes());
    output.extend_from_slice(&bytes[tail..]);
    (output, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 清理控制序列并宽松解码() {
        assert_eq!(
            strip_ansi(b"\x1b[31mred\x1b[0m\r\n\x1b]0;title\x07ok\x1b]x\x1b\\\x1b(B\x1b7\xff"),
            "red\nok�"
        );
        assert_eq!(strip_ansi(b"a\x1bPpayload\x1b\\b\x1b["), "ab");
        assert_eq!(strip_ansi(b"a\rb\x1b"), "a\rb");
        assert_eq!(strip_ansi(b"a\x1bOAb\x1bNOc"), "abc");
    }
    #[test]
    fn 截断保留头尾且不切多字节字符() {
        assert_eq!(truncate_head_tail(b"abcd", 4), (b"abcd".to_vec(), false));
        assert_eq!(
            truncate_head_tail(b"abcdefgh", 4),
            ("ab\n…（省略 4 字节）…\ngh".as_bytes().to_vec(), true)
        );
        for limit in 0..16 {
            let (out, truncated) = truncate_head_tail("甲乙𠀀丙丁".as_bytes(), limit);
            assert!(truncated);
            assert!(String::from_utf8(out).is_ok());
        }
        assert_eq!(truncate_head_tail(b"", 0), (vec![], false));
    }
}
