//! 每日在线备份，临时目录随作用域清理，只保留最新七份。
use super::*;
use rusqlite::backup::{Backup, StepResult};
use std::{fs, path::PathBuf, time::Instant};

struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        // 仅删除本次备份创建的临时目录，不触及已有备份。
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Db {
    /// 在 dir 中生成当天 UTC 的 pierlink-YYYYMMDD.db 在线备份；已有则跳过生成。
    /// 每次均清理旧备份，只保留日期最新的七份；返回备份或清理结果，不改动其他文件。
    pub fn backup_daily(&self, dir: &Path) -> Result<(), AppError> {
        let date: String = self
            .conn
            .query_row("SELECT strftime('%Y%m%d','now')", [], |r| r.get(0))
            .map_err(sql_error)?;
        self.backup_on(dir, &date)
    }

    pub(super) fn backup_on(&self, dir: &Path, date: &str) -> Result<(), AppError> {
        fs::create_dir_all(dir).map_err(io_error)?;
        let destination = dir.join(format!("pierlink-{date}.db"));
        match fs::symlink_metadata(&destination) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => {
                return Err(failure(
                    "storage.backup",
                    "storage.backup_destination_is_not_a_regular_file",
                    "备份目标不是普通文件",
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let staging_path = dir.join(format!(".pierlink-backup-{}", uuid::Uuid::new_v4()));
                fs::create_dir(&staging_path).map_err(io_error)?;
                let staging = Staging(staging_path);
                let temporary = staging.0.join("backup.db");
                {
                    let mut target = Connection::open(&temporary).map_err(sql_error)?;
                    let backup = Backup::new(&self.conn, &mut target).map_err(sql_error)?;
                    let deadline = Instant::now() + Duration::from_secs(30);
                    loop {
                        match backup.step(128).map_err(sql_error)? {
                            StepResult::Done => break,
                            _ if Instant::now() >= deadline => {
                                return Err(failure(
                                    "storage.backup_timeout",
                                    "storage.online_backup_waited_longer_than_30_seconds",
                                    "在线备份等待超过三十秒",
                                ));
                            }
                            StepResult::Busy | StepResult::Locked => {
                                std::thread::sleep(Duration::from_millis(20))
                            }
                            _ => {}
                        }
                    }
                }
                fs::OpenOptions::new()
                    .write(true)
                    .open(&temporary)
                    .map_err(io_error)?
                    .sync_all()
                    .map_err(io_error)?;
                publish_backup(&temporary, &destination, |source, target| {
                    fs::hard_link(source, target)
                })
                .map_err(io_error)?;
            }
            Err(e) => return Err(io_error(e)),
        }
        let mut backups = Vec::new();
        for entry in fs::read_dir(dir).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            if entry.file_type().map_err(io_error)?.is_file()
                && entry.file_name().to_str().is_some_and(is_backup_name)
            {
                backups.push(entry.path());
            }
        }
        backups.sort();
        let remove_count = backups.len().saturating_sub(7);
        for path in backups.into_iter().take(remove_count) {
            fs::remove_file(path).map_err(io_error)?;
        }
        Ok(())
    }
}

fn is_backup_name(name: &str) -> bool {
    let Some(date) = name
        .strip_prefix("pierlink-")
        .and_then(|s| s.strip_suffix(".db"))
    else {
        return false;
    };
    if date.len() != 8 || !date.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let year: u32 = date[..4].parse().unwrap_or(0);
    let month: u32 = date[4..6].parse().unwrap_or(0);
    let day: u32 = date[6..].parse().unwrap_or(0);
    let days = match month {
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => 0,
    };
    year > 0 && day > 0 && day <= days
}

// 优先用硬链接避免覆盖；文件系统不支持时，确认目标仍不存在后改用重命名。
pub(super) fn publish_backup(
    temporary: &Path,
    destination: &Path,
    hard_link: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::io::ErrorKind;
    match hard_link(temporary, destination) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(e)
            if e.kind() == ErrorKind::Unsupported
                || (cfg!(windows) && e.raw_os_error() == Some(1)) =>
        {
            // Windows 的 ERROR_INVALID_FUNCTION 也表示该文件系统不支持硬链接。
            match fs::symlink_metadata(destination) {
                Ok(meta) if meta.file_type().is_file() => Ok(()),
                Ok(_) => Err(std::io::Error::new(
                    ErrorKind::AlreadyExists,
                    "备份目标不是普通文件",
                )),
                Err(e) if e.kind() == ErrorKind::NotFound => fs::rename(temporary, destination),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// 从备份恢复后的说明（给界面提示用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// 用的是哪一天的备份（YYYYMMDD）
    pub backup_date: String,
    /// 损坏的数据库被改名保存到这里（不删除，便于以后排查或手动找回）
    pub corrupt_saved_as: PathBuf,
}

/// 打开数据库；完整性检查失败时，把损坏的文件（连同 -wal、-shm）改名放在原处，
/// 再从 backup_dir 里最新的一份完好的备份复制回来打开（L4）。
///
/// :param path: 数据库文件
/// :param backup_dir: 每日备份目录（[`Db::backup_daily`] 写的地方）
/// :return: 数据库，以及是否从备份恢复；没有可用备份时返回原来的 storage.corrupt 错误
pub fn open_or_restore(path: &Path, backup_dir: &Path) -> Result<(Db, Option<Restored>), AppError> {
    let error = match open(path) {
        Ok(db) => return Ok((db, None)),
        Err(e) if e.code == "storage.corrupt" => e,
        Err(e) => return Err(e),
    };
    let mut backups: Vec<PathBuf> = match fs::read_dir(backup_dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(is_backup_name))
            .map(|e| e.path())
            .collect(),
        Err(_) => return Err(error),
    };
    if backups.is_empty() {
        return Err(error);
    }
    // 损坏的文件连同 -wal、-shm 改名留在原处
    let saved = path.with_extension(format!("db.corrupt-{}", chrono_like_stamp()));
    fs::rename(path, &saved).map_err(io_error)?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", path.display()));
        if side.exists() {
            let _ = fs::rename(&side, PathBuf::from(format!("{}{suffix}", saved.display())));
        }
    }
    // 从新到旧：复制到原位置再打开（打开会做迁移、写 WAL，不能直接在备份文件上试，免得改动备份）
    backups.sort();
    for backup in backups.iter().rev() {
        let _ = fs::remove_file(path);
        for suffix in ["-wal", "-shm"] {
            let _ = fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        if fs::copy(backup, path).is_err() {
            continue;
        }
        if let Ok(db) = open(path) {
            let date = backup
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("pierlink-"))
                .and_then(|n| n.strip_suffix(".db"))
                .unwrap_or_default()
                .to_owned();
            return Ok((
                db,
                Some(Restored {
                    backup_date: date,
                    corrupt_saved_as: saved,
                }),
            ));
        }
    }
    // 没有一份能用：放回损坏的文件，保持原状
    let _ = fs::remove_file(path);
    let _ = fs::rename(&saved, path);
    Err(error)
}

/// 文件名用的时间戳（UTC 秒数，避免引入日期库）
fn chrono_like_stamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}
