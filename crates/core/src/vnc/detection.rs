use super::{model::*, parse};
use crate::{
    error::AppError,
    process::SshProgram,
    remote::run::{RunLimits, run_script},
    ssh::args::SshCommand,
};
use serde::Deserialize;

pub(super) const SCRIPT: &str = include_str!("detect.sh");

/// 用 program 和后台用途 cmd 检测本人桌面及安装前提；不连接 VNC，临时探测目录自动回收。
pub fn detect(program: &SshProgram, cmd: &SshCommand) -> Result<VncDetection, AppError> {
    parse_detection(run_script(
        program,
        cmd,
        &script(),
        RunLimits {
            stdout_bytes: 4 * 1024 * 1024,
            ..Default::default()
        },
    )?)
}

pub(super) fn script() -> String {
    format!(
        "{}\n{}",
        super::desktop::COMMON,
        SCRIPT.replace("printf '[{PIERLINK_STATES}]'", super::desktop::STATES)
    )
}

#[derive(Deserialize)]
struct RawDetection {
    arch: String,
    #[serde(default)]
    uid: String,
    #[serde(default)]
    package_busy: bool,
    glibc: String,
    os_release: String,
    package_manager: PackageManager,
    binary_path: String,
    binary_version: String,
    libraries: String,
    tools: Tools,
    #[serde(default)]
    xkb_namespace: Option<XkbNamespace>,
    kill_user_processes: String,
    linger: String,
    sessions: Vec<RawSession>,
    desktops: Vec<RawDesktop>,
    #[serde(default)]
    apt_available: bool,
    #[serde(default)]
    pierlink_states: Vec<super::desktop::Snapshot>,
}
#[derive(Deserialize)]
struct RawSession {
    pid: u32,
    comm: String,
    display_arg: String,
    security_arg: String,
    version: String,
    cgroup: String,
    tcp: String,
    tcp6: String,
    unix: String,
}
#[derive(Deserialize)]
struct RawDesktop {
    id: String,
    content: String,
    executable: bool,
}

pub(super) fn parse_detection(value: serde_json::Value) -> Result<VncDetection, AppError> {
    let raw: RawDetection = serde_json::from_value(value).map_err(|_| {
        AppError::new("vnc.invalid_detection").with_reason(
            "vnc.invalid_desktop_detection_result_format",
            "桌面检测结果格式不正确",
        )
    })?;
    let glibc = (!raw.glibc.is_empty()).then_some(raw.glibc);
    let official_pkg = parse::official(&raw.arch, glibc.as_deref(), &raw.libraries, &raw.tools);
    let tigervnc = (!raw.binary_path.is_empty()).then(|| XvncBinary {
        path: raw.binary_path,
        version: parse::version(&raw.binary_version).unwrap_or((0, 0, 0)),
        flavor: parse::flavor("", &raw.binary_version),
    });
    let logind = match (raw.kill_user_processes.trim(), raw.linger.trim()) {
        (kill @ ("b true" | "b false"), linger @ ("yes" | "no")) => Some(Logind {
            kill_user_processes: kill == "b true",
            linger: linger == "yes",
        }),
        _ => None,
    };
    let mut pierlink_desktops = Vec::new();
    let mut live_pids = Vec::new();
    for snapshot in &raw.pierlink_states {
        let state = snapshot.state()?;
        let stale = snapshot.stale(&state);
        if !stale {
            live_pids.push(state.xvnc_pid);
        }
        pierlink_desktops.push(state.desktop(stale));
    }
    let sessions = raw
        .sessions
        .into_iter()
        .map(|s| {
            let (display, security_types_arg) = parse::cmdline(
                format!("{}\0-SecurityTypes\0{}\0", s.display_arg, s.security_arg).as_bytes(),
            );
            let mut listens: Vec<_> = parse::tcp(&s.tcp, false)
                .into_iter()
                .chain(parse::tcp(&s.tcp6, true))
                .map(|(_, listen)| listen)
                .collect();
            listens.sort_by(|a, b| (&a.addr, a.port).cmp(&(&b.addr, b.port)));
            listens.dedup();
            let mut unix_paths: Vec<_> = parse::unix(&s.unix)
                .into_iter()
                .map(|(_, path)| path)
                .filter(|path| !parse::is_x11_socket(path))
                .collect();
            unix_paths.sort();
            unix_paths.dedup();
            VncSession {
                pid: s.pid,
                flavor: parse::flavor(&s.comm, &s.version),
                comm: s.comm,
                display,
                listens,
                unix_paths,
                security_types_arg,
                started_by: parse::cgroup(&s.cgroup),
                pierlink: live_pids.contains(&s.pid),
            }
        })
        .collect();
    let mut desktops: Vec<_> = raw
        .desktops
        .into_iter()
        .filter_map(|d| parse::desktop(&d.id, &d.content, d.executable))
        .collect();
    desktops
        .sort_by(|a, b| (parse::desktop_order(a), &a.id).cmp(&(parse::desktop_order(b), &b.id)));
    Ok(VncDetection {
        arch: raw.arch,
        glibc,
        distro: parse::distro(&raw.os_release),
        package_manager: raw.package_manager,
        tigervnc,
        sessions,
        light_desktop_available: desktops.is_empty()
            && raw.apt_available
            && raw.package_manager == PackageManager::Apt,
        root: raw.uid.trim() == "0",
        package_busy: raw.package_busy,
        desktops,
        pierlink_desktops,
        tools: raw.tools,
        xkb_namespace: raw.xkb_namespace,
        logind,
        official_pkg,
    })
}

