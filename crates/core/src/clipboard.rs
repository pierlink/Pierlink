//! 读写系统剪贴板的文字。
//!
//! 界面运行在 WebView2 里，用网页的方式（navigator.clipboard）读剪贴板会弹出“是否允许读取剪贴板”的确认框；
//! 改由这里直接调用 Windows 接口读取，就不会弹窗；写入也改由这里做，窗口不在前台时也能写。
//!
//! 远程桌面把远程复制的文字写进来时，另写一个私有格式的来源标记（哪个桌面来的），读的时候一起读回：
//! 同一个桌面重连、开着两个查看窗口时认得出这段文字是它自己的，不再发回远程（发回去会让远程剪贴板
//! 只剩文字，文件管理器拿不到复制的文件）。Windows 上任何程序重新复制都会清空剪贴板，标记随之消失。

use crate::error::AppError;
use serde::Serialize;
use specta::Type;

const TEXT_LIMIT: usize = 16 * 1024 * 1024;
/// 来源标记的私有剪贴板格式名（同名注册在所有程序里得到同一个格式编号）
#[cfg_attr(not(windows), allow(dead_code))]
const ORIGIN_FORMAT: &str = "Pierlink.DesktopOrigin";
/// 来源值最长多少字节；界面传的是哈希，远小于这个
const ORIGIN_LIMIT: usize = 128;

/// 剪贴板里的文字和远程桌面写入时打的来源标记
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Type)]
pub struct ClipboardText {
    /// 文字；剪贴板里没有文字（如图片）时为 None
    pub text: Option<String>,
    /// 来源标记；不是远程桌面写的、或之后别的程序又复制过时为 None
    pub origin: Option<String>,
}

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

/// 读取剪贴板里的文字和来源标记（同一次打开剪贴板读出，两者对得上）。
///
/// :return: 文字和来源；剪贴板被别的程序占着时返回错误；其他系统同 read_text（没有来源）
pub fn read_marked_text() -> Result<ClipboardText, AppError> {
    #[cfg(windows)]
    {
        windows::read_marked_text()
    }
    #[cfg(not(windows))]
    {
        read_text().map(|text| ClipboardText { text, origin: None })
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
        windows::write_text(&text, None)
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        Err(AppError::new("clipboard.unsupported"))
    }
}

/// 将远程桌面复制来的文字写入系统剪贴板，同时写入来源标记。
///
/// :param text: 同 write_text
/// :param origin: 来源值：1–128 字节的 ASCII 字母、数字和 `-_.:`（界面传桌面的哈希，不放明文）
/// :return: 写入成功返回 ()（标记写不进去只记日志，文字照常可用）；来源值不合规返回 clipboard.invalid_origin；
///          其余同 write_text
pub fn write_marked_text(text: &str, origin: &str) -> Result<(), AppError> {
    let text = prepare_text(text)?;
    check_origin(origin)?;
    #[cfg(windows)]
    {
        windows::write_text(&text, Some(origin))
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

/// 来源值只许 1–128 字节的 ASCII 字母、数字和 `-_.:`（别的程序也能读到这个格式，不放任意内容）
fn check_origin(origin: &str) -> Result<(), AppError> {
    let valid = !origin.is_empty()
        && origin.len() <= ORIGIN_LIMIT
        && origin
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b));
    if valid {
        Ok(())
    } else {
        Err(AppError::new("clipboard.invalid_origin"))
    }
}

/// 从标记格式的数据里取出来源值：到第一个 0 为止（内存块常比内容大）；不合规的（别的程序写的同名格式）当作没有。
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_origin(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let origin = std::str::from_utf8(&bytes[..end]).ok()?;
    check_origin(origin).ok()?;
    Some(origin.to_string())
}

