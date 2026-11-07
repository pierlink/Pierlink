//! 在 Windows 上用默认程序打开授权文件夹里的文件（或文件夹），或用默认浏览器打开 http/https 网址。
//! 打开就会运行的脚本、程序、快捷方式不打开：要运行程序请用 run_command，审批和记录都在那里。
use super::files::{self, Sandbox};
use crate::{error::AppError, fs_local, links};
use std::path::PathBuf;

/// 要打开的东西
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::mcp) enum Opening {
    /// 授权文件夹里规范化后的路径
    Path(PathBuf),
    Url(String),
}

/// 检查 path 或 url 能不能打开（审批前调用，不打开任何东西）。
///
/// :param folders: 这个入口授权的文件夹
/// :param path: Windows 绝对路径；和 url 二选一
/// :param url: http 或 https 网址
/// :return: 要打开的东西；授权外、受保护、会直接运行的文件、不是 http/https 的网址返回错误
pub(super) fn check(
    folders: &[PathBuf],
    path: Option<&str>,
    url: Option<&str>,
) -> Result<Opening, AppError> {
    if let Some(url) = url {
        return links::validate_url(url).map(|url| Opening::Url(url.to_owned()));
    }
    let path = path.unwrap_or_default();
    if folders.is_empty() {
        return Err(super::no_folders());
    }
    let resolved = Sandbox::new(folders).resolve_read(path)?;
    let is_dir = resolved.is_dir();
    if !files::openable(&resolved, is_dir) || !files::openable(std::path::Path::new(path), is_dir) {
        return Err(AppError::new("mcp.open_refused")
            .with("path", fs_local::display_path(&resolved))
            .with_reason(
                "mcp.only_documents_media_and_archives_open",
                "open_path 只打开文件夹和文档、图片、音视频、文本、网页、压缩包；别的类型（脚本、程序、快捷方式、安装包、带宏的文档等）不打开，要运行程序请用 run_command",
            ));
    }
    Ok(Opening::Path(resolved))
}

/// 确认框里的说明
pub(super) fn action(opening: &Opening) -> String {
    match opening {
        Opening::Path(path) => format!("用默认程序打开：{}", path.display()),
        Opening::Url(url) => format!("用默认浏览器打开网址：{url}"),
    }
}

/// 打开（用户已同意或不用问）；不等程序退出
#[cfg(not(test))]
pub(super) fn open(opening: &Opening) -> Result<(), AppError> {
    match opening {
        Opening::Path(path) => fs_local::open_with_default(path),
        Opening::Url(url) => links::open_in_browser(url),
    }
}

/// 测试里不真的启动程序：记下来给测试检查
#[cfg(test)]
pub(in crate::mcp) static OPENED: std::sync::Mutex<Vec<Opening>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(super) fn open(opening: &Opening) -> Result<(), AppError> {
    OPENED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(opening.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 只打开授权文件夹里不会直接运行的文件和_http_网址() {
        let root = crate::test_support::tempdir();
        let allowed = root.path().join("allowed");
        std::fs::create_dir_all(allowed.join("docs")).unwrap();
        for name in [
            "报告.pdf",
            "run.ps1",
            "tool.EXE",
            "a.js",
            "setup.msi",
            "x.lnk ",
            "app.py",
            "build.sh",
            "addin.xll",
            "宏.docm",
            "README",
            "photo.PNG",
            "notes.txt",
            "data.tar.gz",
        ] {
            std::fs::write(allowed.join(name.trim_end()), "x").unwrap();
        }
        std::fs::write(root.path().join("outside.pdf"), "x").unwrap();
        let folders = [allowed.clone()];
        let open = |path: &std::path::Path| check(&folders, path.to_str(), None);
        let pdf = allowed.join("报告.pdf");
        assert_eq!(open(&pdf).unwrap(), Opening::Path(pdf.clone()));
        // 文件夹也能打开（资源管理器）
        assert!(open(&allowed.join("docs")).is_ok());
        // 只放行文档、图片、音视频、文本、网页、压缩包：清单外的（装了别的程序就会运行的、没有扩展名的）都不打开
        for name in [
            "run.ps1",
            "tool.EXE",
            "a.js",
            "setup.msi",
            "x.lnk",
            "app.py",
            "build.sh",
            "addin.xll",
            "宏.docm",
            "README",
        ] {
            let error = open(&allowed.join(name)).unwrap_err();
            assert_eq!(error.code, "mcp.open_refused", "{name}");
            assert_eq!(
                error.params["reason_key"],
                "mcp.only_documents_media_and_archives_open"
            );
        }
        for name in ["photo.PNG", "notes.txt", "data.tar.gz"] {
            assert!(open(&allowed.join(name)).is_ok(), "{name}");
        }
        assert_eq!(
            open(&root.path().join("outside.pdf")).unwrap_err().code,
            "mcp.path_outside"
        );
        assert_eq!(
            open(&allowed.join("missing.pdf")).unwrap_err().code,
            "mcp.not_found"
        );
        assert_eq!(
            check(&[], pdf.to_str(), None).unwrap_err().code,
            "mcp.path_denied"
        );
        assert_eq!(
            check(&folders, None, Some("https://example.com/a?b=1")).unwrap(),
            Opening::Url("https://example.com/a?b=1".into())
        );
        for url in [
            "file:///C:/Windows/System32/calc.exe",
            "ms-settings:privacy",
            "javascript:alert(1)",
            "https://",
            "https://a b",
        ] {
            assert_eq!(
                check(&folders, None, Some(url)).unwrap_err().code,
                "link.invalid_url",
                "{url}"
            );
        }
        assert_eq!(
            action(&Opening::Path(pdf.clone())),
            format!("用默认程序打开：{}", pdf.display())
        );
        assert_eq!(
            action(&Opening::Url("https://example.com".into())),
            "用默认浏览器打开网址：https://example.com"
        );
    }
}
