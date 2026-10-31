//! 删除配置前记录标记名；只恢复本次删除产生的改名，不碰历史标记。
use super::estimate_files::EstimateFileAccess;
use crate::{error::AppError, ssh::quote::sh_quote};
use std::{collections::BTreeSet, fs, path::Path};

#[cfg(all(test, unix))]
#[test]
#[ignore = "需要本机 sshd 和随机监听端口"]
fn 内置同步标记只恢复本次改名并保留外部改动() {
    let f = crate::builtin_test_support::Fixture::new();
    let directory = f.sshd.root.path().join("标记 ' $() 目录\n");
    fs::create_dir(&directory).unwrap();
    let marker = directory.join(".stfolder");
    fs::create_dir(&marker).unwrap();
    fs::write(marker.join("keep"), "原始标记").unwrap();
    let historical = directory.join(".stfolder.removed-old");
    fs::create_dir(&historical).unwrap();
    let access = EstimateFileAccess::Ssh {
        program: f.program(),
        command: f.command(),
    };
    let snapshot = Marker::new(&access, directory.to_str().unwrap()).unwrap();
    let renamed = directory.join(".stfolder.removed-current");
    fs::rename(&marker, &renamed).unwrap();
    snapshot.restore().unwrap();
    assert_eq!(fs::read_to_string(marker.join("keep")).unwrap(), "原始标记");
    assert!(historical.is_dir() && !renamed.exists());
    let snapshot = Marker::new(&access, directory.to_str().unwrap()).unwrap();
    fs::rename(&marker, &renamed).unwrap();
    fs::create_dir(&marker).unwrap();
    assert_eq!(snapshot.restore().unwrap_err().code, "sync.cleanup_failed");
    assert!(marker.is_dir() && renamed.is_dir() && historical.is_dir());
}

