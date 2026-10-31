/// 免 sudo 获取的 Openbox 与 xterm 会话标识。
pub const LIGHT_DESKTOP_ID: &str = "pierlink-light";

use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct VncDetection {
    pub arch: String,
    pub glibc: Option<String>,
    pub distro: Distro,
    pub package_manager: PackageManager,
    pub tigervnc: Option<XvncBinary>,
    pub sessions: Vec<VncSession>,
    pub desktops: Vec<DesktopEntry>,
    /// 没有可用桌面会话且 apt 工具齐全时，可用 pierlink-light 创建轻量桌面。
    #[serde(default)]
    pub light_desktop_available: bool,
    #[serde(default)]
    pub pierlink_desktops: Vec<PierlinkDesktop>,
    pub tools: Tools,
    /// 用户命名空间及 overlay 的实测能力；旧检测结果未探测时为 None。
    #[serde(default)]
    pub xkb_namespace: Option<XkbNamespace>,
    pub logind: Option<Logind>,
    pub official_pkg: OfficialPkgCheck,
}

/// 仅探测挂载能力；前缀中的 xkbcomp 和键盘数据仍须在启动时检查。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct XkbNamespace {
    pub available: bool,
    pub reason: Option<String>,
    pub install_command: String,
}

/// 专用桌面的公开状态；时间为 Unix 毫秒，geometry 为宽×高。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PierlinkDesktop {
    pub display: u16,
    pub socket: String,
    pub desktop_id: String,
    pub desktop_name: String,
    pub created_at: u64,
    pub geometry: String,
    pub stale: bool,
    /// 无法启用 linger 时为 true，桌面可能随登录会话结束。
    #[serde(default)]
    pub linger_failed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct CreateDesktop {
    pub desktop_id: String,
    pub width: u16,
    pub height: u16,
    pub xvnc: XvncBinary,
}

#[derive(Debug, Clone, Default)]
pub struct AcquireOptions {
    pub allow_official_download: bool,
    pub manual_package: Option<std::path::PathBuf>,
}

/// 获取来源，按枚举顺序尝试；官方包各路线共用同一前提检查。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum AcquireSource {
    Installed,
    Apt,
    ServerDownload,
    LocalDownload,
    ManualPackage,
}

/// 阶段开始、字节进度及失败原因；total 未知时为 None。
#[derive(Debug, Clone, Serialize, Type)]
pub enum AcquireStep {
    Started {
        source: AcquireSource,
    },
    Download {
        source: AcquireSource,
        bytes: u64,
        total: Option<u64>,
    },
    Upload {
        bytes: u64,
        total: u64,
    },
    Failed {
        source: AcquireSource,
        error: crate::error::AppError,
    },
    Ready {
        binary: XvncBinary,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Distro {
    pub id: String,
    pub version_id: String,
    pub id_like: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum PackageManager {
    Apt,
    Dnf,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum Flavor {
    TigerVnc,
    TurboVnc,
    X11vnc,
    RealVnc,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct XvncBinary {
    pub path: String,
    pub version: (u16, u16, u16),
    pub flavor: Flavor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct VncSession {
    pub pid: u32,
    pub comm: String,
    pub flavor: Flavor,
    pub display: Option<u16>,
    pub listens: Vec<Listen>,
    pub unix_paths: Vec<String>,
    pub security_types_arg: Option<String>,
    pub started_by: StartedBy,
    pub pierlink: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Listen {
    pub addr: String,
    pub port: u16,
    pub scope: ListenScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum ListenScope {
    Loopback,
    All,
    Specific,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum StartedBy {
    SystemUnit(String),
    UserUnit(String),
    UserSession,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DesktopEntry {
    pub id: String,
    pub name: String,
    pub exec: String,
    pub desktop_names: Vec<String>,
    pub heavy: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Tools {
    pub setsid: bool,
    pub dbus_run_session: bool,
    pub dbus_launch: bool,
    pub xauth: bool,
    pub mcookie: bool,
    pub socat: bool,
    pub python3: bool,
    pub platform_python: bool,
    pub perl: bool,
    pub loginctl: bool,
    pub busctl: bool,
    pub curl: bool,
    pub wget: bool,
    pub xkbcomp: bool,
    pub xkb_rules: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Logind {
    pub kill_user_processes: bool,
    pub linger: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct OfficialPkgCheck {
    pub ok: bool,
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum VncTarget {
    UnixPath(String),
    Loopback(u16),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct RfbProbe {
    pub version: String,
    pub security_types: Vec<u32>,
    pub vencrypt_subtypes: Vec<u32>,
    pub no_auth: bool,
    pub novnc_supported: bool,
}
