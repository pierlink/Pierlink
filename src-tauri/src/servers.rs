//! 服务器管理的 Tauri 命令：列表、保存、删除、收藏。校验在 `pierlink_core::server_input`。

use crate::data::DbState;
use pierlink_core::error::AppError;
use pierlink_core::server_input::{ServerInput, build_server};
use pierlink_core::ssh::target::SshTarget;
use pierlink_core::storage::{AuthMethod, Engine, Server, ServerSource};
use serde::Serialize;
use specta::Type;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

/// 给前端的服务器信息：只含界面需要的字段（数据库里的时间戳、排序号、凭据引用等不外传）
#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ServerDto {
    pub id: String,
    pub name: String,
    pub group_name: Option<String>,
    pub favorite: bool,
    pub source: ServerSource,
    pub ssh_alias: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub auth_method: AuthMethod,
    pub identity_file: Option<String>,
    pub login_shell: Option<String>,
    /// 已保存密码（密码本身在 Windows 凭据管理器里，不外传）
    pub has_password: bool,
    pub notes: String,
    /// 用户选的连接方式：内置 SSH / 系统 OpenSSH
    pub connection_engine: Engine,
    /// 为什么用系统 OpenSSH（导入时写的，或内置连不上后改用时写的）
    pub engine_note: Option<String>,
    /// 命令预测：true 跟随全局设置，false 这台服务器关闭（连接时也不装提示符钩子）
    pub command_prediction: bool,
}

