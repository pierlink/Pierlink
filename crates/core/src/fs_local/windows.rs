//! Windows 文件回收、替换与资源管理器入口。
use super::{display_path, error};
use crate::error::AppError;
use std::{
    io,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use windows_sys::Win32::UI::Shell::{
    FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING,
    SHFILEOPSTRUCTW, SHFileOperationW, ShellExecuteW,
};

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut result: Vec<_> = path.as_os_str().encode_wide().collect();
    if result.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "路径含空字符"));
    }
    result.push(0);
    Ok(result)
}
// 临时文件已复制原文件的只读属性；替换失败时只清除该属性，确保能删除。
pub(super) fn clear_readonly(path: &Path) {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_READONLY, GetFileAttributesW, INVALID_FILE_ATTRIBUTES, SetFileAttributesW,
    };
    let Ok(path) = wide(path) else {
        return;
    };
    let attrs = unsafe { GetFileAttributesW(path.as_ptr()) };
    if attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_READONLY != 0 {
        unsafe {
            SetFileAttributesW(path.as_ptr(), attrs & !FILE_ATTRIBUTE_READONLY);
        }
    }
}
pub(super) fn move_file(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW};
    let from = wide(from)?;
    let to = wide(to)?;
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            if replace {
                MOVEFILE_REPLACE_EXISTING
            } else {
                0
            },
        )
    } != 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
fn recycle_paths(paths: &[PathBuf]) -> Result<Vec<u16>, AppError> {
    let mut values = Vec::new();
    for path in paths {
        // 不解析最后一级链接，回收链接本身。
        std::fs::symlink_metadata(path).map_err(|e| error(e, "读取待回收项", path))?;
        let path = std::path::absolute(path).map_err(|e| error(e, "解析待回收路径", path))?;
        values.extend(
            wide(Path::new(&display_path(&path))).map_err(|e| error(e, "编码待回收路径", &path))?,
        );
    }
    values.push(0);
    Ok(values)
}
pub(super) fn delete(paths: &[PathBuf]) -> Result<usize, AppError> {
    if paths.is_empty() {
        return Ok(0);
    }
    let values = recycle_paths(paths)?;
    let mut operation: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    operation.wFunc = FO_DELETE;
    operation.pFrom = values.as_ptr();
    // FOF_WANTNUKEWARNING：放不进回收站（如网络盘）时由系统先警告，不会悄悄永久删除
    operation.fFlags =
        (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING)
            as u16;
    let result = unsafe { SHFileOperationW(&mut operation) };
    if result != 0 || operation.fAnyOperationsAborted != 0 {
        return Err(AppError::new("fs.failure")
            .with("operation", "移入回收站")
            .with(
                "path",
                paths
                    .iter()
                    .map(|p| display_path(p))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .with("reason", "回收失败或操作被取消")
            .with("status", result));
    }
    Ok(paths.len())
}
pub(super) fn open(path: &Path) -> Result<(), AppError> {
    let wide = wide(path).map_err(|e| error(e, "编码打开路径", path))?;
    let verb: Vec<_> = "open\0".encode_utf16().collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    } as isize;
    if result > 32 {
        Ok(())
    } else {
        Err(AppError::new("fs.failure")
            .with("operation", "用默认程序打开")
            .with("path", display_path(path))
            .with("status", result))
    }
}
fn reveal_argument(path: &Path) -> std::ffi::OsString {
    let mut arg = std::ffi::OsString::from("/select,\"");
    arg.push(path);
    arg.push("\"");
    arg
}
pub(super) fn reveal(path: &Path) -> Result<(), AppError> {
    use std::os::windows::process::CommandExt;
    let executable = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .ok_or_else(|| AppError::new("fs.failure").with("reason", "缺少系统目录"))?
        .join("explorer.exe");
    // Explorer 使用自己的 /select 语法，raw_arg 保留所需引号；不经过 shell。
    let mut child = std::process::Command::new(executable)
        .raw_arg(reveal_argument(path))
        .spawn()
        .map_err(|e| error(e, "启动资源管理器", path))?;
    // 在后台等待启动器，避免资源管理器新窗口长时间存活时阻塞调用方。
    std::thread::Builder::new()
        .name("资源管理器启动器回收".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|e| error(e, "创建启动器回收线程", path))?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 回收列表双空字符及定位引号() {
        let root = crate::test_support::tempdir();
        let paths: Vec<_> = ["中文 a.txt", "b.txt"]
            .iter()
            .map(|n| root.path().join(n))
            .collect();
        for path in &paths {
            std::fs::write(path, "").unwrap();
        }
        let values = recycle_paths(&paths).unwrap();
        assert_eq!(&values[values.len() - 2..], &[0, 0]);
        assert_eq!(values.iter().filter(|v| **v == 0).count(), 3);
        assert_eq!(
            reveal_argument(Path::new(r"D:\中文 a.txt")),
            std::ffi::OsString::from(r#"/select,"D:\中文 a.txt""#)
        );
    }
}
