//! 文件面板的快速删除：在服务器上用一条 rm -rf 删掉选中的条目（不跟随链接），
//! 不像 SFTP 那样每个文件都要和服务器来回一趟。
use crate::{
    error::AppError,
    process::SshProgram,
    remote::run::{RunLimits, run_script},
    ssh::{args::SshCommand, quote::sh_quote},
};
use std::time::Duration;

/// 最多等多久（特别大的目录服务器上也要删一阵）
const TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// 在服务器上用 rm -rf 删除 paths。
///
/// :param program: ssh 程序
/// :param cmd: 后台用途的命令（服务器上运行 /bin/sh -s）
/// :param paths: 要删的绝对路径；结尾的 / 去掉（rm -rf 'link/' 会删掉链接指向的目录里的东西），
///               不接受相对路径、根目录和含 . 或 .. 段的路径
/// :return: 删掉的项数（选中的项数）；有删不掉的返回 files.delete_failed，附还在的第一个路径和原因；
///          服务器上没有能用的 /bin/sh（Windows 服务器）返回 files.fast_delete_unavailable
pub fn fast_delete(
    program: &SshProgram,
    cmd: &SshCommand,
    paths: &[String],
) -> Result<usize, AppError> {
    if paths.is_empty() {
        return Ok(0);
    }
    let mut args = String::from("set --");
    for path in paths {
        let trimmed = path.trim_end_matches('/');
        if !path.starts_with('/')
            || trimmed.is_empty()
            || trimmed.split('/').any(|part| part == "." || part == "..")
        {
            return Err(AppError::new("files.invalid_name").with("path", path));
        }
        args.push(' ');
        args.push_str(&sh_quote(trimmed)?);
    }
    let body = format!("{args}\n{}", include_str!("delete.sh"));
    let limits = RunLimits {
        timeout: TIMEOUT,
        ..RunLimits::default()
    };
    let value = run_script(program, cmd, &body, limits).map_err(|e| {
        // 脚本只会正常退出：退出码不对说明没能在 POSIX shell 里跑起来
        if e.code == "remote.script_failed" {
            AppError::new("files.fast_delete_unavailable")
        } else {
            e
        }
    })?;
    let Some(left) = value.get("left").and_then(serde_json::Value::as_str) else {
        return Ok(paths.len());
    };
    let (key, text) = match value.get("reason").and_then(serde_json::Value::as_str) {
        Some("permission") => ("remote.delete_no_permission", "没有权限"),
        Some("readonly") => ("remote.delete_read_only", "文件系统是只读的"),
        Some("busy") => ("remote.delete_busy", "有文件正在使用"),
        _ => ("remote.delete_left", "rm 没能删掉"),
    };
    Err(AppError::new("files.delete_failed")
        .with("path", left)
        .with_reason(key, text))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn local() -> (SshProgram, SshCommand) {
        (
            SshProgram {
                path: "/bin/sh".into(),
            },
            SshCommand {
                args: vec!["-s".into()],
                env: vec![],
                config_text: None,
                hooked: false,
                builtin: None,
                askpass: None,
            },
        )
    }

    #[test]
    fn 一次删掉目录树和链接_不删链接目标() {
        let (program, cmd) = local();
        let root = crate::test_support::tempdir();
        let outside = root.path().join("外部");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("保留"), "ok").unwrap();
        let tree = root.path().join("树 '带引号'");
        fs::create_dir_all(tree.join("子目录")).unwrap();
        fs::write(tree.join("子目录/文件"), "ok").unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("链接")).unwrap();
        let file = root.path().join("单个文件");
        fs::write(&file, "x").unwrap();
        let paths = [tree.to_str().unwrap().into(), file.to_str().unwrap().into()];
        assert_eq!(fast_delete(&program, &cmd, &paths).unwrap(), 2);
        assert!(!tree.exists() && !file.exists());
        assert_eq!(fs::read(outside.join("保留")).unwrap(), b"ok");
        // 已经不在的照样算删掉
        assert_eq!(fast_delete(&program, &cmd, &paths).unwrap(), 2);
        assert_eq!(fast_delete(&program, &cmd, &[]).unwrap(), 0);
    }

    #[test]
    fn 不接受根目录和相对路径() {
        let (program, cmd) = local();
        for path in [
            "/",
            "//",
            "相对/路径",
            "",
            "/tmp/.",
            "/home/dev/..",
            "/home/../etc",
        ] {
            let e = fast_delete(&program, &cmd, &[path.into()]).unwrap_err();
            assert_eq!(e.code, "files.invalid_name", "{path}");
        }
    }

    #[test]
    fn 结尾的斜杠去掉_只删链接不删链接指向的内容() {
        let (program, cmd) = local();
        let root = crate::test_support::tempdir();
        let target = root.path().join("目标");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("保留"), "ok").unwrap();
        let link = root.path().join("链接");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let path = format!("{}/", link.to_str().unwrap());
        assert_eq!(fast_delete(&program, &cmd, &[path]).unwrap(), 1);
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(fs::read(target.join("保留")).unwrap(), b"ok");
    }

    #[test]
    fn 删不掉时报还在的路径和原因() {
        let (program, cmd) = local();
        let root = crate::test_support::tempdir();
        let tree = root.path().join("只读目录");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("文件"), "x").unwrap();
        fs::set_permissions(&tree, fs::Permissions::from_mode(0o555)).unwrap();
        // root 不受权限限制：这时测不出来
        let writable = fs::write(tree.join("试写"), "x").is_ok();
        if !writable {
            let path = tree.to_str().unwrap().to_owned();
            let e = fast_delete(&program, &cmd, std::slice::from_ref(&path)).unwrap_err();
            assert_eq!(e.code, "files.delete_failed");
            assert_eq!(e.params["path"], path);
            assert_eq!(e.params["reason_key"], "remote.delete_no_permission");
        }
        fs::set_permissions(&tree, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn 没有_posix_shell_时说明不能快速删除() {
        let program = SshProgram {
            path: "/bin/false".into(),
        };
        let (_, cmd) = local();
        let e = fast_delete(&program, &cmd, &["/tmp/不存在的路径".into()]).unwrap_err();
        assert_eq!(e.code, "files.fast_delete_unavailable");
    }
}
