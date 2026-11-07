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

/// Pierlink 自己的目录（数据目录、安装目录）：就算在授权文件夹里，文件工具也不读不写，
/// 免得 AI 改 Pierlink 的设置和数据库、换掉待安装的更新包或程序文件、读到别的服务器的草稿。
/// 宿主启动时设置一次，保存规范化后的路径。
static PROTECTED: std::sync::RwLock<Vec<PathBuf>> = std::sync::RwLock::new(Vec::new());

/// 测试里改受保护目录（全局）时先拿这把锁，免得并行的测试互相覆盖
#[cfg(test)]
pub(super) static PROTECTED_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 设置文件工具不碰的目录（宿主启动时调用）；不存在或无法规范化的跳过。
///
/// :param dirs: Pierlink 的数据目录、安装目录
pub fn set_protected_dirs(dirs: &[PathBuf]) {
    let list: Vec<PathBuf> = dirs.iter().filter_map(|d| canonical(d).ok()).collect();
    *PROTECTED.write().unwrap_or_else(|p| p.into_inner()) = list;
}

/// path 是否落在 Pierlink 自己的目录里（含目录本身）。path 能规范化时按规范化后的路径判断，
/// 给设置页检查用户新加的授权文件夹用；沙箱内部拿到的已是规范化的路径，用 within_protected。
///
/// :param path: 绝对路径
/// :return: 是否受保护
pub fn is_protected(path: &Path) -> bool {
    let resolved = canonical(path).unwrap_or_else(|_| path.to_path_buf());
    within_protected(&resolved) || within_protected(path)
}

/// 已规范化的 path 是否落在受保护的目录里（不碰文件系统）
pub(super) fn within_protected(path: &Path) -> bool {
    PROTECTED
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|dir| path_within(path, dir))
}

pub(super) fn protected_error(path: &Path) -> AppError {
    path_error(
        "mcp.path_protected",
        path,
        "mcp.pierlink_s_own_folders_are_off_limits",
        "Pierlink 自己的文件夹（数据、安装目录）不让 AI 读写",
    )
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
                Ok(folder) if within_protected(&folder) => {
                    sandbox.unavailable_folders.push(protected_error(&folder))
                }
                Ok(folder) => sandbox.folders.push(folder),
                Err(error) => sandbox
                    .unavailable_folders
                    .push(error.with("path", fs_local::display_path(folder))),
            }
        }
        sandbox
    }

    fn contains(&self, path: &Path) -> Result<(), AppError> {
        if within_protected(path) {
            return Err(protected_error(path));
        }
        if self.folders.iter().any(|folder| path_within(path, folder)) {
            Ok(())
        } else {
            Err(path_error(
                "mcp.path_outside",
                path,
                "mcp.path_is_outside_the_allowed_folders",
                "路径不在允许的文件夹中",
            )
            .with(
                "allowed",
                self.folders
                    .iter()
                    .map(|p| fs_local::display_path(p))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ))
        }
    }
    /// 规范化绝对 path 并检查授权前缀；返回可读路径或带原因的路径、网络盘、占用、缺失错误。
    pub fn resolve_read(&self, path: &str) -> Result<PathBuf, AppError> {
        let path = Path::new(path);
        reject_network_folder(path)?;
        valid_path(path)?;
        let resolved = canonical(path)?;
        self.contains(&resolved)?;
        Ok(resolved)
    }
    /// 校验 path 及最近存在的父目录，返回可替换的普通文件路径；不创建目录、不跟随链接。
    pub fn resolve_write(&self, path: &str) -> Result<PathBuf, AppError> {
        let target = self.resolve_entry(path)?;
        if let Some(metadata) = metadata_if_exists(&target)?
            && (!metadata.is_file() || is_link(&metadata))
        {
            return Err(path_error(
                if is_link(&metadata) {
                    "mcp.path_outside"
                } else {
                    "mcp.path_invalid"
                },
                &target,
                "mcp.only_regular_files_can_be_replaced",
                "只能替换普通文件，不能覆盖目录、链接或设备",
            ));
        }
        Ok(target)
    }

    // 只解析父目录，末级可以是链接；删除或重命名时操作链接本身。
    pub(super) fn resolve_entry(&self, path: &str) -> Result<PathBuf, AppError> {
        let path = Path::new(path);
        reject_network_folder(path)?;
        valid_path(path)?;
        // 授权目录本身也在范围内；检查它自身，不能因为父目录未授权而误拒绝。
        if self
            .folders
            .iter()
            .any(|folder| path_within(path, folder) && path_within(folder, path))
        {
            return self.resolve_directory(path);
        }
        let parent = path
            .parent()
            .ok_or_else(|| io_error(std::io::ErrorKind::NotFound.into(), path))?;
        let name = path.file_name().ok_or_else(|| {
            path_error(
                "mcp.path_invalid",
                path,
                "mcp.filename_is_missing",
                "缺少文件名",
            )
        })?;
        let target = self.resolve_directory(parent)?.join(name);
        self.contains(&target)?;
        Ok(target)
    }

    // 从最近存在的祖先规范化，缺失部分只拼接；审批前调用不会产生文件系统改动。
    pub(super) fn resolve_directory(&self, path: &Path) -> Result<PathBuf, AppError> {
        reject_network_folder(path)?;
        valid_path(path)?;
        let mut ancestor = path;
        while metadata_if_exists(ancestor)?.is_none() {
            ancestor = ancestor
                .parent()
                .ok_or_else(|| io_error(std::io::ErrorKind::NotFound.into(), path))?;
        }
        for part in ancestor.ancestors() {
            if metadata_if_exists(part)?.is_some_and(|m| is_link(&m)) {
                return Err(path_error(
                    "mcp.path_outside",
                    part,
                    "mcp.path_links_are_not_allowed",
                    "写入路径不能经过符号链接或目录联接",
                ));
            }
        }
        let resolved = canonical(ancestor)?;
        self.contains(&resolved)?;
        if !resolved.is_dir() {
            return Err(path_error(
                "mcp.path_invalid",
                path,
                "mcp.parent_is_not_a_directory",
                "父路径不是目录",
            ));
        }
        let suffix = path
            .strip_prefix(ancestor)
            .map_err(|_| io_error(std::io::ErrorKind::InvalidInput.into(), path))?;
        Ok(resolved.join(suffix))
    }
}

