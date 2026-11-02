//! 自动更新：从公开仓库的 GitHub Releases 检查新版本，下载并校验签名后安装。
//! Windows 上安装程序启动前会先正常关闭同步、隧道、终端等（和托盘“退出”一样），否则安装程序替换不了正在用的文件。

use pierlink_core::error::AppError;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tauri::ipc::Channel;
use tauri_plugin_updater::UpdaterExt;

/// 正在进行的更新下载（编号和用来停掉它的句柄）：同一时间只有一个。再点“立即更新”时先停掉上一个，
/// 否则两个下载会同时往进度条上报（网络断了第一个卡住，恢复后两个一起走，进度来回跳）
static CURRENT: Mutex<Option<(u64, tokio::task::AbortHandle)>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// 多久没收到数据算下载卡住：网络断了但连接没断开时，下载会一直等下去
const STALL: Duration = Duration::from_secs(30);

/// 换成编号 id 的这个下载，停掉上一个还在进行的
fn take_over(id: u64, handle: tokio::task::AbortHandle) {
    let mut current = CURRENT.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((old, previous)) = current.replace((id, handle)) {
        eprintln!("停掉上一个还在进行的更新下载: 编号={old}");
        previous.abort();
    }
}

/// 编号 id 的下载结束了：还记着的是它就清掉
fn release(id: u64) {
    let mut current = CURRENT.lock().unwrap_or_else(|p| p.into_inner());
    if current.as_ref().is_some_and(|(c, _)| *c == id) {
        *current = None;
    }
}

/// 有新版本时给界面的信息
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfoDto {
    pub version: String,
    pub current_version: String,
    /// 更新说明（发版时写的）
    pub notes: Option<String>,
    pub date: Option<String>,
    /// 这个版本已经下载好、核对过签名（点“安装更新”就能装，不用再下载）
    pub downloaded: bool,
}

/// 下载进度
#[derive(Serialize, Type, Clone)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UpdateProgressDto {
    #[serde(rename_all = "camelCase")]
    Downloading { downloaded: f64, total: Option<f64> },
}

/// 下载好、核对过签名的安装包存在数据目录 updates/ 里，用户点“安装更新”时才装（重启后也还在）。
/// 只留一个：pending.bin 是安装包，pending.json 记它的版本号和大小。只装和服务器上最新版本一样的那个：
/// 下好以后又出了新版本，检查和安装时都会删掉旧包、让用户重新下载
#[derive(Serialize, Deserialize)]
struct PendingMeta {
    version: String,
    size: u64,
}

fn pending_paths(app: &AppHandle) -> Result<(PathBuf, PathBuf), AppError> {
    let dir = crate::data::data_dir(app)?.join("updates");
    Ok((dir.join("pending.bin"), dir.join("pending.json")))
}

/// 下载好的安装包的版本号；没有、读不懂或安装包大小对不上时为 None
fn pending_version(app: &AppHandle) -> Option<String> {
    let (bin, meta) = pending_paths(app).ok()?;
    let meta: PendingMeta = serde_json::from_slice(&std::fs::read(meta).ok()?).ok()?;
    (std::fs::metadata(bin).ok()?.len() == meta.size).then_some(meta.version)
}

/// 删掉下载好的安装包（不是最新版本了，或者已经装过了）
fn clear_pending(app: &AppHandle) {
    let Ok((bin, meta)) = pending_paths(app) else {
        return;
    };
    if bin.exists() || meta.exists() {
        let _ = std::fs::remove_file(&meta);
        let _ = std::fs::remove_file(&bin);
        eprintln!("删除下载好的旧安装包: 路径={}", bin.display());
    }
}

/// 存下载好的安装包：先写临时文件再改名（写到一半断电不会留下坏包），最后写版本号
fn save_pending(app: &AppHandle, version: &str, bytes: &[u8]) -> Result<(), AppError> {
    clear_pending(app);
    let (bin, meta) = pending_paths(app)?;
    let write = || -> std::io::Result<()> {
        if let Some(dir) = bin.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = bin.with_extension("part");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &bin)?;
        let info = PendingMeta {
            version: version.to_owned(),
            size: bytes.len() as u64,
        };
        std::fs::write(&meta, serde_json::to_vec(&info).unwrap_or_default())
    };
    write().map_err(|e| fail("update.install_failed", e))
}

/// 读出 version 这个版本的安装包；没有或版本不一样时为 None
fn load_pending(app: &AppHandle, version: &str) -> Option<Vec<u8>> {
    if pending_version(app)? != version {
        return None;
    }
    std::fs::read(pending_paths(app).ok()?.0).ok()
}

/// 记下出错原因（日志），给界面只返回错误码和原因
fn fail(code: &str, e: impl std::fmt::Display) -> AppError {
    let reason = e.to_string();
    eprintln!("自动更新出错: 步骤={code}, 原因={reason}");
    AppError::new(code).with("reason", reason)
}

/// 更新走的代理：按代理设置，github.com 不在不走代理的名单里时才用（新版本的说明和安装包都从 GitHub 下载）
fn update_proxy() -> Option<tauri::Url> {
    let proxy = pierlink_core::proxy::for_target("github.com")?;
    tauri::Url::parse(&pierlink_core::proxy::url(&proxy)).ok()
}

