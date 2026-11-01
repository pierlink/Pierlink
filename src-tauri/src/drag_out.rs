//! 把服务器文件栏里的文件拖到窗口外（资源管理器、桌面等），松手放下时才下载（只支持 Windows）。
//!
//! 开始拖动时只在数据目录的 transfer-tmp 下建一个空的 `drag-<传输 ID>` 目录，算出每一项下载后的本机路径，
//! 作为 CF_HDROP 交给系统拖放；松手放到别的程序上时才把文件下载到这个目录（显示系统的进度框），下完再让对方取走。
//! 系统拖放本身（COM 对象、进度框、消息循环）在 `drag_out/win.rs`，只在 Windows 上编译；
//! 这里的准备、下载和收尾在所有平台上编译（其他平台的命令一开始就返回“不支持”）。
//!
//! 临时目录保留多久：对方“移动”（资源管理器默认）时文件已经被移走，空目录马上删掉；
//! 对方“复制”时它可能稍后还要读（比如网页上传），目录先留着，超过 1 小时（[`DRAG_KEEP`]）后在下次拖出或下次启动时删除。

mod format;
mod job;
#[cfg(windows)]
mod win;

use crate::files::{
    FilesState, LocDto, RELAY_RESERVE, TransferProgressDto, TransferReportDto, blocking,
    format_bytes, lock, open_session, progress_sender, valid_transfer_id,
};
use pierlink_core::error::AppError;
use pierlink_core::fs_local;
use pierlink_core::sftp::transfer::{
    self, Conflict, TransferProgress, TransferReport, windows_filename,
};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};

/// 拖出的临时目录保留多久：复制走它的程序可能稍后还要读，超过这个时间才删
pub(crate) const DRAG_KEEP: Duration = Duration::from_secs(3600);
/// 拖出临时目录的名字前缀（后面是传输 ID）
const DRAG_PREFIX: &str = "drag-";

/// 系统进度框里的文字（随界面语言）
#[derive(Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DragOutLabels {
    /// 标题栏
    pub title: String,
    /// 第一行，如“正在下载 report.pdf”
    pub line: String,
    /// 点了取消之后显示的文字
    pub cancelling: String,
}

/// 拖出的结果
#[derive(Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DragOutResultDto {
    /// 已下载并交给了放下的程序；report 里有跳过、改名、失败的项
    Dropped { report: TransferReportDto },
    /// 没有放下：按了 Esc、放在不接收文件的地方
    Cancelled,
    /// 拖回了本窗口：x、y 是松手处相对窗口客户区的物理像素，界面按窗口内的拖放处理
    Returned { x: i32, y: i32 },
}

/// 把服务器上的文件拖到窗口外（资源管理器、桌面等）：开始系统拖放，松手放到别的程序上时才下载，
/// 下载期间显示系统的进度框。拖放结束后才返回。
///
/// :param transfer_id: 前端生成的传输 ID（取消、临时目录名都用它）
/// :param from: 文件所在的服务器
/// :param sources: 要拖出的完整路径
/// :param labels: 进度框里的文字
/// :param on_progress: 下载进度；开始下载时先发一条全是 0 的
/// :return: 拖放结果；下载出错时返回错误（在进度框里取消时是 sftp.cancelled）
#[tauri::command]
#[specta::specta]
pub async fn files_drag_out(
    app: AppHandle,
    transfer_id: String,
    from: LocDto,
    sources: Vec<String>,
    labels: DragOutLabels,
    on_progress: Channel<TransferProgressDto>,
) -> Result<DragOutResultDto, AppError> {
    if !cfg!(windows) {
        return Err(AppError::new("files.drag_out_unsupported"));
    }
    if sources.is_empty() {
        return Ok(DragOutResultDto::Cancelled);
    }
    if !valid_transfer_id(&transfer_id) {
        return Err(AppError::new("files.invalid_id"));
    }
    if matches!(from, LocDto::Local) {
        return Err(AppError::new("files.not_remote"));
    }
    blocking(app, move |app| {
        drag_out(app, transfer_id, from, sources, labels, on_progress)
    })
    .await
}

