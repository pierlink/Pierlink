//! 粘贴图片：剪贴板里只有图片时，转成 PNG。服务器终端经文件面板的 SFTP 会话传到那台服务器的 /tmp
//! （只有自己能读）；本机终端存到本机数据目录。返回路径给界面粘贴进终端，Claude Code 等会把它当成图片。
//! 旧图片：粘贴时顺手删掉自己 7 天前的（服务器上传完后在后台删，每台每小时最多一次；服务器系统也会定期清 /tmp）。
use crate::files::LocDto;
use pierlink_core::error::AppError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::AppHandle;

/// 一张图片（PNG）最大多少字节
const MAX_BYTES: usize = 20 * 1024 * 1024;
/// 图片保留多久
const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);
/// 文件名开头；清理时只认这个开头加 .png 的
const PREFIX: &str = "pierlink-paste-";
/// 服务器上放图片的目录
const REMOTE_DIR: &str = "/tmp";
/// 上传最多等多久
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// 清理时最多看 /tmp 里多少个条目
const LIST_LIMIT: usize = 20_000;
/// 同一台服务器多久最多清理一次
const CLEAN_EVERY: Duration = Duration::from_secs(3600);

/// 各服务器上次清理的时间（键同文件面板的会话）
static LAST_CLEAN: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 这台服务器该不该清理：一小时内清理过就不清；要清理时记下时间
fn clean_due(key: &str, now: Instant) -> bool {
    let mut last = LAST_CLEAN.lock().unwrap_or_else(|p| p.into_inner());
    if last
        .get(key)
        .is_some_and(|t| now.saturating_duration_since(*t) < CLEAN_EVERY)
    {
        return false;
    }
    last.insert(key.to_owned(), now);
    true
}

/// Unix 秒换成 UTC 的“年月日-时分秒”
fn stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    // 从 1970-01-01 起的天数换算成公历日期（Howard Hinnant 的 civil_from_days）
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// 10 位随机十六进制字符（取自系统随机数）：别人猜不到文件名
fn random_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..10].to_owned()
}

/// 新图片的文件名：pierlink-paste-年月日-时分秒-随机10位.png（UTC；只有字母、数字和横线，路径不用加引号）
fn file_name(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{PREFIX}{}-{}.png", stamp(secs), random_suffix())
}

/// 是不是我们放的图片
fn is_ours(name: &str) -> bool {
    name.starts_with(PREFIX) && name.ends_with(".png")
}

/// 剪贴板里的图片（PNG）；没有图片返回 clipboard.no_image，太大返回 paste.image_too_large
fn clipboard_png() -> Result<Vec<u8>, AppError> {
    let png =
        crate::clipboard_image::read_png()?.ok_or_else(|| AppError::new("clipboard.no_image"))?;
    if png.len() > MAX_BYTES {
        return Err(AppError::new("paste.image_too_large")
            .with("mb", format!("{:.1}", png.len() as f64 / 1024.0 / 1024.0)));
    }
    Ok(png)
}

/// 删掉服务器 /tmp 里自己 7 天前的图片；列不出来或删不掉（别的用户的）都忽略
fn clean_remote(session: &mut pierlink_core::sftp::SftpSession, now: SystemTime) {
    let Ok(listing) = session.list_dir(REMOTE_DIR, LIST_LIMIT) else {
        return;
    };
    let cutoff = now
        .checked_sub(KEEP)
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    for entry in listing.entries {
        if is_ours(&entry.name) && entry.mtime < cutoff {
            let _ = session.remove_file(&format!("{REMOTE_DIR}/{}", entry.name));
        }
    }
}

