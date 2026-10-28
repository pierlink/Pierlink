use super::model::*;
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

pub(super) fn tcp(text: &str, ipv6: bool) -> Vec<(String, Listen)> {
    text.lines().filter_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10 || fields[3] != "0A" { return None; }
        let (hex, port) = fields[1].split_once(':')?;
        let addr = if ipv6 {
            if hex.len() != 32 || !hex.is_ascii() { return None; }
            let mut bytes = [0; 16];
            for (chunk, out) in hex.as_bytes().as_chunks::<8>().0.iter().zip(bytes.as_chunks_mut::<4>().0) {
                out.copy_from_slice(&u32::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?.to_le_bytes());
            }
            IpAddr::V6(Ipv6Addr::from(bytes))
        } else {
            if hex.len() != 8 { return None; }
            IpAddr::V4(Ipv4Addr::from(u32::from_str_radix(hex, 16).ok()?.to_le_bytes()))
        };
        let scope = if addr.is_unspecified() { ListenScope::All }
            else if addr.is_loopback() || matches!(addr, IpAddr::V6(a) if a.to_ipv4_mapped().is_some_and(|a| a.is_loopback())) { ListenScope::Loopback }
            else { ListenScope::Specific };
        Some((fields[9].into(), Listen { addr: addr.to_string(), port: u16::from_str_radix(port, 16).ok()?, scope }))
    }).collect()
}

pub(super) fn unix(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            // 前七列固定，路径允许空格。
            let mut rest = line;
            let mut inode = "";
            for _ in 0..7 {
                rest = rest.trim_start();
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                inode = &rest[..end];
                rest = &rest[end..];
            }
            let path = rest.trim_start();
            (!path.is_empty() && inode.parse::<u64>().is_ok()).then(|| (inode.into(), path.into()))
        })
        .collect()
}

/// Xvnc 本身是 X 服务器，还会监听 X11 的套接字（`/tmp/.X11-unix/XN` 和同名抽象套接字），那不是 VNC 入口
pub(super) fn is_x11_socket(path: &str) -> bool {
    path.starts_with('@') || path.starts_with("/tmp/.X11-unix/")
}

pub(super) fn cmdline(bytes: &[u8]) -> (Option<u16>, Option<String>) {
    let args: Vec<_> = bytes
        .split(|b| *b == 0)
        .filter_map(|s| std::str::from_utf8(s).ok())
        .collect();
    let display = args
        .iter()
        .find_map(|s| s.strip_prefix(':').and_then(|n| n.parse().ok()));
    let security = args.iter().enumerate().find_map(|(i, s)| {
        s.strip_prefix("-SecurityTypes=")
            .map(str::to_owned)
            .or_else(|| {
                (*s == "-SecurityTypes")
                    .then(|| {
                        args.get(i + 1)
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                    })
                    .flatten()
            })
    });
    (display, security)
}

pub(super) fn cgroup(text: &str) -> StartedBy {
    for line in text.lines() {
        let Some(path) = line.splitn(3, ':').nth(2) else {
            continue;
        };
        let parts: Vec<_> = path.split('/').collect();
        if parts.contains(&"system.slice") {
            if let Some(unit) = parts
                .iter()
                .rev()
                .find(|s| s.ends_with(".service") || s.ends_with(".scope"))
            {
                return StartedBy::SystemUnit((*unit).into());
            }
        } else if parts.contains(&"user.slice") {
            if parts
                .iter()
                .any(|s| s.starts_with("session-") && s.ends_with(".scope"))
            {
                return StartedBy::UserSession;
            }
            if let Some(unit) = parts.iter().rev().find(|s| {
                (s.ends_with(".service") || s.ends_with(".scope")) && !s.starts_with("user@")
            }) {
                return StartedBy::UserUnit((*unit).into());
            }
        }
    }
    StartedBy::Unknown
}

pub(super) fn distro(text: &str) -> Distro {
    let fields: BTreeMap<_, _> = text.lines().filter_map(|s| s.split_once('=')).collect();
    let get = |key| {
        fields
            .get(key)
            .copied()
            .unwrap_or("")
            .trim()
            .trim_matches(['\'', '"'])
            .to_string()
    };
    Distro {
        id: get("ID"),
        version_id: get("VERSION_ID"),
        id_like: get("ID_LIKE")
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
    }
}

