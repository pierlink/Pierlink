//! 导入的私钥（设置 → 私钥；服务器的“指定私钥”从这里选）：列出、导入、改名、删除、保存或忘掉口令、选私钥文件。
//! 核心逻辑在 `pierlink_core::keys::imported`；口令只在凭据管理器里，不写日志。
use crate::data::DbState;
use crate::keys::imported_dir;
use crate::sync::credential_store;
use pierlink_core::error::AppError;
use pierlink_core::keys::imported::{self, ImportedKey};
use pierlink_core::security::vault::Zeroizing;
use pierlink_core::storage::{AuthMethod, Server};
use serde::Serialize;
use specta::Type;
use std::path::Path;
use tauri::{AppHandle, Manager};

/// 一把导入的私钥（给界面）
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportedKeyDto {
    pub id: String,
    pub name: String,
    /// 算法，如 ssh-ed25519、ssh-rsa
    pub algorithm: String,
    /// 公钥指纹（SHA256:…）
    pub fingerprint: String,
    /// 私钥里带的注释（可能为空）
    pub comment: String,
    /// 原文件有口令（用户自己的口令；没有的话口令是随机生成、一定存着）
    pub user_passphrase: bool,
    /// 口令存在凭据管理器里，连接时自动填
    pub passphrase_saved: bool,
    /// 存着的私钥的完整路径：用它的服务器“私钥文件”填这个
    pub path: String,
    /// 用这把私钥的服务器名
    pub used_by: Vec<String>,
}

async fn blocking<T, F>(app: AppHandle, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&AppHandle) -> Result<T, AppError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || f(&app))
        .await
        .map_err(|e| AppError::new("keys.task_failed").with("reason", e.to_string()))?
}

/// 用这把私钥（按路径认）的服务器名
fn users_of(dir: &Path, servers: &[Server], id: &str) -> Vec<String> {
    servers
        .iter()
        .filter(|s| s.auth_method == AuthMethod::Key)
        .filter(|s| {
            s.identity_file
                .as_deref()
                .and_then(|p| imported::id_of_path(dir, Path::new(p)))
                .is_some_and(|x| x == id)
        })
        .map(|s| s.name.clone())
        .collect()
}

fn to_dto(dir: &Path, servers: &[Server], key: ImportedKey) -> ImportedKeyDto {
    let used_by = users_of(dir, servers, &key.id);
    ImportedKeyDto {
        id: key.id,
        name: key.name,
        algorithm: key.algorithm,
        fingerprint: key.fingerprint,
        comment: key.comment,
        user_passphrase: key.user_passphrase,
        passphrase_saved: key.passphrase_saved,
        path: key.path.to_string_lossy().into_owned(),
        used_by,
    }
}

fn servers(app: &AppHandle) -> Result<Vec<Server>, AppError> {
    app.state::<DbState>().with(|db| db.list_servers())
}

/// 列出导入的私钥（按导入时间），带着哪些服务器在用。
///
/// :return: 私钥列表
#[tauri::command]
#[specta::specta]
pub async fn keys_imported_list(app: AppHandle) -> Result<Vec<ImportedKeyDto>, AppError> {
    blocking(app, |app| {
        let dir = imported_dir(app)?;
        let store = credential_store()?;
        let list = imported::list(&dir, store.as_ref())?;
        let servers = servers(app)?;
        Ok(list
            .into_iter()
            .map(|k| to_dto(&dir, &servers, k))
            .collect())
    })
    .await
}

