//! 检查网页链接并交给系统默认浏览器。

use crate::error::AppError;

const URL_LIMIT: usize = 8192;

/// 用系统默认浏览器打开网页链接。
///
/// :param url: 最多 8192 字节、带主机名的 HTTP 或 HTTPS 网址；不允许空白、ASCII 控制字符和双引号
/// :return: 成功交给系统启动器返回 ()，不等待浏览器退出；网址不合格或启动失败时返回错误
pub fn open_in_browser(url: &str) -> Result<(), AppError> {
    let url = validate_url(url)?;
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

        let verb: Vec<u16> = "open\0".encode_utf16().collect();
        let wide: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
        let status = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                wide.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        } as isize;
        if status <= 32 {
            Err(AppError::new("link.open_failed").with("status", status))
        } else {
            Ok(())
        }
    }
    #[cfg(not(windows))]
    {
        launch(&mut browser_command(url))
    }
}

fn invalid_url(text_key: &'static str, reason: &str) -> AppError {
    AppError::new("link.invalid_url").with_reason(text_key, reason)
}

fn validate_url(url: &str) -> Result<&str, AppError> {
    if url.len() > URL_LIMIT {
        return Err(invalid_url(
            "links.url_exceeds_8192_bytes",
            "网址不能超过 8192 字节",
        ));
    }
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_ascii_control() || c == '"')
    {
        return Err(invalid_url(
            "links.url_cannot_contain_whitespace_ascii_control_characters_or_double_quotes",
            "网址不能包含空白、ASCII 控制字符或双引号",
        ));
    }
    let (_, rest) = url
        .split_once("://")
        .filter(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
        })
        .ok_or_else(|| {
            invalid_url(
                "links.only_http_and_https_urls_are_allowed",
                "只允许以 http:// 或 https:// 开头的网址",
            )
        })?;
    // 浏览器也将反斜杠视为路径分隔符；用户信息和端口都不能充当主机名。
    let authority = rest.split(['/', '\\', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split_once(']')
            .map(|(host, _)| host)
            .unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    if host.is_empty() {
        return Err(invalid_url(
            "links.url_must_include_a_hostname",
            "网址必须包含主机名",
        ));
    }
    Ok(url)
}

#[cfg(not(windows))]
fn browser_command(url: &str) -> std::process::Command {
    let mut command = std::process::Command::new("xdg-open");
    command.arg(url);
    command
}

#[cfg(not(windows))]
fn launch(command: &mut std::process::Command) -> Result<(), AppError> {
    use std::process::{Child, Stdio};

    // 先创建回收线程，避免线程创建失败时留下无人等待的子进程。
    let (sender, receiver) = std::sync::mpsc::channel::<Child>();
    std::thread::Builder::new()
        .name("浏览器启动器回收".into())
        .spawn(move || {
            if let Ok(mut child) = receiver.recv() {
                let _ = child.wait();
            }
        })
        .map_err(|_| {
            AppError::new("link.open_failed").with_reason(
                "links.couldn_t_create_the_launcher_cleanup_thread",
                "创建启动器回收线程失败",
            )
        })?;
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            AppError::new("link.open_failed")
                .with_reason(
                    "links.couldn_t_launch_the_default_browser",
                    "启动默认浏览器失败",
                )
                .with("status", error.raw_os_error().unwrap_or(0))
        })?;
    if let Err(error) = sender.send(child) {
        let mut child = error.0;
        let _ = child.kill();
        let _ = child.wait();
        return Err(AppError::new("link.open_failed").with_reason(
            "links.launcher_cleanup_thread_has_exited",
            "启动器回收线程已退出",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_invalid(url: &str) {
        let error = validate_url(url).unwrap_err();
        assert_eq!(error.code, "link.invalid_url");
        assert_eq!(error.params.len(), 2);
        assert!(error.params["reason_key"].starts_with("links."));
        assert!(!error.params["reason"].is_ascii());
    }

    #[test]
    fn 网页协议不区分大小写并保留完整网址() {
        for url in [
            "http://example.com",
            "https://example.com/",
            "HTTPS://example.com",
            "hTtP://localhost:8080/path?key=value&other=1#anchor",
            "http://127.0.0.1:8080/",
            "http://[::1]:8080/",
            "https://[2001:db8::1]/path?q=1#anchor",
            "https://例子.测试/中文/🙂?查询=值#锚点",
            "https://user:password@example.com/path?token=value",
            "https://example.com/%20?q=%22%00",
        ] {
            assert_eq!(validate_url(url).unwrap(), url);
        }
    }

    #[test]
    fn 拒绝其他协议相对地址和空网址() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "mailto:user@example.com",
            "ftp://example.com",
            "//example.com/path",
            "/relative",
            "example.com",
            "",
            "http:example.com",
            "https:/example.com",
            "httpsx://example.com",
        ] {
            assert_invalid(url);
        }
    }

    #[test]
    fn 拒绝只有路径端口或用户信息而没有主机的网址() {
        for url in [
            "http://",
            "https:///x",
            "https://?query=value",
            "https://#anchor",
            "http://:8080/",
            "https://user:password@/path",
            "https://user@:8080/",
            "https://[]:8080/",
            "https://[::1/",
            "https://\\example.com",
        ] {
            assert_invalid(url);
        }
    }

    #[test]
    fn 拒绝各位置的空白和双引号() {
        for character in [' ', '\t', '\n', '\r', '\u{85}', '\u{a0}', '\u{3000}', '"'] {
            for url in [
                format!("{character}https://example.com"),
                format!("https://exam{character}ple.com"),
                format!("https://example.com/path{character}?key=value"),
                format!("https://example.com/?key={character}"),
                format!("https://example.com/#{character}"),
            ] {
                assert_invalid(&url);
            }
        }
    }

    #[test]
    fn 拒绝全部低位控制字符和删除字符() {
        for byte in (0u8..=31).chain(Some(127)) {
            assert_invalid(&format!("https://example.com/a{}b", char::from(byte)));
        }
    }

    #[test]
    fn 网址长度按字节计算且允许恰好达到上限() {
        let prefix = "https://example.com/";
        let mut url = format!("{prefix}{}", "中".repeat((URL_LIMIT - prefix.len()) / 3));
        url.push_str(&"a".repeat(URL_LIMIT - url.len()));
        assert_eq!(url.len(), URL_LIMIT);
        assert_eq!(validate_url(&url).unwrap(), url);
        url.push('a');
        assert_invalid(&url);
    }

    #[test]
    fn 打开接口在启动前拒绝非法网址且错误不泄露凭据() {
        let error =
            open_in_browser("https://user:secret@example.com/?token=private\n").unwrap_err();
        assert_eq!(error.code, "link.invalid_url");
        let serialized = serde_json::to_string(&error).unwrap();
        for secret in ["user", "secret", "example.com", "token", "private"] {
            assert!(!serialized.contains(secret));
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn 启动器接收完整网址作为单个参数() {
        let url = "https://example.com/中文?q=$(touch${IFS}file);`id`&other=1#anchor";
        let command = browser_command(validate_url(url).unwrap());
        assert_eq!(command.get_program(), "xdg-open");
        assert_eq!(command.get_args().collect::<Vec<_>>(), [url]);
    }

    #[cfg(not(windows))]
    #[test]
    fn 启动程序不存在时返回脱敏错误() {
        let root = crate::test_support::tempdir();
        let mut command = std::process::Command::new(root.path().join("不存在的启动器"));
        command.arg("https://user:secret@example.com/?token=private");
        let error = launch(&mut command).unwrap_err();
        assert_eq!(error.code, "link.open_failed");
        assert_eq!(error.params["reason"], "启动默认浏览器失败");
        assert_eq!(error.params["status"], "2");
        let serialized = serde_json::to_string(&error).unwrap();
        for secret in ["secret", "example.com", "private", "不存在的启动器"] {
            assert!(!serialized.contains(secret));
        }
    }
}