/// 检查是否有新版本；没有返回 null
#[tauri::command]
#[specta::specta]
pub async fn update_check(app: AppHandle) -> Result<Option<UpdateInfoDto>, AppError> {
    let mut builder = app.updater_builder();
    if let Some(proxy) = update_proxy() {
        builder = builder.proxy(proxy);
    }
    let updater = builder
        .build()
        .map_err(|e| fail("update.check_failed", e))?;
    let update = updater
        .check()
        .await
        .map_err(|e| fail("update.check_failed", e))?;
    if let Some(u) = &update {
        eprintln!("发现新版本: 当前={}, 新={}", u.current_version, u.version);
    }
    // 下载好的安装包只认服务器上最新的这个版本：没有新版本（已经装过了）或出了更新的版本时删掉
    let downloaded = match (&update, pending_version(&app)) {
        (Some(u), Some(v)) if v == u.version => true,
        (_, Some(_)) => {
            clear_pending(&app);
            false
        }
        _ => false,
    };
    Ok(update.map(|u| UpdateInfoDto {
        version: u.version,
        current_version: u.current_version,
        notes: u.body,
        date: u.date.map(|d| d.date().to_string()),
        downloaded,
    }))
}

/// 下载新版本并核对签名，留着等用户点“安装更新”（不自动安装）；返回下载好的版本号。
/// 同一时间只下载一个（再调用时停掉上一个，上一个返回 update.superseded）；30 秒没收到数据算卡住，
/// 停掉并返回 update.stalled（界面上可以重试）。
#[tauri::command]
#[specta::specta]
pub async fn update_download(
    app: AppHandle,
    on_progress: Channel<UpdateProgressDto>,
) -> Result<String, AppError> {
    let exit_app = app.clone();
    let mut builder = app.updater_builder();
    if let Some(proxy) = update_proxy() {
        builder = builder.proxy(proxy);
    }
    let updater = builder
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
    let version = update.version.clone();
    // 这个版本已经下载好了（上次下完没装）：不用再下
    if pending_version(&app).as_deref() == Some(version.as_str()) {
        eprintln!("更新已经下载好: 版本={version}");
        return Ok(version);
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    eprintln!("开始下载更新: 版本={version}, 编号={id}");
    let started = Instant::now();
    // 最后一次收到数据的时间（从开始下载算，毫秒）；下载完、核对签名时不再看
    let last = Arc::new(AtomicU64::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    let (seen, done) = (last.clone(), finished.clone());
    let mut downloaded = 0f64;
    let mut task = tokio::spawn(async move {
        let bytes = update
            .download(
                move |chunk, total| {
                    seen.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    downloaded += chunk as f64;
                    let _ = on_progress.send(UpdateProgressDto::Downloading {
                        downloaded,
                        total: total.map(|t| t as f64),
                    });
                },
                move || done.store(true, Ordering::Relaxed),
            )
            .await?;
        Ok::<_, tauri_plugin_updater::Error>((update, bytes))
    });
    take_over(id, task.abort_handle());
    let result = loop {
        match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
            Ok(joined) => break joined,
            Err(_) => {
                let idle = started
                    .elapsed()
                    .saturating_sub(Duration::from_millis(last.load(Ordering::Relaxed)));
                if !finished.load(Ordering::Relaxed) && idle >= STALL {
                    task.abort();
                    release(id);
                    return Err(fail(
                        "update.stalled",
                        format!("{} 秒没收到数据", idle.as_secs()),
                    ));
                }
            }
        }
    };
    release(id);
    let (_, bytes) = match result {
        Ok(done) => done.map_err(|e| fail("update.install_failed", e))?,
        // 又点了一次下载：这一个被新的换掉了
        Err(e) if e.is_cancelled() => return Err(AppError::new("update.superseded")),
        Err(e) => return Err(fail("update.install_failed", e)),
    };
    eprintln!("更新已下载并核对签名: 版本={version}, 大小={}", bytes.len());
    save_pending(&app, &version, &bytes)?;
    Ok(version)
}

/// 安装已经下载好的新版本（用户点了“安装更新”）。先再查一次服务器上的最新版本，只装和它一样的安装包：
/// 下好以后又出了新版本时删掉旧包、返回 update.outdated（让用户重新下载）；没下载时返回 update.not_downloaded。
/// Windows 上会先正常关闭 Pierlink、运行安装程序，装好后自动重新打开，本命令不会返回。
#[tauri::command]
#[specta::specta]
pub async fn update_install(app: AppHandle) -> Result<(), AppError> {
    let exit_app = app.clone();
    let mut builder = app.updater_builder();
    if let Some(proxy) = update_proxy() {
        builder = builder.proxy(proxy);
    }
    let updater = builder
        .on_before_exit(move || {
            eprintln!("即将运行安装程序：先关闭同步、隧道和终端");
            // 关闭要同步等几秒：在工作线程上时让运行时先把这个线程上的其他任务挪走
            tokio::task::block_in_place(|| crate::shutdown_all(&exit_app));
        })
        .build()
        .map_err(|e| fail("update.install_failed", e))?;
    let Some(update) = updater
        .check()
        .await
        .map_err(|e| fail("update.check_failed", e))?
    else {
        clear_pending(&app);
        return Err(AppError::new("update.none"));
    };
    let Some(bytes) = load_pending(&app, &update.version) else {
        if pending_version(&app).is_some() {
            clear_pending(&app);
            return Err(AppError::new("update.outdated").with("version", &update.version));
        }
        return Err(AppError::new("update.not_downloaded"));
    };
    eprintln!("开始安装更新: 版本={}", update.version);
    tokio::task::spawn_blocking(move || update.install(bytes))
        .await
        .map_err(|e| fail("update.install_failed", e))?
        .map_err(|e| fail("update.install_failed", e))
}
