//! 根据直接子进程查询本机 shell 的忙碌状态；不可确认时返回未知。
#[cfg(unix)]
fn stat(pid: u32) -> std::io::Result<(u32, u64)> {
    let value = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse_stat(&value)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "进程属性无效"))
}
#[cfg(unix)]
fn parse_stat(value: &str) -> Option<(u32, u64)> {
    let fields: Vec<_> = value.rsplit_once(") ")?.1.split_whitespace().collect();
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}
#[cfg(unix)]
pub(super) fn query(pid: u32) -> Option<bool> {
    let (_, created) = stat(pid).ok()?;
    let mut unknown = false;
    for entry in std::fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        let Some(child) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        match stat(child) {
            Ok((parent, start)) if parent == pid && start >= created => return Some(true),
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => unknown = true,
        }
    }
    if stat(pid).ok()?.1 != created || unknown {
        None
    } else {
        Some(false)
    }
}
#[cfg(any(windows, test))]
struct ProcessInfo {
    created: u64,
    image: Option<String>,
}
#[cfg(any(windows, test))]
struct ProcessEntry {
    pid: u32,
    parent: u32,
    name: String,
}
// 只认可同一安装根目录的 bin/bash.exe → usr/bin/bash.exe，普通 bash 子进程照常算忙。
#[cfg(any(windows, test))]
fn git_bash_child(parent: &str, child: &str) -> bool {
    let normalize = |s: &str| s.replace('\\', "/").to_lowercase();
    let parent = normalize(parent);
    let child = normalize(child);
    parent
        .strip_suffix("/bin/bash.exe")
        .is_some_and(|root| !root.is_empty() && child == format!("{root}/usr/bin/bash.exe"))
}
#[cfg(any(windows, test))]
fn children(
    pid: u32,
    created: u64,
    entries: &[ProcessEntry],
    inspect: &mut impl FnMut(u32) -> Option<ProcessInfo>,
) -> Option<Vec<(u32, ProcessInfo)>> {
    let mut children = Vec::new();
    for entry in entries.iter().filter(|e| e.parent == pid) {
        if entry.name.eq_ignore_ascii_case("conhost.exe")
            || entry.name.eq_ignore_ascii_case("OpenConsole.exe")
        {
            continue;
        }
        let info = inspect(entry.pid)?;
        if info.created >= created {
            children.push((entry.pid, info));
        }
    }
    Some(children)
}
#[cfg(any(windows, test))]
fn windows_busy(
    pid: u32,
    entries: &[ProcessEntry],
    mut inspect: impl FnMut(u32) -> Option<ProcessInfo>,
) -> Option<bool> {
    let shell = inspect(pid)?;
    let direct = children(pid, shell.created, entries, &mut inspect)?;
    let result = if let [(child_pid, child)] = direct.as_slice()
        && shell
            .image
            .as_deref()
            .zip(child.image.as_deref())
            .is_some_and(|(p, c)| git_bash_child(p, c))
    {
        let busy = !children(*child_pid, child.created, entries, &mut inspect)?.is_empty();
        if inspect(*child_pid)?.created != child.created {
            return None;
        }
        busy
    } else {
        !direct.is_empty()
    };
    (inspect(pid)?.created == shell.created).then_some(result)
}
#[cfg(windows)]
fn process_info(pid: u32) -> Option<ProcessInfo> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
    };
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let [mut creation, mut exit, mut kernel, mut user]: [FILETIME; 4] =
        unsafe { std::mem::zeroed() };
    let ok = unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
    let mut image = vec![0u16; 32768];
    let mut length = image.len() as u32;
    let image_ok =
        unsafe { QueryFullProcessImageNameW(process, 0, image.as_mut_ptr(), &mut length) };
    unsafe {
        CloseHandle(process);
    }
    (ok != 0).then(|| ProcessInfo {
        created: (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
        image: (image_ok != 0).then(|| String::from_utf16_lossy(&image[..length as usize])),
    })
}
#[cfg(windows)]
pub(super) fn query(pid: u32) -> Option<bool> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_NO_MORE_FILES, GetLastError, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
    };
    let before = process_info(pid)?.created;
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of_val(&entry) as u32;
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) };
    let mut entries = Vec::new();
    while ok != 0 {
        let end = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        entries.push(ProcessEntry {
            pid: entry.th32ProcessID,
            parent: entry.th32ParentProcessID,
            name: String::from_utf16_lossy(&entry.szExeFile[..end]),
        });
        ok = unsafe { Process32NextW(snapshot, &mut entry) };
    }
    let complete = unsafe { GetLastError() } == ERROR_NO_MORE_FILES;
    unsafe {
        CloseHandle(snapshot);
    }
    if !complete || process_info(pid)?.created != before {
        return None;
    }
    windows_busy(pid, &entries, process_info)
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn stat名字含空格与括号及进程不存在() {
        let mut fields = vec!["S", "42"];
        fields.extend(std::iter::repeat_n("0", 17));
        fields.push("123");
        assert_eq!(
            parse_stat(&format!("9 (a ) b) {}", fields.join(" "))),
            Some((42, 123))
        );
        assert_eq!(parse_stat("bad"), None);
        assert_eq!(query(u32::MAX), None);
    }
}