/// 把剪贴板里的图片传到服务器终端所在的服务器，返回路径。
///
/// :param loc: 终端所在的服务器（保存过的服务器或快速连接）
/// :return: 服务器上的绝对路径；剪贴板里没有图片返回 clipboard.no_image；上传超过 30 秒返回 paste.upload_timeout
#[tauri::command]
#[specta::specta]
pub async fn terminal_paste_image_remote(app: AppHandle, loc: LocDto) -> Result<String, AppError> {
    if matches!(loc, LocDto::Local) {
        return Err(AppError::new("paste.image_unsupported"));
    }
    tauri::async_runtime::spawn_blocking(move || {
        let png = clipboard_png()?;
        let now = SystemTime::now();
        let path = format!("{REMOTE_DIR}/{}", file_name(now));
        let result = crate::files::with_session(&app, &loc, |session| {
            // 超时：另起一个线程，到时间就让写入停下
            let cancel = Arc::new(AtomicBool::new(false));
            let (done, wait) = std::sync::mpsc::channel::<()>();
            let flag = cancel.clone();
            std::thread::spawn(move || {
                if wait.recv_timeout(UPLOAD_TIMEOUT).is_err() {
                    flag.store(true, Ordering::Release);
                }
            });
            let written = session.write_from(
                &path,
                &mut std::io::Cursor::new(&png),
                Some(0o600),
                &mut |_| {},
                &cancel,
            );
            let _ = done.send(());
            match written {
                Ok(_) => Ok(()),
                Err(e) => {
                    // 失败时删掉传了一半的文件
                    let _ = session.remove_file(&path);
                    Err(if cancel.load(Ordering::Acquire) {
                        AppError::new("paste.upload_timeout")
                    } else {
                        e
                    })
                }
            }
        });
        result?;
        eprintln!("粘贴图片已传到服务器: 路径={path}, 字节={}", png.len());
        // 清理放到后台：/tmp 里东西多时列目录要一会儿，不耽误这次粘贴
        if clean_due(&loc.key(), Instant::now()) {
            let (app, loc) = (app.clone(), loc.clone());
            std::thread::spawn(move || {
                let _ = crate::files::with_session(&app, &loc, |session| {
                    clean_remote(session, SystemTime::now());
                    Ok(())
                });
            });
        }
        Ok(path)
    })
    .await
    .map_err(|e| AppError::new("terminal.task_failed").with("reason", e.to_string()))?
}

/// 本机放图片的目录（数据目录下的 paste）
fn local_dir(app: &AppHandle) -> Result<PathBuf, AppError> {
    Ok(crate::data::data_dir(app)?.join("paste"))
}

/// 删掉本机目录里 7 天前的图片；出错忽略
fn clean_local(dir: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > KEEP));
        if is_ours(&name.to_string_lossy()) && old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 把剪贴板里的图片存到本机，返回 Windows 路径（界面再按终端的 shell 改写成它认的写法）。
///
/// :return: 本机绝对路径；剪贴板里没有图片返回 clipboard.no_image
#[tauri::command]
#[specta::specta]
pub async fn terminal_paste_image_local(app: AppHandle) -> Result<String, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let png = clipboard_png()?;
        let now = SystemTime::now();
        let dir = local_dir(&app)?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| AppError::new("paste.save_failed").with("reason", e.to_string()))?;
        clean_local(&dir, now);
        let path = dir.join(file_name(now));
        std::fs::write(&path, &png)
            .map_err(|e| AppError::new("paste.save_failed").with("reason", e.to_string()))?;
        eprintln!(
            "粘贴图片已存到本机: 路径={}, 字节={}",
            path.display(),
            png.len()
        );
        Ok(path.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| AppError::new("terminal.task_failed").with("reason", e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 时间换成年月日时分秒() {
        assert_eq!(stamp(0), "19700101-000000");
        // 2026-10-31 14:32:05 UTC
        assert_eq!(stamp(1_793_457_125), "20261031-143205");
        // 闰年 2024-02-29 23:59:59 UTC
        assert_eq!(stamp(1_709_251_199), "20240229-235959");
    }

    #[test]
    fn 文件名只有字母数字横线且不重复() {
        let now = UNIX_EPOCH + Duration::from_secs(1_793_457_125);
        let a = file_name(now);
        let b = file_name(now);
        assert!(
            a.starts_with("pierlink-paste-20261031-143205-") && a.ends_with(".png"),
            "{a}"
        );
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        );
        assert_ne!(a, b);
        assert!(is_ours(&a));
        assert!(!is_ours("pierlink-paste-x.txt"));
        assert!(!is_ours("other.png"));
    }

    #[test]
    fn 每台服务器一小时最多清理一次() {
        let now = Instant::now();
        assert!(clean_due("server:清理测试", now));
        assert!(!clean_due("server:清理测试", now + Duration::from_secs(60)));
        // 别的服务器不受影响
        assert!(clean_due("server:清理测试2", now));
        assert!(clean_due("server:清理测试", now + CLEAN_EVERY));
    }

    #[test]
    fn 本机只删自己七天前的图片() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir
            .path()
            .join("pierlink-paste-20200101-000000-aaaaaaaa.png");
        let fresh = dir
            .path()
            .join("pierlink-paste-20261031-000000-bbbbbbbb.png");
        let other = dir.path().join("notes.png");
        for p in [&old, &fresh, &other] {
            std::fs::write(p, b"x").unwrap();
        }
        // 把“现在”放到 8 天以后：三个文件都超过 7 天，但只删我们的
        let later = SystemTime::now() + Duration::from_secs(8 * 24 * 3600);
        clean_local(dir.path(), later);
        assert!(!old.exists() && !fresh.exists());
        assert!(other.exists());
        // 现在：刚写的不删
        std::fs::write(&fresh, b"x").unwrap();
        clean_local(dir.path(), SystemTime::now());
        assert!(fresh.exists());
    }
}
