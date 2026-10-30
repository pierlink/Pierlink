//! 事件泵保留内存游标，规范化的事件由调用方存入数据库；重启通过最新事件探测识别。
use super::{LocalSync, invalid_response, time::parse_ms};
use crate::{
    error::AppError,
    syncthing::client::{Client, Event},
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

#[cfg(test)]
mod redial;
#[cfg(test)]
mod sources;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SyncEventKind {
    Downloaded,
    Uploaded,
    Deleted,
    Conflict,
    Error,
    DeviceConnected,
    DeviceDisconnected,
    Paused,
    StateChanged,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncEvent {
    pub folder_id: String,
    pub time_ms: i64,
    pub kind: SyncEventKind,
    pub device: Option<String>,
    /// 改动来源：本机为 local，远端为完整设备 ID，无法确认时为空。
    #[serde(default)]
    pub by: Option<String>,
    pub path: Option<String>,
    pub detail: Option<String>,
}
const EVENT_TYPES: &str = "LocalChangeDetected,RemoteChangeDetected,ItemFinished,LocalIndexUpdated,RemoteIndexUpdated,FolderErrors,DeviceConnected,DeviceDisconnected,FolderPaused,StateChanged";

pub struct EventPump {
    client: Client,
    since: u64,
    initialized: bool,
    last_time: Option<String>,
    start_time: Option<String>,
    pending: HashMap<(String, String), (SyncEvent, Instant, serde_json::Value)>,
    local_id: String,
}
impl EventPump {
    /// 从 local 创建内存游标；返回尚未轮询的事件泵。
    pub fn new(local: &LocalSync) -> Self {
        Self {
            client: local.client().clone(),
            since: 0,
            initialized: false,
            last_time: None,
            start_time: None,
            pending: HashMap::new(),
            local_id: local.device_id().into(),
        }
    }
    /// 长轮询本机 30 秒并整理事件；设备地址重拨不产生人为断连，真实断开和恢复照常保留。
    /// 失败保留游标供重试，实例重启后重新读取。
    pub fn poll(&mut self) -> Result<Vec<SyncEvent>, AppError> {
        let status = self.client.call("GET", "/rest/system/status", &[], None)?;
        let start = status["startTime"]
            .as_str()
            .ok_or_else(invalid_response)?
            .to_string();
        if self.start_time.as_ref().is_some_and(|old| old != &start) {
            self.since = 0;
            self.last_time = None;
            self.pending.clear();
        }
        self.start_time = Some(start);
        let latest = self.client.call(
            "GET",
            "/rest/events",
            &[
                ("since", "0"),
                ("limit", "1"),
                ("timeout", "0"),
                ("events", EVENT_TYPES),
            ],
            None,
        )?;
        let latest_values = latest.as_array().ok_or_else(invalid_response)?;
        let latest_id = latest_values
            .iter()
            .filter_map(|e| e["id"].as_u64())
            .max()
            .unwrap_or(0);
        let latest: Vec<Event> = latest_values
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        if let Some(e) = latest.last()
            && (latest_id < self.since
                || (latest_id == e.id
                    && e.id == self.since
                    && self.last_time.as_ref().is_some_and(|t| t != &e.time)))
        {
            self.since = 0;
            self.last_time = None;
            self.pending.clear();
        }
        if !self.initialized {
            self.since = latest_id;
            self.last_time = latest.last().map(|e| e.time.clone());
            self.initialized = true;
            return Ok(Vec::new());
        }
        let types = EVENT_TYPES.split(',').map(String::from).collect::<Vec<_>>();
        let batch = self.client.events(self.since, 30, &types)?;
        let mut pending = self.pending.clone();
        pending.retain(|_, (_, when, _)| when.elapsed() < Duration::from_secs(3600));
        let mut result = Vec::new();
        let mut versions = HashMap::new();
        let mut device_ids = None;
        let mut cursor = self.since;
        let mut time = self.last_time.clone();
        for e in batch.events.iter().filter(|e| e.id > self.since) {
            cursor = cursor.max(e.id);
            time = Some(e.time.clone());
            versions.clear();
            let mut normalized = match normalize(&self.client, e, &mut versions) {
                Ok(items) => items,
                Err(e)
                    if matches!(
                        e.code.as_str(),
                        "syncthing.unreachable" | "syncthing.unauthorized"
                    ) =>
                {
                    return Err(e);
                }
                Err(_) => {
                    eprintln!("跳过格式异常的同步事件，编号 {}", e.id);
                    continue;
                }
            };
            // 文件变化事件带有该次改动的来源；完成事件仅用于错误，避免重复或按发送端猜作者。
            if e.kind == "ItemFinished" {
                normalized.retain(|v| v.kind == SyncEventKind::Error);
            }
            if e.kind == "LocalIndexUpdated" {
                normalized.retain(|v| v.kind != SyncEventKind::Deleted);
            }
            for item in &mut normalized {
                if e.kind != "LocalChangeDetected"
                    && let Some(by) = item.by.take()
                {
                    item.by = resolve_source(&self.client, &self.local_id, &by, &mut device_ids);
                }
            }
            if e.kind == "LocalIndexUpdated" {
                for item in &normalized {
                    if item.kind == SyncEventKind::StateChanged
                        && let Some(path) = &item.path
                        && let Some(version) = versions.get(&(item.folder_id.clone(), path.clone()))
                    {
                        if pending.len() >= 10000 {
                            pending.clear();
                        }
                        pending.insert(
                            (item.folder_id.clone(), path.clone()),
                            (item.clone(), Instant::now(), version.clone()),
                        );
                    }
                }
            }
            if e.kind == "RemoteIndexUpdated" {
                let (Some(folder), Some(device)) =
                    (e.data["folder"].as_str(), e.data["device"].as_str())
                else {
                    eprintln!("跳过字段缺失的远端索引事件，编号 {}", e.id);
                    continue;
                };
                let mut finished = Vec::new();
                for ((f, path), (item, _, expected)) in &pending {
                    if f != folder {
                        continue;
                    }
                    let Some(info) = index_file(&self.client, f, path)? else {
                        continue;
                    };
                    let version = &info["local"]["version"];
                    if !version.is_null()
                        && version == expected
                        && version == &info["global"]["version"]
                        && info["availability"].as_array().is_some_and(|list| {
                            list.iter()
                                .any(|v| v["id"] == device && v["fromTemporary"] == false)
                        })
                    {
                        let mut uploaded = item.clone();
                        uploaded.kind = SyncEventKind::Uploaded;
                        uploaded.device = Some(device.into());
                        uploaded.time_ms = parse_ms(&e.time)?;
                        result.push(uploaded);
                        finished.push((f.clone(), path.clone()));
                    }
                }
                for key in finished {
                    pending.remove(&key);
                }
            }
            result.extend(normalized);
            if e.id >= cursor {
                cursor = e.id;
                time = Some(e.time.clone());
            }
        }
        self.pending = pending;
        self.since = cursor.max(batch.last_id);
        self.last_time = time;
        Ok(result)
    }
}
// 短 ID 仅在已知设备中唯一时才展开，设备缺失、短 ID 碰撞和查询失败都保留未知来源。
fn resolve_source(
    client: &Client,
    local: &str,
    source: &str,
    cache: &mut Option<Vec<String>>,
) -> Option<String> {
    if crate::syncthing::folder::validate_device_id(source).is_ok() {
        return Some(if source == local {
            "local".into()
        } else {
            source.into()
        });
    }
    if source.len() != 7
        || !source
            .bytes()
            .all(|c| c.is_ascii_uppercase() || (b'2'..=b'7').contains(&c))
    {
        return None;
    }
    if cache.is_none() {
        let value = match client.list_devices() {
            Ok(value) if value.is_array() => value,
            _ => {
                *cache = Some(Vec::new());
                return None;
            }
        };
        let mut ids: Vec<String> = value
            .as_array()?
            .iter()
            .filter_map(|v| v["deviceID"].as_str())
            .filter(|id| crate::syncthing::folder::validate_device_id(id).is_ok())
            .map(String::from)
            .collect();
        if crate::syncthing::folder::validate_device_id(local).is_ok() {
            ids.push(local.into());
        }
        ids.sort();
        ids.dedup();
        *cache = Some(ids);
    }
    let mut matches = cache.as_ref()?.iter().filter(|id| id.starts_with(source));
    let id = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(if id == local {
        "local".into()
    } else {
        id.clone()
    })
}
// 索引是瞬时快照；历史事件引用的文件或临时文件夹可能已经消失。
fn index_file(
    client: &Client,
    folder: &str,
    path: &str,
) -> Result<Option<serde_json::Value>, AppError> {
    match client.call(
        "GET",
        "/rest/db/file",
        &[("folder", folder), ("file", path)],
        None,
    ) {
        Ok(value) => Ok(Some(value)),
        Err(e)
            if matches!(
                e.code.as_str(),
                "syncthing.unreachable" | "syncthing.unauthorized"
            ) =>
        {
            Err(e)
        }
        Err(_) => Ok(None),
    }
}
fn normalize(
    client: &Client,
    e: &Event,
    versions: &mut HashMap<(String, String), serde_json::Value>,
) -> Result<Vec<SyncEvent>, AppError> {
    let d = &e.data;
    let base = SyncEvent {
        folder_id: d["folder"].as_str().unwrap_or("").into(),
        time_ms: parse_ms(&e.time)?,
        kind: SyncEventKind::StateChanged,
        device: d["device"].as_str().map(String::from),
        by: None,
        path: d["item"].as_str().map(String::from),
        detail: None,
    };
    let mut out = base.clone();
    match e.kind.as_str() {
        "LocalChangeDetected" | "RemoteChangeDetected" => {
            let path = d["path"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(invalid_response)?;
            if out.folder_id.is_empty() {
                return Err(invalid_response());
            }
            if d["type"] != "file" {
                return Ok(Vec::new());
            }
            out.path = Some(path.into());
            out.by = if e.kind == "LocalChangeDetected" {
                Some("local".into())
            } else {
                d["modifiedBy"].as_str().map(String::from)
            };
            out.kind = match d["action"].as_str() {
                Some("deleted") => SyncEventKind::Deleted,
                Some("added" | "modified") if path.contains(".sync-conflict-") => {
                    SyncEventKind::Conflict
                }
                Some("added" | "modified") if e.kind == "RemoteChangeDetected" => {
                    SyncEventKind::Downloaded
                }
                Some("added" | "modified") => SyncEventKind::StateChanged,
                _ => return Err(invalid_response()),
            };
        }
        "ItemFinished" => {
            out.kind = if d["error"].as_str().is_some_and(|s| !s.is_empty()) {
                SyncEventKind::Error
            } else if out
                .path
                .as_deref()
                .is_some_and(|s| s.contains(".sync-conflict-"))
            {
                SyncEventKind::Conflict
            } else if d["action"] == "delete" {
                SyncEventKind::Deleted
            } else {
                SyncEventKind::Downloaded
            };
            if out.kind == SyncEventKind::Error {
                out.detail = Some(format!(
                    "文件同步失败：{}",
                    client
                        .redact_text(d["error"].as_str().unwrap_or(""))
                        .chars()
                        .take(512)
                        .collect::<String>()
                ));
            }
            // 删除保护只统计文件，目录删除不算文件数。
            if out.kind == SyncEventKind::Deleted && d["type"] == "dir" {
                return Ok(Vec::new());
            }
        }
        "LocalIndexUpdated" => {
            let mut result = Vec::new();
            if let Some(names) = d["filenames"].as_array() {
                for name in names {
                    let path = name.as_str().ok_or_else(invalid_response)?;
                    let file = index_file(client, &base.folder_id, path)?.unwrap_or_default();
                    let mut item = base.clone();
                    item.path = Some(path.into());
                    item.by = file["local"]["modifiedBy"].as_str().map(String::from);
                    if !file["local"]["version"].is_null() {
                        versions.insert(
                            (base.folder_id.clone(), path.into()),
                            file["local"]["version"].clone(),
                        );
                    }
                    item.kind = if file["local"]["deleted"] == true
                        && !matches!(
                            file["local"]["type"].as_str(),
                            Some("DIRECTORY" | "FILE_INFO_TYPE_DIRECTORY")
                        ) {
                        SyncEventKind::Deleted
                    } else {
                        SyncEventKind::StateChanged
                    };
                    result.push(item);
                }
            }
            return Ok(result);
        }
        // 远端索引仅表示远端已发布索引，不等于文件上传完成。
        "RemoteIndexUpdated" => {}
        "RemoteDownloadProgress" => {
            return Ok(Vec::new());
        }
        "FolderErrors" => {
            return Ok(d["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|v| {
                    let mut item = base.clone();
                    item.kind = SyncEventKind::Error;
                    item.path = v["path"].as_str().map(String::from);
                    item.detail = Some(format!(
                        "文件夹扫描或同步失败：{}",
                        client
                            .redact_text(v["error"].as_str().unwrap_or(""))
                            .chars()
                            .take(512)
                            .collect::<String>()
                    ));
                    item
                })
                .collect());
        }
        // Syncthing 的设备连接/断开事件把设备 ID 放在 id 字段，不是 device
        "DeviceConnected" => {
            out.kind = SyncEventKind::DeviceConnected;
            out.device = d["id"].as_str().map(String::from);
        }
        "DeviceDisconnected" => {
            out.kind = SyncEventKind::DeviceDisconnected;
            out.device = d["id"].as_str().map(String::from);
        }
        "FolderPaused" => out.kind = SyncEventKind::Paused,
        "StateChanged" => out.detail = d["to"].as_str().map(String::from),
        _ => return Ok(Vec::new()),
    }
    Ok(vec![out])
}
struct Window {
    deleted: VecDeque<(i64, String)>,
    reported: bool,
}
pub struct DeletionGuard {
    threshold_files: u64,
    threshold_ratio: f64,
    window: Duration,
    folders: HashMap<String, Window>,
}
impl DeletionGuard {
    /// 用数量、比例与时间窗口创建保护器；无效比例退回 20%，返回内存状态。
    pub fn new(threshold_files: u64, threshold_ratio: f64, window: Duration) -> Self {
        Self {
            threshold_files,
            threshold_ratio: if threshold_ratio.is_finite()
                && (0.0..=1.0).contains(&threshold_ratio)
            {
                threshold_ratio
            } else {
                0.2
            },
            window,
            folders: HashMap::new(),
        }
    }
    /// 根据 events 和当前 total_files 返回首次超限的文件夹 ID；调用方恢复任务后调用 reset 重新计数。
    pub fn observe(
        &mut self,
        events: &[SyncEvent],
        total_files: &HashMap<String, u64>,
    ) -> Vec<String> {
        self.observe_at(events, total_files)
    }
    /// 调用方恢复 folder_id 后显式清空该任务窗口；无返回数据。
    pub fn reset(&mut self, folder_id: &str) {
        self.folders.remove(folder_id);
    }
    fn observe_at(&mut self, events: &[SyncEvent], totals: &HashMap<String, u64>) -> Vec<String> {
        let mut result = Vec::new();
        let mut events: Vec<_> = events.iter().collect();
        events.sort_by_key(|e| e.time_ms);
        for event in events {
            for state in self.folders.values_mut() {
                while state.deleted.front().is_some_and(|(t, _)| {
                    event.time_ms.saturating_sub(*t)
                        >= self.window.as_millis().min(i64::MAX as u128) as i64
                }) {
                    state.deleted.pop_front();
                }
            }
            self.folders
                .retain(|_, s| s.reported || !s.deleted.is_empty());
            if event.folder_id.is_empty() {
                continue;
            }
            if event.kind != SyncEventKind::Deleted {
                continue;
            }
            let state = self
                .folders
                .entry(event.folder_id.clone())
                .or_insert_with(|| Window {
                    deleted: VecDeque::new(),
                    reported: false,
                });
            if state.reported {
                continue;
            }
            let key = event
                .path
                .clone()
                .unwrap_or_else(|| format!("{}:{}", event.time_ms, state.deleted.len()));
            if state.deleted.iter().any(|(_, p)| p == &key) {
                continue;
            }
            state.deleted.push_back((event.time_ms, key));
            let count = state.deleted.len() as u64;
            let total = totals.get(&event.folder_id).copied().unwrap_or(0);
            if count > self.threshold_files
                || (total >= 20 && count as f64 > total as f64 * self.threshold_ratio)
            {
                state.reported = true;
                state.deleted.clear();
                result.push(event.folder_id.clone());
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    fn deleted(n: usize) -> Vec<SyncEvent> {
        (0..n)
            .map(|i| SyncEvent {
                folder_id: "f".into(),
                time_ms: 0,
                kind: SyncEventKind::Deleted,
                device: None,
                by: None,
                path: Some(i.to_string()),
                detail: None,
            })
            .collect()
    }
    #[test]
    fn y23_阈值小目录只报一次及恢复() {
        let mut g = DeletionGuard::new(100, 0.2, Duration::from_secs(60));
        let totals = HashMap::from([("f".into(), 19)]);
        assert!(g.observe(&deleted(100), &totals).is_empty());
        assert_eq!(g.observe(&deleted(101), &totals), vec!["f"]);
        assert!(g.observe(&deleted(150), &totals).is_empty());
        g.reset("f");
        assert_eq!(
            g.observe(&deleted(5), &HashMap::from([("f".into(), 20)])),
            vec!["f"]
        );
    }
    #[test]
    fn y23_滑动窗口() {
        let mut g = DeletionGuard::new(100, 0.2, Duration::from_secs(60));
        let later = |n, time| {
            let mut e = deleted(n);
            for event in &mut e {
                event.time_ms = time;
            }
            e
        };
        let totals = HashMap::new();
        assert!(g.observe_at(&deleted(100), &totals).is_empty());
        assert!(g.observe_at(&later(100, 61000), &totals).is_empty());
        assert_eq!(g.observe_at(&later(101, 62000), &totals), vec!["f"]);
    }
    #[test]
    fn 事件夹具及重启去重() {
        let mut client = Client::new("http://127.0.0.1:1", "secret").unwrap();
        let events = Arc::new(Mutex::new(
            json!([{"id":5,"type":"ItemFinished","time":"2026-10-22T00:00:00Z","data":{"folder":"f","item":"a","action":"delete","type":"file"}}]),
        ));
        let data = events.clone();
        client.test_handler = Some(Arc::new(move |_, path, q, _| {
            if path == "/rest/system/status" {
                return Ok(json!({"startTime":"2026-10-22T00:00:00Z"}));
            }
            let value = data.lock().unwrap().clone();
            if q.iter().any(|(k, v)| *k == "since" && *v == "5") {
                Ok(json!([]))
            } else {
                Ok(value)
            }
        }));
        let local = LocalSync {
            address_updates: Default::default(),
            instance: None,
            client,
            id: "test".into(),
        };
        let mut pump = EventPump::new(&local);
        assert!(pump.poll().unwrap().is_empty());
        assert_eq!(pump.since, 5);
        assert!(pump.poll().unwrap().is_empty());
        *events.lock().unwrap() = json!([{"id":1,"type":"DeviceConnected","time":"2026-10-22T01:00:00Z","data":{"id":"d"}}]);
        let connected = pump.poll().unwrap();
        assert_eq!(connected[0].kind, SyncEventKind::DeviceConnected);
        assert_eq!(
            connected[0].device.as_deref(),
            Some("d"),
            "设备 ID 取自 id 字段"
        );
        for (kind, data, want) in [
            (
                "ItemFinished",
                json!({"error":"secret"}),
                SyncEventKind::Error,
            ),
            (
                "ItemFinished",
                json!({"item":"a.sync-conflict-x"}),
                SyncEventKind::Conflict,
            ),
            (
                "ItemFinished",
                json!({"item":"a"}),
                SyncEventKind::Downloaded,
            ),
            ("FolderPaused", json!({}), SyncEventKind::Paused),
            (
                "StateChanged",
                json!({"to":"idle"}),
                SyncEventKind::StateChanged,
            ),
            (
                "DeviceDisconnected",
                json!({}),
                SyncEventKind::DeviceDisconnected,
            ),
        ] {
            let e = Event {
                id: 1,
                kind: kind.into(),
                time: "2026-10-22T00:00:00Z".into(),
                data,
            };
            assert_eq!(
                normalize(local.client(), &e, &mut HashMap::new()).unwrap()[0].kind,
                want
            );
        }
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    #[test]
    fn 本机删除与远端索引确认上传_失败保留游标() {
        let mut c = Client::new("http://127.0.0.1:1", "secret").unwrap();
        let fail = Arc::new(AtomicBool::new(true));
        let flag = fail.clone();
        c.test_handler = Some(Arc::new(move |_, path, q, _| match path {
            "/rest/system/status" => Ok(json!({"startTime":"2026-10-22T00:00:00Z"})),
            "/rest/events" => Ok(json!([
                {"id":1,"type":"LocalIndexUpdated","time":"2026-10-22T00:00:01Z","data":{"folder":"f","filenames":["delete","upload"]}},
                {"id":2,"type":"LocalChangeDetected","time":"2026-10-22T00:00:01Z","data":{"folder":"f","path":"delete","action":"deleted","type":"file"}},
                {"id":3,"type":"RemoteIndexUpdated","time":"2026-10-22T00:00:02Z","data":{"folder":"f","device":"d"}}
            ])),
            "/rest/db/file" => {
                if flag.load(Ordering::Relaxed) {
                    return Err(AppError::new("syncthing.unreachable"));
                }
                let deleted = q.contains(&("file", "delete"));
                Ok(
                    json!({"local":{"deleted":deleted,"type":"FILE_INFO_TYPE_FILE","version":["a:1"]},"global":{"version":["a:1"]},"availability":[{"id":"d","fromTemporary":false}]}),
                )
            }
            _ => panic!("未预期接口"),
        }));
        let local = LocalSync {
            address_updates: Default::default(),
            instance: None,
            client: c,
            id: "a".into(),
        };
        let mut pump = EventPump::new(&local);
        pump.initialized = true;
        assert!(pump.poll().is_err());
        assert_eq!(pump.since, 0);
        fail.store(false, Ordering::Relaxed);
        let events = pump.poll().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == SyncEventKind::Deleted)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == SyncEventKind::Uploaded)
                .count(),
            1
        );
        assert!(pump.poll().unwrap().is_empty());
    }
}

#[cfg(test)]
mod missing_index_tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn 索引外文件和临时文件夹不阻塞事件及后续游标() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let mut client = Client::new("http://127.0.0.1:1", "secret").unwrap();
        client.test_handler = Some(Arc::new(move |_, path, q, _| match path {
            "/rest/system/status" => Ok(json!({"startTime":"2026-10-22T00:00:00Z"})),
            "/rest/events" => Ok(json!([
                {"id":1,"type":"LocalIndexUpdated","time":"2026-10-22T00:00:01Z","data":{"folder":"pl-est-gone","filenames":["deleted","unindexed","known"]}},
                {"id":2,"type":"RemoteIndexUpdated","time":"2026-10-22T00:00:02Z","data":{"folder":"pl-est-gone","device":"d"}},
                {"id":3,"type":"RemoteChangeDetected","time":"2026-10-22T00:00:03Z","data":{"folder":"f","path":"next","action":"modified","type":"file"}}
            ])),
            "/rest/db/file" => {
                counter.fetch_add(1, Ordering::Relaxed);
                if !q.contains(&("file", "known")) {
                    Err(AppError::new("syncthing.http_error")
                        .with("status", 404)
                        .with("body", "No such object in the index"))
                } else {
                    Ok(
                        json!({"local":{"version":["a:1"]},"global":{"version":["a:1"]},"availability":[{"id":"d","fromTemporary":false}]}),
                    )
                }
            }
            _ => panic!("未预期接口"),
        }));
        let local = LocalSync {
            address_updates: Default::default(),
            instance: None,
            client,
            id: "a".into(),
        };
        let mut pump = EventPump::new(&local);
        pump.initialized = true;
        let result = pump.poll().unwrap();
        assert_eq!(pump.since, 3);
        for path in ["deleted", "unindexed"] {
            let events: Vec<_> = result
                .iter()
                .filter(|e| e.path.as_deref() == Some(path))
                .collect();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].kind, SyncEventKind::StateChanged);
            assert!(events[0].detail.is_none());
        }
        assert!(
            result
                .iter()
                .any(|e| e.kind == SyncEventKind::Uploaded && e.path.as_deref() == Some("known"))
        );
        assert!(
            result
                .iter()
                .any(|e| e.kind == SyncEventKind::Downloaded && e.path.as_deref() == Some("next"))
        );
        assert_eq!(calls.load(Ordering::Relaxed), 4);
        assert!(pump.poll().unwrap().is_empty());
    }

    #[test]
    fn 索引补充不吞掉鉴权及服务故障() {
        for code in ["syncthing.unauthorized", "syncthing.unreachable"] {
            let mut client = Client::new("http://127.0.0.1:1", "secret").unwrap();
            client.test_handler = Some(Arc::new(move |_, _, _, _| {
                Err(AppError::new(code).with("status", 500))
            }));
            assert_eq!(index_file(&client, "f", "p").unwrap_err().code, code);
        }
    }
}

