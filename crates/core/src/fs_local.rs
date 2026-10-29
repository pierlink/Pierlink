//! 本机目录的有界读取；不跟随符号链接，单项元数据读取失败时跳过。
use crate::error::AppError;
mod operations;
pub(crate) mod text;
#[cfg(windows)]
mod windows;
pub use crate::sftp::EntryKind;
pub use operations::{
    create_dir, create_file, delete, open_with_default, read_text, rename, reveal_in_explorer,
    write_text,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
pub use text::{FileVersion, TextFile};

#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct LocalEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    /// Unix 秒，与 SFTP 文件面板一致。
    pub mtime: i64,
    pub hidden: bool,
}
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct LocalListing {
    pub entries: Vec<LocalEntry>,
    pub truncated: bool,
    pub skipped_non_utf8: usize,
}
pub(crate) fn error(e: io::Error, operation: &str, path: &Path) -> AppError {
    let code = match e.kind() {
        io::ErrorKind::NotFound => "fs.not_found",
        io::ErrorKind::PermissionDenied => "fs.permission_denied",
        io::ErrorKind::AlreadyExists => "fs.exists",
        _ => "fs.failure",
    };
    AppError::new(code)
        .with("operation", operation)
        .with("path", display_path(path))
}
pub(crate) fn display_path(path: &Path) -> String {
    #[cfg(windows)]
    {
        without_windows_prefix(&path.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        path.display().to_string()
    }
}
pub(crate) fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(value) => i64::try_from(value.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}
pub(crate) fn kind(metadata: &fs::Metadata) -> EntryKind {
    let kind = metadata.file_type();
    if kind.is_symlink() {
        EntryKind::Symlink
    } else if kind.is_dir() {
        EntryKind::Dir
    } else if kind.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}
/// 读取 path 的目录，最多返回 limit 项（含隐藏文件）；单项失败或非 UTF-8 名字跳过，目录读取失败返回 fs.*。
pub fn list_dir(path: &Path, limit: usize) -> Result<LocalListing, AppError> {
    let entries = fs::read_dir(path).map_err(|e| error(e, "读取本机目录", path))?;
    let mut result = LocalListing {
        entries: vec![],
        truncated: false,
        skipped_non_utf8: 0,
    };
    for entry in entries {
        let entry = entry.map_err(|e| error(e, "读取本机目录项", path))?;
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            result.skipped_non_utf8 += 1;
            continue;
        };
        if result.entries.len() == limit {
            result.truncated = true;
            break;
        }
        let hidden = name.starts_with('.');
        #[cfg(windows)]
        let hidden = {
            use std::os::windows::fs::MetadataExt;
            hidden || metadata.file_attributes() & 2 != 0
        };
        result.entries.push(LocalEntry {
            name: name.into(),
            kind: kind(&metadata),
            size: metadata.len(),
            mtime: metadata.modified().map(unix_seconds).unwrap_or(0),
            hidden,
        });
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 隐藏项空目录截断和缺失() {
        let root = crate::test_support::tempdir();
        assert!(list_dir(root.path(), 0).unwrap().entries.is_empty());
        fs::write(root.path().join(".隐藏"), b"abc").unwrap();
        fs::create_dir(root.path().join("空目录")).unwrap();
        let listing = list_dir(root.path(), 5).unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert!(!listing.truncated);
        assert!(
            listing
                .entries
                .iter()
                .any(|e| e.name == ".隐藏" && e.hidden && e.size == 3)
        );
        assert!(listing.entries.iter().any(|e| e.kind == EntryKind::Dir));
        assert!(list_dir(root.path(), 1).unwrap().truncated);
        assert_eq!(
            list_dir(&root.path().join("无"), 5).unwrap_err().code,
            "fs.not_found"
        );
        assert_eq!(
            error(
                io::Error::from(io::ErrorKind::PermissionDenied),
                "读取目录",
                root.path()
            )
            .code,
            "fs.permission_denied"
        );
        #[cfg(unix)]
        {
            use std::os::unix::{
                ffi::OsStringExt,
                fs::{PermissionsExt, symlink},
            };
            symlink(root.path(), root.path().join("链接")).unwrap();
            fs::write(
                root.path().join(std::ffi::OsString::from_vec(vec![255])),
                b"",
            )
            .unwrap();
            let result = list_dir(root.path(), 10).unwrap();
            assert!(result.entries.iter().any(|e| e.kind == EntryKind::Symlink));
            assert_eq!(result.skipped_non_utf8, 1);
            let blocked = root.path().join("无权");
            fs::create_dir(&blocked).unwrap();
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o0)).unwrap();
            if unsafe { libc::geteuid() } != 0 {
                assert_eq!(
                    list_dir(&blocked, 1).unwrap_err().code,
                    "fs.permission_denied"
                );
            }
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
}

/// 某个目录所在磁盘的可用空间（字节）。
///
/// :param path: 已存在的目录
/// :return: 可用字节数；查询失败返回 None（调用方应当作“未知”处理，不阻止操作）
pub fn free_space(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut available = 0u64;
        // 第二个参数是当前用户可用的字节数（考虑了磁盘配额）
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(available)
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
    }
}

#[cfg(test)]
mod free_space_tests {
    use super::*;
    #[test]
    fn 可用空间能查到且不存在的路径返回空() {
        let root = crate::test_support::tempdir();
        assert!(free_space(root.path()).is_some_and(|n| n > 0));
        assert!(free_space(&root.path().join("没有这个目录")).is_none());
    }
    #[test]
    fn 普通规范路径与解析失败() {
        let root = crate::test_support::tempdir();
        let path = plain_canonical(root.path()).unwrap();
        #[cfg(not(windows))]
        assert_eq!(path, fs::canonicalize(root.path()).unwrap());
        #[cfg(windows)]
        assert!(!path.to_string_lossy().starts_with(r"\\?\"));
        assert!(plain_canonical(&root.path().join("不存在")).is_err());
    }
}

/// 解析已存在的 path，返回无 Windows 扩展前缀的绝对路径；UNC 还原为普通共享路径，失败返回 I/O 错误。
///
/// 交给 Syncthing 的路径必须用这种形式：带前缀的路径里 Windows 不会把 `/` 当分隔符，
/// Syncthing 用 `/` 拼数据库路径时会报“The specified path is invalid”，本机同步服务起不来。
pub fn plain_canonical(path: &Path) -> std::io::Result<PathBuf> {
    let out = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        Ok(PathBuf::from(without_windows_prefix(
            &out.to_string_lossy(),
        )))
    }
    #[cfg(not(windows))]
    {
        Ok(out)
    }
}
#[cfg(any(windows, test))]
pub(crate) fn without_windows_prefix(text: &str) -> String {
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(text).to_owned()
    }
}
