//! 仅允许访问授权目录中的普通路径；写入以同目录临时文件原子替换。
use super::{
    check_cancel,
    encoding::{Document, binary},
};
use crate::{error::AppError, fs_local};
use serde::Serialize;
use sha2::{Digest, Sha256};
use ssh_key::encoding::base64::{Base64, Encoding};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

const READ_LIMIT: u64 = 1024 * 1024;
pub(super) const HASH_LIMIT: u64 = 64 * 1024 * 1024;
pub(super) const HASH_SKIPPED: &str = "文件超过 64 MiB，未计算整文件 SHA-256";
const WRITE_LIMIT: usize = 5 * 1024 * 1024;

/// 已规范化的允许目录；不存在或无效的授权目录不会扩大访问范围。
pub struct Sandbox {
    pub(super) folders: Vec<PathBuf>,
    pub(super) unavailable_folders: Vec<AppError>,
}
impl Sandbox {
    /// 规范化 folders 中已存在的绝对目录，返回只允许这些目录的沙箱。
    pub fn new(folders: &[PathBuf]) -> Self {
        let mut sandbox = Self {
            folders: Vec::new(),
            unavailable_folders: Vec::new(),
        };
        for folder in folders {
            match validate_folder(folder) {
                Ok(folder) => sandbox.folders.push(folder),
                Err(error) => sandbox
                    .unavailable_folders
                    .push(error.with("path", fs_local::display_path(folder))),
            }
        }
        sandbox
    }

    fn contains(&self, path: &Path) -> Result<(), AppError> {
        if self.folders.iter().any(|folder| path_within(path, folder)) {
            Ok(())
        } else {
            Err(
                path_error("mcp.path_outside", path, "路径不在允许的文件夹中").with(
                    "allowed",
                    self.folders
                        .iter()
                        .map(|p| fs_local::display_path(p))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            )
        }
    }
    /// 规范化绝对 path 并检查授权前缀；返回可读路径或 mcp.path_* / mcp.not_found。
    pub fn resolve_read(&self, path: &str) -> Result<PathBuf, AppError> {
        let path = Path::new(path);
        valid_path(path)?;
        let resolved = canonical(path)?;
        self.contains(&resolved)?;
        Ok(resolved)
    }
    /// 规范化 path 的父目录再拼接文件名；已有目标另查链接及类型，返回可替换的普通文件路径。
    pub fn resolve_write(&self, path: &str) -> Result<PathBuf, AppError> {
        let path = Path::new(path);
        valid_path(path)?;
        let name = path
            .file_name()
            .ok_or_else(|| path_error("mcp.path_invalid", path, "缺少文件名"))?;
        let parent = path
            .parent()
            .ok_or_else(|| path_error("mcp.path_invalid", path, "缺少父目录"))?;
        let parent = canonical(parent)?;
        self.contains(&parent)?;
        let target = parent.join(name);
        match fs::symlink_metadata(&target) {
            Ok(_) => {
                let resolved = canonical(&target)?;
                self.contains(&resolved)?;
                if !fs::metadata(&target)
                    .map_err(|e| io_error(e, &target))?
                    .is_file()
                {
                    return Err(path_error(
                        "mcp.path_invalid",
                        path,
                        "只能替换普通文件，不能覆盖目录或设备",
                    ));
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(e, &target)),
        }
        Ok(target)
    }
}
/// 校验待授权的 folder；返回规范化的本机目录，网络盘返回 mcp.network_folder。
/// 保存授权设置前调用；不存在、不可访问或非目录的路径返回带原因的错误。
pub fn validate_folder(folder: &Path) -> Result<PathBuf, AppError> {
    reject_network_folder(folder)?;
    let resolved = canonical(folder)?;
    if !resolved.is_dir() {
        return Err(path_error("mcp.path_invalid", folder, "授权路径必须是目录"));
    }
    Ok(resolved)
}
fn reject_network_folder(path: &Path) -> Result<(), AppError> {
    let text = path.to_string_lossy().replace('\\', "/");
    let lower = text.to_ascii_lowercase();
    let unc = lower.starts_with("//?/unc/")
        || (lower.starts_with("//") && !lower.starts_with("//?/") && !lower.starts_with("//./"));
    #[cfg(windows)]
    let mapped = mapped_drive(path, |root| {
        // 根路径以 NUL 结尾，查询不打开远程文件。
        unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root.as_ptr()) }
    });
    #[cfg(not(windows))]
    let mapped = false;
    if unc || mapped {
        return Err(path_error(
            "mcp.network_folder",
            path,
            "不支持网络盘上的文件夹",
        ));
    }
    Ok(())
}
#[cfg(windows)]
fn mapped_drive(path: &Path, drive_type: impl FnOnce(&[u16]) -> u32) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};
    let drive = match path.components().next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => Some(d),
            _ => None,
        },
        _ => None,
    };
    drive.is_some_and(|d| {
        let root: Vec<u16> = std::ffi::OsStr::new(&format!("{}:\\", char::from(d)))
            .encode_wide()
            .chain(Some(0))
            .collect();
        drive_type(&root) == 4 // DRIVE_REMOTE：映射网络盘。
    })
}
fn path_error(code: &str, path: &Path, reason: &str) -> AppError {
    AppError::new(code)
        .with("path", fs_local::display_path(path))
        .with("reason", reason)
}
fn io_error(error: std::io::Error, path: &Path) -> AppError {
    let (code, reason) = if error.kind() == std::io::ErrorKind::NotFound {
        ("mcp.not_found", "文件或父目录不存在")
    } else {
        ("mcp.path_invalid", "无法访问路径或完成文件操作")
    };
    path_error(code, path, reason)
}
fn canonical(path: &Path) -> Result<PathBuf, AppError> {
    check_links(path, 0)?;
    let resolved = fs_local::plain_canonical(path).map_err(|e| io_error(e, path))?;
    valid_path(&resolved)?;
    Ok(resolved)
}
fn check_links(path: &Path, depth: usize) -> Result<(), AppError> {
    valid_path(path)?;
    if depth > 40 {
        return Err(path_error("mcp.path_invalid", path, "路径链接层级过多"));
    }
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        // 盘符（含 \\?\C: 这种长路径前缀）和根目录本身不是链接，Windows 也不允许单独查它们的属性
        if matches!(
            part,
            std::path::Component::Prefix(_) | std::path::Component::RootDir
        ) {
            continue;
        }
        let metadata = fs::symlink_metadata(&current).map_err(|e| io_error(e, &current))?;
        if metadata.file_type().is_symlink() {
            let link = fs::read_link(&current).map_err(|e| io_error(e, &current))?;
            let target = if link.is_absolute() {
                link
            } else {
                current.parent().unwrap_or(path).join(link)
            };
            check_links(&target, depth + 1)?;
        }
    }
    Ok(())
}
fn path_within(path: &Path, folder: &Path) -> bool {
    if cfg!(windows) {
        Path::new(&path.to_string_lossy().to_lowercase())
            .starts_with(folder.to_string_lossy().to_lowercase())
    } else {
        path.starts_with(folder)
    }
}
fn valid_path(path: &Path) -> Result<(), AppError> {
    reject_network_folder(path)?;
    let invalid = || {
        path_error(
            "mcp.path_invalid",
            path,
            "只允许本机绝对路径，不能含网络路径、设备、保留名、数据流或上级目录",
        )
    };
    let text = path.to_str().ok_or_else(invalid)?;
    let slashes = text.replace('\\', "/");
    let plain = if let Some(tail) = slashes.strip_prefix("//?/") {
        if tail.len() < 3
            || !tail.as_bytes()[0].is_ascii_alphabetic()
            || tail.as_bytes()[1] != b':'
            || tail.as_bytes()[2] != b'/'
        {
            return Err(invalid());
        }
        tail
    } else {
        slashes.as_str()
    };
    if !path.is_absolute()
        || plain.starts_with("//")
        || plain.starts_with("/??/")
        || text.contains('\0')
    {
        return Err(invalid());
    }
    for (index, name) in plain.split('/').enumerate() {
        if name.is_empty() {
            continue;
        }
        if index == 0
            && name.len() == 2
            && name.as_bytes()[0].is_ascii_alphabetic()
            && name.ends_with(':')
        {
            continue;
        }
        fs_local::text::validate_name(name, true).map_err(|_| invalid())?;
    }
    Ok(())
}

