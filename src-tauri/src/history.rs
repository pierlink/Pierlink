//! 命令历史（终端命令预测用）的 Tauri 命令：读一个范围的记录、记一条、清除全部。
//! 校验、隐私过滤和保留策略（每个范围最新 5000 条、180 天）在 `pierlink_core::storage::command_history`。

use crate::data::DbState;
use pierlink_core::error::AppError;
use pierlink_core::storage::CommandEntry;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, Manager};

/// 一条命令记录，字段和前端的 HistoryEntry 一致
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq)]
pub struct HistoryEntryDto {
    pub command: String,
    /// 敲这条命令时所在的目录；不知道时为 null
    pub cwd: Option<String>,
    /// 退出码；不知道时为 null
    pub exit: Option<i32>,
    /// 执行时间（Unix 毫秒）
    pub at: f64,
}

impl From<CommandEntry> for HistoryEntryDto {
    fn from(e: CommandEntry) -> Self {
        Self {
            command: e.command,
            cwd: e.cwd,
            exit: e.exit_code,
            at: e.used_at_ms as f64,
        }
    }
}

/// 前端的记录换成存储的格式；时间必须是有限的非负毫秒数
///
/// :param scope: 范围键（server:<id>、quick:<用户>@<主机>:<端口>、local:bash）
/// :param dto: 前端传来的记录
/// :return: 存储用的记录；时间不合法时返回 `history.invalid_entry`
fn to_entry(scope: String, dto: HistoryEntryDto) -> Result<CommandEntry, AppError> {
    // 9e15 毫秒约为公元 287000 年，再大就不是真实时间了
    if !dto.at.is_finite() || !(0.0..=9.0e15).contains(&dto.at) {
        return Err(AppError::new("history.invalid_entry").with("field", "used_at_ms"));
    }
    Ok(CommandEntry {
        scope,
        cwd: dto.cwd,
        command: dto.command,
        exit_code: dto.exit,
        used_at_ms: dto.at as i64,
    })
}

/// 读一个范围的命令记录（按时间先后，最多最新 5000 条）
#[tauri::command]
#[specta::specta]
pub async fn history_list(app: AppHandle, scope: String) -> Result<Vec<HistoryEntryDto>, AppError> {
    let list = app.state::<DbState>().with(|db| db.list_commands(&scope))?;
    Ok(list.into_iter().map(HistoryEntryDto::from).collect())
}

/// 记一条执行过的命令。可能带口令、令牌的命令后端会再过滤一次：不记，也不报错
#[tauri::command]
#[specta::specta]
pub async fn history_add(
    app: AppHandle,
    scope: String,
    entry: HistoryEntryDto,
) -> Result<(), AppError> {
    let entry = to_entry(scope, entry)?;
    app.state::<DbState>().with(|db| db.add_command(&entry))
}

/// 清除全部命令记录（设置里的“清除命令记录”）
#[tauri::command]
#[specta::specta]
pub async fn history_clear(app: AppHandle) -> Result<(), AppError> {
    app.state::<DbState>().with(|db| db.clear_commands(None))?;
    eprintln!("清除命令记录: 范围=全部");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dto(at: f64) -> HistoryEntryDto {
        HistoryEntryDto {
            command: "git status".into(),
            cwd: Some("/srv/app".into()),
            exit: Some(0),
            at,
        }
    }

    #[test]
    fn 记录和前端格式互相转换() {
        let entry = to_entry("server:a".into(), dto(1_700_000_000_123.0)).unwrap();
        assert_eq!(entry.scope, "server:a");
        assert_eq!(entry.used_at_ms, 1_700_000_000_123);
        assert_eq!(entry.exit_code, Some(0));
        assert_eq!(HistoryEntryDto::from(entry), dto(1_700_000_000_123.0));
    }

    #[test]
    fn 时间不合法时拒绝() {
        for at in [f64::NAN, f64::INFINITY, -1.0, 1.0e16] {
            let err = to_entry("server:a".into(), dto(at)).unwrap_err();
            assert_eq!(err.code, "history.invalid_entry");
        }
    }
}