#[cfg(test)]
mod revision_tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    #[test]
    fn 首次只对齐游标且异常事件与500补充查询不阻塞后续() {
        let mut client = Client::new("http://127.0.0.1:1", "secret").unwrap();
        let data = Arc::new(Mutex::new(
            json!([{ "id":9,"type":"ItemFinished","time":"2026-10-22T00:00:00Z","data":{"action":"delete","folder":"f","item":"historical"}}]),
        ));
        let shared = data.clone();
        client.test_handler = Some(Arc::new(move |_, path, _, _| match path {
            "/rest/system/status" => Ok(json!({"startTime":"2026-10-22T00:00:00Z"})),
            "/rest/events" => Ok(shared.lock().unwrap().clone()),
            "/rest/db/file" => Err(AppError::new("syncthing.http_error").with("status", 500)),
            _ => panic!("未预期请求"),
        }));
        let local = LocalSync {
            address_updates: Default::default(),
            instance: None,
            client,
            id: "a".into(),
        };
        let mut pump = EventPump::new(&local);
        assert!(pump.poll().unwrap().is_empty());
        assert_eq!(pump.since, 9);
        *data.lock().unwrap() = json!([
            {"id":10, "type":"LocalIndexUpdated", "time":"2026-10-22T00:00:01Z", "data":{"folder":"f","filenames":["missing"]}},
            {"id":11, "type":"ItemFinished", "time":"无效日期", "data":{}},
            {"id":12, "type":"RemoteIndexUpdated", "time":"2026-10-22T00:00:02Z", "data":{}},
            {"id":13, "type":"RemoteChangeDetected", "time":"2026-10-22T00:00:03Z", "data":{"folder":"f","path":"ok","action":"modified","type":"file"}},
            {"id":14, "type":1}
        ]);
        let events = pump.poll().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].path.as_deref(), Some("ok"));
        assert_eq!(pump.since, 14);
        assert!(pump.poll().unwrap().is_empty());
    }
    #[test]
    fn 删除时间跨度大于窗口时同批不会误触发() {
        let mut guard = DeletionGuard::new(2, 1.0, Duration::from_secs(60));
        let events: Vec<_> = (0..10)
            .map(|i| SyncEvent {
                folder_id: "f".into(),
                time_ms: i * 61000,
                kind: SyncEventKind::Deleted,
                device: None,
                by: None,
                path: Some(i.to_string()),
                detail: None,
            })
            .collect();
        assert!(guard.observe(&events, &HashMap::new()).is_empty());
    }
}
