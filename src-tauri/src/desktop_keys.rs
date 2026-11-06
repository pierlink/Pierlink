//! 远程桌面里的 Ctrl+空格。Windows 装了中文输入法时，这是系统切换输入法的快捷键，Windows 自己先处理掉，
//! 程序收不到，也就转不到远程桌面。远程桌面画面有焦点、Pierlink 在前台时，用低级键盘钩子先截住这一下的空格，
//! 交给界面转发给远程（Ctrl 照常经过界面）。只截这一个组合，其他快捷键照常归 Windows。
use tauri::AppHandle;

/// 截住的空格交给界面：true 为按下，false 为松开
pub const CTRL_SPACE_EVENT: &str = "pierlink://desktop-ctrl-space";

/// 远程桌面画面拿到焦点时打开、失去焦点时关掉：打开期间 Ctrl+空格不让 Windows 拿去切输入法，交给远程桌面。只在 Windows 上起作用。
#[tauri::command]
#[specta::specta]
pub fn desktop_capture_ctrl_space(app: AppHandle, enabled: bool) {
    #[cfg(windows)]
    hook::set(app, enabled);
    #[cfg(not(windows))]
    let _ = (app, enabled);
}

#[cfg(windows)]
mod hook {
    use super::CTRL_SPACE_EVENT;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tauri::{AppHandle, Emitter};
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SPACE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, HC_ACTION,
        KBDLLHOOKSTRUCT, MSG, SetWindowsHookExW, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
        WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    /// 远程桌面画面现在有焦点
    static CAPTURE: AtomicBool = AtomicBool::new(false);
    /// 截住了空格的按下：松开也要截（Ctrl 可能先松开，或者这期间焦点已经离开）
    static SPACE_TAKEN: AtomicBool = AtomicBool::new(false);
    static APP: OnceLock<AppHandle> = OnceLock::new();
    static STARTED: OnceLock<()> = OnceLock::new();

    pub(super) fn set(app: AppHandle, enabled: bool) {
        let _ = APP.set(app);
        CAPTURE.store(enabled, Ordering::Relaxed);
        if enabled {
            STARTED.get_or_init(start);
        }
    }

    /// 钩子要装在有消息循环的线程上：单开一个，一直跑到程序退出
    fn start() {
        let spawned = std::thread::Builder::new()
            .name("远程桌面按键".into())
            .spawn(|| {
                // SAFETY：钩子函数是 'static 的；本线程一直跑消息循环，进程退出时系统收回钩子
                unsafe {
                    let hook = SetWindowsHookExW(
                        WH_KEYBOARD_LL,
                        Some(on_key),
                        GetModuleHandleW(std::ptr::null()),
                        0,
                    );
                    if hook.is_null() {
                        eprintln!("远程桌面按键钩子安装失败");
                        return;
                    }
                    let mut msg: MSG = std::mem::zeroed();
                    while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {}
                }
            });
        if let Err(e) = spawned {
            eprintln!("远程桌面按键线程启动失败: 错误={e}");
        }
    }

    /// 前台窗口是 Pierlink 自己的（主窗口或单独打开的桌面窗口）
    fn ours_in_front() -> bool {
        // SAFETY：只读取前台窗口和它所属的进程
        unsafe {
            let window = GetForegroundWindow();
            if window.is_null() {
                return false;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(window, &mut pid);
            pid == GetCurrentProcessId()
        }
    }

    fn held(vk: u16) -> bool {
        // SAFETY：只读取按键状态
        unsafe { GetAsyncKeyState(i32::from(vk)) < 0 }
    }

    /// 低级键盘钩子：只看空格。按住 Ctrl（没按 Alt、Win）按下空格、远程桌面有焦点、Pierlink 在前台时截住；
    /// 其他一律交给下一个钩子。钩子必须很快返回，这里只读状态、发一个事件
    unsafe extern "system" fn on_key(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            // SAFETY：HC_ACTION 时 lparam 指向系统给的 KBDLLHOOKSTRUCT
            let key = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            if key.vkCode == u32::from(VK_SPACE) {
                let msg = wparam as u32;
                let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
                let take = if down {
                    CAPTURE.load(Ordering::Relaxed)
                        && held(VK_CONTROL)
                        && !held(VK_MENU)
                        && !held(VK_LWIN)
                        && !held(VK_RWIN)
                        && ours_in_front()
                } else {
                    up && SPACE_TAKEN.load(Ordering::Relaxed)
                };
                if take {
                    SPACE_TAKEN.store(down, Ordering::Relaxed);
                    if let Some(app) = APP.get() {
                        let _ = app.emit(CTRL_SPACE_EVENT, down);
                    }
                    return 1;
                }
            }
        }
        // SAFETY：原样交给下一个钩子
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }
}
