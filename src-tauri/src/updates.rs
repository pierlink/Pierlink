//! 自动更新：从公开仓库的 GitHub Releases 检查新版本，下载并校验签名后安装。
//! Windows 上安装程序启动前会先正常关闭同步、隧道、终端等（和托盘“退出”一样），否则安装程序替换不了正在用的文件。

use pierlink_core::error::AppError;
use serde::Serialize;
use specta::Type;
use tauri::AppHandle;
use tauri::ipc::Channel;
use tauri_plugin_updater::UpdaterExt;

/// 有新版本时给界面的信息
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfoDto {
    pub version: String,
    pub current_version: String,
    /// 更新说明（发版时写的）
    pub notes: Option<String>,
    pub date: Option<String>,
}

/// 下载安装进度
#[derive(Serialize, Type, Clone)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UpdateProgressDto {
    #[serde(rename_all = "camelCase")]
    Downloading { downloaded: f64, total: Option<f64> },
    /// 下载完、签名校验通过，马上关闭 Pierlink 并运行安装程序
    Installing,
}

/// 记下出错原因（日志），给界面只返回错误码和原因
fn fail(code: &str, e: impl std::fmt::Display) -> AppError {
    let reason = e.to_string();
    eprintln!("自动更新出错: 步骤={code}, 原因={reason}");
    AppError::new(code).with("reason", reason)
}

/// 检查是否有新版本；没有返回 null
#[tauri::command]
#[specta::specta]
pub async fn update_check(app: AppHandle) -> Result<Option<UpdateInfoDto>, AppError> {
    let updater = app.updater().map_err(|e| fail("update.check_failed", e))?;
    let update = updater
        .check()
        .await
        .map_err(|e| fail("update.check_failed", e))?;
    if let Some(u) = &update {
        eprintln!("发现新版本: 当前={}, 新={}", u.current_version, u.version);
    }
    Ok(update.map(|u| UpdateInfoDto {
        version: u.version,
        current_version: u.current_version,
        notes: u.body,
        date: u.date.map(|d| d.date().to_string()),
    }))
}

/// 下载、校验签名并安装新版本。Windows 上成功启动安装程序后 Pierlink 会退出，本命令不会返回。
#[tauri::command]
#[specta::specta]
pub async fn update_install(
    app: AppHandle,
    on_progress: Channel<UpdateProgressDto>,
) -> Result<(), AppError> {
    let exit_app = app.clone();
    let updater = app
        .updater_builder()
        .on_before_exit(move || {
            eprintln!("即将运行安装程序：先关闭同步、隧道和终端");
            // 这里在异步工作线程上，关闭要同步等几秒：让运行时先把这个线程上的其他任务挪走
            tokio::task::block_in_place(|| crate::shutdown_all(&exit_app));
        })
        .build()
        .map_err(|e| fail("update.install_failed", e))?;
    let Some(update) = updater
        .check()
        .await
        .map_err(|e| fail("update.check_failed", e))?
    else {
        return Err(AppError::new("update.none"));
    };
    eprintln!("开始下载更新: 版本={}", update.version);
    let progress = on_progress.clone();
    let mut downloaded = 0f64;
    update
        .download_and_install(
            move |chunk, total| {
                downloaded += chunk as f64;
                let _ = progress.send(UpdateProgressDto::Downloading {
                    downloaded,
                    total: total.map(|t| t as f64),
                });
            },
            move || {
                let _ = on_progress.send(UpdateProgressDto::Installing);
            },
        )
        .await
        .map_err(|e| fail("update.install_failed", e))
}
