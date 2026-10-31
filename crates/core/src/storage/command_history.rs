//! 命令历史按次保存，不合并重复命令；敏感命令直接忽略。
//! 每个 scope 超过 5200 条时清理到最新 5000 条，查询最多返回最新 5000 条。
//! 打开数据库时删除超过 180 天的记录；插入时只清理当前 scope 的数量。
//! 删除服务器时在同一事务删除其记录；清除接口支持单个 scope 或全部历史。
//! 每日数据库备份包含历史，可移植配置导出不包含历史。

use super::{Db, sql_error};
use crate::error::AppError;
use regex::Regex;
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::LazyLock;

const RETENTION_MS: i64 = 180 * 24 * 60 * 60 * 1000;
const MAX_COMMANDS: i64 = 5000;
const PRUNE_SLACK: i64 = 200;

/// 一次命令执行记录；数据库内部主键由写入时生成，不参与去重。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct CommandEntry {
    /// server:<服务器 id>、quick:<用户>@<主机>:<端口> 或 local:bash。
    pub scope: String,
    /// 执行命令时的目录；未知时为空。
    pub cwd: Option<String>,
    /// 原始命令，最多 1000 个字符，不得含 C0 控制字符及 DEL。
    pub command: String,
    /// 0 至 255 的退出码；未知时为空。
    pub exit_code: Option<i32>,
    /// 执行时间，Unix 毫秒。
    pub used_at_ms: i64,
}

static SENSITIVE_COMMAND: LazyLock<Result<Regex, regex::Error>> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)passw(or)?d|passwd|secret|token|api[_-]?key|private[_-]?key|",
        r"BEGIN [A-Z ]*PRIVATE KEY|authorization|sshpass|--password|(^|\s)-p\S+|",
        r"AKIA[0-9A-Z]{16}|ghp_[a-z0-9_-]{16,}|github_pat_[a-z0-9_-]{9,}|",
        r"xox[abprs]-[a-z0-9_-]{15,}|sk-[a-z0-9_-]{17,}"
    ))
});

/// 检查 command 是否含常见凭据名称、私钥标记或至少 20 字符的令牌串，返回是否敏感。
/// 匹配不区分大小写；路径中出现凭据名称也会命中。规则无法编译时保守地禁止记录。
pub fn is_sensitive_command(command: &str) -> bool {
    match &*SENSITIVE_COMMAND {
        Ok(pattern) => pattern.is_match(command),
        Err(_) => true,
    }
}

fn invalid(field: &str) -> AppError {
    AppError::new("history.invalid_entry").with("field", field)
}

