//! 资源管理器右键菜单“在 Pierlink 中打开”（设置里可以关）。
//!
//! 注册在当前用户的 HKCU\Software\Classes 下，不用管理员权限：文件夹、文件夹空白处（含桌面）、磁盘三处各一项，
//! 点了以 `Pierlink.exe --open-dir "<目录>"` 启动。Pierlink 已在运行时，单实例插件把参数转给已有实例，
//! 由界面在这个目录新开本机终端。Windows 11 的新菜单只放有程序包身份的扩展，这几项在“显示更多选项”里。
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// 命令行参数：后面跟要打开的目录
pub const OPEN_DIR_ARG: &str = "--open-dir";
/// 有新的“在这个目录打开终端”请求（界面收到后用 app_take_open_dirs 取走）
const OPEN_DIR_EVENT: &str = "pierlink://open-dir";
/// 三处菜单项在 HKCU 下的位置。卸载程序（nsis/hooks.nsh）删的也是这三个，改名要一起改
#[cfg(windows)]
const KEYS: [&str; 3] = [
    r"Software\Classes\Directory\Background\shell\Pierlink",
    r"Software\Classes\Directory\shell\Pierlink",
    r"Software\Classes\Drive\shell\Pierlink",
];

/// 还没交给界面的目录：启动时命令行带的、运行中再次启动时转过来的
#[derive(Default)]
pub struct OpenDirs(Mutex<Vec<String>>);

impl OpenDirs {
    /// 启动时命令行里带了目录就先放进来，界面加载好后来取。
    ///
    /// :param args: 本进程的命令行参数（不含程序名也可以）
    /// :return: 队列
    pub fn from_args<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Self {
        Self(Mutex::new(parse_open_dir(args).into_iter().collect()))
    }
}

/// 从命令行参数里取 `--open-dir <目录>`。
///
/// 资源管理器按 "%V" 传目录：磁盘根目录 `"C:\"` 按 Windows 的命令行规则会被解析成 `C:"`（反斜杠转义了后面的引号）。
/// 路径里不会有引号，结尾的引号只能是这样来的，换回反斜杠。
///
/// :param args: 命令行参数
/// :return: 目录；没有这个参数或目录为空时返回 None
pub fn parse_open_dir<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Option<String> {
    let i = args.iter().position(|a| a.as_ref() == OPEN_DIR_ARG)?;
    let raw = args.get(i + 1)?.as_ref().to_string_lossy();
    let dir = match raw.strip_suffix('"') {
        Some(head) => format!("{head}\\"),
        None => raw.into_owned(),
    };
    (!dir.trim().is_empty()).then_some(dir)
}

/// 运行中又从右键菜单打开（单实例插件转来另一个进程的参数）：目录交给界面，切到工作区。
///
/// :param app: 应用句柄
/// :param args: 另一个进程的命令行参数
/// :return: 参数里有要打开的目录
pub fn forward(app: &AppHandle, args: &[String]) -> bool {
    let Some(dir) = parse_open_dir(args) else {
        return false;
    };
    eprintln!("右键菜单打开目录: 目录={dir}");
    app.state::<OpenDirs>()
        .0
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(dir);
    let main = crate::app_shell::MAIN;
    if let Err(e) = app.emit_to(main, crate::app_shell::OPEN_PAGE_EVENT, "workspace") {
        eprintln!("切到工作区失败: 错误={e}");
    }
    if let Err(e) = app.emit_to(main, OPEN_DIR_EVENT, ()) {
        eprintln!("通知界面打开目录失败: 错误={e}");
    }
    true
}

/// 界面取走等着打开的目录（界面加载好时、收到 pierlink://open-dir 时调用），每个目录在新标签里开一个本机终端。
///
/// :return: 目录列表（取走后清空）
#[tauri::command]
#[specta::specta]
pub fn app_take_open_dirs(app: AppHandle) -> Vec<String> {
    std::mem::take(
        &mut *app
            .state::<OpenDirs>()
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner()),
    )
}