pub(super) fn flavor(comm: &str, version: &str) -> Flavor {
    let text = version.to_ascii_lowercase();
    if text.contains("turbovnc") {
        Flavor::TurboVnc
    } else if text.contains("tigervnc") || matches!(comm, "Xtigervnc" | "x0vncserver") {
        Flavor::TigerVnc
    } else if comm == "x11vnc" {
        Flavor::X11vnc
    } else if text.contains("realvnc") || comm.starts_with("vncserver-") {
        Flavor::RealVnc
    } else {
        Flavor::Other
    }
}

pub(super) fn version(text: &str) -> Option<(u16, u16, u16)> {
    text.split(|c: char| !c.is_ascii_digit() && c != '.')
        .find_map(|word| {
            let parts: Vec<_> = word.split('.').collect();
            if !(2..=3).contains(&parts.len()) {
                return None;
            }
            Some((
                parts[0].parse().ok()?,
                parts[1].parse().ok()?,
                parts.get(2).map_or(Some(0), |s| s.parse().ok())?,
            ))
        })
}

pub(super) fn ssh_version(text: &str) -> Option<(u16, u16)> {
    let rest = text
        .trim()
        .strip_prefix("OpenSSH_for_Windows_")
        .or_else(|| text.trim().strip_prefix("OpenSSH_"))?;
    let number = rest
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .next()?;
    let (major, minor) = number.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

pub(super) fn desktop(id: &str, text: &str, executable: bool) -> Option<DesktopEntry> {
    let mut active = false;
    let mut fields = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            active = line == "[Desktop Entry]";
        } else if active && let Some((k, v)) = line.split_once('=') {
            fields.insert(k.trim(), v.trim());
        }
    }
    let get = |k| fields.get(k).copied().unwrap_or("");
    if !executable
        || get("Exec").is_empty()
        || get("Hidden") == "true"
        || get("NoDisplay") == "true"
    {
        return None;
    }
    let mut entry = DesktopEntry {
        id: id.into(),
        name: get("Name").into(),
        exec: get("Exec").into(),
        desktop_names: get("DesktopNames")
            .split(';')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        heavy: false,
    };
    entry.heavy = desktop_order(&entry) == 100;
    Some(entry)
}

pub(super) fn desktop_order(entry: &DesktopEntry) -> u8 {
    // 发行版会把 GNOME 会话命名为 ubuntu；优先 ID，再看桌面声明和启动程序。
    std::iter::once(entry.id.as_str())
        .chain(entry.desktop_names.iter().map(String::as_str))
        .chain([entry.exec.as_str(), entry.name.as_str()])
        .map(desktop_weight)
        .find(|weight| *weight != 55)
        .unwrap_or(55)
}

pub(super) fn desktop_weight(id: &str) -> u8 {
    let id = id.to_ascii_lowercase();
    [
        "openbox", "icewm", "lxqt", "lxde", "xfce", "mate", "cinnamon", "budgie", "plasma", "gnome",
    ]
    .iter()
    .position(|name| id.contains(name))
    .map_or(55, |i| (i as u8 + 1) * 10)
}

pub(super) fn official(
    arch: &str,
    glibc: Option<&str>,
    libraries: &str,
    tools: &Tools,
) -> OfficialPkgCheck {
    let mut missing = Vec::new();
    if arch != "x86_64" {
        missing.push("x86_64".into());
    }
    if !glibc.and_then(version).is_some_and(|v| v >= (2, 28, 0)) {
        missing.push("glibc>=2.28".into());
    }
    for library in [
        "libxcvt.so.0",
        "libgbm.so.1",
        "libxshmfence.so.1",
        "libX11.so.6",
        "libpam.so.0",
        "libsystemd.so.0",
        "libstdc++.so.6",
    ] {
        if !libraries
            .lines()
            .any(|s| s.split_whitespace().next() == Some(library))
        {
            missing.push(library.into());
        }
    }
    if !tools.xkbcomp {
        missing.push("/usr/bin/xkbcomp".into());
    }
    if !tools.xkb_rules {
        missing.push("/usr/share/X11/xkb/rules/evdev".into());
    }
    OfficialPkgCheck {
        ok: missing.is_empty(),
        missing,
    }
}