#[cfg(test)]
mod windows_tests {
    use super::*;
    fn entry(pid: u32, parent: u32, name: &str) -> ProcessEntry {
        ProcessEntry {
            pid,
            parent,
            name: name.into(),
        }
    }
    fn info(pid: u32) -> Option<ProcessInfo> {
        Some(ProcessInfo {
            created: pid.into(),
            image: Some(
                match pid {
                    1 => r"C:\Program Files\Git\bin\bash.exe",
                    2 => r"c:\program files\git\usr\bin\BASH.EXE",
                    _ => r"C:\tool.exe",
                }
                .into(),
            ),
        })
    }
    #[test]
    fn 启动器仅穿透同安装目录的唯一真实shell() {
        assert!(git_bash_child(
            r"C:\Git\bin\bash.exe",
            "c:/git/usr/bin/bash.exe"
        ));
        for child in [
            r"D:\Git\usr\bin\bash.exe",
            r"C:\Other\usr\bin\bash.exe",
            r"C:\Git\bin\bash.exe",
            r"C:\Git\usr\bin\other.exe",
        ] {
            assert!(!git_bash_child(r"C:\Git\bin\bash.exe", child));
        }
        let mut entries = vec![entry(2, 1, "bash.exe"), entry(9, 1, "conhost.exe")];
        assert_eq!(windows_busy(1, &entries, info), Some(false));
        entries.push(entry(3, 2, "sleep.exe"));
        assert_eq!(windows_busy(1, &entries, info), Some(true));
        entries[2].parent = 1;
        assert_eq!(windows_busy(1, &entries, info), Some(true));
        assert_eq!(windows_busy(2, &[], info), Some(false));
        assert_eq!(
            windows_busy(2, &[entry(3, 2, "bash.exe")], info),
            Some(true)
        );
    }
    #[test]
    fn 进程消失身份变化与查不到完整路径时保守判断() {
        let entries = [entry(2, 1, "bash.exe")];
        assert_eq!(
            windows_busy(1, &entries, |pid| if pid == 2 { None } else { info(pid) }),
            None
        );
        assert_eq!(
            windows_busy(1, &entries, |pid| {
                let mut p = info(pid)?;
                p.image = None;
                Some(p)
            }),
            Some(true)
        );
        let mut calls = 0;
        assert_eq!(
            windows_busy(1, &entries, |pid| {
                calls += 1;
                let mut p = info(pid)?;
                if calls > 2 {
                    p.created += 10;
                }
                Some(p)
            }),
            None
        );
        assert_eq!(
            windows_busy(1, &entries, |pid| {
                let mut p = info(pid)?;
                if pid == 2 {
                    p.created = 0;
                }
                Some(p)
            }),
            Some(false)
        );
    }
}
