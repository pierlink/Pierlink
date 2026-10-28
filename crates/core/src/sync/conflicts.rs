//! 冲突版本总是先存档；存档命名空间为规范本机路径的稳定摘要，不依赖数据库。
#[path = "conflict_diff.rs"]
mod diff;
use super::{
    error,
    time::{format_ms, now_ms, parse_ms},
};
use crate::error::AppError;
pub use diff::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub path: String,
    pub conflict_file: String,
    pub from_device: Option<String>,
    pub conflict_mtime_ms: i64,
    pub current_mtime_ms: i64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    Current,
    Other,
}
fn io(e: std::io::Error) -> AppError {
    AppError::new("sync.archive_io").with("reason", format!("文件操作失败（{:?}）", e.kind()))
}
pub(super) fn relative(path: &str) -> Result<(), AppError> {
    if path.is_empty()
        || path.contains(['\\', '\0'])
        || path.split('/').next().is_some_and(|part| {
            part.len() == 2
                && part.as_bytes()[0].is_ascii_alphabetic()
                && part.as_bytes()[1] == b':'
        })
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(error("必须使用安全的相对文件路径"));
    }
    Ok(())
}
fn safe(root: &Path, relative_path: &str) -> Result<PathBuf, AppError> {
    relative(relative_path)?;
    let mut p = root.to_path_buf();
    for c in Path::new(relative_path).components() {
        p.push(c);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => return Err(error("冲突路径不得经过符号链接")),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
    }
    Ok(p)
}
fn mtime(path: &Path) -> Result<i64, AppError> {
    let m = fs::symlink_metadata(path).map_err(io)?;
    if !m.is_file() || m.file_type().is_symlink() {
        return Err(error("冲突对象必须是普通文件"));
    }
    let t = m.modified().map_err(io)?;
    Ok(match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    })
}
fn parse_name(path: &str) -> Option<(String, String)> {
    let (head, tail) = path.rsplit_once(".sync-conflict-")?;
    if tail.len() < 23 || !tail.get(..23).is_some_and(str::is_ascii) {
        return None;
    }
    let date = &tail[..8];
    let clock = &tail[9..15];
    let device = &tail[16..23];
    if &tail[8..9] != "-"
        || &tail[15..16] != "-"
        || !device
            .bytes()
            .all(|c| c.is_ascii_uppercase() || (b'2'..=b'7').contains(&c))
    {
        return None;
    }
    parse_ms(&format!(
        "{}-{}-{}T{}:{}:{}Z",
        &date[..4],
        &date[4..6],
        &date[6..8],
        &clock[..2],
        &clock[2..4],
        &clock[4..6]
    ))
    .ok()?;
    let suffix = &tail[23..];
    if !suffix.is_empty() && !suffix.starts_with('.') {
        return None;
    }
    Some((format!("{head}{suffix}"), device.into()))
}
// 按 Syncthing 的首条匹配优先规则，支持通配符、否定、大小写及可删除前缀。
pub(super) struct CompiledRules(Vec<(usize, bool, regex::Regex)>);
impl CompiledRules {
    pub(super) fn ignored(&self, path: &str) -> Option<usize> {
        self.0
            .iter()
            .find(|(_, _, re)| re.is_match(path))
            .and_then(|(i, negate, _)| (!negate).then_some(*i))
    }
}
pub(super) fn compile_rules(rules: &[String]) -> (CompiledRules, Vec<usize>) {
    let mut compiled = Vec::new();
    let mut invalid = Vec::new();
    for (i, line) in rules.iter().enumerate() {
        let mut rule = line.trim();
        if rule.is_empty() || rule.starts_with("//") {
            continue;
        }
        if rule.starts_with("#include") {
            continue;
        }
        let mut insensitive = false;
        let mut negate = false;
        loop {
            if let Some(v) = rule.strip_prefix("(?d)") {
                rule = v;
            } else if let Some(v) = rule.strip_prefix("(?i)") {
                insensitive = true;
                rule = v;
            } else if let Some(v) = rule.strip_prefix('!') {
                negate = true;
                rule = v;
            } else {
                break;
            }
        }
        let anchored = rule.starts_with('/');
        rule = rule.trim_start_matches('/');
        let mut pattern = if insensitive {
            "(?i)".to_string()
        } else {
            String::new()
        };
        pattern.push_str(if anchored { "^" } else { "(?:^|/)" });
        let mut chars = rule.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '*' if chars.peek() == Some(&'*') => {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        pattern.push_str("(?:.*/)?");
                    } else {
                        pattern.push_str(".*");
                    }
                }
                '*' => pattern.push_str("[^/]*"),
                '?' => pattern.push_str("[^/]"),
                '[' => {
                    pattern.push('[');
                    for c in chars.by_ref() {
                        pattern.push(c);
                        if c == ']' {
                            break;
                        }
                    }
                }
                c => pattern.push_str(&regex::escape(&c.to_string())),
            }
        }
        pattern.push_str("(?:/|$)");
        match regex::Regex::new(&pattern) {
            Ok(re) => compiled.push((i, negate, re)),
            Err(_) => invalid.push(i),
        }
    }
    (CompiledRules(compiled), invalid)
}
#[cfg(test)]
fn ignored(path: &str, rules: &[String]) -> Result<Option<usize>, AppError> {
    Ok(compile_rules(rules).0.ignored(path))
}

