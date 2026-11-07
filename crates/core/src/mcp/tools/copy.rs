//! 在授权文件夹里复制文件或目录：先统计和检查（审批前、没有副作用），确认后逐项复制；不跟随链接和目录联接。
use super::{
    check_cancel,
    files::{self, Sandbox},
};
use crate::{error::AppError, fs_local};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

/// 一次最多复制多少字节、多少项（文件、目录和跳过的链接都算）
const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
/// 结果里最多列出多少个跳过的项
const SKIPPED_LIMIT: usize = 100;
/// 确认框里最多列出多少个要确认的目标
const CONFIRM_LIMIT: usize = 5;

/// 要复制的一项
enum Item {
    /// 相对源目录的路径
    Dir(PathBuf),
    /// 相对源目录的路径
    File(PathBuf),
}

/// 复制前统计和检查的结果
pub(super) struct Plan {
    source: PathBuf,
    destination: PathBuf,
    overwrite: bool,
    /// 源是目录时，按先父后子的顺序列出里面的每一项；源是文件时为空
    items: Vec<Item>,
    /// 源是文件时它的大小
    file: Option<u64>,
    files: usize,
    dirs: usize,
    bytes: u64,
    /// 跳过的项：(源路径, 种类 link / special / invalid_name)
    skipped: Vec<(PathBuf, &'static str)>,
    /// 写进去要用户确认的目标（脚本、程序、启动文件夹、SSH/Git/shell 配置）
    confirm: Vec<PathBuf>,
}
impl Plan {
    /// 有要确认的目标：按危险操作审批
    pub(super) fn dangerous(&self) -> bool {
        !self.confirm.is_empty()
    }
}

fn invalid(path: &Path, key: &'static str, reason: &str) -> AppError {
    files::path_error("mcp.path_invalid", path, key, reason)
}

fn too_many(files: usize, bytes: u64) -> AppError {
    AppError::new("mcp.copy_too_large")
        .with("max_bytes", MAX_BYTES)
        .with("max_entries", MAX_ENTRIES)
        .with("bytes", bytes)
        .with("entries", files)
        .with_reason(
            "mcp.copy_exceeds_the_limits",
            "要复制的内容超过 2 GiB 或 10 万项，什么都没复制",
        )
}

fn type_differs(path: &Path) -> AppError {
    invalid(
        path,
        "mcp.copy_target_type_differs",
        "目标里已有同名的项，类型不同（目录、文件或链接），不能覆盖",
    )
}

/// 统计 source 并检查能不能复制到 destination（审批前调用，不改动任何东西）。
///
/// :param sb: 授权文件夹
/// :param source: 源文件或目录（Windows 绝对路径）
/// :param destination: 复制后的完整路径（不是放进哪个目录）
/// :param overwrite: 目标已存在时覆盖同名文件、合并到已有目录
/// :param cancelled: 取消标志
/// :return: 复制计划；源是链接、源和目标相同或互相包含、目标已存在又没给 overwrite、类型不同、超过上限时返回错误
pub(super) fn plan(
    sb: &Sandbox,
    source: &str,
    destination: &str,
    overwrite: bool,
    cancelled: &AtomicBool,
) -> Result<Plan, AppError> {
    let source = sb.resolve_entry(source)?;
    let destination = sb.resolve_entry(destination)?;
    let metadata = fs::symlink_metadata(&source).map_err(|e| files::io_error(e, &source))?;
    if files::is_link(&metadata) {
        return Err(invalid(
            &source,
            "mcp.copy_source_is_a_link",
            "源路径是链接或目录联接，不复制链接",
        ));
    }
    if !metadata.is_dir() && !metadata.is_file() {
        return Err(invalid(
            &source,
            "mcp.only_regular_files_are_supported",
            "只支持普通文件",
        ));
    }
    if files::path_within(&source, &destination) || files::path_within(&destination, &source) {
        return Err(invalid(
            &destination,
            "mcp.copy_onto_itself",
            "源和目标不能相同，也不能一个在另一个里面",
        ));
    }
    let mut plan = Plan {
        source,
        destination,
        overwrite,
        items: Vec::new(),
        file: None,
        files: 0,
        dirs: 0,
        bytes: 0,
        skipped: Vec::new(),
        confirm: Vec::new(),
    };
    check_target(&plan.destination, metadata.is_dir(), overwrite)?;
    if metadata.is_dir() {
        plan.dirs = 1;
        scan(&mut plan, cancelled)?;
    } else {
        plan.files = 1;
        plan.bytes = metadata.len();
        plan.file = Some(metadata.len());
        if plan.bytes > MAX_BYTES {
            return Err(too_many(1, plan.bytes));
        }
    }
    let mut targets = vec![plan.destination.clone()];
    targets.extend(plan.items.iter().map(|item| match item {
        Item::Dir(relative) | Item::File(relative) => plan.destination.join(relative),
    }));
    plan.confirm = targets
        .into_iter()
        .filter(|target| files::write_needs_confirmation(target).is_some())
        .collect();
    Ok(plan)
}

/// 目标 path 现在的样子能不能写：不存在可以；存在时要 overwrite，而且类型相同（目录对目录、普通文件对普通文件，链接不行）
fn check_target(path: &Path, dir: bool, overwrite: bool) -> Result<(), AppError> {
    if files::within_protected(path) {
        return Err(files::protected_error(path));
    }
    let Some(metadata) = files::metadata_if_exists(path)? else {
        return Ok(());
    };
    if !overwrite {
        return Err(files::path_error(
            "mcp.path_exists",
            path,
            "mcp.rename_target_exists",
            "目标已存在，请选择其他路径或设置 overwrite",
        ));
    }
    if files::is_link(&metadata)
        || metadata.is_dir() != dir
        || !(metadata.is_dir() || metadata.is_file())
    {
        return Err(type_differs(path));
    }
    Ok(())
}

/// 按先父后子的顺序列出源目录里的每一项；链接、特殊文件和不能用的名字跳过
fn scan(plan: &mut Plan, cancelled: &AtomicBool) -> Result<(), AppError> {
    let mut stack = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        let dir = plan.source.join(&relative);
        let mut entries: Vec<_> = fs::read_dir(&dir)
            .map_err(|e| files::io_error(e, &dir))?
            .collect::<Result<_, _>>()
            .map_err(|e| files::io_error(e, &dir))?;
        // 按名字排序，复制顺序和结果稳定
        entries.sort_by_key(|entry| entry.file_name());
        let mut children = Vec::new();
        for entry in entries {
            check_cancel(cancelled)?;
            let path = entry.path();
            let child = relative.join(entry.file_name());
            if plan.items.len() + plan.skipped.len() >= MAX_ENTRIES {
                return Err(too_many(
                    plan.items.len() + plan.skipped.len() + 1,
                    plan.bytes,
                ));
            }
            // 名字先做词法检查：不能借复制读到设备名、数据流
            if files::valid_path(&path).is_err() {
                plan.skipped.push((path, "invalid_name"));
                continue;
            }
            if files::within_protected(&path) {
                return Err(files::protected_error(&path));
            }
            let metadata = fs::symlink_metadata(&path).map_err(|e| files::io_error(e, &path))?;
            let target = plan.destination.join(&child);
            if files::is_link(&metadata) {
                plan.skipped.push((path, "link"));
            } else if metadata.is_dir() {
                check_target(&target, true, plan.overwrite)?;
                plan.dirs += 1;
                plan.items.push(Item::Dir(child.clone()));
                children.push(child);
            } else if metadata.is_file() {
                check_target(&target, false, plan.overwrite)?;
                plan.files += 1;
                plan.bytes = plan.bytes.saturating_add(metadata.len());
                if plan.bytes > MAX_BYTES {
                    return Err(too_many(plan.items.len() + 1, plan.bytes));
                }
                plan.items.push(Item::File(child));
            } else {
                plan.skipped.push((path, "special"));
            }
        }
        // 倒序压栈，按名字顺序先处理第一个子目录
        stack.extend(children.into_iter().rev());
    }
    Ok(())
}