#[cfg(windows)]
mod windows {
    use crate::error::AppError;
    use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, HWND};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
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
        open_text()
    }

    pub(super) fn read_marked_text() -> Result<super::ClipboardText, AppError> {
        let _opened = open(std::ptr::null_mut())?;
        Ok(super::ClipboardText {
            text: open_text()?,
            origin: open_origin(),
        })
    }

    /// 来源标记的格式编号；注册不了（几乎不会）时为 None，这时不写也不读标记
    fn origin_format() -> Option<u32> {
        let name: Vec<u16> = super::ORIGIN_FORMAT.encode_utf16().chain(Some(0)).collect();
        // SAFETY: name 以 0 结尾，调用期间一直有效
        let id = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
        (id != 0).then_some(id)
    }

    /// 读出来源标记；调用前本线程已打开剪贴板。没有标记、读不出来或不合规时为 None
    fn open_origin() -> Option<String> {
        let format = origin_format()?;
        // SAFETY: 剪贴板已打开；句柄是剪贴板持有的全局内存，锁住后最多复制 ORIGIN_LIMIT + 1 字节再解锁
        unsafe {
            let handle = GetClipboardData(format);
            if handle.is_null() {
                return None;
            }
            let pointer = GlobalLock(handle) as *const u8;
            if pointer.is_null() {
                return None;
            }
            let length = GlobalSize(handle).min(super::ORIGIN_LIMIT + 1);
            let bytes = std::slice::from_raw_parts(pointer, length).to_vec();
            GlobalUnlock(handle);
            super::parse_origin(&bytes)
        }
    }

    /// 读出剪贴板里的文字；调用前本线程已打开剪贴板
    fn open_text() -> Result<Option<String>, AppError> {
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

    /// 写入文字；origin 有值时再写来源标记（写不进去只记日志：文字已在剪贴板里，只是这次认不出它是远程来的）
    pub(super) fn write_text(text: &str, origin: Option<&str>) -> Result<(), AppError> {
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let owner = OwnerWindow::create()?;
        // 逆序释放：先关闭剪贴板，再销毁本线程的临时窗口。
        let _opened = open(owner.0)?;
        if unsafe { EmptyClipboard() } == 0 {
            return Err(write_failed(
                "clipboard.couldn_t_clear_the_clipboard",
                "清空剪贴板失败",
            ));
        }
        set_data(CF_UNICODETEXT, &wide)?;
        if let Some(origin) = origin {
            let bytes: Vec<u8> = origin.bytes().chain(Some(0)).collect();
            match origin_format() {
                Some(format) => {
                    if let Err(e) = set_data(format, &bytes) {
                        eprintln!(
                            "写入剪贴板来源标记失败: 错误={}, 参数={:?}",
                            e.code, e.params
                        );
                    }
                }
                None => {
                    let status = unsafe { GetLastError() };
                    eprintln!("注册剪贴板来源标记格式失败: 状态={status}");
                }
            }
        }
        Ok(())
    }

    /// 把 data 复制进新分配的全局内存，以 format 放进剪贴板；调用前本线程已打开并清空剪贴板。
    /// 成功后内存归系统所有，不再释放；失败时在这里释放
    fn set_data<T: Copy>(format: u32, data: &[T]) -> Result<(), AppError> {
        let size = std::mem::size_of_val(data);
        // SAFETY: 新分配的内存锁住后只写 size 字节；交给剪贴板失败时由这里释放，成功后不再访问
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE, size);
            if memory.is_null() {
                return Err(write_failed(
                    "clipboard.couldn_t_allocate_clipboard_memory",
                    "分配剪贴板内存失败",
                ));
            }
            let pointer = GlobalLock(memory) as *mut u8;
            if pointer.is_null() {
                let error = write_failed(
                    "clipboard.couldn_t_lock_clipboard_memory",
                    "锁定剪贴板内存失败",
                );
                GlobalFree(memory);
                return Err(error);
            }
            std::ptr::copy_nonoverlapping(data.as_ptr().cast::<u8>(), pointer, size);
            GlobalUnlock(memory);
            if SetClipboardData(format, memory).is_null() {
                let error = write_failed(
                    "clipboard.couldn_t_set_clipboard_text",
                    "设置剪贴板文字失败",
                );
                GlobalFree(memory);
                return Err(error);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use crate::clipboard::{read_text, write_text};
        use std::sync::Mutex;

        /// 这些测试共用系统剪贴板：一个一个来，免得并行时互相改掉对方写的内容
        static CLIPBOARD: Mutex<()> = Mutex::new(());

        #[test]
        fn 剪贴板中文和多行原样读出() {
            let _one = CLIPBOARD.lock().unwrap_or_else(|p| p.into_inner());
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

        #[test]
        fn 远程桌面写入的文字带来源标记且普通写入清掉标记() {
            use crate::clipboard::{ClipboardText, read_marked_text, write_marked_text};
            let _one = CLIPBOARD.lock().unwrap_or_else(|p| p.into_inner());
            if write_marked_text("远程来的 🙂", "0123abcd").is_err() {
                eprintln!("跳过：这个会话没有可用的剪贴板");
                return;
            }
            assert_eq!(
                read_marked_text().unwrap(),
                ClipboardText {
                    text: Some("远程来的 🙂".into()),
                    origin: Some("0123abcd".into()),
                }
            );
            assert_eq!(read_text().unwrap().as_deref(), Some("远程来的 🙂"));
            write_text("本机复制的").unwrap();
            assert_eq!(
                read_marked_text().unwrap(),
                ClipboardText {
                    text: Some("本机复制的".into()),
                    origin: None,
                }
            );
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

    #[test]
    fn 来源值只许不长的字母数字和少数符号() {
        let longest = "a".repeat(ORIGIN_LIMIT);
        for origin in [
            "0123456789abcdef0123456789abcdef",
            "a",
            "srv-1:desk_2.x",
            longest.as_str(),
        ] {
            assert!(check_origin(origin).is_ok(), "{origin}");
        }
        let too_long = "a".repeat(ORIGIN_LIMIT + 1);
        for origin in ["", "中文", "a b", "a\0b", "a/b", too_long.as_str()] {
            assert_eq!(
                check_origin(origin).unwrap_err().code,
                "clipboard.invalid_origin"
            );
        }
    }

    #[test]
    fn 标记数据截到第一个空字节且不合规时当作没有() {
        assert_eq!(parse_origin(b"abc123\0garbage").as_deref(), Some("abc123"));
        assert_eq!(parse_origin(b"abc123").as_deref(), Some("abc123"));
        assert_eq!(parse_origin(b"\0abc"), None);
        assert_eq!(parse_origin(b"a b\0"), None);
        assert_eq!(parse_origin(&[0xff, 0xfe, 0]), None);
        assert_eq!(parse_origin(&[b'a'; ORIGIN_LIMIT + 1]), None);
    }

    #[test]
    fn 带标记写入在接触剪贴板前拒绝不合规的来源和超长输入() {
        for origin in ["", "服务器"] {
            assert_eq!(
                write_marked_text("x", origin).unwrap_err().code,
                "clipboard.invalid_origin"
            );
        }
        let error = write_marked_text(&"\0".repeat(TEXT_LIMIT + 1), "abc").unwrap_err();
        assert_eq!(error.code, "clipboard.too_large");
    }

    #[cfg(not(windows))]
    #[test]
    fn 非视窗平台读写返回不支持() {
        assert_eq!(read_text().unwrap_err().code, "clipboard.unsupported");
        assert_eq!(
            read_marked_text().unwrap_err().code,
            "clipboard.unsupported"
        );
        for text in ["", "中文\0🙂"] {
            assert_eq!(write_text(text).unwrap_err().code, "clipboard.unsupported");
            assert_eq!(
                write_marked_text(text, "abc").unwrap_err().code,
                "clipboard.unsupported"
            );
        }
    }
}
