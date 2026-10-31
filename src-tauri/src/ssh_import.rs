//! 从用户的 ssh config 导入服务器（设计“参数来源”：导入的服务器是链接型，参数以 ssh config 为准）。
//! 解析在 `pierlink_core::ssh::config_file`；这里负责找配置文件、并发控制、取消和写数据库。

use crate::data::DbState;
use crate::terminal::find_ssh;
use pierlink_core::error::AppError;
use pierlink_core::process::SshProgram;
use pierlink_core::server_input::build_alias_server;
use pierlink_core::ssh::config_file::{self, SkipReason};
use serde::Serialize;
use specta::Type;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

/// 一次只跑一个预览；后来的预览排队等前一个结束（前一个通常已被取消，很快释放）。
/// 取消标志放在锁外，预览进行中也能取消；拿到锁之后才清除取消标志。
#[derive(Default)]
pub struct ImportState {
    busy: Mutex<()>,
    cancel: AtomicBool,
}

impl ImportState {
    /// 取消进行中的预览（关闭对话框、页面重载时）
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportCandidateDto {
    pub alias: String,
    pub file: String,
    pub line: f64,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
    pub via_jump: bool,
    pub has_forwards: bool,
    pub host_key_check_weakened: bool,
    pub already_imported: bool,
    pub error: Option<AppError>,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportSkippedDto {
    pub pattern: String,
    /// wildcard / negated / inMatchBlock / invalidAlias / duplicate
    pub reason: String,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreviewDto {
    pub config_path: String,
    pub candidates: Vec<ImportCandidateDto>,
    pub skipped: Vec<ImportSkippedDto>,
    pub warnings: Vec<AppError>,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportFailureDto {
    pub alias: String,
    pub error: AppError,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// 用户主目录（Windows 为 %USERPROFILE%）
fn home_dir() -> Result<PathBuf, AppError> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| AppError::new("ssh.config_read_failed").with("reason", "home"))
}

fn reason_code(r: &SkipReason) -> &'static str {
    match r {
        SkipReason::Wildcard => "wildcard",
        SkipReason::Negated => "negated",
        SkipReason::InMatchBlock => "inMatchBlock",
        SkipReason::InvalidAlias => "invalidAlias",
        SkipReason::Duplicate => "duplicate",
    }
}

/// 读取 ~/.ssh/config，列出可导入的别名并逐个用 `ssh -G` 解析实际参数。
#[tauri::command]
#[specta::specta]
pub async fn ssh_import_preview(app: AppHandle) -> Result<ImportPreviewDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<ImportState>();
        let _guard = state.busy.lock().unwrap_or_else(|p| p.into_inner());
        state.cancel.store(false, Ordering::SeqCst);
        let home = home_dir()?;
        let config = home.join(".ssh").join("config");
        let list = config_file::list_hosts(&config, &home)?;
        let existing = app.state::<DbState>().with(|db| db.list_ssh_aliases())?;
        let program = SshProgram {
            path: find_ssh(&app)?,
        };
        let candidates = config_file::preview(&program, &list, &existing, &state.cancel);
        eprintln!(
            "读取 ssh config: 路径={}, 别名数={}, 跳过={}",
            config.display(),
            candidates.len(),
            list.skipped.len()
        );
        Ok(ImportPreviewDto {
            config_path: config.display().to_string(),
            candidates: candidates
                .into_iter()
                .map(|c| ImportCandidateDto {
                    alias: c.alias,
                    file: c.file.display().to_string(),
                    line: f64::from(c.line),
                    hostname: c.hostname,
                    user: c.user,
                    port: c.port,
                    identity_files: c.identity_files,
                    via_jump: c.via_jump,
                    has_forwards: c.has_forwards,
                    host_key_check_weakened: c.host_key_check_weakened,
                    already_imported: c.already_imported,
                    error: c.error,
                })
                .collect(),
            skipped: list
                .skipped
                .iter()
                .map(|s| ImportSkippedDto {
                    pattern: s.pattern.clone(),
                    reason: reason_code(&s.reason).into(),
                })
                .collect(),
            warnings: list.warnings,
        })
    })
    .await
    .map_err(|e| AppError::new("ssh.import_failed").with("reason", e.to_string()))?
}

/// 取消进行中的预览
#[tauri::command]
#[specta::specta]
pub fn ssh_import_cancel(app: AppHandle) {
    app.state::<ImportState>().cancel();
}

/// 导入选中的别名。导入前重新读取配置确认别名仍然存在（防止界面传入任意别名）。
///
/// :param aliases: 要导入的别名
/// :param group_name: 放进的分组，空为不分组
/// :return: 没有导入成功的别名及原因
#[tauri::command]
#[specta::specta]
pub async fn ssh_import(
    app: AppHandle,
    aliases: Vec<String>,
    group_name: Option<String>,
) -> Result<Vec<ImportFailureDto>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        if aliases.len() > 5000 {
            return Err(AppError::new("ssh.config_too_large"));
        }
        let home = home_dir()?;
        let list = config_file::list_hosts(&home.join(".ssh").join("config"), &home)?;
        let group = group_name
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty());
        // 分组名不合法时整体报错，不能悄悄导入到“不分组”
        if group.is_some_and(|g| g.chars().count() > 64 || g.chars().any(char::is_control)) {
            return Err(AppError::new("server.invalid_field").with("field", "groupName"));
        }
        let mut failed = Vec::new();
        for alias in aliases {
            if !list.hosts.iter().any(|h| h.alias == alias) {
                failed.push(ImportFailureDto {
                    alias,
                    error: AppError::new("ssh.alias_not_in_config"),
                });
                continue;
            }
            let result =
                build_alias_server(&alias, group, &uuid::Uuid::new_v4().to_string(), now_ms())
                    .and_then(|s| app.state::<DbState>().with(|db| db.insert_server(&s)));
            match result {
                Ok(()) => eprintln!("导入服务器: 别名={alias}"),
                Err(error) => failed.push(ImportFailureDto { alias, error }),
            }
        }
        Ok(failed)
    })
    .await
    .map_err(|e| AppError::new("ssh.import_failed").with("reason", e.to_string()))?
}