pub(super) fn is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// 删除前的检查（审批前调用，免得用户点了允许才失败）：授权文件夹本身不能删；
/// 非空文件夹要 recursive。target 是 resolve_entry 的结果。
pub(super) fn check_delete(sb: &Sandbox, target: &Path, recursive: bool) -> Result<(), AppError> {
    check_move(sb, target)?;
    let metadata = fs::symlink_metadata(target).map_err(|e| io_error(e, target))?;
    if !recursive
        && metadata.is_dir()
        && !is_link(&metadata)
        && fs::read_dir(target)
            .map_err(|e| io_error(e, target))?
            .next()
            .is_some()
    {
        return Err(path_error(
            "mcp.path_invalid",
            target,
            "mcp.folder_is_not_empty_set_recursive",
            "文件夹不是空的；要连同里面的内容一起删除，请设 recursive: true",
        ));
    }
    Ok(())
}

/// 删除、移走前的检查：授权文件夹本身不能删、不能移走（移走后授权就失效了）
pub(super) fn check_move(sb: &Sandbox, target: &Path) -> Result<(), AppError> {
    if sb
        .folders
        .iter()
        .any(|folder| path_within(target, folder) && path_within(folder, target))
    {
        return Err(path_error(
            "mcp.path_invalid",
            target,
            "mcp.allowed_folder_itself_cannot_be_deleted_or_moved",
            "不能删除或移动授权文件夹本身",
        ));
    }
    Ok(())
}
pub(super) fn metadata_if_exists(path: &Path) -> Result<Option<fs::Metadata>, AppError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(e, path)),
    }
}

/// 在 sb 授权范围内创建 path 及父目录；已有目录成功，链接和非目录返回错误。
pub fn make_dir(sb: &Sandbox, path: &str) -> Result<PathBuf, AppError> {
    let target = sb.resolve_directory(Path::new(path))?;
    fs::create_dir_all(&target).map_err(|e| io_error(e, &target))?;
    sb.resolve_directory(Path::new(path))
}

/// 删除 sb 内的 path；recursive 允许删除非空目录，末级链接仅删除链接本身，返回原路径。
pub fn delete_path(sb: &Sandbox, path: &str, recursive: bool) -> Result<PathBuf, AppError> {
    let target = sb.resolve_entry(path)?;
    check_delete(sb, &target, recursive)?;
    let metadata = fs::symlink_metadata(&target).map_err(|e| io_error(e, &target))?;
    let result = if is_link(&metadata) {
        remove_link(&target, &metadata)
    } else if metadata.is_dir() {
        if recursive {
            fs::remove_dir_all(&target)
        } else {
            fs::remove_dir(&target)
        }
    } else {
        fs::remove_file(&target)
    };
    result.map_err(|e| io_error(e, &target))?;
    Ok(target)
}
fn remove_link(path: &Path, metadata: &fs::Metadata) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if metadata.file_type().is_symlink_dir() {
            return fs::remove_dir(path);
        }
    }
    let _ = metadata;
    fs::remove_file(path)
}

