//! SQLite 仓储：迁移、实体读写、路径约束及每日在线备份。
//! 凭据只能传入凭据管理器的引用名；普通文本和设置不得用于存放秘密。
mod backup;
pub mod command_history;
mod conversion;
pub use backup::{Restored, open_or_restore};
pub use command_history::CommandEntry;
mod forwards;
mod mobile;
mod models;
mod paths;
pub(crate) mod profile;
mod sync_events;
mod writes;

pub use models::{
    AuthMethod, ConflictPolicy, EndpointMode, Engine, ForwardKind, PortForward, Server,
    ServerInstance, ServerSource, Snippet, SyncEndpoint, SyncTask,
};
pub use paths::{dangerous_local_path, dangerous_remote_path, paths_overlap};
pub use sync_events::StoredSyncEvent;

use crate::error::AppError;
use rand::RngExt;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use rusqlite_migration::{M, Migrations};
use serde::{Serialize, de::DeserializeOwned};
use std::{path::Path, time::Duration};

/// 数据库连接；连接及未提交事务在离开作用域时自动释放。
pub struct Db {
    conn: Connection,
}

fn failure(code: &str, text_key: &'static str, reason: &str) -> AppError {
    AppError::new(code).with_reason(text_key, reason)
}

fn sql_error(error: rusqlite::Error) -> AppError {
    // 不转发 SQLite 原始错误文本，避免 SQL、触发器或参数中的秘密泄露。
    let (code, text_key, reason) = match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => (
            "storage.corrupt",
            "storage.database_integrity_check_failed",
            "数据库完整性检查失败",
        ),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => (
            "storage.busy",
            "storage.database_busy",
            "数据库正被其他操作占用",
        ),
        Some(rusqlite::ErrorCode::ConstraintViolation) => (
            "storage.constraint",
            "storage.constraint_failed",
            "数据不满足唯一性、外键或字段约束",
        ),
        _ => (
            "storage.database",
            "storage.database_operation",
            "数据库操作失败",
        ),
    };
    let mut result = failure(code, text_key, reason);
    if let Some(e) = error.sqlite_error() {
        result = result.with("sqlite_code", e.extended_code);
    }
    result
}

fn io_error(error: std::io::Error) -> AppError {
    let mut result = failure(
        "storage.io",
        "storage.couldn_t_access_the_database_directory_or_file",
        "无法读写数据库目录或文件",
    );
    if let Some(code) = error.raw_os_error() {
        result = result.with("os_code", code);
    }
    result
}

/// 打开 path 指定的数据库并创建父目录；返回启用 WAL、外键、5 秒忙等待且完成迁移的 Db。
/// 清理超过 180 天的命令历史；启动完整性检查失败返回 storage.corrupt，不自动覆盖损坏文件。
pub fn open(path: &Path) -> Result<Db, AppError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    let mut conn = Connection::open(path).map_err(sql_error)?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(sql_error)?;
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(sql_error)?;
    if check != "ok" {
        return Err(failure(
            "storage.corrupt",
            "storage.database_integrity_check_failed",
            "数据库完整性检查失败",
        ));
    }
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(sql_error)?;
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(sql_error)?;
    Migrations::new(vec![
        M::up(include_str!("v1.sql")),
        M::up(include_str!("v2.sql")),
        M::up(include_str!("v3.sql")),
        M::up(include_str!("v4.sql")),
        M::up(include_str!("v5.sql")),
        M::up(include_str!("v6.sql")),
        M::up(include_str!("v7.sql")),
        M::up(include_str!("v8.sql")),
        M::up(include_str!("v9.sql")),
    ])
    .to_latest(&mut conn)
    .map_err(|_| {
        failure(
            "storage.migration",
            "storage.database_migration_failed_or_its_version_is_unsupported",
            "数据库结构迁移失败或版本不受支持",
        )
    })?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or_else(|| {
            failure(
                "storage.clock",
                "storage.system_time_is_outside_the_supported_range",
                "系统时间超出可保存范围",
            )
        })?;
    let mut db = Db { conn };
    db.prune_commands(now_ms)?;
    Ok(db)
}

