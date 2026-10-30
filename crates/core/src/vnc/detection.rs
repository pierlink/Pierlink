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
        desktops,
        pierlink_desktops,
        tools: raw.tools,
        xkb_namespace: raw.xkb_namespace,
        logind,
        official_pkg,
    })
}

/// 按 distro 和包管理器生成可交给终端的安装命令；light 选择 Openbox，未知包管理器返回 None。
pub fn install_command(distro: &Distro, pm: PackageManager, light: bool) -> Option<String> {
    match pm {
        PackageManager::Apt => Some(format!(
            "sudo apt-get install -y tigervnc-standalone-server {} dbus-x11",
            if light { "openbox xterm" } else { "xfce4" }
        )),
        PackageManager::Dnf => {
            let epel = std::iter::once(distro.id.as_str())
                .chain(distro.id_like.iter().map(String::as_str))
                .any(|id| matches!(id, "rhel" | "rocky" | "almalinux" | "alma"));
            Some(format!(
                "{}sudo dnf install -y tigervnc-server {}",
                if epel {
                    "sudo dnf install -y epel-release && "
                } else {
                    ""
                },
                if light {
                    "openbox xterm"
                } else {
                    "@xfce-desktop-environment"
                }
            ))
        }
        PackageManager::None => None,
    }
}