/// 一段文件原始字节；encoding 为 utf-8 或 base64，next_offset 可用于继续读取。
/// 超过 64 MiB 时 sha256 为 None，sha256_skipped_reason 说明跳过原因。
#[derive(Debug, Serialize)]
pub struct FileSlice {
    pub content: String,
    pub encoding: &'static str,
    pub offset: u64,
    pub next_offset: u64,
    pub size: u64,
    pub eof: bool,
    pub sha256: Option<String>,
    pub sha256_skipped_reason: Option<&'static str>,
    pub mtime_ms: u64,
}
/// 原子写入的路径和 UTF-8 字节数。
#[derive(Debug, Serialize)]
pub struct FileWritten {
    pub path: PathBuf,
    pub bytes: usize,
    pub sha256: String,
}
/// 目录项包含名称、类型、大小、Unix 秒修改时间和隐藏标记。
#[derive(Debug, Serialize)]
pub struct DirEntryInfo {
    #[serde(flatten)]
    pub entry: fs_local::LocalEntry,
    pub is_dir: bool,
    pub mtime_ms: u64,
}
impl std::ops::Deref for DirEntryInfo {
    type Target = fs_local::LocalEntry;
    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}
pub(super) fn mtime(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis().min(u64::MAX as u128) as u64)
}

