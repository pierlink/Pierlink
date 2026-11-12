//! 开机自动启动（设置里打开，默认关）。
//!
//! 写在当前用户的 HKCU\Software\Microsoft\Windows\CurrentVersion\Run 下，不用管理员权限：登录 Windows 后以
//! `"<本程序>" --autostart` 启动，窗口不弹出，只在托盘里（托盘建不起来时照常显示窗口）。
//! 在任务管理器“启动应用”里禁用时，Windows 另记在 StartupApproved 下，这里不去改它。

/// 命令行参数：开机自动启动的，启动时不显示窗口
pub const AUTOSTART_ARG: &str = "--autostart";
/// Run 下的值名。卸载程序（nsis/hooks.nsh）删的也是它，改名要一起改
#[cfg(windows)]
const RUN_VALUE: &str = "Pierlink";
#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// 命令行里有没有开机自动启动的标记。
///
/// :param args: 命令行参数（含不含程序名都可以）
/// :return: 是开机自动启动的
pub fn launched_at_login<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> bool {
    args.iter().any(|a| a.as_ref() == AUTOSTART_ARG)
}

/// 界面把设置同步过来：打开时写入（每次启动都按当前程序的位置重写），关掉时删除。只在 Windows 上做事。
///
/// :param enabled: 设置里“开机自动启动”是否打开
#[tauri::command]
#[specta::specta]
pub async fn autostart_config(enabled: bool) {
    #[cfg(windows)]
    {
        // 开发版（tauri dev）不动：不然会改成指向开发目录里的程序，盖掉装好的版本
        if cfg!(debug_assertions) {
            eprintln!("开发版不同步开机自动启动: 打开={enabled}");
            return;
        }
        let done = tauri::async_runtime::spawn_blocking(move || {
            if enabled {
                register()
            } else {
                crate::hkcu::delete_value(RUN_KEY, RUN_VALUE)
            }
        })
        .await;
        match done {
            Ok(Ok(())) => eprintln!("开机自动启动已同步: 打开={enabled}"),
            Ok(Err(e)) => eprintln!("开机自动启动同步失败: 打开={enabled}, 错误={e}"),
            Err(e) => eprintln!("开机自动启动同步失败: 打开={enabled}, 错误={e}"),
        }
    }
    #[cfg(not(windows))]
    let _ = enabled;
}

/// 写入 Run 值：`"<本程序>" --autostart`
#[cfg(windows)]
fn register() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut command = std::ffi::OsString::from("\"");
    command.push(exe.as_os_str());
    command.push(format!("\" {AUTOSTART_ARG}"));
    crate::hkcu::set(RUN_KEY, Some(RUN_VALUE), &command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 认出开机自动启动的标记() {
        assert!(launched_at_login(&["--autostart"]));
        // 单实例插件转来的参数里第一个是程序名
        assert!(launched_at_login(&[
            r"C:\Pierlink\Pierlink.exe",
            "--autostart"
        ]));
        assert!(!launched_at_login(&["Pierlink.exe"]));
        // 只认单独的参数，目录名里带这几个字不算
        assert!(!launched_at_login(&[
            "Pierlink.exe",
            "--open-dir",
            r"D:\--autostart"
        ]));
        let none: [&str; 0] = [];
        assert!(!launched_at_login(&none));
    }
}
