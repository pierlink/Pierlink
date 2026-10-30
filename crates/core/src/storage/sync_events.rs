//! 最近事件持久化；保存 90 天，每任务超过 20500 条时清理到最新 20000 条。
use super::{Db, failure, sql_error};
use crate::{
    error::AppError,
    sync::{SyncEvent, SyncEventKind},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

const RETENTION_MS: i64 = 90 * 24 * 60 * 60 * 1000;
const MAX_EVENTS: usize = 20_000;
const PRUNE_SLACK: i64 = 500;

/// 已持久化的同步事件；id 可作为界面键，(time_ms, id) 可作为下一页的游标。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredSyncEvent {
    /// 数据库自增主键；清理后不会复用。
    pub id: i64,
    /// 同步文件夹 ID；设备级事件可为空字符串。
    pub folder_id: String,
    /// 事件发生时间，Unix 毫秒。
    pub time_ms: i64,
    /// 事件种类；旧库中的 Paused 和 StateChanged 保留至正常清理。
    pub kind: SyncEventKind,
    /// 相关传输设备的 ID，无法确认时为空。
    pub device: Option<String>,
    /// 改动来源：本机为 local，远端为完整设备 ID，无法确认时为空。
    pub by: Option<String>,
    /// 相关文件路径，没有时为空。
    pub path: Option<String>,
    /// 已脱敏的事件详情，没有时为空。
    pub detail: Option<String>,
}

fn now_ms() -> Result<i64, AppError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .ok_or_else(|| {
            failure(
                "storage.clock",
                "storage.system_time_is_outside_the_supported_range",
                "系统时间超出可保存范围",
            )
        })
}
fn prune_expired(conn: &Connection, now_ms: i64) -> Result<(), AppError> {
    conn.execute(
        "DELETE FROM sync_events WHERE time_ms < ?1",
        [now_ms.saturating_sub(RETENTION_MS)],
    )
    .map_err(sql_error)?;
    Ok(())
}

fn prune_task(conn: &Connection, task_id: &str) -> Result<(), AppError> {
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM sync_events WHERE task_id=?1",
            [task_id],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if count > MAX_EVENTS as i64 + PRUNE_SLACK {
        // 反向遍历任务时间索引，只删除最旧的差额；同毫秒先写入的先删除。
        conn.execute(
            "DELETE FROM sync_events WHERE id IN (SELECT id FROM sync_events WHERE task_id=?1 ORDER BY time_ms ASC,id ASC LIMIT ?2)",
            params![task_id, count - MAX_EVENTS as i64],
        )
        .map_err(sql_error)?;
    }
    Ok(())
}

