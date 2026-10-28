//! 同步文件夹配置与默认排除规则。
use super::error;
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderKind {
    #[default]
    SendReceive,
    ReceiveOnly,
}
#[derive(Debug, Clone)]
pub struct FolderSpec {
    pub id: String,
    pub label: String,
    pub path: PathBuf,
    pub kind: FolderKind,
    pub devices: Vec<String>,
    pub watch_delay_s: u32,
    pub versions_dir: Option<PathBuf>,
    pub paused: bool,
    /// 按目标端设置：本机 Windows 为 true，服务器 Linux 为 false。
    pub ignore_perms: bool,
}
impl Default for FolderSpec {
    fn default() -> Self {
        Self {
            id: new_folder_id(),
            label: String::new(),
            path: PathBuf::new(),
            kind: FolderKind::default(),
            devices: vec![],
            watch_delay_s: 10,
            versions_dir: None,
            paused: true,
            ignore_perms: false,
        }
    }
}
/// 无参数；返回随机生成的 pl- 加 16 位小写十六进制文件夹 ID。
pub fn new_folder_id() -> String {
    format!("pl-{:016x}", rand::random::<u64>())
}
pub(crate) fn validate_folder_id(id: &str) -> Result<(), AppError> {
    let suffix = id
        .strip_prefix("pl-est-")
        .or_else(|| id.strip_prefix("pl-"));
    if !suffix.is_some_and(|s| {
        s.len() == 16
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    }) {
        return Err(error("文件夹 ID 格式无效"));
    }
    Ok(())
}
// 临时编号只交给崩溃清理入口，普通任务 API 仍拒绝临时命名空间。
pub(crate) fn validate_task_folder_id(id: &str) -> Result<(), AppError> {
    validate_folder_id(id)?;
    if id.starts_with("pl-est-") {
        return Err(error("临时文件夹不能作为同步任务"));
    }
    Ok(())
}
/// 检查 id 是否为 8 组、每组 7 位的 Syncthing 大写 Base32 设备 ID；返回校验结果。
pub fn validate_device_id(id: &str) -> Result<(), AppError> {
    let groups: Vec<_> = id.split('-').collect();
    if groups.len() != 8
        || groups.iter().any(|s| {
            s.len() != 7
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
        })
    {
        return Err(error("设备 ID 格式无效"));
    }
    Ok(())
}
/// 逐级解析 path 中已有路径，返回无 Windows 扩展前缀的绝对路径；保留大小写和尚未创建的末尾。
/// 先解析链接再处理 ..，避免词法折叠绕过。
pub(crate) fn normalized(path: &Path) -> Result<PathBuf, AppError> {
    if !path.is_absolute() {
        return Err(error("必须使用绝对路径"));
    }
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            // 盘符前缀（如 \\?\C:）和根目录本身不能拿去查文件信息（Windows 会报错而不是“不存在”），直接拼接
            v @ (Component::Prefix(_) | Component::RootDir) => out.push(v),
            v => {
                out.push(v);
                match std::fs::symlink_metadata(&out) {
                    Ok(_) => {
                        out = crate::fs_local::plain_canonical(&out)
                            .map_err(|_| error("无法解析路径"))?
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err(error("无法读取路径")),
                }
            }
        }
    }
    #[cfg(windows)]
    {
        out = PathBuf::from(crate::fs_local::without_windows_prefix(
            &out.to_string_lossy(),
        ));
    }
    Ok(PathBuf::from(upper_drive(&out.to_string_lossy())))
}