fn expand_rules(
    root: &Path,
    rules: &[String],
    depth: usize,
    output: &mut Vec<String>,
) -> Result<(), AppError> {
    if depth > 16 {
        return Err(error("排除规则包含文件嵌套过深或形成循环"));
    }
    for rule in rules {
        if let Some(include) = rule.trim().strip_prefix("#include ") {
            let path = safe(root, include.trim())?;
            let file = fs::File::open(&path).map_err(io)?;
            let mut content = String::new();
            use std::io::Read;
            file.take(1024 * 1024 + 1)
                .read_to_string(&mut content)
                .map_err(io)?;
            if content.len() > 1024 * 1024 {
                return Err(error("排除规则包含文件超过 1 MiB"));
            }
            let nested = content.lines().map(String::from).collect::<Vec<_>>();
            expand_rules(root, &nested, depth + 1, output)?;
        } else {
            output.push(rule.clone());
            if output.len() > 10000 {
                return Err(error("展开后的排除规则超过 10000 条"));
            }
        }
    }
    crate::syncthing::folder::validate_ignores(output)
}
/// 冲突列表及因访问或路径异常跳过的条目数；invalid_rules 为非法规则的零基序号。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ConflictList {
    pub conflicts: Vec<Conflict>,
    pub skipped: usize,
    pub invalid_rules: Vec<usize>,
}
/// 扫描 local_path 下未被 ignore_rules 排除的冲突，最多返回 limit 条；不跟随符号链接。
pub fn list_conflicts(
    local_path: &Path,
    ignore_rules: &[String],
    limit: usize,
) -> Result<ConflictList, AppError> {
    let root = crate::fs_local::plain_canonical(local_path).map_err(io)?;
    let mut expanded = Vec::new();
    expand_rules(&root, ignore_rules, 0, &mut expanded)?;
    let (rules, invalid_rules) = compile_rules(&expanded);
    let mut output = ConflictList {
        invalid_rules,
        ..Default::default()
    };
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        if output.conflicts.len() >= limit {
            break;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                output.skipped += 1;
                continue;
            }
        };
        for entry in entries {
            let candidate = (|| -> Result<Option<Conflict>, AppError> {
                let entry = entry.map_err(io)?;
                let ty = entry.file_type().map_err(io)?;
                if ty.is_symlink() {
                    return Err(error("跳过符号链接"));
                }
                let p = entry.path();
                let name = p
                    .strip_prefix(&root)
                    .map_err(|_| error("路径超出同步目录"))?
                    .to_str()
                    .ok_or_else(|| error("文件名不是 UTF-8"))?
                    .replace('\\', "/");
                if name
                    .split('/')
                    .any(|s| matches!(s, ".stfolder" | ".stversions"))
                {
                    return Ok(None);
                }
                if ty.is_dir() {
                    stack.push(p);
                    return Ok(None);
                }
                if let Some((original, device)) = parse_name(&name) {
                    if rules.ignored(&name).is_some() {
                        return Ok(None);
                    }
                    let current = safe(&root, &original)?;
                    return Ok(Some(Conflict {
                        path: original,
                        conflict_file: name,
                        from_device: Some(device),
                        conflict_mtime_ms: mtime(&p)?,
                        current_mtime_ms: mtime(&current)?,
                    }));
                }
                Ok(None)
            })();
            match candidate {
                Ok(Some(c)) => output.conflicts.push(c),
                Ok(None) => {}
                Err(_) => output.skipped += 1,
            }
            if output.conflicts.len() >= limit {
                break;
            }
        }
    }
    output.conflicts.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(output)
}

