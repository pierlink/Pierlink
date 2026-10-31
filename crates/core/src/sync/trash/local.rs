//! Unix 用目录描述符定位子项；Windows 持有禁止删除共享的目录句柄，防止遍历期间替换祖先。
use super::*;
use std::io::ErrorKind;

pub(super) struct Directory {
    path: PathBuf,
    _handles: Vec<fs::File>,
}
fn reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.is_symlink()
    }
}
fn open_directory(path: &Path) -> Result<fs::File, AppError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };
        options
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    let file = options.open(path).map_err(io)?;
    let metadata = file.metadata().map_err(io)?;
    if !metadata.is_dir() || reparse(&metadata) {
        return Err(unsafe_path());
    }
    Ok(file)
}
fn anchored(file: &fs::File, path: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let _ = path;
        PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        path.to_owned()
    }
}
pub(super) fn open_path(path: &Path) -> Result<Option<Directory>, AppError> {
    let mut current = PathBuf::new();
    let mut handles = Vec::new();
    for part in path.components() {
        current.push(part);
        // Windows 盘符前缀还不是完整目录。
        if matches!(part, Component::Prefix(_)) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&current) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(e)),
        };
        if reparse(&metadata) || !metadata.is_dir() {
            return Err(unsafe_path());
        }
        let handle = open_directory(&current)?;
        current = anchored(&handle, &current);
        handles.push(handle);
    }
    Ok(Some(Directory {
        path: current,
        _handles: handles,
    }))
}
pub(super) fn walk(
    directory: &Directory,
    clear: bool,
    usage: &mut TrashUsage,
) -> Result<(), AppError> {
    for entry in fs::read_dir(&directory.path).map_err(io)? {
        let path = entry.map_err(io)?.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(io(e)),
        };
        if reparse(&metadata) {
            return Err(unsafe_path());
        }
        if metadata.is_dir() {
            let handle = open_directory(&path)?;
            let child = Directory {
                path: anchored(&handle, &path),
                _handles: vec![handle],
            };
            walk(&child, clear, usage)?;
            drop(child);
            if clear
                && let Err(e) = fs::remove_dir(&path)
                && !matches!(e.kind(), ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty)
            {
                return Err(io(e));
            }
        } else if metadata.is_file() {
            if clear && let Err(e) = fs::remove_file(&path) {
                if e.kind() == ErrorKind::NotFound {
                    continue;
                }
                return Err(io(e));
            }
            usage.files = usage.files.checked_add(1).ok_or_else(unsafe_path)?;
            usage.bytes = usage
                .bytes
                .checked_add(metadata.len())
                .ok_or_else(unsafe_path)?;
        } else {
            return Err(unsafe_path());
        }
    }
    Ok(())
}
