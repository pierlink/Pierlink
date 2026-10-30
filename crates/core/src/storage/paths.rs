//! 纯词法路径检查；符号链接必须由调用方预先解析。

#[derive(Debug, PartialEq, Eq)]
struct NormalPath {
    root: String,
    parts: Vec<String>,
    absolute: bool,
}

fn windows_style(p: &str) -> bool {
    p.as_bytes().get(1) == Some(&b':') || p.starts_with("\\\\") || p.starts_with("//")
}

fn normalize(p: &str, windows: bool) -> NormalPath {
    let p = if windows {
        p.replace('\\', "/").to_lowercase()
    } else {
        p.to_owned()
    };
    let p = if windows {
        if let Some(rest) = p.strip_prefix("//?/unc/") {
            format!("//{rest}")
        } else {
            p.strip_prefix("//?/").unwrap_or(&p).to_owned()
        }
    } else {
        p
    };
    let (root, rest, absolute) = if windows && p.starts_with("//") {
        let mut split = p[2..].split('/').filter(|v| !v.is_empty());
        let host = split.next().unwrap_or("");
        let share = split.next().unwrap_or("");
        (
            format!("//{host}/{share}"),
            split.collect::<Vec<_>>().join("/"),
            !host.is_empty() && !share.is_empty(),
        )
    } else if windows && p.as_bytes().get(1) == Some(&b':') {
        (
            p[..2].to_owned(),
            p[2..].to_owned(),
            p.as_bytes().get(2) == Some(&b'/'),
        )
    } else {
        (
            if p.starts_with('/') { "/" } else { "" }.to_owned(),
            p.clone(),
            p.starts_with('/'),
        )
    };
    let mut parts: Vec<String> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|v| v != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            v => parts.push(v.to_owned()),
        }
    }
    NormalPath {
        root,
        parts,
        absolute,
    }
}

fn contains(parent: &NormalPath, child: &NormalPath) -> bool {
    parent.root == child.root
        && parent.absolute == child.absolute
        && child.parts.starts_with(&parent.parts)
}

/// 比较 a、b 的词法规范化路径；相同或互为父子返回 true，不访问文件系统。
/// Windows 盘符和 UNC 路径忽略大小写并统一分隔符，POSIX 路径区分大小写。
pub fn paths_overlap(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() || windows_style(a) != windows_style(b) {
        return false;
    }
    let a = normalize(a, windows_style(a));
    let b = normalize(b, windows_style(b));
    contains(&a, &b) || contains(&b, &a)
}

pub(super) fn normalized_absolute(p: &str) -> Option<String> {
    if p.contains('\0') {
        return None;
    }
    let n = normalize(p, windows_style(p));
    if !n.absolute {
        return None;
    }
    Some(if n.root == "/" {
        format!("/{}", n.parts.join("/"))
    } else {
        format!("{}/{}", n.root, n.parts.join("/"))
    })
}

/// 检查本地路径 p；home、local_app_data、windows_dir 均由调用方提供，不读取环境。
/// home 及其上级、与 SSH/Pierlink 目录相互包含的路径、系统目录树返回 true；无效路径也拒绝。
/// 调用方必须先把路径规范化为长路径并解析符号链接；Windows 8.3 短名
/// （如 C:\PROGRA~1）不在本函数处理范围。
pub fn dangerous_local_path(p: &str, home: &str, local_app_data: &str, windows_dir: &str) -> bool {
    let n = normalize(p, windows_style(p));
    if !n.absolute || p.contains('\0') || n.parts.is_empty() {
        return true;
    }
    if contains(&n, &normalize(home, windows_style(home))) {
        return true;
    }
    for dir in [format!("{home}/.ssh"), format!("{local_app_data}/Pierlink")] {
        let sensitive = normalize(&dir, windows_style(&dir));
        if contains(&sensitive, &n) || contains(&n, &sensitive) {
            return true;
        }
    }
    let mut denied = vec![windows_dir.to_owned()];
    if n.root.ends_with(':') {
        for dir in [
            "Windows",
            "Program Files",
            "Program Files (x86)",
            "ProgramData",
            "$Recycle.Bin",
            "System Volume Information",
        ] {
            denied.push(format!("{}/{dir}", n.root));
        }
    }
    denied
        .iter()
        .any(|dir| contains(&normalize(dir, windows_style(dir)), &n))
}

/// 检查远程 POSIX 路径 p；home 为远程用户绝对目录，支持 p 中的 ~ 与 ~/。
/// home 及其上级、与 SSH/Pierlink 目录相互包含的路径、系统目录树返回 true；非绝对路径也拒绝。
pub fn dangerous_remote_path(p: &str, home: &str) -> bool {
    let expanded = if p == "~" {
        home.to_owned()
    } else if let Some(rest) = p.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        p.to_owned()
    };
    let n = normalize(&expanded, false);
    if !n.absolute
        || p.contains('\0')
        || n.parts.is_empty()
        || contains(&n, &normalize(home, false))
    {
        return true;
    }
    for dir in [".ssh", ".local/share/pierlink", ".local/state/pierlink"] {
        let sensitive = normalize(&format!("{home}/{dir}"), false);
        if contains(&sensitive, &n) || contains(&n, &sensitive) {
            return true;
        }
    }
    let denied = [
        "/etc", "/usr", "/bin", "/boot", "/var", "/proc", "/sys", "/sbin", "/lib", "/lib64", "/dev",
    ];
    denied
        .iter()
        .any(|dir| contains(&normalize(dir, false), &n))
}
