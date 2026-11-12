//! 当前用户注册表（HKCU）的几个小操作，不用管理员权限：资源管理器右键菜单、开机自动启动用。只在 Windows 上编译。
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegSetValueExW,
};

/// 以 0 结尾的 UTF-16
fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// 注册表函数的返回码转成结果。
///
/// :param rc: 返回码
/// :param ok_if_missing: 要删的东西本来就没有时算成功
/// :return: 失败时是系统错误
fn check(rc: u32, ok_if_missing: bool) -> io::Result<()> {
    if rc == ERROR_SUCCESS || (ok_if_missing && rc == ERROR_FILE_NOT_FOUND) {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(rc as i32))
    }
}

/// 在 HKCU 下建好 key（已有就打开），写一个字符串值。
///
/// :param key: HKCU 下的路径
/// :param name: 值名；None 时写默认值
/// :param value: 要写的字符串
/// :return: 失败时是系统错误
pub fn set(key: &str, name: Option<&str>, value: &OsStr) -> io::Result<()> {
    let key_w = wide(OsStr::new(key));
    let name_w = name.map(|n| wide(OsStr::new(n)));
    let data = wide(value);
    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY：字符串都以 0 结尾、在调用期间有效；hkey 成功后由下面关闭
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key_w.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        )
    };
    check(rc, false)?;
    // SAFETY：data 是 u16 数组，按字节长度传入（含结尾的 0）
    let rc = unsafe {
        RegSetValueExW(
            hkey,
            name_w.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
            0,
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
    // SAFETY：hkey 由上面成功打开
    unsafe { RegCloseKey(hkey) };
    check(rc, false)
}

/// 删掉 HKCU 下的一个 key 和它下面的所有内容（本来就没有也算成功）。
///
/// :param key: HKCU 下的路径
/// :return: 失败时是系统错误
pub fn delete_tree(key: &str) -> io::Result<()> {
    let key_w = wide(OsStr::new(key));
    // SAFETY：key_w 以 0 结尾
    let rc = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key_w.as_ptr()) };
    check(rc, true)
}

/// 删掉 HKCU 下某个 key 里的一个值（key 或值本来就没有也算成功）。
///
/// :param key: HKCU 下的路径
/// :param name: 值名
/// :return: 失败时是系统错误
pub fn delete_value(key: &str, name: &str) -> io::Result<()> {
    let key_w = wide(OsStr::new(key));
    let name_w = wide(OsStr::new(name));
    // SAFETY：两个字符串都以 0 结尾、在调用期间有效
    let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key_w.as_ptr(), name_w.as_ptr()) };
    check(rc, true)
}
