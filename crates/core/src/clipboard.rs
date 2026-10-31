//! 读写系统剪贴板的文字。
//!
//! 界面运行在 WebView2 里，用网页的方式（navigator.clipboard）读剪贴板会弹出“是否允许读取剪贴板”的确认框；
//! 改由这里直接调用 Windows 接口读取，就不会弹窗；写入也改由这里做，窗口不在前台时也能写。

use crate::error::AppError;

const TEXT_LIMIT: usize = 16 * 1024 * 1024;

/// 读取剪贴板里的文字。
///
/// :return: 文字；剪贴板里没有文字（如图片）时返回 None；剪贴板被别的程序占着、读不到时返回错误
pub fn read_text() -> Result<Option<String>, AppError> {
    #[cfg(windows)]
    {
        windows::read_text()
    }
    #[cfg(not(windows))]
    {
        Err(AppError::new("clipboard.unsupported"))
    }
}

/// 将文字写入系统剪贴板。
///
/// :param text: 最多 16 MiB 的 UTF-8 文字；移除内含的空字符，空字符串表示清空文字
/// :return: 写入成功返回 ()；超长、剪贴板忙、写入失败或平台不支持时返回错误
pub fn write_text(text: &str) -> Result<(), AppError> {
    let text = prepare_text(text)?;
    #[cfg(windows)]
    {
        windows::write_text(&text)
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        Err(AppError::new("clipboard.unsupported"))
    }
}

fn prepare_text(text: &str) -> Result<String, AppError> {
    // 在复制前限制输入字节数，避免为超长输入分配额外内存。
    if text.len() > TEXT_LIMIT {
        return Err(AppError::new("clipboard.too_large").with("limit", TEXT_LIMIT));
    }
    Ok(text.replace('\0', ""))
}