/// 下载线程的结果，拖放结束后用来给出报告或错误
type Fetched = Arc<Mutex<Option<Result<TransferReport, AppError>>>>;

/// 准备临时目录和路径，在主线程上开始系统拖放，等它结束后收尾（在后台线程上运行，拖放期间一直等着）
fn drag_out(
    app: &AppHandle,
    id: String,
    from: LocDto,
    sources: Vec<String>,
    labels: DragOutLabels,
    on_progress: Channel<TransferProgressDto>,
) -> Result<DragOutResultDto, AppError> {
    let root = crate::data::data_dir(app)?.join("transfer-tmp");
    // 顺便删掉超过保留时间的旧拖出目录
    remove_stale(&root, SystemTime::now(), false);
    let dir = make_dir(&root.join(format!("{DRAG_PREFIX}{id}")))?;
    // 和下载时的改名规则一致（Windows 上替换不允许的字符）
    let paths = match drop_paths(&dir, &sources, cfg!(windows)) {
        Ok(paths) => paths,
        Err(e) => {
            remove_all(&dir);
            return Err(e);
        }
    };
    let state = app.state::<FilesState>();
    let cancel = state.cancel_flag(&id);
    let fetched = Fetched::default();
    let fetch: job::Fetch = {
        let (app, dir, planned, slot) = (app.clone(), dir.clone(), paths.clone(), fetched.clone());
        Box::new(move |progress: &job::Progress| {
            let result = download(&app, &from, &sources, &dir, &on_progress, progress);
            // 只把实际存在的项交给对方（被跳过的符号链接、整个下载失败的不在里面）
            let present: Vec<PathBuf> = planned
                .iter()
                .filter(|p| p.symlink_metadata().is_ok())
                .cloned()
                .collect();
            let ok = result.is_ok() && !present.is_empty();
            *lock(&slot) = Some(result);
            ok.then_some(present)
        })
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let request = job::Request {
        paths,
        labels: job::Labels {
            title: labels.title,
            line: labels.line,
            cancelling: labels.cancelling,
        },
        progress: Arc::new(job::Progress::new(Arc::clone(&cancel))),
        fetch,
        done: Box::new(move |outcome| {
            let _ = tx.send(outcome);
        }),
    };
    let outcome = match start_on_main_thread(app, request) {
        // 请求没被处理就丢掉了（应用正在退出）时发送端随之释放，这里收到错误
        Ok(()) => rx.recv().unwrap_or(job::Outcome::Failed {
            reason: job::Failure::Unfinished,
            downloaded: false,
        }),
        Err(e) => {
            eprintln!("拖出没能交给主线程: 错误={e}");
            job::Outcome::Failed {
                reason: job::Failure::Unfinished,
                downloaded: false,
            }
        }
    };
    state.forget_transfer(&id);
    let fetched = lock(&fetched).take();
    finish(&dir, outcome, fetched, cancel.load(Ordering::Relaxed))
}

/// 在主线程上开始系统拖放；结束时（包括没能开始）会调用 request.done
#[cfg(windows)]
fn start_on_main_thread(app: &AppHandle, request: job::Request) -> tauri::Result<()> {
    app.run_on_main_thread(move || win::start(request))
}

/// 其他平台没有这里用的系统拖放（命令一开始就会返回“不支持”，走不到这里）
#[cfg(not(windows))]
fn start_on_main_thread(_app: &AppHandle, request: job::Request) -> tauri::Result<()> {
    (request.done)(job::Outcome::Failed {
        reason: job::Failure::Unsupported,
        downloaded: false,
    });
    Ok(())
}

/// 拖放出错的错误（说明按当前语言显示；附带的系统信息放在 detail 里）
fn failed(reason: &job::Failure) -> AppError {
    use job::Failure;
    let error = AppError::new("files.drag_out_failed");
    let error = match reason {
        Failure::Timer => error.with_reason("files.drag_out_timer", "开始拖放失败"),
        Failure::Busy => error.with_reason("files.drag_out_busy", "已有一个拖出正在进行"),
        Failure::Crashed => error.with_reason("files.drag_out_crashed", "拖放时程序出错"),
        Failure::Ole(_) => error.with_reason("files.drag_out_ole", "系统拖放初始化失败"),
        Failure::Rejected => error.with_reason("files.drag_out_rejected", "放下的程序没有接收文件"),
        Failure::Unexpected(_) => {
            error.with_reason("files.drag_out_unexpected", "系统拖放返回了意外的结果")
        }
        Failure::Unsupported => error.with_reason("files.drag_out_unsupported", "只支持 Windows"),
        Failure::Unfinished => error.with_reason("files.drag_out_unfinished", "拖放没有完成"),
    };
    match reason.detail() {
        Some(detail) => error.with("detail", detail),
        None => error,
    }
}

/// 拖出的临时目录先不删（对方可能还要读，或下载线程还在写）：目录空了就删；不空就留给清理。
/// 清理按目录的修改时间算保留期，而目录里只有直接子项变化才更新这个时间（下载一个文件夹时顶层时间停在开始那一刻），
/// 这里先建一个再删一个标记文件，把时间刷到现在，保留期从现在起算
fn leave_for_cleanup(dir: &Path) {
    let mark = dir.join(".pierlink-keep");
    if std::fs::write(&mark, b"").is_ok() {
        let _ = std::fs::remove_file(&mark);
    }
    let _ = std::fs::remove_dir(dir);
}

/// 建好拖出的临时目录，返回规范化后的路径：下载时也会规范化目标目录（去掉 `\\?\` 前缀等），
/// 用同一个形式，CF_HDROP 里的路径才和实际下载到的一致
fn make_dir(dir: &Path) -> Result<PathBuf, AppError> {
    let failed = |operation: &str, e: std::io::Error| {
        AppError::new("fs.failure")
            .with("operation", operation)
            .with("reason", e.to_string())
    };
    std::fs::create_dir_all(dir).map_err(|e| failed("create_dir", e))?;
    fs_local::plain_canonical(dir).map_err(|e| {
        remove_all(dir);
        failed("canonicalize", e)
    })
}

/// 下载（在工作线程里）：先统计、检查本机空间（和中转一样要多留 1 GB），再下载到拖出的临时目录。
/// 进度同时写给进度框和界面的传输列表
fn download(
    app: &AppHandle,
    from: &LocDto,
    sources: &[String],
    dir: &Path,
    channel: &Channel<TransferProgressDto>,
    progress: &job::Progress,
) -> Result<TransferReport, AppError> {
    let mut send = progress_sender(channel.clone(), None);
    let mut report = |p: &TransferProgress| {
        let current = relative_name(&p.current, dir);
        progress.set(p.done_bytes, p.total_bytes, &current);
        send(&TransferProgress {
            total_bytes: p.total_bytes,
            done_bytes: p.done_bytes,
            current,
            files_done: p.files_done,
            files_total: p.files_total,
        });
    };
    // 先报一条全是 0 的进度：界面收到后在传输列表里加上这一项
    report(&TransferProgress {
        total_bytes: 0,
        done_bytes: 0,
        current: String::new(),
        files_done: 0,
        files_total: 0,
    });
    let mut session = open_session(app, from)?;
    let need = transfer::plan_remote(&mut session, sources)?.total_bytes;
    if let Some(free) = fs_local::free_space(dir)
        && free < need.saturating_add(RELAY_RESERVE)
    {
        return Err(AppError::new("files.drag_no_space")
            .with("need", format_bytes(need))
            .with("free", format_bytes(free)));
    }
    transfer::download(
        &mut session,
        sources,
        dir,
        Conflict::Error,
        &mut report,
        progress.cancel_flag(),
    )
}

/// 拖放结束后的收尾：删掉用不着的临时文件，把结果转成给界面的。
///
/// :param fetched: 下载线程的结果（没下载时为 None）
/// :param cancelled: 用户是否取消过（下载线程没来得及结束时据此报“已取消”）
fn finish(
    dir: &Path,
    outcome: job::Outcome,
    fetched: Option<Result<TransferReport, AppError>>,
    cancelled: bool,
) -> Result<DragOutResultDto, AppError> {
    match outcome {
        job::Outcome::Dropped => {
            // 对方移动走了的话目录已经空了，直接删；复制的话对方可能稍后还要读，留到超过保留时间再删
            // （保留时间从现在起算，见 leave_for_cleanup）
            leave_for_cleanup(dir);
            let report = fetched.and_then(Result::ok).unwrap_or_default();
            Ok(DragOutResultDto::Dropped {
                report: report.into(),
            })
        }
        job::Outcome::Cancelled => {
            remove_all(dir);
            Ok(DragOutResultDto::Cancelled)
        }
        job::Outcome::Returned { x, y } => {
            remove_all(dir);
            Ok(DragOutResultDto::Returned { x, y })
        }
        job::Outcome::FetchFailed => {
            if fetched.is_some() {
                remove_all(dir);
            } else {
                // 下载线程还没结束（取消后网络卡住，没再等它）：它可能还在往里写，不删，留给清理
                leave_for_cleanup(dir);
            }
            Err(match fetched {
                Some(Err(e)) => e,
                Some(Ok(report)) => nothing_downloaded(&report),
                // 取消后下载线程迟迟没结束（网络卡住），没再等它
                None if cancelled => AppError::new("sftp.cancelled"),
                None => failed(&job::Failure::Unfinished),
            })
        }
        job::Outcome::Failed { reason, downloaded } => {
            // 已经下载好的话对方可能正在读，只删空目录，其余留给清理
            if downloaded {
                leave_for_cleanup(dir);
            } else {
                remove_all(dir);
            }
            Err(failed(&reason))
        }
    }
}

/// 删除整个目录（不存在不算错），失败只记日志
fn remove_all(dir: &Path) {
    if let Err(e) = std::fs::remove_dir_all(dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("删除拖出临时目录失败: 目录={}, 错误={e}", dir.display());
    }
}

/// 每个来源下载到 dir 后的本机路径。名字和下载时一样处理：Windows 上替换不允许的字符、给保留名加前缀。
///
/// :param dir: 下载到的本机目录
/// :param sources: 服务器上的完整路径
/// :param windows_names: 是否按 Windows 的规则改名（Windows 上下载就是这样改的）
/// :return: 和 sources 一一对应的本机路径；名字不能用，或两项落到同一个路径时返回错误
fn drop_paths(
    dir: &Path,
    sources: &[String],
    windows_names: bool,
) -> Result<Vec<PathBuf>, AppError> {
    let mut seen = HashSet::new();
    sources
        .iter()
        .map(|source| {
            let name = source.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            if name.is_empty() || name == "." || name == ".." || name.contains('\0') {
                return Err(AppError::new("files.invalid_name"));
            }
            let local = if windows_names {
                windows_filename(name)
            } else {
                name.to_owned()
            };
            // Windows 的文件名不区分大小写
            let key = if windows_names {
                local.to_lowercase()
            } else {
                local.clone()
            };
            if !seen.insert(key) {
                return Err(AppError::new("transfer.exists")
                    .with_reason(
                        "sftp.multiple_sources_map_to_the_same_destination_transfer_them_separately",
                        "多个来源映射到同一目标，请分别传输",
                    )
                    .with("items", &local));
            }
            Ok(dir.join(local))
        })
        .collect()
}

/// 进度里的当前文件去掉拖出临时目录的前缀，只留相对的部分（进度框、传输列表里看着清楚）。
///
/// :param current: 核心报告的当前文件（本机完整路径）
/// :param dir: 拖出的临时目录
/// :return: 相对路径；不在 dir 下时原样返回
fn relative_name(current: &str, dir: &Path) -> String {
    Path::new(current)
        .strip_prefix(dir)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| current.to_owned())
}