/// 将 sb 内的 from 重命名到 to；overwrite 为假时拒绝已有目标，链接仅移动链接本身，返回目标路径。
pub fn rename_path(
    sb: &Sandbox,
    from: &str,
    to: &str,
    overwrite: bool,
) -> Result<PathBuf, AppError> {
    let source = sb.resolve_entry(from)?;
    let target = sb.resolve_entry(to)?;
    check_move(sb, &source)?;
    fs::symlink_metadata(&source).map_err(|e| io_error(e, &source))?;
    if !overwrite && metadata_if_exists(&target)?.is_some() {
        return Err(path_error(
            "mcp.path_exists",
            &target,
            "mcp.rename_target_exists",
            "目标已存在，请选择其他路径或设置 overwrite",
        ));
    }
    if overwrite {
        fs::rename(&source, &target).map_err(|e| io_error(e, &target))?;
    } else {
        // 使用现有排他重命名，检查之后新出现的目标也不能覆盖。
        fs_local::rename(&source, &target).map_err(|error| {
            if error.code == "fs.exists" {
                path_error(
                    "mcp.path_exists",
                    &target,
                    "mcp.rename_target_exists",
                    "目标已存在，请选择其他路径或设置 overwrite",
                )
            } else {
                path_error(
                    "mcp.path_invalid",
                    &target,
                    "mcp.file_operation",
                    "无法访问路径或完成文件操作",
                )
            }
        })?;
    }
    Ok(target)
}
/// 校验待授权的 folder；返回规范化的本机目录，网络盘返回 mcp.network_folder。
/// 保存授权设置前调用；不存在、不可访问或非目录的路径返回带原因的错误。
pub fn validate_folder(folder: &Path) -> Result<PathBuf, AppError> {
    reject_network_folder(folder)?;
    let resolved = canonical(folder)?;
    if !resolved.is_dir() {
        return Err(path_error(
            "mcp.path_invalid",
            folder,
            "mcp.allowed_path_must_be_a_directory",
            "授权路径必须是目录",
        ));
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
            "mcp.folders_on_network_drives_are_not_supported",
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
pub(super) fn path_error(
    code: &str,
    path: &Path,
    text_key: &'static str,
    reason: &str,
) -> AppError {
    AppError::new(code)
        .with("path", fs_local::display_path(path))
        .with_reason(text_key, reason)
}
pub(super) fn io_error(error: std::io::Error, path: &Path) -> AppError {
    #[cfg(windows)]
    if let Some(code @ (32 | 33)) = error.raw_os_error() {
        return path_error(
            "mcp.file_busy",
            path,
            "mcp.file_is_in_use_by_another_program",
            "文件被别的程序占用",
        )
        .with("os_code", code);
    }
    let (code, text_key, reason) = if error.kind() == std::io::ErrorKind::NotFound {
        ("mcp.not_found", "mcp.path_missing", "文件或父目录不存在")
    } else {
        (
            "mcp.path_invalid",
            "mcp.file_operation",
            "无法访问路径或完成文件操作",
        )
    };
    path_error(code, path, text_key, reason)
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
        return Err(path_error(
            "mcp.path_invalid",
            path,
            "mcp.too_many_levels_of_symbolic_links",
            "路径链接层级过多",
        ));
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
pub(super) fn path_within(path: &Path, folder: &Path) -> bool {
    if cfg!(windows) {
        Path::new(&path.to_string_lossy().to_lowercase())
            .starts_with(folder.to_string_lossy().to_lowercase())
    } else {
        path.starts_with(folder)
    }
}
pub(super) fn valid_path(path: &Path) -> Result<(), AppError> {
    let invalid = || {
        path_error(
            "mcp.path_invalid",
            path,
            "mcp.use_a_local_absolute_path_without_network_paths_devices_reserved",
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
            let count = read_chunk(&mut file, &mut buffer, &path, cancelled)?;
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
            &path,
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
    path: &Path,
    cancelled: &AtomicBool,
) -> Result<usize, AppError> {
    loop {
        check_cancel(cancelled)?;
        match reader.read(buffer) {
            Ok(count) => {
                check_cancel(cancelled)?;
                return Ok(count);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(error, path)),
        }
    }
}
pub(super) fn check_metadata(file: &fs::File, before: &fs::Metadata) -> Result<(), AppError> {
    let after = file.metadata().map_err(|_| {
        AppError::new("mcp.path_invalid")
            .with_reason("mcp.couldn_t_read_file_properties", "无法读取文件属性")
    })?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(AppError::new("mcp.file_changed").with_reason(
            "mcp.file_changed_while_reading_read_it_again",
            "读取期间文件内容发生变化，请重新读取",
        ));
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
    make_parent(sb, &target)?;
    atomic_write(sb, path, &target, &bytes, original.as_deref())
}

/// 把 bytes 原样（不转编码、不改换行）原子写入 sb 内的 path，最多 5 MiB：可以新建文件，也可以覆盖已有的普通文件
/// （含二进制文件），已有文件超过 5 MiB 时不覆盖。expected 给出时已有内容的 sha256 要相符；缺少的父目录自动创建。
///
/// :param sb: 授权文件夹
/// :param path: 文件路径（Windows 绝对路径）
/// :param bytes: 要写的内容
/// :param expected: 已有内容应有的 sha256
/// :return: 路径、字节数和 sha256
pub(super) fn write_bytes(
    sb: &Sandbox,
    path: &str,
    bytes: &[u8],
    expected: Option<&str>,
) -> Result<FileWritten, AppError> {
    if bytes.len() > WRITE_LIMIT {
        return Err(too_large(WRITE_LIMIT));
    }
    let target = sb.resolve_write(path)?;
    let original = existing(&target)?;
    check_hash(original.as_deref(), expected)?;
    make_parent(sb, &target)?;
    atomic_write(sb, path, &target, bytes, original.as_deref())
}

/// 建 target 缺少的父目录（在授权范围内）
fn make_parent(sb: &Sandbox, target: &Path) -> Result<(), AppError> {
    if let Some(parent) = target.parent() {
        make_dir(
            sb,
            parent
                .to_str()
                .ok_or_else(|| io_error(std::io::ErrorKind::InvalidInput.into(), parent))?,
        )?;
    }
    Ok(())
}
fn too_large(max: usize) -> AppError {
    AppError::new("mcp.file_too_large")
        .with("max_bytes", max)
        .with_reason("mcp.file_exceeds_the_size_limit", "文件超过大小限制")
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
        return Err(path_error(
            "mcp.path_invalid",
            path,
            "mcp.only_regular_files_are_supported",
            "只支持普通文件",
        ));
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
        return Err(AppError::new("mcp.file_changed").with_reason(
            "mcp.file_changed_since_it_was_read_read_it_again",
            "文件在读取后已改变，请重新读取",
        ));
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
        return Err(path_error(
            "mcp.path_invalid",
            target,
            "mcp.file_is_read_only",
            "文件是只读的",
        ));
    }
    let parent = target.parent().ok_or_else(|| {
        path_error(
            "mcp.path_invalid",
            target,
            "mcp.parent_directory_is_missing",
            "缺少父目录",
        )
    })?;
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
        return Err(AppError::new("mcp.file_changed").with_reason(
            "mcp.path_or_content_changed_while_writing",
            "写入期间路径或内容发生变化",
        ));
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
    // Windows 原子替换也会把缺少删除共享报成拒绝访问，探测后才能区分占用与权限不足。
    #[cfg(windows)]
    if os_code == Some(5) && target_is_busy(target) {
        return path_error(
            "mcp.file_busy",
            target,
            "mcp.file_is_in_use_by_another_program",
            "文件被别的程序占用",
        )
        .with("os_code", 5);
    }
    let mut result = io_error(error, target);
    if let Some(code) = os_code {
        result = result.with("os_code", code);
    }
    result
}
#[cfg(windows)]
fn target_is_busy(target: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    fs::OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(target)
        .is_err_and(|error| error.raw_os_error() == Some(32))
}
#[derive(Debug, Serialize)]
pub(super) struct FileEdited {
    pub path: PathBuf,
    pub replacements: usize,
    pub sha256: String,
}
/// 一处修改：old 换成 new，all 为真时换掉所有匹配
#[derive(Debug, Clone, Copy)]
pub(super) struct Edit<'a> {
    pub old: &'a str,
    pub new: &'a str,
    pub all: bool,
}

pub(super) fn edit_file(
    sb: &Sandbox,
    path: &str,
    old: &str,
    new: &str,
    all: bool,
    expected: Option<&str>,
) -> Result<FileEdited, AppError> {
    edit_file_many(sb, path, &[Edit { old, new, all }], false, expected)
}

/// 按顺序做 edits 里的每处修改（后一处在前一处改完的内容上找），全部成功才写入；任何一处找不到或不唯一，
/// 文件都不改。保持原编码、BOM 和换行。
///
/// :param sb: 授权文件夹
/// :param path: 文件路径（Windows 绝对路径）
/// :param edits: 要做的修改
/// :param numbered: 为真时（edits 参数）错误原因里写明是第几处
/// :param expected: 修改前内容应有的 sha256
/// :return: 路径、总共替换的次数和写入后的 sha256
pub(super) fn edit_file_many(
    sb: &Sandbox,
    path: &str,
    edits: &[Edit<'_>],
    numbered: bool,
    expected: Option<&str>,
) -> Result<FileEdited, AppError> {
    let target = sb.resolve_write(path)?;
    let bytes = read_bounded(&target, WRITE_LIMIT)?;
    check_hash(Some(&bytes), expected)?;
    let mut doc = Document::decode(&bytes)?;
    let mut total = 0;
    for (index, edit) in edits.iter().enumerate() {
        let number = numbered.then_some(index + 1);
        let (updated, count) = apply_edit(&doc, edit, number)?;
        doc.text = updated;
        total += count;
    }
    let encoded = doc.encode(&doc.text)?;
    let written = atomic_write(sb, path, &target, &encoded, Some(&bytes))?;
    Ok(FileEdited {
        path: written.path,
        replacements: total,
        sha256: written.sha256,
    })
}

/// 在 doc 上做一处修改，返回改好的全文和替换次数；number 是第几处（多处修改时写进错误原因）
fn apply_edit(
    doc: &Document,
    edit: &Edit<'_>,
    number: Option<usize>,
) -> Result<(String, usize), AppError> {
    let old = doc.normalize(edit.old);
    if old.is_empty() {
        let error = AppError::new("mcp.edit_not_found");
        return Err(match number {
            Some(n) => error.with("edit", n).with_reason(
                "mcp.edit_n_matching_text_cannot_be_empty",
                format!("第 {n} 处修改：匹配文字不能为空，文件没有改动"),
            ),
            None => error.with_reason("mcp.matching_text_cannot_be_empty", "匹配文字不能为空"),
        });
    }
    let matches = doc.match_ranges(&old);
    let count = matches.len();
    if count == 0 {
        let error = AppError::new("mcp.edit_not_found");
        return Err(match number {
            Some(n) => error.with("edit", n).with_reason(
                "mcp.edit_n_matching_text_was_not_found",
                format!(
                    "第 {n} 处修改：没有找到匹配文字（前面几处改完之后的内容里），文件没有改动"
                ),
            ),
            None => error.with_reason(
                "mcp.matching_text_was_not_found_in_the_file",
                "文件中没有找到匹配文字",
            ),
        });
    }
    if count > 1 && !edit.all {
        let error = AppError::new("mcp.edit_not_unique").with("count", count);
        return Err(match number {
            Some(n) => error.with("edit", n).with_reason(
                "mcp.edit_n_multiple_matches_found",
                format!(
                    "第 {n} 处修改：匹配文字出现了 {count} 次，请提供更多上下文或给这一处设 replace_all，文件没有改动"
                ),
            ),
            None => error.with_reason(
                "mcp.multiple_matches_found_provide_more_context_or_use_replace_all",
                "匹配文字不唯一，请提供更多上下文或使用 replace_all",
            ),
        });
    }
    let mut updated = String::new();
    let mut previous = 0;
    for range in matches {
        let replacement = doc.fragment_replacement(&doc.text[range.clone()], edit.new);
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
    Ok((updated, count))
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

/// 根据 path 的扩展名、所在目录或文件名返回需要用户确认的中文原因；普通数据文件返回 None。
/// 除了脚本和程序，还有会被 SSH、Git、shell 当作命令执行的配置（.ssh 下的文件、Git 的配置和钩子、shell 的启动文件）。
pub fn write_needs_confirmation(path: &Path) -> Option<String> {
    let path = path.to_string_lossy().replace('\\', "/").to_lowercase();
    let parts: Vec<&str> = path
        .split('/')
        .map(|part| part.trim_end_matches([' ', '.']))
        .collect();
    if parts
        .iter()
        .any(|part| matches!(*part, "startup" | "启动" | "启动项目"))
    {
        return Some("写入启动文件夹会在登录时运行程序".into());
    }
    let name = parts.last().copied().unwrap_or("");
    let in_git = parts.len() >= 2 && parts[parts.len() - 2] == ".git";
    if parts.contains(&".ssh")
        || parts.windows(2).any(|pair| pair == [".git", "hooks"])
        || (in_git && name == "config")
        || matches!(
            name,
            ".gitconfig"
                | ".bashrc"
                | ".bash_profile"
                | ".bash_login"
                | ".profile"
                | ".zshrc"
                | ".zprofile"
                | ".zshenv"
                | ".zlogin"
        )
    {
        return Some("写入 SSH、Git 或 shell 的配置可能让它们执行命令".into());
    }
    ends_with_any(&path, &RUNNABLE).then(|| "写入脚本、程序或快捷方式可能执行命令".into())
}

/// Windows 上双击或打开就会运行的脚本、程序和快捷方式：写入要确认，open_path 不打开
const RUNNABLE: [&str; 17] = [
    ".ps1", ".psm1", ".bat", ".cmd", ".vbs", ".vbe", ".jse", ".wsf", ".lnk", ".hta", ".exe",
    ".com", ".scr", ".url", ".reg", ".msi", ".dll",
];
/// 另外这些用默认程序打开时也会运行（或安装）程序；写它们是正常的开发工作（.js、.py 等），只在打开时拒绝
const RUNS_WHEN_OPENED: [&str; 22] = [
    ".js",
    ".jar",
    ".py",
    ".pyw",
    ".pif",
    ".cpl",
    ".msc",
    ".msp",
    ".scf",
    ".ws",
    ".wsh",
    ".wsc",
    ".sct",
    ".chm",
    ".application",
    ".appref-ms",
    ".settingcontent-ms",
    ".appx",
    ".appxbundle",
    ".msix",
    ".msixbundle",
    ".diagcab",
];

/// 小写、去掉末尾点和空格（Windows 会忽略它们）后的 path 是不是以 suffixes 里的某一个结尾
fn ends_with_any(path: &str, suffixes: &[&str]) -> bool {
    let path = path.trim_end_matches([' ', '.']);
    suffixes.iter().any(|suffix| path.ends_with(suffix))
}

/// 用默认程序打开 path 会不会直接运行程序（脚本、程序、快捷方式、安装包等）
///
/// :param path: 文件路径
/// :return: 会运行为 true
pub fn runs_when_opened(path: &Path) -> bool {
    let path = path.to_string_lossy().replace('\\', "/").to_lowercase();
    ends_with_any(&path, &RUNNABLE) || ends_with_any(&path, &RUNS_WHEN_OPENED)
}

/// open_path 只打开这些类型：文档、图片、音视频、文本、网页、压缩包（打开只是看，不会运行程序）。
/// 列不全“会运行”的类型（装了什么程序就多出什么），所以反过来只放行这些
const OPENABLE: [&str; 72] = [
    // 文档（不含带宏的 .docm、.xlsm 等）
    "pdf", "txt", "md", "markdown", "rtf", "doc", "docx", "odt", "xls", "xlsx", "ods", "csv", "tsv",
    "ppt", "pptx", "odp", "epub", "xps", "oxps", // 图片
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "ico", "tif", "tiff", "heic", "avif",
    // 音频、视频
    "mp3", "wav", "flac", "aac", "ogg", "oga", "m4a", "wma", "opus", "mp4", "m4v", "mkv", "mov",
    "avi", "wmv", "webm", "flv", "mpg", "mpeg", // 文本、数据、网页
    "log", "json", "xml", "yaml", "yml", "toml", "ini", "cfg", "conf", "html", "htm",
    // 压缩包
    "zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "zst", "lz4", "cab",
];

/// open_path 能不能用默认程序打开 path：文件夹、或 OPENABLE 里的类型；其他都不打开
///
/// :param path: 路径（规范化后的）
/// :param is_dir: 是不是文件夹
/// :return: 能打开为 true
pub fn openable(path: &Path, is_dir: bool) -> bool {
    if is_dir {
        return true;
    }
    let name = path.to_string_lossy().to_lowercase();
    let name = name.trim_end_matches([' ', '.']);
    let Some((_, extension)) = name.rsplit_once('.') else {
        return false;
    };
    !extension.contains(['/', '\\']) && OPENABLE.contains(&extension) && !runs_when_opened(path)
}
/// 判断 path 是否需要写入确认；只用于分类，访问前仍须沙箱校验。
pub fn is_dangerous_path(path: &str) -> bool {
    write_needs_confirmation(Path::new(path)).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Reader<F>(F);
    impl<F: FnMut(&mut [u8]) -> std::io::Result<usize>> Read for Reader<F> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            (self.0)(buffer)
        }
    }

    #[test]
    fn 分块读取重试中断且失败带路径并响应取消() {
        use std::io::{Error, ErrorKind};
        use std::sync::atomic::Ordering;
        let path = Path::new("文件.txt");
        let cancelled = AtomicBool::new(false);
        let mut buffer = [0; 2];
        let mut attempts = 0;
        let mut reader = Reader(|buffer: &mut [u8]| {
            attempts += 1;
            if attempts <= 2 {
                return Err(ErrorKind::Interrupted.into());
            }
            buffer.copy_from_slice(b"ok");
            Ok(2)
        });
        assert_eq!(
            read_chunk(&mut reader, &mut buffer, path, &cancelled).unwrap(),
            2
        );
        assert_eq!(buffer, *b"ok");
        assert_eq!(attempts, 3);

        let mut reader = Reader(|_: &mut [u8]| Err(Error::from(ErrorKind::PermissionDenied)));
        let error = read_chunk(&mut reader, &mut buffer, path, &cancelled).unwrap_err();
        assert_eq!(error.code, "mcp.path_invalid");
        assert_eq!(error.params["path"], fs_local::display_path(path));
        assert!(!error.params["reason"].is_empty());

        let mut reader = Reader(|_: &mut [u8]| {
            cancelled.store(true, Ordering::Release);
            Err(Error::from(ErrorKind::Interrupted))
        });
        assert_eq!(
            read_chunk(&mut reader, &mut buffer, path, &cancelled)
                .unwrap_err()
                .code,
            "mcp.cancelled"
        );
        let mut reader = Reader(|_: &mut [u8]| panic!("取消后不能继续读取"));
        assert_eq!(
            read_chunk(&mut reader, &mut buffer, path, &cancelled)
                .unwrap_err()
                .code,
            "mcp.cancelled"
        );
    }

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
    pub(super) fn junction(link: &Path, target: &Path) {
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
        for old in ["a\nb", "a\r\nb"] {
            let original = "a\r\nb\nx\na\nb\n";
            fs::write(path, original).unwrap();
            let error = edit_file(&sb, path, old, "c\nd", false, None).unwrap_err();
            assert_eq!(error.code, "mcp.edit_not_unique");
            assert_eq!(error.params["count"], "2");
            assert_eq!(fs::read_to_string(path).unwrap(), original);
            assert_eq!(
                edit_file(&sb, path, old, "c\nd", true, None)
                    .unwrap()
                    .replacements,
                2
            );
            assert_eq!(fs::read_to_string(path).unwrap(), "c\r\nd\nx\nc\nd\n");
        }
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
        for path in [
            r"\\server\share\folder",
            r"\\?\UNC\server\share\folder",
            "//server/share/folder",
        ] {
            assert_eq!(
                validate_folder(Path::new(path)).unwrap_err().code,
                "mcp.network_folder"
            );
            assert_eq!(
                sb.resolve_read(path).unwrap_err().code,
                "mcp.network_folder"
            );
            assert_eq!(
                sb.resolve_write(path).unwrap_err().code,
                "mcp.network_folder"
            );
        }
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
        assert!(description.contains("不能覆盖二进制文件"));
        assert!(description.contains("不能覆盖超过 5 MiB 的已有文件"));
    }
    #[cfg(windows)]
    #[test]
    fn 文件占用时返回系统码且保留原文并清理临时文件() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let path = root.path().join("busy.txt");
        fs::write(&path, "原文").unwrap();
        let path = fs_local::plain_canonical(&path).unwrap();
        for share in [1, 3] {
            // 允许读取原文，禁止删除共享，使最终原子替换失败。
            let held = fs::OpenOptions::new()
                .read(true)
                .share_mode(share)
                .open(&path)
                .unwrap();
            assert!(target_is_busy(&path));
            let error = persist_error(std::io::Error::from_raw_os_error(5), &path);
            assert_eq!(error.code, "mcp.file_busy");
            assert_eq!(error.params["os_code"], "5");
            for error in [
                write_file(&sb, path.to_str().unwrap(), "替换").unwrap_err(),
                edit_file(&sb, path.to_str().unwrap(), "原文", "替换", false, None).unwrap_err(),
            ] {
                assert_eq!(error.code, "mcp.file_busy");
                assert_eq!(error.params["reason"], "文件被别的程序占用");
                assert_eq!(error.params["path"], fs_local::display_path(&path));
                assert!(error.params.contains_key("os_code"));
            }
            assert_eq!(fs::read_to_string(&path).unwrap(), "原文");
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
            drop(held);
        }
        assert!(!target_is_busy(&path));
        let denied = persist_error(std::io::Error::from_raw_os_error(5), &path);
        assert_eq!(denied.code, "mcp.path_invalid");
        assert_eq!(denied.params["os_code"], "5");
        for code in [32, 33] {
            let error = persist_error(std::io::Error::from_raw_os_error(code), &path);
            assert_eq!(error.code, "mcp.file_busy");
            assert_eq!(error.params["os_code"], code.to_string());
        }
        write_file(&sb, path.to_str().unwrap(), "替换").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "替换");
    }
    #[cfg(windows)]
    #[test]
    fn 独占打开使读写编辑均返回占用与系统码() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let path = root.path().join("busy.txt");
        fs::write(&path, "原文").unwrap();
        let path = fs_local::plain_canonical(&path).unwrap();
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let text = path.to_str().unwrap();
        for error in [
            open_file(&path).unwrap_err(),
            read_file(&sb, text, 0, 1024).unwrap_err(),
            read_lines(&sb, text, 1, 1).unwrap_err(),
            write_file(&sb, text, "替换").unwrap_err(),
            edit_file(&sb, text, "原文", "替换", false, None).unwrap_err(),
        ] {
            assert_eq!(error.code, "mcp.file_busy");
            assert!(error.params.contains_key("os_code"));
            assert_eq!(error.params["path"], fs_local::display_path(&path));
        }
        drop(held);
        assert_eq!(fs::read_to_string(&path).unwrap(), "原文");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        for code in [32, 33] {
            let error = io_error(std::io::Error::from_raw_os_error(code), &path);
            assert_eq!(error.code, "mcp.file_busy");
            assert_eq!(error.params["os_code"], code.to_string());
        }
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

#[cfg(test)]
mod confirmation_tests {
    use super::*;
    #[test]
    fn 开发源码按普通文件而可执行类型和启动目录要确认() {
        for ext in ["js", "py", "sh", "ts", "mjs", "jsx", "psd1"] {
            assert!(!is_dangerous_path(&format!("C:/work/code.{ext}")));
            assert!(is_dangerous_path(&format!("C:/Startup/code.{ext}")));
        }
        for ext in [
            "ps1", "psm1", "bat", "cmd", "vbs", "vbe", "lnk", "exe", "scr", "hta", "reg", "url",
            "wsf", "msi", "com", "jse",
        ] {
            assert!(is_dangerous_path(&format!("C:/work/start.{ext}")));
            assert!(is_dangerous_path(&format!(
                "C:/work/start.{} .",
                ext.to_uppercase()
            )));
        }
    }
    #[test]
    fn 会被_ssh_git_shell_执行的配置和动态库要确认() {
        for path in [
            "C:/Users/a/.ssh/config",
            "C:/Users/a/.SSH/authorized_keys",
            "C:/work/repo/.git/hooks/pre-commit",
            "C:/work/repo/.git/hooks",
            "C:/work/repo/.git/config",
            "C:/Users/a/.gitconfig",
            "C:/Users/a/.bashrc",
            "C:/Users/a/.profile .",
            "C:/work/lib/plugin.DLL",
        ] {
            assert!(is_dangerous_path(path), "{path}");
        }
        assert_eq!(
            write_needs_confirmation(Path::new("C:/work/repo/.git/hooks/pre-push")).as_deref(),
            Some("写入 SSH、Git 或 shell 的配置可能让它们执行命令")
        );
        for path in [
            "C:/work/repo/.gitignore",
            "C:/work/repo/.github/workflows/ci.yml",
            "C:/work/notes/config",
            "C:/work/repo/src/hooks/use_state.ts",
            "C:/work/repo/.git/HEAD",
        ] {
            assert!(!is_dangerous_path(path), "{path}");
        }
    }
}

#[cfg(test)]
mod protected_tests {
    use super::*;
    #[test]
    fn pierlink_自己的目录在授权文件夹里也不让读写() {
        let _protected = PROTECTED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = crate::test_support::tempdir();
        let data = root.path().join("pierlink-data");
        fs::create_dir_all(data.join("updates")).unwrap();
        fs::write(data.join("updates/pending.bin"), "x").unwrap();
        fs::write(root.path().join("ok.txt"), "x").unwrap();
        set_protected_dirs(std::slice::from_ref(&data));
        let sb = Sandbox::new(&[root.path().into()]);
        let pending = data.join("updates/pending.bin");
        assert_eq!(
            sb.resolve_read(pending.to_str().unwrap()).unwrap_err().code,
            "mcp.path_protected"
        );
        assert_eq!(
            sb.resolve_write(pending.to_str().unwrap())
                .unwrap_err()
                .code,
            "mcp.path_protected"
        );
        assert_eq!(
            sb.resolve_entry(data.join("new.txt").to_str().unwrap())
                .unwrap_err()
                .code,
            "mcp.path_protected"
        );
        assert!(
            sb.resolve_read(root.path().join("ok.txt").to_str().unwrap())
                .is_ok()
        );
        // 直接把受保护的目录授权给 AI：列为不可用，不当授权目录
        let only = Sandbox::new(std::slice::from_ref(&data));
        assert!(only.folders.is_empty());
        assert_eq!(only.unavailable_folders[0].code, "mcp.path_protected");
        assert!(is_protected(&data.join("不存在的/文件.txt")));
        assert!(!is_protected(&root.path().join("ok.txt")));
    }
}

/// 往终端里发的图片最大多少字节（和手机发图片一样）
const TERMINAL_IMAGE_LIMIT: usize = 8 * 1024 * 1024;

/// 图片文件头对应的扩展名（PNG、JPEG、GIF、WebP）；不是这几种返回 None
fn image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("webp")
    } else {
        None
    }
}

/// 读出 sb 里要发给终端的图片：扩展名是 png/jpg/jpeg/gif/webp，文件头也是这几种之一，最大 8 MiB。
///
/// :param sb: 授权文件夹
/// :param path: 图片路径（Windows 绝对路径）
/// :return: (图片内容, 按文件头定的扩展名 png/jpg/gif/webp)
pub(super) fn read_terminal_image(
    sb: &Sandbox,
    path: &str,
) -> Result<(Vec<u8>, &'static str), AppError> {
    let resolved = sb.resolve_read(path)?;
    let named = resolved
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        });
    let not_image = || {
        path_error(
            "mcp.path_invalid",
            &resolved,
            "mcp.image_must_be_png_jpeg_gif_or_webp",
            "image_path 要是 PNG、JPEG、GIF 或 WebP 图片（按扩展名和文件头判断）",
        )
    };
    if !named {
        return Err(not_image());
    }
    let bytes = read_bounded(&resolved, TERMINAL_IMAGE_LIMIT)?;
    let extension = image_extension(&bytes).ok_or_else(not_image)?;
    Ok((bytes, extension))
}