/// 从 sb 允许的 path 按 offset 读取最多 max_bytes（上限 1 MiB）字节；非 UTF-8 分段返回 base64。
pub fn read_file(
    sb: &Sandbox,
    path: &str,
    offset: u64,
    max_bytes: u64,
) -> Result<FileSlice, AppError> {
    read_cancelled(sb, path, offset, max_bytes, &AtomicBool::new(false))
}
pub(super) fn read_cancelled(
    sb: &Sandbox,
    path: &str,
    offset: u64,
    max_bytes: u64,
    cancelled: &AtomicBool,
) -> Result<FileSlice, AppError> {
    check_cancel(cancelled)?;
    let path = sb.resolve_read(path)?;
    let mut file = open_file(&path)?;
    let metadata = file.metadata().map_err(|e| io_error(e, &path))?;
    let size = metadata.len();
    let mut digest = FileDigest::new(size);
    if size <= HASH_LIMIT {
        let mut buffer = [0; 32768];
        loop {
            let count = read_chunk(&mut file, &mut buffer, cancelled)?;
            digest.update(&buffer[..count]);
            if count == 0 || digest.skipped() {
                break;
            }
        }
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| io_error(e, &path))?;
    let mut bytes = Vec::new();
    let mut buffer = [0; 32768];
    let limit = max_bytes.min(READ_LIMIT) as usize;
    while bytes.len() < limit {
        let count = read_chunk(
            &mut file,
            &mut buffer[..(limit - bytes.len()).min(32768)],
            cancelled,
        )?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    check_cancel(cancelled)?;
    check_metadata(&file, &metadata)?;
    let sha256 = digest.finish();
    let sha256_skipped_reason = sha256.is_none().then_some(HASH_SKIPPED);
    let next_offset = offset.saturating_add(bytes.len() as u64);
    let (content, encoding) = if binary(&bytes) {
        (Base64::encode_string(&bytes), "base64")
    } else {
        match String::from_utf8(bytes) {
            Ok(text) => (text, "utf-8"),
            Err(e) => (Base64::encode_string(e.as_bytes()), "base64"),
        }
    };
    Ok(FileSlice {
        content,
        encoding,
        offset,
        next_offset,
        size,
        eof: next_offset >= size,
        sha256,
        sha256_skipped_reason,
        mtime_ms: mtime(&metadata),
    })
}

// 每次只读取固定大小的块，取消不必等整个文件读完。
pub(super) fn read_chunk(
    reader: &mut impl Read,
    buffer: &mut [u8],
    cancelled: &AtomicBool,
) -> Result<usize, AppError> {
    check_cancel(cancelled)?;
    let count = reader
        .read(buffer)
        .map_err(|_| AppError::new("mcp.path_invalid").with("reason", "无法读取文件"))?;
    check_cancel(cancelled)?;
    Ok(count)
}
pub(super) fn check_metadata(file: &fs::File, before: &fs::Metadata) -> Result<(), AppError> {
    let after = file
        .metadata()
        .map_err(|_| AppError::new("mcp.path_invalid").with("reason", "无法读取文件属性"))?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(AppError::new("mcp.file_changed")
            .with("reason", "读取期间文件内容发生变化，请重新读取"));
    }
    Ok(())
}
pub(super) struct FileDigest {
    digest: Option<Sha256>,
    bytes: u64,
}
impl FileDigest {
    pub(super) fn new(size: u64) -> Self {
        Self {
            digest: (size <= HASH_LIMIT).then(Sha256::new),
            bytes: 0,
        }
    }
    pub(super) fn skipped(&self) -> bool {
        self.digest.is_none()
    }
    pub(super) fn update(&mut self, bytes: &[u8]) {
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        if self.bytes > HASH_LIMIT {
            self.digest = None;
        }
        if let Some(digest) = &mut self.digest {
            digest.update(bytes);
        }
    }
    pub(super) fn finish(self) -> Option<String> {
        self.digest.map(|d| format!("{:x}", d.finalize()))
    }
}

