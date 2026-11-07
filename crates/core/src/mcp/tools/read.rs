//! 按固定缓冲扫描一次，同时识别编码、统计行数并只保留请求的行。
use super::{
    check_cancel, encoding,
    files::{self, Sandbox},
};
use crate::error::AppError;
use encoding_rs::{Decoder, DecoderResult, Encoding, UTF_8, UTF_16BE, UTF_16LE};
use serde_json::{Value, json};
use std::{io::Read, path::Path, sync::atomic::AtomicBool};

pub(super) fn lines(
    sb: &Sandbox,
    path: &str,
    offset: u64,
    limit: u64,
    cancelled: &AtomicBool,
) -> Result<Value, AppError> {
    check_cancel(cancelled)?;
    let resolved = sb.resolve_read(path)?;
    let mut file = files::open_file(&resolved)?;
    let metadata = file.metadata().map_err(|_| io_error())?;
    let mut sample = [0; 8000];
    let mut len = 0;
    while len < sample.len() {
        let count = files::read_chunk(&mut file, &mut sample[len..], &resolved, cancelled)?;
        if count == 0 {
            break;
        }
        len += count;
    }
    let sample = &sample[..len];
    if encoding::binary(sample) {
        return Ok(json!(files::read_cancelled(
            sb,
            path,
            0,
            1024 * 1024,
            cancelled
        )?));
    }
    let mut result = scan(
        &mut file,
        &resolved,
        sample,
        metadata.len(),
        offset,
        limit,
        cancelled,
    )?;
    files::check_metadata(&file, &metadata)?;
    result["mtime_ms"] = json!(files::mtime(&metadata));
    Ok(result)
}
/// read_files 这次读到的图片加起来最多多少字节（超过的那张请用 read_file 单独读）
pub(super) const IMAGE_BUDGET: usize = 8 * 1024 * 1024;