// 小图片走 MCP 图片内容，不占用按行或按字节文本响应。
pub(super) fn read_image(
    sb: &Sandbox,
    path: &str,
    cancelled: &AtomicBool,
) -> Result<Option<serde_json::Value>, AppError> {
    let extension = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return Ok(None),
    };
    check_cancel(cancelled)?;
    let path = sb.resolve_read(path)?;
    let mut file = open_file(&path)?;
    let metadata = file.metadata().map_err(|e| io_error(e, &path))?;
    const LIMIT: usize = 2 * 1024 * 1024;
    if metadata.len() > LIMIT as u64 {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    let mut buffer = [0; 32768];
    loop {
        let count = read_chunk(&mut file, &mut buffer, &path, cancelled)?;
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > LIMIT {
            return Err(too_large(LIMIT));
        }
        if count == 0 {
            break;
        }
    }
    check_metadata(&file, &metadata)?;
    Ok(Some(
        serde_json::json!({"path":path,"size":bytes.len(),"image":{"data":Base64::encode_string(&bytes),"mimeType":mime}}),
    ))
}

#[cfg(test)]
mod multi_edit_tests {
    use super::*;
    #[test]
    fn 多处修改按顺序做_全部成功才写入_失败时写明第几处() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let path = root.path().join("a.txt");
        let text = path.to_str().unwrap();
        fs::write(&path, "alpha\r\nbeta\r\nbeta\r\ngamma\r\n").unwrap();
        let original = fs::read(&path).unwrap();
        let edit = |old, new, all| Edit { old, new, all };
        // 第二处在第一处改完的内容上找；换行按原文件的 CRLF 写
        let edited = edit_file_many(
            &sb,
            text,
            &[
                edit("alpha", "first\nline", false),
                edit("first\nline\nbeta", "one", false),
                edit("beta", "two", true),
            ],
            true,
            Some(&hash(&original)),
        )
        .unwrap();
        assert_eq!(edited.replacements, 3);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "one\r\ntwo\r\ngamma\r\n"
        );
        let before = fs::read(&path).unwrap();
        for (edits, code, key) in [
            (
                vec![edit("one", "1", false), edit("missing", "x", false)],
                "mcp.edit_not_found",
                "mcp.edit_n_matching_text_was_not_found",
            ),
            (
                vec![edit("gamma", "two", false), edit("two", "x", false)],
                "mcp.edit_not_unique",
                "mcp.edit_n_multiple_matches_found",
            ),
            (
                vec![edit("one", "1", false), edit("", "x", false)],
                "mcp.edit_not_found",
                "mcp.edit_n_matching_text_cannot_be_empty",
            ),
        ] {
            let error = edit_file_many(&sb, text, &edits, true, None).unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(error.params["reason_key"], key);
            assert_eq!(error.params["edit"], "2");
            assert!(error.params["reason"].starts_with("第 2 处修改："));
            // 任何一处不成，文件都不改
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        // 一处修改照旧，原因里不写第几处
        let error = edit_file(&sb, text, "missing", "x", false, None).unwrap_err();
        assert_eq!(
            error.params["reason_key"],
            "mcp.matching_text_was_not_found_in_the_file"
        );
        assert!(!error.params.contains_key("edit"));
    }
}

