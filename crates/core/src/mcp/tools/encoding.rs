//! 文件工具共用的严格解码、编码和换行处理，不用替代字符静默丢失内容。
use crate::error::AppError;
use encoding_rs::{Encoding, GBK};

#[derive(Clone, Copy, Debug)]
enum Codec {
    Utf8,
    Utf16(bool),
    Ansi(&'static Encoding),
}
#[derive(Debug)]
pub(super) struct Document {
    pub text: String,
    codec: Codec,
    bom: bool,
}
pub(super) fn unsupported() -> AppError {
    AppError::new("mcp.encoding_unsupported").with("reason", "文件编码无效或新内容无法用原编码表示")
}
pub(super) fn binary(bytes: &[u8]) -> bool {
    !bytes.starts_with(&[0xff, 0xfe])
        && !bytes.starts_with(&[0xfe, 0xff])
        && bytes.iter().take(8000).any(|b| *b == 0)
}
pub(super) fn ansi() -> Result<&'static Encoding, AppError> {
    #[cfg(windows)]
    {
        let cp = unsafe { windows_sys::Win32::Globalization::GetACP() };
        match cp {
            65001 => Ok(encoding_rs::UTF_8),
            936 => Ok(GBK),
            932 => Ok(encoding_rs::SHIFT_JIS),
            949 => Ok(encoding_rs::EUC_KR),
            950 => Ok(encoding_rs::BIG5),
            _ => Encoding::for_label(format!("windows-{cp}").as_bytes()).ok_or_else(unsupported),
        }
    }
    #[cfg(not(windows))]
    {
        Ok(GBK)
    }
}
impl Document {
    pub(super) fn decode(bytes: &[u8]) -> Result<Self, AppError> {
        let encoding = if std::str::from_utf8(bytes).is_ok()
            || bytes.starts_with(&[0xff, 0xfe])
            || bytes.starts_with(&[0xfe, 0xff])
            || bytes.starts_with(&[0xef, 0xbb, 0xbf])
        {
            encoding_rs::UTF_8
        } else {
            ansi()?
        };
        Self::decode_with(bytes, encoding)
    }
    fn decode_with(bytes: &[u8], ansi: &'static Encoding) -> Result<Self, AppError> {
        if binary(bytes) {
            return Err(AppError::new("mcp.binary_file").with("reason", "二进制文件不能按文字编辑"));
        }
        let (codec, bom, data) = if bytes.starts_with(&[0xff, 0xfe]) {
            (Codec::Utf16(true), true, &bytes[2..])
        } else if bytes.starts_with(&[0xfe, 0xff]) {
            (Codec::Utf16(false), true, &bytes[2..])
        } else if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            (Codec::Utf8, true, &bytes[3..])
        } else if std::str::from_utf8(bytes).is_ok() {
            (Codec::Utf8, false, bytes)
        } else {
            (Codec::Ansi(ansi), false, bytes)
        };
        let text = match codec {
            Codec::Utf8 => std::str::from_utf8(data)
                .map_err(|_| unsupported())?
                .to_owned(),
            Codec::Utf16(le) => {
                if !data.len().is_multiple_of(2) {
                    return Err(unsupported());
                }
                let words: Vec<_> = data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| {
                        if le {
                            u16::from_le_bytes([b[0], b[1]])
                        } else {
                            u16::from_be_bytes([b[0], b[1]])
                        }
                    })
                    .collect();
                String::from_utf16(&words).map_err(|_| unsupported())?
            }
            Codec::Ansi(encoding) => encoding
                .decode_without_bom_handling_and_without_replacement(data)
                .ok_or_else(unsupported)?
                .into_owned(),
        };
        Ok(Self { text, codec, bom })
    }
    pub(super) fn name(&self) -> &'static str {
        match self.codec {
            Codec::Utf8 => "utf-8",
            Codec::Utf16(true) => "utf-16le",
            Codec::Utf16(false) => "utf-16be",
            Codec::Ansi(e) if e == GBK => "gbk",
            Codec::Ansi(e) => e.name(),
        }
    }
    pub(super) fn line_ending(&self) -> &'static str {
        line_ending(&self.text)
    }
    pub(super) fn normalize(&self, text: &str) -> String {
        match self.line_ending() {
            "crlf" => text.replace("\r\n", "\n").replace('\n', "\r\n"),
            "lf" => text.replace("\r\n", "\n"),
            _ => text.to_owned(),
        }
    }
    pub(super) fn encode(&self, text: &str) -> Result<Vec<u8>, AppError> {
        let mut bytes = Vec::new();
        match self.codec {
            Codec::Utf8 => {
                if self.bom {
                    bytes.extend([0xef, 0xbb, 0xbf]);
                }
                bytes.extend(text.as_bytes());
            }
            Codec::Utf16(le) => {
                bytes.extend(if le { [0xff, 0xfe] } else { [0xfe, 0xff] });
                for word in text.encode_utf16() {
                    bytes.extend(if le {
                        word.to_le_bytes()
                    } else {
                        word.to_be_bytes()
                    });
                }
            }
            Codec::Ansi(e) => {
                let (encoded, _, errors) = e.encode(text);
                if errors {
                    return Err(unsupported());
                }
                bytes.extend(encoded.as_ref());
            }
        }
        Ok(bytes)
    }
    // 混合换行统一归一化匹配，避免漏掉仅分隔符不同的重复内容；返回范围仍指向原文。
    pub(super) fn match_ranges(&self, old: &str) -> Vec<std::ops::Range<usize>> {
        if self.line_ending() != "mixed" {
            let direct: Vec<_> = self
                .text
                .match_indices(old)
                .map(|(i, s)| i..i + s.len())
                .collect();
            if !direct.is_empty() {
                return direct;
            }
        }
        let normalized = self.text.replace("\r\n", "\n");
        let needle = old.replace("\r\n", "\n");
        let removed: Vec<_> = self
            .text
            .match_indices("\r\n")
            .enumerate()
            .map(|(n, (i, _))| i - n)
            .collect();
        let original = |i| i + removed.partition_point(|p| *p < i);
        normalized
            .match_indices(&needle)
            .map(|(i, s)| original(i)..original(i + s.len()))
            .collect()
    }
    // 混合换行的整文件写入按原行序保留各行分隔符；新增行沿用最后一个分隔符。
    pub(super) fn fragment_replacement(&self, old: &str, new: &str) -> String {
        if self.line_ending() == "mixed" && old.contains('\n') {
            Self {
                text: old.to_owned(),
                codec: self.codec,
                bom: self.bom,
            }
            .replacement(new)
        } else {
            self.normalize(new)
        }
    }
    pub(super) fn replacement(&self, text: &str) -> String {
        if self.line_ending() != "mixed" {
            return self.normalize(text);
        }
        let endings: Vec<_> = self
            .text
            .split_inclusive('\n')
            .filter(|s| s.ends_with('\n'))
            .map(|s| if s.ends_with("\r\n") { "\r\n" } else { "\n" })
            .collect();
        let normalized = text.replace("\r\n", "\n");
        let mut result = String::new();
        for (index, line) in normalized.split_inclusive('\n').enumerate() {
            if let Some(line) = line.strip_suffix('\n') {
                result.push_str(line);
                result.push_str(
                    endings
                        .get(index)
                        .or_else(|| endings.last())
                        .copied()
                        .unwrap_or("\n"),
                );
            } else {
                result.push_str(line);
            }
        }
        result
    }
}
pub(super) fn line_ending(text: &str) -> &'static str {
    let lf = text.bytes().filter(|b| *b == b'\n').count();
    let crlf = text.as_bytes().windows(2).filter(|b| *b == b"\r\n").count();
    if crlf == 0 {
        "lf"
    } else if crlf == lf {
        "crlf"
    } else {
        "mixed"
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 编码与原字节和换行保持() {
        for bytes in [
            b"\xef\xbb\xbfabc\r\n".to_vec(),
            [
                vec![0xff, 0xfe],
                "中文\r\n"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            ]
            .concat(),
            [
                vec![0xfe, 0xff],
                "中文".encode_utf16().flat_map(u16::to_be_bytes).collect(),
            ]
            .concat(),
            GBK.encode("中文\n").0.into_owned(),
            b"a\r\nb\nc\r\n".to_vec(),
        ] {
            let doc = Document::decode_with(&bytes, GBK).unwrap();
            assert_eq!(doc.encode(&doc.text).unwrap(), bytes);
        }
        let doc = Document::decode_with(&GBK.encode("中文\r\n").0, GBK).unwrap();
        assert_eq!(doc.name(), "gbk");
        assert_eq!(doc.normalize("新\n"), "新\r\n");
        assert_eq!(
            doc.encode("😀").unwrap_err().code,
            "mcp.encoding_unsupported"
        );
        let doc = Document::decode(b"a\r\nb\nc\r\n").unwrap();
        assert_eq!(doc.replacement("x\ny\nz\n"), "x\r\ny\nz\r\n");
        assert!(binary(b"a\0b"));
        assert!(Document::decode(b"\xff\xfe\0").is_err());
        assert!(Document::decode(b"\xef\xbb\xbf\xff").is_err());
    }
    #[cfg(windows)]
    #[test]
    fn 系统代码页严格往返() {
        let e = ansi().unwrap();
        let (bytes, _, errors) = e.encode("测试文字");
        if !errors {
            let doc = Document::decode_with(&bytes, e).unwrap();
            assert_eq!(doc.text, "测试文字");
        }
    }
}
