//! 文件菜单操作；写入临时文件在成功与失败路径上均自动清理。
use super::{FileVersion, TextFile, error, plain_canonical, text, unix_seconds};
use crate::error::AppError;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

fn validate_path(path: &Path) -> Result<(), AppError> {
    let value = path
        .to_str()
        .ok_or_else(|| AppError::new("files.invalid_name"))?;
    let name = value
        .rsplit(if cfg!(windows) {
            &['/', '\\'][..]
        } else {
            &['/'][..]
        })
        .next()
        .unwrap_or("");
    text::validate_name(name, true).map_err(|e| e.with("path", super::display_path(path)))
}
fn absent(path: &Path) -> Result<(), AppError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(AppError::new("fs.exists").with("path", super::display_path(path))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(error(e, "fs_local.check_destination", "检查目标", path)),
    }
}
/// 将 from 改名为 to；验证目标名字，已存在返回 fs.exists，始终不覆盖。
pub fn rename(from: &Path, to: &Path) -> Result<(), AppError> {
    validate_path(to)?;
    // Windows 文件名不区分大小写：只改大小写（a.txt → A.txt）时目标“已存在”的就是它自己，不算重名
    if !case_only_change(from, to) {
        absent(to)?;
    }
    rename_no_replace(from, to).map_err(|e| error(e, "fs_local.rename", "重命名", to))
}
/// 同一目录下、名字只有大小写不同（只在 Windows 上成立；其他系统大小写不同就是不同的文件）
fn case_only_change(from: &Path, to: &Path) -> bool {
    cfg!(windows)
        && from.parent() == to.parent()
        && from.file_name() != to.file_name()
        && from.file_name().zip(to.file_name()).is_some_and(|(a, b)| {
            a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        })
}
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let to = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        if unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        super::windows::move_file(from, to, false)
    }
    #[cfg(all(not(windows), not(target_os = "linux")))]
    {
        let _ = (from, to);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "此系统不支持排他重命名",
        ))
    }
}
/// 在 path 排他创建一个目录；父目录必须存在，返回 fs.* 错误。
pub fn create_dir(path: &Path) -> Result<(), AppError> {
    validate_path(path)?;
    fs::create_dir(path).map_err(|e| error(e, "fs_local.create_directory", "新建目录", path))
}
/// 在 path 排他创建空文件；父目录必须存在，返回 fs.* 错误。
pub fn create_file(path: &Path) -> Result<(), AppError> {
    validate_path(path)?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(|_| ())
        .map_err(|e| error(e, "fs_local.create_file", "新建文件", path))
}
/// 删除 paths；Windows 放入回收站并返回选中项数，开发用 Unix 递归删除且不跟随链接、返回删除条目数。
/// 失败带上具体 path；之前成功的删除不会回滚。
pub fn delete(paths: &[PathBuf]) -> Result<usize, AppError> {
    #[cfg(windows)]
    {
        super::windows::delete(paths)
    }
    #[cfg(not(windows))]
    {
        let mut count = 0;
        for path in paths {
            delete_one(path, &mut count)?;
        }
        Ok(count)
    }
}
#[cfg(not(windows))]
fn delete_one(path: &Path, count: &mut usize) -> Result<(), AppError> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| error(e, "fs_local.read_item_to_delete", "读取待删除项", path))?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        for entry in fs::read_dir(path).map_err(|e| {
            error(
                e,
                "fs_local.read_directory_to_delete",
                "读取待删除目录",
                path,
            )
        })? {
            let entry = entry
                .map_err(|e| error(e, "fs_local.read_item_to_delete", "读取待删除项", path))?;
            delete_one(&entry.path(), count)?;
        }
        fs::remove_dir(path)
            .map_err(|e| error(e, "fs_local.delete_directory", "删除目录", path))?;
    } else {
        fs::remove_file(path).map_err(|e| error(e, "fs_local.delete_file", "删除文件", path))?;
    }
    *count += 1;
    Ok(())
}
fn version(meta: &fs::Metadata, path: &Path) -> Result<FileVersion, AppError> {
    if !meta.is_file() {
        return Err(AppError::new("files.not_text")
            .with("path", super::display_path(path))
            .with_reason(
                "fs_local.only_regular_text_files_are_supported",
                "只支持普通文本文件",
            ));
    }
    Ok(FileVersion {
        size: meta.len(),
        hash: None,
        mtime: unix_seconds(
            meta.modified()
                .map_err(|e| error(e, "fs_local.read_modification_time", "读取修改时间", path))?,
        ),
    })
}
/// 读取 path 的 UTF-8 文本，最多 max_bytes 字节；去掉 BOM 并返回原字节数和 Unix 秒修改时间。
pub fn read_text(path: &Path, max_bytes: u64) -> Result<TextFile, AppError> {
    // 先拒绝目录和管道，避免打开 FIFO 时阻塞；随后仍使用已打开句柄的属性。
    version(
        &fs::metadata(path).map_err(|e| {
            error(
                e,
                "fs_local.read_text_file_properties",
                "读取文本属性",
                path,
            )
        })?,
        path,
    )?;
    let file =
        File::open(path).map_err(|e| error(e, "fs_local.open_text_file", "打开文本", path))?;
    let current = version(
        &file
            .metadata()
            .map_err(|e| error(e, "fs_local.read_file_properties", "读取属性", path))?,
        path,
    )?;
    if current.size > max_bytes {
        return Err(text::too_large(max_bytes).with("path", super::display_path(path)));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| error(e, "fs_local.read_text_file", "读取文本", path))?;
    text::decode(bytes, max_bytes, current).map_err(|e| e.with("path", super::display_path(path)))
}
struct Pending(PathBuf);
impl Drop for Pending {
    fn drop(&mut self) {
        #[cfg(windows)]
        super::windows::clear_readonly(&self.0);
        let _ = fs::remove_file(&self.0);
    }
}
/// 原子写入 path 的 text，按 bom 保留 UTF-8 BOM；expected 先校验大小、时间，再校验可选哈希。
/// 哈希校验最多读取 5 MiB；文件必须已存在，解析链接后替换目标并保留权限，返回含哈希的新版本。
pub fn write_text(
    path: &Path,
    value: &str,
    bom: bool,
    expected: Option<FileVersion>,
) -> Result<FileVersion, AppError> {
    let meta = check_text_version(path, expected.as_ref())?;
    let target = plain_canonical(path)
        .map_err(|e| error(e, "fs_local.resolve_text_file_path", "解析文本路径", path))?;
    let pending =
        target.with_file_name(format!(".pierlink-{}.pierlink-part", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&pending).map_err(|e| {
        error(
            e,
            "fs_local.create_temporary_text_file",
            "创建文本临时文件",
            &target,
        )
    })?;
    let pending = Pending(pending);
    // 保证失败返回时先关闭句柄，再删除临时文件。
    let mut file = file;
    if bom {
        file.write_all(b"\xef\xbb\xbf").map_err(|e| {
            error(
                e,
                "fs_local.write_text_file_marker",
                "写入文本标记",
                &target,
            )
        })?;
    }
    file.write_all(value.as_bytes())
        .and_then(|()| file.set_permissions(meta.permissions()))
        .and_then(|()| file.sync_all())
        .map_err(|e| error(e, "fs_local.write_text_file", "写入文本", &target))?;
    drop(file);
    check_text_version(&target, expected.as_ref())?;
    #[cfg(windows)]
    super::windows::move_file(&pending.0, &target, true)
        .map_err(|e| error(e, "fs_local.replace_text_file", "替换文本", &target))?;
    #[cfg(not(windows))]
    fs::rename(&pending.0, &target)
        .map_err(|e| error(e, "fs_local.replace_text_file", "替换文本", &target))?;
    // Windows 最后写入时间可能到句柄关闭时才更新，替换后读取实际版本。
    let mut updated = version(
        &fs::metadata(&target).map_err(|e| {
            error(
                e,
                "fs_local.read_updated_file_properties",
                "读取新属性",
                &target,
            )
        })?,
        &target,
    )?;
    updated.hash = Some(text::content_hash(value, bom));
    Ok(updated)
}
fn check_text_version(
    path: &Path,
    expected: Option<&FileVersion>,
) -> Result<fs::Metadata, AppError> {
    let meta = text_metadata(path, expected)?;
    let current = version(&meta, path)?;
    text::check_version(Some(&current), expected)
        .map_err(|e| e.with("path", super::display_path(path)))?;
    if let Some(expected) = expected.filter(|e| e.hash.is_some()) {
        if current.size > text::TEXT_LIMIT {
            return Err(text::too_large(text::TEXT_LIMIT));
        }
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|f| f.take(text::TEXT_LIMIT + 1).read_to_end(&mut bytes))
            .map_err(|e| error(e, "fs_local.verify_text_content", "校验文本内容", path))?;
        if bytes.len() as u64 > text::TEXT_LIMIT {
            return Err(text::too_large(text::TEXT_LIMIT));
        }
        text::check_hash(expected, &text::hash(&bytes))
            .map_err(|e| e.with("path", super::display_path(path)))?;
    }
    Ok(meta)
}
fn text_metadata(path: &Path, expected: Option<&FileVersion>) -> Result<fs::Metadata, AppError> {
    fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound && expected.is_some() {
            AppError::new("files.changed")
                .with("path", super::display_path(path))
                .with_reason(
                    "fs_local.the_file_was_deleted_or_moved",
                    "文件已被删除或移动",
                )
        } else {
            error(
                e,
                "fs_local.read_text_file_properties",
                "读取文本属性",
                path,
            )
        }
    })
}
/// 用默认程序打开已存在的 path；Windows 使用 open 动词，Unix 开发环境调用 xdg-open，返回启动结果。
pub fn open_with_default(path: &Path) -> Result<(), AppError> {
    let path = plain_canonical(path)
        .map_err(|e| error(e, "fs_local.resolve_path_to_open", "解析打开路径", path))?;
    #[cfg(windows)]
    {
        super::windows::open(&path)
    }
    #[cfg(not(windows))]
    {
        launch("xdg-open", &[path.as_os_str()], &path)
    }
}
/// 在资源管理器选中已存在的 path；Unix 开发环境打开其父目录，返回启动结果。
pub fn reveal_in_explorer(path: &Path) -> Result<(), AppError> {
    let path = plain_canonical(path)
        .map_err(|e| error(e, "fs_local.resolve_path_to_reveal", "解析定位路径", path))?;
    #[cfg(windows)]
    {
        super::windows::reveal(&path)
    }
    #[cfg(not(windows))]
    {
        launch(
            "xdg-open",
            &[path.parent().unwrap_or(&path).as_os_str()],
            &path,
        )
    }
}
#[cfg(not(windows))]
fn launch(program: &str, args: &[&std::ffi::OsStr], path: &Path) -> Result<(), AppError> {
    let status = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| {
            error(
                e,
                "fs_local.launch_default_application",
                "启动默认程序",
                path,
            )
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::new("fs.failure")
            .with("path", super::display_path(path))
            .with_text(
                "operation",
                "fs_local.launch_default_application",
                "启动默认程序",
            ))
    }
}

#[cfg(test)]
mod tests;
