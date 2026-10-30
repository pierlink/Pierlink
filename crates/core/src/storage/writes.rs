//! 实体写入语句。
use super::*;
pub(super) fn insert_servers(conn: &Connection, value: &Server) -> Result<(), AppError> {
    let changed = conn.execute(
        "INSERT INTO servers (id, name, group_name, favorite, sort_order, source, ssh_alias, host, port, username, auth_method, identity_file, jump_server_id, login_shell, use_tmux, remember_password, password_cred, host_key_sha256, vnc_autostart, notes, created_at, updated_at, connection_engine, engine_note, identity_files, identities_only, host_key_alias, command_prediction) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28)",
        rusqlite::params![
            value.id,
            value.name,
            value.group_name,
            value.favorite,
            value.sort_order,
            value.source,
            value.ssh_alias,
            value.host,
            value.port,
            value.username,
            value.auth_method,
            value.identity_file,
            value.jump_server_id,
            value.login_shell,
            value.use_tmux,
            value.remember_password,
            value.password_cred,
            value.host_key_sha256,
            value.vnc_autostart,
            value.notes,
            value.created_at,
            value.updated_at,
            value.connection_engine,
            value.engine_note,
            serde_json::to_string(&value.identity_files).map_err(|_| failure("storage.json", "storage.couldn_t_encode_the_identity_file_list", "身份文件列表编码失败"))?,
            value.identities_only,
            value.host_key_alias,
            value.command_prediction
        ],
    ).map_err(sql_error)?;
    if changed == 0 {
        return Err(failure(
            "storage.not_found",
            "storage.record_does_not_exist",
            "记录不存在",
        ));
    }
    Ok(())
}
pub(super) fn update_servers(conn: &Connection, value: &Server) -> Result<(), AppError> {
    let changed = conn.execute(
        "UPDATE servers SET name=?2, group_name=?3, favorite=?4, sort_order=?5, source=?6, ssh_alias=?7, host=?8, port=?9, username=?10, auth_method=?11, identity_file=?12, jump_server_id=?13, login_shell=?14, use_tmux=?15, remember_password=?16, password_cred=?17, host_key_sha256=?18, vnc_autostart=?19, notes=?20, created_at=?21, updated_at=?22, connection_engine=?23, engine_note=?24, identity_files=?25, identities_only=?26, host_key_alias=?27, command_prediction=?28 WHERE id=?1",
        rusqlite::params![
            value.id,
            value.name,
            value.group_name,
            value.favorite,
            value.sort_order,
            value.source,
            value.ssh_alias,
            value.host,
            value.port,
            value.username,
            value.auth_method,
            value.identity_file,
            value.jump_server_id,
            value.login_shell,
            value.use_tmux,
            value.remember_password,
            value.password_cred,
            value.host_key_sha256,
            value.vnc_autostart,
            value.notes,
            value.created_at,
            value.updated_at,
            value.connection_engine,
            value.engine_note,
            serde_json::to_string(&value.identity_files).map_err(|_| failure("storage.json", "storage.couldn_t_encode_the_identity_file_list", "身份文件列表编码失败"))?,
            value.identities_only,
            value.host_key_alias,
            value.command_prediction
        ],
    ).map_err(sql_error)?;
    if changed == 0 {
        return Err(failure(
            "storage.not_found",
            "storage.record_does_not_exist",
            "记录不存在",
        ));
    }
    Ok(())
}
pub(super) fn insert_sync_tasks(conn: &Connection, value: &SyncTask) -> Result<(), AppError> {
    let changed = conn.execute(
        "INSERT INTO sync_tasks (id, folder_id, name, local_path, conflict_policy, collab_mode, watch_delay_s, versioning_enabled, versioning_days, ignore_rules, editor_id, paused, created_at, updated_at, local_mode) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        rusqlite::params![
            value.id,
            value.folder_id,
            value.name,
            value.local_path,
            value.conflict_policy,
            value.collab_mode,
            value.watch_delay_s,
            value.versioning_enabled,
            value.versioning_days,
            value.ignore_rules,
            value.editor_id,
            value.paused,
            value.created_at,
            value.updated_at,
            value.local_mode
        ],
    ).map_err(sql_error)?;
    if changed == 0 {
        return Err(failure(
            "storage.not_found",
            "storage.record_does_not_exist",
            "记录不存在",
        ));
    }
    Ok(())
}
pub(super) fn update_sync_tasks(conn: &Connection, value: &SyncTask) -> Result<(), AppError> {
    let changed = conn.execute(
        "UPDATE sync_tasks SET folder_id=?2, name=?3, local_path=?4, conflict_policy=?5, collab_mode=?6, watch_delay_s=?7, versioning_enabled=?8, versioning_days=?9, ignore_rules=?10, editor_id=?11, paused=?12, created_at=?13, updated_at=?14, local_mode=?15 WHERE id=?1",
        rusqlite::params![
            value.id,
            value.folder_id,
            value.name,
            value.local_path,
            value.conflict_policy,
            value.collab_mode,
            value.watch_delay_s,
            value.versioning_enabled,
            value.versioning_days,
            value.ignore_rules,
            value.editor_id,
            value.paused,
            value.created_at,
            value.updated_at,
            value.local_mode
        ],
    ).map_err(sql_error)?;
    if changed == 0 {
        return Err(failure(
            "storage.not_found",
            "storage.record_does_not_exist",
            "记录不存在",
        ));
    }
    Ok(())
}
