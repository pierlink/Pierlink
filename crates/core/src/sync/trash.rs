//! 回收站按既有版本根目录定位，不接受待删除的任意路径；不创建缓存或临时副本。
use super::{EstimateFileAccess, LocalSync, ServerLink, invalid_response};
use crate::{
    error::AppError,
    remote::run::{RunLimits, run_script},
    ssh::quote::sh_quote,
    syncthing::{client::Client, folder::validate_task_folder_id},
};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::Duration,
};

/// 回收站当前用量或成功删除的用量；字节数为文件逻辑大小，不计目录。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct TrashUsage {
    pub files: u64,
    pub bytes: u64,
}
/// 一个端点的查询结果；某服务器离线不影响其他端点展示。
#[derive(Debug, Serialize, Type)]
pub struct TrashEndpointUsage {
    /// None 表示本机；其他值为应用内服务器 ID。
    pub server_id: Option<String>,
    pub usage: Option<TrashUsage>,
    pub error: Option<AppError>,
}
/// 清空 local 的 folder_id 回收站内容，保留版本目录；versions_root 必须来自应用配置。
/// 返回实际删除的文件数和字节数；部分失败的错误参数也带 files、bytes。无需暂停同步。
/// 遍历期间新产生的版本可能留待下次清理；folder_id 是 Syncthing 文件夹 ID。
pub fn clear_local_trash(
    local: &LocalSync,
    versions_root: &Path,
    folder_id: &str,
) -> Result<TrashUsage, AppError> {
    local_trash(local, versions_root, folder_id, true)
}
/// 清空 link 对应服务器上 folder_id 的回收站，返回实际删除用量；使用该连接的后台命令通道。
pub fn clear_remote_trash(link: &ServerLink, folder_id: &str) -> Result<TrashUsage, AppError> {
    remote_trash(link, folder_id, true)
}
/// 查询 folder_id 在本机及 links 各服务器的回收站；根目录取应用配置和服务器实例目录。
/// 返回每个端点的用量或错误；版本目录尚未创建时用量为零，关闭版本保留后仍能查询遗留内容。
pub fn query_trash(
    local: &LocalSync,
    versions_root: &Path,
    links: &[&ServerLink],
    folder_id: &str,
) -> Result<Vec<TrashEndpointUsage>, AppError> {
    validate_task_folder_id(folder_id)?;
    let entry = |server_id, result: Result<TrashUsage, AppError>| match result {
        Ok(usage) => TrashEndpointUsage {
            server_id,
            usage: Some(usage),
            error: None,
        },
        Err(error) => TrashEndpointUsage {
            server_id,
            usage: None,
            error: Some(error),
        },
    };
    let mut result = vec![entry(
        None,
        local_trash(local, versions_root, folder_id, false),
    )];
    for link in links {
        result.push(entry(
            Some(link.server_id.clone()),
            remote_trash(link, folder_id, false),
        ));
    }
    Ok(result)
}
fn unsafe_path() -> AppError {
    AppError::new("sync.trash_unsafe_path")
        .with("reason", "回收站路径不安全、包含符号链接或与同步目录重叠")
}
fn io(error: std::io::Error) -> AppError {
    let mut out = AppError::new("sync.trash_io").with("reason", "无法读取或清理回收站");
    if let Some(code) = error.raw_os_error() {
        out = out.with("os_code", code);
    }
    out
}
fn folders(client: &Client, id: &str) -> Result<Vec<String>, AppError> {
    validate_task_folder_id(id)?;
    let value = client.list_folders()?;
    let folders = value.as_array().ok_or_else(invalid_response)?;
    if !folders.iter().any(|f| f["id"] == id) {
        return Err(AppError::new("sync.folder_not_found").with("folder_id", id));
    }
    folders
        .iter()
        .map(|f| {
            f["path"]
                .as_str()
                .map(String::from)
                .ok_or_else(invalid_response)
        })
        .collect()
}
fn local_trash(
    local: &LocalSync,
    root: &Path,
    id: &str,
    clear: bool,
) -> Result<TrashUsage, AppError> {
    let paths = folders(local.client(), id)?;
    local_usage(root, id, &paths, clear)
}
fn local_usage(
    root: &Path,
    id: &str,
    sync_paths: &[String],
    clear: bool,
) -> Result<TrashUsage, AppError> {
    validate_task_folder_id(id)?;
    if !root.is_absolute()
        || root.parent().is_none()
        || root
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(unsafe_path());
    }
    let target = root.join(id);
    // 规范化同步目录的已有祖先，连同符号链接解析后比较；不存在的版本目录也要检查。
    let canonical_target = crate::syncthing::folder::normalized(&target)?;
    for path in sync_paths {
        let path = crate::syncthing::folder::normalized(Path::new(path))?;
        if crate::storage::paths_overlap(
            &canonical_target.to_string_lossy(),
            &path.to_string_lossy(),
        ) {
            return Err(unsafe_path());
        }
    }
    let Some(directory) = local::open_path(&target)? else {
        return Ok(TrashUsage::default());
    };
    let mut usage = TrashUsage::default();
    local::walk(&directory, clear, &mut usage)
        .map_err(|e| e.with("files", usage.files).with("bytes", usage.bytes))?;
    Ok(usage)
}
fn remote_trash(link: &ServerLink, id: &str, clear: bool) -> Result<TrashUsage, AppError> {
    let paths = folders(link.client(), id)?;
    let root = format!("{}/versions", link.state_dir.trim_end_matches('/'));
    let body = remote_script(&root, id, &paths, clear)?;
    let value = match link.estimate_files.as_ref() {
        Some(EstimateFileAccess::Ssh { program, command }) => run_script(
            program,
            command,
            &body,
            RunLimits {
                timeout: Duration::from_secs(120),
                stdout_bytes: 4096,
                stderr_bytes: 1024,
            },
        )
        .map_err(|mut e| {
            e.params.remove("stderr_tail");
            e.with("reason", "服务器回收站操作中断，已处理数量无法确认")
                .with("server_id", &link.server_id)
                .with("counts_known", false)
        })?,
        // 本机双实例测试也必须显式声明文件访问方式。
        Some(EstimateFileAccess::Local) => return local_usage(Path::new(&root), id, &paths, clear),
        None => {
            return Err(AppError::new("sync.trash_transport_missing")
                .with("reason", "服务器缺少后台命令通道"));
        }
    };
    let usage: TrashUsage =
        serde_json::from_value(value.clone()).map_err(|_| invalid_response())?;
    match value.get("error") {
        None | Some(serde_json::Value::Null) => Ok(usage),
        Some(serde_json::Value::String(code)) if code == "unsafe_path" => Err(unsafe_path()
            .with("files", usage.files)
            .with("bytes", usage.bytes)),
        Some(serde_json::Value::String(_)) => Err(AppError::new("sync.trash_io")
            .with("reason", "服务器无法读取或清理回收站")
            .with("files", usage.files)
            .with("bytes", usage.bytes)),
        Some(_) => Err(invalid_response()),
    }
}
fn remote_script(
    root: &str,
    id: &str,
    sync_paths: &[String],
    clear: bool,
) -> Result<String, AppError> {
    validate_task_folder_id(id)?;
    if !root.starts_with('/')
        || root == "/"
        || root
            .split('/')
            .skip(1)
            .any(|s| matches!(s, "" | "." | ".."))
    {
        return Err(unsafe_path());
    }
    let mut script = format!(
        "root={}\nid={}\nclear={}\n",
        sh_quote(root)?,
        sh_quote(id)?,
        if clear { 1 } else { 0 }
    );
    script.push_str(include_str!("trash.sh"));
    for path in sync_paths {
        script.push_str(&format!("check_sync {} || exit 0\n", sh_quote(path)?));
    }
    script.push_str("run_trash\n");
    Ok(script)
}

mod local;
#[cfg(test)]
mod tests;
