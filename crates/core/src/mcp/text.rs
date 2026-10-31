//! 工具输出的控制序列清理和头尾截断。

/// 字符串类控制序列最多占 4 KiB（含引导符），缺少终止符也必须推进读取。
pub(super) const STRING_SEQUENCE_LIMIT: usize = 4 * 1024;

fn string_sequence_len(bytes: &[u8]) -> Option<usize> {
    let window = &bytes[..bytes.len().min(STRING_SEQUENCE_LIMIT)];
    for end in 2..window.len() {
        if (bytes[1] == b']' && window[end] == 7) || matches!(window[end], 0x18 | 0x1a) {
            return Some(end + 1);
        }
        if window[end] == 0x1b {
            return Some(if window[end..].starts_with(b"\x1b\\") {
                end + 2
            } else {
                end
            });
        }
    }
    (bytes.len() >= STRING_SEQUENCE_LIMIT).then_some(STRING_SEQUENCE_LIMIT)
}

/// 清理 bytes 中的 CSI、OSC 和其他 ESC 序列，宽松解码 UTF-8，并将 CRLF 转成 LF。
/// 字符串类序列最多清理 4 KiB，防止缺失终止符吞掉后续输出；返回清理后的文字。
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
                let start = i - 2;
                i = start + string_sequence_len(&bytes[start..]).unwrap_or(bytes.len() - start);
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
    token_len_settled(bytes, false)
}

/// settled 表示已收到历史末尾且静止满一秒；此时孤立的字符串引导符可单独消费。
pub(super) fn token_len_settled(bytes: &[u8], settled: bool) -> Option<usize> {
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
            string_sequence_len(bytes).or_else(|| settled.then_some(2))
        }
        b'O' | b'N' => (bytes.len() >= 3).then_some(3),
        0x20..=0x2f => bytes[2..]
            .iter()
            .position(|b| !(0x20..=0x2f).contains(b))
            .map(|end| end + 3),
        _ => Some(2),
    }
}

/// 对齐时最多回看 4 KiB，避免大段终端历史使每次增量读取线性变慢。
pub(super) const ALIGN_LOOKBEHIND: usize = 4 * 1024;

/// 把请求起点向后移到字符或控制序列边界；只从起点前最多 4 KiB 开始解析。
pub(super) fn aligned_start(bytes: &[u8], requested: usize) -> usize {
    aligned_start_settled(bytes, requested, false)
}

/// 按 settled 指定的末尾静止状态把 requested 向后对齐；返回实际字节起点。
pub(super) fn aligned_start_settled(bytes: &[u8], requested: usize, settled: bool) -> usize {
    let requested = requested.min(bytes.len());
    let mut offset = requested.saturating_sub(ALIGN_LOOKBEHIND);
    offset += bytes[offset..]
        .iter()
        .take_while(|b| **b & 0xc0 == 0x80)
        .count();
    while offset < requested {
        match token_len_settled(&bytes[offset..], settled) {
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
    fn 字符串控制序列到达上限后继续读取且清理规则一致() {
        for kind in *b"]PX^_" {
            let mut bytes = vec![0x1b, kind];
            bytes.resize(STRING_SEQUENCE_LIMIT - 1, b'x');
            assert_eq!(token_len(&bytes), None);
            assert_eq!(strip_ansi(&bytes), "");
            bytes.push(b'x');
            assert_eq!(token_len(&bytes), Some(STRING_SEQUENCE_LIMIT));
            bytes.extend_from_slice("后续输出".as_bytes());
            assert_eq!(token_len(&bytes), Some(STRING_SEQUENCE_LIMIT));
            assert_eq!(strip_ansi(&bytes), "后续输出");
            assert_eq!(aligned_start(&bytes, 1), STRING_SEQUENCE_LIMIT);
            for terminator in [b"\x1b\\".as_slice(), b"\x07"] {
                if kind != b']' && terminator == b"\x07" {
                    continue;
                }
                let mut terminated = vec![0x1b, kind];
                terminated.resize(STRING_SEQUENCE_LIMIT - terminator.len(), b'x');
                terminated.extend_from_slice(terminator);
                terminated.extend_from_slice(b"tail");
                assert_eq!(token_len(&terminated), Some(STRING_SEQUENCE_LIMIT));
                assert_eq!(strip_ansi(&terminated), "tail");
            }
        }
    }

    #[test]
    fn 字符串遇到任意转义或取消字节立即结束() {
        for kind in *b"]PX^_" {
            let mut bytes = vec![0x1b, kind];
            bytes.extend(vec![b'x'; 2048]);
            let prefix = bytes.len();
            for ending in [
                b"\x1b[32m$ \x1b[0m".as_slice(),
                b"\x1b\\$ ",
                b"\x18$ ",
                b"\x1a$ ",
            ] {
                let data = [&bytes, ending].concat();
                let terminator = if ending.starts_with(b"\x1b\\") {
                    2
                } else if ending[0] == 0x1b {
                    0
                } else {
                    1
                };
                assert_eq!(token_len(&data), Some(prefix + terminator));
                assert_eq!(strip_ansi(&data), "$ ");
            }
            bytes.extend_from_slice(b"$ ");
            assert_eq!(token_len_settled(&bytes, false), None);
            assert_eq!(token_len_settled(&bytes, true), Some(2));
            assert_eq!(strip_ansi(&bytes[..2]), "");
        }
    }

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
