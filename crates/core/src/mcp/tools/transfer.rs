//! 在 Windows 和一台授权的服务器之间传单个文件，走 Pierlink 的 SFTP 连接（和文件栏的上传下载一样）。
//! Windows 一侧受授权文件夹和受保护目录限制；先检查、再审批、再传，传的时候盯着取消、期限和服务器权限。
use super::{
    ActionApproval, Failure, check_cancel, command_allowed,
    files::{self, Sandbox},
    login_needed, number, required, schema,
};
use crate::{
    error::AppError,
    fs_local,
    mcp::{
        Entry, Permissions, Target, TargetKind, lock, service::Shared, session::cancelled_error,
    },
    sftp::{EntryKind, SftpSession, transfer},
};
use rmcp::model::JsonObject;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// 一个文件最多多少字节
const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// 多久核对一次服务器权限
const PERMISSION_CHECK: Duration = Duration::from_millis(500);

/// 传输为什么停下
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Cancelled,
    Timeout,
    Revoked,
}

/// 一次传输的两端
struct Ends<'a> {
    from: &'a Target,
    to: &'a Target,
    from_path: &'a str,
    to_path: &'a str,
    /// 从 Windows 传到服务器
    upload: bool,
    /// Windows 一侧规范化后的路径
    local: PathBuf,
}
impl Ends<'_> {
    fn server(&self) -> &Target {
        if self.upload { self.to } else { self.from }
    }
}

fn too_large() -> AppError {
    AppError::new("mcp.file_too_large")
        .with("max_bytes", MAX_BYTES)
        .with_reason("mcp.file_exceeds_the_size_limit", "文件超过大小限制")
}

fn exists(path: &str) -> AppError {
    AppError::new("mcp.path_exists")
        .with("path", path)
        .with_reason(
            "mcp.rename_target_exists",
            "目标已存在，请选择其他路径或设置 overwrite",
        )
}

