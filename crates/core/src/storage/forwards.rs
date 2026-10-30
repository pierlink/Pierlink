//! 端口转发与命令片段的持久化，所有冲突检查和写入在同一事务内完成。
use super::{Db, PortForward, Snippet, failure, require_changed, sql_error};
use crate::{error::AppError, storage::ForwardKind};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

impl Db {
    /// 返回 server_id 的转发；None 返回全部，按 sort_order、名称、ID 排序。
    pub fn list_port_forwards(
        &self,
        server_id: Option<&str>,
    ) -> Result<Vec<PortForward>, AppError> {
        self.conn.prepare("SELECT * FROM port_forwards WHERE (?1 IS NULL OR server_id=?1) ORDER BY sort_order,name,id")
            .map_err(sql_error)?.query_map([server_id], PortForward::from_row).map_err(sql_error)?
            .collect::<Result<_, _>>().map_err(sql_error)
    }

    /// 校验并插入或更新 f；同一服务器已启用转发监听端口冲突时返回 tunnel.port_in_use。
    pub fn upsert_port_forward(&self, f: &PortForward) -> Result<(), AppError> {
        crate::tunnel::forward::validate(f)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        check_conflict(&tx, f)?;
        tx.execute("INSERT INTO port_forwards(id,server_id,name,kind,local_port,remote_host,remote_port,enabled,sort_order) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(id) DO UPDATE SET server_id=excluded.server_id,name=excluded.name,kind=excluded.kind,local_port=excluded.local_port,remote_host=excluded.remote_host,remote_port=excluded.remote_port,enabled=excluded.enabled,sort_order=excluded.sort_order",
            params![f.id,f.server_id,f.name,f.kind,f.local_port,f.remote_host,f.remote_port,f.enabled,f.sort_order]).map_err(sql_error)?;
        tx.commit().map_err(sql_error)
    }

    /// 设置 id 的应用启动时自动启用状态；启用前重新校验并检查端口冲突，不存在返回 storage.not_found。
    pub fn set_port_forward_enabled(&self, id: &str, enabled: bool) -> Result<(), AppError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let mut f = tx
            .query_row(
                "SELECT * FROM port_forwards WHERE id=?1",
                [id],
                PortForward::from_row,
            )
            .optional()
            .map_err(sql_error)?
            .ok_or_else(|| {
                failure(
                    "storage.not_found",
                    "storage.port_forward_does_not_exist",
                    "端口转发不存在",
                )
            })?;
        f.enabled = enabled;
        crate::tunnel::forward::validate(&f)?;
        check_conflict(&tx, &f)?;
        tx.execute(
            "UPDATE port_forwards SET enabled=?2 WHERE id=?1",
            params![id, enabled],
        )
        .map_err(sql_error)?;
        tx.commit().map_err(sql_error)
    }

    /// 删除 id 对应转发，不存在返回 storage.not_found；不会操作正在运行的隧道。
    pub fn delete_port_forward(&self, id: &str) -> Result<(), AppError> {
        require_changed(
            self.conn
                .execute("DELETE FROM port_forwards WHERE id=?1", [id])
                .map_err(sql_error)?,
        )
    }

    /// 返回全部命令片段，按 sort_order、标题、ID 排序；保留未知分组名称。
    pub fn list_snippets(&self) -> Result<Vec<Snippet>, AppError> {
        self.conn
            .prepare("SELECT * FROM snippets ORDER BY sort_order,title,id")
            .map_err(sql_error)?
            .query_map([], Snippet::from_row)
            .map_err(sql_error)?
            .collect::<Result<_, _>>()
            .map_err(sql_error)
    }

    /// 插入或更新 s，要求 ID、标题和正文非空且无 NUL；正文及换行原样保存，错误不含正文。
    pub fn upsert_snippet(&self, s: &Snippet) -> Result<(), AppError> {
        for (field, value) in [
            ("id", s.id.as_str()),
            ("title", &s.title),
            ("body", &s.body),
        ] {
            if value.trim().is_empty() || value.contains('\0') {
                return Err(failure(
                    "storage.invalid_snippet",
                    "storage.snippet_field_cannot_be_empty_or_contain_null_characters",
                    "片段字段不能为空或含 NUL",
                )
                .with("field", field));
            }
        }
        if s.group_name
            .as_ref()
            .is_some_and(|g| g.trim().is_empty() || g.contains('\0'))
        {
            return Err(failure(
                "storage.invalid_snippet",
                "storage.snippet_group_must_be_empty_or_a_valid_name",
                "片段分组必须为空或有效名称",
            )
            .with("field", "group_name"));
        }
        self.conn.execute("INSERT INTO snippets(id,title,body,group_name,sort_order) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET title=excluded.title,body=excluded.body,group_name=excluded.group_name,sort_order=excluded.sort_order",
            params![s.id,s.title,s.body,s.group_name,s.sort_order]).map_err(sql_error)?;
        Ok(())
    }

    /// 删除 id 对应片段，不存在返回 storage.not_found。
    pub fn delete_snippet(&self, id: &str) -> Result<(), AppError> {
        require_changed(
            self.conn
                .execute("DELETE FROM snippets WHERE id=?1", [id])
                .map_err(sql_error)?,
        )
    }
}

fn check_conflict(conn: &Connection, f: &PortForward) -> Result<(), AppError> {
    if !f.enabled {
        return Ok(());
    }
    // -R 的 local_port 是连接目标，可以与监听端口重合；只检查同一监听端的端口。
    let remote = f.kind == ForwardKind::Remote;
    let port = if remote {
        f.remote_port
    } else {
        Some(f.local_port)
    };
    let conflict: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM port_forwards WHERE server_id=?1 AND id<>?2 AND enabled=1 AND ((?3=1 AND kind='remote' AND remote_port=?4) OR (?3=0 AND kind IN ('local','dynamic') AND local_port=?4)))",
        params![f.server_id,f.id,remote,port], |row| row.get(0)).map_err(sql_error)?;
    if conflict {
        return Err(AppError::new("tunnel.port_in_use")
            .with("port", port.unwrap_or(0))
            .with("side", if remote { "remote" } else { "local" }));
    }
    Ok(())
}