/// 确认框里的说明：源、目标、数量和要确认的目标（最多列 5 个）
pub(super) fn action(plan: &Plan) -> String {
    let mut text = format!(
        "复制：{} → {}\n文件 {} 个、目录 {} 个，共 {} 字节；覆盖已有目标：{}",
        plan.source.display(),
        plan.destination.display(),
        plan.files,
        plan.dirs,
        plan.bytes,
        plan.overwrite
    );
    if !plan.confirm.is_empty() {
        text.push_str(&format!(
            "\n会写入要确认的文件（共 {} 项）：",
            plan.confirm.len()
        ));
        for path in plan.confirm.iter().take(CONFIRM_LIMIT) {
            text.push_str(&format!("\n{}", path.display()));
        }
        if plan.confirm.len() > CONFIRM_LIMIT {
            text.push_str("\n…");
        }
    }
    text
}

/// 按计划复制（用户已同意）。目录逐项建立，文件先写到同目录的临时文件再改名，取消或失败时临时文件自动删掉，
/// 已经复制好的保留。
///
/// :param sb: 授权文件夹
/// :param plan: plan 的结果
/// :param cancelled: 取消标志
/// :return: {source, destination, files, dirs, bytes, skipped, skipped_count}
pub(super) fn copy(sb: &Sandbox, plan: &Plan, cancelled: &AtomicBool) -> Result<Value, AppError> {
    if let Some(parent) = plan.destination.parent() {
        files::make_dir(
            sb,
            parent
                .to_str()
                .ok_or_else(|| invalid(parent, "mcp.path_is_not_valid_text", "路径不是有效文字"))?,
        )?;
    }
    let mut copied = 0u64;
    match plan.file {
        Some(size) => {
            copy_file(&plan.source, &plan.destination, plan.overwrite, cancelled)?;
            copied = size;
        }
        None => {
            make_target_dir(&plan.destination)?;
            for item in &plan.items {
                check_cancel(cancelled)?;
                match item {
                    Item::Dir(relative) => make_target_dir(&plan.destination.join(relative))?,
                    Item::File(relative) => {
                        let target = plan.destination.join(relative);
                        let written = copy_file(
                            &plan.source.join(relative),
                            &target,
                            plan.overwrite,
                            cancelled,
                        )?;
                        copied = copied.saturating_add(written);
                        if copied > MAX_BYTES {
                            return Err(too_many(plan.items.len(), copied));
                        }
                    }
                }
            }
        }
    }
    let skipped: Vec<Value> = plan
        .skipped
        .iter()
        .take(SKIPPED_LIMIT)
        .map(|(path, kind)| json!({"path": fs_local::display_path(path), "kind": kind}))
        .collect();
    Ok(json!({
        "source": plan.source,
        "destination": plan.destination,
        "files": plan.files,
        "dirs": plan.dirs,
        "bytes": copied,
        "skipped": skipped,
        "skipped_count": plan.skipped.len(),
    }))
}

