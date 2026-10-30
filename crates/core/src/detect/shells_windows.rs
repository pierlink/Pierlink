//! Windows 路径、App Paths 注册表与 WSL 检测。
use super::git::git_bash;
use super::*;
use std::os::windows::ffi::OsStringExt;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};
fn app_path(root: HKEY, executable: &str) -> Option<PathBuf> {
    registry_path(
        root,
        &format!("Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{executable}"),
        None,
    )
}
fn registry_path(root: HKEY, key: &str, value: Option<&str>) -> Option<PathBuf> {
    let key: Vec<_> = key.encode_utf16().chain(Some(0)).collect();
    let value: Option<Vec<_>> = value.map(|v| v.encode_utf16().chain(Some(0)).collect());
    let mut data = vec![0u16; 32768];
    let mut size = (data.len() * 2) as u32;
    let result = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result != 0 || size < 2 {
        return None;
    }
    data.truncate(size as usize / 2);
    while data.last() == Some(&0) {
        data.pop();
    }
    app_path_value(data)
}
fn app_path_value(mut data: Vec<u16>) -> Option<PathBuf> {
    if data.first() == Some(&34) && data.last() == Some(&34) && data.len() >= 2 {
        data = data[1..data.len() - 1].to_vec();
    }
    let path = PathBuf::from(std::ffi::OsString::from_wide(&data));
    path.is_absolute().then_some(path)
}
pub(super) fn detect() -> Vec<LocalShell> {
    let mut shells = Vec::new();
    let entries = [
        ("pwsh.exe", "PowerShell 7", ShellKind::PowerShell7),
        (
            "powershell.exe",
            "Windows PowerShell",
            ShellKind::WindowsPowerShell,
        ),
        ("bash.exe", "Git Bash", ShellKind::GitBash),
        ("cmd.exe", "命令提示符", ShellKind::Cmd),
    ];
    for (exe, name, kind) in entries {
        let mut paths = Vec::new();
        for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(variable) {
                for relative in [
                    "PowerShell/7/pwsh.exe",
                    "Git/bin/bash.exe",
                    "Programs/Git/bin/bash.exe",
                ] {
                    let path = PathBuf::from(&base).join(relative);
                    if path.file_name().is_some_and(|n| n == exe) {
                        paths.push(path);
                    }
                }
            }
        }
        if exe != "bash.exe"
            && let Some(root) = std::env::var_os("SystemRoot")
        {
            paths.push(PathBuf::from(&root).join("System32").join(exe));
            if exe == "powershell.exe" {
                paths.push(
                    PathBuf::from(root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
                );
            }
        }
        for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
            if exe == "bash.exe" {
                if let Some(git) = app_path(root, "git.exe")
                    && let Some(base) = git.parent().and_then(|p| p.parent())
                {
                    paths.push(base.join("bin/bash.exe"));
                }
            } else if let Some(path) = app_path(root, exe) {
                paths.push(path);
            }
        }
        let path_entries = std::env::var_os("PATH").unwrap_or_default();
        if exe == "bash.exe" {
            let mut installs = Vec::new();
            for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
                for key in [
                    r"SOFTWARE\GitForWindows",
                    r"SOFTWARE\WOW6432Node\GitForWindows",
                ] {
                    if let Some(path) = registry_path(root, key, Some("InstallPath")) {
                        installs.push(path);
                    }
                }
            }
            let mut scoops = Vec::new();
            if let Some(root) = std::env::var_os("SCOOP") {
                scoops.push(PathBuf::from(root));
            }
            if let Some(root) = std::env::var_os("USERPROFILE") {
                scoops.push(PathBuf::from(root).join("scoop"));
            }
            paths.extend(super::git::candidates(
                &std::env::split_paths(&path_entries).collect::<Vec<_>>(),
                &installs,
                &scoops,
            ));
        }
        paths = ordered_candidates(paths, &path_entries, exe);
        for path in paths {
            let args = if exe == "bash.exe" {
                if !git_bash(
                    &path,
                    std::env::var_os("SystemRoot")
                        .as_deref()
                        .map(std::path::Path::new),
                ) {
                    continue;
                }
                vec!["--login".into(), "-i".into()]
            } else {
                vec![]
            };
            add(&mut shells, path, name, kind.clone(), args);
        }
    }
    if let Some(program) = std::env::var_os("SHELL") {
        add(
            &mut shells,
            program.into(),
            "用户 Shell",
            ShellKind::Other,
            vec![],
        );
    }
    if let Some(root) = std::env::var_os("SystemRoot") {
        let program = PathBuf::from(root).join(r"System32\wsl.exe");
        for distro in wsl_list() {
            add(
                &mut shells,
                program.clone(),
                &format!("WSL: {distro}"),
                ShellKind::Wsl {
                    distro: distro.clone(),
                },
                vec!["-d".into(), distro],
            );
        }
    }
    shells
}
fn ordered_candidates(
    mut known_and_app: Vec<PathBuf>,
    path: &std::ffi::OsStr,
    exe: &str,
) -> Vec<PathBuf> {
    if exe != "bash.exe" {
        known_and_app.extend(
            std::env::split_paths(path)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(exe)),
        );
    }
    known_and_app
        .into_iter()
        .filter(|p| p.is_absolute())
        .collect()
}
fn distro_allowed(name: &str, state: u32) -> bool {
    state == 1
        && !name.trim().is_empty()
        && !["docker-desktop", "docker-desktop-data"]
            .iter()
            .any(|s| name.eq_ignore_ascii_case(s))
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
fn wsl_list() -> Vec<String> {
    let path: Vec<_> = "Software\\Microsoft\\Windows\\CurrentVersion\\Lxss\0"
        .encode_utf16()
        .collect();
    let mut handle = std::ptr::null_mut();
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut handle) } != 0 {
        return vec![];
    }
    let key = Key(handle);
    let mut result = Vec::new();
    let state_name: Vec<_> = "State\0".encode_utf16().collect();
    let distro_name: Vec<_> = "DistributionName\0".encode_utf16().collect();
    for index in 0.. {
        let mut name = vec![0u16; 256];
        let mut length = name.len() as u32;
        let status = unsafe {
            RegEnumKeyExW(
                key.0,
                index,
                name.as_mut_ptr(),
                &mut length,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            break;
        }
        name.truncate(length as usize + 1);
        let mut state = 0u32;
        let mut bytes = 4u32;
        if unsafe {
            RegGetValueW(
                key.0,
                name.as_ptr(),
                state_name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut state as *mut u32).cast(),
                &mut bytes,
            )
        } != 0
        {
            continue;
        }
        let mut data = vec![0u16; 32768];
        bytes = (data.len() * 2) as u32;
        if unsafe {
            RegGetValueW(
                key.0,
                name.as_ptr(),
                distro_name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                data.as_mut_ptr().cast(),
                &mut bytes,
            )
        } != 0
        {
            continue;
        }
        data.truncate(bytes as usize / 2);
        while data.last() == Some(&0) {
            data.pop();
        }
        let name = String::from_utf16_lossy(&data);
        if distro_allowed(&name, state) && !result.contains(&name) {
            result.push(name);
        }
    }
    result
}
#[cfg(test)]
mod tests {
    use super::git::git_bash;
    use super::*;
    #[test]
    fn 路径优先级相对项及注册表引号() {
        let known = PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe");
        let app = PathBuf::from(r"C:\Apps\pwsh.exe");
        let paths = ordered_candidates(
            vec![known.clone(), app.clone()],
            std::ffi::OsStr::new(r".;relative;C:relative;\rooted;C:\Tools"),
            "pwsh.exe",
        );
        assert_eq!(paths, [known, app, PathBuf::from(r"C:\Tools\pwsh.exe")]);
        assert!(
            ordered_candidates(
                vec![],
                std::ffi::OsStr::new(r"C:\Windows\System32"),
                "bash.exe"
            )
            .is_empty()
        );
        assert_eq!(
            app_path_value(r#""C:\Program Files\Git\cmd\git.exe""#.encode_utf16().collect()),
            Some(PathBuf::from(r"C:\Program Files\Git\cmd\git.exe"))
        );
        assert!(app_path_value("relative.exe".encode_utf16().collect()).is_none());
    }
    #[test]
    fn 注册表发行版过滤() {
        assert!(distro_allowed("Ubuntu 中文", 1));
        for name in ["Ubuntu", "docker-desktop", "docker-desktop-data", ""] {
            assert!(!distro_allowed(name, 0));
        }
        assert!(!distro_allowed("docker-desktop", 1));
        assert!(!distro_allowed("docker-desktop-data", 1));
    }
    #[test]
    fn git安装位置和系统bash() {
        let dir = crate::test_support::tempdir();
        let bin = dir.path().join("Git/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let bash = bin.join("bash.exe");
        std::fs::write(&bash, "").unwrap();
        assert!(!git_bash(&bash, None));
        std::fs::write(bin.join("git.exe"), "").unwrap();
        assert!(git_bash(&bash, None));
        assert!(!git_bash(&bash, Some(dir.path())));
        assert!(!git_bash(std::path::Path::new("Git/bin/bash.exe"), None));
    }
}
