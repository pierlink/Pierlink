//! 任务设置预检、跨端更新与只接收端还原。
use super::{LocalSync, ServerLink, error, invalid_response};
use crate::{
    error::AppError,
    syncthing::{
        client::Client,
        folder::{FolderKind, FolderSpec, settings_config, validate_task_folder_id},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, path::PathBuf};

/// 可编辑的服务器同步方式；必须包含任务当前的全部服务器。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointSettings {
    pub server_id: String,
    pub mode: FolderKind,
}

/// 任务可编辑设置；versions_root 为本机项目目录以外的旧版本根目录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSettings {
    #[serde(default)]
    pub local_mode: FolderKind,
    pub endpoints: Vec<EndpointSettings>,
    pub watch_delay_s: u32,
    pub collab: bool,
    pub versioning: bool,
    pub versions_root: PathBuf,
}

pub(super) fn validate_modes(
    local: FolderKind,
    modes: impl IntoIterator<Item = FolderKind>,
) -> Result<(), AppError> {
    if local == FolderKind::ReceiveOnly {
        let modes: Vec<_> = modes.into_iter().collect();
        if modes.is_empty() || modes.contains(&FolderKind::ReceiveOnly) {
            return Err(AppError::new("sync.invalid_mode")
                .with("reason", "本机只接收时，所有服务器端点都必须为收发方式"));
        }
    }
    Ok(())
}

struct Change<'a> {
    name: &'a str,
    client: &'a Client,
    previous: Value,
    next: Value,
}
fn offline(cause: AppError, server_id: &str) -> AppError {
    if cause.code == "syncthing.unreachable" {
        AppError::new("sync.server_offline")
            .with("server_id", server_id)
            .with("cause", cause.code)
    } else {
        cause.with("server_id", server_id)
    }
}
fn read_folder(client: &Client, id: &str) -> Result<Value, AppError> {
    client
        .get_folder(id)?
        .ok_or_else(|| AppError::new("sync.folder_not_found").with("folder_id", id))
}
fn merge(previous: &Value, settings: Value) -> Result<Value, AppError> {
    let mut next = previous.clone();
    next.as_object_mut()
        .ok_or_else(invalid_response)?
        .extend(settings.as_object().ok_or_else(invalid_response)?.clone());
    Ok(next)
}
fn write(client: &Client, id: &str, config: &Value) -> Result<(), AppError> {
    client.call(
        "PUT",
        &format!("/rest/config/folders/{id}"),
        &[],
        Some(config.clone()),
    )?;
    Ok(())
}