/// 将 UTF-8 content（最多 5 MiB）原子写入 sb 内的 path；临时文件成功或失败均自动清理，返回路径与字节数。
pub fn write_file(sb: &Sandbox, path: &str, content: &str) -> Result<FileWritten, AppError> {
    write_checked(sb, path, content, None)
}
pub(super) fn write_checked(
    sb: &Sandbox,
    path: &str,
    content: &str,
    expected: Option<&str>,
) -> Result<FileWritten, AppError> {
    if content.len() > WRITE_LIMIT {
        return Err(too_large(WRITE_LIMIT));
    }
    let target = sb.resolve_write(path)?;
    let original = existing(&target)?;
    check_hash(original.as_deref(), expected)?;
    let bytes = match &original {
        Some(bytes) => {
            let doc = Document::decode(bytes)?;
            doc.encode(&doc.replacement(content))?
        }
        None => content.as_bytes().to_vec(),
    };
    atomic_write(sb, path, &target, &bytes, original.as_deref())
}
fn too_large(max: usize) -> AppError {
    AppError::new("mcp.file_too_large")
        .with("max_bytes", max)
        .with("reason", "文件超过大小限制")
}
fn existing(path: &Path) -> Result<Option<Vec<u8>>, AppError> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(e, path)),
        Ok(_) => read_bounded(path, WRITE_LIMIT).map(Some),
    }
}
pub(super) fn open_file(path: &Path) -> Result<fs::File, AppError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|e| io_error(e, path))?;
    if !file.metadata().map_err(|e| io_error(e, path))?.is_file() {
        return Err(path_error("mcp.path_invalid", path, "只支持普通文件"));
    }
    Ok(file)
}
pub(super) fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, AppError> {
    let file = open_file(path)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(e, path))?;
    if bytes.len() > limit {
        return Err(too_large(limit));
    }
    Ok(bytes)
}
fn check_hash(bytes: Option<&[u8]>, expected: Option<&str>) -> Result<(), AppError> {
    if expected.is_some_and(|expected| bytes.is_none_or(|b| hash(b) != expected)) {
        return Err(
            AppError::new("mcp.file_changed").with("reason", "文件在读取后已改变，请重新读取")
        );
    }
    Ok(())
}
pub(super) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn atomic_write(
    sb: &Sandbox,
    path: &str,
    target: &Path,
    bytes: &[u8],
    original: Option<&[u8]>,
) -> Result<FileWritten, AppError> {
    if bytes.len() > WRITE_LIMIT {
        return Err(too_large(WRITE_LIMIT));
    }
    if fs::metadata(target).is_ok_and(|m| m.permissions().readonly()) {
        return Err(path_error("mcp.path_invalid", target, "文件是只读的"));
    }
    let parent = target
        .parent()
        .ok_or_else(|| path_error("mcp.path_invalid", target, "缺少父目录"))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".pierlink-")
        .suffix(".pierlink-part")
        .tempfile_in(parent)
        .map_err(|e| io_error(e, target))?;
    temporary
        .write_all(bytes)
        .map_err(|e| io_error(e, target))?;
    if let Ok(meta) = fs::metadata(target) {
        temporary
            .as_file()
            .set_permissions(meta.permissions())
            .map_err(|e| io_error(e, target))?;
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|e| io_error(e, target))?;
    if sb.resolve_write(path)? != target || existing(target)?.as_deref() != original {
        return Err(AppError::new("mcp.file_changed").with("reason", "写入期间路径或内容发生变化"));
    }
    temporary
        .persist(target)
        .map_err(|e| persist_error(e.error, target))?;
    Ok(FileWritten {
        path: target.to_owned(),
        bytes: bytes.len(),
        sha256: hash(bytes),
    })
}
fn persist_error(error: std::io::Error, target: &Path) -> AppError {
    let os_code = error.raw_os_error();
    // Windows 的共享冲突和锁冲突才表示占用；权限、磁盘等错误不误报为占用。
    #[cfg(windows)]
    if matches!(os_code, Some(32 | 33)) {
        return path_error("mcp.file_busy", target, "文件被别的程序占用")
            .with("os_code", os_code.unwrap_or_default());
    }
    let mut result = io_error(error, target);
    if let Some(code) = os_code {
        result = result.with("os_code", code);
    }
    result
}
#[derive(Debug, Serialize)]
pub(super) struct FileEdited {
    pub path: PathBuf,
    pub replacements: usize,
    pub sha256: String,
}
pub(super) fn edit_file(
    sb: &Sandbox,
    path: &str,
    old: &str,
    new: &str,
    all: bool,
    expected: Option<&str>,
) -> Result<FileEdited, AppError> {
    let target = sb.resolve_write(path)?;
    let bytes = read_bounded(&target, WRITE_LIMIT)?;
    check_hash(Some(&bytes), expected)?;
    let doc = Document::decode(&bytes)?;
    let old = doc.normalize(old);
    if old.is_empty() {
        return Err(AppError::new("mcp.edit_not_found").with("reason", "匹配文字不能为空"));
    }
    let matches = doc.match_ranges(&old);
    let count = matches.len();
    if count == 0 {
        return Err(AppError::new("mcp.edit_not_found").with("reason", "文件中没有找到匹配文字"));
    }
    if count > 1 && !all {
        return Err(AppError::new("mcp.edit_not_unique")
            .with("count", count)
            .with(
                "reason",
                "匹配文字不唯一，请提供更多上下文或使用 replace_all",
            ));
    }
    let mut updated = String::new();
    let mut previous = 0;
    for range in matches {
        let replacement = doc.fragment_replacement(&doc.text[range.clone()], new);
        // 单字节编码转成 UTF-8 最多膨胀三倍，写入前仍检查最终编码大小。
        let expanded = updated
            .len()
            .saturating_add(range.start - previous)
            .saturating_add(replacement.len());
        if expanded > WRITE_LIMIT * 3 {
            return Err(too_large(WRITE_LIMIT));
        }
        updated.push_str(&doc.text[previous..range.start]);
        updated.push_str(&replacement);
        previous = range.end;
    }
    if updated.len().saturating_add(doc.text.len() - previous) > WRITE_LIMIT * 3 {
        return Err(too_large(WRITE_LIMIT));
    }
    updated.push_str(&doc.text[previous..]);
    let encoded = doc.encode(&updated)?;
    let written = atomic_write(sb, path, &target, &encoded, Some(&bytes))?;
    Ok(FileEdited {
        path: written.path,
        replacements: count,
        sha256: written.sha256,
    })
}
#[cfg(test)]
fn read_lines(
    sb: &Sandbox,
    path: &str,
    offset: u64,
    limit: u64,
) -> Result<serde_json::Value, AppError> {
    super::read::lines(
        sb,
        path,
        offset,
        limit,
        &std::sync::atomic::AtomicBool::new(false),
    )
}