/// 下载“成功”却一项也没下载到（都被跳过或失败）时的错误：用第一项的原因
fn nothing_downloaded(report: &TransferReport) -> AppError {
    report
        .failed
        .first()
        .or(report.skipped.first())
        .map(|issue| issue.reason.clone())
        .unwrap_or_else(|| AppError::new("files.drag_out_empty"))
}

/// transfer-tmp 下该删的项。
///
/// 拖出的临时目录（`drag-` 开头）超过 keep 才删：复制走它的程序可能稍后还要读；修改时间读不到、
/// 或者在 now 之后（时钟往回调过）时当作还新。other 为 true 时（启动时）其余的中转残留一律删，
/// 平时它们可能正被中转传输使用，不动。
///
/// :param root: transfer-tmp 目录（不存在时返回空）
/// :param now: 当前时间
/// :param keep: 拖出临时目录的保留时长
/// :param other: 是否包括拖出以外的项
/// :return: 要删的路径
fn stale_entries(root: &Path, now: SystemTime, keep: Duration, other: bool) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            let dragged = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(DRAG_PREFIX));
            if !dragged {
                return other;
            }
            entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > keep)
        })
        .map(|entry| entry.path())
        .collect()
}

/// 删除 transfer-tmp 下过期的项（规则见 [`stale_entries`]，拖出的临时目录保留 [`DRAG_KEEP`]），失败只记日志。
///
/// :param root: transfer-tmp 目录
/// :param now: 当前时间
/// :param other: 是否连中转传输的残留一起删（只在启动时）
pub(crate) fn remove_stale(root: &Path, now: SystemTime, other: bool) {
    for path in stale_entries(root, now, DRAG_KEEP, other) {
        let result = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path),
            Ok(_) => std::fs::remove_file(&path),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            eprintln!("清理传输临时文件失败: 路径={}, 错误={e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pierlink_core::sftp::transfer::TransferIssue;

    fn sources(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn 每一项下载后的路径是目录加名字() {
        let dir = Path::new("/data/transfer-tmp/drag-x1");
        let paths = drop_paths(
            dir,
            &sources(&["/home/u/a.txt", "/home/u/logs/", "/b"]),
            false,
        )
        .unwrap();
        assert_eq!(
            paths,
            vec![dir.join("a.txt"), dir.join("logs"), dir.join("b")]
        );
    }

    #[test]
    fn 按windows规则改名和下载时一致() {
        let dir = Path::new("/t");
        let paths = drop_paths(
            dir,
            &sources(&["/srv/a:b?.txt", "/srv/CON", "/srv/end. "]),
            true,
        )
        .unwrap();
        assert_eq!(
            paths,
            vec![
                dir.join(windows_filename("a:b?.txt")),
                dir.join(windows_filename("CON")),
                dir.join(windows_filename("end. "))
            ]
        );
        assert_eq!(paths[0], dir.join("a_b_.txt"));
        assert_eq!(paths[1], dir.join("_CON"));
    }

    #[test]
    fn 两项落到同一个名字时报错() {
        let dir = Path::new("/t");
        // Windows 上不区分大小写
        let e = drop_paths(dir, &sources(&["/x/Report.txt", "/y/report.TXT"]), true).unwrap_err();
        assert_eq!(e.code, "transfer.exists");
        // 改名后撞上
        let e = drop_paths(dir, &sources(&["/x/a:b", "/y/a_b"]), true).unwrap_err();
        assert_eq!(e.code, "transfer.exists");
        // 区分大小写的规则下是两个名字
        assert!(drop_paths(dir, &sources(&["/x/Report.txt", "/y/report.TXT"]), false).is_ok());
    }

    #[test]
    fn 根目录和点开头的特殊名字不能拖出() {
        let dir = Path::new("/t");
        for bad in ["/", "", "/home/.", "/home/..", "/a/b\0c"] {
            let e = drop_paths(dir, &sources(&[bad]), true).unwrap_err();
            assert_eq!(e.code, "files.invalid_name", "{bad:?}");
        }
        // 隐藏文件照常
        assert!(drop_paths(dir, &sources(&["/home/u/.bashrc"]), true).is_ok());
    }

    #[test]
    fn 进度里的当前文件只留相对部分() {
        let dir = Path::new("/data/transfer-tmp/drag-x1");
        assert_eq!(
            relative_name("/data/transfer-tmp/drag-x1/logs/a.log", dir),
            Path::new("logs/a.log").display().to_string()
        );
        assert_eq!(
            relative_name("/data/transfer-tmp/drag-x1/b.txt", dir),
            "b.txt"
        );
        // 不在临时目录下的原样返回
        assert_eq!(relative_name("/other/c.txt", dir), "/other/c.txt");
        assert_eq!(relative_name("", dir), "");
    }

    #[test]
    fn 一项也没下载到时用第一项的原因() {
        let failed = AppError::new("sftp.permission_denied");
        let skipped = AppError::new("transfer.skipped");
        let mut report = TransferReport::default();
        report.skipped.push(TransferIssue {
            path: "/a".into(),
            reason: skipped.clone(),
        });
        assert_eq!(nothing_downloaded(&report).code, "transfer.skipped");
        report.failed.push(TransferIssue {
            path: "/b".into(),
            reason: failed,
        });
        assert_eq!(nothing_downloaded(&report).code, "sftp.permission_denied");
        assert_eq!(
            nothing_downloaded(&TransferReport::default()).code,
            "files.drag_out_empty"
        );
    }

    #[test]
    fn 拖出的临时目录超过一小时才删() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("drag-a")).unwrap();
        std::fs::write(root.path().join("drag-a").join("f.txt"), "x").unwrap();
        std::fs::write(root.path().join("drag-b"), "x").unwrap();
        let now = SystemTime::now();
        // 刚建的：不删
        assert!(stale_entries(root.path(), now, DRAG_KEEP, false).is_empty());
        // 两小时后：都删
        let later = now + Duration::from_secs(2 * 3600);
        let mut old = stale_entries(root.path(), later, DRAG_KEEP, false);
        old.sort();
        assert_eq!(
            old,
            vec![root.path().join("drag-a"), root.path().join("drag-b")]
        );
        // 时钟往回调（修改时间在当前时间之后）：当作还新
        let earlier = now - Duration::from_secs(2 * 3600);
        assert!(stale_entries(root.path(), earlier, DRAG_KEEP, false).is_empty());
    }

    #[test]
    fn 中转残留只在启动时删() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("x1abc-2")).unwrap();
        std::fs::create_dir(root.path().join("drag-new")).unwrap();
        let now = SystemTime::now();
        // 平时（拖出时顺便清理）：正在进行的中转传输的目录不动
        assert!(stale_entries(root.path(), now, DRAG_KEEP, false).is_empty());
        // 启动时：中转残留删掉，新的拖出目录留着
        assert_eq!(
            stale_entries(root.path(), now, DRAG_KEEP, true),
            vec![root.path().join("x1abc-2")]
        );
    }

    #[test]
    fn 清理时删掉过期的目录和文件() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("drag-a").join("sub")).unwrap();
        std::fs::write(root.path().join("drag-a").join("sub").join("f.txt"), "x").unwrap();
        std::fs::write(root.path().join("leftover.part"), "x").unwrap();
        std::fs::create_dir(root.path().join("drag-new")).unwrap();
        remove_stale(root.path(), SystemTime::now(), true);
        assert!(root.path().join("drag-new").exists());
        assert!(!root.path().join("leftover.part").exists());
        remove_stale(
            root.path(),
            SystemTime::now() + Duration::from_secs(2 * 3600),
            false,
        );
        assert!(!root.path().join("drag-a").exists());
        assert!(!root.path().join("drag-new").exists());
        // 目录不存在时什么也不做
        remove_stale(&root.path().join("missing"), SystemTime::now(), true);
    }

    #[test]
    fn 传输编号只能是字母数字和横线() {
        assert!(valid_transfer_id("x1abc-2"));
        assert!(valid_transfer_id(&"a".repeat(64)));
        assert!(!valid_transfer_id(""));
        assert!(!valid_transfer_id(&"a".repeat(65)));
        assert!(!valid_transfer_id("../x"));
        assert!(!valid_transfer_id("a b"));
    }
}