/// transfer_file：在 Windows 和服务器之间传一个文件，阻塞到传完、出错、取消或超时。
///
/// :param shared: 服务共享状态（宿主、权限核对）
/// :param permissions: 这个入口的权限
/// :param targets: 这个入口能操作的目标
/// :param args: 工具参数
/// :param cancelled: 调用取消标志
/// :param approval: 本次调用的审批
/// :return: {from_target, from_path, to_target, to_path, bytes, duration_ms}
pub(super) fn run(
    shared: &Shared,
    permissions: &Permissions,
    targets: &[Target],
    args: &JsonObject,
    cancelled: &AtomicBool,
    approval: &mut ActionApproval<'_>,
) -> Result<Value, Failure> {
    let from = super::target(targets, required(args, "from_target"))?;
    let to = super::target(targets, required(args, "to_target"))?;
    let (from_path, to_path) = (required(args, "from_path"), required(args, "to_path"));
    let overwrite = args
        .get("overwrite")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let upload = match (from.kind, to.kind) {
        (TargetKind::Windows, TargetKind::Server) => true,
        (TargetKind::Server, TargetKind::Windows) => false,
        _ => {
            return Err(schema::invalid(
                "mcp.transfer_needs_windows_and_a_server",
                "一端要是 windows、另一端是服务器；Windows 里复制用 copy_path，两台服务器之间先传到 Windows",
            )
            .into());
        }
    };
    let server_path = if upload { to_path } else { from_path };
    if !server_path.starts_with('/') || server_path.contains('\0') {
        return Err(schema::invalid(
            "mcp.server_path_must_be_absolute",
            "服务器上的路径要用绝对路径（以 / 开头）",
        )
        .into());
    }
    if permissions.folders.is_empty() {
        return Err(super::no_folders().into());
    }
    let sandbox = Sandbox::new(&permissions.folders);
    let local = if upload {
        sandbox.resolve_read(from_path)?
    } else {
        sandbox.resolve_write(to_path)?
    };
    let ends = Ends {
        from,
        to,
        from_path,
        to_path,
        upload,
        local,
    };
    check_cancel(cancelled)?;
    let started = Instant::now();
    let mut session = shared
        .host
        .open_sftp(&ends.server().id)
        .inspect_err(|error| login_needed(shared.host.as_ref(), ends.server(), error))?;
    // 注定失败的（不存在、不是普通文件、太大、目标已有又没给 overwrite）不问用户
    let size = check(&mut session, &ends, overwrite)?;
    let reason = if upload {
        files::write_needs_confirmation(Path::new(to_path))
    } else {
        files::write_needs_confirmation(&ends.local)
            .or_else(|| files::write_needs_confirmation(Path::new(to_path)))
    };
    approval.confirm(
        to,
        &action(&ends, size, overwrite, reason.as_deref()),
        reason.is_some(),
        cancelled,
    )?;
    check_cancel(cancelled)?;
    if upload {
        remote_parents(&mut session, to_path)?;
    } else if let Some(parent) = ends.local.parent() {
        files::make_dir(&sandbox, &parent.to_string_lossy())?;
    }
    let entry = approval.session.snapshot().entry;
    let timeout = Duration::from_secs(number(args, "timeout_secs", 600));
    let ((result, oversized), stop) =
        watch(shared, &entry, ends.server(), cancelled, timeout, |stop| {
            let oversized = AtomicBool::new(false);
            let mut progress = |bytes: u64| {
                if bytes > MAX_BYTES {
                    oversized.store(true, Ordering::Release);
                    stop.store(true, Ordering::Release);
                }
            };
            let result = if upload {
                transfer::upload_file(
                    &mut session,
                    &ends.local,
                    to_path,
                    overwrite,
                    &mut progress,
                    stop,
                )
            } else {
                transfer::download_file(
                    &mut session,
                    from_path,
                    &ends.local,
                    overwrite,
                    &mut progress,
                    stop,
                )
            };
            (result, oversized.load(Ordering::Acquire))
        });
    match stop {
        Some(Stop::Cancelled) => return Err(cancelled_error().into()),
        Some(Stop::Timeout) => {
            return Err(AppError::new("mcp.transfer_timeout")
                .with("timeout_secs", timeout.as_secs())
                .with_reason(
                    "mcp.transfer_timed_out",
                    "传输超过 timeout_secs，已停止并删掉没传完的文件",
                )
                .into());
        }
        Some(Stop::Revoked) => {
            return Err(AppError::new("mcp.target_denied")
                .with_reason(
                    "mcp.transfer_permission_revoked",
                    "传输期间收回了这台服务器的权限，已停止并删掉没传完的文件",
                )
                .into());
        }
        None if oversized => return Err(too_large().into()),
        None => {}
    }
    let bytes =
        result.inspect_err(|error| login_needed(shared.host.as_ref(), ends.server(), error))?;
    let windows = fs_local::display_path(&ends.local);
    Ok(json!({
        "from_target": from.target,
        "from_path": if upload { windows.clone() } else { from_path.to_owned() },
        "to_target": to.target,
        "to_path": if upload { to_path.to_owned() } else { windows },
        "bytes": bytes,
        "duration_ms": started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    }))
}

/// 传之前检查两端：源要是普通文件、不超过 2 GiB；目标已存在时要 overwrite，而且是普通文件。返回源的大小
fn check(session: &mut SftpSession, ends: &Ends<'_>, overwrite: bool) -> Result<u64, AppError> {
    if ends.upload {
        let file = files::open_file(&ends.local)?;
        let size = file
            .metadata()
            .map_err(|e| files::io_error(e, &ends.local))?
            .len();
        if size > MAX_BYTES {
            return Err(too_large());
        }
        // 路径上已经有的部分要是目录：注定失败的不问用户
        remote_parents_ok(session, ends.to_path)?;
        match session.stat(ends.to_path)? {
            Some(_) if !overwrite => Err(exists(ends.to_path)),
            Some(entry) if entry.kind != EntryKind::File => Err(not_a_file(ends.to_path)),
            _ => Ok(size),
        }
    } else {
        let entry = session
            .stat(ends.from_path)?
            .ok_or_else(|| AppError::new("sftp.not_found").with("path", ends.from_path))?;
        if entry.kind != EntryKind::File {
            return Err(not_a_file(ends.from_path));
        }
        if entry.size > MAX_BYTES {
            return Err(too_large());
        }
        match files::metadata_if_exists(&ends.local)? {
            Some(_) if !overwrite => Err(exists(&fs_local::display_path(&ends.local))),
            Some(metadata) if !metadata.is_file() || files::is_link(&metadata) => {
                Err(not_a_file(&fs_local::display_path(&ends.local)))
            }
            _ => Ok(entry.size),
        }
    }
}

