use super::{LocalSync, ServerLink, error, invalid_response};
use crate::{
    error::AppError,
    syncthing::{
        client::Client,
        folder::{
            FolderKind, FolderSpec, new_folder_id, validate_ignores, validate_task_folder_id,
        },
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

/// 加入时的规则来源；调用方须先展示服务器规则，空规则冲突须明确确认。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IgnoresSource {
    /// 尚未确认；服务器为空而本机非空时返回规则冲突。
    #[default]
    Unconfirmed,
    /// 用户确认沿用服务器规则，包括空规则。
    Server,
    /// 用户确认以本次规则覆盖服务器。
    Local,
}
pub struct TaskSpec {
    /// 本机数据目录；回滚恢复副本保留 30 天。
    pub data_dir: PathBuf,
    pub folder_id: Option<String>,
    pub name: String,
    pub local_path: PathBuf,
    /// 本机同步方向，通常使用默认的收发方式。
    pub local_mode: FolderKind,
    pub endpoints: Vec<EndpointSpec>,
    pub ignore_rules: Vec<String>,
    pub ignores_source: IgnoresSource,
    pub watch_delay_s: u32,
    pub collab: bool,
    pub versioning: bool,
    pub versions_root: PathBuf,
}
pub struct EndpointSpec {
    pub server_id: String,
    pub remote_path: String,
    pub mode: FolderKind,
}
#[derive(Debug, Clone, Serialize)]
pub struct Joinable {
    pub folder_id: String,
    pub devices: Vec<String>,
    pub ignores: Vec<String>,
}
fn rules(client: &Client, id: &str) -> Result<Vec<String>, AppError> {
    let response = client.get_ignores(id)?;
    if response
        .get("error")
        .is_some_and(|e| !e.is_null() && e != "")
    {
        return Err(invalid_response());
    }
    match response.get("ignore") {
        // Syncthing 的空规则切片会编码为 null；缺字段和错误类型仍属于无效响应。
        Some(Value::Null) => Ok(Vec::new()),
        Some(value) => serde_json::from_value(value.clone()).map_err(|_| invalid_response()),
        None => Err(invalid_response()),
    }
}
fn devices(folder: &Value) -> Result<Vec<Value>, AppError> {
    folder["devices"]
        .as_array()
        .cloned()
        .ok_or_else(invalid_response)
}
fn folder(client: &Client, id: &str) -> Result<Value, AppError> {
    client
        .get_folder(id)?
        .ok_or_else(|| AppError::new("sync.folder_not_found").with("folder_id", id))
}
/// 查询规范化后的 remote_path；返回可加入的文件夹、共享设备和服务器规则，无匹配返回 None。
/// 远程符号链接须先用 check_remote_path 解析，并传入 canonical_path。
pub fn find_joinable(link: &ServerLink, remote_path: &str) -> Result<Option<Joinable>, AppError> {
    let path = super::paths::remote_normalized(remote_path)?;
    let all = link.client().list_folders()?;
    for f in all.as_array().ok_or_else(invalid_response)? {
        let other =
            super::paths::remote_normalized(f["path"].as_str().ok_or_else(invalid_response)?)?;
        if path == other {
            let id = f["id"].as_str().ok_or_else(invalid_response)?;
            validate_task_folder_id(id).map_err(|_| {
                AppError::new("sync.foreign_folder")
                    .with("path", &path)
                    .with("folder_id", id)
            })?;
            return Ok(Some(Joinable {
                folder_id: id.into(),
                devices: devices(f)?
                    .iter()
                    .map(|d| {
                        d["deviceID"]
                            .as_str()
                            .map(String::from)
                            .ok_or_else(invalid_response)
                    })
                    .collect::<Result<_, _>>()?,
                ignores: rules(link.client(), id)?,
            }));
        }
        if crate::storage::paths_overlap(&path, &other) {
            return Err(AppError::new("sync.path_overlap"));
        }
    }
    Ok(None)
}
struct Plan<'a> {
    client: &'a Client,
    config: Value,
    previous: Option<Value>,
    ignores: Option<Vec<String>>,
    previous_ignores: Option<Vec<String>>,
    marker: Option<super::markers::Marker<'a>>,
    ignore_file: Option<super::estimate_files::IgnoreFile<'a>>,
}
fn put(client: &Client, config: Value) -> Result<(), AppError> {
    let id = config["id"]
        .as_str()
        .ok_or_else(invalid_response)?
        .to_owned();
    validate_task_folder_id(&id)?;
    client.call(
        "PUT",
        &format!("/rest/config/folders/{id}"),
        &[],
        Some(config),
    )?;
    Ok(())
}
/// 按 spec 创建暂停任务或加入已确认的文件夹；失败回滚配置，返回文件夹 ID，不删除文件。
/// 调用方必须向用户展示 Joinable.ignores；确认规则来源后设置 ignores_source。
/// 新端点须先设置原始文件访问方式，以便失败时恢复标记。
/// data_dir 用于保存回滚失败的原规则副本，调用方仍需接入 DeletionGuard。
pub fn create_task(
    local: &LocalSync,
    links: &[&ServerLink],
    spec: &TaskSpec,
) -> Result<String, AppError> {
    validate_ignores(&spec.ignore_rules)?;
    super::settings::validate_modes(spec.local_mode, spec.endpoints.iter().map(|e| e.mode))?;
    if !(1..=60).contains(&spec.watch_delay_s) || spec.endpoints.is_empty() {
        return Err(error(
            "sync.wait_time_must_be_1_60_seconds_and_at_least",
            "等待须为 1–60 秒且至少有一个端点",
        ));
    }
    let id = spec.folder_id.clone().unwrap_or_else(new_folder_id);
    validate_task_folder_id(&id)?;
    if local.client().get_folder(&id)?.is_some() {
        return Err(AppError::new("sync.folder_exists"));
    }
    let mut used = HashSet::new();
    let mut device_ids = HashSet::new();
    let mut plans = Vec::new();
    let mut local_rules = spec.ignore_rules.clone();
    let mut joined_rules = None;
    for ep in &spec.endpoints {
        if !used.insert(&ep.server_id) {
            return Err(error("sync.duplicate_endpoint", "端点重复"));
        }
        let matches: Vec<_> = links
            .iter()
            .filter(|l| l.server_id == ep.server_id)
            .collect();
        if matches.len() != 1 {
            return Err(error(
                "sync.each_endpoint_must_have_a_unique_connection",
                "每个端点须有唯一连接",
            ));
        }
        let link = *matches[0];
        if !device_ids.insert(link.device_id.clone()) || link.device_id == local.device_id() {
            return Err(error(
                "sync.duplicate_endpoint_device_identity",
                "端点设备身份重复",
            ));
        }
        let join = find_joinable(link, &ep.remote_path)?;
        let previous = link.client().get_folder(&id)?;
        if let Some(join) = join {
            if spec.folder_id.is_none() || join.folder_id != id {
                return Err(AppError::new("sync.join_required").with("folder_id", join.folder_id));
            }
            if spec.ignores_source == IgnoresSource::Unconfirmed
                && join.ignores.is_empty()
                && !spec.ignore_rules.is_empty()
            {
                return Err(AppError::new("sync.ignores_mismatch")
                    .with("server_count", join.ignores.len())
                    .with("local_count", spec.ignore_rules.len()));
            }
            let selected = if spec.ignores_source == IgnoresSource::Local {
                spec.ignore_rules.clone()
            } else {
                join.ignores.clone()
            };
            if joined_rules.as_ref().is_some_and(|r| r != &selected) {
                return Err(AppError::new("sync.ignores_mismatch")
                    .with("server_count", selected.len())
                    .with("local_count", local_rules.len()));
            }
            joined_rules = Some(selected.clone());
            local_rules = selected;
            let mut config = previous.clone().ok_or_else(invalid_response)?;
            if spec.local_mode == FolderKind::ReceiveOnly {
                let actual_mode = serde_json::from_value(config["type"].clone())
                    .map_err(|_| invalid_response())?;
                super::settings::validate_modes(spec.local_mode, [actual_mode])?;
            }
            let mut shared = devices(&config)?;
            if !shared.iter().any(|d| d["deviceID"] == local.device_id()) {
                shared.push(json!({"deviceID":local.device_id()}));
            }
            config["devices"] = json!(shared);
            plans.push(Plan {
                client: link.client(),
                config,
                previous,
                ignores: (spec.ignores_source == IgnoresSource::Local).then(|| local_rules.clone()),
                previous_ignores: Some(join.ignores),
                marker: None,
                ignore_file: None,
            });
        } else {
            if previous.is_some() {
                return Err(AppError::new("sync.folder_exists"));
            }
            let path = super::paths::remote_normalized(&ep.remote_path)?;
            let versions =
                super::paths::remote_normalized(&format!("{}/versions/{id}", link.state_dir))?;
            if spec.versioning && crate::storage::paths_overlap(&path, &versions) {
                return Err(error(
                    "sync.version_directory_overlaps_the_sync_directory",
                    "旧版本目录与同步目录重叠",
                ));
            }
            let mut config = json!({"id":id,"label":spec.name,"path":path,"devices":[{"deviceID":local.device_id()}],"paused":true,"rescanIntervalS":3600,"maxConflicts":-1,"ignorePerms":false,"markerName":".stfolder"});
            let settings = crate::syncthing::folder::settings_config(
                ep.mode,
                if spec.collab { 1 } else { spec.watch_delay_s },
                spec.versioning.then(|| Path::new(&versions)),
            )?;
            config
                .as_object_mut()
                .ok_or_else(invalid_response)?
                .extend(settings.as_object().ok_or_else(invalid_response)?.clone());
            plans.push(Plan {
                client: link.client(),
                config,
                previous: None,
                ignores: Some(Vec::new()),
                previous_ignores: None,
                ignore_file: Some(
                    super::estimate_files::IgnoreFile::new(
                        link.estimate_files
                            .as_ref()
                            .ok_or_else(|| AppError::new("sync.estimate_transport_missing"))?,
                        &path,
                    )?
                    .with_recovery(&spec.data_dir, &id),
                ),
                marker: Some(super::markers::Marker::new(
                    link.estimate_files
                        .as_ref()
                        .ok_or_else(|| AppError::new("sync.estimate_transport_missing"))?,
                    &path,
                )?),
            });
        }
    }
    for plan in &mut plans {
        if plan.ignores.is_some() {
            plan.ignores = Some(local_rules.clone());
        }
    }
    let config = FolderSpec {
        id: id.clone(),
        label: spec.name.clone(),
        path: spec.local_path.clone(),
        kind: spec.local_mode,
        devices: device_ids.into_iter().collect(),
        watch_delay_s: if spec.collab { 1 } else { spec.watch_delay_s },
        versions_dir: spec.versioning.then(|| spec.versions_root.join(&id)),
        paused: true,
        ignore_perms: cfg!(windows),
    }
    .to_config()?;
    // 先完整预检所有端点，避免参数错误造成部分创建。
    let local_path = config["path"].as_str().ok_or_else(invalid_response)?;
    let existing = local.client().list_folders()?;
    for f in existing.as_array().ok_or_else(invalid_response)? {
        if crate::storage::paths_overlap(
            local_path,
            f["path"].as_str().ok_or_else(invalid_response)?,
        ) {
            return Err(AppError::new("sync.path_overlap"));
        }
    }
    let local_files = super::EstimateFileAccess::Local;
    let local_ignore = super::estimate_files::IgnoreFile::new(&local_files, local_path)?
        .with_recovery(&spec.data_dir, &id);
    let local_marker = super::markers::Marker::new(&local_files, local_path)?;
    plans.insert(
        0,
        Plan {
            client: local.client(),
            config,
            previous: None,
            ignores: Some(local_rules),
            previous_ignores: None,
            marker: Some(local_marker),
            ignore_file: Some(local_ignore),
        },
    );
    for index in 0..plans.len() {
        let plan = &plans[index];
        let result = if let Some(previous) = &plan.previous {
            if previous["devices"] == plan.config["devices"] {
                Ok(())
            } else {
                plan.client
                    .patch_folder(&id, json!({"devices":plan.config["devices"]}))
            }
        } else {
            put(plan.client, plan.config.clone())
        }
        .and_then(|()| match &plan.ignores {
            Some(r) => plan.client.set_ignores(&id, r),
            None => Ok(()),
        });
        if let Err(cause) = result {
            let mut failed = Vec::new();
            let mut marker_error = None;
            let mut recovery_paths = Vec::new();
            for p in plans[..=index].iter_mut().rev() {
                let rollback = match &p.previous {
                    Some(old) => p
                        .client
                        .patch_folder(&id, json!({"devices":old["devices"]})),
                    None => super::estimate::delete_confirmed(p.client, &id),
                };
                if rollback.is_ok()
                    && let Some(marker) = &p.marker
                    && let Err(e) = marker.restore()
                {
                    marker_error = Some(e);
                }
                if let (Some(file), Some(rules)) = (&mut p.ignore_file, &p.ignores)
                    && let Err(e) = file.restore_api_write(rules)
                {
                    if let Some(path) = e.params.get("recovery_path") {
                        recovery_paths.push(path.clone());
                    }
                    marker_error = Some(e);
                }
                if p.ignores.is_some()
                    && let Some(old) = &p.previous_ignores
                    && let Err(e) = p.client.set_ignores(&id, old)
                {
                    failed.push(e.code);
                }
                if let Err(e) = rollback
                    && !(e.code == "syncthing.http_error"
                        && e.params.get("status").is_some_and(|s| s == "404"))
                {
                    failed.push(e.code);
                }
            }
            if let Some(e) = marker_error {
                return Err(e
                    .with("cause", cause.code)
                    .with("cleanup", failed.join(","))
                    .with("recovery_path", recovery_paths.join(",")));
            }
            return if failed.is_empty() {
                Err(cause)
            } else {
                Err(AppError::new("sync.rollback_failed")
                    .with("cause", cause.code)
                    .with("folder_id", &id)
                    .with("cleanup", failed.join(",")))
            };
        }
    }
    Ok(id)
}
fn all(
    local: &LocalSync,
    links: &[&ServerLink],
    mut action: impl FnMut(&Client) -> Result<(), AppError>,
) -> Result<(), AppError> {
    let mut failures = Vec::new();
    if let Err(e) = action(local.client()) {
        failures.push(format!("local:{}", e.code));
    }
    for link in links {
        if let Err(e) = action(link.client()) {
            failures.push(format!("{}:{}", link.server_id, e.code));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(AppError::new("sync.partial_failure").with("failures", failures.join(",")))
    }
}
/// 将本机 folder_id 设为 paused；服务器仅在共享设备只有本机时跟随，部分失败返回端点错误码。
pub fn set_paused(
    local: &LocalSync,
    links: &[&ServerLink],
    folder_id: &str,
    paused: bool,
) -> Result<(), AppError> {
    all(local, links, |client| {
        if !std::ptr::eq(client, local.client()) {
            let link = links
                .iter()
                .find(|link| std::ptr::eq(link.client(), client))
                .ok_or_else(invalid_response)?;
            let shared = devices(&folder(client, folder_id)?)?;
            if has_other_computers(&shared, &link.device_id, local.device_id()) {
                return Ok(());
            }
        }
        client.patch_folder(folder_id, json!({"paused": paused}))
    })
}
// 服务器自身不算共享电脑；保留设备对象，避免丢失设备级配置。
fn has_other_computers(shared: &[Value], server_id: &str, local_id: &str) -> bool {
    shared
        .iter()
        .any(|d| d["deviceID"] != server_id && d["deviceID"] != local_id)
}
/// 请求本机及 links 重新扫描 folder_id；返回汇总结果。
pub fn rescan(local: &LocalSync, links: &[&ServerLink], folder_id: &str) -> Result<(), AppError> {
    all(local, links, |c| c.scan(folder_id, "").map(|_| ()))
}
/// 移除本机 folder_id 配置，服务器仅取消本机共享；除服务器自己外无剩余设备才删除配置，保留全部文件。
/// 成功后关闭不再使用的服务器隧道。
pub fn remove_task(
    local: &LocalSync,
    links: &[&ServerLink],
    folder_id: &str,
) -> Result<(), AppError> {
    all(local, links, |c| {
        if std::ptr::eq(c, local.client()) {
            c.remove_folder(folder_id)
        } else {
            let server_id = links
                .iter()
                .find(|l| std::ptr::eq(l.client(), c))
                .map(|l| l.device_id.as_str())
                .ok_or_else(invalid_response)?;
            unshare(c, folder_id, local.device_id(), server_id)
        }
    })?;
    for link in links {
        link.stop_if_unused(local)?;
    }
    Ok(())
}
/// 从本机取消对 link 的共享；服务器移除本机设备，除服务器自己外无剩余设备才删除 folder_id 配置，保留文件。
/// 本机只接收时，移除最后一个服务器返回 sync.invalid_mode；移除整个任务请调用 remove_task。
pub fn remove_endpoint(
    local: &LocalSync,
    link: &ServerLink,
    folder_id: &str,
) -> Result<(), AppError> {
    let f = folder(local.client(), folder_id)?;
    let shared: Vec<_> = devices(&f)?
        .into_iter()
        .filter(|d| d["deviceID"] != link.device_id)
        .collect();
    if f["type"] == "receiveonly" && !shared.iter().any(|d| d["deviceID"] != local.device_id()) {
        return Err(AppError::new("sync.invalid_mode").with_reason(
            "sync.a_local_receive_only_task_needs_at_least_one_send",
            "本机只接收任务至少需要保留一个收发服务器端点",
        ));
    }
    local
        .client()
        .patch_folder(folder_id, json!({"devices":shared}))?;
    unshare(link.client(), folder_id, local.device_id(), &link.device_id)?;
    link.stop_if_unused(local)
}
/// 服务器端取消对本电脑的共享；除服务器自己外没有其他设备时删除文件夹配置（不删文件）
fn unshare(client: &Client, id: &str, device: &str, server_id: &str) -> Result<(), AppError> {
    let Some(f) = client.get_folder(id)? else {
        return Ok(());
    };
    let shared: Vec<_> = devices(&f)?
        .into_iter()
        .filter(|d| d["deviceID"] != device)
        .collect();
    // Syncthing 会把服务器自己放进共享列表，只剩它自己时等于没人在用
    if !has_other_computers(&shared, server_id, device) {
        client.remove_folder(id)
    } else {
        client.patch_folder(id, json!({"devices": shared}))
    }
}
/// 给 folder_id 添加 link 端点；remote_path 须先经过远程路径检查，mode 为服务器同步方式。
/// 从本机复制规则和设置，服务器暂停创建；返回执行结果，失败逆序回滚配置，保留文件。
/// link 须先设置原始文件访问方式；data_dir 为本机数据目录，回滚恢复副本保留 30 天。
pub fn add_endpoint(
    local: &LocalSync,
    link: &ServerLink,
    folder_id: &str,
    remote_path: &str,
    mode: FolderKind,
    data_dir: &Path,
) -> Result<(), AppError> {
    let original = folder(local.client(), folder_id)?;
    let local_mode =
        serde_json::from_value(original["type"].clone()).map_err(|_| invalid_response())?;
    super::settings::validate_modes(local_mode, [mode])?;
    let mut shared = devices(&original)?;
    if link.device_id == local.device_id() || shared.iter().any(|d| d["deviceID"] == link.device_id)
    {
        return Err(error(
            "sync.duplicate_endpoint_device_identity",
            "端点设备身份重复",
        ));
    }
    if link.client().get_folder(folder_id)?.is_some() {
        return Err(AppError::new("sync.folder_exists"));
    }
    if let Some(join) = find_joinable(link, remote_path)? {
        return Err(AppError::new("sync.join_required").with("folder_id", join.folder_id));
    }
    let ignores = rules(local.client(), folder_id)?;
    let path = super::paths::remote_normalized(remote_path)?;
    let mut config = original.clone();
    config["path"] = json!(path);
    config["paused"] = json!(true);
    config["ignorePerms"] = json!(false);
    config["devices"] = json!([{"deviceID":local.device_id()}]);
    config["type"] = json!(match mode {
        FolderKind::SendReceive => "sendreceive",
        FolderKind::ReceiveOnly => "receiveonly",
    });
    if config["versioning"]["type"]
        .as_str()
        .is_some_and(|t| !t.is_empty())
    {
        let versions =
            super::paths::remote_normalized(&format!("{}/versions/{folder_id}", link.state_dir))?;
        if crate::storage::paths_overlap(&path, &versions) {
            return Err(error(
                "sync.version_directory_overlaps_the_sync_directory",
                "旧版本目录与同步目录重叠",
            ));
        }
        config["versioning"]["fsPath"] = json!(versions);
    }
    shared.push(json!({"deviceID":link.device_id}));
    let marker = super::markers::Marker::new(
        link.estimate_files
            .as_ref()
            .ok_or_else(|| AppError::new("sync.estimate_transport_missing"))?,
        &path,
    )?;
    let mut ignore_file = super::estimate_files::IgnoreFile::new(
        link.estimate_files
            .as_ref()
            .ok_or_else(|| AppError::new("sync.estimate_transport_missing"))?,
        &path,
    )?
    .with_recovery(data_dir, folder_id);
    let mut remote_attempted = false;
    let result = local
        .client()
        .patch_folder(folder_id, json!({"devices":shared}))
        .and_then(|()| {
            remote_attempted = true;
            put(link.client(), config)
        })
        .and_then(|()| link.client().set_ignores(folder_id, &ignores));
    if let Err(cause) = result {
        let mut failures = Vec::new();
        let removed = if remote_attempted {
            super::estimate::delete_confirmed(link.client(), folder_id)
        } else {
            Ok(())
        };
        if let Err(e) = &removed
            && !(e.code == "syncthing.http_error"
                && e.params.get("status").is_some_and(|s| s == "404"))
        {
            failures.push(e.code.clone());
        }
        let mut recovery_error = None;
        let marker_result = if remote_attempted {
            if let Err(e) = ignore_file.restore_api_write(&ignores) {
                recovery_error = Some(e);
            }
            if removed.is_ok() {
                marker.restore()
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        if let Err(e) = local
            .client()
            .patch_folder(folder_id, json!({"devices":original["devices"]}))
        {
            failures.push(e.code);
        }
        if let Some(e) = recovery_error {
            return Err(e
                .with("cause", cause.code)
                .with("cleanup", failures.join(","))
                .with(
                    "marker_error",
                    marker_result.err().map(|e| e.code).unwrap_or_default(),
                ));
        }
        marker_result?;
        if !failures.is_empty() {
            return Err(AppError::new("sync.rollback_failed")
                .with("folder_id", folder_id)
                .with("cause", cause.code)
                .with("cleanup", failures.join(",")));
        }
        return Err(cause);
    }
    Ok(())
}
/// 将 rules 写到 folder_id 的本机和全部服务器；所有端均尝试，返回汇总结果。
pub fn update_ignores(
    local: &LocalSync,
    links: &[&ServerLink],
    folder_id: &str,
    rules: &[String],
) -> Result<(), AppError> {
    validate_ignores(rules)?;
    all(local, links, |c| c.set_ignores(folder_id, rules))
}
/// 移除本机 folder_id 配置并等待索引清除，再以 new_path 暂停重新加入；保留原设置和排除规则。
pub fn relocate_local(local: &LocalSync, folder_id: &str, new_path: &Path) -> Result<(), AppError> {
    relocate(local.client(), folder_id, new_path, Duration::from_secs(30))
}
pub(super) fn relocate(
    client: &Client,
    id: &str,
    path: &Path,
    timeout: Duration,
) -> Result<(), AppError> {
    let path = super::paths::local_normalized(path)?;
    let mut config = folder(client, id)?;
    let ignores = rules(client, id)?;
    if let Some(versions) = config["versioning"]["fsPath"]
        .as_str()
        .filter(|p| !p.is_empty())
        && crate::storage::paths_overlap(&path.to_string_lossy(), versions)
    {
        return Err(error(
            "sync.new_location_overlaps_the_version_directory",
            "新位置与旧版本目录重叠",
        ));
    }
    let folders = client.list_folders()?;
    for f in folders.as_array().ok_or_else(invalid_response)? {
        if f["id"] != id
            && crate::storage::paths_overlap(
                &path.to_string_lossy(),
                f["path"].as_str().ok_or_else(invalid_response)?,
            )
        {
            return Err(AppError::new("sync.path_overlap"));
        }
    }
    // 旧目录里的 .stfolder 是这个任务自己创建的；任务搬走后 Syncthing 会把它改名或移走，
    // 这里不恢复（恢复反而会让旧目录看起来仍在同步）。预估和创建失败回滚才需要恢复用户原有的标记。
    client.remove_folder(id)?;
    let incomplete = |stage: &str, cause: AppError| {
        AppError::new("sync.relocate_incomplete")
            .with("stage", stage)
            .with("folder_id", id)
            .with("cause", cause.code)
    };
    wait_index_clear(client, id, timeout).map_err(|e| incomplete("index_clear", e))?;
    config["path"] = json!(path);
    config["paused"] = json!(true);
    put(client, config).map_err(|e| incomplete("readd", e))?;
    client
        .set_ignores(id, &ignores)
        .map_err(|e| incomplete("ignores", e))
}
// 配置中缺少文件夹不代表后台已删完旧索引，重新加入前必须等待。
pub(super) fn wait_index_clear(
    client: &Client,
    id: &str,
    timeout: Duration,
) -> Result<(), AppError> {
    let deadline = Instant::now() + timeout;
    loop {
        match client.db_status(id) {
            Err(e)
                if e.code == "syncthing.http_error"
                    && e.params.get("status").is_some_and(|s| s == "404") =>
            {
                break;
            }
            Ok(v)
                if v["localFiles"].as_u64() == Some(0)
                    && v["localDeleted"].as_u64().unwrap_or(0) == 0
                    && v["localDirectories"].as_u64().unwrap_or(0) == 0 =>
            {
                break;
            }
            Err(e) => return Err(e),
            _ if Instant::now() >= deadline => {
                return Err(AppError::new("sync.index_clear_timeout"));
            }
            _ => thread::sleep(Duration::from_millis(50)),
        }
    }
    Ok(())
}
#[derive(Debug, Serialize, Deserialize)]
pub struct FolderError {
    pub path: String,
    pub reason: String,
}
#[derive(Debug, Serialize)]
pub struct ServerStatus {
    pub server_id: String,
    pub connected: bool,
    pub completion: Option<f64>,
    pub need_items: Option<u64>,
    pub need_deletes: Option<u64>,
    pub error: Option<AppError>,
}
#[derive(Debug, Serialize)]
pub struct TaskStatus {
    pub state: String,
    pub global_files: u64,
    pub global_bytes: u64,
    pub need_bytes: u64,
    /// 本机只接收文件夹中的本地改动项目数（含文件、目录和删除）。
    pub local_changed_items: u64,
    pub errors: Vec<FolderError>,
    pub servers: Vec<ServerStatus>,
}
/// 根据本机 folder_id 状态和服务器 ID/设备 ID 列表返回分端状态；离线端不影响其他端查询。
/// Y28 各端排除规则摘要核对留待后续实现，目前不返回规则摘要。
pub fn task_status(
    local: &LocalSync,
    folder_id: &str,
    servers: &[(String, String)],
) -> Result<TaskStatus, AppError> {
    let c = local.client();
    let status = c.db_status(folder_id)?;
    let paused = folder(c, folder_id)?["paused"].as_bool().unwrap_or(false);
    let connections = c.connections()?;
    let errors = c.call(
        "GET",
        "/rest/folder/errors",
        &[("folder", folder_id), ("page", "1"), ("perpage", "100")],
        None,
    )?;
    let mut result = TaskStatus {
        state: if paused {
            "paused".into()
        } else {
            status["state"]
                .as_str()
                .ok_or_else(invalid_response)?
                .into()
        },
        global_files: status["globalFiles"]
            .as_u64()
            .ok_or_else(invalid_response)?,
        global_bytes: status["globalBytes"]
            .as_u64()
            .ok_or_else(invalid_response)?,
        need_bytes: status["needBytes"].as_u64().ok_or_else(invalid_response)?,
        local_changed_items: status["receiveOnlyTotalItems"].as_u64().unwrap_or(0),
        errors: Vec::new(),
        servers: Vec::new(),
    };
    if let Some(list) = errors["errors"].as_array() {
        for e in list.iter().take(100) {
            result.errors.push(FolderError {
                path: c.redact_text(e["path"].as_str().unwrap_or("")),
                reason: c.redact_text(e["error"].as_str().unwrap_or("")),
            });
        }
    }
    for (server_id, device_id) in servers {
        let connected = connections["connections"][device_id]["connected"]
            .as_bool()
            .unwrap_or(false);
        let (completion, need_items, need_deletes, error) =
            match c.db_completion(folder_id, device_id) {
                Ok(v) => match v["completion"].as_f64() {
                    Some(n) => (
                        Some(n),
                        v["needItems"].as_u64(),
                        v["needDeletes"].as_u64(),
                        None,
                    ),
                    None => (None, None, None, Some(invalid_response())),
                },
                Err(e) => (None, None, None, Some(e)),
            };
        result.servers.push(ServerStatus {
            server_id: server_id.clone(),
            connected,
            completion,
            need_items,
            need_deletes,
            error,
        });
    }
    Ok(result)
}

#[cfg(test)]
mod completion_tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    #[test]
    fn 本机扫描和完成度待处理计数保留未知值() {
        const FOLDER: &str = "pl-1234567890123456";
        const DEVICE: &str = "AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA";
        let mode = Arc::new(AtomicUsize::new(0));
        let flag = mode.clone();
        let scans = Arc::new(AtomicUsize::new(0));
        let scanned = scans.clone();
        let mut client = Client::new("http://127.0.0.1:1", "测试凭据").unwrap();
        client.test_handler = Some(Arc::new(move |method, path, query, _| {
            Ok(match path {
                "/rest/db/scan" => {
                    assert_eq!(method, "POST");
                    assert_eq!(query, &[("folder", FOLDER)]);
                    scanned.fetch_add(1, Ordering::Relaxed);
                    Value::Null
                }
                "/rest/db/status" => {
                    json!({"state":"idle","globalFiles":1,"globalBytes":1,"needBytes":0})
                }
                "/rest/system/connections" => json!({"connections":{DEVICE:{"connected":true}}}),
                "/rest/folder/errors" => json!({"errors":[]}),
                "/rest/db/completion" => match flag.load(Ordering::Relaxed) {
                    0 => json!({"completion":100,"needItems":0,"needDeletes":2}),
                    1 => json!({"completion":100}),
                    _ => return Err(AppError::new("syncthing.busy")),
                },
                _ if path.starts_with("/rest/config/folders/") => json!({"paused":false}),
                _ => panic!("未预期接口：{path}"),
            })
        }));
        let local = LocalSync {
            instance: None,
            client,
            id: DEVICE.into(),
            address_updates: Default::default(),
        };
        local.rescan(FOLDER, Duration::from_secs(90)).unwrap();
        assert_eq!(scans.load(Ordering::Relaxed), 1);
        assert_eq!(
            local.rescan(FOLDER, Duration::ZERO).unwrap_err().code,
            "syncthing.timeout"
        );
        for value in 0..3 {
            mode.store(value, Ordering::Relaxed);
            let status = task_status(&local, FOLDER, &[("server".into(), DEVICE.into())]).unwrap();
            assert_eq!(status.servers[0].need_items, (value == 0).then_some(0));
            assert_eq!(status.servers[0].need_deletes, (value == 0).then_some(2));
            assert_eq!(status.servers[0].completion, (value != 2).then_some(100.0));
        }
    }
}
