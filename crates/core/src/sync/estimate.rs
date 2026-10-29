//! 通过不共享的临时索引预估。使用已有目录作为标记，避免在远端遗留 .stfolder。
use super::{
    LocalSync, ServerLink, error,
    estimate_files::{EstimateFileAccess, IgnoreFile},
    invalid_response,
    time::parse_ms,
};
use crate::{
    error::AppError,
    syncthing::{client::Client, folder::validate_ignores},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
static ESTIMATE_LOCK: Mutex<()> = Mutex::new(());
#[derive(Debug, Clone)]
pub struct EstimateSpec {
    /// 本机应用数据目录，清理失败时存放恢复副本（保留 30 天）。
    pub data_dir: PathBuf,
    pub local_path: PathBuf,
    pub remote_path: String,
    pub ignore_rules: Vec<String>,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct EstimateProgress {
    pub scanned_local: u64,
    pub scanned_remote: u64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EstimateAmount {
    pub files: u64,
    pub bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EstimateConflict {
    pub path: String,
    pub local_mtime_ms: i64,
    pub remote_mtime_ms: i64,
    pub newer: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DirectorySummary {
    pub path: String,
    pub files: u64,
    pub bytes: u64,
    pub reason: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unreadable {
    pub side: String,
    pub path: String,
    pub reason: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Estimate {
    pub upload: EstimateAmount,
    pub download: EstimateAmount,
    pub same: EstimateAmount,
    pub conflict: EstimateAmount,
    pub local_total: EstimateAmount,
    pub remote_total: EstimateAmount,
    pub local_sync: EstimateAmount,
    pub remote_sync: EstimateAmount,
    pub conflicts: Vec<EstimateConflict>,
    pub syncing: Vec<DirectorySummary>,
    pub excluded: Vec<DirectorySummary>,
    pub scanned: EstimateProgress,
    pub unreadable: Vec<Unreadable>,
    /// 清理时发现外部修改而保留的路径（不含文件内容）。
    #[serde(default)]
    pub preserved: Vec<String>,
    /// 非法规则的零基序号；这些规则不参与本次预估。
    #[serde(default)]
    pub invalid_rules: Vec<usize>,
    /// 保留外部修改时保存的原始规则副本。
    #[serde(default)]
    pub recovery_paths: Vec<String>,
}
#[derive(Clone)]
struct File {
    size: u64,
    mtime: i64,
}
type Tree = BTreeMap<String, File>;
struct Scan<'a> {
    cancel: &'a AtomicBool,
    progress: &'a dyn Fn(EstimateProgress),
    counts: EstimateProgress,
}
impl Scan<'_> {
    fn check(&self) -> Result<(), AppError> {
        (self.progress)(self.counts);
        if self.cancel.load(Ordering::Relaxed) {
            Err(AppError::new("sync.cancelled"))
        } else {
            Ok(())
        }
    }
    fn call(
        &self,
        c: &Client,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Value, AppError> {
        self.check()?;
        // 配置写入必须等到请求结束，避免取消后写入与清理竞争。
        if method != "GET" && path != "/rest/db/scan" {
            return c.request(method, path, query, body, Duration::from_secs(5));
        }
        let client = c.clone();
        let method = method.to_owned();
        let path = path.to_owned();
        let query: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        thread::Builder::new()
            .name("同步预估请求".into())
            .spawn(move || {
                let q: Vec<_> = query
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                let _ = tx.send(client.request(&method, &path, &q, body, Duration::from_secs(5)));
            })
            .map_err(|_| error("无法创建预估请求线程"))?;
        loop {
            self.check()?;
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return Err(invalid_response()),
            }
        }
    }
}
struct Temporary<'a> {
    client: &'a Client,
    id: String,
    attempted: bool,
    preserved: bool,
    ignores: Option<IgnoreFile<'a>>,
    marker: super::markers::Marker<'a>,
    recovery_path: Option<String>,
}
impl Temporary<'_> {
    fn config_path(&self) -> String {
        format!("/rest/config/folders/{}", self.id)
    }
    fn cleanup(&mut self) -> Result<(), AppError> {
        if !self.attempted {
            return Ok(());
        }
        self.attempted = false;
        let deletion = delete_confirmed(self.client, &self.id);
        let marker_result = match deletion {
            Ok(()) => self.marker.restore(),
            Err(e) => Err(e),
        };
        if let Some(ignores) = &mut self.ignores {
            match ignores.restore() {
                Ok(()) => {}
                Err(e) if e.code == "sync.estimate_file_changed" => {
                    self.preserved = true;
                    self.recovery_path = e.params.get("recovery_path").cloned();
                }
                Err(e) => return Err(e.with("folder_id", &self.id)),
            }
        }
        marker_result.map_err(|e| e.with("folder_id", &self.id))
    }

    fn prepare(&mut self, path: &str, scan: &Scan<'_>) -> Result<(), AppError> {
        let folders = scan.call(self.client, "GET", "/rest/config/folders", &[], None)?;
        for f in folders.as_array().ok_or_else(invalid_response)? {
            if crate::storage::paths_overlap(path, f["path"].as_str().ok_or_else(invalid_response)?)
            {
                return Err(AppError::new("sync.path_overlap"));
            }
        }
        self.attempted = true;
        scan.call(self.client,"PUT",&self.config_path(),&[],Some(json!({"id":self.id,"path":path,"type":"sendonly","autoNormalize":false,"devices":[],"paused":true,"markerName":".","fsWatcherEnabled":false,"rescanIntervalS":0,"maxConflicts":-1})))?;
        Ok(())
    }
    fn tree(
        &mut self,
        rules: &[String],
        side: &str,
        scan: &mut Scan<'_>,
        unreadable: &mut Vec<Unreadable>,
    ) -> Result<Tree, AppError> {
        scan.check()?;
        self.ignores
            .as_mut()
            .ok_or_else(invalid_response)?
            .write_checked(rules, &|| scan.check())?;
        scan.check()?;
        let before = scan
            .call(
                self.client,
                "GET",
                "/rest/db/status",
                &[("folder", &self.id)],
                None,
            )
            .ok()
            .and_then(|v| v["stateChanged"].as_str().map(String::from));
        scan.call(
            self.client,
            "PATCH",
            &self.config_path(),
            &[],
            Some(json!({"paused":false})),
        )?;
        // scan 是同步接口；超时后继续查询状态，仍可取消，不能无限等待。
        let completed = match scan.call(
            self.client,
            "POST",
            "/rest/db/scan",
            &[("folder", &self.id)],
            None,
        ) {
            Err(e) if e.code == "syncthing.unreachable" => false,
            Err(e) => return Err(e),
            Ok(_) => true,
        };
        let mut changed = false;
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            let status = scan.call(
                self.client,
                "GET",
                "/rest/db/status",
                &[("folder", &self.id)],
                None,
            )?;
            let count = status["localFiles"].as_u64().unwrap_or(0);
            if side == "local" {
                scan.counts.scanned_local = count;
            } else {
                scan.counts.scanned_remote = count;
            }
            changed |= status["state"] == "scanning"
                || status["stateChanged"]
                    .as_str()
                    .is_some_and(|v| Some(v) != before.as_deref());
            if status["state"] == "idle" && (completed || changed) {
                break;
            }
            if status["state"] == "error" {
                return Err(AppError::new("sync.scan_failed").with("side", side));
            }
            if Instant::now() >= deadline {
                return Err(AppError::new("sync.scan_timeout"));
            }
            thread::sleep(Duration::from_millis(50));
        }
        let errors = scan.call(
            self.client,
            "GET",
            "/rest/folder/errors",
            &[("folder", &self.id)],
            None,
        )?;
        if let Some(list) = errors["errors"].as_array() {
            for e in list {
                let path = e["path"].as_str().ok_or_else(invalid_response)?;
                if !unreadable.iter().any(|u| u.side == side && u.path == path) {
                    unreadable.push(Unreadable {
                        side: side.into(),
                        path: path.into(),
                        reason: "扫描无法读取此路径".into(),
                    });
                }
            }
        }
        let mut tree = Tree::new();
        let mut dirs = vec![String::new()];
        while let Some(prefix) = dirs.pop() {
            let v = scan.call(
                self.client,
                "GET",
                "/rest/db/browse",
                &[("folder", &self.id), ("prefix", &prefix), ("levels", "0")],
                None,
            )?;
            browse(&v, &prefix, &mut tree, &mut dirs)?;
        }
        if side == "local" {
            scan.counts.scanned_local = tree.len() as u64;
        } else {
            scan.counts.scanned_remote = tree.len() as u64;
        }
        Ok(tree)
    }
}
impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        if self.attempted {
            let _ = self.cleanup();
        }
    }
}
fn browse(
    value: &Value,
    prefix: &str,
    tree: &mut Tree,
    dirs: &mut Vec<String>,
) -> Result<(), AppError> {
    for v in value.as_array().ok_or_else(invalid_response)? {
        let name = v["name"].as_str().ok_or_else(invalid_response)?;
        if name.contains('/') || name.contains('\\') || matches!(name, "." | "..") {
            return Err(invalid_response());
        }
        let path = if prefix.is_empty() {
            name.into()
        } else {
            format!("{prefix}/{name}")
        };
        match v["type"].as_str() {
            Some("FILE" | "FILE_INFO_TYPE_FILE") => {
                tree.insert(
                    path,
                    File {
                        size: v["size"].as_u64().ok_or_else(invalid_response)?,
                        mtime: parse_ms(v["modTime"].as_str().ok_or_else(invalid_response)?)?,
                    },
                );
            }
            Some("DIRECTORY" | "FILE_INFO_TYPE_DIRECTORY") => dirs.push(path),
            Some(
                "SYMLINK"
                | "SYMLINK_FILE"
                | "SYMLINK_DIRECTORY"
                | "FILE_INFO_TYPE_SYMLINK"
                | "FILE_INFO_TYPE_SYMLINK_FILE"
                | "FILE_INFO_TYPE_SYMLINK_DIRECTORY",
            ) => {}
            _ => return Err(invalid_response()),
        }
    }
    Ok(())
}
fn amount(tree: &Tree) -> EstimateAmount {
    EstimateAmount {
        files: tree.len() as u64,
        bytes: tree.values().map(|f| f.size).sum(),
    }
}
fn add(a: &mut EstimateAmount, size: u64) {
    a.files += 1;
    a.bytes += size;
}
fn classify(
    local: &Tree,
    remote: &Tree,
    mut equal: impl FnMut(&str) -> Result<bool, AppError>,
    result: &mut Estimate,
    checkpoint: impl Fn() -> Result<(), AppError>,
) -> Result<(), AppError> {
    for (path, a) in local {
        checkpoint()?;
        match remote.get(path) {
            None => add(&mut result.upload, a.size),
            Some(b) if (a.size == b.size && a.mtime == b.mtime) || equal(path)? => {
                add(&mut result.same, a.size)
            }
            Some(b) => {
                add(&mut result.conflict, a.size.saturating_add(b.size));
                if result.conflicts.len() < 200 {
                    result.conflicts.push(EstimateConflict {
                        path: path.clone(),
                        local_mtime_ms: a.mtime,
                        remote_mtime_ms: b.mtime,
                        newer: if a.mtime > b.mtime {
                            "local"
                        } else if a.mtime < b.mtime {
                            "remote"
                        } else {
                            "same"
                        }
                        .into(),
                    });
                }
            }
        }
    }
    for (path, b) in remote {
        checkpoint()?;
        if !local.contains_key(path) {
            add(&mut result.download, b.size);
        }
    }
    Ok(())
}
fn summaries(
    all: &Tree,
    filtered: &Tree,
    rules: &[String],
    syncing: &mut BTreeMap<String, DirectorySummary>,
    excluded: &mut BTreeMap<String, DirectorySummary>,
    checkpoint: impl Fn() -> Result<(), AppError>,
) -> Result<(), AppError> {
    let compiled = super::conflicts::compile_rules(rules).0;
    for (path, f) in all {
        checkpoint()?;
        let is_sync = filtered.contains_key(path);
        let name = path.split('/').next().unwrap_or(path);
        let target = if is_sync {
            &mut *syncing
        } else {
            &mut *excluded
        };
        let entry = target
            .entry(name.into())
            .or_insert_with(|| DirectorySummary {
                path: name.into(),
                ..Default::default()
            });
        entry.files += 1;
        entry.bytes += f.size;
        if !is_sync {
            let rule = compiled
                .ignored(path)
                .and_then(|i| rules.get(i))
                .map(String::as_str)
                .unwrap_or("");
            entry.reason = Some(exclusion_reason(rule).into());
        }
    }
    Ok(())
}
fn exclusion_reason(rule: &str) -> &'static str {
    let rule = rule
        .trim()
        .trim_start_matches("(?d)")
        .trim_start_matches("(?i)")
        .trim_start_matches('/');
    match rule {
        "node_modules" | ".venv" => "依赖目录",
        ".git" => "版本库",
        ".env" | "*.pem" | "*.key" => "敏感文件",
        "__pycache__" | "target" | "dist" | "build" | ".cache" | "*.tmp" | "Thumbs.db"
        | "desktop.ini" | "~$*" => "缓存",
        _ => "自定义规则",
    }
}
pub(super) fn delete_confirmed(client: &Client, id: &str) -> Result<(), AppError> {
    let path = format!("/rest/config/folders/{id}");
    client
        .request("DELETE", &path, &[], None, Duration::from_secs(15))
        .map_err(|e| e.with("folder_id", id))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match client.request(
            "GET",
            &path,
            &[],
            None,
            deadline.saturating_duration_since(Instant::now()),
        ) {
            Err(e)
                if e.code == "syncthing.http_error"
                    && e.params.get("status").is_some_and(|s| s == "404") =>
            {
                return Ok(());
            }
            Err(e) => return Err(e.with("folder_id", id)),
            Ok(_) if Instant::now() >= deadline => {
                return Err(AppError::new("sync.cleanup_failed")
                    .with("folder_id", id)
                    .with("reason", "删除后配置仍存在"));
            }
            Ok(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}
/// 删除 client 中进程崩溃遗留的 pl-est- 临时配置，返回删除数量；连接建立时由调用方调用。
/// 无原始目录快照时不尝试修改标记或排除文件。
pub fn cleanup_stale_estimates(client: &Client) -> Result<usize, AppError> {
    let folders = client.list_folders()?;
    let mut count = 0;
    for folder in folders.as_array().ok_or_else(invalid_response)? {
        let id = folder["id"].as_str().ok_or_else(invalid_response)?;
        if id.starts_with("pl-est-") && crate::syncthing::folder::validate_folder_id(id).is_ok() {
            delete_confirmed(client, id)?;
            count += 1;
        }
    }
    Ok(count)
}
fn acquire_lock(lock: &Mutex<()>) -> Result<std::sync::MutexGuard<'_, ()>, AppError> {
    lock.try_lock().or_else(|e| match e {
        std::sync::TryLockError::Poisoned(e) => Ok(e.into_inner()),
        std::sync::TryLockError::WouldBlock => Err(AppError::new("sync.estimate_busy")),
    })
}
/// 用两端临时不共享文件夹扫描 spec 并比较内容；progress 报告数量，cancel 停止扫描请求等待并在文件脚本之间检查。
/// 两端配置在成功、失败、取消后均清理（清理不受取消约束）；失败返回临时 ID 和可用的恢复副本路径。
/// 不创建 .stfolder；按原始字节恢复 .stignore，外部修改保留并标注。冲突字节数为两方之和。
/// link 须通过 set_estimate_file_access 指定远端文件访问，否则在修改前返回错误。
pub fn estimate(
    local: &LocalSync,
    link: &ServerLink,
    spec: &EstimateSpec,
    progress: &dyn Fn(EstimateProgress),
    cancel: &AtomicBool,
) -> Result<Estimate, AppError> {
    let _lock = acquire_lock(&ESTIMATE_LOCK)?;
    validate_ignores(&spec.ignore_rules)?;
    let path = super::paths::local_normalized(&spec.local_path)?;
    let path = path.to_str().ok_or_else(|| error("路径不是 UTF-8"))?;
    let remote_path = super::paths::remote_normalized(&spec.remote_path)?;
    let remote_files = link
        .estimate_files
        .as_ref()
        .ok_or_else(|| AppError::new("sync.estimate_transport_missing"))?;
    let local_files = EstimateFileAccess::Local;
    let mut scan = Scan {
        cancel,
        progress,
        counts: EstimateProgress::default(),
    };
    scan.check()?;
    let local_ignore = IgnoreFile::new(&local_files, path)?;
    scan.check()?;
    let mut a = Temporary {
        client: local.client(),
        id: format!("pl-est-{:016x}", rand::random::<u64>()),
        attempted: false,
        preserved: false,
        recovery_path: None,
        ignores: Some(local_ignore),
        marker: super::markers::Marker::new_checked(&local_files, path, &|| scan.check())?,
    };
    scan.check()?;
    let remote_ignore = IgnoreFile::new(remote_files, &remote_path)?;
    scan.check()?;
    let mut b = Temporary {
        client: link.client(),
        id: format!("pl-est-{:016x}", rand::random::<u64>()),
        attempted: false,
        preserved: false,
        recovery_path: None,
        ignores: Some(remote_ignore),
        marker: super::markers::Marker::new_checked(remote_files, &remote_path, &|| scan.check())?,
    };
    let (_, invalid_rules) = super::conflicts::compile_rules(&spec.ignore_rules);
    let rules: Vec<_> = spec
        .ignore_rules
        .iter()
        .enumerate()
        .filter(|(i, _)| !invalid_rules.contains(i))
        .map(|(_, r)| r.clone())
        .collect();
    let mut partial = Estimate {
        invalid_rules,
        ..Default::default()
    };
    if let Some(ignore) = a.ignores.take() {
        a.ignores = Some(ignore.with_recovery(&spec.data_dir, &a.id));
    }
    if let Some(ignore) = b.ignores.take() {
        b.ignores = Some(ignore.with_recovery(&spec.data_dir, &b.id));
    }
    let mut result: Result<Estimate, AppError> = (|| {
        scan.check()?;
        a.prepare(path, &scan)?;
        b.prepare(&remote_path, &scan)?;
        let result = &mut partial;
        let all_a = a.tree(&[], "local", &mut scan, &mut result.unreadable)?;
        result.local_total = amount(&all_a);
        let all_b = b.tree(&[], "remote", &mut scan, &mut result.unreadable)?;
        result.remote_total = amount(&all_b);
        let tree_a = a.tree(&rules, "local", &mut scan, &mut result.unreadable)?;
        let tree_b = b.tree(&rules, "remote", &mut scan, &mut result.unreadable)?;
        result.local_total = amount(&all_a);
        result.remote_total = amount(&all_b);
        result.local_sync = amount(&tree_a);
        result.remote_sync = amount(&tree_b);
        classify(
            &tree_a,
            &tree_b,
            |path| {
                let x = scan.call(
                    a.client,
                    "GET",
                    "/rest/db/file",
                    &[("folder", &a.id), ("file", path)],
                    None,
                )?;
                let y = scan.call(
                    b.client,
                    "GET",
                    "/rest/db/file",
                    &[("folder", &b.id), ("file", path)],
                    None,
                )?;
                let x = x["local"]["blocksHash"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(invalid_response)?;
                let y = y["local"]["blocksHash"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(invalid_response)?;
                Ok(x == y)
            },
            result,
            || scan.check(),
        )?;
        let mut syncing = BTreeMap::new();
        let mut excluded = BTreeMap::new();
        summaries(&all_a, &tree_a, &rules, &mut syncing, &mut excluded, || {
            scan.check()
        })?;
        summaries(&all_b, &tree_b, &rules, &mut syncing, &mut excluded, || {
            scan.check()
        })?;
        result.syncing = syncing.into_values().take(50).collect();
        result.excluded = excluded.into_values().take(50).collect();
        result.scanned = EstimateProgress {
            scanned_local: all_a.len() as u64,
            scanned_remote: all_b.len() as u64,
        };
        scan.check()?;
        Ok(result.clone())
    })();
    let (ca, cb) = thread::scope(|s| {
        let a = s.spawn(|| a.cleanup());
        let b = s.spawn(|| b.cleanup());
        (a.join(), b.join())
    });
    let mut failures = Vec::new();
    let mut failed_paths = Vec::new();
    let mut recovery_paths = Vec::new();
    for c in [ca, cb] {
        match c {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if let Some(path) = e.params.get("recovery_path") {
                    recovery_paths.push(path.clone());
                }
                if let Some(path) = e.params.get("path") {
                    failed_paths.push(path.clone());
                }
                failures.push(format!(
                    "{}:{}",
                    e.params.get("folder_id").cloned().unwrap_or_default(),
                    e.params.get("cause").cloned().unwrap_or(e.code)
                ));
            }
            Err(_) => failures.push("清理线程异常".into()),
        }
    }
    for path in [&a.recovery_path, &b.recovery_path].into_iter().flatten() {
        partial.recovery_paths.push(path.clone());
        if let Ok(r) = &mut result {
            r.recovery_paths.push(path.clone());
        }
    }
    for (side, access, directory, preserved) in [
        ("local", &local_files, path, a.preserved),
        ("remote", remote_files, remote_path.as_str(), b.preserved),
    ] {
        if preserved {
            let path = format!("{side}:{}", access.join(directory, ".stignore"));
            partial.preserved.push(path.clone());
            if let Ok(r) = &mut result {
                r.preserved.push(path);
            }
        }
    }
    if !failures.is_empty() {
        result = Err(AppError::new("sync.cleanup_failed")
            .with("folders", failures.join(","))
            .with("path", failed_paths.join(","))
            .with("recovery_path", recovery_paths.join(","))
            .with(
                "cause",
                result
                    .as_ref()
                    .err()
                    .map(|e| e.code.as_str())
                    .unwrap_or("清理失败"),
            ));
    }
    result.map_err(|e| {
        partial.scanned = scan.counts;
        e.with(
            "partial_estimate",
            serde_json::to_string(&partial).unwrap_or_default(),
        )
        .with("scanned_local", scan.counts.scanned_local)
        .with("scanned_remote", scan.counts.scanned_remote)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 预估四类哈希及明细上限() {
        let mut a = Tree::new();
        let mut b = Tree::new();
        a.insert("upload".into(), File { size: 1, mtime: 0 });
        b.insert("download".into(), File { size: 2, mtime: 0 });
        for name in ["same", "hash", "conflict"] {
            a.insert(name.into(), File { size: 3, mtime: 0 });
            b.insert(
                name.into(),
                File {
                    size: 3,
                    mtime: if name == "same" { 0 } else { 1000 },
                },
            );
        }
        let mut r = Estimate::default();
        classify(&a, &b, |p| Ok(p == "hash"), &mut r, || Ok(())).unwrap();
        assert_eq!(
            (
                r.upload.files,
                r.download.files,
                r.same.files,
                r.conflict.files
            ),
            (1, 1, 2, 1)
        );
        assert_eq!(r.conflicts[0].newer, "remote");
        for n in 0..250 {
            a.insert(n.to_string(), File { size: 1, mtime: 0 });
            b.insert(n.to_string(), File { size: 2, mtime: 1 });
        }
        let mut r = Estimate::default();
        classify(&a, &b, |_| Ok(false), &mut r, || Ok(())).unwrap();
        assert_eq!(r.conflicts.len(), 200);
    }
    #[test]
    fn y21_扫描取消与失败后清理() {
        let mut client = Client::new("http://127.0.0.1:1", "secret").unwrap();
        let deleted = std::sync::Arc::new(AtomicBool::new(false));
        let flag = deleted.clone();
        client.test_handler = Some(std::sync::Arc::new(move |method, path, _, _| {
            if method == "DELETE" {
                flag.store(true, Ordering::Relaxed);
                return Ok(Value::Null);
            }
            if path == "/rest/config/folders" {
                return Ok(json!([]));
            }
            if method == "PUT" {
                return Err(AppError::new("syncthing.unreachable"));
            }
            Err(AppError::new("syncthing.http_error").with("status", 404))
        }));
        let cancel = AtomicBool::new(false);
        let scan = Scan {
            cancel: &cancel,
            progress: &|_| {},
            counts: Default::default(),
        };
        {
            let mut temp = Temporary {
                client: &client,
                id: "pl-est-1234567890123456".into(),
                attempted: false,
                preserved: false,
                recovery_path: None,
                ignores: None,
                marker: super::super::markers::Marker::new(&EstimateFileAccess::Local, "/work")
                    .unwrap(),
            };
            assert!(temp.prepare("/work", &scan).is_err());
        }
        assert!(deleted.load(Ordering::Relaxed));
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(scan.check().unwrap_err().code, "sync.cancelled");
    }
    #[test]
    fn 排除原因与目录汇总() {
        let a = Tree::from([
            ("node_modules/a".into(), File { size: 3, mtime: 0 }),
            ("src/a".into(), File { size: 2, mtime: 0 }),
        ]);
        let b = Tree::from([("src/a".into(), File { size: 2, mtime: 0 })]);
        let mut included = BTreeMap::new();
        let mut excluded = BTreeMap::new();
        summaries(
            &a,
            &b,
            &["(?d)node_modules".into()],
            &mut included,
            &mut excluded,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(excluded["node_modules"].reason.as_deref(), Some("依赖目录"));
        assert_eq!(included["src"].files, 1);
    }
}

// 两端共用本机文件系统模拟远端，目录必须同时是本机绝对路径和 POSIX 绝对路径。
#[cfg(all(test, unix))]
mod lifecycle_tests {
    use super::*;
    use crate::sync::ServerEndpointInfo;
    use std::sync::{Arc, Mutex};
    const A: &str = "AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA";
    const B: &str = "BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB";
    #[derive(Default)]
    struct State {
        folder: Option<Value>,
        browses: usize,
        deletes: usize,
        fail_browse: bool,
        fail_delete: bool,
    }
    fn client(id: &'static str, state: Arc<Mutex<State>>) -> Client {
        let mut c = Client::new("http://127.0.0.1:1", "secret").unwrap();
        c.test_handler = Some(Arc::new(move |method, path, _, body| {
            let mut s = state.lock().unwrap();
            match (method, path) {
                ("GET", "/rest/system/status") => Ok(json!({"myID":id})),
                ("GET", "/rest/config/devices") => Ok(json!([])),
                ("PUT", p) if p.starts_with("/rest/config/devices/") => Ok(Value::Null),
                ("GET", "/rest/config/folders") => Ok(json!([])),
                ("PUT", p) if p.starts_with("/rest/config/folders/") => {
                    let b = body.unwrap();
                    assert!(b["devices"].as_array().unwrap().is_empty());
                    assert_eq!(b["markerName"], ".");
                    assert_eq!(b["paused"], true);
                    s.folder = Some(b);
                    Ok(Value::Null)
                }
                ("DELETE", p) if p.starts_with("/rest/config/folders/") => {
                    if s.fail_delete {
                        return Err(AppError::new("syncthing.unreachable"));
                    }
                    if let Some(folder) = &s.folder {
                        let directory = std::path::Path::new(folder["path"].as_str().unwrap());
                        let marker = directory.join(".stfolder");
                        if marker.exists() {
                            if std::fs::read_dir(&marker).unwrap().next().is_none() {
                                std::fs::remove_dir(&marker).unwrap();
                            } else {
                                std::fs::rename(
                                    marker,
                                    directory.join(".stfolder.removed-20261022-203904"),
                                )
                                .unwrap();
                            }
                        }
                    }
                    s.folder = None;
                    s.deletes += 1;
                    Ok(Value::Null)
                }
                ("GET", p) if p.starts_with("/rest/config/folders/") => s
                    .folder
                    .clone()
                    .ok_or_else(|| AppError::new("syncthing.http_error").with("status", 404)),
                ("PATCH", p) if p.starts_with("/rest/config/folders/") => Ok(Value::Null),
                ("POST", "/rest/db/scan") => Ok(Value::Null),
                ("GET", "/rest/db/status") => Ok(json!({"state":"idle","localFiles":1})),
                ("GET", "/rest/folder/errors") => {
                    Ok(json!({"errors":[{"path":"private","error":"permission denied"}]}))
                }
                ("GET", "/rest/db/browse") => {
                    s.browses += 1;
                    if s.fail_browse && s.browses == 2 {
                        Err(AppError::new("syncthing.unreachable"))
                    } else {
                        Ok(
                            json!([{"name":"a","type":"FILE_INFO_TYPE_FILE","size":1,"modTime":"2026-10-22T00:00:00Z"}]),
                        )
                    }
                }
                _ => panic!("未预期接口 {method} {path}"),
            }
        }));
        c
    }
    #[test]
    fn y21_完整预估无权限断线取消均清理两端() {
        for mode in [
            "成功",
            "空标记",
            "非法规则",
            "断线",
            "取消",
            "后期取消",
            "外部修改",
            "外部修改取消",
            "清理失败",
            "外部修改清理失败",
            "未配置",
        ] {
            let temp = crate::test_support::tempdir();
            std::fs::create_dir(temp.path().join("remote")).unwrap();
            let sa = Arc::new(Mutex::new(State::default()));
            let sb = Arc::new(Mutex::new(State {
                fail_browse: mode == "断线",
                fail_delete: mode.ends_with("清理失败"),
                ..Default::default()
            }));
            let local = LocalSync {
                address_updates: Default::default(),
                instance: None,
                client: client(A, sa.clone()),
                id: A.into(),
            };
            let remote = client(B, sb.clone());
            let info = ServerEndpointInfo {
                server_id: "b".into(),
                name: "b".into(),
                device_id: B.into(),
                socket_path: "/unused".into(),
                sync_port: 1,
                api_key: "secret".into(),
                state_dir: "/state".into(),
            };
            let mut link =
                ServerLink::from_parts(remote, "tcp://127.0.0.1:1".into(), &info, &local, "a")
                    .unwrap();
            if mode != "未配置" {
                link.set_estimate_file_access(EstimateFileAccess::Local);
            }
            let original = "// 原始规则\r\n\r\nexisting".as_bytes();
            std::fs::write(temp.path().join(".stignore"), original).unwrap();
            for dir in [temp.path().to_path_buf(), temp.path().join("remote")] {
                std::fs::create_dir(dir.join(".stfolder")).unwrap();
                if mode != "空标记" {
                    std::fs::write(dir.join(".stfolder/user-marker"), "保留标记").unwrap();
                }
            }
            let before_local = super::super::tests::directory_snapshot(temp.path());
            let recovery = crate::test_support::tempdir();
            let spec = EstimateSpec {
                data_dir: recovery.path().into(),
                local_path: temp.path().into(),
                remote_path: temp.path().join("remote").display().to_string(),
                ignore_rules: if mode == "非法规则" {
                    vec!["[".into(), "excluded".into()]
                } else {
                    vec!["excluded".into()]
                },
            };
            let cancel = AtomicBool::new(false);
            let modified = AtomicBool::new(false);
            let result = estimate(
                &local,
                &link,
                &spec,
                &|p| {
                    if mode == "取消" && p.scanned_local > 0 {
                        cancel.store(true, Ordering::Relaxed);
                    }
                    if sb.lock().unwrap().browses >= 2 {
                        if mode.starts_with("外部修改") && !modified.swap(true, Ordering::Relaxed)
                        {
                            std::fs::write(temp.path().join("remote/.stignore"), "external")
                                .unwrap();
                        }
                        if mode.ends_with("取消") {
                            cancel.store(true, Ordering::Relaxed);
                        }
                    }
                },
                &cancel,
            );
            if matches!(mode, "成功" | "空标记" | "非法规则" | "外部修改") {
                let r = result.unwrap();
                assert_eq!(
                    r.invalid_rules,
                    if mode == "非法规则" {
                        vec![0]
                    } else {
                        vec![]
                    }
                );
                assert_eq!(r.same.files, 1);
                assert_eq!(r.unreadable.len(), 2);
                assert_eq!(r.preserved.len(), usize::from(mode == "外部修改"));
            } else {
                let error = result.unwrap_err();
                if mode != "未配置" {
                    let partial: Estimate =
                        serde_json::from_str(&error.params["partial_estimate"]).unwrap();
                    assert_eq!(
                        partial.preserved.len(),
                        usize::from(mode.starts_with("外部修改"))
                    );
                }
                assert_eq!(
                    error.code,
                    if mode.ends_with("取消") {
                        "sync.cancelled"
                    } else if mode.ends_with("清理失败") {
                        "sync.cleanup_failed"
                    } else if mode == "未配置" {
                        "sync.estimate_transport_missing"
                    } else {
                        "syncthing.unreachable"
                    }
                );
            }
            let mut expected = before_local;
            if mode.starts_with("外部修改") {
                expected.insert(
                    std::path::PathBuf::from("remote/.stignore"),
                    Some(b"external".to_vec()),
                );
            }
            assert_eq!(
                super::super::tests::directory_snapshot(temp.path()),
                expected
            );
            for state in [&sa, &sb] {
                let s = state.lock().unwrap();
                assert_eq!(s.folder.is_none(), mode == "未配置" || !s.fail_delete);
                assert_eq!(s.deletes, usize::from(mode != "未配置" && !s.fail_delete));
            }
        }
    }
}

#[cfg(test)]
mod revision_tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicUsize};
    #[test]
    fn 慢请求取消停止等待而正常慢请求完成() {
        let mut c = Client::new("http://127.0.0.1:1", "secret").unwrap();
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        c.test_handler = Some(Arc::new(move |_, _, _, _| {
            thread::sleep(Duration::from_millis(600));
            done.store(true, Ordering::Relaxed);
            Ok(json!([]))
        }));
        let cancel = AtomicBool::new(false);
        let scan = Scan {
            cancel: &cancel,
            progress: &|_| {},
            counts: Default::default(),
        };
        assert_eq!(
            scan.call(&c, "GET", "/rest/db/browse", &[], None).unwrap(),
            json!([])
        );
        finished.store(false, Ordering::Relaxed);
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(50));
                cancel.store(true, Ordering::Relaxed);
            });
            let start = Instant::now();
            assert_eq!(
                scan.call(&c, "GET", "/rest/db/browse", &[], None)
                    .unwrap_err()
                    .code,
                "sync.cancelled"
            );
            assert!(start.elapsed() < Duration::from_millis(400));
        });
        // 等到有界请求结束，确保测试退出时没有残留工作线程。
        while !finished.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn 崩溃清理只删除临时配置且等待删除确认() {
        let mut c = Client::new("http://127.0.0.1:1", "secret").unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let counter = reads.clone();
        c.test_handler = Some(Arc::new(move |method, path, _, _| {
            if path == "/rest/config/folders" {
                return Ok(json!([
                    {"id":"pl-est-1234567890123456"}, {"id":"pl-1234567890123456"}, {"id":"pl-est-../../bad"}
                ]));
            }
            assert_eq!(path, "/rest/config/folders/pl-est-1234567890123456");
            if method == "DELETE" {
                thread::sleep(Duration::from_millis(250));
                return Ok(Value::Null);
            }
            if counter.fetch_add(1, Ordering::Relaxed) == 0 {
                Ok(json!({"id":"pl-est-1234567890123456"}))
            } else {
                Err(AppError::new("syncthing.http_error").with("status", 404))
            }
        }));
        assert_eq!(cleanup_stale_estimates(&c).unwrap(), 1);
        assert_eq!(reads.load(Ordering::Relaxed), 2);
        assert!(
            crate::syncthing::folder::validate_task_folder_id("pl-est-1234567890123456").is_err()
        );
        assert_eq!(cleanup_stale_estimates(&c).unwrap(), 1);
    }
    #[test]
    fn 预估锁中毒仍可使用且占用仍返回忙() {
        let lock = Mutex::new(());
        let _ = std::panic::catch_unwind(|| {
            let _guard = lock.lock().unwrap();
            panic!("模拟中毒");
        });
        let guard = acquire_lock(&lock).unwrap();
        assert_eq!(acquire_lock(&lock).unwrap_err().code, "sync.estimate_busy");
        drop(guard);
        assert!(acquire_lock(&lock).is_ok());
    }
    #[test]
    fn 初次扫描暂停创建写规则后启用且旧空闲状态不能提前完成() {
        let root = crate::test_support::tempdir();
        std::fs::write(root.path().join(".stignore"), "old").unwrap();
        let path = root.path().to_str().unwrap().to_owned();
        let copy = path.clone();
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let mut c = Client::new("http://127.0.0.1:1", "secret").unwrap();
        c.test_handler = Some(Arc::new(move |method, route, _, body| {
            match (method, route) {
                ("GET", "/rest/config/folders") => Ok(json!([])),
                ("PUT", _) => {
                    assert_eq!(body.unwrap()["paused"], true);
                    Ok(Value::Null)
                }
                ("PATCH", _) => {
                    assert_eq!(
                        std::fs::read(std::path::Path::new(&copy).join(".stignore")).unwrap(),
                        b"new"
                    );
                    Ok(Value::Null)
                }
                ("POST", "/rest/db/scan") => Err(AppError::new("syncthing.unreachable")),
                ("GET", "/rest/db/status") => {
                    let n = count.fetch_add(1, Ordering::Relaxed);
                    Ok(json!({"state":"idle", "stateChanged": if n < 2 { "old" } else { "new" }}))
                }
                ("GET", "/rest/folder/errors") => Ok(json!({"errors":[]})),
                ("GET", "/rest/db/browse") => {
                    assert!(count.load(Ordering::Relaxed) >= 3);
                    Ok(json!([]))
                }
                ("DELETE", _) => Ok(Value::Null),
                ("GET", _) => Err(AppError::new("syncthing.http_error").with("status", 404)),
                _ => panic!("未预期请求"),
            }
        }));
        let access = EstimateFileAccess::Local;
        let mut temp = Temporary {
            client: &c,
            id: "pl-est-1234567890123456".into(),
            attempted: false,
            preserved: false,
            recovery_path: None,
            ignores: Some(IgnoreFile::new(&access, &path).unwrap()),
            marker: super::super::markers::Marker::new(&access, &path).unwrap(),
        };
        let cancel = AtomicBool::new(false);
        let mut scan = Scan {
            cancel: &cancel,
            progress: &|_| {},
            counts: Default::default(),
        };
        temp.prepare(&path, &scan).unwrap();
        temp.tree(&["new".into()], "local", &mut scan, &mut vec![])
            .unwrap();
        cancel.store(true, Ordering::Relaxed);
        temp.cleanup().unwrap();
        assert_eq!(
            std::fs::read(root.path().join(".stignore")).unwrap(),
            b"old"
        );
    }
}