fn not_a_file(path: &str) -> AppError {
    AppError::new("mcp.path_invalid")
        .with("path", path)
        .with_reason(
            "sftp.only_regular_files_can_be_transferred",
            "只能传普通文件，不跟随链接",
        )
}

/// 确认框里的说明
fn action(ends: &Ends<'_>, size: u64, overwrite: bool, reason: Option<&str>) -> String {
    let windows = fs_local::display_path(&ends.local);
    let (from_path, to_path) = if ends.upload {
        (windows.as_str(), ends.to_path)
    } else {
        (ends.from_path, windows.as_str())
    };
    let mut text = String::new();
    if let Some(reason) = reason {
        text.push_str(&format!("{reason}：{to_path}\n"));
    }
    text.push_str(&format!(
        "传输文件：{} {from_path} → {} {to_path}\n大小：{size} 字节；覆盖已有目标：{overwrite}",
        ends.from.name, ends.to.name
    ));
    text
}

/// 在服务器上建 path 缺少的父目录（和 write_file 在 Windows 上一样）；路径上已有的要是目录或指向目录的链接
/// 只查不建：path 的父路径上已经有的各级都是目录（还没有的之后再建）
fn remote_parents_ok(session: &mut SftpSession, path: &str) -> Result<(), AppError> {
    let parent = match path.rfind('/') {
        Some(0) | None => return Ok(()),
        Some(index) => &path[..index],
    };
    let mut current = String::new();
    for part in parent.split('/').filter(|part| !part.is_empty()) {
        current.push('/');
        current.push_str(part);
        match session.stat(&current)? {
            Some(entry) if entry.kind == EntryKind::Dir || entry.target_is_dir == Some(true) => {}
            Some(_) => {
                return Err(AppError::new("mcp.path_invalid")
                    .with("path", &current)
                    .with_reason("mcp.parent_is_not_a_directory", "父路径不是目录"));
            }
            None => return Ok(()),
        }
    }
    Ok(())
}

fn remote_parents(session: &mut SftpSession, path: &str) -> Result<(), AppError> {
    let parent = match path.rfind('/') {
        Some(0) | None => return Ok(()),
        Some(index) => &path[..index],
    };
    let is_dir = |entry: &crate::sftp::RemoteEntry| {
        entry.kind == EntryKind::Dir || entry.target_is_dir == Some(true)
    };
    if session.stat(parent)?.is_some_and(|entry| is_dir(&entry)) {
        return Ok(());
    }
    let mut current = String::new();
    for part in parent.split('/').filter(|part| !part.is_empty()) {
        current.push('/');
        current.push_str(part);
        match session.stat(&current)? {
            Some(entry) if is_dir(&entry) => {}
            Some(_) => {
                return Err(AppError::new("mcp.path_invalid")
                    .with("path", &current)
                    .with_reason("mcp.parent_is_not_a_directory", "父路径不是目录"));
            }
            None => session.mkdir(&current)?,
        }
    }
    Ok(())
}

/// 运行 f 期间盯着调用取消、期限和服务器权限（每半秒核对一次）：任何一个出现就立起交给 f 的停止标志，
/// 并返回停下的原因
fn watch<T>(
    shared: &Shared,
    entry: &Entry,
    server: &Target,
    cancelled: &AtomicBool,
    timeout: Duration,
    f: impl FnOnce(&AtomicBool) -> T,
) -> (T, Option<Stop>) {
    watch_with(
        cancelled,
        timeout,
        || command_allowed(shared.host.as_ref(), entry, server),
        f,
    )
}

