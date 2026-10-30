//! 应用数据目录与数据库。
//!
//! 数据放在 `%LOCALAPPDATA%\com.pierlink.desktop`（应用标识命名的目录，WebView2 的数据也在这里）。
//! 以前放在 `%LOCALAPPDATA%\Pierlink`，和安装目录是同一个；启动时会把旧位置的数据搬过来（阶段 5）。
//! 这样卸载时勾选“同时删除我的数据”才能删干净，不勾就保留。

use pierlink_core::error::AppError;
use pierlink_core::storage::{self, Db};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tauri::{AppHandle, Manager};

/// 本次运行实际使用的数据目录（启动时迁移后确定）。
/// 搬家中断且需要人工恢复时存错误：这次运行里谁都不能往数据目录写东西，
/// 否则新目录里多出的文件会让下次启动的自动恢复失败
static DATA_DIR: OnceLock<Result<PathBuf, AppError>> = OnceLock::new();

/// 以前的数据目录（和安装目录相同）
fn legacy_dir(app: &AppHandle) -> Result<PathBuf, AppError> {
    Ok(app
        .path()
        .local_data_dir()
        .map_err(|e| AppError::new("app.data_dir_unavailable").with("reason", e.to_string()))?
        .join("Pierlink"))
}

/// 现在的数据目录
fn current_dir(app: &AppHandle) -> Result<PathBuf, AppError> {
    app.path()
        .app_local_data_dir()
        .map_err(|e| AppError::new("app.data_dir_unavailable").with("reason", e.to_string()))
}

/// 应用数据目录（不存在时由使用方创建）
pub fn data_dir(app: &AppHandle) -> Result<PathBuf, AppError> {
    match DATA_DIR.get() {
        Some(dir) => dir.clone(),
        None => current_dir(app),
    }
}

/// 启动时最先调用（早于打开数据库、启动同步服务）：把旧位置的数据搬到新位置。
/// 搬家失败（已回滚）时这次继续用旧位置，下次启动再试；
/// 需要人工恢复时返回错误，调用方不要打开数据库（否则会在新位置建出一个空数据库）。
pub fn prepare_data_dir(app: &AppHandle) -> Result<(), AppError> {
    let new = current_dir(app)?;
    let old = match legacy_dir(app) {
        Ok(old) => old,
        Err(e) => {
            let _ = DATA_DIR.set(Ok(new));
            return Err(e);
        }
    };
    match pierlink_core::app_data::migrate_data_dir(&old, &new) {
        Ok(report) => {
            if let Some(r) = report {
                eprintln!(
                    "数据目录已搬到新位置: 旧={}, 新={}, 搬运={}, 跳过={}, 改写路径={}",
                    old.display(),
                    new.display(),
                    r.moved.len(),
                    r.skipped.len(),
                    r.rewritten
                );
            }
            let _ = DATA_DIR.set(Ok(new));
            Ok(())
        }
        Err(e) if e.code == "app.data_migration_recovery_required" => {
            eprintln!("数据目录搬家中断且无法自动恢复，暂停使用数据目录: 错误={e:?}");
            let _ = DATA_DIR.set(Err(e.clone()));
            Err(e)
        }
        Err(e) => {
            eprintln!("数据目录搬家失败，已回滚，这次继续使用旧位置: 错误={e:?}");
            let _ = DATA_DIR.set(Ok(old));
            Ok(())
        }
    }
}

/// 数据库状态：启动时打开；打不开（损坏、权限）时保留原因，相关命令返回该错误，应用其他功能照常可用。
/// 损坏时会先尝试从每日备份恢复（L4），恢复了就记下来，界面启动后提示一次。
pub struct DbState {
    db: Mutex<Option<Db>>,
    open_error: Option<AppError>,
    restored: Mutex<Option<storage::Restored>>,
}

/// 每日备份所在目录
fn backup_dir(dir: &std::path::Path) -> PathBuf {
    dir.join("backups")
}

impl DbState {
    /// 打开数据库（损坏时从最新的完好备份恢复）
    ///
    /// :param app: 应用句柄
    /// :return: 数据库状态（打开失败时记录原因）
    pub fn open(app: &AppHandle) -> Self {
        let result = data_dir(app)
            .and_then(|dir| storage::open_or_restore(&dir.join("pierlink.db"), &backup_dir(&dir)));
        match result {
            Ok((db, restored)) => {
                if let Some(r) = &restored {
                    eprintln!(
                        "数据库损坏，已从备份恢复: 备份日期={}, 损坏文件保存为={}",
                        r.backup_date,
                        r.corrupt_saved_as.display()
                    );
                }
                Self {
                    db: Mutex::new(Some(db)),
                    open_error: None,
                    restored: Mutex::new(restored),
                }
            }
            Err(e) => {
                eprintln!("打开数据库失败: 错误={e:?}");
                Self {
                    db: Mutex::new(None),
                    open_error: Some(e),
                    restored: Mutex::new(None),
                }
            }
        }
    }

    /// 不打开数据库，直接记下原因（数据目录需要人工恢复时）
    pub fn failed(error: AppError) -> Self {
        Self {
            db: Mutex::new(None),
            open_error: Some(error),
            restored: Mutex::new(None),
        }
    }

    /// 取出“从备份恢复过”的说明（只取一次，界面提示后就不再提示）
    pub fn take_restored(&self) -> Option<storage::Restored> {
        self.restored
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// 取得数据库进行操作
    pub fn with<T>(&self, f: impl FnOnce(&mut Db) -> Result<T, AppError>) -> Result<T, AppError> {
        let mut guard: MutexGuard<'_, Option<Db>> =
            self.db.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_mut() {
            Some(db) => f(db),
            None => Err(self
                .open_error
                .clone()
                .unwrap_or_else(|| AppError::new("storage.unavailable"))),
        }
    }
}

/// 每日备份：启动时做一次，之后每 6 小时检查一次（当天已有备份就跳过），只保留最新 7 份。失败只记日志。
pub fn start_daily_backup(app: &AppHandle) {
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("数据库备份".into())
        .spawn(move || {
            loop {
                if let Ok(dir) = data_dir(&app) {
                    let state = app.state::<DbState>();
                    if let Err(e) = state.with(|db| db.backup_daily(&backup_dir(&dir))) {
                        eprintln!("数据库备份失败: 错误={e:?}");
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(6 * 3600));
            }
        });
    if let Err(e) = spawned {
        eprintln!("启动数据库备份线程失败: 错误={e}");
    }
}

/// 启动时数据库是否从备份恢复过（界面提示一次）
#[derive(serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct DbRestoredDto {
    /// 备份日期 YYYYMMDD
    pub backup_date: String,
    /// 损坏的数据库保存在哪里
    pub corrupt_saved_as: String,
}

/// 取出“数据库损坏、已从备份恢复”的说明；没有发生过时返回 null。只返回一次。
#[tauri::command]
#[specta::specta]
pub fn app_db_restored(app: AppHandle) -> Option<DbRestoredDto> {
    app.try_state::<DbState>()?
        .take_restored()
        .map(|r| DbRestoredDto {
            backup_date: r.backup_date,
            corrupt_saved_as: r.corrupt_saved_as.display().to_string(),
        })
}

/// 数据目录这次能不能用：搬家中断且需要人工恢复时返回原因（界面启动时提示一次），否则返回 null
#[tauri::command]
#[specta::specta]
pub fn app_data_problem() -> Option<AppError> {
    match DATA_DIR.get() {
        Some(Err(e)) => Some(e.clone()),
        _ => None,
    }
}
