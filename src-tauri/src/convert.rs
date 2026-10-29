//! 从 ssh config 导入的服务器转换成内置 SSH：先预览（会抄进去的字段、不能转的原因），用户确认后按这份预览执行。
//! 逻辑在 `pierlink_core::ssh::config_file::conversion`；这里只做命令转发和界面用的数据形状。

use crate::data::DbState;
use crate::servers::ServerDto;
use pierlink_core::error::AppError;
use pierlink_core::process::SshProgram;
use pierlink_core::ssh::config_file::conversion::{
    ConversionPreview, ConvertedHost, execute_conversion, preview_conversion,
};
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

/// 每台服务器最近一次预览（执行时用它核对：配置在这期间变了就要重新预览）
#[derive(Default)]
pub struct ConversionState(Mutex<HashMap<String, ConversionPreview>>);

/// 一台主机会抄进服务器记录的连接字段
#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ConvertedHostDto {
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub identity_files: Vec<String>,
    pub identities_only: bool,
    pub host_key_alias: Option<String>,
}

/// 一级跳板；existing_server_id 为空表示转换时新建一台内置的手动服务器
#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ConversionJumpDto {
    pub fields: ConvertedHostDto,
    pub existing_server_id: Option<String>,
}

/// 不能转换的一条原因：code 是固定的原因标识，option 是相关的配置项名，hop 是目标或跳板的别名
#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ConversionReasonDto {
    pub code: String,
    pub option: String,
    pub hop: String,
}

/// 预览结果；reasons 为空才能执行
#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ConversionPreviewDto {
    pub target: ConvertedHostDto,
    /// 从最外层到最内层
    pub jumps: Vec<ConversionJumpDto>,
    pub reasons: Vec<ConversionReasonDto>,
}

fn host(h: &ConvertedHost) -> ConvertedHostDto {
    ConvertedHostDto {
        alias: h.alias.clone(),
        host: h.host.clone(),
        port: h.port,
        username: h.username.clone(),
        identity_files: h.identity_files.clone(),
        identities_only: h.identities_only,
        host_key_alias: h.host_key_alias.clone(),
    }
}

fn dto(p: &ConversionPreview) -> ConversionPreviewDto {
    ConversionPreviewDto {
        target: host(&p.target),
        jumps: p
            .jumps
            .iter()
            .map(|j| ConversionJumpDto {
                fields: host(&j.fields),
                existing_server_id: j.existing_server_id.clone(),
            })
            .collect(),
        reasons: p
            .reasons
            .iter()
            .map(|r| ConversionReasonDto {
                code: r.code.clone(),
                option: r.option.clone(),
                hop: r.hop.clone(),
            })
            .collect(),
    }
}

fn program(app: &AppHandle) -> Result<SshProgram, AppError> {
    Ok(SshProgram {
        path: crate::terminal::find_ssh(app)?,
    })
}

/// 预览把一台从 ssh config 导入的服务器转成内置 SSH：不改任何数据（会用 `ssh -G` 解析它和各级跳板）。
///
/// :param id: 服务器 ID
/// :return: 会抄进去的字段、需要新建的跳板和不能转的原因
#[tauri::command]
#[specta::specta]
pub async fn server_convert_preview(
    app: AppHandle,
    id: String,
) -> Result<ConversionPreviewDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let server = crate::servers::load(&app, &id)?;
        let program = program(&app)?;
        let preview = app
            .state::<DbState>()
            .with(|db| preview_conversion(&program, db, &server))?;
        let result = dto(&preview);
        app.state::<ConversionState>()
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, preview);
        Ok(result)
    })
    .await
    .map_err(|e| AppError::new("ssh.task_failed").with("reason", e.to_string()))?
}

/// 按刚才的预览执行转换：服务器变成应用内添加、用内置 SSH 的服务器，需要的跳板一并建好。
/// 配置在预览之后变了返回 `ssh.conversion_changed`（界面重新预览）；没有预览过返回 `ssh.conversion_changed`。
///
/// :param id: 服务器 ID
/// :return: 转换后的服务器
#[tauri::command]
#[specta::specta]
pub async fn server_convert(app: AppHandle, id: String) -> Result<ServerDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let preview = app
            .state::<ConversionState>()
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id)
            .ok_or_else(|| AppError::new("ssh.conversion_changed"))?;
        let server = crate::servers::load(&app, &id)?;
        let program = program(&app)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let result = app
            .state::<DbState>()
            .with(|db| execute_conversion(&program, db, &server, &preview, now))?;
        // 转换前的连接按旧方式建立，忘掉；下次按内置 SSH 重新连接
        crate::passwords::forget(&app, &id);
        eprintln!(
            "导入的服务器已转换为内置 SSH: 服务器={}, 新建或复用跳板={}",
            result.server.name,
            result.jumps.len()
        );
        Ok(ServerDto::from(result.server))
    })
    .await
    .map_err(|e| AppError::new("ssh.task_failed").with("reason", e.to_string()))?
}
