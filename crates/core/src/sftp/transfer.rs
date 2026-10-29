//! 先扫描、后传输；普通文件使用同目录临时文件及改名，符号链接始终跳过。
use super::{EntryKind, RemoteEntry, SftpSession, cancelled, join_path};
use crate::{error::AppError, fs_local};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, UNIX_EPOCH},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
pub enum Conflict {
    Error,
    Overwrite,
    Skip,
}
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct TransferProgress {
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub current: String,
    pub files_done: u32,
    pub files_total: u32,
}
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct TransferIssue {
    pub path: String,
    pub reason: AppError,
}
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct RenamedItem {
    pub from: String,
    pub to: String,
}
#[derive(Debug, Clone, Default, serde::Serialize, specta::Type)]
pub struct TransferReport {
    pub succeeded: u32,
    pub skipped: Vec<TransferIssue>,
    pub renamed: Vec<RenamedItem>,
    pub failed: Vec<TransferIssue>,
    /// 已完成操作的提示，例如无扩展时覆盖存在短暂空窗。
    pub warnings: Vec<TransferIssue>,
}
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct TransferPlan {
    pub files_total: u32,
    pub total_bytes: u64,
    pub existing: Vec<String>,
    pub skipped: Vec<TransferIssue>,
    pub renamed: Vec<RenamedItem>,
    #[serde(skip)]
    #[specta(skip)]
    entries: Vec<PlannedEntry>,
}
#[derive(Debug, Clone)]
struct PlannedEntry {
    local: PathBuf,
    remote: String,
    kind: EntryKind,
    size: u64,
    mtime: i64,
    mode: u32,
}
const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 128;
fn failure(reason: &str) -> AppError {
    AppError::new("transfer.failure").with("reason", reason)
}
fn issue(path: impl ToString, reason: &str) -> TransferIssue {
    TransferIssue {
        path: path.to_string(),
        reason: AppError::new("transfer.skipped").with("reason", reason),
    }
}
fn empty_plan() -> TransferPlan {
    TransferPlan {
        files_total: 0,
        total_bytes: 0,
        existing: vec![],
        skipped: vec![],
        renamed: vec![],
        entries: vec![],
    }
}
fn component(value: &str) -> Result<(), AppError> {
    if value.is_empty() || value == "." || value == ".." || value.contains(['/', '\0']) {
        return Err(failure("来源名字为空或包含非法路径分隔符"));
    }
    Ok(())
}
/// 将单个远端名字 name 转成 Windows 可用名字，替换非法字符、尾部点和空格，并给保留名加前缀。
pub fn windows_filename(name: &str) -> String {
    let mut result: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let end = result.trim_end_matches(['.', ' ']).len();
    result = result[..end].to_owned() + &"_".repeat(result.len() - end);
    if result.is_empty() {
        result.push('_');
    }
    let stem = result
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_uppercase();
    let numbered = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|n| {
            matches!(
                n,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered
    {
        result.insert(0, '_');
    }
    result
}
impl TransferPlan {
    fn push(&mut self, item: PlannedEntry) -> Result<(), AppError> {
        if self.entries.len() >= MAX_ENTRIES {
            return Err(failure("传输项超过十万，请分批选择"));
        }
        if item.kind == EntryKind::File {
            self.files_total += 1;
            self.total_bytes = self
                .total_bytes
                .checked_add(item.size)
                .ok_or_else(|| failure("总字节数溢出"))?;
        }
        self.entries.push(item);
        Ok(())
    }
    fn validate_targets(&self, upload: bool) -> Result<(), AppError> {
        let mut seen = HashSet::new();
        for item in &self.entries {
            let name = if upload {
                item.remote.clone()
            } else {
                item.local.to_string_lossy().into_owned()
            };
            let key = if !upload && cfg!(windows) {
                name.to_lowercase()
            } else {
                name.clone()
            };
            if !seen.insert(key) {
                return Err(AppError::new("transfer.exists")
                    .with("reason", "多个来源映射到同一目标，请分别传输")
                    .with("items", name));
            }
        }
        Ok(())
    }
}
fn local_metadata(path: &Path) -> Result<Option<fs::Metadata>, AppError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(fs_local::error(e, "读取本机属性", path)),
    }
}
fn ensure_local_dir(path: &Path) -> Result<(), AppError> {
    match local_metadata(path)? {
        Some(m) if m.is_dir() && !m.file_type().is_symlink() => Ok(()),
        _ => Err(failure("本机目标目录不存在、不是目录或是符号链接").with("path", path.display())),
    }
}
fn ensure_remote_dir(session: &mut SftpSession, path: &str) -> Result<(), AppError> {
    if session
        .stat(path)?
        .is_some_and(|e| e.kind == EntryKind::Dir)
    {
        Ok(())
    } else {
        Err(failure("远端目标目录不存在、不是目录或是符号链接").with("path", path))
    }
}
fn depth(depth: usize) -> Result<(), AppError> {
    if depth > MAX_DEPTH {
        Err(failure("目录层级超过 128 层，请分批选择"))
    } else {
        Ok(())
    }
}
fn scan_upload(
    session: &mut SftpSession,
    local: &Path,
    remote: &str,
    plan: &mut TransferPlan,
    level: usize,
) -> Result<(), AppError> {
    depth(level)?;
    let meta =
        fs::symlink_metadata(local).map_err(|e| fs_local::error(e, "扫描上传来源", local))?;
    let kind = fs_local::kind(&meta);
    if !matches!(kind, EntryKind::File | EntryKind::Dir) {
        plan.skipped
            .push(issue(local.display(), "不跟随符号链接，也不传输特殊文件"));
        return Ok(());
    }
    // 冲突预检只列顶层；子项执行前再检查，避免重复远端查询或穿过类型冲突的父项。
    if level == 0 && session.stat(remote)?.is_some() {
        plan.existing.push(remote.into());
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o777
    };
    #[cfg(not(unix))]
    let mode = if kind == EntryKind::Dir { 0o755 } else { 0o644 };
    let mtime = fs_local::unix_seconds(
        meta.modified()
            .map_err(|e| fs_local::error(e, "读取上传修改时间", local))?,
    );
    plan.push(PlannedEntry {
        local: local.into(),
        remote: remote.into(),
        kind,
        size: meta.len(),
        mtime,
        mode,
    })?;
    if kind == EntryKind::Dir {
        for item in fs::read_dir(local).map_err(|e| fs_local::error(e, "扫描上传目录", local))?
        {
            let item = item.map_err(|e| fs_local::error(e, "扫描上传条目", local))?;
            let name = item.file_name();
            let Some(name) = name.to_str() else {
                plan.skipped
                    .push(issue(item.path().display(), "文件名不是 UTF-8"));
                continue;
            };
            component(name)?;
            scan_upload(
                session,
                &item.path(),
                &join_path(remote, name),
                plan,
                level + 1,
            )?;
        }
    }
    Ok(())
}
/// 扫描 sources 文件和目录到 remote_dir，返回文件总量、已存在目标和跳过项；不修改文件。
pub fn plan_upload(
    session: &mut SftpSession,
    sources: &[PathBuf],
    remote_dir: &str,
) -> Result<TransferPlan, AppError> {
    // 用户主动选择的目标目录可以是链接；先解析根目录，再拒绝递归目标里的链接。
    let remote_dir = session.realpath(remote_dir)?;
    ensure_remote_dir(session, &remote_dir)?;
    let mut plan = empty_plan();
    for source in sources {
        let name = source
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| failure("上传来源必须有 UTF-8 文件名"))?;
        component(name)?;
        // 来源根目录的链接仍由扫描器跳过，只解析父目录以保留此语义。
        let parent = source
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let source = fs_local::plain_canonical(parent)
            .map_err(|e| fs_local::error(e, "解析上传来源目录", parent))?
            .join(name);
        scan_upload(
            session,
            &source,
            &join_path(&remote_dir, name),
            &mut plan,
            0,
        )?;
    }
    plan.validate_targets(true)?;
    Ok(plan)
}
fn scan_download(
    session: &mut SftpSession,
    remote: &str,
    local: Option<&Path>,
    metadata: RemoteEntry,
    plan: &mut TransferPlan,
    level: usize,
) -> Result<(), AppError> {
    depth(level)?;
    if !matches!(metadata.kind, EntryKind::File | EntryKind::Dir) {
        plan.skipped
            .push(issue(remote, "不跟随符号链接，也不传输特殊文件"));
        return Ok(());
    }
    if level == 0
        && let Some(local) = local
        && local_metadata(local)?.is_some()
    {
        plan.existing.push(local.display().to_string());
    }
    let kind = metadata.kind;
    plan.push(PlannedEntry {
        local: local.unwrap_or_else(|| Path::new("")).into(),
        remote: remote.into(),
        kind,
        size: metadata.size,
        mtime: metadata.mtime,
        mode: metadata.mode,
    })?;
    if kind == EntryKind::Dir {
        let listing = session.list_dir(remote, MAX_ENTRIES)?;
        if listing.truncated {
            return Err(failure("远端目录超过十万项，请分批选择"));
        }
        if listing.skipped_non_utf8 > 0 {
            plan.skipped.push(issue(
                remote,
                &format!("跳过 {} 个非 UTF-8 名字", listing.skipped_non_utf8),
            ));
        }
        for entry in listing.entries {
            let target = local
                .map(|parent| download_name(&entry.name, parent, plan))
                .transpose()?;
            scan_download(
                session,
                &join_path(remote, &entry.name),
                target.as_deref(),
                entry,
                plan,
                level + 1,
            )?;
        }
    }
    Ok(())
}
fn download_name(name: &str, parent: &Path, plan: &mut TransferPlan) -> Result<PathBuf, AppError> {
    component(name)?;
    let safe = if cfg!(windows) {
        windows_filename(name)
    } else {
        name.into()
    };
    let target = parent.join(&safe);
    if name != safe {
        plan.renamed.push(RenamedItem {
            from: name.into(),
            to: target.display().to_string(),
        });
    }
    Ok(target)
}
/// 扫描远端 sources 到 local_dir；Windows 名字在计划阶段替换并检测映射碰撞，返回总量及冲突列表。
pub fn plan_download(
    session: &mut SftpSession,
    sources: &[String],
    local_dir: &Path,
) -> Result<TransferPlan, AppError> {
    let local_dir = fs_local::plain_canonical(local_dir)
        .map_err(|e| fs_local::error(e, "解析下载目标目录", local_dir))?;
    ensure_local_dir(&local_dir)?;
    let mut plan = empty_plan();
    for source in sources {
        let source = source.trim_end_matches('/');
        let name = source
            .rsplit('/')
            .next()
            .ok_or_else(|| failure("下载来源没有文件名"))?;
        let target = download_name(name, &local_dir, &mut plan)?;
        let metadata = session
            .stat(source)?
            .ok_or_else(|| AppError::new("sftp.not_found").with("path", source))?;
        scan_download(session, source, Some(&target), metadata, &mut plan, 0)?;
    }
    plan.validate_targets(false)?;
    Ok(plan)
}
/// 只扫描远端 sources 的递归文件数和字节数；跳过符号链接，不读取本机目录或检查同名目标。
/// 返回的 existing 和 renamed 为空；来源不存在或扫描失败时返回相应错误。
pub fn plan_remote(
    session: &mut SftpSession,
    sources: &[String],
) -> Result<TransferPlan, AppError> {
    let mut plan = empty_plan();
    for source in sources {
        let source = source.trim_end_matches('/');
        component(source.rsplit('/').next().unwrap_or(""))?;
        let metadata = session
            .stat(source)?
            .ok_or_else(|| AppError::new("sftp.not_found").with("path", source))?;
        scan_download(session, source, None, metadata, &mut plan, 0)?;
    }
    plan.validate_targets(true)?;
    Ok(plan)
}
fn preflight(plan: &TransferPlan, conflict: Conflict) -> Result<(), AppError> {
    if conflict == Conflict::Error && !plan.existing.is_empty() {
        return Err(AppError::new("transfer.exists")
            .with("reason", "目标已存在，请选择冲突策略")
            .with(
                "items",
                serde_json::to_string(&plan.existing).map_err(|_| failure("冲突列表编码失败"))?,
            ));
    }
    Ok(())
}
fn remote_part(
    session: &mut SftpSession,
    item: &PlannedEntry,
    overwrite: bool,
    progress: &mut dyn FnMut(u64),
    cancel: &AtomicBool,
) -> Result<(u64, bool), AppError> {
    let mut file =
        File::open(&item.local).map_err(|e| fs_local::error(e, "读取上传文件", &item.local))?;
    if !file
        .metadata()
        .map_err(|e| fs_local::error(e, "读取上传属性", &item.local))?
        .is_file()
        || fs::symlink_metadata(&item.local)
            .map_err(|e| fs_local::error(e, "读取上传属性", &item.local))?
            .file_type()
            .is_symlink()
    {
        return Err(failure("上传来源已变成符号链接或特殊文件"));
    }
    let part = format!("{}.pierlink-part", item.remote);
    // 固定后缀的残留文件在排他创建失败且确认存在时删除，只重试一次。
    // 只删除文件或链接；同名目录作为错误报告，不递归删除。
    let handle = match session.open_write(&part, Some(item.mode), true) {
        Ok(handle) => handle,
        Err(error) => {
            if !terminal_error(&error)
                && session
                    .stat(&part)?
                    .is_some_and(|e| matches!(e.kind, EntryKind::File | EntryKind::Symlink))
            {
                session.remove_file(&part)?;
                session.open_write(&part, Some(item.mode), true)
            } else {
                Err(error)
            }?
        }
    };
    let result = (|| {
        let bytes = session.with_handle(handle, |session, handle| {
            session.write_handle(handle, &mut file, progress, cancel)
        })?;
        cancelled(cancel)?;
        session.set_mtime(&part, item.mtime)?;
        let non_atomic = session.rename_file(&part, &item.remote, overwrite)?;
        Ok((bytes, non_atomic))
    })();
    if result.is_err()
        && let Err(cleanup) = session.cleanup_file(&part)
        && cleanup.code != "sftp.not_found"
    {
        return result.map_err(|e: AppError| {
            e.with("cleanup_path", &part)
                .with("cleanup_error", cleanup.code)
        });
    }
    result
}
struct LocalPart {
    path: PathBuf,
    file: Option<File>,
    committed: bool,
}
impl Drop for LocalPart {
    fn drop(&mut self) {
        self.file.take();
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}
fn set_mtime(file: &File, seconds: i64) -> io::Result<()> {
    let time = if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
    }
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "修改时间超出本机范围"))?;
    file.set_times(fs::FileTimes::new().set_modified(time))
}
fn local_part(
    session: &mut SftpSession,
    item: &PlannedEntry,
    overwrite: bool,
    progress: &mut dyn FnMut(u64),
    cancel: &AtomicBool,
) -> Result<u64, AppError> {
    let mut name = item.local.as_os_str().to_owned();
    name.push(".pierlink-part");
    let path = PathBuf::from(name);
    let create = || OpenOptions::new().write(true).create_new(true).open(&path);
    let file = match create() {
        Err(error)
            if error.kind() == io::ErrorKind::AlreadyExists
                && local_metadata(&path)?
                    .is_some_and(|m| m.is_file() || m.file_type().is_symlink()) =>
        {
            fs::remove_file(&path)
                .map_err(|e| fs_local::error(e, "删除残留下载临时文件", &path))?;
            create()
        }
        result => result,
    }
    .map_err(|e| fs_local::error(e, "创建下载临时文件", &path))?;
    let mut part = LocalPart {
        path,
        file: Some(file),
        committed: false,
    };
    let result = (|| {
        let file = part
            .file
            .as_mut()
            .ok_or_else(|| failure("下载临时文件已关闭"))?;
        let bytes = session.read_to(&item.remote, file, progress, cancel)?;
        cancelled(cancel)?;
        set_mtime(file, item.mtime)
            .and_then(|_| file.sync_all())
            .map_err(|e| fs_local::error(e, "保存下载临时文件", &part.path))?;
        part.file.take();
        complete_local(&part.path, &item.local, overwrite).map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                AppError::new("transfer.exists").with("path", item.local.display())
            } else {
                fs_local::error(e, "完成下载改名", &item.local)
            }
        })?;
        part.committed = true;
        Ok(bytes)
    })();
    if result.is_err() {
        part.file.take();
        match fs::remove_file(&part.path) {
            Ok(()) => part.committed = true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => part.committed = true,
            Err(e) => {
                let cleanup = fs_local::error(e, "清理下载临时文件", &part.path);
                return result.map_err(|e: AppError| {
                    e.with("cleanup_path", part.path.display())
                        .with("cleanup_error", cleanup.code)
                });
            }
        }
    }
    result
}
fn complete_local(from: &Path, to: &Path, overwrite: bool) -> io::Result<()> {
    if overwrite {
        return fs::rename(from, to);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let to = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        // 原子拒绝覆盖，无需文件系统支持硬链接（例如 FAT 分区）。
        if unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
        let from: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        fs::hard_link(from, to)?;
        fs::remove_file(from)
    }
}
fn terminal_error(error: &AppError) -> bool {
    matches!(
        error.code.as_str(),
        "sftp.cancelled" | "sftp.connection_lost" | "sftp.protocol"
    )
}
fn transfer(
    session: &mut SftpSession,
    plan: TransferPlan,
    upload: bool,
    conflict: Conflict,
    callback: &mut dyn FnMut(&TransferProgress),
    cancel: &AtomicBool,
) -> Result<TransferReport, AppError> {
    preflight(&plan, conflict)?;
    let mut report = TransferReport {
        skipped: plan.skipped,
        renamed: plan.renamed,
        ..Default::default()
    };
    let mut progress = TransferProgress {
        total_bytes: plan.total_bytes,
        done_bytes: 0,
        current: String::new(),
        files_done: 0,
        files_total: plan.files_total,
    };
    callback(&progress);
    let mut skipped_dirs: Vec<PlannedEntry> = vec![];
    for item in plan.entries {
        if let Err(e) = cancelled(cancel) {
            return Err(with_report(e, &report));
        }
        progress.current = if upload {
            item.remote.clone()
        } else {
            item.local.display().to_string()
        };
        let inherited = skipped_dirs.iter().any(|parent| {
            if upload {
                item.remote.starts_with(&format!("{}/", parent.remote))
            } else {
                item.local.starts_with(&parent.local)
            }
        });
        let result = (|| {
            if inherited {
                return Ok(false);
            }
            // 执行前重查类型；同名目录合并，类型冲突不能删除整个用户目录。
            let target = if upload {
                session.stat(&item.remote)?.map(|e| e.kind)
            } else {
                local_metadata(&item.local)?.map(|m| fs_local::kind(&m))
            };
            let exists = target.is_some();
            if exists && conflict == Conflict::Error {
                return Err(AppError::new("transfer.exists").with("path", &progress.current));
            }
            if exists && !(item.kind == EntryKind::Dir && target == Some(EntryKind::Dir)) {
                if conflict == Conflict::Skip {
                    return Ok(false);
                }
                if target != Some(item.kind) {
                    return Err(failure("来源与目标类型冲突，保留目标不作替换"));
                }
            }
            if item.kind == EntryKind::Dir {
                if exists {
                    if upload {
                        ensure_remote_dir(session, &item.remote)?;
                    } else {
                        ensure_local_dir(&item.local)?;
                    }
                } else if upload {
                    session.mkdir(&item.remote)?;
                } else {
                    fs::create_dir(&item.local)
                        .map_err(|e| fs_local::error(e, "创建下载目录", &item.local))?;
                }
                return Ok(true);
            }
            if !upload {
                ensure_local_dir(
                    item.local
                        .parent()
                        .ok_or_else(|| failure("下载文件没有父目录"))?,
                )?;
            }
            let before = progress.done_bytes;
            let mut on_bytes = |bytes| {
                progress.done_bytes = before.saturating_add(bytes);
                callback(&progress);
            };
            if upload {
                let (_, non_atomic) = remote_part(
                    session,
                    &item,
                    conflict == Conflict::Overwrite,
                    &mut on_bytes,
                    cancel,
                )?;
                if non_atomic {
                    report.warnings.push(TransferIssue {
                        path: item.remote.clone(),
                        reason: AppError::new("transfer.non_atomic_overwrite").with(
                            "reason",
                            "服务器不支持原子覆盖；先删除旧目标再改名，存在短暂空窗",
                        ),
                    });
                }
            } else {
                local_part(
                    session,
                    &item,
                    conflict == Conflict::Overwrite,
                    &mut on_bytes,
                    cancel,
                )?;
            }
            Ok(true)
        })();
        match result {
            Ok(true) if item.kind == EntryKind::File => report.succeeded += 1,
            Ok(false) => {
                report.skipped.push(issue(
                    &progress.current,
                    "同名目标按策略跳过，或父目录已跳过",
                ));
                if item.kind == EntryKind::Dir {
                    skipped_dirs.push(item.clone());
                }
            }
            Err(e) => {
                report.failed.push(TransferIssue {
                    path: progress.current.clone(),
                    reason: e.clone(),
                });
                if terminal_error(&e) {
                    return Err(with_report(e, &report));
                }
                if item.kind == EntryKind::Dir {
                    skipped_dirs.push(item.clone());
                }
            }
            _ => (),
        }
        if item.kind == EntryKind::File {
            progress.files_done += 1;
        }
        callback(&progress);
    }
    Ok(report)
}
fn with_report(error: AppError, report: &TransferReport) -> AppError {
    match serde_json::to_string(report) {
        Ok(report) => error.with("report", report),
        Err(_) => error,
    }
}
/// 按 conflict 上传 sources 到 remote_dir；先列顶层冲突，progress 回调累计量，cancel 可取消；文件级失败写入报告。
pub fn upload(
    session: &mut SftpSession,
    sources: &[PathBuf],
    remote_dir: &str,
    conflict: Conflict,
    progress: &mut dyn FnMut(&TransferProgress),
    cancel: &AtomicBool,
) -> Result<TransferReport, AppError> {
    cancelled(cancel)?;
    let plan = plan_upload(session, sources, remote_dir)?;
    transfer(session, plan, true, conflict, progress, cancel)
}
/// 按 conflict 下载远端 sources 到 local_dir，递归创建空目录、保留文件修改时间；连接故障或取消返回带部分报告的错误。
pub fn download(
    session: &mut SftpSession,
    sources: &[String],
    local_dir: &Path,
    conflict: Conflict,
    progress: &mut dyn FnMut(&TransferProgress),
    cancel: &AtomicBool,
) -> Result<TransferReport, AppError> {
    cancelled(cancel)?;
    let plan = plan_download(session, sources, local_dir)?;
    transfer(session, plan, false, conflict, progress, cancel)
}

#[cfg(test)]
mod tests;
