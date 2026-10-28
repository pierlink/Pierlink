//! 预估的原始字节读写；只撤销内容仍匹配本次写入的改动。
use crate::{
    error::AppError,
    process::SshProgram,
    remote::run::{RunLimits, run_script},
    ssh::{args::SshCommand, quote::sh_quote},
};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const MAX_IGNORE: usize = 1024 * 1024;

#[cfg(all(test, unix))]
#[test]
#[ignore = "需要本机 sshd 和随机监听端口"]
fn 内置预估读取原始字节及排除规则写入回滚() {
    let f = crate::builtin_test_support::Fixture::new();
    let directory = f.sshd.root.path().join("预估 ' $() 目录\n");
    fs::create_dir(&directory).unwrap();
    let file = directory.join(".stignore");
    let original = b"# \xff\r\noriginal\n";
    fs::write(&file, original).unwrap();
    let access = EstimateFileAccess::Ssh {
        program: f.program(),
        command: f.command(),
    };
    assert_eq!(
        access.read(file.to_str().unwrap()).unwrap().as_deref(),
        Some(original.as_slice())
    );
    let mut guard = IgnoreFile::new(&access, directory.to_str().unwrap()).unwrap();
    guard.write(&["(?d).git".into(), "缓存".into()]).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "(?d).git\n缓存");
    guard.restore().unwrap();
    assert_eq!(fs::read(&file).unwrap(), original);
    let mut guard = IgnoreFile::new(&access, directory.to_str().unwrap()).unwrap();
    guard.write(&["临时规则".into()]).unwrap();
    fs::write(&file, "外部编辑").unwrap();
    assert_eq!(
        guard.restore().unwrap_err().code,
        "sync.estimate_file_changed"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "外部编辑");
}

