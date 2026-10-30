//! 应用数据迁移：持久化搬运计划，逐项改名；提交前中断一律先回滚，再重新迁移。
mod rewrite;
#[cfg(test)]
mod tests;

use crate::{
    error::AppError,
    syncthing::{home, process},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

const JOURNAL: &str = ".pierlink-migration";
const ROOT_ITEMS: &[&str] = &[
    "pierlink.db",
    "pierlink.db-wal",
    "pierlink.db-shm",
    "pierlink.db-journal",
    "backups",
    "versions",
    "conflicts",
    "keys",
    "ssh-configs",
    "shell-hooks",
    "transfer-tmp",
    "config",
    ".ssh",
    "recovery",
];
// 与安装包资源清单对应；安装程序、程序包及资源清单都不是 Syncthing home 数据。
fn installed(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "syncthing.exe"
            | "syncthing"
            | "syncthing-linux-amd64.gz"
            | "syncthing-linux-arm64.gz"
            | "manifest.json"
    ) || matches!(
        Path::new(&name).extension().and_then(|s| s.to_str()),
        Some("exe" | "dll" | "gz" | "msi")
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationReport {
    /// 已搬运的相对路径；Syncthing home 按直接子项列出。
    pub moved: Vec<String>,
    /// 不存在的数据项及保留原处的程序资源。
    pub skipped: Vec<String>,
    /// XML 属性/文本、SSH 路径参数和数据库路径值的改写总数（含备份）。
    pub rewritten: usize,
}
#[derive(Serialize, Deserialize)]
struct Plan {
    old: PathBuf,
    new: PathBuf,
    items: Vec<String>,
    skipped: Vec<String>,
    replacements: Vec<String>,
}
struct MigrationLock(fs::File);
impl Drop for MigrationLock {
    fn drop(&mut self) {
        // 并发测试可能正在 fork；显式解锁，避免子进程短暂继承描述符导致误报占用。
        let _ = self.0.unlock();
    }
}
fn failure(text_key: &'static str, reason: &str) -> AppError {
    AppError::new("app.data_migration_failed").with_reason(text_key, reason)
}
fn io(e: std::io::Error) -> AppError {
    let mut error = failure(
        "app_data.file_operation_failed_during_data_migration",
        "数据迁移的文件操作失败",
    );
    if let Some(code) = e.raw_os_error() {
        error = error.with("os_code", code);
    }
    error
}
fn exists(path: &Path) -> Result<bool, AppError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io(e)),
    }
}
fn plain(path: &Path) -> Result<(), AppError> {
    let meta = fs::symlink_metadata(path).map_err(io)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(failure(
                "app_data.migration_paths_cannot_be_reparse_points",
                "迁移路径不能是重解析点",
            ));
        }
    }
    if meta.file_type().is_symlink() || (!meta.is_file() && !meta.is_dir()) {
        return Err(failure(
            "app_data.migration_paths_cannot_be_links_or_special_files",
            "迁移路径不能是链接或特殊文件",
        ));
    }
    Ok(())
}
fn check_ancestors(path: &Path) -> Result<(), AppError> {
    for p in path.ancestors() {
        if exists(p)? {
            plain(p)?;
        }
    }
    Ok(())
}
fn sync_dir(path: &Path) -> Result<(), AppError> {
    #[cfg(unix)]
    fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(io)?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn rename(from: &Path, to: &Path) -> Result<(), AppError> {
    if exists(to)? {
        return Err(failure(
            "app_data.destination_already_exists_and_cannot_be_overwritten",
            "迁移目标已存在，拒绝覆盖",
        )
        .with("path", to.display()));
    }
    plain(from)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
        let a: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let b: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let a = std::ffi::CString::new(from.as_os_str().as_bytes())
            .map_err(|_| failure("app_data.invalid_migration_path", "迁移路径无效"))?;
        let b = std::ffi::CString::new(to.as_os_str().as_bytes())
            .map_err(|_| failure("app_data.invalid_migration_path", "迁移路径无效"))?;
        if unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                a.as_ptr(),
                libc::AT_FDCWD,
                b.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } != 0
        {
            return Err(io(std::io::Error::last_os_error()));
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    fs::rename(from, to).map_err(io)?;
    if let Some(parent) = from.parent() {
        sync_dir(parent)?;
    }
    if let Some(parent) = to.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

fn plan(old: &Path, new: &Path) -> Result<Plan, AppError> {
    let mut plan = Plan {
        old: old.into(),
        new: new.into(),
        items: vec![],
        skipped: vec![],
        replacements: vec![],
    };
    for name in ROOT_ITEMS {
        if exists(&old.join(name))? {
            plan.items.push((*name).into());
        } else {
            plan.skipped.push((*name).into());
        }
    }
    if exists(&old.join("syncthing"))? {
        plain(&old.join("syncthing"))?;
        for e in fs::read_dir(old.join("syncthing")).map_err(io)? {
            let e = e.map_err(io)?;
            let name = e.file_name().into_string().map_err(|_| {
                failure(
                    "app_data.instance_entry_name_is_not_valid_text",
                    "实例目录项不是有效文本",
                )
            })?;
            let relative = format!("syncthing/{name}");
            if installed(&name) || name == "pierlink.lock" {
                plan.skipped.push(relative);
            } else {
                plan.items.push(relative);
            }
        }
    } else {
        plan.skipped.push("syncthing".into());
    }
    for entry in fs::read_dir(old).map_err(io)? {
        let name = entry.map_err(io)?.file_name().into_string().map_err(|_| {
            failure(
                "app_data.old_entry_name_is_not_valid_text",
                "旧目录项不是有效文本",
            )
        })?;
        if name != "syncthing" && !ROOT_ITEMS.contains(&name.as_str()) {
            plan.skipped.push(name);
        }
    }
    plan.items.sort();
    plan.skipped.sort();
    for relative in &plan.items {
        plain(&old.join(relative))?;
        if exists(&new.join(relative))? {
            return Err(failure(
                "app_data.the_new_directory_contains_data_with_the_same_name",
                "新目录含同名数据，拒绝覆盖",
            )
            .with("path", new.join(relative).display()));
        }
    }
    plan.replacements = rewrite::files(&plan)?;
    Ok(plan)
}
fn validate(plan: &Plan, old: &Path, new: &Path) -> Result<(), AppError> {
    if plan.old != old || plan.new != new {
        return Err(failure(
            "app_data.migration_record_does_not_match_the_requested_directory",
            "迁移记录与请求目录不一致",
        ));
    }
    let mut unique = std::collections::HashSet::new();
    for name in &plan.items {
        let allowed = ROOT_ITEMS.contains(&name.as_str())
            || name.strip_prefix("syncthing/").is_some_and(|n| {
                !installed(n) && n != "pierlink.lock" && Path::new(n).components().count() == 1
            });
        if !allowed
            || !unique.insert(name)
            || Path::new(name)
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(failure(
                "app_data.migration_manifest_contains_invalid_entries",
                "迁移清单包含无效条目",
            ));
        }
    }
    for name in &plan.replacements {
        if !plan.items.iter().any(|p| Path::new(name).starts_with(p))
            || Path::new(name)
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(failure(
                "app_data.migration_rewrite_manifest_contains_invalid_paths",
                "迁移改写清单包含无效路径",
            ));
        }
    }
    Ok(())
}
fn save(path: &Path, value: &impl Serialize) -> Result<(), AppError> {
    let bytes = serde_json::to_vec(value).map_err(|_| {
        failure(
            "app_data.couldn_t_encode_the_migration_record",
            "无法编码迁移记录",
        )
    })?;
    home::atomic_write(path, &bytes)?;
    sync_dir(path.parent().ok_or_else(|| {
        failure(
            "app_data.migration_record_has_no_parent_directory",
            "迁移记录没有父目录",
        )
    })?)
}
fn restore_copy(from: &Path, to: &Path, temporary: &Path) -> Result<(), AppError> {
    plain(from)?;
    check_ancestors(to)?;
    fs::copy(from, temporary).map_err(io)?;
    // Windows 刷盘要求写句柄；句柄在替换目标之前关闭。
    fs::OpenOptions::new()
        .write(true)
        .open(temporary)
        .and_then(|f| f.sync_all())
        .map_err(io)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        let a: Vec<_> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let b: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe {
            MoveFileExW(
                a.as_ptr(),
                b.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(io(std::io::Error::last_os_error()));
        }
    }
    #[cfg(not(windows))]
    fs::rename(temporary, to).map_err(io)?;
    sync_dir(to.parent().ok_or_else(|| {
        failure(
            "app_data.recovery_destination_has_no_parent_directory",
            "恢复目标没有父目录",
        )
    })?)
}
fn load<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, AppError> {
    use std::io::Read;
    plain(path)?;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(io)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(failure(
            "app_data.migration_record_is_too_large",
            "迁移记录过大",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        failure(
            "app_data.migration_record_is_damaged_data_has_been_preserved_for_recovery",
            "迁移记录损坏，保留现场等待恢复",
        )
    })
}
// 回滚可能被再次中断：原文件保存在 original-*，恢复副本不消耗该备份；restore-* 证明恢复已完成。
fn rollback(plan: &Plan, journal: &Path) -> Result<(), AppError> {
    for (i, relative) in plan.replacements.iter().enumerate().rev() {
        let original = journal.join(format!("original-{i}"));
        let restored = journal.join(format!("restored-{i}"));
        if exists(&original)? && !exists(&restored)? {
            let target = plan.new.join(relative);
            restore_copy(&original, &target, &journal.join(format!("restore-{i}")))?;
            save(&restored, &true)?;
        }
    }
    for name in plan.items.iter().rev() {
        let from = plan.new.join(name);
        let to = plan.old.join(name);
        match (exists(&from)?, exists(&to)?) {
            (true, false) => rename(&from, &to)?,
            (false, true) => {}
            _ => {
                return Err(failure(
                    "app_data.rollback_found_missing_or_conflicting_data_the_migration_record_has",
                    "回滚遇到数据缺失或两端同名，保留迁移记录",
                )
                .with("path", name));
            }
        }
    }
    cleanup(journal)
}
fn cleanup(journal: &Path) -> Result<(), AppError> {
    // 先移出活动标记再删除已结束的临时工作区；重启只需清理这一固定垃圾目录。
    let garbage = journal.with_file_name(".pierlink-migration-finished");
    rename(journal, &garbage)?;
    fs::remove_dir_all(&garbage).map_err(io)?;
    sync_dir(journal.parent().ok_or_else(|| {
        failure(
            "app_data.migration_directory_has_no_parent",
            "迁移目录没有父目录",
        )
    })?)
}
fn stop(old: &Path, new: &Path) -> Result<Vec<home::HomeLock>, AppError> {
    let mut locks = vec![];
    let original_home = old.join("syncthing");
    if exists(&original_home)? {
        locks.push(process::stop_for_migration(&original_home, &original_home)?);
        if exists(&new.join("syncthing"))? {
            locks.push(process::stop_for_migration(
                &new.join("syncthing"),
                &original_home,
            )?);
        }
    }
    Ok(locks)
}

