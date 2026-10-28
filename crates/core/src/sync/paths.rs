use super::{error, invalid_response};
use crate::{
    error::AppError,
    process::SshProgram,
    remote::run::{RunLimits, run_script},
    ssh::{args::SshCommand, quote::sh_quote},
    storage::{dangerous_local_path, paths_overlap},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PathIssue {
    Dangerous { side: String, path: String },
    Overlap { side: String, task_id: String },
    CloudFolder { provider: String },
    OtherSyncthing { side: String },
    JoinExisting { folder_id: String },
    IgnoreFile { side: String },
    GitRepository { side: String },
    Unreadable { side: String },
}
pub struct ExistingTask {
    pub task_id: String,
    pub local_path: PathBuf,
}
/// existing 只包含本次检查服务器上的端点，路径应来自此前的 realpath 检查。
pub struct ExistingEndpoint {
    pub task_id: String,
    pub folder_id: String,
    pub remote_path: String,
}
#[derive(Default)]
pub struct LocalEnv {
    pub home: PathBuf,
    pub local_app_data: PathBuf,
    pub windows_dir: PathBuf,
    /// 环境中的 OneDrive、OneDriveConsumer、OneDriveCommercial 路径由调用方收集。
    pub onedrive: Vec<PathBuf>,
    pub dropbox: Vec<PathBuf>,
    pub network_drives: Vec<PathBuf>,
    /// 当前实例配置中的路径，或经调用方确认属于本实例的遗留标记路径。
    pub managed_paths: Vec<PathBuf>,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct RemotePathInfo {
    pub canonical_path: String,
    pub home: String,
    pub filesystem: String,
    pub marker_present: bool,
    pub has_ignore_file: bool,
    pub git_repository: bool,
}
pub(super) fn local_normalized(path: &Path) -> Result<PathBuf, AppError> {
    let out = crate::syncthing::folder::normalized(path)?;
    if out.to_str().is_none_or(|s| s.contains('\0')) {
        return Err(error("目录不是有效文本"));
    }
    Ok(out)
}

pub(super) fn remote_normalized(path: &str) -> Result<String, AppError> {
    if !path.starts_with('/') || path.contains('\0') {
        return Err(error("远程目录必须为绝对路径"));
    }
    let mut parts = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            v => parts.push(v),
        }
    }
    Ok(format!("/{}", parts.join("/")))
}
fn normalized_text(path: &Path) -> Result<String, AppError> {
    // 允许在 Linux 的纯函数测试中验证 Windows 路径；Windows 实机仍先解析符号链接和短名。
    #[cfg(not(windows))]
    if let Some(text) = path.to_str()
        && (text.as_bytes().get(1) == Some(&b':') || text.starts_with(r"\\"))
    {
        return Ok(text.to_owned());
    }
    Ok(local_normalized(path)?.to_string_lossy().into_owned())
}
fn equal(a: &str, b: &str) -> bool {
    lexical(a) == lexical(b)
}
fn lexical(s: &str) -> String {
    let windows = s.as_bytes().get(1) == Some(&b':') || s.starts_with(r"\\");
    let text = if windows {
        s.replace('\\', "/").to_lowercase()
    } else {
        s.into()
    };
    let text = if let Some(unc) = text.strip_prefix("//?/unc/") {
        format!("//{unc}")
    } else {
        text.strip_prefix("//?/").unwrap_or(&text).to_owned()
    };
    let mut parts = Vec::new();
    for p in text.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}
