use super::{model::*, parse};
use crate::{
    error::AppError,
    process::SshProgram,
    remote::run::{RunLimits, run_script},
    security::redact,
    ssh::{args::SshCommand, quote::sh_quote},
};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

pub(super) const COMMON: &str = concat!(
    include_str!("common.sh"),
    "\n",
    include_str!("environment.sh"),
    "\n",
    include_str!("xkb.sh")
);
pub(super) const STATES: &str = include_str!("states.sh");

pub(super) fn error(code: &str) -> AppError {
    AppError::new(format!("vnc.{code}"))
}

pub(super) fn run(
    program: &SshProgram,
    cmd: &SshCommand,
    body: &str,
    seconds: u64,
) -> Result<Value, AppError> {
    let value = run_script(
        program,
        cmd,
        &format!("{COMMON}\n{body}"),
        RunLimits {
            timeout: Duration::from_secs(seconds),
            stdout_bytes: 4 * 1024 * 1024,
            stderr_bytes: 4096,
        },
    )?;
    decode_error(value)
}

pub(super) fn decode_error(value: Value) -> Result<Value, AppError> {
    if let Some(code) = value["error"].as_str() {
        let allowed = [
            "display_exhausted",
            "socket_path",
            "start_timeout",
            "desktop_missing",
            "tcp_listen",
            "xvnc_too_old",
            "not_tigervnc",
            "stale",
            "download_failed",
            "sha_mismatch",
            "missing_libs",
            "unsafe_path",
            "permission_denied",
            "missing_tool",
            "invalid_package",
            "locked",
            "upload_conflict",
            "checksum_mismatch",
            "missing_base64",
            "missing_gzip",
            "download_limit",
            "light_unavailable",
            "xkb_unavailable",
        ];
        let suffix = code
            .strip_prefix("vnc.")
            .filter(|c| allowed.contains(c))
            .ok_or_else(|| error("invalid_report"))?;
        let mut e = error(suffix);
        let cookie = regex::Regex::new(r"(?i)\b[0-9a-f]{32}\b");
        for key in [
            "reason",
            "log",
            "libs",
            "source",
            "package",
            "limit_bytes",
            "total_bytes",
            "install_command",
        ] {
            if let Some(text) = value[key].as_str() {
                let safe = redact(text, &[]);
                // cookie 即使出现在第三方输出里也不返回。
                let safe = cookie
                    .as_ref()
                    .map(|r| r.replace_all(&safe, "***").into_owned())
                    .unwrap_or_else(|_| "***".into());
                e = e.with(key, safe.chars().take(8192).collect::<String>());
            }
        }
        return Err(e);
    }
    Ok(value)
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct State {
    pub display: u16,
    pub socket: String,
    pub xauth: String,
    pub log: String,
    pub xvnc_pid: u32,
    pub pgid: u32,
    pub starttime: String,
    pub boot_id: String,
    pub desktop_id: String,
    pub desktop_name: String,
    pub created_at: u64,
    pub geometry: String,
    #[serde(default)]
    pub linger_failed: bool,
}
#[derive(Debug, Deserialize)]
pub(super) struct Snapshot {
    pub file: u16,
    pub content: String,
    pub boot_id: String,
    pub identity: String,
    pub comm: String,
}
impl Snapshot {
    pub(super) fn state(&self) -> Result<State, AppError> {
        let s: State = serde_json::from_str(&self.content)
            .map_err(|_| error("stale").with("display", self.file))?;
        if s.display != self.file
            || s.xvnc_pid <= 1
            || s.pgid <= 1
            || s.starttime.is_empty()
            || !s.starttime.bytes().all(|c| c.is_ascii_digit())
            || s.boot_id.is_empty()
        {
            return Err(error("stale").with("display", self.file));
        }
        Ok(s)
    }
    pub(super) fn stale(&self, s: &State) -> bool {
        self.boot_id != s.boot_id
            || self.identity != format!("{} {}", s.starttime, s.pgid)
            || !matches!(self.comm.as_str(), "Xvnc" | "Xtigervnc")
    }
}
impl State {
    pub(super) fn desktop(&self, stale: bool) -> PierlinkDesktop {
        PierlinkDesktop {
            display: self.display,
            socket: self.socket.clone(),
            desktop_id: self.desktop_id.clone(),
            desktop_name: self.desktop_name.clone(),
            created_at: self.created_at,
            geometry: self.geometry.clone(),
            stale,
            linger_failed: self.linger_failed,
        }
    }
}

pub(super) fn check_binary(binary: &XvncBinary) -> Result<(), AppError> {
    if binary.flavor != Flavor::TigerVnc {
        return Err(error("not_tigervnc"));
    }
    if binary.version < (1, 9, 0) {
        return Err(error("xvnc_too_old"));
    }
    if !binary.path.starts_with('/') || binary.path.chars().any(char::is_control) {
        return Err(error("not_tigervnc"));
    }
    Ok(())
}

pub(super) fn verify_binary(
    program: &SshProgram,
    cmd: &SshCommand,
    path: &str,
) -> Result<XvncBinary, AppError> {
    let result = run(
        program,
        cmd,
        &format!(
            r#"
binary={}
binary_environment "$binary"
[ -x "$binary" ] || fail not_tigervnc
output=$(bounded 5 "$binary" -version 2>&1) || fail not_tigervnc
printf '{{"version":'; json "$output"; printf '}}'
"#,
            sh_quote(path)?
        ),
        10,
    )?;
    let text = result["version"]
        .as_str()
        .ok_or_else(|| error("invalid_report"))?;
    let binary = XvncBinary {
        path: path.into(),
        version: parse::version(text).unwrap_or((0, 0, 0)),
        flavor: parse::flavor("", text),
    };
    check_binary(&binary)?;
    Ok(binary)
}

pub(super) fn create_script(req: &CreateDesktop) -> Result<String, AppError> {
    if req.desktop_id.is_empty()
        || req.desktop_id.len() > 128
        || !req
            .desktop_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-+.".contains(&c))
        || matches!(req.desktop_id.as_str(), "." | "..")
    {
        return Err(error("desktop_missing"));
    }
    check_binary(&req.xvnc)?;
    let geometry = format!(
        "{}x{}",
        req.width.clamp(640, 2560),
        req.height.clamp(480, 1600)
    );
    let supervisor = format!("{COMMON}\n{}", include_str!("supervise.sh"));
    let entry = if req.desktop_id == LIGHT_DESKTOP_ID {
        include_str!("light_entry.sh")
    } else {
        include_str!("desktop_entry.sh")
    };
    Ok(format!(
        "desktop_id={}\nbinary={}\ngeometry={}\nforce_xkb_namespace={}\nxsessions=${{PIERLINK_VNC_XSESSIONS_DIR:-/usr/share/xsessions}}\n{}\nsupervisor={}\n{}",
        sh_quote(&req.desktop_id)?,
        sh_quote(&req.xvnc.path)?,
        sh_quote(&geometry)?,
        if cfg!(test) {
            "${PIERLINK_TEST_XKB_NAMESPACE:-0}"
        } else {
            "0"
        },
        entry,
        sh_quote(&supervisor)?,
        include_str!("launch.sh")
    ))
}

/// 用 program/cmd 启动 req 指定的桌面；普通 id 重读服务器条目，pierlink-light 自动获取轻量桌面。
/// 返回已写入的专用桌面状态；需要下载进度时可先调用 acquire_light_desktop。
pub fn create_desktop(
    program: &SshProgram,
    cmd: &SshCommand,
    req: CreateDesktop,
) -> Result<PierlinkDesktop, AppError> {
    let script = create_script(&req)?;
    verify_binary(program, cmd, &req.xvnc.path)?;
    if req.desktop_id == LIGHT_DESKTOP_ID {
        super::acquire::acquire_light_desktop(
            program,
            cmd,
            &|_| {},
            &std::sync::atomic::AtomicBool::new(false),
        )?;
    }
    let value = run(program, cmd, &script, 240)?;
    let state: State = serde_json::from_value(value).map_err(|_| error("invalid_report"))?;
    Ok(state.desktop(false))
}

fn snapshots(program: &SshProgram, cmd: &SshCommand) -> Result<Vec<Snapshot>, AppError> {
    serde_json::from_value(run(program, cmd, STATES, 30)?).map_err(|_| error("invalid_report"))
}

fn change_state(
    program: &SshProgram,
    cmd: &SshCommand,
    snapshot: &Snapshot,
    stop: bool,
) -> Result<bool, AppError> {
    let s = snapshot.state()?;
    let script = format!(
        r#"
display={display}
expected={expected}
pid={pid}
pgid={pgid}
starttime={starttime}
boot={boot}
socket={socket}
xauth_file={xauth}
log={log}
stop={stop}
{body}
"#,
        display = s.display,
        expected = sh_quote(&snapshot.content)?,
        pid = s.xvnc_pid,
        pgid = s.pgid,
        starttime = sh_quote(&s.starttime)?,
        boot = sh_quote(&s.boot_id)?,
        socket = sh_quote(&s.socket)?,
        xauth = sh_quote(&s.xauth)?,
        log = sh_quote(&s.log)?,
        stop = if stop { "true" } else { "false" },
        body = include_str!("stop.sh")
    );
    let v = run(program, cmd, &script, 20)?;
    v["removed"]
        .as_bool()
        .ok_or_else(|| error("invalid_report"))
}

/// 用 program/cmd 关闭 display；核对启动标识后才发送信号，重启遗留状态只删除记录。
pub fn stop_desktop(program: &SshProgram, cmd: &SshCommand, display: u16) -> Result<(), AppError> {
    let snapshot = snapshots(program, cmd)?
        .into_iter()
        .find(|s| s.file == display)
        .ok_or_else(|| error("stale").with("display", display))?;
    change_state(program, cmd, &snapshot, true)?;
    Ok(())
}

/// 用 program/cmd 删除过期状态及无监听者的残留套接字，返回清理数量；不发送任何信号。
pub fn cleanup_stale(program: &SshProgram, cmd: &SshCommand) -> Result<u32, AppError> {
    let mut count = 0;
    for snapshot in snapshots(program, cmd)? {
        let state = snapshot.state()?;
        if snapshot.stale(&state) && change_state(program, cmd, &snapshot, false)? {
            count += 1;
        }
    }
    Ok(count)
}