/// 列出 sb 允许的 path 中最多 2000 项；不跟随目录项链接，返回名称、类型、大小和修改时间。
pub fn list_dir(sb: &Sandbox, path: &str) -> Result<Vec<DirEntryInfo>, AppError> {
    let path = sb.resolve_read(path)?;
    let mut result = Vec::new();
    for item in fs::read_dir(&path).map_err(|e| io_error(e, &path))? {
        let item = item.map_err(|e| io_error(e, &path))?;
        let entry_path = item.path();
        // 目录内容也先做词法检查，不能借列表读取设备名或数据流的属性。
        if valid_path(&entry_path).is_err() {
            continue;
        }
        let metadata = fs::symlink_metadata(&entry_path).map_err(|e| io_error(e, &entry_path))?;
        let name = item.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let entry = fs_local::LocalEntry {
            name: name.into(),
            kind: fs_local::kind(&metadata),
            size: metadata.len(),
            mtime: metadata.modified().map(fs_local::unix_seconds).unwrap_or(0),
            hidden: name.starts_with('.'),
        };
        result.push(DirEntryInfo {
            is_dir: metadata.is_dir(),
            mtime_ms: mtime(&metadata),
            entry,
        });
        if result.len() == 2000 {
            break;
        }
    }
    Ok(result)
}

