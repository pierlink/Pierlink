//! 同步失败里的“没有权限”：认出这类原因，找出要修复权限的文件夹。
//! 修复本身（Windows 上用管理员权限给当前账户授权）由调用方执行；这里只算范围，保证不超出同步目录。
use std::path::{Component, Path, PathBuf};

/// 一次最多修复多少个文件夹
pub const PERMISSION_FIX_LIMIT: usize = 50;
/// 一次修复的文件夹路径加起来最多多少字节：路径要编进提权运行的命令行，命令行最长 32767 个字符，
/// 每个路径字节约占 3.6 个字符，再给脚本本身留出空间。超出的下次再点时修
pub const PERMISSION_FIX_BYTES: usize = 5000;

/// 失败原因是不是没有权限（Windows 的 Access is denied，Linux 的 permission denied、operation not permitted）
///
/// :param reason: 同步程序报的原因原文
/// :return: 是不是没有权限
pub fn is_permission_denied(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    [
        "access is denied",
        "permission denied",
        "operation not permitted",
    ]
    .iter()
    .any(|p| reason.contains(*p))
}

/// 没有权限的文件 → 要修复权限的文件夹。失败的路径本身是文件夹（不是链接）就用它，否则用它所在的文件夹；
/// 只留最上层的（修复上层时会连同里面的一起），最多 PERMISSION_FIX_LIMIT 个、路径共 PERMISSION_FIX_BYTES 字节。
/// 路径不合法（空、绝对路径、含 .. 或盘符）的跳过；从同步目录到它之间有目录链接（junction、符号链接）的也跳过，
/// 所以结果都在同步目录里面（含同步目录本身），修复时不会顺着链接改到外面。
///
/// :param root: 本机的同步目录
/// :param paths: 失败的文件（相对同步目录，同步程序报的写法）
/// :return: 要修复的文件夹（绝对路径，排好序）
pub fn permission_fix_targets(root: &Path, paths: &[String]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = paths
        .iter()
        .filter_map(|rel| {
            let rel = Path::new(rel);
            let normal = rel.components().next().is_some()
                && rel.components().all(|c| matches!(c, Component::Normal(_)));
            if !normal {
                return None;
            }
            let path = root.join(rel);
            // 不跟随链接：链接本身不算文件夹，取它所在的文件夹
            let is_dir = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
            let dir = if is_dir {
                path
            } else {
                path.parent().unwrap_or(root).to_path_buf()
            };
            (!through_link(root, &dir)).then_some(dir)
        })
        .collect();
    // 排序后上层在前，后面被上层包含的丢掉
    dirs.sort();
    dirs.dedup();
    let mut top: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if !top.iter().any(|t| dir.starts_with(t)) {
            top.push(dir);
        }
    }
    top.truncate(PERMISSION_FIX_LIMIT);
    let mut used = 0usize;
    top.retain(|dir| {
        used += dir.as_os_str().len();
        used <= PERMISSION_FIX_BYTES
    });
    top
}

/// 从同步目录到 dir（含 dir，不含同步目录本身）之间有没有目录链接（Windows 上 junction 也算）
fn through_link(root: &Path, dir: &Path) -> bool {
    let mut current = dir;
    while current != root && current.starts_with(root) {
        if std::fs::symlink_metadata(current).is_ok_and(|m| m.file_type().is_symlink()) {
            return true;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 认出各系统的没有权限() {
        assert!(is_permission_denied(
            "opening temp file: open C:\\a\\.syncthing.b.tmp: Access is denied."
        ));
        assert!(is_permission_denied("lstat /srv/a: permission denied"));
        assert!(is_permission_denied(
            "chmod /srv/a: operation not permitted"
        ));
        assert!(!is_permission_denied(
            "The process cannot access the file because it is being used by another process."
        ));
    }

    #[test]
    fn 修复范围取所在文件夹只留最上层且不出同步目录() {
        let temp = crate::test_support::tempdir();
        let root = temp.path();
        std::fs::create_dir_all(root.join("工具/pdf依赖/子目录")).unwrap();
        std::fs::write(root.join("说明.md"), "x").unwrap();
        let paths: Vec<String> = [
            "工具/pdf依赖/a.pdf",
            "工具/pdf依赖/子目录",
            "工具/pdf依赖/子目录/b.pdf",
            "工具/新的/c.pdf",
            "说明.md",
            "../外面/d.txt",
            "/etc/passwd",
            "",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let targets = permission_fix_targets(root, &paths);
        // 同步目录最外层的文件 → 同步目录本身，它包含了其他所有的
        assert_eq!(targets, vec![root.to_path_buf()]);

        let targets = permission_fix_targets(root, &paths[..4]);
        assert_eq!(
            targets,
            vec![root.join("工具/pdf依赖"), root.join("工具/新的")]
        );
        assert!(permission_fix_targets(root, &paths[5..]).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn 链接到外面的文件夹不当成文件夹() {
        let temp = crate::test_support::tempdir();
        let root = temp.path().join("同步");
        let outside = temp.path().join("外面");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("a/链接")).unwrap();
        let targets = permission_fix_targets(&root, &["a/链接".to_string()]);
        assert_eq!(targets, vec![root.join("a")]);
    }

    #[cfg(unix)]
    #[test]
    fn 上层有链接的跳过() {
        let temp = crate::test_support::tempdir();
        let root = temp.path().join("同步");
        let outside = temp.path().join("外面");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("a/链接")).unwrap();
        let paths = vec!["a/链接/sub/b.txt".to_string(), "a/c.txt".to_string()];
        assert_eq!(permission_fix_targets(&root, &paths), vec![root.join("a")]);
        assert!(permission_fix_targets(&root, &paths[..1]).is_empty());
    }

    #[test]
    fn 路径加起来太长时只修前面的() {
        // 不要求路径存在：用短的同步目录，结果不随临时目录的位置变
        let long = "长".repeat(300);
        let paths: Vec<String> = (0..10).map(|i| format!("{i}{long}/f.txt")).collect();
        let targets = permission_fix_targets(Path::new("/s"), &paths);
        assert!(!targets.is_empty() && targets.len() < 10);
        let bytes: usize = targets.iter().map(|t| t.as_os_str().len()).sum();
        assert!(bytes <= PERMISSION_FIX_BYTES);
    }

    #[test]
    fn 最多修复五十个文件夹() {
        let paths: Vec<String> = (0..80).map(|i| format!("d{i:02}/f.txt")).collect();
        assert_eq!(
            permission_fix_targets(Path::new("/s"), &paths).len(),
            PERMISSION_FIX_LIMIT
        );
    }
}