/// Windows 盘符统一大写显示和保存（d:\work → D:\work）；盘符本来就不分大小写，其他路径原样返回
pub(crate) fn upper_drive(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_lowercase() {
        let mut out = path.to_owned();
        out.replace_range(..1, &path[..1].to_ascii_uppercase());
        out
    } else {
        path.to_owned()
    }
}
fn comparison(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}
impl FolderSpec {
    /// 根据本对象生成 REST 文件夹配置；检查 ID、设备、等待时间及旧版本路径，失败返回错误。
    pub fn to_config(&self) -> Result<Value, AppError> {
        validate_task_folder_id(&self.id)?;
        if !(1..=60).contains(&self.watch_delay_s) {
            return Err(error("同步等待须在 1–60 秒内"));
        }
        for id in &self.devices {
            validate_device_id(id)?;
        }
        let path = normalized(&self.path)?;
        let versions_dir = if let Some(dir) = &self.versions_dir {
            let dir = normalized(dir)?;
            let a = comparison(&path);
            let b = comparison(&dir);
            let user_home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .and_then(|p| normalized(Path::new(&p)).ok());
            if a.starts_with(&b)
                || dir.parent().is_none()
                || user_home.is_some_and(|p| comparison(&p) == b)
            {
                return Err(AppError::new("syncthing.versions_contains_folder"));
            }
            if b.starts_with(&a) {
                return Err(AppError::new("syncthing.versions_inside_folder"));
            }
            Some(dir)
        } else {
            None
        };
        let settings = settings_config(self.kind, self.watch_delay_s, versions_dir.as_deref())?;
        let mut config = json!({"id":self.id,"label":self.label,"path":path,"devices":self.devices.iter().map(|id| json!({"deviceID":id})).collect::<Vec<_>>(),"rescanIntervalS":3600,"maxConflicts":-1,"ignorePerms":self.ignore_perms,"markerName":".stfolder","paused":self.paused});
        if let (Some(config), Some(settings)) = (config.as_object_mut(), settings.as_object()) {
            config.extend(settings.clone());
        }
        Ok(config)
    }
}
// 创建和编辑任务共用此设置生成器；路径校验由目标端的调用方负责。
pub(crate) fn settings_config(
    kind: FolderKind,
    watch_delay_s: u32,
    versions_dir: Option<&Path>,
) -> Result<Value, AppError> {
    if !(1..=60).contains(&watch_delay_s) {
        return Err(error("同步等待须在 1–60 秒内"));
    }
    Ok(json!({
        "type": kind,
        "fsWatcherEnabled": true,
        "fsWatcherDelayS": watch_delay_s,
        "versioning": match versions_dir {
            Some(dir) => json!({"type":"trashcan", "params":{"cleanoutDays":"30"}, "fsPath":dir, "fsType":"basic"}),
            None => json!({"type":"", "params":{}}),
        }
    }))
}
/// 无参数；返回设计 3.3 的默认规则（保持顺序）。
pub fn default_ignores() -> Vec<String> {
    [
        "(?d)node_modules",
        "(?d).venv",
        "(?d)__pycache__",
        "(?d)target",
        "(?d)dist",
        "(?d)build",
        "(?d).cache",
        "(?d)*.tmp",
        ".env",
        "*.pem",
        "*.key",
        "(?d).git",
        "(?d)Thumbs.db",
        "(?d)desktop.ini",
        "(?d)~$*",
        "(?d)*.pierlink-part",
    ]
    .map(String::from)
    .to_vec()
}
/// 检查 lines 的数量、字节长度和控制字符；合法返回空值，不回显规则内容。
pub fn validate_ignores(lines: &[String]) -> Result<(), AppError> {
    if lines.len() > 10_000
        || lines
            .iter()
            .any(|line| line.len() > 4096 || line.contains(['\0', '\n', '\r']))
    {
        return Err(error("排除规则超过限制或包含非法控制字符"));
    }
    Ok(())
}
#[cfg(test)]
mod path_tests {
    #[test]
    fn windows_去前缀保留盘符与共享路径大小写() {
        for (input, expected) in [
            (r"\\?\C:\Work\Project", r"C:\Work\Project"),
            (r"\\?\UNC\Server\Share\Project", r"\\Server\Share\Project"),
            (r"C:\Work\Project", r"C:\Work\Project"),
        ] {
            assert_eq!(crate::fs_local::without_windows_prefix(input), expected);
        }
    }
}

#[cfg(test)]
mod drive_tests {
    use super::upper_drive;

    #[test]
    fn 盘符统一大写_其他路径不变() {
        assert_eq!(upper_drive(r"d:\work\demo"), r"D:\work\demo");
        assert_eq!(upper_drive(r"C:\work"), r"C:\work");
        assert_eq!(upper_drive("/home/u/work"), "/home/u/work");
        assert_eq!(upper_drive(r"\\server\share"), r"\\server\share");
        assert_eq!(upper_drive(""), "");
    }
}