/// read_files 里的一个文件：和 read_file 默认的读法一样（小图片返回图片内容，其他按行读，二进制按字节读）。
/// budget 是这次还能返回的图片字节数，读到图片时扣掉。
///
/// :param sb: 授权文件夹
/// :param path: 文件路径（Windows 绝对路径）
/// :param limit: 最多读多少行
/// :param budget: 剩下的图片额度
/// :param cancelled: 取消标志
/// :return: 和 read_file 一样的结果；图片超过剩下的额度时返回 mcp.file_too_large
pub(super) fn listed(
    sb: &Sandbox,
    path: &str,
    limit: u64,
    budget: &mut usize,
    cancelled: &AtomicBool,
) -> Result<Value, AppError> {
    if let Some(image) = files::read_image(sb, path, cancelled)? {
        let size = image["size"]
            .as_u64()
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(usize::MAX);
        if size > *budget {
            return Err(AppError::new("mcp.file_too_large")
                .with("max_bytes", IMAGE_BUDGET)
                .with_reason(
                    "mcp.images_exceed_the_read_files_budget",
                    "这次读的图片加起来超过 8 MiB，这张请用 read_file 单独读",
                ));
        }
        *budget -= size;
        return Ok(image);
    }
    lines(sb, path, 1, limit, cancelled)
}
fn scan(
    reader: &mut impl Read,
    path: &Path,
    sample: &[u8],
    size: u64,
    offset: u64,
    limit: u64,
    cancelled: &AtomicBool,
) -> Result<Value, AppError> {
    let (encoding, bom) = if sample.starts_with(&[0xff, 0xfe]) {
        (UTF_16LE, 2)
    } else if sample.starts_with(&[0xfe, 0xff]) {
        (UTF_16BE, 2)
    } else if sample.starts_with(&[0xef, 0xbb, 0xbf]) {
        (UTF_8, 3)
    } else {
        (UTF_8, 0)
    };
    let mut primary = Some(Scan::new(encoding, offset, limit));
    // 无 BOM 时并行保留系统编码的有限行结果，文件末尾才出现非 UTF-8 也不必回读。
    let mut fallback = if bom == 0 {
        encoding::ansi()
            .ok()
            .filter(|e| *e != UTF_8)
            .map(|e| Scan::new(e, offset, limit))
    } else {
        None
    };
    let mut digest = files::FileDigest::new(size);
    let mut reader = std::io::Cursor::new(sample).chain(reader);
    let mut buffer = [0; 32768];
    let mut skip = bom;
    loop {
        let count = files::read_chunk(&mut reader, &mut buffer, path, cancelled)?;
        digest.update(&buffer[..count]);
        let start = skip.min(count);
        skip -= start;
        for candidate in [&mut primary, &mut fallback] {
            if let Some(scan) = candidate
                && decode(
                    &mut scan.decoder,
                    &buffer[start..count],
                    count == 0,
                    |text| {
                        scan.lines.push(text);
                        Ok(())
                    },
                )
                .is_err()
            {
                *candidate = None;
            }
        }
        check_cancel(cancelled)?;
        if primary.is_none() && fallback.is_none() {
            return Err(encoding::unsupported());
        }
        if count == 0 {
            break;
        }
    }
    let mut scan = primary.or(fallback).ok_or_else(encoding::unsupported)?;
    scan.lines.finish();
    let lines = scan.lines;
    let line_ending = if lines.crlf == 0 {
        "lf"
    } else if lines.crlf == lines.lf {
        "crlf"
    } else {
        "mixed"
    };
    let sha256 = digest.finish();
    let skipped = sha256.is_none().then_some(files::HASH_SKIPPED);
    Ok(
        json!({"content":lines.output,"total_lines":lines.total,"encoding":name(scan.encoding),"line_ending":line_ending,"sha256":sha256,"sha256_skipped_reason":skipped,"offset":offset,"truncated":lines.total>offset.saturating_sub(1).saturating_add(limit.min(5000))}),
    )
}
struct Scan {
    encoding: &'static Encoding,
    decoder: Decoder,
    lines: Lines,
}
impl Scan {
    fn new(encoding: &'static Encoding, offset: u64, limit: u64) -> Self {
        Self {
            encoding,
            decoder: encoding.new_decoder_without_bom_handling(),
            lines: Lines::new(offset, limit.min(5000)),
        }
    }
}
fn io_error() -> AppError {
    AppError::new("mcp.path_invalid").with_reason("mcp.couldn_t_read_the_file", "无法读取文件")
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
    fn 多文件读取每项和_read_file_一样_图片扣额度() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let text = root.path().join("a.txt");
        std::fs::write(&text, "一\n二\n三\n").unwrap();
        let image = root.path().join("b.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\n0123456789").unwrap();
        let cancelled = AtomicBool::new(false);
        let mut budget = 30;
        let read = listed(&sb, text.to_str().unwrap(), 2, &mut budget, &cancelled).unwrap();
        assert_eq!(read["content"], "     1\t一\n     2\t二\n");
        assert_eq!(read["truncated"], true);
        let first = listed(&sb, image.to_str().unwrap(), 2000, &mut budget, &cancelled).unwrap();
        assert_eq!(first["size"], 18);
        assert_eq!(budget, 12);
        // 额度不够：这张不返回
        let error =
            listed(&sb, image.to_str().unwrap(), 2000, &mut budget, &cancelled).unwrap_err();
        assert_eq!(error.code, "mcp.file_too_large");
        assert_eq!(
            error.params["reason_key"],
            "mcp.images_exceed_the_read_files_budget"
        );
        assert_eq!(budget, 12);
    }
    #[test]
    fn 单次扫描支持末尾编码回退与读取中取消() {
        struct OnceReader<'a> {
            bytes: std::io::Cursor<Vec<u8>>,
            read: usize,
            cancel_after: usize,
            cancelled: &'a AtomicBool,
        }
        impl Read for OnceReader<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let count = self.bytes.read(buffer)?;
                self.read += count;
                if self.read >= self.cancel_after {
                    self.cancelled
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                Ok(count)
            }
        }
        let cancelled = AtomicBool::new(false);
        let mut bytes = vec![b'x'; 70000];
        bytes.extend_from_slice(b"\nlast\n");
        let mut reader = OnceReader {
            bytes: std::io::Cursor::new(bytes.clone()),
            read: 0,
            cancel_after: usize::MAX,
            cancelled: &cancelled,
        };
        let result = scan(
            &mut reader,
            Path::new("file.txt"),
            &[],
            bytes.len() as u64,
            2,
            1,
            &cancelled,
        )
        .unwrap();
        assert_eq!(reader.read, bytes.len());
        assert_eq!(result["content"], "     2\tlast\n");
        assert_eq!(result["sha256"], files::hash(&bytes));
        reader.bytes.set_position(0);
        reader.read = 0;
        reader.cancel_after = 32768;
        assert_eq!(
            scan(
                &mut reader,
                Path::new("file.txt"),
                &[],
                bytes.len() as u64,
                1,
                1,
                &cancelled
            )
            .unwrap_err()
            .code,
            "mcp.cancelled"
        );
        assert_eq!(reader.read, 32768);
        cancelled.store(false, std::sync::atomic::Ordering::Release);
        let encoding = encoding::ansi().unwrap();
        let (tail, _, errors) = encoding.encode("中文");
        if !errors && encoding != UTF_8 {
            bytes.extend_from_slice(&tail);
            let result = scan(
                &mut std::io::Cursor::new(&bytes),
                Path::new("file.txt"),
                &[],
                bytes.len() as u64,
                3,
                1,
                &cancelled,
            )
            .unwrap();
            assert_eq!(result["content"], "     3\t中文\n");
            assert_eq!(result["encoding"], name(encoding));
        }
    }
    #[test]
    fn 散列阈值含边界且超限行读取说明原因() {
        assert!(!files::FileDigest::new(files::HASH_LIMIT).skipped());
        assert!(files::FileDigest::new(files::HASH_LIMIT + 1).skipped());
        let result = scan(
            &mut std::io::Cursor::new(b"first\nlast\n"),
            Path::new("file.txt"),
            &[],
            files::HASH_LIMIT + 1,
            2,
            1,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result["content"], "     2\tlast\n");
        assert!(result["sha256"].is_null());
        assert_eq!(result["sha256_skipped_reason"], files::HASH_SKIPPED);
    }
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