/// 导入私钥：选好的文件读出来（有口令要给口令），加密后存进 Pierlink；原文件不动。
///
/// :param path: 私钥文件的完整路径
/// :param name: 显示的名字（空时用文件名）
/// :param passphrase: 原文件的口令（没有口令时为 null）
/// :param save_passphrase: 原文件有口令时，要不要存进凭据管理器（连接时自动填）
/// :return: 导入好的私钥
#[tauri::command]
#[specta::specta]
pub async fn keys_import(
    app: AppHandle,
    path: String,
    name: String,
    passphrase: Option<String>,
    save_passphrase: bool,
) -> Result<ImportedKeyDto, AppError> {
    let passphrase = passphrase.map(Zeroizing::new);
    blocking(app, move |app| {
        let dir = imported_dir(app)?;
        let store = credential_store()?;
        let key = imported::import(
            &dir,
            Path::new(&path),
            &name,
            passphrase.as_deref().map(String::as_str),
            save_passphrase,
            store.as_ref(),
        )?;
        eprintln!(
            "导入私钥: 名字={}, 算法={}, 指纹={}",
            key.name, key.algorithm, key.fingerprint
        );
        Ok(to_dto(&dir, &servers(app)?, key))
    })
    .await
}

/// 给导入的私钥改名。
///
/// :param id: 私钥编号
/// :param name: 新名字
/// :return: 改好的私钥
#[tauri::command]
#[specta::specta]
pub async fn keys_imported_rename(
    app: AppHandle,
    id: String,
    name: String,
) -> Result<ImportedKeyDto, AppError> {
    blocking(app, move |app| {
        let dir = imported_dir(app)?;
        let store = credential_store()?;
        let key = imported::rename(&dir, &id, &name, store.as_ref())?;
        Ok(to_dto(&dir, &servers(app)?, key))
    })
    .await
}

/// 删掉导入的私钥和存着的口令。还有服务器在用时不删，报 keys.imported_in_use（带服务器名）。
///
/// :param id: 私钥编号
#[tauri::command]
#[specta::specta]
pub async fn keys_imported_remove(app: AppHandle, id: String) -> Result<(), AppError> {
    blocking(app, move |app| {
        let dir = imported_dir(app)?;
        let users = users_of(&dir, &servers(app)?, &id);
        if !users.is_empty() {
            return Err(AppError::new("keys.imported_in_use").with("servers", users.join("、")));
        }
        imported::remove(&dir, &id, credential_store()?.as_ref())?;
        eprintln!("删除导入的私钥: 编号={id}");
        Ok(())
    })
    .await
}

/// 保存或忘掉导入私钥的口令（只对原文件有口令的私钥）。保存前先核对口令能解开私钥。
///
/// :param id: 私钥编号
/// :param passphrase: 要保存的口令；null 表示忘掉（以后连接时问）
#[tauri::command]
#[specta::specta]
pub async fn keys_imported_passphrase(
    app: AppHandle,
    id: String,
    passphrase: Option<String>,
) -> Result<(), AppError> {
    let passphrase = passphrase.map(Zeroizing::new);
    blocking(app, move |app| {
        let dir = imported_dir(app)?;
        imported::set_passphrase(
            &dir,
            &id,
            passphrase.as_deref().map(String::as_str),
            credential_store()?.as_ref(),
        )?;
        eprintln!("导入私钥的口令: 编号={id}, 保存={}", passphrase.is_some());
        Ok(())
    })
    .await
}

/// 弹出系统的“打开文件”框选私钥文件（从用户的 .ssh 文件夹开始）。
///
/// :return: 选中的文件完整路径；取消时为 null
#[tauri::command]
#[specta::specta]
pub async fn keys_pick_file(app: AppHandle) -> Result<Option<String>, AppError> {
    use tauri_plugin_dialog::DialogExt;
    let start = app
        .path()
        .home_dir()
        .ok()
        .map(|h| h.join(".ssh"))
        .filter(|d| d.is_dir());
    tauri::async_runtime::spawn_blocking(move || {
        let mut dialog = app.dialog().file();
        if let Some(dir) = start {
            dialog = dialog.set_directory(dir);
        }
        dialog
            .blocking_pick_file()
            .map(|p| {
                p.into_path()
                    .map(|p| p.to_string_lossy().into_owned())
                    .map_err(|e| AppError::new("files.pick_failed").with("reason", e.to_string()))
            })
            .transpose()
    })
    .await
    .map_err(|e| AppError::new("files.pick_failed").with("reason", e.to_string()))?
}
