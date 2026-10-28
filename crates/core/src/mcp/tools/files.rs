//! 仅允许访问授权目录中的普通路径；写入以同目录临时文件原子替换。
use super::encoding::{Document, binary};
use crate::{error::AppError, fs_local};
use serde::Serialize;
use sha2::{Digest, Sha256};
use ssh_key::encoding::base64::{Base64, Encoding};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const READ_LIMIT: u64 = 1024 * 1024;
const WRITE_LIMIT: usize = 5 * 1024 * 1024;

/// 已规范化的允许目录；不存在或无效的授权目录不会扩大访问范围。
pub struct Sandbox {
    pub(super) folders: Vec<PathBuf>,
}
impl Sandbox {
    /// 规范化 folders 中已存在的绝对目录，返回只允许这些目录的沙箱。
    pub fn new(folders: &[PathBuf]) -> Self {
        Self {
            folders: folders
                .iter()
                .filter(|p| valid_path(p).is_ok())
                .filter_map(|p| canonical(p).ok())
                .filter(|p| p.is_dir())
                .collect(),
        }
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
#[derive(Debug, Serialize)]
pub struct FileSlice {
    pub content: String,
    pub encoding: &'static str,
    pub offset: u64,
    pub next_offset: u64,
    pub size: u64,
    pub eof: bool,
    pub sha256: String,
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
    let path = sb.resolve_read(path)?;
    if !fs::metadata(&path)
        .map_err(|e| io_error(e, &path))?
        .is_file()
    {
        return Err(path_error("mcp.path_invalid", &path, "只支持读取普通文件"));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(&path).map_err(|e| io_error(e, &path))?;
    let metadata = file.metadata().map_err(|e| io_error(e, &path))?;
    if !metadata.is_file() {
        return Err(path_error("mcp.path_invalid", &path, "只支持读取普通文件"));
    }
    let size = metadata.len();
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest).map_err(|e| io_error(e, &path))?;
    let sha256 = format!("{:x}", digest.finalize());
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| io_error(e, &path))?;
    let mut bytes = Vec::new();
    file.take(max_bytes.min(READ_LIMIT))
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(e, &path))?;
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
        mtime_ms: mtime(&metadata),
    })
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
        .map_err(|e| io_error(e.error, target))?;
    Ok(FileWritten {
        path: target.to_owned(),
        bytes: bytes.len(),
        sha256: hash(bytes),
    })
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
    let count = doc.text.matches(&old).count();
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
    let replacement = doc.fragment_replacement(&old, new);
    // 先计算展开量，防止 replace_all 在检查最终编码大小前分配巨量内存。
    // 单字节编码转成 UTF-8 最多膨胀三倍，最终写入仍限制为 5 MiB。
    let expanded = doc
        .text
        .len()
        .saturating_sub(count.saturating_mul(old.len()))
        .saturating_add(count.saturating_mul(replacement.len()));
    if expanded > WRITE_LIMIT * 3 {
        return Err(too_large(WRITE_LIMIT));
    }
    let updated = doc.text.replace(&old, &replacement);
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