impl From<Server> for ServerDto {
    fn from(s: Server) -> Self {
        Self {
            id: s.id,
            name: s.name,
            group_name: s.group_name,
            favorite: s.favorite,
            source: s.source,
            ssh_alias: s.ssh_alias,
            host: s.host,
            port: s.port,
            username: s.username,
            auth_method: s.auth_method,
            identity_file: s.identity_file,
            login_shell: s.login_shell,
            has_password: s.remember_password && s.password_cred.is_some(),
            connection_engine: s.connection_engine,
            engine_note: s.engine_note,
            command_prediction: s.command_prediction,
            notes: s.notes,
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// 读取一台服务器，不存在时返回 `server.not_found`
pub fn load(app: &AppHandle, id: &str) -> Result<Server, AppError> {
    app.state::<DbState>()
        .with(|db| db.get_server(id))?
        .ok_or_else(|| AppError::new("server.not_found"))
}

/// 由服务器记录得到 SSH 目标。跳板机设置在后续阶段接入，目前忽略。
pub fn target(server: &Server) -> Result<SshTarget, AppError> {
    match server.source {
        ServerSource::Manual => Ok(SshTarget::Manual {
            host: server
                .host
                .clone()
                .ok_or_else(|| AppError::new("server.invalid_field").with("field", "host"))?,
            port: server.port.unwrap_or(22),
            user: server.username.clone(),
            identity_file: server.identity_file.as_ref().map(PathBuf::from),
            jump: None,
        }),
        ServerSource::SshConfig => Ok(SshTarget::Alias {
            alias: server
                .ssh_alias
                .clone()
                .ok_or_else(|| AppError::new("server.invalid_field").with("field", "sshAlias"))?,
        }),
    }
}

/// 列出全部服务器
#[tauri::command]
#[specta::specta]
pub async fn server_list(app: AppHandle) -> Result<Vec<ServerDto>, AppError> {
    let list = app.state::<DbState>().with(|db| db.list_servers())?;
    Ok(list.into_iter().map(ServerDto::from).collect())
}

/// 服务器密码在凭据管理器里的引用名（与数据库约定一致）
pub fn password_ref(server_id: &str) -> String {
    format!("pierlink/server/{server_id}/password")
}

/// 新建或修改服务器；返回保存后的记录。
///
/// :param password: 密码登录时要保存的密码（只存进 Windows 凭据管理器）；None 或空表示不修改
/// :param clear_password: 清除已保存的密码
#[tauri::command]
#[specta::specta]
pub async fn server_save(
    app: AppHandle,
    input: ServerInput,
    password: Option<String>,
    clear_password: bool,
) -> Result<ServerDto, AppError> {
    let password = password.filter(|p| !p.is_empty());
    if password
        .as_deref()
        .is_some_and(|p| p.len() > 1024 || p.contains(['\n', '\r', '\0']))
    {
        return Err(AppError::new("server.invalid_field").with("field", "password"));
    }
    let mut credentials_changed = password.is_some() || clear_password;
    // 连接方式或连接参数变了：旧连接不能再复用
    let mut connection_changed = false;
    let saved = app.state::<DbState>().with(|db| {
        let existing = match &input.id {
            Some(id) => Some(
                db.get_server(id)?
                    .ok_or_else(|| AppError::new("server.not_found"))?,
            ),
            None => None,
        };
        let mut server = build_server(
            &input,
            existing.as_ref(),
            &uuid::Uuid::new_v4().to_string(),
            now_ms(),
        )?;
        if server.auth_method == AuthMethod::Password {
            if password.is_some() {
                // 自动填密码要严格匹配“用户@主机”的提示，没有用户名就无法安全地自动填
                if server.username.is_none() {
                    return Err(AppError::new("server.invalid_field").with("field", "passwordUser"));
                }
                server.remember_password = true;
                server.password_cred = Some(password_ref(&server.id));
            } else if clear_password {
                server.remember_password = false;
                server.password_cred = None;
            }
        }
        // 地址、用户名、登录方式变了：内存里的旧密码不一定还对，忘掉
        if let Some(e) = &existing
            && (e.host != server.host
                || e.port != server.port
                || e.username != server.username
                || e.auth_method != server.auth_method)
        {
            credentials_changed = true;
            connection_changed = true;
        }
        if let Some(e) = &existing
            && (e.connection_engine != server.connection_engine
                || e.identity_file != server.identity_file
                || e.jump_server_id != server.jump_server_id)
        {
            connection_changed = true;
        }
        if existing.is_some() {
            db.update_server(&server)?;
        } else {
            db.insert_server(&server)?;
        }
        Ok(server)
    })?;
    if connection_changed {
        crate::builtin::connection_changed(&app, &saved.id).await;
    }
    let store = crate::sync::credential_store()?;
    let key = password_ref(&saved.id);
    if let Some(p) = password.filter(|_| saved.remember_password) {
        if let Err(e) = store.set(&key, &p) {
            // 密码没存进去：数据库也改回“没有保存密码”，不能留下指向空条目的引用
            let mut s = saved.clone();
            s.remember_password = false;
            s.password_cred = None;
            let _ = app.state::<DbState>().with(|db| db.update_server(&s));
            return Err(e);
        }
    } else if !saved.remember_password {
        // 清除了密码，或改成了别的登录方式：凭据管理器里的旧密码一并删掉
        let _ = store.delete(&key);
    }
    if credentials_changed {
        // 下次需要时重新从刚保存的密码读，或重新问
        crate::passwords::forget(&app, &saved.id);
    }
    Ok(ServerDto::from(saved))
}

/// 删除服务器。还有同步任务用到它时返回 `storage.server_in_use`（附任务名列表），由界面让用户先处理任务。
#[tauri::command]
#[specta::specta]
pub async fn server_delete(app: AppHandle, id: String) -> Result<(), AppError> {
    // 先记下这台服务器的端口转发：数据库删除会级联删掉记录，隧道却还在运行
    let forwards = app
        .state::<DbState>()
        .with(|db| db.list_port_forwards(Some(&id)))
        .unwrap_or_default();
    app.state::<DbState>().with(|db| db.delete_server(&id))?;
    for f in forwards {
        crate::forwards::stop(&app, &f.id);
    }
    crate::monitor::stop_server(&app, &id);
    // 内置连接不等空闲回收，现在就关
    crate::builtin::close_server(&app, &id).await;
    crate::passwords::forget(&app, &id);
    // 一并清理凭据管理器里这台服务器的同步组件凭据（没有部署过时不存在，忽略）
    if let Ok(store) = crate::sync::credential_store() {
        for purpose in ["apikey", "gui-password"] {
            let _ = store.delete(&format!("pierlink/instance/{id}/{purpose}"));
        }
        let _ = store.delete(&password_ref(&id));
        let _ = store.delete(&crate::desktop::passwords_ref(&id));
    }
    Ok(())
}

/// 收藏或取消收藏
#[tauri::command]
#[specta::specta]
pub async fn server_set_favorite(
    app: AppHandle,
    id: String,
    favorite: bool,
) -> Result<(), AppError> {
    app.state::<DbState>().with(|db| {
        let mut server = db
            .get_server(&id)?
            .ok_or_else(|| AppError::new("server.not_found"))?;
        server.favorite = favorite;
        server.updated_at = now_ms();
        db.update_server(&server)
    })
}

/// 修改导入自 ssh config 的服务器在应用里的设置（连接参数以 ssh config 为准，不能在这里改）。
///
/// :param group_name: 分组，空为不分组（最长 64 字符）
/// :param notes: 备注（最长 4000 字符）
#[tauri::command]
#[specta::specta]
pub async fn server_update_link(
    app: AppHandle,
    id: String,
    group_name: Option<String>,
    notes: String,
) -> Result<ServerDto, AppError> {
    let group_name = group_name
        .map(|g| g.trim().to_owned())
        .filter(|g| !g.is_empty());
    if group_name
        .as_deref()
        .is_some_and(|g| g.chars().count() > 64 || g.chars().any(char::is_control))
    {
        return Err(AppError::new("server.invalid_field").with("field", "groupName"));
    }
    if notes.chars().count() > 4000 {
        return Err(AppError::new("server.invalid_field").with("field", "notes"));
    }
    app.state::<DbState>().with(|db| {
        let mut server = db
            .get_server(&id)?
            .ok_or_else(|| AppError::new("server.not_found"))?;
        if server.source != ServerSource::SshConfig {
            return Err(AppError::new("server.not_link"));
        }
        server.group_name = group_name;
        server.notes = notes;
        server.updated_at = now_ms();
        db.update_server(&server)?;
        Ok(ServerDto::from(server))
    })
}

/// 只改一台应用内添加的服务器的连接方式（内置 SSH 连不上时“改用系统 OpenSSH 重试”用）。
///
/// :param id: 服务器 ID
/// :param engine: 新的连接方式
/// :param note: 改用的原因，显示在服务器设置里（最长 512 字符）；None 或空表示清掉原来的说明
/// :return: 保存后的服务器；从 ssh config 导入的服务器返回 `server.not_editable`
#[tauri::command]
#[specta::specta]
pub async fn server_set_engine(
    app: AppHandle,
    id: String,
    engine: Engine,
    note: Option<String>,
) -> Result<ServerDto, AppError> {
    let note = note
        .map(|n| n.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|n| !n.is_empty());
    if note.as_deref().is_some_and(|n| n.chars().count() > 512) {
        return Err(AppError::new("server.invalid_field").with("field", "engineNote"));
    }
    let (changed, dto) = app.state::<DbState>().with(|db| {
        let mut server = db
            .get_server(&id)?
            .ok_or_else(|| AppError::new("server.not_found"))?;
        if server.source != ServerSource::Manual {
            return Err(AppError::new("server.not_editable"));
        }
        let changed = server.connection_engine != engine;
        server.connection_engine = engine;
        server.engine_note = note;
        server.updated_at = now_ms();
        db.update_server(&server)?;
        Ok((changed, ServerDto::from(server)))
    })?;
    if changed {
        crate::builtin::connection_changed(&app, &id).await;
    }
    Ok(dto)
}

/// 打开或关闭这台服务器的命令预测；关闭后下次连接也不装提示符钩子。各种来源的服务器都可以设
///
/// :param id: 服务器 id
/// :param enabled: true 跟随全局设置，false 关闭
/// :return: 保存后的服务器；不存在时返回 `server.not_found`
#[tauri::command]
#[specta::specta]
pub async fn server_set_command_prediction(
    app: AppHandle,
    id: String,
    enabled: bool,
) -> Result<ServerDto, AppError> {
    let dto = app.state::<DbState>().with(|db| {
        let mut server = db
            .get_server(&id)?
            .ok_or_else(|| AppError::new("server.not_found"))?;
        server.command_prediction = enabled;
        server.updated_at = now_ms();
        db.update_server(&server)?;
        Ok(ServerDto::from(server))
    })?;
    eprintln!("设置命令预测: 服务器={id}, 开启={enabled}");
    Ok(dto)
}

/// 导入自 ssh config 的服务器当前的实际连接参数（`ssh -G` 解析，只读显示）
#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedDto {
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub identity_files: Vec<String>,
    pub proxy_jump: Option<String>,
    pub via_proxy_command: bool,
    pub has_forwards: bool,
    pub host_key_check_weakened: bool,
}

/// 解析导入服务器的别名；别名已从 ssh config 删掉或解析失败时返回错误。
#[tauri::command]
#[specta::specta]
pub async fn server_resolve(app: AppHandle, id: String) -> Result<ResolvedDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let server = load(&app, &id)?;
        let alias = match (&server.source, &server.ssh_alias) {
            (ServerSource::SshConfig, Some(a)) => a.clone(),
            _ => return Err(AppError::new("server.not_link")),
        };
        let program = pierlink_core::process::SshProgram {
            path: crate::terminal::find_ssh(&app)?,
        };
        let cfg = pierlink_core::ssh::config_file::resolve_alias(
            &program,
            &alias,
            std::time::Duration::from_secs(5),
        )?;
        Ok(ResolvedDto {
            host_key_check_weakened: pierlink_core::ssh::config::host_key_check_weakened(&cfg),
            hostname: cfg.hostname,
            user: cfg.user,
            port: cfg.port,
            identity_files: cfg.identity_files,
            proxy_jump: cfg.proxy_jump,
            via_proxy_command: cfg.proxy_command.is_some(),
            has_forwards: cfg.has_forwards,
        })
    })
    .await
    .map_err(|e| AppError::new("server.task_failed").with("reason", e.to_string()))?
}
