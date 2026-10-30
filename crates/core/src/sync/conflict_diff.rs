//! 本机冲突只读对比；读取和差异计算均有上限，不写临时文件。
use super::{Conflict, parse_name, safe};
use crate::error::AppError;
use serde::Serialize;
use std::{
    fs::{self, File},
    io::Read,
    path::Path,
    time::{Duration, UNIX_EPOCH},
};

const LIMIT: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct DiffFile {
    pub size: u64,
    pub modified_ms: i64,
    pub encoding: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Equal,
    Deleted,
    Added,
    Modified,
}
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct DiffLine {
    /// 行号从 1 开始；空侧为 None，正文保留原来的行尾。
    pub left_number: Option<u32>,
    pub right_number: Option<u32>,
    pub left: Option<String>,
    pub right: Option<String>,
    pub kind: DiffKind,
}
#[derive(Debug, Clone, Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConflictDiff {
    Text {
        left: DiffFile,
        right: DiffFile,
        lines: Vec<DiffLine>,
    },
    Binary {
        left: DiffFile,
        right: DiffFile,
    },
    TooLarge {
        left: DiffFile,
        right: DiffFile,
        limit: u64,
    },
}
fn io(e: std::io::Error) -> AppError {
    AppError::new("sync.diff_io")
        .with_reason(
            "sync.read_conflict",
            format!("读取冲突文件失败（{:?}）", e.kind()),
        )
        .with("io_kind", format!("{:?}", e.kind()))
}
fn open(root: &Path, relative: &str) -> Result<(File, DiffFile), AppError> {
    let path = safe(root, relative)?;
    let canonical = fs::canonicalize(&path).map_err(io)?;
    if !canonical.starts_with(root) {
        return Err(AppError::new("sync.diff_outside_root"));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(&path).map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err(AppError::new("sync.diff_not_file"));
    }
    let modified = meta.modified().map_err(io)?;
    let modified_ms = match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis().min(i64::MAX as u128) as i64,
        Err(e) => -(e.duration().as_millis().min(i64::MAX as u128) as i64),
    };
    Ok((
        file,
        DiffFile {
            size: meta.len(),
            modified_ms,
            encoding: None,
        },
    ))
}
fn decode(bytes: &[u8]) -> Option<(String, String)> {
    let (text, encoding) = if bytes.starts_with(b"\xff\xfe") || bytes.starts_with(b"\xfe\xff") {
        if !bytes.len().is_multiple_of(2) {
            return None;
        }
        let little = bytes[0] == 255;
        let words: Vec<_> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| {
                if little {
                    u16::from_le_bytes([b[0], b[1]])
                } else {
                    u16::from_be_bytes([b[0], b[1]])
                }
            })
            .collect();
        (
            String::from_utf16(&words).ok()?,
            if little { "utf-16le" } else { "utf-16be" },
        )
    } else {
        (
            std::str::from_utf8(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes))
                .ok()?
                .to_owned(),
            "utf-8",
        )
    };
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\u{c}'))
    {
        return None;
    }
    Some((text, encoding.into()))
}