/// 界面把设置同步过来：打开时注册（或更新）右键菜单项，文字随界面语言；关掉时删除。只在 Windows 上做事。
///
/// :param enabled: 设置里“资源管理器右键菜单”是否打开
/// :param label: 菜单项文字（当前界面语言）
#[tauri::command]
#[specta::specta]
pub async fn explorer_menu_config(enabled: bool, label: String) {
    #[cfg(windows)]
    {
        // 开发版（tauri dev）不动右键菜单：不然会改成指向开发目录里的程序，盖掉装好的版本
        if cfg!(debug_assertions) {
            eprintln!("开发版不同步资源管理器右键菜单: 打开={enabled}");
            return;
        }
        let label: String = label.chars().filter(|c| !c.is_control()).take(80).collect();
        let done = tauri::async_runtime::spawn_blocking(move || {
            if enabled {
                registry::register(&label)
            } else {
                registry::unregister()
            }
        })
        .await;
        match done {
            Ok(Ok(())) => eprintln!("资源管理器右键菜单已同步: 打开={enabled}"),
            Ok(Err(e)) => eprintln!("资源管理器右键菜单同步失败: 打开={enabled}, 错误={e}"),
            Err(e) => eprintln!("资源管理器右键菜单同步失败: 打开={enabled}, 错误={e}"),
        }
    }
    #[cfg(not(windows))]
    let _ = (enabled, label);
}

#[cfg(windows)]
mod registry {
    use super::{KEYS, OPEN_DIR_ARG};
    use crate::hkcu;
    use std::ffi::{OsStr, OsString};
    use std::io;

    /// 注册三处菜单项：文字、图标（本程序）、命令 `"<本程序>" --open-dir "%V"`
    pub(super) fn register(label: &str) -> io::Result<()> {
        let exe = std::env::current_exe()?;
        let mut command = OsString::from("\"");
        command.push(exe.as_os_str());
        command.push(format!("\" {OPEN_DIR_ARG} \"%V\""));
        for key in KEYS {
            hkcu::set(key, None, OsStr::new(label))?;
            hkcu::set(key, Some("Icon"), exe.as_os_str())?;
            hkcu::set(&format!(r"{key}\command"), None, &command)?;
        }
        Ok(())
    }

    /// 删掉三处菜单项（本来就没有也算成功）
    pub(super) fn unregister() -> io::Result<()> {
        for key in KEYS {
            hkcu::delete_tree(key)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 取命令行里的目录() {
        assert_eq!(
            parse_open_dir(&["Pierlink.exe", "--open-dir", r"D:\work\项目 A"]).as_deref(),
            Some(r"D:\work\项目 A")
        );
        // 磁盘根目录："C:\" 被 Windows 的命令行规则解析成 C:"
        assert_eq!(
            parse_open_dir(&["Pierlink.exe", "--open-dir", "C:\""]).as_deref(),
            Some(r"C:\")
        );
        assert_eq!(parse_open_dir(&["Pierlink.exe"]), None);
        assert_eq!(parse_open_dir(&["Pierlink.exe", "--open-dir"]), None);
        assert_eq!(parse_open_dir(&["Pierlink.exe", "--open-dir", "  "]), None);
        // 单实例插件转来的参数里第一个是程序名，前面多几个别的参数也能找到
        assert_eq!(
            parse_open_dir(&["x.exe", "--other", "--open-dir", r"\\server\share"]).as_deref(),
            Some(r"\\server\share")
        );
    }

    #[test]
    fn 启动参数里的目录先放进队列() {
        let dirs = OpenDirs::from_args(&["--open-dir", r"E:\"]);
        assert_eq!(*dirs.0.lock().unwrap(), vec![r"E:\".to_string()]);
        assert!(OpenDirs::from_args(&["--mcp"]).0.lock().unwrap().is_empty());
    }
}
