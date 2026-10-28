//! 本机与 SFTP 共用的文本模型、名字与版本校验。
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct FileVersion {
    pub size: u64,
    /// Unix 秒；与 SFTP v3 精度一致。
    pub mtime: i64,
    /// 包含 BOM 的原始字节 SHA-256；旧调用方可省略。
    #[serde(default)]
    pub hash: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct TextFile {
    pub text: String,
    pub bom: bool,
    /// 包含 BOM 的原文件字节数。
    pub size: u64,
    /// Unix 秒。
    pub mtime: i64,
    /// 包含 BOM 的原始字节 SHA-256。
    pub hash: Option<String>,
}

pub(crate) fn validate_name(name: &str, windows: bool) -> Result<(), AppError> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.contains(['/', '\0'])
        || (windows && crate::sftp::transfer::windows_filename(name) != name)
    {
        return Err(
            AppError::new("files.invalid_name").with("reason", "名字为空、含非法字符或为保留名")
        );
    }
    Ok(())
}

pub(crate) fn check_version(
    current: Option<&FileVersion>,
    expected: Option<&FileVersion>,
) -> Result<(), AppError> {
    if let Some(expected) = expected
        && !current.is_some_and(|v| v.size == expected.size && v.mtime == expected.mtime)
    {
        return Err(AppError::new("files.changed").with("reason", "文件大小或修改时间已改变"));
    }
    Ok(())
}

pub(crate) const TEXT_LIMIT: u64 = 5 << 20;

pub(crate) fn hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn content_hash(content: &str, bom: bool) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    if bom {
        hash.update(b"\xef\xbb\xbf");
    }
    hash.update(content.as_bytes());
    format!("{:x}", hash.finalize())
}

pub(crate) fn check_hash(expected: &FileVersion, hash: &str) -> Result<(), AppError> {
    if expected.hash.as_deref().is_some_and(|h| h != hash) {
        return Err(AppError::new("files.changed").with("reason", "文件内容已改变"));
    }
    Ok(())
}

pub(crate) fn decode(
    bytes: Vec<u8>,
    max_bytes: u64,
    version: FileVersion,
) -> Result<TextFile, AppError> {
    if bytes.len() as u64 > max_bytes {
        return Err(too_large(max_bytes));
    }
    if bytes.contains(&0) {
        return Err(not_text());
    }
    let hash = Some(hash(&bytes));
    let mut text = String::from_utf8(bytes).map_err(|_| not_text())?;
    let bom = text.starts_with('\u{feff}');
    if bom {
        text.drain(..3);
    }
    Ok(TextFile {
        text,
        bom,
        size: version.size,
        mtime: version.mtime,
        hash,
    })
}
pub(crate) fn too_large(limit: u64) -> AppError {
    AppError::new("files.too_large").with("max_bytes", limit)
}
/// 读到一部分就先检查：有空字符，或已经出现确定无效的 UTF-8（不是末尾被截断的半个字符），
/// 就不是文本，不必把整个文件读完（服务器上的大图片逐块读要很久）
pub(crate) fn sniff(bytes: &[u8]) -> Result<(), AppError> {
    if bytes.contains(&0) {
        return Err(not_text());
    }
    match std::str::from_utf8(bytes) {
        Err(e) if e.error_len().is_some() => Err(not_text()),
        _ => Ok(()),
    }
}
pub(crate) fn not_text() -> AppError {
    AppError::new("files.not_text").with("reason", "文件包含空字符或不是有效的 UTF-8 文本")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 读到一部分就能判断不是文本() {
        assert!(sniff(b"plain text").is_ok());
        assert!(sniff("中文".as_bytes()).is_ok());
        // 末尾被截断的半个汉字不算错
        assert!(sniff(&"中文".as_bytes()[..4]).is_ok());
        assert_eq!(
            sniff(b"\x89PNG\r\n\x1a\n\0\0").unwrap_err().code,
            "files.not_text"
        );
        assert_eq!(sniff(b"\xff\xfe abc").unwrap_err().code, "files.not_text");
    }
    #[test]
    fn 名字及版本校验() {
        for name in ["", ".", "..", "a/b", "a\0b"] {
            assert!(validate_name(name, false).is_err());
        }
        for name in ["CON", "nul.txt", "COM¹", "a\\b", "a:", "a?", "a.", "a "] {
            assert!(validate_name(name, true).is_err());
        }
        assert!(validate_name("中文 ' $().txt", true).is_ok());
        assert!(validate_name("a\\b", false).is_ok());
        assert!(check_version(None, None).is_ok());
        assert!(
            check_version(
                None,
                Some(&FileVersion {
                    size: 0,
                    mtime: 0,
                    hash: None
                })
            )
            .is_err()
        );
    }
}