#[cfg(test)]
mod terminal_image_tests {
    use super::*;
    #[test]
    fn 发给终端的图片按扩展名和文件头认_最大八兆() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let write = |name: &str, bytes: &[u8]| {
            let path = root.path().join(name);
            fs::write(&path, bytes).unwrap();
            path.to_str().unwrap().to_owned()
        };
        for (name, bytes, extension) in [
            ("a.png", b"\x89PNG\r\n\x1a\n...".as_slice(), "png"),
            ("b.JPEG", b"\xff\xd8\xff\xe0", "jpg"),
            ("c.gif", b"GIF89a..", "gif"),
            ("d.webp", b"RIFF\0\0\0\0WEBPVP8 ", "webp"),
            // 扩展名和文件头不一致时按文件头定
            ("e.png", b"\xff\xd8\xff\xe0", "jpg"),
        ] {
            let path = write(name, bytes);
            let (read, ext) = read_terminal_image(&sb, &path).unwrap();
            assert_eq!((read.as_slice(), ext), (bytes, extension), "{name}");
        }
        for (name, bytes) in [
            ("f.txt", b"\x89PNG\r\n\x1a\n".as_slice()),
            ("g.png", b"not png"),
        ] {
            let path = write(name, bytes);
            let error = read_terminal_image(&sb, &path).unwrap_err();
            assert_eq!(
                error.params["reason_key"], "mcp.image_must_be_png_jpeg_gif_or_webp",
                "{name}"
            );
        }
        let big = write("big.png", &vec![0; TERMINAL_IMAGE_LIMIT + 1]);
        assert_eq!(
            read_terminal_image(&sb, &big).unwrap_err().code,
            "mcp.file_too_large"
        );
    }
}