/// distro 是 Ubuntu 或从 Ubuntu 派生（Linux Mint、Pop!_OS 等，ID_LIKE 里有 ubuntu）。
fn ubuntu_family(distro: &Distro) -> bool {
    std::iter::once(distro.id.as_str())
        .chain(distro.id_like.iter().map(String::as_str))
        .any(|id| id == "ubuntu")
}

/// 用 apt 装的完整桌面（和常见的本机 XFCE 一样）：XFCE 桌面和面板、Thunar 文件管理器、终端、通知，
/// 发行版给 XFCE 配的默认图标和主题（Ubuntu 是 elementary-xfce + Greybird，Debian 是 Tango），
/// SVG 图标支持、dbus-launch、中文字体、TigerVNC。不装浏览器；配合 --no-install-recommends 不带登录界面和屏保。
///
/// :param distro: 服务器的发行版
/// :return: 软件包名（只含小写字母、数字和 +.-）
pub fn apt_packages(distro: &Distro) -> Vec<&'static str> {
    let theme: &[&str] = if ubuntu_family(distro) {
        &["elementary-xfce-icon-theme", "greybird-gtk-theme"]
    } else {
        &["tango-icon-theme"]
    };
    ["xfce4", "xfce4-terminal", "xfce4-notifyd"]
        .into_iter()
        .chain(theme.iter().copied())
        .chain([
            "librsvg2-common",
            "xdg-user-dirs",
            "dbus-x11",
            "fonts-noto-cjk",
            "tigervnc-standalone-server",
        ])
        .collect()
}

/// 按 distro 和包管理器生成交给终端运行的完整桌面安装命令（需要管理员权限，密码由用户在终端里输）。
///
/// :param root: 登录的就是 root，命令不带 sudo
/// :return: 安装命令；未知包管理器返回 None
pub fn install_command(distro: &Distro, pm: PackageManager, root: bool) -> Option<String> {
    let sudo = if root { "" } else { "sudo " };
    match pm {
        PackageManager::Apt => Some(format!(
            "{sudo}apt-get update && {sudo}env DEBIAN_FRONTEND=noninteractive apt-get install -y -o Dpkg::Progress-Fancy=1 --no-install-recommends {}",
            apt_packages(distro).join(" ")
        )),
        PackageManager::Dnf => {
            let epel = std::iter::once(distro.id.as_str())
                .chain(distro.id_like.iter().map(String::as_str))
                .any(|id| matches!(id, "rhel" | "rocky" | "almalinux" | "alma"));
            // 企业发行版的 Xfce 在 EPEL 里，用软件包组安装
            Some(if epel {
                format!(
                    "{sudo}dnf install -y epel-release && {sudo}dnf install -y tigervnc-server dbus-x11 google-noto-sans-cjk-ttc-fonts && {sudo}dnf groupinstall -y \"Xfce\""
                )
            } else {
                format!("{sudo}dnf install -y tigervnc-server dbus-x11 @xfce-desktop-environment")
            })
        }
        PackageManager::None => None,
    }
}

/// Google Chrome 的官方安装包（只有 x86_64 版）：.deb 给 apt 系统，.rpm 给 dnf 系统
const CHROME_DEB: &str =
    "https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb";
const CHROME_RPM: &str =
    "https://dl.google.com/linux/direct/google-chrome-stable_current_x86_64.rpm";

/// 生成下载并安装 Google Chrome 的命令（交给终端运行或给用户复制，需要管理员权限）。
/// apt：更新软件源，下载官方 .deb（有 wget 用 wget，没有用 curl，都没有先装 wget），装好后删掉下载的文件；
/// dnf：直接装官方 .rpm。
///
/// :param arch: 服务器的 CPU 架构（uname -m）
/// :param pm: 包管理器
/// :param root: 登录的就是 root，命令不带 sudo
/// :param tools: 服务器上有哪些工具（看有没有 wget、curl）
/// :return: 安装命令；不是 x86_64（Chrome 没有这种架构的 Linux 版）或不认识包管理器时为 None
pub fn chrome_command(arch: &str, pm: PackageManager, root: bool, tools: &Tools) -> Option<String> {
    if arch.trim() != "x86_64" {
        return None;
    }
    let sudo = if root { "" } else { "sudo " };
    match pm {
        PackageManager::Apt => {
            let file = "/tmp/google-chrome.deb";
            let fetch = if tools.wget {
                format!("wget -O {file} {CHROME_DEB}")
            } else if tools.curl {
                format!("curl -fL -o {file} {CHROME_DEB}")
            } else {
                format!("{sudo}apt-get install -y wget && wget -O {file} {CHROME_DEB}")
            };
            Some(format!(
                "{sudo}apt-get update && {fetch} && {sudo}apt-get install -y {file} && rm -f {file}"
            ))
        }
        PackageManager::Dnf => Some(format!("{sudo}dnf install -y {CHROME_RPM}")),
        PackageManager::None => None,
    }
}
