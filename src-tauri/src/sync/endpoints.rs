//! 同一个本机目录同步到多台服务器：给已有任务添加服务器、移除其中一台。
use super::{
    DbState, Inner, data_dir, drop_unused_links, endpoint_mode, ensure_link, ensure_local,
    find_ssh, folder_kind, load_task, now_ms, with_sync,
};
use pierlink_core::error::AppError;
use pierlink_core::process::SshProgram;
use pierlink_core::ssh::args::{Purpose, build_args};
use pierlink_core::storage::SyncEndpoint;
use pierlink_core::sync;
use tauri::{AppHandle, Manager};

/// 在服务器上把 remote_path 解析成真实路径（跟随符号链接），和新建任务时一样
fn canonical_remote(
    app: &AppHandle,
    inner: &Inner,
    server_id: &str,
    remote_path: &str,
) -> Result<String, AppError> {
    let link = inner
        .links
        .get(server_id)
        .ok_or_else(|| AppError::new("sync.not_connected"))?;
    let server = crate::servers::load(app, server_id)?;
    let access = crate::keys::Access::User("添加同步服务器");
    let cmd = crate::builtin::background(app, &server, access, || {
        build_args(
            &crate::servers::target(&server)?,
            &Purpose::Background,
            &crate::keys::ssh_opts(app, &server, access)?,
        )
    })?;
    let (info, _) = sync::check_remote_path(
        &SshProgram {
            path: find_ssh(app)?,
        },
        &cmd,
        remote_path,
        &[],
        &link.folder_paths()?,
    )?;
    Ok(info.canonical_path)
}

/// 给已有的同步任务再加一台服务器：本机目录、排除规则和设置沿用任务的。
/// 服务器上先暂停创建；start 为 true 且任务没暂停时随后开始同步。
///
/// :param task_id: 任务 ID
/// :param server_id: 要添加的服务器（已部署同步组件）
/// :param remote_path: 服务器上的目录
/// :param mode: 服务器一侧的方式：sendReceive / receiveOnly
/// :param start: 添加后是否开始同步这台服务器
#[tauri::command]
#[specta::specta]
pub async fn sync_add_endpoint(
    app: AppHandle,
    task_id: String,
    server_id: String,
    remote_path: String,
    mode: String,
    start: bool,
) -> Result<(), AppError> {
    with_sync(app, move |app, inner| {
        let (mut task, mut endpoints) = load_task(app, &task_id)?;
        if endpoints.iter().any(|e| e.server_id == server_id) {
            return Err(AppError::new("sync.server_already_in_task"));
        }
        ensure_link(app, inner, &server_id)?;
        ensure_local(app, inner)?;
        let path = canonical_remote(app, inner, &server_id, &remote_path)?;
        let kind = folder_kind(&mode);
        let local = inner
            .local
            .as_ref()
            .ok_or_else(|| AppError::new("sync.local_unavailable"))?;
        let link = inner
            .links
            .get(&server_id)
            .ok_or_else(|| AppError::new("sync.not_connected"))?;
        sync::add_endpoint(local, link, &task.folder_id, &path, kind, &data_dir(app)?)?;
        endpoints.push(SyncEndpoint {
            task_id: task.id.clone(),
            server_id: server_id.clone(),
            remote_path: path.clone(),
            mode: endpoint_mode(kind),
        });
        if let Err(e) = app
            .state::<DbState>()
            .with(|db| db.set_endpoints(&task.id, &endpoints))
        {
            // 数据库写失败：撤销刚加的（不删文件），免得出现应用里看不到的同步
            let _ = sync::remove_endpoint(local, link, &task.folder_id);
            drop_unused_links(app, inner);
            return Err(e);
        }
        eprintln!(
            "给同步任务添加服务器: 任务={}, 服务器={server_id}, 目录={path}",
            task.name
        );
        if start && !task.paused {
            // 已加好（服务器一侧暂停中）；开始失败时单独报告，由用户在同步页点“继续”
            if let Err(e) = sync::set_paused(local, &[link], &task.folder_id, false) {
                task.paused = true;
                task.updated_at = now_ms();
                let _ = app.state::<DbState>().with(|db| db.update_sync_task(&task));
                return Err(AppError::new("sync.created_not_started").with("reason", e.code));
            }
        }
        Ok(())
    })
    .await
}

/// 从任务里移除一台服务器：这台服务器不再和本机同步，两边的文件都保留。至少要留一台
///
/// :param task_id: 任务 ID
/// :param server_id: 要移除的服务器
#[tauri::command]
#[specta::specta]
pub async fn sync_remove_endpoint(
    app: AppHandle,
    task_id: String,
    server_id: String,
) -> Result<(), AppError> {
    with_sync(app, move |app, inner| {
        let (task, endpoints) = load_task(app, &task_id)?;
        let rest: Vec<SyncEndpoint> = endpoints
            .iter()
            .filter(|e| e.server_id != server_id)
            .cloned()
            .collect();
        if rest.len() == endpoints.len() {
            return Err(AppError::new("sync.server_not_in_task"));
        }
        if rest.is_empty() {
            return Err(AppError::new("sync.last_server"));
        }
        ensure_link(app, inner, &server_id)?;
        ensure_local(app, inner)?;
        let local = inner
            .local
            .as_ref()
            .ok_or_else(|| AppError::new("sync.local_unavailable"))?;
        let link = inner
            .links
            .get(&server_id)
            .ok_or_else(|| AppError::new("sync.not_connected"))?;
        sync::remove_endpoint(local, link, &task.folder_id)?;
        app.state::<DbState>()
            .with(|db| db.set_endpoints(&task.id, &rest))?;
        drop_unused_links(app, inner);
        eprintln!(
            "从同步任务移除服务器: 任务={}, 服务器={server_id}",
            task.name
        );
        Ok(())
    })
    .await
}