/// 规范化 path 后返回危险目录、任务重叠、网盘和外来标记问题；env 提供用户目录及归属记录。
pub fn check_local_path(path: &Path, existing: &[ExistingTask], env: &LocalEnv) -> Vec<PathIssue> {
    let mut issues = Vec::new();
    // UNC 即使离线或无权访问也是网络位置，必须在查询文件系统前识别。
    #[cfg(windows)]
    if is_unc(path) {
        issues.push(PathIssue::CloudFolder {
            provider: "network".into(),
        });
    }
    let Ok(p) = normalized_text(path) else {
        issues.push(PathIssue::Unreadable {
            side: "local".into(),
        });
        return issues;
    };
    let env_path =
        |p: &Path| normalized_text(p).unwrap_or_else(|_| p.to_string_lossy().into_owned());
    if dangerous_local_path(
        &p,
        &env_path(&env.home),
        &env_path(&env.local_app_data),
        &env_path(&env.windows_dir),
    ) {
        issues.push(PathIssue::Dangerous {
            side: "local".into(),
            path: p.clone(),
        });
    }
    for task in existing {
        match normalized_text(&task.local_path) {
            Ok(other) if paths_overlap(&p, &other) => issues.push(PathIssue::Overlap {
                side: "local".into(),
                task_id: task.task_id.clone(),
            }),
            Err(_) => issues.push(PathIssue::Unreadable {
                side: "local".into(),
            }),
            _ => {}
        }
    }
    for (provider, roots) in [
        ("OneDrive", &env.onedrive),
        ("Dropbox", &env.dropbox),
        ("network", &env.network_drives),
    ] {
        if roots.iter().any(|root| paths_overlap(&p, &env_path(root))) {
            issues.push(PathIssue::CloudFolder {
                provider: provider.into(),
            });
        }
    }
    let lower = p.replace('\\', "/").to_lowercase();
    for (name, component) in [("OneDrive", "onedrive"), ("Dropbox", "dropbox")] {
        if lower
            .split('/')
            .any(|p| p == component || (component == "onedrive" && p.starts_with("onedrive - ")))
            && !issues.contains(&PathIssue::CloudFolder {
                provider: name.into(),
            })
        {
            issues.push(PathIssue::CloudFolder {
                provider: name.into(),
            });
        }
    }
    if ((lower.starts_with("//") && !lower.starts_with("//?/"))
        || lower.starts_with("//?/unc/")
        || is_network(Path::new(&p)))
        && !issues.contains(&PathIssue::CloudFolder {
            provider: "network".into(),
        })
    {
        issues.push(PathIssue::CloudFolder {
            provider: "network".into(),
        });
    }
    let managed = env
        .managed_paths
        .iter()
        .chain(existing.iter().map(|t| &t.local_path))
        .any(|other| normalized_text(other).is_ok_and(|other| equal(&p, &other)));
    if path.join(".stfolder").exists() && !managed {
        issues.push(PathIssue::OtherSyncthing {
            side: "local".into(),
        });
    }
    if path.join(".stignore").exists() {
        issues.push(PathIssue::IgnoreFile {
            side: "local".into(),
        });
    }
    if path.join(".git").exists() {
        issues.push(PathIssue::GitRepository {
            side: "local".into(),
        });
    }
    issues
}
#[cfg(windows)]
fn is_unc(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    matches!(path.components().next(), Some(Component::Prefix(prefix))
        if matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..)))
}
#[cfg(windows)]
fn is_network(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    let root: PathBuf = path.components().take(2).collect();
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe { GetDriveTypeW(wide.as_ptr()) == 4 }
}
#[cfg(not(windows))]
fn is_network(_: &Path) -> bool {
    false
}
fn remote_issues(
    info: &RemotePathInfo,
    existing: &[ExistingEndpoint],
    managed_paths: &[String],
) -> Result<Vec<PathIssue>, AppError> {
    let p = remote_normalized(&info.canonical_path)?;
    let home = remote_normalized(&info.home)?;
    let under = |root: &str| p == root || p.starts_with(&format!("{root}/"));
    let system = [
        "/etc", "/usr", "/bin", "/sbin", "/boot", "/proc", "/sys", "/dev", "/run",
    ]
    .iter()
    .any(|root| under(root))
        || p.split('/').nth(1).is_some_and(|s| s.starts_with("lib"));
    let sensitive = [".ssh", ".local/share/pierlink", ".local/state/pierlink"]
        .iter()
        .any(|s| paths_overlap(&p, &format!("{home}/{s}")));
    let mut issues = Vec::new();
    if p == "/"
        || p == "/var"
        || p == home
        || home.starts_with(&format!("{p}/"))
        || system
        || sensitive
    {
        issues.push(PathIssue::Dangerous {
            side: "remote".into(),
            path: p.clone(),
        });
    }
    let mut managed = managed_paths
        .iter()
        .map(|p| remote_normalized(p))
        .collect::<Result<Vec<_>, _>>()?
        .contains(&p);
    for endpoint in existing {
        let other = remote_normalized(&endpoint.remote_path)?;
        if p == other {
            managed = true;
            issues.push(PathIssue::JoinExisting {
                folder_id: endpoint.folder_id.clone(),
            });
        } else if paths_overlap(&p, &other) {
            issues.push(PathIssue::Overlap {
                side: "remote".into(),
                task_id: endpoint.task_id.clone(),
            });
        }
    }
    if info.marker_present && !managed {
        issues.push(PathIssue::OtherSyncthing {
            side: "remote".into(),
        });
    }
    if info.has_ignore_file {
        issues.push(PathIssue::IgnoreFile {
            side: "remote".into(),
        });
    }
    if info.git_repository {
        issues.push(PathIssue::GitRepository {
            side: "remote".into(),
        });
    }
    Ok(issues)
}
/// 经 program/cmd 的只读脚本解析 remote_path 的真实路径及标记，和该服务器 existing 比较并返回检查结果。
/// managed_paths 为服务器实例配置中的规范路径；未连接时传空，调用方再结合 find_joinable 判断。
/// Git 排除文件仅提示调用方征求同意，本函数不修改仓库。
pub fn check_remote_path(
    program: &SshProgram,
    cmd: &SshCommand,
    remote_path: &str,
    existing: &[ExistingEndpoint],
    managed_paths: &[String],
) -> Result<(RemotePathInfo, Vec<PathIssue>), AppError> {
    if remote_path.is_empty() || remote_path.contains('\0') {
        return Err(error("远程路径无效"));
    }
    let script = format!(
        "target={}\n{}",
        sh_quote(remote_path)?,
        include_str!("path_check.sh")
    );
    let value = run_script(program, cmd, &script, RunLimits::default())?;
    if value.get("error").is_some() {
        return Err(
            AppError::new("sync.path_check_failed").with("reason", "无法解析目录或查询文件系统")
        );
    }
    let info: RemotePathInfo = serde_json::from_value(value).map_err(|_| invalid_response())?;
    let issues = remote_issues(&info, existing, managed_paths)?;
    Ok((info, issues))
}