/// 对比 local_root 内 conflict 的原文件及副本，返回并排行、二进制元数据或超限大小。
/// 每侧最多读取 2 MiB；接受 UTF-8 和有 BOM 的 UTF-16，无法确定编码时按二进制处理。
/// 路径为任务内相对路径，拒绝链接及越界；文件读取期间变化时返回 conflict_changed。
pub fn compare_conflict(local_root: &Path, conflict: &Conflict) -> Result<ConflictDiff, AppError> {
    if parse_name(&conflict.conflict_file).map(|v| v.0).as_deref() != Some(&conflict.path) {
        return Err(AppError::new("sync.invalid_input").with_reason(
            "sync.conflict_copy_does_not_match_the_original_file",
            "冲突副本与原文件不匹配",
        ));
    }
    let root = fs::canonicalize(local_root).map_err(io)?;
    let (mut a, mut left) = open(&root, &conflict.path)?;
    let (mut b, mut right) = open(&root, &conflict.conflict_file)?;
    if left.size > LIMIT || right.size > LIMIT {
        return Ok(ConflictDiff::TooLarge {
            left,
            right,
            limit: LIMIT,
        });
    }
    let mut contents = Vec::new();
    for (file, info) in [(&mut a, &left), (&mut b, &right)] {
        let before = file.metadata().map_err(io)?;
        let mut bytes = Vec::new();
        file.take(LIMIT).read_to_end(&mut bytes).map_err(io)?;
        let after = file.metadata().map_err(io)?;
        if bytes.len() as u64 != info.size
            || after.len() != before.len()
            || after.modified().map_err(io)? != before.modified().map_err(io)?
        {
            return Err(AppError::new("sync.conflict_changed"));
        }
        contents.push(decode(&bytes));
    }
    let (Some((a, a_encoding)), Some((b, b_encoding))) = (contents.remove(0), contents.remove(0))
    else {
        return Ok(ConflictDiff::Binary { left, right });
    };
    left.encoding = Some(a_encoding);
    right.encoding = Some(b_encoding);
    let diff = similar::TextDiff::configure()
        .timeout(Duration::from_secs(1))
        .diff_lines(&a, &b);
    let mut lines = Vec::new();
    for op in diff.ops() {
        let (tag, old, new) = op.as_tag_tuple();
        for offset in 0..old.len().max(new.len()) {
            let l = (offset < old.len()).then_some(old.start + offset);
            let r = (offset < new.len()).then_some(new.start + offset);
            lines.push(DiffLine {
                left_number: l.map(|v| v as u32 + 1),
                right_number: r.map(|v| v as u32 + 1),
                left: l.map(|v| diff.old_lookup()[v].to_string()),
                right: r.map(|v| diff.new_lookup()[v].to_string()),
                kind: match (tag, l, r) {
                    (similar::DiffTag::Equal, _, _) => DiffKind::Equal,
                    (_, Some(_), Some(_)) => DiffKind::Modified,
                    (_, Some(_), None) => DiffKind::Deleted,
                    _ => DiffKind::Added,
                },
            });
        }
    }
    Ok(ConflictDiff::Text { left, right, lines })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn conflict() -> Conflict {
        Conflict {
            path: "a".into(),
            conflict_file: "a.sync-conflict-20261022-123456-ABCDEFG".into(),
            from_device: None,
            current_mtime_ms: 0,
            conflict_mtime_ms: 0,
        }
    }
    #[test]
    fn 并排行号修改增删与换行保留() {
        let root = crate::test_support::tempdir();
        let c = conflict();
        fs::write(root.path().join(&c.path), "同\n旧\n尾\n删除\n").unwrap();
        fs::write(root.path().join(&c.conflict_file), "同\n新\n增加\n尾\n").unwrap();
        let ConflictDiff::Text { lines, .. } = compare_conflict(root.path(), &c).unwrap() else {
            panic!()
        };
        assert!(lines.iter().any(|l| l.kind == DiffKind::Modified));
        assert!(lines.iter().any(|l| l.kind == DiffKind::Added));
        assert!(lines.iter().any(|l| l.kind == DiffKind::Deleted));
        assert_eq!(lines[0].left_number, Some(1));
        assert_eq!(lines[0].left.as_deref(), Some("同\n"));
    }
    #[test]
    fn 编码二进制超限和路径校验() {
        let root = crate::test_support::tempdir();
        let c = conflict();
        for bytes in [
            b"\xff\xfeA\0\n\0".as_slice(),
            b"\xfe\xff\0A\0\n",
            b"\xef\xbb\xbfA\n",
        ] {
            assert!(decode(bytes).is_some());
        }
        for bytes in [b"a\0b".as_slice(), b"\xff\xfe\0", b"\xff\xff"] {
            assert!(decode(bytes).is_none());
        }
        fs::write(root.path().join(&c.path), b"\0PNG").unwrap();
        fs::write(root.path().join(&c.conflict_file), b"text").unwrap();
        assert!(matches!(
            compare_conflict(root.path(), &c).unwrap(),
            ConflictDiff::Binary { .. }
        ));
        File::create(root.path().join(&c.path))
            .unwrap()
            .set_len(LIMIT + 1)
            .unwrap();
        assert!(
            matches!(compare_conflict(root.path(), &c).unwrap(), ConflictDiff::TooLarge { left, .. } if left.size == LIMIT + 1)
        );
        let mut bad = c.clone();
        bad.path = "../a".into();
        bad.conflict_file = format!("../{}", c.conflict_file);
        assert!(compare_conflict(root.path(), &bad).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path(), root.path().join("link")).unwrap();
            bad.path = "link/a".into();
            bad.conflict_file = format!("link/{}", c.conflict_file);
            assert!(compare_conflict(root.path(), &bad).is_err());
        }
    }
}
