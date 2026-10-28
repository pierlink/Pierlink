//! 按固定缓冲扫描文件两遍：识别编码和整文件散列，再只保留请求的行。
use super::{
    check_cancel, encoding,
    files::{self, Sandbox},
};
use crate::error::AppError;
use encoding_rs::{Decoder, DecoderResult, Encoding, UTF_8, UTF_16BE, UTF_16LE};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom},
    sync::atomic::AtomicBool,
};

pub(super) fn lines(
    sb: &Sandbox,
    path: &str,
    offset: u64,
    limit: u64,
    cancelled: &AtomicBool,
) -> Result<Value, AppError> {
    let resolved = sb.resolve_read(path)?;
    let mut file = files::open_file(&resolved)?;
    let metadata = file.metadata().map_err(|_| io_error())?;
    let mut digest = Sha256::new();
    let mut sample = Vec::new();
    let mut utf8 = true;
    let mut decoder = UTF_8.new_decoder_without_bom_handling();
    let mut buffer = [0u8; 32768];
    loop {
        check_cancel(cancelled)?;
        let count = file.read(&mut buffer).map_err(|_| io_error())?;
        sample.extend_from_slice(&buffer[..count.min(8000usize.saturating_sub(sample.len()))]);
        digest.update(&buffer[..count]);
        if utf8 && decode(&mut decoder, &buffer[..count], count == 0, |_| Ok(())).is_err() {
            utf8 = false;
        }
        if count == 0 {
            break;
        }
    }
    let sha256 = format!("{:x}", digest.finalize());
    if encoding::binary(&sample) {
        return Ok(json!(files::read_file(sb, path, 0, 1024 * 1024)?));
    }
    let (encoding, bom) = if sample.starts_with(&[0xff, 0xfe]) {
        (UTF_16LE, 2)
    } else if sample.starts_with(&[0xfe, 0xff]) {
        (UTF_16BE, 2)
    } else if sample.starts_with(&[0xef, 0xbb, 0xbf]) {
        (UTF_8, 3)
    } else if utf8 {
        (UTF_8, 0)
    } else {
        (encoding::ansi()?, 0)
    };
    file.seek(SeekFrom::Start(0)).map_err(|_| io_error())?;
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let mut lines = Lines::new(offset, limit.min(5000));
    let mut digest = Sha256::new();
    let mut skip = bom;
    loop {
        check_cancel(cancelled)?;
        let count = file.read(&mut buffer).map_err(|_| io_error())?;
        digest.update(&buffer[..count]);
        let start = skip.min(count);
        skip -= start;
        decode(&mut decoder, &buffer[start..count], count == 0, |text| {
            check_cancel(cancelled)?;
            lines.push(text);
            Ok(())
        })?;
        if count == 0 {
            break;
        }
    }
    if format!("{:x}", digest.finalize()) != sha256 {
        return Err(AppError::new("mcp.file_changed")
            .with("reason", "读取期间文件内容发生变化，请重新读取"));
    }
    lines.finish();
    let line_ending = if lines.crlf == 0 {
        "lf"
    } else if lines.crlf == lines.lf {
        "crlf"
    } else {
        "mixed"
    };
    Ok(
        json!({"content":lines.output,"total_lines":lines.total,"encoding":name(encoding),"line_ending":line_ending,"mtime_ms":files::mtime(&metadata),"sha256":sha256,"offset":offset,"truncated":lines.total>offset.saturating_sub(1).saturating_add(limit.min(5000))}),
    )
}
fn io_error() -> AppError {
    AppError::new("mcp.path_invalid").with("reason", "无法读取文件")
}
fn name(encoding: &'static Encoding) -> &'static str {
    if encoding == UTF_8 {
        "utf-8"
    } else if encoding == UTF_16LE {
        "utf-16le"
    } else if encoding == UTF_16BE {
        "utf-16be"
    } else if encoding == encoding_rs::GBK {
        "gbk"
    } else {
        encoding.name()
    }
}
fn decode(
    decoder: &mut Decoder,
    mut bytes: &[u8],
    last: bool,
    mut output: impl FnMut(&str) -> Result<(), AppError>,
) -> Result<(), AppError> {
    let mut text = [0; 65536];
    loop {
        let (result, read, written) =
            decoder.decode_to_utf8_without_replacement(bytes, &mut text, last);
        if matches!(result, DecoderResult::Malformed(..)) {
            return Err(encoding::unsupported());
        }
        output(std::str::from_utf8(&text[..written]).map_err(|_| encoding::unsupported())?)?;
        bytes = &bytes[read..];
        if result == DecoderResult::InputEmpty {
            return Ok(());
        }
    }
}
struct Lines {
    offset: u64,
    limit: u64,
    total: u64,
    line: String,
    chars: usize,
    output: String,
    lf: u64,
    crlf: u64,
    previous: Option<char>,
}
impl Lines {
    fn new(offset: u64, limit: u64) -> Self {
        Self {
            offset: offset.max(1),
            limit,
            total: 0,
            line: String::new(),
            chars: 0,
            output: String::new(),
            lf: 0,
            crlf: 0,
            previous: None,
        }
    }
    fn selected(&self) -> bool {
        self.total >= self.offset - 1 && self.total < (self.offset - 1).saturating_add(self.limit)
    }
    fn push(&mut self, text: &str) {
        for c in text.chars() {
            if c == '\n' {
                self.lf += 1;
                if self.previous == Some('\r') {
                    self.crlf += 1;
                    if self.line.ends_with('\r') {
                        self.line.pop();
                    }
                    self.chars = self.chars.saturating_sub(1);
                }
                self.emit();
            } else {
                self.chars = self.chars.saturating_add(1);
                if self.selected() && self.chars <= 2000 {
                    self.line.push(c);
                }
            }
            self.previous = Some(c);
        }
    }
    fn emit(&mut self) {
        if self.selected() {
            use std::fmt::Write;
            let suffix = if self.chars > 2000 {
                "…（行已截断）"
            } else {
                ""
            };
            let _ = writeln!(self.output, "{:>6}\t{}{suffix}", self.total + 1, self.line);
        }
        self.total += 1;
        self.line.clear();
        self.chars = 0;
    }
    fn finish(&mut self) {
        if self.chars > 0 {
            self.emit();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 大文件仅保留选中行且散列覆盖全部字节() {
        let root = crate::test_support::tempdir();
        let path = root.path().join("large.txt");
        let bytes = [vec![b'x'; 10 * 1024 * 1024 + 10], b"\r\nlast\n".to_vec()].concat();
        std::fs::write(&path, &bytes).unwrap();
        let sb = Sandbox::new(&[root.path().into()]);
        let value = lines(&sb, path.to_str().unwrap(), 2, 1, &AtomicBool::new(false)).unwrap();
        assert_eq!(value["content"], "     2\tlast\n");
        assert_eq!(value["total_lines"], 2);
        assert_eq!(value["sha256"], files::hash(&bytes));
        assert_eq!(value["line_ending"], "mixed");
        assert_eq!(
            lines(&sb, path.to_str().unwrap(), 1, 1, &AtomicBool::new(true))
                .unwrap_err()
                .code,
            "mcp.cancelled"
        );
    }
}
