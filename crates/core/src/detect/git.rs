//! Git Bash 候选路径推导；输入由平台层收集，不访问注册表或进程环境。
#![cfg(any(windows, test))]
use std::path::{Path, PathBuf};

pub(super) fn candidates(
    paths: &[PathBuf],
    installs: &[PathBuf],
    scoops: &[PathBuf],
) -> Vec<PathBuf> {
    let mut result: Vec<_> = installs
        .iter()
        .filter(|p| p.is_absolute())
        .map(|p| p.join("bin/bash.exe"))
        .collect();
    for dir in paths
        .iter()
        .filter(|p| p.is_absolute() && p.join("git.exe").is_file())
    {
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        if name.eq_ignore_ascii_case("cmd") {
            if let Some(root) = dir.parent() {
                result.push(root.join("bin/bash.exe"));
            }
        } else if name.eq_ignore_ascii_case("bin") {
            let parent = dir.parent();
            if parent
                .and_then(Path::file_name)
                .is_some_and(|n| n.eq_ignore_ascii_case("mingw64"))
            {
                if let Some(root) = parent.and_then(Path::parent) {
                    result.push(root.join("bin/bash.exe"));
                }
            } else {
                result.push(dir.join("bash.exe"));
            }
        } else if name.eq_ignore_ascii_case("shims")
            && let Some(root) = dir.parent()
        {
            result.push(root.join("apps/git/current/bin/bash.exe"));
        }
    }
    result.extend(
        scoops
            .iter()
            .filter(|p| p.is_absolute())
            .map(|p| p.join("apps/git/current/bin/bash.exe")),
    );
    result
}

#[cfg(any(windows, test))]
pub(super) fn git_bash(path: &Path, system_root: Option<&Path>) -> bool {
    if !path.is_absolute() || !path.is_file() {
        return false;
    }
    let normalized = crate::fs_local::plain_canonical(path).unwrap_or_else(|_| path.into());
    if let Some(root) = system_root {
        let root = crate::fs_local::plain_canonical(root).unwrap_or_else(|_| root.into());
        let normalized = normalized
            .to_string_lossy()
            .replace('\\', "/")
            .to_lowercase();
        let root = root.to_string_lossy().replace('\\', "/").to_lowercase();
        if normalized == root || normalized.starts_with(&format!("{}/", root.trim_end_matches('/')))
        {
            return false;
        }
    }
    path.parent().is_some_and(|bin| {
        bin.join("git.exe").is_file()
            || bin
                .parent()
                .is_some_and(|root| root.join("usr/bin").is_dir())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 注册表path和scoop候选均经过校验并去重() {
        let root = crate::test_support::tempdir();
        let git = root.path().join("Git 中文");
        for dir in ["cmd", "bin", "mingw64/bin", "usr/bin"] {
            std::fs::create_dir_all(git.join(dir)).unwrap();
            std::fs::write(git.join(dir).join("git.exe"), "").unwrap();
        }
        let bash = git.join("bin/bash.exe");
        std::fs::write(&bash, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bash, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let scoop = root.path().join("scoop");
        std::fs::create_dir_all(scoop.join("shims")).unwrap();
        std::fs::write(scoop.join("shims/git.exe"), "").unwrap();
        std::fs::create_dir_all(scoop.join("apps/git/current/bin")).unwrap();
        std::fs::write(scoop.join("apps/git/current/bin/bash.exe"), "").unwrap();
        std::fs::write(scoop.join("apps/git/current/bin/git.exe"), "").unwrap();
        let paths = vec![
            git.join("cmd"),
            git.join("bin"),
            git.join("mingw64/bin"),
            scoop.join("shims"),
            root.path().join("missing/cmd"),
            "relative".into(),
        ];
        let found = candidates(
            &paths,
            std::slice::from_ref(&git),
            std::slice::from_ref(&scoop),
        );
        assert_eq!(found.len(), 6);
        assert!(found.iter().all(|p| git_bash(p, None)));
        assert!(!git_bash(&bash, Some(root.path())));
        assert!(!git_bash(Path::new("Git/bin/bash.exe"), None));
        let mut shells = vec![];
        for candidate in &found[..4] {
            super::super::add(
                &mut shells,
                candidate.clone(),
                "Git Bash",
                super::super::ShellKind::GitBash,
                vec![],
            );
        }
        assert_eq!(shells.len(), 1);
    }
}