/// 根据 path 的扩展名或启动目录返回需要用户确认的中文原因；普通数据文件返回 None。
pub fn write_needs_confirmation(path: &Path) -> Option<String> {
    let path = path.to_string_lossy().replace('\\', "/").to_lowercase();
    if path
        .split('/')
        .any(|part| matches!(part, "startup" | "启动" | "启动项目"))
    {
        return Some("写入启动文件夹会在登录时运行程序".into());
    }
    [
        ".ps1", ".psm1", ".psd1", ".bat", ".cmd", ".vbs", ".vbe", ".js", ".jse", ".wsf", ".wsh",
        ".lnk", ".hta", ".exe", ".com", ".scr", ".sh", ".py", ".url", ".reg",
    ]
    .iter()
    .any(|suffix| path.trim_end_matches([' ', '.']).ends_with(suffix))
    .then(|| "写入脚本、程序或快捷方式可能执行命令".into())
}
/// 判断 path 是否需要写入确认；只用于分类，访问前仍须沙箱校验。
pub fn is_dangerous_path(path: &str) -> bool {
    write_needs_confirmation(Path::new(path)).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 沙箱拒绝跳出设备名数据流并原子读写列目录() {
        let root = crate::test_support::tempdir();
        let allowed = root.path().join("allowed");
        fs::create_dir(&allowed).unwrap();
        let sb = Sandbox::new(std::slice::from_ref(&allowed));
        for path in [
            "//host/share/file",
            "//?/C中文",
            r"\\host\share\file",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1",
            "/??/C:/file",
            "../escape",
            "relative",
            r"\\?\C:\data",
            r"\\.\NUL",
            r"\\?\UNC\host\share",
        ] {
            assert!(sb.resolve_read(path).is_err());
        }
        for name in [
            "../outside",
            "CON",
            "NUL.txt",
            "COM1.log",
            "LPT9",
            "a:b",
            "bad.",
            "bad ",
        ] {
            assert_eq!(
                sb.resolve_write(allowed.join(name).to_str().unwrap())
                    .unwrap_err()
                    .code,
                "mcp.path_invalid"
            );
        }
        assert_eq!(
            sb.resolve_read(root.path().to_str().unwrap())
                .unwrap_err()
                .code,
            "mcp.path_outside"
        );
        assert_eq!(
            sb.resolve_write(root.path().join("outside").to_str().unwrap())
                .unwrap_err()
                .code,
            "mcp.path_outside"
        );
        assert_eq!(
            sb.resolve_read(allowed.join("missing").to_str().unwrap())
                .unwrap_err()
                .code,
            "mcp.not_found"
        );
        let path = allowed.join("file.txt");
        let path = path.to_str().unwrap();
        assert_eq!(write_file(&sb, path, "旧内容").unwrap().bytes, 9);
        write_file(&sb, path, "新内容").unwrap();
        assert_eq!(read_file(&sb, path, 0, 1024).unwrap().content, "新内容");
        assert!(read_file(&sb, path, 100, 1024).unwrap().eof);
        assert_eq!(list_dir(&sb, allowed.to_str().unwrap()).unwrap().len(), 1);
        assert!(write_file(&sb, allowed.to_str().unwrap(), "内容").is_err());
        assert_eq!(
            write_file(&sb, path, &"x".repeat(WRITE_LIMIT + 1))
                .unwrap_err()
                .code,
            "mcp.file_too_large"
        );
    }
    #[test]
    fn 分段限额二进制与危险文件确认() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let path = root.path().join("binary");
        let path = path.to_str().unwrap();
        fs::write(path, vec![255; READ_LIMIT as usize + 3]).unwrap();
        let first = read_file(&sb, path, 0, u64::MAX).unwrap();
        assert_eq!(first.encoding, "base64");
        assert!(!first.eof);
        assert_eq!(first.next_offset, READ_LIMIT);
        let last = read_file(&sb, path, first.next_offset, READ_LIMIT).unwrap();
        assert_eq!(last.content, "////");
        assert!(last.eof);
        for path in [
            "script.PS1",
            "a.cmd",
            "a.lnk",
            "C:/Startup/data.txt",
            "C:/启动/a.txt",
        ] {
            assert!(write_needs_confirmation(Path::new(path)).is_some());
        }
        assert!(!is_dangerous_path("a.txt"));
    }
    #[test]
    fn 目录项最多两千且不返回原子写入临时文件() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        for index in 0..2001 {
            fs::write(root.path().join(format!("entry-{index}")), b"").unwrap();
        }
        let entries = list_dir(&sb, root.path().to_str().unwrap()).unwrap();
        assert_eq!(entries.len(), 2000);
        assert!(
            entries
                .iter()
                .all(|entry| entry.size == 0 && entry.kind == fs_local::EntryKind::File)
        );
    }
    #[cfg(unix)]
    #[test]
    fn 符号链接及父目录链接不能跳出授权范围() {
        use std::os::unix::fs::symlink;
        let root = crate::test_support::tempdir();
        let allowed = root.path().join("allowed");
        let outside = root.path().join("outside");
        fs::create_dir(&allowed).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), "原内容").unwrap();
        symlink(outside.join("secret"), allowed.join("link")).unwrap();
        symlink(&outside, allowed.join("parent")).unwrap();
        let sb = Sandbox::new(std::slice::from_ref(&allowed));
        for name in ["link", "parent/secret", "parent/new"] {
            let path = allowed.join(name);
            let path = path.to_str().unwrap();
            assert_eq!(sb.resolve_write(path).unwrap_err().code, "mcp.path_outside");
            if !name.ends_with("new") {
                assert_eq!(sb.resolve_read(path).unwrap_err().code, "mcp.path_outside");
            }
        }
        assert_eq!(
            fs::read_to_string(outside.join("secret")).unwrap(),
            "原内容"
        );
    }
    #[cfg(windows)]
    #[test]
    fn 目录联接不能跳出授权范围() {
        let root = crate::test_support::tempdir();
        let allowed = root.path().join("allowed");
        let outside = root.path().join("outside");
        fs::create_dir(&allowed).unwrap();
        fs::create_dir(&outside).unwrap();
        let link = allowed.join("junction");
        junction(&link, &outside);
        let sb = Sandbox::new(&[allowed]);
        assert_eq!(
            sb.resolve_read(link.to_str().unwrap()).unwrap_err().code,
            "mcp.path_outside"
        );
        assert_eq!(
            sb.resolve_write(link.join("new").to_str().unwrap())
                .unwrap_err()
                .code,
            "mcp.path_outside"
        );
    }
    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) {
        use std::os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        };
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE, Storage::FileSystem::*, System::IO::DeviceIoControl,
        };
        fs::create_dir(link).unwrap();
        let target = fs_local::plain_canonical(target).unwrap();
        let substitute: Vec<_> = format!(r"\??\{}", target.display())
            .encode_utf16()
            .collect();
        let print: Vec<_> = target.as_os_str().encode_wide().collect();
        let mut data = Vec::new();
        data.extend_from_slice(&0xa0000003u32.to_le_bytes());
        data.extend_from_slice(
            &(8u16 + ((substitute.len() + print.len() + 2) * 2) as u16).to_le_bytes(),
        );
        data.extend_from_slice(&0u16.to_le_bytes());
        for value in [
            0,
            (substitute.len() * 2) as u16,
            ((substitute.len() + 1) * 2) as u16,
            (print.len() * 2) as u16,
        ] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        for value in substitute.into_iter().chain([0]).chain(print).chain([0]) {
            data.extend_from_slice(&value.to_le_bytes());
        }
        let wide: Vec<_> = link.as_os_str().encode_wide().chain([0]).collect();
        unsafe {
            let handle = CreateFileW(
                wide.as_ptr(),
                0x40000000,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            );
            assert_ne!(handle, INVALID_HANDLE_VALUE);
            let file = std::fs::File::from_raw_handle(handle);
            let mut written = 0;
            assert_ne!(
                DeviceIoControl(
                    file.as_raw_handle(),
                    0x000900a4,
                    data.as_ptr().cast(),
                    data.len() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut written,
                    std::ptr::null_mut()
                ),
                0
            );
        }
    }
}