impl Db {
    /// 将 events 中界面显示的七类事件写入 task_id，忽略 Paused 和 StateChanged；返回写入结果。
    /// 调用方须沿用事件泵的脱敏结果，禁止添加秘密；设备级事件可使用空文件夹 ID。
    /// 同一事务内校验全部事件的非空文件夹归属、清理所有任务超过 90 天的事件，
    /// 并在当前任务超过 20500 条时保留最新 20000 条；失败整批撤销。
    pub fn insert_sync_events(
        &mut self,
        task_id: &str,
        events: &[SyncEvent],
    ) -> Result<(), AppError> {
        let now = now_ms()?;
        let tx = self.conn.transaction().map_err(sql_error)?;
        let folder_id: String = tx
            .query_row(
                "SELECT folder_id FROM sync_tasks WHERE id=?1",
                [task_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_error)?
            .ok_or_else(|| {
                failure(
                    "storage.not_found",
                    "storage.sync_task_does_not_exist",
                    "同步任务不存在",
                )
            })?;
        if events
            .iter()
            .any(|e| !e.folder_id.is_empty() && e.folder_id != folder_id)
        {
            return Err(failure(
                "storage.invalid_events",
                "storage.event_folder_does_not_belong_to_the_current_sync_task",
                "事件文件夹不属于当前同步任务",
            ));
        }
        {
            let mut insert = tx.prepare("INSERT INTO sync_events(task_id,folder_id,time_ms,kind,device,path,detail,by_device) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)").map_err(sql_error)?;
            for event in events {
                match event.kind {
                    SyncEventKind::Paused | SyncEventKind::StateChanged => continue,
                    SyncEventKind::Downloaded
                    | SyncEventKind::Uploaded
                    | SyncEventKind::Deleted
                    | SyncEventKind::Conflict
                    | SyncEventKind::Error
                    | SyncEventKind::DeviceConnected
                    | SyncEventKind::DeviceDisconnected => {}
                }
                let kind = serde_json::to_string(&event.kind).map_err(|_| {
                    failure(
                        "storage.json",
                        "storage.couldn_t_serialize_the_event_type",
                        "事件类型序列化失败",
                    )
                })?;
                insert
                    .execute(params![
                        task_id,
                        event.folder_id,
                        event.time_ms,
                        kind,
                        event.device,
                        event.path,
                        event.detail,
                        event.by
                    ])
                    .map_err(sql_error)?;
            }
        }
        prune_expired(&tx, now)?;
        prune_task(&tx, task_id)?;
        tx.commit().map_err(sql_error)
    }

    /// 返回 task_id 的一页完整事件及数据库 id，按 (time_ms DESC, id DESC) 排序。
    /// limit 为本页上限，最多 20000 条；零条、任务不存在或翻页结束均返回空列表。
    /// before 为 None 时读取首页，否则只取严格早于 (time_ms, id) 的事件；
    /// 下一页传入本页最后一条的 (time_ms, id)，游标对应的行被清理后仍可继续翻页。
    pub fn list_sync_events(
        &self,
        task_id: &str,
        limit: usize,
        before: Option<(i64, i64)>,
    ) -> Result<Vec<StoredSyncEvent>, AppError> {
        // 首页不加可空游标的 OR 条件，使后续页能直接按复合索引定位。
        let cursor_filter = if before.is_some() {
            " AND (time_ms,id) < (?3,?4)"
        } else {
            ""
        };
        let mut stmt = self.conn.prepare(&format!("SELECT id,folder_id,time_ms,kind,device,path,detail,by_device FROM sync_events WHERE task_id=?1{cursor_filter} ORDER BY time_ms DESC,id DESC LIMIT ?2")).map_err(sql_error)?;
        let limit = limit.min(MAX_EVENTS) as i64;
        let mut rows = match before {
            Some((time_ms, id)) => stmt.query(params![task_id, limit, time_ms, id]),
            None => stmt.query(params![task_id, limit]),
        }
        .map_err(sql_error)?;
        let mut events = Vec::new();
        while let Some(row) = rows.next().map_err(sql_error)? {
            let kind: String = row.get(3).map_err(sql_error)?;
            events.push(StoredSyncEvent {
                id: row.get(0).map_err(sql_error)?,
                folder_id: row.get(1).map_err(sql_error)?,
                time_ms: row.get(2).map_err(sql_error)?,
                kind: serde_json::from_str(&kind).map_err(|_| {
                    failure(
                        "storage.json",
                        "storage.couldn_t_read_the_event_type",
                        "事件类型无法读取",
                    )
                })?,
                device: row.get(4).map_err(sql_error)?,
                by: row.get(7).map_err(sql_error)?,
                path: row.get(5).map_err(sql_error)?,
                detail: row.get(6).map_err(sql_error)?,
            });
        }
        Ok(events)
    }

    /// 启动时传入当前 Unix 毫秒 now_ms；返回清理结果，失败整批撤销。
    /// 事务内清除所有任务超过 90 天的事件，并将超过 20500 条的任务清理到最新 20000 条。
    pub fn prune_sync_events(&mut self, now_ms: i64) -> Result<(), AppError> {
        let tx = self.conn.transaction().map_err(sql_error)?;
        prune_expired(&tx, now_ms)?;
        {
            let mut stmt = tx.prepare("SELECT id FROM sync_tasks").map_err(sql_error)?;
            let tasks = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?;
            for task_id in tasks {
                prune_task(&tx, &task_id.map_err(sql_error)?)?;
            }
        }
        tx.commit().map_err(sql_error)
    }
}