fn watch_with<T>(
    cancelled: &AtomicBool,
    timeout: Duration,
    allowed: impl Fn() -> bool + Sync,
    f: impl FnOnce(&AtomicBool) -> T,
) -> (T, Option<Stop>) {
    let stop = AtomicBool::new(false);
    let reason = Mutex::new(None);
    let done = AtomicBool::new(false);
    let deadline = Instant::now() + timeout;
    let result = std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            let mut next_check = Instant::now();
            while !done.load(Ordering::Acquire) {
                let now = Instant::now();
                let why = if cancelled.load(Ordering::Acquire) {
                    Some(Stop::Cancelled)
                } else if now >= deadline {
                    Some(Stop::Timeout)
                } else if now >= next_check {
                    next_check = now + PERMISSION_CHECK;
                    (!allowed()).then_some(Stop::Revoked)
                } else {
                    None
                };
                if let Some(why) = why {
                    *lock(&reason) = Some(why);
                    stop.store(true, Ordering::Release);
                    break;
                }
                // f 结束时会叫醒它
                std::thread::park_timeout(Duration::from_millis(50));
            }
        });
        let result = f(&stop);
        done.store(true, Ordering::Release);
        watcher.thread().unpark();
        result
    });
    let reason = *lock(&reason);
    (result, reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 取消超时和收回权限都立起停止标志并说明原因() {
        let wait = |stop: &AtomicBool| {
            let started = Instant::now();
            while !stop.load(Ordering::Acquire) {
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "停止标志没有立起"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let cancelled = AtomicBool::new(true);
        let ((), stop) = watch_with(&cancelled, Duration::from_secs(60), || true, wait);
        assert_eq!(stop, Some(Stop::Cancelled));
        let cancelled = AtomicBool::new(false);
        let ((), stop) = watch_with(&cancelled, Duration::ZERO, || true, wait);
        assert_eq!(stop, Some(Stop::Timeout));
        let ((), stop) = watch_with(&cancelled, Duration::from_secs(60), || false, wait);
        assert_eq!(stop, Some(Stop::Revoked));
        // 传完了就不算停下
        let (value, stop) = watch_with(&cancelled, Duration::from_secs(60), || true, |_| 7);
        assert_eq!((value, stop), (7, None));
    }

    #[cfg(unix)]
    #[test]
    fn 服务器上缺少的父目录逐级建出来_路径上是文件时报错() {
        let root = crate::test_support::tempdir();
        let mut session =
            SftpSession::spawn_server(Path::new("/usr/lib/openssh/sftp-server")).unwrap();
        let target = root.path().join("a/b/c/file.txt");
        remote_parents(&mut session, target.to_str().unwrap()).unwrap();
        assert!(root.path().join("a/b/c").is_dir());
        std::fs::write(root.path().join("plain"), "x").unwrap();
        let error = remote_parents(
            &mut session,
            root.path().join("plain/x/file").to_str().unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.params["reason_key"], "mcp.parent_is_not_a_directory");
        remote_parents(&mut session, "/file-in-root").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn 传到一半取消时两个方向都删掉没传完的文件() {
        let root = crate::test_support::tempdir();
        let mut session =
            SftpSession::spawn_server(Path::new("/usr/lib/openssh/sftp-server")).unwrap();
        let source = root.path().join("big.bin");
        std::fs::write(&source, vec![3u8; 2 * 1024 * 1024]).unwrap();
        for upload in [true, false] {
            let target = root.path().join(format!("out-{upload}.bin"));
            let cancel = AtomicBool::new(false);
            let mut progress = |_: u64| cancel.store(true, Ordering::Release);
            let error = if upload {
                transfer::upload_file(
                    &mut session,
                    &source,
                    target.to_str().unwrap(),
                    false,
                    &mut progress,
                    &cancel,
                )
            } else {
                transfer::download_file(
                    &mut session,
                    source.to_str().unwrap(),
                    &target,
                    false,
                    &mut progress,
                    &cancel,
                )
            }
            .unwrap_err();
            assert_eq!(error.code, "sftp.cancelled", "{upload}");
            let names: Vec<_> = std::fs::read_dir(root.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, ["big.bin"], "{upload}");
        }
    }
}