/// 建目标目录；已经有了（合并）时要是真的目录，不能是链接
fn make_target_dir(path: &Path) -> Result<(), AppError> {
    if files::within_protected(path) {
        return Err(files::protected_error(path));
    }
    match files::metadata_if_exists(path)? {
        Some(metadata) if metadata.is_dir() && !files::is_link(&metadata) => Ok(()),
        Some(_) => Err(type_differs(path)),
        None => fs::create_dir(path).map_err(|e| files::io_error(e, path)),
    }
}

/// 复制一个普通文件：先写到 target 同目录的临时文件，再改名成 target（overwrite 为假时不覆盖之后才出现的同名文件）。
/// 保留权限和修改时间；返回字节数
fn copy_file(
    source: &Path,
    target: &Path,
    overwrite: bool,
    cancelled: &AtomicBool,
) -> Result<u64, AppError> {
    // 统计之后源变成了链接：不跟过去
    let metadata = fs::symlink_metadata(source).map_err(|e| files::io_error(e, source))?;
    if files::is_link(&metadata) || !metadata.is_file() {
        return Err(invalid(
            source,
            "mcp.copy_source_is_a_link",
            "源路径是链接或目录联接，不复制链接",
        ));
    }
    if files::within_protected(target) {
        return Err(files::protected_error(target));
    }
    let parent = target
        .parent()
        .ok_or_else(|| invalid(target, "mcp.parent_directory_is_missing", "缺少父目录"))?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(|e| files::io_error(e, parent))?;
    if files::is_link(&parent_metadata) || !parent_metadata.is_dir() {
        return Err(type_differs(parent));
    }
    let mut input = files::open_file(source)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".pierlink-")
        .suffix(".pierlink-part")
        .tempfile_in(parent)
        .map_err(|e| files::io_error(e, target))?;
    let mut buffer = vec![0; 64 * 1024];
    let mut written = 0u64;
    loop {
        let count = files::read_chunk(&mut input, &mut buffer, source, cancelled)?;
        if count == 0 {
            break;
        }
        temporary
            .write_all(&buffer[..count])
            .map_err(|e| files::io_error(e, target))?;
        written += count as u64;
        if written > MAX_BYTES {
            return Err(too_many(1, written));
        }
    }
    let file = temporary.as_file();
    if let Ok(modified) = metadata.modified() {
        file.set_times(fs::FileTimes::new().set_modified(modified))
            .map_err(|e| files::io_error(e, target))?;
    }
    file.sync_all().map_err(|e| files::io_error(e, target))?;
    check_cancel(cancelled)?;
    let persisted = if overwrite {
        temporary.persist(target).map(|_| ())
    } else {
        temporary.persist_noclobber(target).map(|_| ())
    };
    persisted.map_err(|e| {
        if e.error.kind() == std::io::ErrorKind::AlreadyExists {
            files::path_error(
                "mcp.path_exists",
                target,
                "mcp.rename_target_exists",
                "目标已存在，请选择其他路径或设置 overwrite",
            )
        } else {
            files::io_error(e.error, target)
        }
    })?;
    // 权限（只读、可执行）改名之后再设：只读的临时文件在 Windows 上删不掉，失败时会留下来
    fs::set_permissions(target, metadata.permissions()).map_err(|e| files::io_error(e, target))?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(root: &Path) -> Sandbox {
        Sandbox::new(&[root.to_path_buf()])
    }
    fn text(path: &Path) -> &str {
        path.to_str().unwrap()
    }
    fn run(sb: &Sandbox, from: &Path, to: &Path, overwrite: bool) -> Result<Value, AppError> {
        let cancelled = AtomicBool::new(false);
        let plan = plan(sb, text(from), text(to), overwrite, &cancelled)?;
        copy(sb, &plan, &cancelled)
    }

    #[test]
    fn 复制文件和目录树_自动建父目录_保留内容和修改时间() {
        let root = crate::test_support::tempdir();
        let sb = sandbox(root.path());
        let source = root.path().join("src");
        fs::create_dir_all(source.join("a/b")).unwrap();
        fs::create_dir_all(source.join("empty")).unwrap();
        fs::write(source.join("top.txt"), "顶层").unwrap();
        fs::write(source.join("a/b/deep.bin"), [0u8, 255, 1]).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::File::options()
            .write(true)
            .open(source.join("top.txt"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let target = root.path().join("新/层/dst");
        let result = run(&sb, &source, &target, false).unwrap();
        assert_eq!(result["files"], 2);
        assert_eq!(result["dirs"], 4);
        assert_eq!(result["bytes"], "顶层".len() as u64 + 3);
        assert_eq!(fs::read_to_string(target.join("top.txt")).unwrap(), "顶层");
        assert_eq!(fs::read(target.join("a/b/deep.bin")).unwrap(), [0, 255, 1]);
        assert!(target.join("empty").is_dir());
        assert_eq!(
            fs::metadata(target.join("top.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            old
        );
        // 单个文件：destination 是复制后的完整路径
        let file = root.path().join("单个/复制.txt");
        let result = run(&sb, &source.join("top.txt"), &file, false).unwrap();
        assert_eq!(result["files"], 1);
        assert_eq!(fs::read_to_string(&file).unwrap(), "顶层");
        // 不留临时文件
        assert!(fs::read_dir(root.path().join("单个")).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".pierlink-part")
        }));
    }

    #[test]
    fn 目标已存在要_overwrite_类型不同或自己套自己报错() {
        let root = crate::test_support::tempdir();
        let sb = sandbox(root.path());
        let source = root.path().join("src");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::write(source.join("sub/f.txt"), "新").unwrap();
        let target = root.path().join("dst");
        fs::create_dir_all(target.join("sub")).unwrap();
        fs::write(target.join("sub/f.txt"), "旧").unwrap();
        fs::write(target.join("keep.txt"), "留着").unwrap();
        assert_eq!(
            run(&sb, &source, &target, false).unwrap_err().code,
            "mcp.path_exists"
        );
        assert_eq!(fs::read_to_string(target.join("sub/f.txt")).unwrap(), "旧");
        // overwrite：同名文件覆盖，目录合并，目标里多出来的不动
        run(&sb, &source, &target, true).unwrap();
        assert_eq!(fs::read_to_string(target.join("sub/f.txt")).unwrap(), "新");
        assert_eq!(fs::read_to_string(target.join("keep.txt")).unwrap(), "留着");
        // 目标里同名的是目录、源里是文件：不覆盖
        fs::remove_file(target.join("sub/f.txt")).unwrap();
        fs::create_dir(target.join("sub/f.txt")).unwrap();
        let error = run(&sb, &source, &target, true).unwrap_err();
        assert_eq!(error.params["reason_key"], "mcp.copy_target_type_differs");
        for (from, to) in [
            (source.clone(), source.clone()),
            (source.clone(), source.join("sub/inside")),
            (source.join("sub"), source.clone()),
        ] {
            let error = run(&sb, &from, &to, true).unwrap_err();
            assert_eq!(error.params["reason_key"], "mcp.copy_onto_itself");
        }
        assert_eq!(
            run(&sb, &root.path().join("missing"), &target, true)
                .unwrap_err()
                .code,
            "mcp.not_found"
        );
    }

    #[test]
    fn 授权外和受保护的目录不读不写() {
        let _protected = files::PROTECTED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = crate::test_support::tempdir();
        let allowed = root.path().join("allowed");
        let other = root.path().join("other");
        let data = allowed.join("pierlink-data");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(data.join("settings.db"), "x").unwrap();
        fs::write(allowed.join("a.txt"), "x").unwrap();
        fs::write(root.path().join("outside.txt"), "x").unwrap();
        files::set_protected_dirs(std::slice::from_ref(&data));
        let sb = Sandbox::new(&[allowed.clone(), other.clone()]);
        assert_eq!(
            run(
                &sb,
                &root.path().join("outside.txt"),
                &allowed.join("b.txt"),
                false
            )
            .unwrap_err()
            .code,
            "mcp.path_outside"
        );
        assert_eq!(
            run(
                &sb,
                &allowed.join("a.txt"),
                &root.path().join("b.txt"),
                false
            )
            .unwrap_err()
            .code,
            "mcp.path_outside"
        );
        // 源目录里有 Pierlink 自己的目录：不复制出去，什么都不写
        let copy = other.join("copy");
        assert_eq!(
            run(&sb, &allowed, &copy, false).unwrap_err().code,
            "mcp.path_protected"
        );
        assert!(!copy.exists());
        let backup = other.join("backup");
        fs::create_dir_all(backup.join("pierlink-data")).unwrap();
        fs::write(backup.join("pierlink-data/settings.db"), "坏").unwrap();
        // 往 Pierlink 自己的目录里合并：不写
        let error = run(&sb, &backup, &allowed, true).unwrap_err();
        assert_eq!(error.code, "mcp.path_protected");
        assert_eq!(fs::read_to_string(data.join("settings.db")).unwrap(), "x");
        let error = run(&sb, &data, &allowed.join("偷出来"), false).unwrap_err();
        assert_eq!(error.code, "mcp.path_protected");
    }

    #[cfg(unix)]
    #[test]
    fn 链接跳过并列出_源是链接时报错_要确认的目标列在说明里() {
        let root = crate::test_support::tempdir();
        let sb = sandbox(root.path());
        let source = root.path().join("src");
        fs::create_dir_all(source.join(".git/hooks")).unwrap();
        fs::write(source.join(".git/hooks/pre-commit"), "#!/bin/sh").unwrap();
        fs::write(source.join("run.ps1"), "x").unwrap();
        fs::write(source.join("data.txt"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", source.join("passwd")).unwrap();
        std::os::unix::fs::symlink(root.path(), source.join("loop")).unwrap();
        let cancelled = AtomicBool::new(false);
        let target = root.path().join("dst");
        let planned = plan(&sb, text(&source), text(&target), false, &cancelled).unwrap();
        assert!(planned.dangerous());
        let action = action(&planned);
        assert!(action.starts_with(&format!(
            "复制：{} → {}",
            source.display(),
            target.display()
        )));
        assert!(action.contains("文件 3 个、目录 3 个，共 "));
        assert!(action.contains("会写入要确认的文件（共 3 项）："));
        assert!(action.contains("pre-commit") && action.contains("run.ps1"));
        let result = copy(&sb, &planned, &cancelled).unwrap();
        assert_eq!(result["skipped_count"], 2);
        let kinds: Vec<_> = result["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["link", "link"]);
        assert!(!target.join("passwd").exists() && !target.join("loop").exists());
        assert!(target.join("data.txt").is_file());
        // 源本身是链接
        let error = run(&sb, &source.join("passwd"), &root.path().join("p"), false).unwrap_err();
        assert_eq!(error.params["reason_key"], "mcp.copy_source_is_a_link");
        // 只是普通数据文件：不用按危险操作确认
        let plain = plan(
            &sb,
            text(&source.join("data.txt")),
            text(&root.path().join("data2.txt")),
            false,
            &cancelled,
        )
        .unwrap();
        assert!(!plain.dangerous());
    }

    #[test]
    fn 取消时不留临时文件() {
        let root = crate::test_support::tempdir();
        let sb = sandbox(root.path());
        let source = root.path().join("big.bin");
        fs::write(&source, vec![7u8; 300 * 1024]).unwrap();
        let cancelled = AtomicBool::new(false);
        let target = root.path().join("out/big.bin");
        let planned = plan(&sb, text(&source), text(&target), false, &cancelled).unwrap();
        cancelled.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(
            copy(&sb, &planned, &cancelled).unwrap_err().code,
            "mcp.cancelled"
        );
        assert!(!target.exists());
        assert_eq!(fs::read_dir(root.path().join("out")).unwrap().count(), 0);
    }
}