#[cfg(test)]
mod editing_tests {
    use super::*;
    #[test]
    fn 混合换行按归一化匹配并逐处保留原分隔符() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let path = root.path().join("mixed.txt");
        let path = path.to_str().unwrap();
        for utf16 in [false, true] {
            let text = "前\r\n甲\r\n乙\n丙\n中\n甲\n乙\r\n丙\n后\r\n";
            let bytes = if utf16 {
                [
                    vec![0xff, 0xfe],
                    text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
                ]
                .concat()
            } else {
                text.as_bytes().to_vec()
            };
            fs::write(path, &bytes).unwrap();
            let error =
                edit_file(&sb, path, "甲\n乙\n丙", "新甲\n新乙\n新丙", false, None).unwrap_err();
            assert_eq!(error.code, "mcp.edit_not_unique");
            assert_eq!(error.params["count"], "2");
            assert_eq!(fs::read(path).unwrap(), bytes);
            let edited =
                edit_file(&sb, path, "甲\n乙\n丙", "新甲\n新乙\n新丙", true, None).unwrap();
            assert_eq!(edited.replacements, 2);
            let result = Document::decode(&fs::read(path).unwrap()).unwrap();
            assert_eq!(
                result.text,
                "前\r\n新甲\r\n新乙\n新丙\n中\n新甲\n新乙\r\n新丙\n后\r\n"
            );
            assert_eq!(result.name(), if utf16 { "utf-16le" } else { "utf-8" });
        }
        fs::write(path, "a\r\nb\nx\na\nb\n").unwrap();
        assert_eq!(
            edit_file(&sb, path, "a\nb", "c\nd", false, None)
                .unwrap()
                .replacements,
            1
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "a\r\nb\nx\nc\nd\n");
    }
    #[test]
    fn 授权校验保留不可用目录原因且不扩大边界() {
        let root = crate::test_support::tempdir();
        assert_eq!(
            validate_folder(root.path()).unwrap(),
            fs_local::plain_canonical(root.path()).unwrap()
        );
        let missing = root.path().join("missing/child");
        let file = root.path().join("file");
        fs::write(&file, "内容").unwrap();
        assert_eq!(validate_folder(&missing).unwrap_err().code, "mcp.not_found");
        assert_eq!(validate_folder(&file).unwrap_err().code, "mcp.path_invalid");
        let sb = Sandbox::new(&[
            root.path().into(),
            missing.clone(),
            r"\\server\share\folder".into(),
        ]);
        assert_eq!(sb.folders.len(), 1);
        assert_eq!(sb.unavailable_folders.len(), 2);
        assert_eq!(
            sb.unavailable_folders[0].params["path"],
            fs_local::display_path(&missing)
        );
        assert_eq!(sb.unavailable_folders[1].code, "mcp.network_folder");
    }
    #[test]
    fn 超大文件按字节不计算散列且读写限制明确() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let path = root.path().join("large");
        let path = path.to_str().unwrap();
        let file = fs::File::create(path).unwrap();
        file.set_len(HASH_LIMIT + 1).unwrap();
        drop(file);
        let read = read_file(&sb, path, HASH_LIMIT, 1).unwrap();
        assert!(read.sha256.is_none());
        assert_eq!(read.sha256_skipped_reason, Some(HASH_SKIPPED));
        assert!(read.eof);
        assert_eq!(
            write_file(&sb, path, "新内容").unwrap_err().code,
            "mcp.file_too_large"
        );
        assert_eq!(
            read_cancelled(&sb, path, 0, 1, &AtomicBool::new(true))
                .unwrap_err()
                .code,
            "mcp.cancelled"
        );
        fs::write(path, b"a\0b").unwrap();
        assert_eq!(
            write_file(&sb, path, "新内容").unwrap_err().code,
            "mcp.binary_file"
        );
        let read = read_file(&sb, path, 0, 1).unwrap();
        assert_eq!(read.sha256, Some(hash(b"a\0b")));
        assert!(read.sha256_skipped_reason.is_none());
        let tools = super::super::definitions();
        let description = tools
            .iter()
            .find(|t| t.name == "write_file")
            .unwrap()
            .description
            .as_deref()
            .unwrap();
        assert!(description.contains("不能覆盖已有二进制文件或超过 5 MiB 的已有文件"));
    }
    #[cfg(windows)]
    #[test]
    fn 文件占用时返回系统码且保留原文并清理临时文件() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let path = root.path().join("busy.txt");
        fs::write(&path, "原文").unwrap();
        // 允许读取原文，禁止删除共享，使最终原子替换失败。
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        let error = write_file(&sb, path.to_str().unwrap(), "替换").unwrap_err();
        assert_eq!(error.code, "mcp.file_busy");
        assert_eq!(error.params["reason"], "文件被别的程序占用");
        assert_eq!(error.params["os_code"], "32");
        assert_eq!(fs::read_to_string(&path).unwrap(), "原文");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        drop(held);
        assert_ne!(
            persist_error(std::io::Error::from_raw_os_error(5), &path).code,
            "mcp.file_busy"
        );
        assert_eq!(
            persist_error(std::io::Error::from_raw_os_error(33), &path).code,
            "mcp.file_busy"
        );
    }
    #[cfg(windows)]
    #[test]
    fn 网络路径拒绝且已有映射盘保留不可用原因() {
        for path in [r"Z:\project", r"\\?\Z:\project"] {
            assert!(mapped_drive(Path::new(path), |root| {
                assert_eq!(root, &[b'Z' as u16, b':' as u16, b'\\' as u16, 0]);
                4
            }));
            assert!(!mapped_drive(Path::new(path), |_| 3));
        }

        for path in [
            r"\\server\share\folder",
            r"\\?\UNC\server\share\folder",
            "//server/share/folder",
        ] {
            assert_eq!(
                validate_folder(Path::new(path)).unwrap_err().code,
                "mcp.network_folder"
            );
        }
        // CI 无需网络共享；如运行机有映射盘，连同断开的映射盘一起检查。
        for drive in b'A'..=b'Z' {
            let path = PathBuf::from(format!("{}:\\", char::from(drive)));
            if reject_network_folder(&path).is_err() {
                assert_eq!(
                    validate_folder(&path).unwrap_err().code,
                    "mcp.network_folder"
                );
                let sb = Sandbox::new(&[path]);
                assert!(sb.folders.is_empty());
                assert_eq!(sb.unavailable_folders[0].code, "mcp.network_folder");
            }
        }
    }
    #[test]
    fn 编辑匹配次数散列保护与原子失败() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let path = root.path().join("file.txt");
        let path = path.to_str().unwrap();
        write_file(&sb, path, "old\r\nold\r\n").unwrap();
        let sha = hash(&fs::read(path).unwrap());
        assert_eq!(
            edit_file(&sb, path, "missing", "new", false, None)
                .unwrap_err()
                .code,
            "mcp.edit_not_found"
        );
        let error = edit_file(&sb, path, "old", "new", false, None).unwrap_err();
        assert_eq!(error.code, "mcp.edit_not_unique");
        assert_eq!(error.params["count"], "2");
        assert_eq!(
            edit_file(&sb, path, "old", "new", true, Some("stale"))
                .unwrap_err()
                .code,
            "mcp.file_changed"
        );
        assert_eq!(
            write_checked(&sb, path, "new", Some("stale"))
                .unwrap_err()
                .code,
            "mcp.file_changed"
        );
        let result = edit_file(&sb, path, "old\n", "new\n", true, Some(&sha)).unwrap();
        assert_eq!(result.replacements, 2);
        assert_eq!(fs::read(path).unwrap(), b"new\r\nnew\r\n");
        assert_eq!(result.sha256, hash(&fs::read(path).unwrap()));
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
    #[test]
    fn 读写保留编码换行且按行读取带完整散列() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let path = root.path().join("file.txt");
        let path = path.to_str().unwrap();
        for bytes in [
            b"\xef\xbb\xbfhello\r\nnext\r\n".to_vec(),
            [
                vec![0xff, 0xfe],
                "hello\r\nnext\r\n"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            ]
            .concat(),
            b"hello\r\nnext\n".to_vec(),
        ] {
            fs::write(path, &bytes).unwrap();
            let read = read_lines(&sb, path, 2, 1).unwrap();
            assert_eq!(read["total_lines"], 2);
            assert_eq!(read["sha256"], hash(&bytes));
            assert_eq!(read["content"], "     2\tnext\n");
            edit_file(&sb, path, "hello", "world", false, None).unwrap();
            let expected = Document::decode(&bytes).unwrap();
            let actual = Document::decode(&fs::read(path).unwrap()).unwrap();
            assert_eq!(expected.name(), actual.name());
            assert_eq!(expected.line_ending(), actual.line_ending());
            write_checked(&sb, path, "hello\nnext\n", None).unwrap();
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        fs::write(path, "长".repeat(2001)).unwrap();
        assert!(
            read_lines(&sb, path, 1, 1).unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("行已截断")
        );
        fs::write(path, b"a\0b").unwrap();
        assert_eq!(read_lines(&sb, path, 1, 1).unwrap()["encoding"], "base64");
    }
    #[cfg(any(not(windows), test))]
    #[test]
    fn 无法编码时拒绝写入且原文件不变() {
        #[cfg(windows)]
        if unsafe { windows_sys::Win32::Globalization::GetACP() } != 936 {
            return;
        }
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let path = root.path().join("gbk.txt");
        let path = path.to_str().unwrap();
        let bytes = encoding_rs::GBK.encode("中文\r\n").0.into_owned();
        fs::write(path, &bytes).unwrap();
        assert_eq!(
            edit_file(&sb, path, "中文", "😀", false, None)
                .unwrap_err()
                .code,
            "mcp.encoding_unsupported"
        );
        assert_eq!(
            write_checked(&sb, path, "😀", None).unwrap_err().code,
            "mcp.encoding_unsupported"
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    #[cfg(windows)]
    #[test]
    fn 长路径前缀和大小写使用相同授权边界() {
        let root = crate::test_support::tempdir();
        let allowed = root.path().join("Folder");
        fs::create_dir(&allowed).unwrap();
        fs::write(allowed.join("File.txt"), "内容").unwrap();
        let sb = Sandbox::new(&[allowed.to_string_lossy().to_uppercase().into()]);
        let path = allowed.join("file.TXT");
        let long = format!(
            r"\\?\{}",
            crate::fs_local::plain_canonical(&allowed)
                .unwrap()
                .join("File.txt")
                .display()
        );
        assert!(sb.resolve_read(path.to_str().unwrap()).is_ok());
        assert!(sb.resolve_read(&long).is_ok());
    }
    #[test]
    fn 全部替换先限制展开量且失败不改变文件() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let path = root.path().join("file.txt");
        let path = path.to_str().unwrap();
        fs::write(path, "x".repeat(5000)).unwrap();
        assert_eq!(
            edit_file(&sb, path, "x", &"y".repeat(5000), true, None)
                .unwrap_err()
                .code,
            "mcp.file_too_large"
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "x".repeat(5000));
    }
}