fn server_change<'a>(
    link: &'a ServerLink,
    ep: &'a EndpointSettings,
    folder_id: &str,
    settings: &TaskSettings,
) -> Result<Change<'a>, AppError> {
    let previous = read_folder(link.client(), folder_id).map_err(|e| offline(e, &ep.server_id))?;
    let path =
        super::paths::remote_normalized(previous["path"].as_str().ok_or_else(invalid_response)?)?;
    let versions =
        super::paths::remote_normalized(&format!("{}/versions/{folder_id}", link.state_dir))?;
    if settings.versioning && crate::storage::paths_overlap(&path, &versions) {
        return Err(error("旧版本目录与同步目录重叠"));
    }
    let patch = settings_config(
        ep.mode,
        if settings.collab {
            1
        } else {
            settings.watch_delay_s
        },
        settings.versioning.then(|| std::path::Path::new(&versions)),
    )?;
    Ok(Change {
        name: &ep.server_id,
        client: link.client(),
        next: merge(&previous, patch)?,
        previous,
    })
}
fn local_change<'a>(
    local: &'a LocalSync,
    folder_id: &str,
    settings: &TaskSettings,
    previous: Value,
) -> Result<Change<'a>, AppError> {
    let generated = FolderSpec {
        id: folder_id.into(),
        path: PathBuf::from(previous["path"].as_str().ok_or_else(invalid_response)?),
        kind: settings.local_mode,
        watch_delay_s: if settings.collab {
            1
        } else {
            settings.watch_delay_s
        },
        versions_dir: settings
            .versioning
            .then(|| settings.versions_root.join(folder_id)),
        ..Default::default()
    }
    .to_config()?;
    let patch = ["type", "fsWatcherEnabled", "fsWatcherDelayS", "versioning"]
        .into_iter()
        .map(|key| (key.to_string(), generated[key].clone()))
        .collect();
    Ok(Change {
        name: "local",
        client: local.client(),
        next: merge(&previous, Value::Object(patch))?,
        previous,
    })
}
fn plan_settings<'a>(
    local: &'a LocalSync,
    links: &[&'a ServerLink],
    folder_id: &str,
    settings: &'a TaskSettings,
) -> Result<Vec<Change<'a>>, AppError> {
    validate_task_folder_id(folder_id)?;
    validate_modes(
        settings.local_mode,
        settings.endpoints.iter().map(|e| e.mode),
    )?;
    if !(1..=60).contains(&settings.watch_delay_s) || settings.endpoints.is_empty() {
        return Err(error("等待须为 1–60 秒且至少有一个端点"));
    }
    let previous = read_folder(local.client(), folder_id)?;
    let mut seen = HashSet::new();
    let mut devices = HashSet::new();
    let mut changes = Vec::new();
    for ep in &settings.endpoints {
        if !seen.insert(&ep.server_id) {
            return Err(error("端点重复"));
        }
        let mut matches = links.iter().filter(|l| l.server_id == ep.server_id);
        let link = matches
            .next()
            .ok_or_else(|| AppError::new("sync.server_offline").with("server_id", &ep.server_id))?;
        if matches.next().is_some() {
            return Err(error("每个端点须有唯一连接"));
        }
        if link.device_id == local.device_id() || !devices.insert(link.device_id.as_str()) {
            return Err(error("端点设备身份重复"));
        }
        changes.push(server_change(link, ep, folder_id, settings)?);
    }
    let shared: HashSet<_> = previous["devices"]
        .as_array()
        .ok_or_else(invalid_response)?
        .iter()
        .map(|d| d["deviceID"].as_str().ok_or_else(invalid_response))
        .collect::<Result<_, _>>()?;
    let shared: HashSet<_> = shared
        .into_iter()
        .filter(|id| *id != local.device_id())
        .collect();
    if shared != devices || links.len() != settings.endpoints.len() {
        return Err(error("设置和连接须覆盖当前任务的全部服务器端点"));
    }
    changes.insert(0, local_change(local, folder_id, settings, previous)?);
    Ok(changes)
}
fn apply_settings(folder_id: &str, changes: &[Change<'_>]) -> Result<(), AppError> {
    for (index, change) in changes.iter().enumerate() {
        if let Err(cause) = write(change.client, folder_id, &change.next) {
            let mut failures = Vec::new();
            // 失败请求也可能已经生效，必须一起恢复。
            for changed in changes[..=index].iter().rev() {
                if let Err(e) = write(changed.client, folder_id, &changed.previous) {
                    failures.push(format!("{}:{}", changed.name, e.code));
                }
            }
            return Err(if failures.is_empty() {
                cause
                    .with("endpoint", change.name)
                    .with("folder_id", folder_id)
            } else {
                AppError::new("sync.partial_failure")
                    .with("folder_id", folder_id)
                    .with("endpoint", change.name)
                    .with("cause", cause.code)
                    .with("failures", failures.join(","))
            });
        }
    }
    Ok(())
}

/// 用 settings 更新 folder_id 的本机和全部 links；完整预检后下发，成功后调用方再写数据库。
/// 任一端失败逆序恢复已尝试端的完整配置；恢复失败返回 sync.partial_failure 和 failures 端点列表。
/// 离线或缺少连接时返回 sync.server_offline，不下发任何设置。
pub fn update_task_settings(
    local: &LocalSync,
    links: &[&ServerLink],
    folder_id: &str,
    settings: &TaskSettings,
) -> Result<(), AppError> {
    apply_settings(
        folder_id,
        &plan_settings(local, links, folder_id, settings)?,
    )
}

/// 请求将本机 folder_id 的只接收改动还原为全局版本；完成后须重新查询 task_status。
/// 非只接收任务返回 sync.invalid_mode；还原异步执行，失败返回接口错误。
pub fn revert_local_changes(local: &LocalSync, folder_id: &str) -> Result<(), AppError> {
    validate_task_folder_id(folder_id)?;
    if read_folder(local.client(), folder_id)?["type"] != "receiveonly" {
        return Err(
            AppError::new("sync.invalid_mode").with("reason", "只有只接收文件夹能还原本地改动")
        );
    }
    local
        .client()
        .call("POST", "/rest/db/revert", &[("folder", folder_id)], None)?;
    Ok(())
}