#[cfg(windows)]
mod windows {
    use crate::error::AppError;
    use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, HWND};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, HWND_MESSAGE,
    };

    /// 剪贴板的 UTF-16 文字格式
    const CF_UNICODETEXT: u32 = 13;

    /// 打开剪贴板期间持有，离开作用域时关闭
    struct Opened;
    impl Drop for Opened {
        fn drop(&mut self) {
            unsafe {
                CloseClipboard();
            }
        }
    }

    /// 打开剪贴板：别的程序正占用时稍等重试（最多约 0.5 秒）
    fn open(owner: HWND) -> Result<Opened, AppError> {
        for _ in 0..25 {
            if unsafe { OpenClipboard(owner) } != 0 {
                return Ok(Opened);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Err(AppError::new("clipboard.busy"))
    }

    pub(super) fn read_text() -> Result<Option<String>, AppError> {
        let _opened = open(std::ptr::null_mut())?;
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
            return Ok(None);
        }
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
        if handle.is_null() {
            return Ok(None);
        }
        let ptr = unsafe { GlobalLock(handle) } as *const u16;
        if ptr.is_null() {
            return Err(AppError::new("clipboard.read_failed"));
        }
        // 按内存块大小读取，并在第一个 0 处截断（不越界）
        let units = unsafe { GlobalSize(handle) } / 2;
        let slice = unsafe { std::slice::from_raw_parts(ptr, units) };
        let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
        let text = String::from_utf16_lossy(&slice[..end]);
        unsafe {
            GlobalUnlock(handle);
        }
        Ok(Some(text))
    }

    // 写入必须提供窗口所有者；空句柄在清空剪贴板后会使 SetClipboardData 失败。
    struct OwnerWindow(HWND);

    impl OwnerWindow {
        fn create() -> Result<Self, AppError> {
            let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            let window = unsafe {
                CreateWindowExW(
                    0,
                    class.as_ptr(),
                    std::ptr::null(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            if window.is_null() {
                Err(write_failed(
                    "clipboard.couldn_t_create_the_clipboard_window",
                    "创建剪贴板窗口失败",
                ))
            } else {
                Ok(Self(window))
            }
        }
    }

    impl Drop for OwnerWindow {
        fn drop(&mut self) {
            unsafe {
                DestroyWindow(self.0);
            }
        }
    }

    fn write_failed(text_key: &'static str, reason: &str) -> AppError {
        AppError::new("clipboard.write_failed")
            .with_reason(text_key, reason)
            .with("status", unsafe { GetLastError() })
    }

    pub(super) fn write_text(text: &str) -> Result<(), AppError> {
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let owner = OwnerWindow::create()?;
        // 逆序释放：先关闭剪贴板，再销毁本线程的临时窗口。
        let _opened = open(owner.0)?;
        unsafe {
            if EmptyClipboard() == 0 {
                return Err(write_failed(
                    "clipboard.couldn_t_clear_the_clipboard",
                    "清空剪贴板失败",
                ));
            }
            let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * size_of::<u16>());
            if memory.is_null() {
                return Err(write_failed(
                    "clipboard.couldn_t_allocate_clipboard_memory",
                    "分配剪贴板内存失败",
                ));
            }
            let pointer = GlobalLock(memory) as *mut u16;
            if pointer.is_null() {
                let error = write_failed(
                    "clipboard.couldn_t_lock_clipboard_memory",
                    "锁定剪贴板内存失败",
                );
                GlobalFree(memory);
                return Err(error);
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), pointer, wide.len());
            GlobalUnlock(memory);
            if SetClipboardData(CF_UNICODETEXT, memory).is_null() {
                let error = write_failed(
                    "clipboard.couldn_t_set_clipboard_text",
                    "设置剪贴板文字失败",
                );
                GlobalFree(memory);
                return Err(error);
            }
            // 成功后内存归系统所有，不再释放。
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use crate::clipboard::{read_text, write_text};

        #[test]
        fn 剪贴板中文和多行原样读出() {
            let text = "第一行 中文\r\n第二行 $(echo) \"引号\" 🙂";
            if write_text(text).is_err() {
                eprintln!("跳过：这个会话没有可用的剪贴板");
                return;
            }
            assert_eq!(read_text().unwrap().as_deref(), Some(text));
            for (text, expected) in [("中\0文\0🙂", "中文🙂"), ("", "")] {
                write_text(text).unwrap();
                assert_eq!(read_text().unwrap().as_deref(), Some(expected));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 移除空字符并保留中文多行和字面反斜杠() {
        assert_eq!(
            prepare_text("\0中\0文\r\n🙂\\0\0").unwrap(),
            "中文\r\n🙂\\0"
        );
    }

    #[test]
    fn 空文字和全空字符通过检查() {
        assert_eq!(prepare_text("").unwrap(), "");
        assert_eq!(prepare_text("\0\0").unwrap(), "");
    }

    #[test]
    fn 长度按输入字节计算并允许恰好达到上限() {
        let mut text = "中".repeat(TEXT_LIMIT / 3);
        text.push('a');
        assert_eq!(text.len(), TEXT_LIMIT);
        assert_eq!(prepare_text(&text).unwrap(), text);
        text.push('字');
        let error = prepare_text(&text).unwrap_err();
        assert_eq!(error.code, "clipboard.too_large");
        assert_eq!(error.params.len(), 1);
        assert_eq!(error.params["limit"], TEXT_LIMIT.to_string());
    }

    #[test]
    fn 写入接口在接触剪贴板前拒绝超长输入() {
        let error = write_text(&"\0".repeat(TEXT_LIMIT + 1)).unwrap_err();
        assert_eq!(error.code, "clipboard.too_large");
    }

    #[cfg(not(windows))]
    #[test]
    fn 非视窗平台读写返回不支持() {
        assert_eq!(read_text().unwrap_err().code, "clipboard.unsupported");
        for text in ["", "中文\0🙂"] {
            assert_eq!(write_text(text).unwrap_err().code, "clipboard.unsupported");
        }
    }
}