impl Db {
    /// 插入 server，凭据字段必须为此服务器的引用名；返回写入结果。
    pub fn insert_server(&self, server: &Server) -> Result<(), AppError> {
        validate_server(server)?;
        self.write_alias_checked(server, false)
    }

    /// 按 server.id 更新全部字段；不存在返回 storage.not_found。
    pub fn update_server(&self, server: &Server) -> Result<(), AppError> {
        validate_server(server)?;
        self.write_alias_checked(server, true)
    }

    /// 返回所有来源为 ssh_config 的别名集合，区分大小写；查询失败返回数据库错误。
    pub fn list_ssh_aliases(&self) -> Result<std::collections::HashSet<String>, AppError> {
        self.conn
            .prepare("SELECT ssh_alias FROM servers WHERE source='ssh_config'")
            .map_err(sql_error)?
            .query_map([], |row| row.get(0))
            .map_err(sql_error)?
            .collect::<Result<_, _>>()
            .map_err(sql_error)
    }

    fn write_alias_checked(&self, server: &Server, update: bool) -> Result<(), AppError> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if server.source == ServerSource::SshConfig {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM servers WHERE source='ssh_config' AND ssh_alias=?1 AND (?2=0 OR id<>?3))",
                rusqlite::params![server.ssh_alias, update, server.id], |row| row.get(0)
            ).map_err(sql_error)?;
            if exists {
                return Err(failure(
                    "storage.alias_exists",
                    "storage.this_ssh_alias_has_already_been_imported",
                    "此 SSH 别名已经导入",
                ));
            }
        }
        if update {
            writes::update_servers(&tx, server)?;
        } else {
            writes::insert_servers(&tx, server)?;
        }
        tx.commit().map_err(sql_error)
    }

    /// 按 id 查询服务器，未找到返回 None，读取失败返回错误。
    pub fn get_server(&self, id: &str) -> Result<Option<Server>, AppError> {
        self.conn
            .query_row("SELECT * FROM servers WHERE id=?1", [id], Server::from_row)
            .optional()
            .map_err(sql_error)
    }

    /// 返回全部服务器，按 sort_order、名称、ID 排序；读取失败返回错误。
    pub fn list_servers(&self) -> Result<Vec<Server>, AppError> {
        self.conn
            .prepare("SELECT * FROM servers ORDER BY sort_order, name, id")
            .map_err(sql_error)?
            .query_map([], Server::from_row)
            .map_err(sql_error)?
            .collect::<Result<_, _>>()
            .map_err(sql_error)
    }

    /// 删除 id 对应服务器并级联清理；在用时返回 storage.server_in_use 及 JSON 任务名列表。
    /// 不存在返回 storage.not_found。
    pub fn delete_server(&mut self, id: &str) -> Result<(), AppError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let names: Vec<String> = tx.prepare("SELECT t.name FROM sync_tasks t JOIN sync_endpoints e ON e.task_id=t.id WHERE e.server_id=?1 ORDER BY t.name,t.id").map_err(sql_error)?
            .query_map([id], |r| r.get(0)).map_err(sql_error)?.collect::<Result<_, _>>().map_err(sql_error)?;
        if !names.is_empty() {
            let task_names = serde_json::to_string(&names).map_err(|_| {
                failure(
                    "storage.json",
                    "storage.couldn_t_serialize_the_task_name_list",
                    "任务名列表序列化失败",
                )
            })?;
            return Err(failure(
                "storage.server_in_use",
                "storage.server_is_still_used_by_sync_tasks",
                "服务器仍被同步任务使用",
            )
            .with("task_names", task_names));
        }
        require_changed(
            tx.execute("DELETE FROM servers WHERE id=?1", [id])
                .map_err(sql_error)?,
        )?;
        tx.execute(
            "DELETE FROM command_history WHERE scope=?1",
            [format!("server:{id}")],
        )
        .map_err(sql_error)?;
        tx.commit().map_err(sql_error)
    }

    /// 在同一事务插入 task 与非空 endpoints；路径会词法规范化，重叠时返回对方任务名。
    pub fn insert_sync_task(
        &mut self,
        task: &SyncTask,
        endpoints: &[SyncEndpoint],
    ) -> Result<(), AppError> {
        let task = normalized_task(task)?;
        validate_endpoints(&task.id, endpoints)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        check_overlap(&tx, &task, false)?;
        writes::insert_sync_tasks(&tx, &task)?;
        write_endpoints(&tx, endpoints)?;
        tx.commit().map_err(sql_error)
    }

    /// 按 id 查询任务及按服务器 ID 排序的端点；未找到返回 None。
    pub fn get_sync_task(
        &mut self,
        id: &str,
    ) -> Result<Option<(SyncTask, Vec<SyncEndpoint>)>, AppError> {
        let tx = self.conn.transaction().map_err(sql_error)?;
        let task = tx
            .query_row(
                "SELECT * FROM sync_tasks WHERE id=?1",
                [id],
                SyncTask::from_row,
            )
            .optional()
            .map_err(sql_error)?;
        let result = task
            .map(|t| read_endpoints(&tx, id).map(|e| (t, e)))
            .transpose()?;
        tx.commit().map_err(sql_error)?;
        Ok(result)
    }

    /// 返回全部任务，按名称、ID 排序；端点可用 get_sync_task 读取。
    pub fn list_sync_tasks(&self) -> Result<Vec<SyncTask>, AppError> {
        self.conn
            .prepare("SELECT * FROM sync_tasks ORDER BY name,id")
            .map_err(sql_error)?
            .query_map([], SyncTask::from_row)
            .map_err(sql_error)?
            .collect::<Result<_, _>>()
            .map_err(sql_error)
    }

    /// 按 task.id 更新任务字段并检查路径重叠，保留端点；不存在返回 storage.not_found。
    /// 改变路径前，调用方须先完成设计要求的移除文件夹与清空索引流程。
    pub fn update_sync_task(&mut self, task: &SyncTask) -> Result<(), AppError> {
        let task = normalized_task(task)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        check_overlap(&tx, &task, true)?;
        writes::update_sync_tasks(&tx, &task)?;
        tx.commit().map_err(sql_error)
    }

    /// 在一个事务中更新 task 并替换全部非空 endpoints；校验、外键或写入失败时两者均保持原值。
    /// 路径规范化及重叠检查与 update_sync_task 一致；不存在返回 storage.not_found。
    pub fn update_sync_task_with_endpoints(
        &mut self,
        task: &SyncTask,
        endpoints: &[SyncEndpoint],
    ) -> Result<(), AppError> {
        let task = normalized_task(task)?;
        validate_endpoints(&task.id, endpoints)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sync_tasks WHERE id=?1)",
                [&task.id],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if !exists {
            return Err(failure(
                "storage.not_found",
                "storage.sync_task_does_not_exist",
                "同步任务不存在",
            ));
        }
        check_overlap(&tx, &task, true)?;
        writes::update_sync_tasks(&tx, &task)?;
        tx.execute("DELETE FROM sync_endpoints WHERE task_id=?1", [&task.id])
            .map_err(sql_error)?;
        write_endpoints(&tx, endpoints)?;
        tx.commit().map_err(sql_error)
    }

    /// 按 id 删除任务并级联删除端点和事件，不删除任何同步文件；不存在返回 storage.not_found。
    pub fn delete_sync_task(&self, id: &str) -> Result<(), AppError> {
        require_changed(
            self.conn
                .execute("DELETE FROM sync_tasks WHERE id=?1", [id])
                .map_err(sql_error)?,
        )
    }

    /// 在事务中用非空 endpoints 替换 task_id 的全部端点；失败时保留原端点。
    pub fn set_endpoints(
        &mut self,
        task_id: &str,
        endpoints: &[SyncEndpoint],
    ) -> Result<(), AppError> {
        validate_endpoints(task_id, endpoints)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sync_tasks WHERE id=?1)",
                [task_id],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if !exists {
            return Err(failure(
                "storage.not_found",
                "storage.sync_task_does_not_exist",
                "同步任务不存在",
            ));
        }
        tx.execute("DELETE FROM sync_endpoints WHERE task_id=?1", [task_id])
            .map_err(sql_error)?;
        write_endpoints(&tx, endpoints)?;
        tx.commit().map_err(sql_error)
    }

    /// 写入或替换某台服务器的实例记录（部署、升级、换端口后调用）。
    /// 凭据字段必须是该服务器的凭据引用名，不能是秘密本身。
    pub fn upsert_instance(&self, inst: &ServerInstance) -> Result<(), AppError> {
        for (cred, purpose) in [
            (&inst.apikey_cred, "apikey"),
            (&inst.gui_password_cred, "gui-password"),
        ] {
            if cred != &format!("pierlink/instance/{}/{purpose}", inst.server_id) {
                return Err(failure(
                    "storage.invalid_credential_ref",
                    "storage.instance_credential_fields_must_reference_this_server_s_credentials",
                    "实例凭据字段只能保存此服务器的凭据引用",
                ));
            }
        }
        self.conn
            .execute(
                "INSERT INTO server_instances (server_id, instance_id, syncthing_version, arch, state_dir, socket_path, sync_port, device_id, apikey_cred, gui_password_cred, autostart_consent, deployed_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
                 ON CONFLICT(server_id) DO UPDATE SET instance_id=excluded.instance_id, syncthing_version=excluded.syncthing_version, arch=excluded.arch, \
                 state_dir=excluded.state_dir, socket_path=excluded.socket_path, sync_port=excluded.sync_port, device_id=excluded.device_id, \
                 apikey_cred=excluded.apikey_cred, gui_password_cred=excluded.gui_password_cred, autostart_consent=excluded.autostart_consent, updated_at=excluded.updated_at",
                rusqlite::params![
                    inst.server_id, inst.instance_id, inst.syncthing_version, inst.arch, inst.state_dir, inst.socket_path,
                    inst.sync_port, inst.device_id, inst.apikey_cred, inst.gui_password_cred, inst.autostart_consent,
                    inst.deployed_at, inst.updated_at
                ],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    /// 读取某台服务器的实例记录；没有部署过返回 None。
    pub fn get_instance(&self, server_id: &str) -> Result<Option<ServerInstance>, AppError> {
        self.conn
            .query_row(
                "SELECT * FROM server_instances WHERE server_id=?1",
                [server_id],
                ServerInstance::from_row,
            )
            .optional()
            .map_err(sql_error)
    }

    /// 删除某台服务器的实例记录（卸载后调用）；不存在时也算成功。
    pub fn delete_instance(&self, server_id: &str) -> Result<(), AppError> {
        self.conn
            .execute(
                "DELETE FROM server_instances WHERE server_id=?1",
                [server_id],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    /// 按 key 读取 JSON 设置并反序列化为 T；不存在返回 None，读取或类型错误返回错误。
    pub fn get_setting<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, AppError> {
        read_setting(&self.conn, key)
    }

    /// 将非敏感 value 序列化成 JSON 并写入 key，已有值会替换；返回写入结果。
    /// 密码、API 密钥和口令必须通过凭据管理器保存，不得作为设置传入。
    pub fn set_setting<T: Serialize>(&self, key: &str, value: &T) -> Result<(), AppError> {
        let value = serde_json::to_string(value).map_err(|_| {
            failure(
                "storage.json",
                "storage.couldn_t_serialize_settings",
                "设置序列化失败",
            )
        })?;
        self.conn.execute("INSERT INTO settings(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key, &value]).map_err(sql_error)?;
        Ok(())
    }

    /// 返回持久化的 8 位小写字母数字设备 ID；首次调用在事务内随机生成，之后保持不变。
    pub fn device_short_id(&mut self) -> Result<String, AppError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(id) = read_setting::<String>(&tx, "device.short_id")? {
            if id.len() != 8
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            {
                return Err(failure(
                    "storage.invalid_device_id",
                    "storage.saved_device_short_id_has_an_invalid_format",
                    "已保存的设备短 ID 格式错误",
                ));
            }
            tx.commit().map_err(sql_error)?;
            return Ok(id);
        }
        let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let mut rng = rand::rng();
        let id: String = (0..8)
            .map(|_| alphabet[rng.random_range(0..alphabet.len())] as char)
            .collect();
        let json = serde_json::to_string(&id).map_err(|_| {
            failure(
                "storage.json",
                "storage.couldn_t_serialize_the_device_short_id",
                "设备短 ID 序列化失败",
            )
        })?;
        tx.execute(
            "INSERT INTO settings(key,value) VALUES ('device.short_id',?1)",
            [json],
        )
        .map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        Ok(id)
    }
}

fn validate_server(server: &Server) -> Result<(), AppError> {
    if server
        .password_cred
        .as_ref()
        .is_some_and(|v| v != &format!("pierlink/server/{}/password", server.id))
    {
        return Err(failure(
            "storage.invalid_credential_ref",
            "storage.password_field_must_reference_this_server_s_credentials",
            "密码字段只能保存此服务器的凭据引用",
        ));
    }
    Ok(())
}

fn require_changed(changed: usize) -> Result<(), AppError> {
    if changed == 0 {
        Err(failure(
            "storage.not_found",
            "storage.record_does_not_exist",
            "记录不存在",
        ))
    } else {
        Ok(())
    }
}

fn normalized_task(task: &SyncTask) -> Result<SyncTask, AppError> {
    let mut task = task.clone();
    task.local_path = paths::normalized_absolute(&task.local_path).ok_or_else(|| {
        failure(
            "storage.invalid_path",
            "storage.local_directory_must_be_a_valid_absolute_path",
            "本地目录必须为有效绝对路径",
        )
    })?;
    Ok(task)
}

fn check_overlap(conn: &Connection, task: &SyncTask, exclude_self: bool) -> Result<(), AppError> {
    let mut stmt = conn
        .prepare("SELECT id,name,local_path FROM sync_tasks ORDER BY id")
        .map_err(sql_error)?;
    let mut rows = stmt.query([]).map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let id: String = row.get(0).map_err(sql_error)?;
        let name: String = row.get(1).map_err(sql_error)?;
        let path: String = row.get(2).map_err(sql_error)?;
        if !(exclude_self && id == task.id) && paths_overlap(&path, &task.local_path) {
            return Err(failure(
                "storage.local_path_overlap",
                "storage.local_directory_overlaps_an_existing_task",
                "本地目录与已有任务重叠",
            )
            .with("task_name", name));
        }
    }
    Ok(())
}

fn validate_endpoints(task_id: &str, endpoints: &[SyncEndpoint]) -> Result<(), AppError> {
    if endpoints.is_empty() || endpoints.iter().any(|e| e.task_id != task_id) {
        return Err(failure(
            "storage.invalid_endpoints",
            "storage.endpoints_must_be_nonempty_and_belong_to_the_current_task",
            "端点不能为空且必须属于当前任务",
        ));
    }
    Ok(())
}

fn write_endpoints(conn: &Connection, endpoints: &[SyncEndpoint]) -> Result<(), AppError> {
    let mut stmt = conn
        .prepare(
            "INSERT INTO sync_endpoints(task_id,server_id,remote_path,mode) VALUES (?1,?2,?3,?4)",
        )
        .map_err(sql_error)?;
    for e in endpoints {
        stmt.execute(rusqlite::params![
            e.task_id,
            e.server_id,
            e.remote_path,
            e.mode
        ])
        .map_err(sql_error)?;
    }
    Ok(())
}

fn read_endpoints(conn: &Connection, task_id: &str) -> Result<Vec<SyncEndpoint>, AppError> {
    conn.prepare("SELECT * FROM sync_endpoints WHERE task_id=?1 ORDER BY server_id")
        .map_err(sql_error)?
        .query_map([task_id], SyncEndpoint::from_row)
        .map_err(sql_error)?
        .collect::<Result<_, _>>()
        .map_err(sql_error)
}

fn read_setting<T: DeserializeOwned>(conn: &Connection, key: &str) -> Result<Option<T>, AppError> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()
        .map_err(sql_error)?;
    value
        .map(|v| {
            serde_json::from_str(&v).map_err(|_| {
                failure(
                    "storage.json",
                    "storage.settings_content_or_type_does_not_match_the_json_contract",
                    "设置内容或类型不符合 JSON 约定",
                )
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests;