fn archive_path(
    root: &Path,
    archive: &Path,
    folder: &str,
    name: &str,
) -> Result<PathBuf, AppError> {
    let archive = crate::syncthing::folder::normalized(archive)?;
    if archive.starts_with(root) || root.starts_with(&archive) {
        return Err(error("冲突存档必须位于同步目录之外"));
    }
    let date = format_ms(now_ms())[..10].to_string();
    let path = safe(&archive, &format!("{folder}/{date}/{name}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }
    Ok(path)
}
fn archive_file(source: &Path, destination: &Path, remove: bool) -> Result<PathBuf, AppError> {
    for n in 0..10000 {
        let dest = if n == 0 {
            destination.to_path_buf()
        } else {
            destination.with_file_name(format!(
                "{}.{n}",
                destination
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            ))
        };
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = match options.open(&dest) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io(e)),
        };
        let result = (|| {
            let mut input = fs::File::open(source)?;
            let before = input.metadata()?;
            std::io::copy(&mut input, &mut output)?;
            let after = fs::symlink_metadata(source)?;
            if after.file_type().is_symlink()
                || before.len() != after.len()
                || before.modified()? != after.modified()?
            {
                return Err(std::io::Error::other("存档期间文件已改变"));
            }
            output.set_permissions(before.permissions())?;
            output.set_times(fs::FileTimes::new().set_modified(before.modified()?))?;
            output.sync_all()?;
            if remove {
                fs::remove_file(source)
            } else {
                Ok(())
            }
        })();
        if let Err(e) = result {
            drop(output);
            let _ = fs::remove_file(&dest);
            return Err(io(e));
        }
        return Ok(dest);
    }
    Err(AppError::new("sync.archive_full"))
}
// rename 始终保留原名的完整文件；Windows 标准库使用替换语义。
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let a = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        let b = std::ffi::CString::new(target.as_os_str().as_bytes())?;
        if unsafe { libc::renameat2(libc::AT_FDCWD, a.as_ptr(), libc::AT_FDCWD, b.as_ptr(), 0) }
            == 0
        {
            return Ok(());
        }
    }
    fs::rename(source, target)
}
/// 核对 conflict 两方路径及修改时间，按 keep 将另一方移入 archive_root；返回存档/替换结果。
/// 目录键由规范本机路径生成稳定 pl- 摘要；调用方可将此键关联到任务，存档应每日以 30 天清理。
pub fn resolve_conflict(
    local_path: &Path,
    archive_root: &Path,
    conflict: &Conflict,
    keep: Keep,
) -> Result<(), AppError> {
    let root = fs::canonicalize(local_path).map_err(io)?;
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let folder = format!("pl-{:x}", digest)[..19].to_string();
    resolve_conflict_for_folder(&root, archive_root, &folder, conflict, keep)
}
/// 按显式 folder_id 将 conflict 的被舍弃版本存入 archive_root/文件夹ID/日期/相对路径；返回操作结果。
/// local_path、conflict、keep 与 resolve_conflict 相同；有数据库任务 ID 的调用方应使用此入口。
pub fn resolve_conflict_for_folder(
    local_path: &Path,
    archive_root: &Path,
    folder_id: &str,
    conflict: &Conflict,
    keep: Keep,
) -> Result<(), AppError> {
    crate::syncthing::folder::validate_task_folder_id(folder_id)?;
    let root = crate::fs_local::plain_canonical(local_path).map_err(io)?;
    if parse_name(&conflict.conflict_file).as_ref().map(|v| &v.0) != Some(&conflict.path) {
        return Err(error("冲突文件与原路径不匹配"));
    }
    let current = safe(&root, &conflict.path)?;
    let other = safe(&root, &conflict.conflict_file)?;
    if mtime(&current).ok() != Some(conflict.current_mtime_ms)
        || mtime(&other).ok() != Some(conflict.conflict_mtime_ms)
    {
        return Err(AppError::new("sync.conflict_changed"));
    }
    let destination = archive_path(&root, archive_root, folder_id, &conflict.path)?;
    let archived = archive_file(
        if keep == Keep::Current {
            &other
        } else {
            &current
        },
        &destination,
        keep == Keep::Current,
    )?;
    if keep == Keep::Other
        && let Err(e) = replace_file(&other, &current)
    {
        return Err(AppError::new("sync.conflict_incomplete")
            .with("archive", archived.display())
            .with(
                "reason",
                format!("替换失败（{:?}），原版本已存档", e.kind()),
            ));
    }
    Ok(())
}
/// 按 ignore_rules 排除后自动处理 local_path 最多 limit 条冲突，返回已处理记录；同秒版本和单条失败留给用户。
pub fn auto_resolve_newest(
    local_path: &Path,
    archive_root: &Path,
    ignore_rules: &[String],
    limit: usize,
) -> Result<Vec<Conflict>, AppError> {
    auto_resolve(local_path, archive_root, None, ignore_rules, limit)
}
/// 自动处理 local_path 最多 limit 条冲突，并以真实 folder_id 归档到 archive_root；返回已处理记录。
/// 按 ignore_rules 排除；同秒版本和单条失败不处理，存档与旧版本开关无关。
pub fn auto_resolve_newest_for_folder(
    local_path: &Path,
    archive_root: &Path,
    folder_id: &str,
    ignore_rules: &[String],
    limit: usize,
) -> Result<Vec<Conflict>, AppError> {
    crate::syncthing::folder::validate_task_folder_id(folder_id)?;
    auto_resolve(
        local_path,
        archive_root,
        Some(folder_id),
        ignore_rules,
        limit,
    )
}
fn auto_resolve(
    local_path: &Path,
    archive_root: &Path,
    folder_id: Option<&str>,
    ignore_rules: &[String],
    limit: usize,
) -> Result<Vec<Conflict>, AppError> {
    let mut done = Vec::new();
    for c in list_conflicts(local_path, ignore_rules, limit)?.conflicts {
        let a = c.current_mtime_ms.div_euclid(1000);
        let b = c.conflict_mtime_ms.div_euclid(1000);
        if a == b {
            continue;
        }
        let keep = if a > b { Keep::Current } else { Keep::Other };
        let result = if let Some(id) = folder_id {
            resolve_conflict_for_folder(local_path, archive_root, id, &c, keep)
        } else {
            resolve_conflict(local_path, archive_root, &c, keep)
        };
        if result.is_ok() {
            done.push(c);
        }
    }
    Ok(done)
}
/// 按日期目录清理 archive_root 超过 max_age_days 天的存档（通常传 30）；返回删除的日期目录数。
/// 只处理本模块生成的 pl- 命名空间和合法日期，不跟随链接。
pub fn cleanup_archive(archive_root: &Path, max_age_days: u32) -> Result<usize, AppError> {
    if !archive_root.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for folder in fs::read_dir(archive_root).map_err(io)? {
        let folder = folder.map_err(io)?;
        if !folder.file_type().map_err(io)?.is_dir()
            || crate::syncthing::folder::validate_task_folder_id(
                &folder.file_name().to_string_lossy(),
            )
            .is_err()
        {
            continue;
        }
        for date in fs::read_dir(folder.path()).map_err(io)? {
            let date = date.map_err(io)?;
            if !date.file_type().map_err(io)?.is_dir() {
                continue;
            }
            let name = date.file_name();
            let name = name.to_string_lossy();
            if name.len() != 10 {
                continue;
            }
            if let Ok(ms) = parse_ms(&format!("{name}T00:00:00Z"))
                && now_ms().div_euclid(86400000) - ms.div_euclid(86400000) > i64::from(max_age_days)
            {
                fs::remove_dir_all(date.path()).map_err(io)?;
                count += 1;
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair(root: &Path, name: &str, current: i64, other: i64) {
        fs::create_dir_all(root).unwrap();
        for (p, content, t) in [
            (root.join(name), "当前", current),
            (
                root.join(format!("{name}.sync-conflict-20261022-123456-ABCDEFG")),
                "冲突",
                other,
            ),
        ] {
            fs::write(&p, content).unwrap();
            fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_modified(UNIX_EPOCH + std::time::Duration::from_millis(t as u64)),
                )
                .unwrap();
        }
    }
    #[test]
    fn y26_解析各种名称() {
        for (name, want) in [
            (
                "文件.sync-conflict-20261022-123456-ABCDEFG.中文",
                "文件.中文",
            ),
            ("a.tar.sync-conflict-20261022-123456-ABCDEFG.gz", "a.tar.gz"),
            ("README.sync-conflict-20261022-123456-ABCDEFG", "README"),
        ] {
            assert_eq!(parse_name(name).unwrap(), (want.into(), "ABCDEFG".into()));
        }
        assert!(parse_name("a.sync-conflict-20260230-123456-ABCDEFG").is_none());
        assert!(parse_name("a.sync-conflict-中文日期").is_none());
    }
    #[test]
    fn y26_选择两方存档重名及修改检测() {
        let temp = crate::test_support::tempdir();
        let root = temp.path().join("work");
        let archive = temp.path().join("archive");
        for keep in [Keep::Current, Keep::Other] {
            pair(&root, "a", 1000, 2000);
            let c = list_conflicts(&root, &[], 20).unwrap().conflicts.remove(0);
            let mut stale = c.clone();
            stale.current_mtime_ms += 1;
            assert_eq!(
                resolve_conflict(&root, &archive, &stale, keep)
                    .unwrap_err()
                    .code,
                "sync.conflict_changed"
            );
            resolve_conflict(&root, &archive, &c, keep).unwrap();
            assert_eq!(
                fs::read_to_string(root.join("a")).unwrap(),
                if keep == Keep::Current {
                    "当前"
                } else {
                    "冲突"
                }
            );
            assert!(list_conflicts(&root, &[], 20).unwrap().conflicts.is_empty());
        }
        let folder = fs::read_dir(&archive)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let date = fs::read_dir(folder)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::read_to_string(date.join("a")).unwrap(), "冲突");
        assert_eq!(fs::read_to_string(date.join("a.1")).unwrap(), "当前");
    }
    #[test]
    fn y26_自动模式同秒不处理及排除() {
        let temp = crate::test_support::tempdir();
        let root = temp.path().join("work");
        pair(&root, "same", 1000, 1999);
        pair(&root, "older", 3000, 1000);
        pair(&root, "newer", 1000, 3000);
        assert_eq!(
            auto_resolve_newest(&root, &temp.path().join("archive"), &[], 20)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(list_conflicts(&root, &[], 20).unwrap().conflicts.len(), 1);
        assert!(
            list_conflicts(&root, &["same*".into()], 20)
                .unwrap()
                .conflicts
                .is_empty()
        );
        assert_eq!(
            ignored("node_modules/x", &["(?d)node_modules".into()]).unwrap(),
            Some(0)
        );
        assert_eq!(
            ignored("x.env", &["!x.env".into(), "*.env".into()]).unwrap(),
            None
        );
    }
    #[test]
    fn d18_按日期保留三十天() {
        let temp = crate::test_support::tempdir();
        let folder = temp.path().join("pl-1234567890123456");
        for age in [29, 30, 31] {
            let date = format_ms(now_ms() - age * 86400000)[..10].to_string();
            fs::create_dir_all(folder.join(date)).unwrap();
        }
        fs::create_dir_all(folder.join("无效日期")).unwrap();
        assert_eq!(cleanup_archive(temp.path(), 30).unwrap(), 1);
        assert_eq!(cleanup_archive(temp.path(), 30).unwrap(), 0);
    }
    #[cfg(unix)]
    #[test]
    fn 冲突路径拒绝链接越界() {
        let temp = crate::test_support::tempdir();
        let root = temp.path().join("work");
        pair(&root, "a", 1000, 2000);
        let mut c = list_conflicts(&root, &[], 10).unwrap().conflicts.remove(0);
        c.path = "../a".into();
        assert!(resolve_conflict(&root, &temp.path().join("archive"), &c, Keep::Other).is_err());
        std::os::unix::fs::symlink(temp.path(), root.join("link")).unwrap();
        assert!(safe(&root, "link/a").is_err());
    }
}

#[cfg(test)]
mod include_tests {
    use super::*;
    #[test]
    fn 包含规则有界展开并拒绝越界与循环() {
        let temp = crate::test_support::tempdir();
        fs::write(temp.path().join("rules"), "(?i)**/*.KEY\n!keep\n").unwrap();
        let mut expanded = Vec::new();
        expand_rules(temp.path(), &["#include rules".into()], 0, &mut expanded).unwrap();
        assert_eq!(ignored("secret.key", &expanded).unwrap(), Some(0));
        assert_eq!(ignored("dir/secret.key", &expanded).unwrap(), Some(0));
        fs::write(temp.path().join("rules"), "#include rules").unwrap();
        assert!(expand_rules(temp.path(), &["#include rules".into()], 0, &mut Vec::new()).is_err());
        assert!(
            expand_rules(
                temp.path(),
                &["#include ../outside".into()],
                0,
                &mut Vec::new()
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod folder_tests {
    use super::*;
    #[test]
    fn 显式文件夹编号的手动与自动入口() {
        let temp = crate::test_support::tempdir();
        let root = temp.path().join("work");
        fs::create_dir(&root).unwrap();
        let archive = temp.path().join("archive");
        let id = "pl-1234567890123456";
        for auto in [false, true] {
            fs::write(root.join("a"), "当前").unwrap();
            let other = root.join("a.sync-conflict-20261022-123456-ABCDEFG");
            fs::write(&other, "另一方").unwrap();
            fs::OpenOptions::new()
                .write(true)
                .open(&other)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
                .unwrap();
            if auto {
                assert_eq!(
                    auto_resolve_newest_for_folder(&root, &archive, id, &[], 10)
                        .unwrap()
                        .len(),
                    1
                );
            } else {
                let c = list_conflicts(&root, &[], 10).unwrap().conflicts.remove(0);
                resolve_conflict_for_folder(&root, &archive, id, &c, Keep::Current).unwrap();
            }
        }
        let dir = archive.join(id).join(&format_ms(now_ms())[..10]);
        assert_eq!(fs::read_to_string(dir.join("a")).unwrap(), "另一方");
        assert_eq!(fs::read_to_string(dir.join("a.1")).unwrap(), "另一方");
        assert!(auto_resolve_newest_for_folder(&root, &archive, "../bad", &[], 10).is_err());
    }
}

#[cfg(test)]
mod revision_tests {
    use super::*;
    fn pair(root: &Path, name: &str) {
        fs::write(root.join(name), "当前内容").unwrap();
        let other = root.join(format!("{name}.sync-conflict-20261022-123456-ABCDEFG"));
        fs::write(&other, "冲突内容").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(other)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
            .unwrap();
    }
    // 文件名含冒号只会出现在 Linux 服务器同步过来的文件里；Windows 上 “a:b” 会被当成文件 a 的附加数据流，无法构造这个场景
    #[cfg(unix)]
    #[test]
    fn 冒号路径和非法规则跳过且自动处理沿用排除() {
        let root = crate::test_support::tempdir();
        let work = root.path().join("work");
        fs::create_dir(&work).unwrap();
        pair(&work, "report:2026");
        pair(&work, "excluded");
        assert!(relative("report:2026").is_ok());
        #[cfg(not(windows))]
        assert!(relative("a:b").is_ok());
        for path in ["C:/Windows/x", "../x", "/x", "a\\b"] {
            assert!(relative(path).is_err());
        }
        let rules = vec!["[".into(), "excluded*".into()];
        let list = list_conflicts(&work, &rules, 10).unwrap();
        assert_eq!(list.invalid_rules, vec![0]);
        assert_eq!(list.conflicts.len(), 1);
        assert_eq!(
            auto_resolve_newest(&work, &root.path().join("archive"), &rules, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_conflicts(&work, &[], 10).unwrap().conflicts[0].path,
            "excluded"
        );
    }
    #[test]
    fn 归档或替换失败原名始终保留完整内容() {
        let root = crate::test_support::tempdir();
        let work = root.path().join("work");
        fs::create_dir(&work).unwrap();
        pair(&work, "a");
        let conflict = list_conflicts(&work, &[], 10).unwrap().conflicts.remove(0);
        let archive = root.path().join("blocked");
        fs::write(&archive, "不能创建子目录").unwrap();
        assert!(resolve_conflict(&work, &archive, &conflict, Keep::Other).is_err());
        assert_eq!(fs::read(work.join("a")).unwrap(), "当前内容".as_bytes());
        let copy = root.path().join("copy");
        archive_file(&work.join("a"), &copy, false).unwrap();
        assert!(replace_file(&work.join("missing"), &work.join("a")).is_err());
        assert_eq!(fs::read(work.join("a")).unwrap(), fs::read(copy).unwrap());
    }
    #[test]
    fn 自动处理单条归档失败仍继续其余条目() {
        let root = crate::test_support::tempdir();
        let work = root.path().join("work");
        fs::create_dir_all(work.join("bad")).unwrap();
        pair(&work, "bad/a");
        pair(&work, "good");
        let archive = root.path().join("archive");
        let folder = "pl-1234567890123456";
        let date = archive.join(folder).join(&format_ms(now_ms())[..10]);
        fs::create_dir_all(&date).unwrap();
        fs::write(date.join("bad"), "不能作为目录").unwrap();
        let done = auto_resolve_newest_for_folder(&work, &archive, folder, &[], 10).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].path, "good");
        assert!(work.join("bad/a").is_file());
    }
    #[cfg(unix)]
    #[test]
    fn 无权限非文本文件名目录及链接条目跳过() {
        use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};
        let root = crate::test_support::tempdir();
        pair(root.path(), "valid");
        for name in ["directory", "link"] {
            pair(root.path(), name);
            fs::remove_file(root.path().join(name)).unwrap();
        }
        fs::create_dir(root.path().join("directory")).unwrap();
        std::os::unix::fs::symlink("valid", root.path().join("link")).unwrap();
        fs::write(
            root.path().join(std::ffi::OsString::from_vec(vec![255])),
            "非文本",
        )
        .unwrap();
        let blocked = root.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o0)).unwrap();
        let result = list_conflicts(root.path(), &[], 20);
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        let result = result.unwrap();
        assert_eq!(result.conflicts.len(), 1);
        assert!(result.skipped >= 4);
    }
}
