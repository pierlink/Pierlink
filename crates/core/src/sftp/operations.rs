//! 文件菜单操作；删除使用 lstat，编辑保存要求原子替换扩展。
use super::*;
use crate::fs_local::{FileVersion, TextFile, text};

fn validate_path(value: &str) -> Result<(), AppError> {
    if value.contains('\0') {
        return Err(AppError::new("files.invalid_name").with("path", value));
    }
    text::validate_name(value.rsplit('/').next().unwrap_or(""), false)
        .map_err(|e| e.with("path", value))
}
fn version(entry: &RemoteEntry) -> Result<FileVersion, AppError> {
    if entry.kind != EntryKind::File {
        return Err(AppError::new("files.not_text").with_reason(
            "sftp.only_regular_text_files_are_supported",
            "只支持普通文本文件",
        ));
    }
    Ok(FileVersion {
        size: entry.size,
        hash: None,
        mtime: entry.mtime,
    })
}
impl SftpSession {
    fn absent(&mut self, path: &str) -> Result<(), AppError> {
        if self.metadata(path, false)?.is_some() {
            return Err(AppError::new("sftp.exists").with("path", path));
        }
        Ok(())
    }
    /// 将 from 改名为 to；验证末段名字并拒绝覆盖，目标已存在返回带 path 的 sftp.exists。
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), AppError> {
        validate_path(to)?;
        self.absent(to).map_err(|e| e.with("path", to))?;
        self.rename_file(from, to, false)
            .map(|_| ())
            .map_err(|e| e.with("path", to))
    }
    /// 在 path 创建目录，已存在返回 sftp.exists，父目录必须已存在。
    pub fn create_dir(&mut self, path: &str) -> Result<(), AppError> {
        validate_path(path)?;
        self.absent(path).map_err(|e| e.with("path", path))?;
        self.mkdir(path).map_err(|e| e.with("path", path))
    }
    /// 在 path 排他创建空文件，已存在（含链接）返回 sftp.exists。
    pub fn create_file(&mut self, path: &str) -> Result<(), AppError> {
        validate_path(path)?;
        self.absent(path).map_err(|e| e.with("path", path))?;
        let handle = match self.open_write(path, None, true) {
            Ok(handle) => handle,
            Err(e) => {
                if self.metadata(path, false).ok().flatten().is_some() {
                    return Err(AppError::new("sftp.exists").with("path", path));
                }
                return Err(e.with("path", path));
            }
        };
        self.with_handle(handle, |_, _| Ok(()))
            .map_err(|e| e.with("path", path))
    }
    /// 递归删除 paths 并返回删除条目数（包含目录和链接）；不跟随链接，失败附实际失败路径。
    pub fn delete(&mut self, paths: &[String]) -> Result<usize, AppError> {
        let mut count = 0;
        for path in paths {
            self.delete_tree(path, 0, &mut count)?;
        }
        Ok(count)
    }
    fn delete_tree(
        &mut self,
        value: &str,
        depth: usize,
        count: &mut usize,
    ) -> Result<(), AppError> {
        let result = (|| {
            if depth > 128 {
                return Err(AppError::new("sftp.failure")
                    .with_reason("sftp.too_deep", "目录层级超过 128 层"));
            }
            let entry = self
                .metadata(value, false)?
                .ok_or_else(|| AppError::new("sftp.not_found"))?;
            if entry.kind == EntryKind::Dir {
                let listing = self.list_dir(value, 100_000)?;
                if listing.truncated || listing.skipped_non_utf8 > 0 {
                    return Err(AppError::new("sftp.failure").with_reason(
                        "sftp.too_many_entries_or_non_utf_8_names_prevent_complete",
                        "目录项超过限制或包含非 UTF-8 名字，无法完整删除",
                    ));
                }
                for entry in listing.entries {
                    self.delete_tree(&join_path(value, &entry.name), depth + 1, count)?;
                }
                let mut body = Vec::new();
                path(&mut body, value)?;
                self.ok(15, body)?;
            } else {
                self.remove_file(value)?;
            }
            *count += 1;
            Ok(())
        })();
        result.map_err(|e: AppError| {
            if e.params.contains_key("path") {
                e
            } else {
                e.with("path", value)
            }
        })
    }
    /// 读取 path 的 UTF-8 文本，最多 max_bytes 字节，返回去 BOM 的内容、BOM 标记和原文件版本。
    pub fn read_text(&mut self, value: &str, max_bytes: u64) -> Result<TextFile, AppError> {
        let result = (|| {
            let entry = self
                .metadata(value, true)?
                .ok_or_else(|| AppError::new("sftp.not_found"))?;
            let current = version(&entry)?;
            if current.size > max_bytes {
                return Err(text::too_large(max_bytes));
            }
            let bytes = self.read_text_bytes(value, max_bytes, true)?;
            text::decode(bytes, max_bytes, current)
        })();
        result.map_err(|e| e.with("path", value))
    }
    fn read_text_bytes(
        &mut self,
        value: &str,
        max_bytes: u64,
        validate_text: bool,
    ) -> Result<Vec<u8>, AppError> {
        let mut body = Vec::new();
        path(&mut body, value)?;
        body.extend(1u32.to_be_bytes());
        body.extend(0u32.to_be_bytes());
        let handle = self.handle(3, body)?;
        self.with_handle(handle, |session, handle| {
            let mut bytes = Vec::new();
            loop {
                let offset = bytes.len() as u64;
                let length =
                    (max_bytes.saturating_sub(offset).saturating_add(1)).min(CHUNK as u64) as usize;
                let id = session.send_read(handle, offset, length)?;
                let Some(data) = session.read_data(id, length)? else {
                    return Ok(bytes);
                };
                bytes.extend(data);
                if bytes.len() as u64 > max_bytes {
                    return Err(text::too_large(max_bytes));
                }
                // 第一块就检查（空字符、无效 UTF-8），之后每块只查空字符；完整校验在 decode 里
                if validate_text && offset == 0 {
                    text::sniff(&bytes)?;
                } else if validate_text && bytes[offset as usize..].contains(&0) {
                    return Err(text::not_text());
                }
            }
        })
    }
    fn check_text_version(
        &mut self,
        path: &str,
        expected: Option<&FileVersion>,
    ) -> Result<(), AppError> {
        let current = self
            .metadata(path, true)?
            .map(|e| version(&e))
            .transpose()?;
        text::check_version(current.as_ref(), expected)?;
        if let Some(expected) = expected.filter(|e| e.hash.is_some()) {
            if expected.size > text::TEXT_LIMIT {
                return Err(text::too_large(text::TEXT_LIMIT));
            }
            let bytes = self.read_text_bytes(path, text::TEXT_LIMIT, false)?;
            text::check_hash(expected, &text::hash(&bytes))?;
        }
        Ok(())
    }
    /// 将 text 按 bom 原子写回已存在的 path；expected 先比大小、时间，再在 5 MiB 内比可选哈希，返回含哈希的新版本。
    /// 符号链接解析到目标；同目录 .pierlink-part 临时文件自动清理，替换前恢复原权限；不支持原子替换时报错。
    pub fn write_text(
        &mut self,
        value: &str,
        content: &str,
        bom: bool,
        expected: Option<FileVersion>,
    ) -> Result<FileVersion, AppError> {
        let result = (|| {
            if !self.posix_rename {
                return Err(AppError::new("sftp.atomic_rename_unsupported").with_reason(
                    "sftp.server_cannot_replace_text_files_atomically",
                    "服务器不支持原子替换文本文件",
                ));
            }
            self.check_text_version(value, expected.as_ref())?;
            let target = self.realpath(value)?;
            let entry = self
                .metadata(&target, false)?
                .ok_or_else(|| AppError::new("sftp.not_found"))?;
            version(&entry)?;
            let parent = target.rsplit_once('/').map(|v| v.0).unwrap_or(".");
            let pending = join_path(
                parent,
                &format!(".pierlink-{}.pierlink-part", uuid::Uuid::new_v4()),
            );
            // 先取得排他句柄，再负责清理，避免误删已存在的临时文件。
            let handle = self.open_write(&pending, Some(0o600), true)?;
            let write_result = (|| {
                let prefix: &[u8] = if bom { b"\xef\xbb\xbf" } else { b"" };
                let mut input = prefix.chain(content.as_bytes());
                self.with_handle(handle, |session, handle| {
                    session.write_handle(handle, &mut input, &mut |_| {}, &AtomicBool::new(false))
                })?;
                self.chmod(&pending, entry.mode & 0o7777)?;
                self.check_text_version(&target, expected.as_ref())?;
                let updated = self
                    .metadata(&pending, false)?
                    .ok_or_else(|| AppError::new("sftp.not_found"))?;
                self.rename_file(&pending, &target, true)?;
                let mut updated = version(&updated)?;
                updated.hash = Some(text::content_hash(content, bom));
                Ok(updated)
            })();
            match write_result {
                Ok(updated) => Ok(updated),
                Err(e) => match self.cleanup_file(&pending) {
                    Ok(()) => Err(e),
                    Err(cleanup) => Err(e
                        .with("temporary_path", &pending)
                        .with_text(
                            "cleanup_reason",
                            "sftp.couldn_t_clean_up_temporary_files",
                            "临时文件清理失败",
                        )
                        .with("cleanup_code", cleanup.code)),
                },
            }
        })();
        result.map_err(|e| e.with("path", value))
    }
    /// 设置 path 的 Unix 权限为 mode（0..=0o7777）；越界返回 sftp.invalid_mode。
    pub fn chmod(&mut self, value: &str, mode: u32) -> Result<(), AppError> {
        if mode > 0o7777 {
            return Err(AppError::new("sftp.invalid_mode").with("path", value));
        }
        let mut body = Vec::new();
        path(&mut body, value)?;
        body.extend(4u32.to_be_bytes());
        body.extend(mode.to_be_bytes());
        self.ok(9, body).map_err(|e| e.with("path", value))
    }
}

#[cfg(all(test, unix))]
mod tests;
