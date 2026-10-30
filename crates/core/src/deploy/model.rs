use super::{AppError, err};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Arch {
    X86_64,
    Aarch64,
}
impl Arch {
    pub(super) fn directory(self) -> &'static str {
        match self {
            Self::X86_64 => "amd64",
            Self::Aarch64 => "arm64",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemdState {
    None,
    UserManagerNotRunning,
    BusUnreachable,
    Available,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub owner: String,
    pub age_seconds: u64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InstanceInfo {
    pub id: String,
    pub version: String,
    pub binary: String,
    pub state_dir: String,
    pub socket_path: String,
    pub sync_port: u16,
    pub device_id: String,
    pub instance_key: String,
    pub autostart_consent: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceReport {
    pub info: InstanceInfo,
    pub running: bool,
    pub devices: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerReport {
    pub user: String,
    pub home: String,
    pub arch: String,
    pub os_name: Option<String>,
    pub arch_supported: Option<Arch>,
    pub instance_key: String,
    pub instance: Option<InstanceReport>,
    pub pending_instance_id: Option<String>,
    pub own_syncthing_running: u32,
    pub systemd: SystemdState,
    pub linger: Option<bool>,
    pub kill_user_processes: Option<bool>,
    pub home_noexec: bool,
    pub home_fs_type: String,
    pub inotify_max_user_watches: Option<u64>,
    pub free_bytes: Option<u64>,
    pub runtime_dir: Option<String>,
    pub lock: Option<LockInfo>,
}
#[derive(Debug, Clone, Serialize)]
pub struct DeployPlan {
    pub home: String,
    pub instance_key: String,
    pub instance_id: String,
    pub version: String,
    pub arch: Arch,
    pub upload_required: bool,
    pub new_instance: bool,
    pub systemd: SystemdState,
    pub socket_path: String,
    pub other_devices: Vec<String>,
    pub local_device_id: Option<String>,
    pub upgrade: bool,
    pub autostart_consent: bool,
    pub upgrade_confirmed: bool,
}
/// 凭据由调用方生成和存入凭据管理器；故意不实现 Debug 或序列化。
pub struct InstanceSecrets {
    pub api_key: String,
    pub gui_user: String,
    pub gui_password: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StartOutcome {
    AlreadyRunning {
        instance: InstanceInfo,
    },
    Systemd {
        instance: InstanceInfo,
        linger: Option<bool>,
        lock_taken_over: bool,
    },
    Nohup {
        instance: InstanceInfo,
        restart_after_reboot: bool,
        killed_on_logout: bool,
        lock_taken_over: bool,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedPath {
    pub path: String,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UninstallReport {
    pub devices_unavailable: bool,
    pub will_delete: Vec<String>,
    pub skipped: Vec<SkippedPath>,
    pub devices: Vec<String>,
    pub dry_run: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Manifest {
    #[serde(skip)]
    pub source_digest: String,
    pub startup_wait_seconds: u64,
    pub info: InstanceInfo,
    pub paths: Vec<String>,
    pub devices: Vec<String>,
}

pub(super) fn text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}
pub(super) fn path(s: &str) -> bool {
    text(s, 4096)
        && s.starts_with('/')
        && !s.split('/').skip(1).any(|c| matches!(c, "" | "." | ".."))
}
pub(super) fn id(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok_and(|id| id.to_string() == s)
}
pub(super) fn key(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub(super) fn version(s: &str) -> Result<[u64; 3], AppError> {
    if s.len() > 64 {
        return Err(err("invalid_version"));
    }
    let parts: Vec<_> = s.strip_prefix('v').unwrap_or(s).split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()))
    {
        return Err(err("invalid_version"));
    }
    let mut v = [0; 3];
    for (i, p) in parts.iter().enumerate() {
        v[i] = p.parse().map_err(|_| err("invalid_version"))?;
    }
    Ok(v)
}
impl InstanceInfo {
    pub(super) fn validate(&self, home: &str) -> Result<(), AppError> {
        version(&self.version)?;
        if !id(&self.id)
            || !key(&self.instance_key)
            || !path(&self.state_dir)
            || self.state_dir != format!("{home}/.local/state/pierlink/{}", self.id)
            || !path(&self.binary)
            || !self
                .binary
                .starts_with(&format!("{home}/.local/share/pierlink/bin/"))
            || !path(&self.socket_path)
            || self.socket_path.len() >= 100
            || !(22000..23000).contains(&self.sync_port)
            || !text(&self.device_id, 128)
        {
            return Err(err("invalid_report"));
        }
        Ok(())
    }
}
impl ServerReport {
    pub(super) fn validate(&self) -> Result<(), AppError> {
        if self.pending_instance_id.as_ref().is_some_and(|s| !id(s))
            || !text(&self.user, 256)
            || !path(&self.home)
            || !text(&self.arch, 80)
            || !key(&self.instance_key)
            || !text(&self.home_fs_type, 80)
            || self.runtime_dir.as_ref().is_some_and(|s| !path(s))
            || self
                .os_name
                .as_ref()
                .is_some_and(|s| !text(s, 320) || s.chars().count() > 80)
            || self.lock.as_ref().is_some_and(|l| !text(&l.owner, 160))
        {
            return Err(err("invalid_report"));
        }
        let expected = match self.arch.as_str() {
            "x86_64" => Some(Arch::X86_64),
            "aarch64" => Some(Arch::Aarch64),
            _ => None,
        };
        if self.arch_supported != expected {
            return Err(err("invalid_report"));
        }
        if let Some(instance) = &self.instance {
            instance.info.validate(&self.home)?;
            if instance.info.instance_key != self.instance_key
                || instance.devices.len() > 1024
                || instance.devices.iter().any(|s| !text(s, 128))
            {
                return Err(err("invalid_report"));
            }
        }
        Ok(())
    }
}
impl DeployPlan {
    /// 传入本机 Syncthing 设备 ID，将其从升级确认名单排除；无效 ID 返回错误。
    pub fn identify_local_device(&mut self, device_id: &str) -> Result<(), AppError> {
        crate::syncthing::folder::validate_device_id(device_id)
            .map_err(|_| err("invalid_input"))?;
        self.local_device_id = Some(device_id.into());
        self.other_devices.retain(|d| d != device_id);
        Ok(())
    }
}
/// 根据检查报告和随包版本生成计划；升级确认和自动启动授权默认关闭，由调用方记录后设置。
pub fn plan(report: &ServerReport, bundled_version: &str) -> Result<DeployPlan, AppError> {
    report.validate()?;
    let bundled = version(bundled_version)?;
    let arch = report
        .arch_supported
        .ok_or_else(|| err("unsupported_arch").with("arch", &report.arch))?;
    if report.home_noexec {
        return Err(err("home_noexec"));
    }
    let old = report.instance.as_ref();
    let mut upgrade = false;
    if let Some(old) = old {
        let v = version(&old.info.version)?;
        if v > bundled {
            return Err(err("instance_newer")
                .with("instance_version", &old.info.version)
                .with("bundled_version", bundled_version));
        }
        upgrade = v < bundled;
    }
    let instance_id = old
        .map(|i| i.info.id.clone())
        .or_else(|| report.pending_instance_id.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let socket_path = socket_path(&report.home, report.runtime_dir.as_deref(), &instance_id)?;
    Ok(DeployPlan {
        home: report.home.clone(),
        instance_key: report.instance_key.clone(),
        instance_id,
        version: bundled_version.trim_start_matches('v').to_owned(),
        arch,
        upload_required: old.is_none() || upgrade,
        new_instance: old.is_none(),
        systemd: report.systemd,
        socket_path,
        other_devices: old.map(|i| i.devices.clone()).unwrap_or_default(),
        upgrade,
        local_device_id: None,
        autostart_consent: old.is_some_and(|i| i.info.autostart_consent),
        upgrade_confirmed: false,
    })
}
/// home、runtime 为远程绝对路径，instance_id 为 UUID；优先运行目录，无法小于 100 字节则报错。
pub fn socket_path(
    home: &str,
    runtime: Option<&str>,
    instance_id: &str,
) -> Result<String, AppError> {
    if !path(home) || !id(instance_id) || runtime.is_some_and(|s| !path(s)) {
        return Err(err("invalid_input"));
    }
    if let Some(runtime) = runtime {
        let p = format!("{runtime}/pierlink-{}/api.sock", &instance_id[..8]);
        if p.len() < 100 {
            return Ok(p);
        }
    }
    let p = format!(
        "{home}/.local/state/pierlink/r/{}/api.sock",
        &instance_id[..8]
    );
    if p.len() < 100 {
        Ok(p)
    } else {
        Err(err("socket_path_too_long").with("home_bytes", home.len()))
    }
}
/// 从 Linux tcp/tcp6 文本解析 LISTEN 端口，返回 22000–22999 中未占用且不等于 exclude 的端口。
pub fn probe_port(tcp: &str, exclude: Option<u16>) -> Result<u16, AppError> {
    let mut used = BTreeSet::new();
    for line in tcp.lines().filter(|l| !l.trim().is_empty()) {
        let cols: Vec<_> = line.split_whitespace().collect();
        if cols.first() == Some(&"sl") {
            continue;
        }
        if cols.len() < 4 {
            return Err(err("cannot_probe_ports"));
        }
        if cols[3] == "0A" {
            let p = cols[1]
                .rsplit(':')
                .next()
                .and_then(|s| u16::from_str_radix(s, 16).ok())
                .ok_or_else(|| err("cannot_probe_ports"))?;
            used.insert(p);
        }
    }
    (22000..23000)
        .find(|p| !used.contains(p) && Some(*p) != exclude)
        .ok_or_else(|| err("no_free_port"))
}
/// 判断清单路径是否严格位于允许删除的子目录；拒绝根目录、路径穿越和相似前缀。
pub fn manifest_path_allowed(home: &str, target: &str) -> bool {
    if !path(home) || !path(target) {
        return false;
    }
    [
        format!("{home}/.local/share/pierlink/"),
        format!("{home}/.local/state/pierlink/"),
    ]
    .iter()
    .any(|p| target.starts_with(p))
        || target
            .strip_prefix(&format!("{home}/.config/systemd/user/pierlink-"))
            .is_some_and(|s| s.strip_suffix(".service").is_some_and(id))
}

/// 创建结果；凭据未应用时调用方须读取服务器现行密钥。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateOutcome {
    pub info: InstanceInfo,
    pub secrets_applied: bool,
}