/// 明确指定预估目录所在的文件系统；REST 客户端本身不提供原始文件访问。
pub enum EstimateFileAccess {
    /// 仅用于目录确实位于当前机器的连接，例如本机双实例测试。
    Local,
    /// 用与服务器连接相同的 SSH 程序和后台用途参数访问远端。
    Ssh {
        program: SshProgram,
        command: SshCommand,
    },
}
fn failed() -> AppError {
    AppError::new("sync.estimate_file_failed").with("reason", "无法安全读取或恢复排除规则文件")
}
fn changed() -> AppError {
    AppError::new("sync.estimate_file_changed").with("reason", "排除规则文件已被外部修改，保留现状")
}
fn symlink() -> AppError {
    AppError::new("sync.ignore_file_symlink").with("reason", ".stignore 是符号链接，不能安全替换")
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let temporary = path.with_file_name(format!(".pierlink-ignore-{:016x}", rand::random::<u64>()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|_| failed())?;
    let result = (|| {
        file.write_all(bytes).map_err(|_| failed())?;
        file.sync_all().map_err(|_| failed())?;
        drop(file);
        fs::rename(&temporary, path).map_err(|_| failed())
    })();
    let _ = fs::remove_file(temporary);
    result
}
impl EstimateFileAccess {
    /// 按访问端拼接目录与文件名，返回本机原生路径或远端 POSIX 路径。
    pub(super) fn join(&self, directory: &str, name: &str) -> String {
        match self {
            Self::Local => Path::new(directory)
                .join(name)
                .to_string_lossy()
                .into_owned(),
            Self::Ssh { .. } => format!("{}/{name}", directory.trim_end_matches('/')),
        }
    }

    pub(super) fn script(&self, body: &str) -> Result<serde_json::Value, AppError> {
        let Self::Ssh { program, command } = self else {
            return Err(failed());
        };
        run_script(
            program,
            command,
            body,
            RunLimits {
                timeout: Duration::from_secs(15),
                stdout_bytes: MAX_IGNORE * 4 + 1024,
                stderr_bytes: 1024,
            },
        )
        .map_err(|e| failed().with("cause", e.code))
    }
    pub(super) fn read(&self, path: &str) -> Result<Option<Vec<u8>>, AppError> {
        match self {
            Self::Local => {
                match fs::symlink_metadata(path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Ok(m) if m.is_symlink() => return Err(symlink()),
                    Ok(m) if m.is_file() && m.len() <= MAX_IGNORE as u64 => {}
                    _ => return Err(failed()),
                }
                let mut bytes = Vec::new();
                fs::File::open(path)
                    .map_err(|_| failed())?
                    .take((MAX_IGNORE + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|_| failed())?;
                if bytes.len() > MAX_IGNORE {
                    return Err(failed());
                }
                Ok(Some(bytes))
            }
            Self::Ssh { .. } => {
                let path = sh_quote(path)?;
                let value = self.script(&format!(
                    r#"
                    p={path}
                    if [ -L "$p" ]; then printf '"symlink"'; exit 0; fi
                    if [ ! -e "$p" ]; then printf 'null'; exit 0; fi
                    [ -f "$p" ] && [ "$(wc -c < "$p")" -le {MAX_IGNORE} ] || exit 1
                    printf '"'
                    od -An -v -tx1 "$p" | tr -d ' \n'
                    printf '"'
                "#
                ))?;
                if value.is_null() {
                    return Ok(None);
                }
                if value == "symlink" {
                    return Err(symlink());
                }
                let hex = value.as_str().ok_or_else(failed)?;
                if !hex.is_ascii() || hex.len() % 2 != 0 {
                    return Err(failed());
                }
                hex.as_bytes()
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| {
                        let text = std::str::from_utf8(pair).map_err(|_| failed())?;
                        u8::from_str_radix(text, 16).map_err(|_| failed())
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(Some)
            }
        }
    }
    pub(super) fn replace(
        &self,
        path: &str,
        expected: Option<&[u8]>,
        content: Option<&[u8]>,
    ) -> Result<(), AppError> {
        self.replace_checked(path, expected, content, &|| Ok(()))
    }
    fn replace_checked(
        &self,
        path: &str,
        expected: Option<&[u8]>,
        content: Option<&[u8]>,
        checkpoint: &dyn Fn() -> Result<(), AppError>,
    ) -> Result<(), AppError> {
        checkpoint()?;
        if self.read(path)?.as_deref() != expected {
            return Err(changed());
        }
        checkpoint()?;
        match self {
            Self::Local => {
                if let Some(bytes) = content {
                    atomic_write(Path::new(path), bytes)?;
                } else if expected.is_some() {
                    fs::remove_file(path).map_err(|_| failed())?;
                }
                Ok(())
            }
            Self::Ssh { .. } => {
                let path = sh_quote(path)?;
                let check = match expected {
                    None => "[ ! -e \"$p\" ] && [ ! -L \"$p\" ]".into(),
                    Some(bytes) => format!(
                        "[ ! -L \"$p\" ] && [ -f \"$p\" ] && [ \"$(od -An -v -tx1 \"$p\" | tr -d ' \\n')\" = {} ]",
                        sh_quote(&bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())?
                    ),
                };
                let write = match content {
                    Some(bytes) => {
                        let encoded = bytes
                            .iter()
                            .map(|b| format!("\\{b:03o}"))
                            .collect::<String>();
                        format!(
                            "t=\"$p.pierlink-{}\"\ntrap 'rm -f -- \"$t\"' EXIT HUP INT TERM\n(umask 077; set -C; printf {} > \"$t\") && mv -f -- \"$t\" \"$p\"",
                            rand::random::<u64>(),
                            sh_quote(&encoded)?
                        )
                    }
                    None if expected.is_some() => "rm -- \"$p\"".into(),
                    None => ":".into(),
                };
                let value = self.script(&format!("p={path}\nif {check}; then\n{write} || exit 1\nprintf 'true'\nelse printf 'false'; fi"))?;
                if value == true {
                    Ok(())
                } else {
                    Err(changed())
                }
            }
        }
    }
}

pub(super) struct IgnoreFile<'a> {
    access: &'a EstimateFileAccess,
    path: String,
    original: Option<Vec<u8>>,
    written: Option<Vec<u8>>,
    dirty: bool,
    recovery: Option<(PathBuf, String)>,
}
impl<'a> IgnoreFile<'a> {
    pub(super) fn new(access: &'a EstimateFileAccess, directory: &str) -> Result<Self, AppError> {
        let path = access.join(directory, ".stignore");
        let original = access.read(&path).map_err(|e| e.with("path", &path))?;
        Ok(Self {
            access,
            path,
            written: original.clone(),
            original,
            dirty: false,
            recovery: None,
        })
    }
    pub(super) fn with_recovery(mut self, data_dir: &Path, id: &str) -> Self {
        self.recovery = Some((data_dir.join("recovery"), id.into()));
        self
    }
    #[cfg(test)]
    pub(super) fn write(&mut self, rules: &[String]) -> Result<(), AppError> {
        self.write_checked(rules, &|| Ok(()))
    }
    pub(super) fn write_checked(
        &mut self,
        rules: &[String],
        checkpoint: &dyn Fn() -> Result<(), AppError>,
    ) -> Result<(), AppError> {
        checkpoint()?;
        let bytes = rules.join("\n").into_bytes();
        let content = if bytes.is_empty() && self.original.is_none() {
            None
        } else {
            Some(bytes)
        };
        if self.written == content {
            return Ok(());
        }
        if content.as_ref().is_some_and(|v| v.len() > MAX_IGNORE) {
            return Err(failed());
        }
        // 请求超时也可能已经写入；先记住拟写内容，让清理仍可核对并撤销。
        let previous = std::mem::replace(&mut self.written, content);
        self.dirty = true;
        self.access.replace_checked(
            &self.path,
            previous.as_deref(),
            self.written.as_deref(),
            checkpoint,
        )?;
        Ok(())
    }
    pub(super) fn restore_api_write(&mut self, rules: &[String]) -> Result<(), AppError> {
        let bytes = rules.join("\n").into_bytes();
        let mut terminated = bytes.clone();
        terminated.push(b'\n');
        self.written = Some(terminated);
        self.dirty = true;
        self.restore()
    }
    pub(super) fn restore(&mut self) -> Result<(), AppError> {
        match self.restore_once().or_else(|_| self.restore_once()) {
            Ok(()) => Ok(()),
            Err(e) => Err(self.save_recovery(e)?),
        }
    }
    fn save_recovery(&self, error: AppError) -> Result<AppError, AppError> {
        let (Some(bytes), Some((directory, id))) = (&self.original, &self.recovery) else {
            return Ok(error);
        };
        fs::create_dir_all(directory).map_err(|_| failed())?;
        // 恢复副本保留 30 天；仅清理由本模块生成的普通文件。
        for entry in fs::read_dir(directory).map_err(|_| failed())? {
            let entry = entry.map_err(|_| failed())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("pl-")
                && name.ends_with(".stignore")
                && entry.file_type().map_err(|_| failed())?.is_file()
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(30 * 86400))
            {
                fs::remove_file(entry.path()).map_err(|_| failed())?;
            }
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| failed())?
            .as_nanos();
        let path = directory.join(format!("{id}-{timestamp}.stignore"));
        atomic_write(&path, bytes)?;
        Ok(error.with("recovery_path", path.display()))
    }
    fn restore_once(&mut self) -> Result<(), AppError> {
        if !self.dirty {
            return Ok(());
        }
        let current = self.access.read(&self.path)?;
        if current != self.original {
            if current
                .as_deref()
                .zip(self.written.as_deref())
                .is_some_and(|(c, w)| w.starts_with(c))
            {
                self.written = current;
            }
            self.access.replace(
                &self.path,
                self.written.as_deref(),
                self.original.as_deref(),
            )?;
        }
        self.written = self.original.clone();
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn 文件访问按本机与远端分别拼接路径() {
        let root = crate::test_support::tempdir();
        let directory = root.path().to_str().unwrap();
        assert_eq!(
            super::EstimateFileAccess::Local.join(directory, ".stignore"),
            root.path().join(".stignore").to_str().unwrap()
        );
        let remote = super::EstimateFileAccess::Ssh {
            program: crate::process::SshProgram {
                path: "unused".into(),
            },
            command: crate::ssh::args::SshCommand {
                args: vec![],
                env: vec![],
                config_text: None,
                builtin: None,
                askpass: None,
            },
        };
        assert_eq!(remote.join("/work/", ".stignore"), "/work/.stignore");
        assert_eq!(remote.join("/", ".stfolder"), "/.stfolder");
    }

    use super::*;
    fn exercise(access: &EstimateFileAccess) {
        let root = crate::test_support::tempdir();
        let path = root.path().join(".stignore");
        let dir = root.path().to_str().unwrap();
        for original in [
            None,
            Some(Vec::new()),
            Some(b"// original\r\n\r\n\xff no newline".to_vec()),
        ] {
            if let Some(bytes) = &original {
                fs::write(&path, bytes).unwrap();
            }
            let before = crate::sync::tests::directory_snapshot(root.path());
            let mut guard = IgnoreFile::new(access, dir).unwrap();
            guard.write(&[]).unwrap();
            guard
                .write(&["node_modules".into(), "中文'$(false)".into()])
                .unwrap();
            guard.restore().unwrap();
            assert_eq!(crate::sync::tests::directory_snapshot(root.path()), before);
        }
        let mut guard = IgnoreFile::new(access, dir).unwrap();
        guard.write(&["ours".into()]).unwrap();
        fs::write(&path, "external").unwrap();
        assert_eq!(
            guard.restore().unwrap_err().code,
            "sync.estimate_file_changed"
        );
        assert_eq!(fs::read(&path).unwrap(), b"external");
    }
    #[test]
    fn 本机原始字节存在状态恢复及外部修改保留() {
        exercise(&EstimateFileAccess::Local);
    }
    #[cfg(unix)]
    #[test]
    fn 远程脚本原始字节存在状态恢复及外部修改保留() {
        // 用本机 POSIX sh 执行与 SSH 发送完全相同的脚本，独立验证转义和字节往返。
        exercise(&EstimateFileAccess::Ssh {
            program: SshProgram {
                path: "/bin/sh".into(),
            },
            command: SshCommand {
                args: vec!["-s".into()],
                env: vec![],
                config_text: None,
                builtin: None,
                askpass: None,
            },
        });
    }
    #[cfg(unix)]
    #[test]
    fn 拒绝符号链接且不改动目标() {
        let root = crate::test_support::tempdir();
        fs::write(root.path().join("target"), "untouched").unwrap();
        std::os::unix::fs::symlink("target", root.path().join(".stignore")).unwrap();
        assert!(
            IgnoreFile::new(&EstimateFileAccess::Local, root.path().to_str().unwrap()).is_err()
        );
        assert_eq!(fs::read(root.path().join("target")).unwrap(), b"untouched");
    }
}

#[cfg(all(test, unix))]
mod revision_tests {
    use super::*;
    fn shell(prefix: &str) -> EstimateFileAccess {
        EstimateFileAccess::Ssh {
            program: SshProgram {
                path: "/bin/sh".into(),
            },
            command: SshCommand {
                args: vec!["-c".into(), format!("{prefix}\nexec /bin/sh -s")],
                env: vec![],
                config_text: None,
                builtin: None,
                askpass: None,
            },
        }
    }
    #[test]
    fn 慢握手和大文件十六进制完整读取() {
        let root = crate::test_support::tempdir();
        let bytes: Vec<_> = (0..512 * 1024).map(|n| n as u8).collect();
        let path = root.path().join(".stignore");
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            shell("sleep 0.5").read(path.to_str().unwrap()).unwrap(),
            Some(bytes)
        );
    }
    #[test]
    fn 原子替换及前缀截断恢复本机和远端一致() {
        let root = crate::test_support::tempdir();
        let path = root.path().join(".stignore");
        for access in [EstimateFileAccess::Local, shell(":")] {
            fs::write(&path, b"original\xff").unwrap();
            let mut guard = IgnoreFile::new(&access, root.path().to_str().unwrap()).unwrap();
            guard.write(&["our-full-rule".into()]).unwrap();
            fs::write(&path, "our-").unwrap();
            guard.restore().unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"original\xff");
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
            fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink("target", &path).unwrap();
            assert_eq!(
                access.read(path.to_str().unwrap()).unwrap_err().code,
                "sync.ignore_file_symlink"
            );
            fs::remove_file(&path).unwrap();
        }
    }
    #[test]
    fn 恢复重试及永久失败落盘权限和保留期() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::test_support::tempdir();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let path = project.join(".stignore");
        fs::write(&path, "original").unwrap();
        let flag = root.path().join("fail-once");
        let access = shell(&format!(
            "if [ -f {} ]; then rm -- {}; exit 1; fi",
            sh_quote(flag.to_str().unwrap()).unwrap(),
            sh_quote(flag.to_str().unwrap()).unwrap()
        ));
        let mut guard = IgnoreFile::new(&access, project.to_str().unwrap())
            .unwrap()
            .with_recovery(root.path(), "pl-1234567890123456");
        guard.write(&["ours".into()]).unwrap();
        fs::write(&flag, "1").unwrap();
        guard.restore().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert!(!root.path().join("recovery").exists());
        guard.write(&["ours".into()]).unwrap();
        let unavailable = shell("exit 1");
        guard.access = &unavailable;
        let recovery = root.path().join("recovery");
        fs::create_dir(&recovery).unwrap();
        let old = recovery.join("pl-1234567890123456-0.stignore");
        fs::write(&old, "old").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&old)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .unwrap();
        let error = guard.restore().unwrap_err();
        let saved = Path::new(&error.params["recovery_path"]);
        assert_eq!(fs::read(saved).unwrap(), b"original");
        assert_eq!(
            fs::metadata(saved).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!old.exists());
        assert_eq!(fs::read(&path).unwrap(), b"ours");
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    #[test]
    fn 读取和写入脚本之间检查取消且不留下临时文件() {
        let root = crate::test_support::tempdir();
        let path = root.path().join(".stignore");
        fs::write(&path, "original").unwrap();
        let mut guard =
            IgnoreFile::new(&EstimateFileAccess::Local, root.path().to_str().unwrap()).unwrap();
        let checks = std::cell::Cell::new(0);
        let error = guard
            .write_checked(&["ours".into()], &|| {
                checks.set(checks.get() + 1);
                if checks.get() == 3 {
                    Err(AppError::new("sync.cancelled"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        assert_eq!(error.code, "sync.cancelled");
        guard.restore().unwrap();
        assert_eq!(fs::read(path).unwrap(), b"original");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