fn valid_scope(scope: &str) -> bool {
    if scope.chars().count() > 300 || scope.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    if scope == "local:bash" {
        return true;
    }
    if let Some(id) = scope.strip_prefix("server:") {
        return !id.is_empty()
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'));
    }
    let Some((user, address)) = scope.strip_prefix("quick:").and_then(|s| s.split_once('@')) else {
        return false;
    };
    let Some((host, port)) = address.rsplit_once(':') else {
        return false;
    };
    // 快速连接没填用户名时为空（由 ssh 用默认用户）
    let valid_user = user.is_empty()
        || (!user.starts_with('-')
            && user
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.-$".contains(&c)));
    let valid_host = if host.contains(':') {
        host.strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(host)
            .parse::<std::net::Ipv6Addr>()
            .is_ok()
    } else {
        !host.is_empty()
            && !host.starts_with('-')
            && host
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    valid_user
        && valid_host
        && port.bytes().all(|c| c.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port != 0)
}

fn validate(entry: &CommandEntry) -> Result<(), AppError> {
    if !valid_scope(&entry.scope) {
        return Err(invalid("scope"));
    }
    if entry.command.is_empty()
        || entry.command.chars().count() > 1000
        || entry.command.bytes().any(|c| c < 0x20 || c == 0x7f)
    {
        return Err(invalid("command"));
    }
    if entry
        .cwd
        .as_ref()
        .is_some_and(|cwd| cwd.len() > 4096 || cwd.chars().any(char::is_control))
    {
        return Err(invalid("cwd"));
    }
    if entry
        .exit_code
        .is_some_and(|code| !(0..=255).contains(&code))
    {
        return Err(invalid("exit_code"));
    }
    Ok(())
}

fn prune_scope(conn: &Connection, scope: &str) -> Result<(), AppError> {
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM command_history WHERE scope=?1",
            [scope],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if count > MAX_COMMANDS + PRUNE_SLACK {
        // 同毫秒按主键决定先后，避免时间相同导致保留数量或顺序不稳定。
        conn.execute(
            "DELETE FROM command_history WHERE id IN (
                SELECT id FROM command_history WHERE scope=?1
                ORDER BY used_at_ms ASC,id ASC LIMIT ?2
            )",
            params![scope, count - MAX_COMMANDS],
        )
        .map_err(sql_error)?;
    }
    Ok(())
}

impl Db {
    /// 校验 entry 并在同一事务写入、清理该 scope 的数量；返回写入或清理结果。
    /// 敏感命令返回成功但不写入；非法字段返回 history.invalid_entry 和字段名，不回传原文。
    pub fn add_command(&mut self, entry: &CommandEntry) -> Result<(), AppError> {
        validate(entry)?;
        if is_sensitive_command(&entry.command) {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        tx.execute(
            "INSERT INTO command_history(scope,cwd,command,exit_code,used_at_ms)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                entry.scope,
                entry.cwd,
                entry.command,
                entry.exit_code,
                entry.used_at_ms
            ],
        )
        .map_err(sql_error)?;
        prune_scope(&tx, &entry.scope)?;
        tx.commit().map_err(sql_error)
    }

    /// 返回 scope 最新的最多 5000 条记录，按 used_at_ms 升序，同毫秒按写入顺序排列。
    /// 合法但不存在的 scope 返回空列表，非法 scope 返回 history.invalid_entry。
    pub fn list_commands(&self, scope: &str) -> Result<Vec<CommandEntry>, AppError> {
        if !valid_scope(scope) {
            return Err(invalid("scope"));
        }
        let mut entries = self
            .conn
            .prepare(
                "SELECT scope,cwd,command,exit_code,used_at_ms FROM command_history
                 WHERE scope=?1 ORDER BY used_at_ms DESC,id DESC LIMIT ?2",
            )
            .map_err(sql_error)?
            .query_map(params![scope, MAX_COMMANDS], |row| {
                Ok(CommandEntry {
                    scope: row.get(0)?,
                    cwd: row.get(1)?,
                    command: row.get(2)?,
                    exit_code: row.get(3)?,
                    used_at_ms: row.get(4)?,
                })
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        entries.reverse();
        Ok(entries)
    }

    /// 清除 scope 对应历史；None 表示全部。返回清除结果，非法 scope 返回字段错误。
    pub fn clear_commands(&mut self, scope: Option<&str>) -> Result<(), AppError> {
        if let Some(scope) = scope {
            if !valid_scope(scope) {
                return Err(invalid("scope"));
            }
            self.conn
                .execute("DELETE FROM command_history WHERE scope=?1", [scope])
                .map_err(sql_error)?;
        } else {
            self.conn
                .execute("DELETE FROM command_history", [])
                .map_err(sql_error)?;
        }
        Ok(())
    }

    /// 按当前 Unix 毫秒 now_ms 删除所有 scope 超过 180 天的记录，返回清理结果。
    /// 恰好 180 天的记录保留；时间下界用饱和减法，避免极端时间溢出。
    pub fn prune_commands(&mut self, now_ms: i64) -> Result<(), AppError> {
        self.conn
            .execute(
                "DELETE FROM command_history WHERE used_at_ms < ?1",
                [now_ms.saturating_sub(RETENTION_MS)],
            )
            .map_err(sql_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