/// 启动早于数据库和 Syncthing 时，将 old 的数据迁往 new，返回搬运、跳过及路径改写数。
/// 新目录已有数据库或旧目录没有数据库时返回 None；未提交中断先回滚再重试。
/// 错误含 safe_to_use_old=true 时可回退旧目录；app.data_migration_recovery_required 必须停止启动并重试恢复。
pub fn migrate_data_dir(old: &Path, new: &Path) -> Result<Option<MigrationReport>, AppError> {
    migrate(old, new, &mut |_, _| Ok(())).map_err(|error| {
        if error.code == "app.data_migration_recovery_required" {
            return error;
        }
        match exists(&new.join(JOURNAL)) {
            Ok(false) => error.with("safe_to_use_old", "true"),
            _ => recovery_required(error),
        }
    })
}
fn migrate(
    old: &Path,
    new: &Path,
    checkpoint: &mut impl FnMut(&str, usize) -> Result<(), AppError>,
) -> Result<Option<MigrationReport>, AppError> {
    if !old.is_absolute() || !new.is_absolute() {
        return Err(failure(
            "app_data.migration_directory_must_be_an_absolute_path",
            "迁移目录必须是绝对路径",
        ));
    }
    check_ancestors(old)?;
    check_ancestors(new)?;
    let old = crate::syncthing::folder::normalized(old)?;
    let new = crate::syncthing::folder::normalized(new)?;
    if crate::storage::paths_overlap(&old.to_string_lossy(), &new.to_string_lossy()) {
        return Err(failure(
            "app_data.old_and_new_data_directories_cannot_overlap",
            "新旧数据目录不能相同或互相包含",
        ));
    }
    let journal = new.join(JOURNAL);
    if !exists(&journal)?
        && !exists(&new.join(".pierlink-migration-finished"))?
        && (exists(&new.join("pierlink.db"))? || !exists(&old.join("pierlink.db"))?)
    {
        return Ok(None);
    }
    fs::create_dir_all(&new).map_err(io)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(new.join(".pierlink-migration.lock"))
        .map_err(io)?;
    lock.try_lock().map_err(|_| {
        failure(
            "app_data.another_data_migration_is_running",
            "另一项数据迁移正在运行",
        )
    })?;
    let _lock = MigrationLock(lock);
    let garbage = new.join(".pierlink-migration-finished");
    if exists(&garbage)? {
        plain(&garbage)?;
        fs::remove_dir_all(&garbage).map_err(io)?;
    }
    if !exists(&journal)?
        && (exists(&new.join("pierlink.db"))? || !exists(&old.join("pierlink.db"))?)
    {
        return Ok(None);
    }
    let _homes = stop(&old, &new)?;
    if exists(&journal)? {
        plain(&journal)?;
        if exists(&journal.join("committed.json"))? {
            let p: Plan = load(&journal.join("plan.json"))?;
            validate(&p, &old, &new)?;
            let report = load(&journal.join("committed.json"))?;
            cleanup(&journal)?;
            return Ok(Some(report));
        }
        if exists(&journal.join("plan.json"))? {
            let p: Plan = load(&journal.join("plan.json"))?;
            validate(&p, &old, &new)?;
            rollback(&p, &journal).map_err(recovery_required)?;
        } else {
            cleanup(&journal)?;
        }
    }
    if exists(&new.join("pierlink.db"))? || !exists(&old.join("pierlink.db"))? {
        return Ok(None);
    }
    let p = plan(&old, &new)?;
    home::private_dir(&journal)?;
    save(&journal.join("plan.json"), &p)?;
    let result: Result<MigrationReport, AppError> = (|| {
        let rewritten = rewrite::prepare(&p, &journal)?;
        if p.items.iter().any(|p| p.starts_with("syncthing/")) {
            home::private_dir(&new.join("syncthing"))?;
        }
        for (i, name) in p.items.iter().enumerate() {
            checkpoint("move", i)?;
            rename(&old.join(name), &new.join(name)).map_err(|e| e.with("path", name))?;
        }
        for (i, name) in p.replacements.iter().enumerate() {
            checkpoint("rewrite", i)?;
            rename(&new.join(name), &journal.join(format!("original-{i}")))?;
            let prepared = journal.join("prepared").join(name);
            if exists(&prepared)? {
                rename(&prepared, &new.join(name))?;
            }
        }
        let report = MigrationReport {
            moved: p.items.clone(),
            skipped: p.skipped.clone(),
            rewritten,
        };
        checkpoint("commit", 0)?;
        save(&journal.join("committed.json"), &report)?;
        Ok(report)
    })();
    match result {
        Ok(report) => {
            checkpoint("cleanup", 0)?;
            cleanup(&journal).map_err(recovery_required)?;
            Ok(Some(report))
        }
        Err(error) => {
            rollback(&p, &journal)
                .map_err(|e| recovery_required(e).with("cause_code", &error.code))?;
            Err(error.with("safe_to_use_old", "true"))
        }
    }
}
fn recovery_required(error: AppError) -> AppError {
    let mut result = AppError::new("app.data_migration_recovery_required")
        .with_reason(
            "app_data.migration_is_incomplete_preserve_both_directories_and_retry_recovery_on",
            "迁移尚未安全完成，必须停止启动并保留两端数据，下次重试恢复",
        )
        .with("cause_code", error.code);
    for (key, value) in error.params {
        result = result.with(format!("cause_{key}"), value);
    }
    result
}
