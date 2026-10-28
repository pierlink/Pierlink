//! 读取系统剪贴板的文字。
//!
//! 界面运行在 WebView2 里，用网页的方式（navigator.clipboard）读剪贴板会弹出“是否允许读取剪贴板”的确认框；
//! 改由这里直接调用 Windows 接口读取，就不会弹窗。

use crate::error::AppError;

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

#[cfg(windows)]
mod windows {
    use crate::error::AppError;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    /// 剪贴板的 UTF-16 文字格式
    const CF_UNICODETEXT: u32 = 13;

    /// 打开剪贴板期间持有，离开作用域时关闭
    pub(super) struct Opened;
    impl Drop for Opened {
        fn drop(&mut self) {
            unsafe {
                CloseClipboard();
            }
        }
    }

    /// 打开剪贴板：别的程序正占用时稍等重试（最多约 0.5 秒）
    pub(super) fn open() -> Result<Opened, AppError> {
        for _ in 0..25 {
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Ok(Opened);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Err(AppError::new("clipboard.busy"))
    }

    pub(super) fn read_text() -> Result<Option<String>, AppError> {
        let _opened = open()?;
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
        use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc};

        /// 测试用：把文字写进剪贴板
        fn write(text: &str) -> bool {
            let Ok(_opened) = open() else { return false };
            let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
            unsafe {
                EmptyClipboard();
                let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
                if mem.is_null() {
                    return false;
                }
                let p = GlobalLock(mem) as *mut u16;
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                GlobalUnlock(mem);
                !SetClipboardData(CF_UNICODETEXT, mem).is_null()
            }
        }

        #[test]
        fn windows_剪贴板中文和多行原样读出() {
            let text = "第一行 中文\r\n第二行 $(echo) \"引号\" 🙂";
            if !write(text) {
                eprintln!("跳过：这个会话没有可用的剪贴板");
                return;
            }
            assert_eq!(read_text().unwrap().as_deref(), Some(text));
        }
    }
}