#[cfg(test)]
mod binary_write_tests {
    use super::*;
    #[test]
    fn 原样写入字节_新建和覆盖二进制文件_超过上限和散列不符时不写() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let path = root.path().join("新/图片.png");
        let text = path.to_str().unwrap();
        let bytes = [0x89, b'P', b'N', b'G', 0, 0xff, b'\r', b'\n'];
        let written = write_bytes(&sb, text, &bytes, None).unwrap();
        assert_eq!(written.bytes, bytes.len());
        assert_eq!(written.sha256, hash(&bytes));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        // 文字方式不能覆盖二进制文件，原样写入可以
        assert!(write_checked(&sb, text, "文字", None).is_err());
        let changed = [1u8, 2, 3, 0];
        assert_eq!(
            write_bytes(&sb, text, &changed, Some("0".repeat(64).as_str()))
                .unwrap_err()
                .code,
            "mcp.file_changed"
        );
        write_bytes(&sb, text, &changed, Some(&written.sha256)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), changed);
        // 换行不转换：CRLF 原样保留
        let crlf = root.path().join("a.txt");
        fs::write(&crlf, "x\n").unwrap();
        write_bytes(&sb, crlf.to_str().unwrap(), b"a\r\nb", None).unwrap();
        assert_eq!(fs::read(&crlf).unwrap(), b"a\r\nb");
        assert_eq!(
            write_bytes(&sb, text, &vec![0; WRITE_LIMIT + 1], None)
                .unwrap_err()
                .code,
            "mcp.file_too_large"
        );
        // 已有文件超过 5 MiB：不覆盖
        let big = root.path().join("big.bin");
        fs::write(&big, vec![0u8; WRITE_LIMIT + 1]).unwrap();
        assert_eq!(
            write_bytes(&sb, big.to_str().unwrap(), b"x", None)
                .unwrap_err()
                .code,
            "mcp.file_too_large"
        );
        assert_eq!(
            write_bytes(&sb, root.path().to_str().unwrap(), b"x", None)
                .unwrap_err()
                .code,
            "mcp.path_invalid"
        );
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;
    #[test]
    fn 写入自动建父目录且预检无副作用() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let path = root.path().join("src/components/New.tsx");
        let text = path.to_str().unwrap();
        assert_eq!(sb.resolve_write(text).unwrap(), path);
        assert!(!root.path().join("src").exists());
        write_file(&sb, text, "组件").unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "组件");
        let bad = root.path().join("src/components/New.tsx/child");
        assert!(write_file(&sb, bad.to_str().unwrap(), "x").is_err());
    }
    #[test]
    fn 目录创建删除与重命名覆盖规则() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let dir = root.path().join("one/two");
        for _ in 0..2 {
            make_dir(&sb, dir.to_str().unwrap()).unwrap();
        }
        make_dir(&sb, root.path().to_str().unwrap()).unwrap();
        let from = dir.join("a.txt");
        let to = dir.join("b.txt");
        write_file(&sb, from.to_str().unwrap(), "a").unwrap();
        assert!(make_dir(&sb, from.to_str().unwrap()).is_err());
        rename_path(&sb, from.to_str().unwrap(), to.to_str().unwrap(), false).unwrap();
        write_file(&sb, from.to_str().unwrap(), "b").unwrap();
        assert_eq!(
            rename_path(&sb, from.to_str().unwrap(), to.to_str().unwrap(), false)
                .unwrap_err()
                .code,
            "mcp.path_exists"
        );
        rename_path(&sb, from.to_str().unwrap(), to.to_str().unwrap(), true).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "b");
        assert!(delete_path(&sb, dir.to_str().unwrap(), false).is_err());
        delete_path(&sb, to.to_str().unwrap(), false).unwrap();
        delete_path(&sb, dir.to_str().unwrap(), false).unwrap();
        make_dir(&sb, dir.to_str().unwrap()).unwrap();
        fs::write(dir.join("nested"), "x").unwrap();
        let moved = root.path().join("moved");
        rename_path(&sb, dir.to_str().unwrap(), moved.to_str().unwrap(), false).unwrap();
        delete_path(&sb, moved.to_str().unwrap(), true).unwrap();
        assert!(!moved.exists());
    }
    #[test]
    fn 新文件操作拒绝越界且不创建越界目录() {
        let root = crate::test_support::tempdir();
        let allowed = root.path().join("allowed");
        fs::create_dir(&allowed).unwrap();
        let sb = Sandbox::new(std::slice::from_ref(&allowed));
        let outside = root.path().join("outside/nested");
        let outside = outside.to_str().unwrap();
        assert!(make_dir(&sb, outside).is_err());
        assert!(write_file(&sb, outside, "x").is_err());
        assert!(delete_path(&sb, outside, true).is_err());
        let source = allowed.join("a");
        fs::write(&source, "safe").unwrap();
        assert!(rename_path(&sb, source.to_str().unwrap(), outside, true).is_err());
        assert!(rename_path(&sb, outside, source.to_str().unwrap(), true).is_err());
        assert!(!root.path().join("outside").exists());
        assert_eq!(fs::read_to_string(source).unwrap(), "safe");
    }
    #[cfg(any(unix, windows))]
    #[test]
    fn 链接与联接不用于创建且删除重命名只操作链接() {
        let root = crate::test_support::tempdir();
        let allowed = root.path().join("allowed");
        let outside = root.path().join("outside");
        fs::create_dir(&allowed).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "safe").unwrap();
        let sb = Sandbox::new(std::slice::from_ref(&allowed));
        for destination in [&outside, &allowed] {
            let link = allowed.join("link");
            #[cfg(unix)]
            std::os::unix::fs::symlink(destination, &link).unwrap();
            #[cfg(windows)]
            super::tests::junction(&link, destination);
            let path = link.join("missing/child");
            assert!(write_file(&sb, path.to_str().unwrap(), "x").is_err());
            assert!(make_dir(&sb, path.to_str().unwrap()).is_err());
            assert!(delete_path(&sb, link.join("keep").to_str().unwrap(), false).is_err());
            let moved = allowed.join("moved");
            rename_path(&sb, link.to_str().unwrap(), moved.to_str().unwrap(), false).unwrap();
            delete_path(&sb, moved.to_str().unwrap(), true).unwrap();
            assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "safe");
            assert!(!destination.join("missing").exists());
        }
    }
    #[cfg(any(unix, windows))]
    #[test]
    fn 递归删除不跟随嵌套链接且授权目录本身不能删除或移走() {
        let root = crate::test_support::tempdir();
        let allowed = root.path().join("allowed");
        let sub = allowed.join("sub");
        let outside = root.path().join("outside");
        fs::create_dir_all(&sub).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "safe").unwrap();
        let link = sub.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        #[cfg(windows)]
        super::tests::junction(&link, &outside);
        let sb = Sandbox::new(std::slice::from_ref(&allowed));
        for recursive in [false, true] {
            let error = delete_path(&sb, allowed.to_str().unwrap(), recursive).unwrap_err();
            assert_eq!(
                error.params["reason_key"],
                "mcp.allowed_folder_itself_cannot_be_deleted_or_moved"
            );
        }
        let moved = root.path().join("moved");
        assert!(
            rename_path(
                &sb,
                allowed.to_str().unwrap(),
                moved.to_str().unwrap(),
                false
            )
            .is_err()
        );
        let error = delete_path(&sb, sub.to_str().unwrap(), false).unwrap_err();
        assert_eq!(
            error.params["reason_key"],
            "mcp.folder_is_not_empty_set_recursive"
        );
        delete_path(&sb, sub.to_str().unwrap(), true).unwrap();
        assert!(!sub.exists() && allowed.exists());
        assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "safe");
    }
    #[cfg(unix)]
    #[test]
    fn 悬空链接删除与重命名不触及指向路径() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let link = root.path().join("link");
        std::os::unix::fs::symlink(root.path().join("absent"), &link).unwrap();
        assert!(write_file(&sb, link.to_str().unwrap(), "x").is_err());
        let moved = root.path().join("moved");
        rename_path(&sb, link.to_str().unwrap(), moved.to_str().unwrap(), false).unwrap();
        delete_path(&sb, moved.to_str().unwrap(), false).unwrap();
        assert!(!root.path().join("absent").exists());
    }
    #[test]
    fn 图片格式大小授权与取消边界() {
        let root = crate::test_support::tempdir();
        let sb = Sandbox::new(&[root.path().into()]);
        let cancelled = AtomicBool::new(false);
        for (ext, mime) in [
            ("PNG", "image/png"),
            ("jpg", "image/jpeg"),
            ("jpeg", "image/jpeg"),
            ("gif", "image/gif"),
            ("webp", "image/webp"),
        ] {
            let path = root.path().join(format!("image.{ext}"));
            fs::write(&path, [0, 1, 255]).unwrap();
            let image = read_image(&sb, path.to_str().unwrap(), &cancelled)
                .unwrap()
                .unwrap();
            assert_eq!(image["image"]["mimeType"], mime);
            assert_eq!(image["image"]["data"], "AAH/");
            assert!(read_image(&sb, path.to_str().unwrap(), &AtomicBool::new(true)).is_err());
        }
        let path = root.path().join("limit.png");
        let file = fs::File::create(&path).unwrap();
        file.set_len(2 * 1024 * 1024).unwrap();
        assert!(
            read_image(&sb, path.to_str().unwrap(), &cancelled)
                .unwrap()
                .is_some()
        );
        file.set_len(2 * 1024 * 1024 + 1).unwrap();
        assert!(
            read_image(&sb, path.to_str().unwrap(), &cancelled)
                .unwrap()
                .is_none()
        );
        assert!(read_image(&Sandbox::new(&[]), path.to_str().unwrap(), &cancelled).is_err());
    }
}