fn failed(path: &str) -> AppError {
    AppError::new("sync.cleanup_failed")
        .with("path", path)
        .with_reason(
            "sync.couldn_t_restore_the_folder_marker_destination_exists_or_marker",
            "无法安全恢复文件夹标记，目标已存在或标记状态发生变化",
        )
}
// Linux 原子拒绝覆盖；Windows 的目录 rename 本身拒绝已存在的目标。
fn rename_marker(source: &str, target: &str) -> Result<(), AppError> {
    #[cfg(target_os = "linux")]
    {
        let source_c = std::ffi::CString::new(source).map_err(|_| failed(target))?;
        let destination = std::ffi::CString::new(target).map_err(|_| failed(target))?;
        if unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source_c.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } != 0
        {
            let code = std::io::Error::last_os_error().raw_os_error();
            return rename_fallback(source, target, code);
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    fs::rename(source, target).map_err(|_| failed(target))
}
#[cfg(target_os = "linux")]
fn rename_fallback(source: &str, target: &str, code: Option<i32>) -> Result<(), AppError> {
    if matches!(code, Some(libc::EINVAL | libc::ENOSYS))
        && matches!(fs::symlink_metadata(target), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    {
        return fs::rename(source, target).map_err(|_| failed(target));
    }
    Err(failed(target))
}
impl EstimateFileAccess {
    fn markers(&self, directory: &str) -> Result<BTreeSet<String>, AppError> {
        match self {
            Self::Local => {
                let entries = match fs::read_dir(directory) {
                    Ok(entries) => entries,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(BTreeSet::new());
                    }
                    Err(_) => return Err(failed(directory)),
                };
                entries
                    .map(|entry| {
                        entry
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .map_err(|_| failed(directory))
                    })
                    .filter(|name| {
                        name.as_ref().map_or(true, |n| {
                            n == ".stfolder" || n.starts_with(".stfolder.removed-")
                        })
                    })
                    .collect()
            }
            Self::Ssh { .. } => {
                let value = self.script(&format!(
                    r#"
                    d={}
                    printf '['; sep=''
                    for p in "$d/.stfolder" "$d"/.stfolder.removed-*; do
                        [ -e "$p" ] || [ -L "$p" ] || continue
                        n=${{p##*/}}
                        case "$n" in *[!a-zA-Z0-9._-]*) continue;; esac
                        printf '%s"%s"' "$sep" "$n"; sep=','
                    done
                    printf ']'
                "#,
                    sh_quote(directory)?
                ))?;
                serde_json::from_value(value).map_err(|_| failed(directory))
            }
        }
    }
}
pub(super) struct Marker<'a> {
    access: &'a EstimateFileAccess,
    directory: String,
    before: BTreeSet<String>,
    empty: bool,
}
impl<'a> Marker<'a> {
    pub(super) fn new(access: &'a EstimateFileAccess, directory: &str) -> Result<Self, AppError> {
        Self::new_checked(access, directory, &|| Ok(()))
    }
    pub(super) fn new_checked(
        access: &'a EstimateFileAccess,
        directory: &str,
        checkpoint: &dyn Fn() -> Result<(), AppError>,
    ) -> Result<Self, AppError> {
        checkpoint()?;
        let before = access.markers(directory)?;
        checkpoint()?;
        Ok(Self {
            access,
            directory: directory.into(),
            before,
            empty: match access {
                EstimateFileAccess::Local => {
                    let p = Path::new(directory).join(".stfolder");
                    if p.symlink_metadata().is_ok_and(|m| m.is_dir()) { fs::read_dir(p).map_err(|_| failed(directory))?.next().is_none() } else { false }
                }
                EstimateFileAccess::Ssh { .. } => access.script(&format!(
                    "p={}\nif [ ! -L \"$p\" ] && [ -d \"$p\" ] && [ -z \"$(ls -A \"$p\")\" ]; then printf true; else printf false; fi",
                    sh_quote(&format!("{directory}/.stfolder"))?
                ))? == true,
            },
        })
    }
    pub(super) fn restore(&self) -> Result<(), AppError> {
        let after = self.access.markers(&self.directory)?;
        let mut added: Vec<_> = after
            .difference(&self.before)
            .filter(|n| n.starts_with(".stfolder.removed-"))
            .collect();
        let target = self.access.join(&self.directory, ".stfolder");
        if self.before.contains(".stfolder") {
            if added.is_empty() && after.contains(".stfolder") {
                return Ok(());
            }
            if added.is_empty() && !after.contains(".stfolder") && self.empty {
                match self.access {
                    EstimateFileAccess::Local => {
                        fs::create_dir(&target).map_err(|_| failed(&target))?
                    }
                    EstimateFileAccess::Ssh { .. } => {
                        self.access.script(&format!(
                            "mkdir -- {} || exit 1\nprintf true",
                            sh_quote(&target)?
                        ))?;
                    }
                }
                return Ok(());
            }
            if added.len() != 1 || after.contains(".stfolder") {
                return Err(failed(&target));
            }
        }
        if !self.before.contains(".stfolder") && after.contains(".stfolder") {
            added.push(after.get(".stfolder").ok_or_else(|| failed(&target))?);
        }
        for name in added {
            let source = self.access.join(&self.directory, name);
            let restore = self.before.contains(".stfolder");
            match self.access {
                EstimateFileAccess::Local => {
                    if restore {
                        if Path::new(&target).symlink_metadata().is_ok() {
                            return Err(failed(&target));
                        }
                        rename_marker(&source, &target)?;
                    } else {
                        fs::remove_dir(&source).map_err(|_| failed(&source))?;
                    }
                }
                EstimateFileAccess::Ssh { .. } => {
                    let body = if restore {
                        format!(
                            "s={}\nt={}\n[ ! -e \"$t\" ] && [ ! -L \"$t\" ] && mv -n -- \"$s\" \"$t\" && [ ! -e \"$s\" ] && [ ! -L \"$s\" ] || exit 1",
                            sh_quote(&source)?,
                            sh_quote(&target)?
                        )
                    } else {
                        format!("rmdir -- {} || exit 1", sh_quote(&source)?)
                    };
                    self.access
                        .script(&format!("{body}\nprintf 'true'"))
                        .map_err(|_| failed(&target))?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn exercise(access: &EstimateFileAccess) {
        for existed in [false, true] {
            let root = crate::test_support::tempdir();
            let dir = root.path();
            let marker = dir.join(".stfolder");
            fs::create_dir(dir.join(".stfolder.removed-old")).unwrap();
            if existed {
                fs::create_dir(&marker).unwrap();
                fs::write(marker.join("user-marker"), b"user").unwrap();
            }
            let before = crate::sync::tests::directory_snapshot(dir);
            let guard = Marker::new(access, dir.to_str().unwrap()).unwrap();
            if !existed {
                fs::create_dir(&marker).unwrap();
            }
            fs::rename(&marker, dir.join(".stfolder.removed-20261022-203904")).unwrap();
            guard.restore().unwrap();
            assert_eq!(crate::sync::tests::directory_snapshot(dir), before);
            guard.restore().unwrap();
            if existed {
                fs::rename(&marker, dir.join(".stfolder.removed-new")).unwrap();
                fs::create_dir(&marker).unwrap();
                let e = guard.restore().unwrap_err();
                assert_eq!(e.code, "sync.cleanup_failed");
                assert_eq!(e.params["path"], marker.to_str().unwrap());
                assert_eq!(
                    fs::read(dir.join(".stfolder.removed-new/user-marker")).unwrap(),
                    b"user"
                );
            } else {
                fs::create_dir(dir.join(".stfolder.removed-new")).unwrap();
                fs::write(dir.join(".stfolder.removed-new/user"), "external").unwrap();
                assert!(guard.restore().is_err());
            }
        }
    }
    #[test]
    fn 本机恢复标记快照且拒绝覆盖或删除用户内容() {
        exercise(&EstimateFileAccess::Local);
    }
    #[cfg(unix)]
    #[test]
    fn 远程恢复标记快照且拒绝覆盖或删除用户内容() {
        exercise(&EstimateFileAccess::Ssh {
            program: crate::process::SshProgram {
                path: "/bin/sh".into(),
            },
            command: crate::ssh::args::SshCommand {
                args: vec!["-s".into()],
                env: vec![],
                config_text: None,
                hooked: false,
                builtin: None,
                askpass: None,
            },
        });
    }
}

#[cfg(test)]
mod revision_tests {
    use super::*;
    fn check(access: &EstimateFileAccess) {
        let root = crate::test_support::tempdir();
        let path = root.path().join(".stfolder");
        fs::create_dir(&path).unwrap();
        let marker = Marker::new(access, root.path().to_str().unwrap()).unwrap();
        fs::remove_dir(&path).unwrap();
        marker.restore().unwrap();
        assert!(path.is_dir());
        fs::write(path.join("user"), "内容").unwrap();
        let marker = Marker::new(access, root.path().to_str().unwrap()).unwrap();
        fs::remove_file(path.join("user")).unwrap();
        fs::remove_dir(&path).unwrap();
        assert_eq!(marker.restore().unwrap_err().code, "sync.cleanup_failed");
    }
    #[test]
    fn 空标记直接删除后恢复而非空消失报错() {
        check(&EstimateFileAccess::Local);
    }
    #[cfg(unix)]
    #[test]
    fn 远端空标记恢复及特殊名字跳过() {
        let access = EstimateFileAccess::Ssh {
            program: crate::process::SshProgram {
                path: "/bin/sh".into(),
            },
            command: crate::ssh::args::SshCommand {
                args: vec!["-s".into()],
                env: vec![],
                config_text: None,
                hooked: false,
                builtin: None,
                askpass: None,
            },
        };
        check(&access);
        let root = crate::test_support::tempdir();
        fs::create_dir(root.path().join(".stfolder.removed-中文'\n")).unwrap();
        assert!(
            access
                .markers(root.path().to_str().unwrap())
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod fallback_tests {
    use super::*;
    #[test]
    fn 不支持原子无覆盖改名时仅允许目标不存在的回退() {
        let root = crate::test_support::tempdir();
        let a = root.path().join("source");
        let b = root.path().join("target");
        fs::create_dir(&a).unwrap();
        rename_fallback(a.to_str().unwrap(), b.to_str().unwrap(), Some(libc::EINVAL)).unwrap();
        fs::create_dir(&a).unwrap();
        assert!(
            rename_fallback(a.to_str().unwrap(), b.to_str().unwrap(), Some(libc::EINVAL)).is_err()
        );
        assert!(a.is_dir());
        assert!(b.is_dir());
        assert!(
            rename_fallback(
                a.to_str().unwrap(),
                root.path().join("other").to_str().unwrap(),
                Some(libc::EACCES)
            )
            .is_err()
        );
    }
}
