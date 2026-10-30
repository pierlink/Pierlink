//! 经受限后台脚本操作默认 authorized_keys；最终以真实认证结果为准。
use super::public::parse;
use crate::{
    error::AppError,
    process::SshProgram,
    remote::run::{RunLimits, run_script},
    ssh::{args::SshCommand, quote::sh_quote},
};

#[derive(Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    AlreadyPresent,
}
#[derive(Debug)]
pub struct InstallReport {
    pub outcome: InstallOutcome,
    /// 已有目录或授权文件权限过宽；没有修改其权限。
    pub permissions_warning: bool,
    /// 尽力读取 sshd_config 发现非默认路径；Include、Match 和实际运行参数仍以 verify 为准。
    pub non_default_authorized_keys: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed { count: u32 },
    NotFound,
    Symlink,
}
#[derive(Debug)]
pub struct AuthorizedKey {
    pub line_no: u32,
    pub key_type: String,
    pub fingerprint: String,
    pub comment: String,
    /// “类型 公钥数据”（不含注释和选项），移除这一行时原样传给 remove
    pub public_line: String,
    pub ours: bool,
    pub pierlink: bool,
}
fn failure(text_key: &'static str, reason: &str) -> AppError {
    AppError::new("keys.remote_failed").with_reason(text_key, reason)
}
fn script(action: &str, line: &str) -> Result<String, AppError> {
    let key = if action == "list" {
        None
    } else {
        Some(parse(line).ok_or_else(|| AppError::new("keys.invalid_public_key"))?)
    };
    Ok(format!(
        "action={}\nline={}\nkind={}\ndata={}\n{}",
        sh_quote(action)?,
        sh_quote(line)?,
        sh_quote(key.as_ref().map_or("", |k| k.kind.as_str()))?,
        sh_quote(key.as_ref().map_or("", |k| k.data.as_str()))?,
        include_str!("authorized.sh")
    ))
}
fn run(
    program: &SshProgram,
    cmd: &SshCommand,
    action: &str,
    line: &str,
) -> Result<serde_json::Value, AppError> {
    run_script(program, cmd, &script(action, line)?, RunLimits::default()).map_err(translate_error)
}
fn translate_error(e: AppError) -> AppError {
    if e.code != "remote.script_failed" {
        return e;
    }
    let (text_key, reason) = match e.params.get("exit_code").map(String::as_str) {
        Some("71") => ("keys.remote_lock_timeout", "等待远程授权文件锁超过十秒"),
        Some("72") => ("keys.remote_interrupted", "远程授权文件操作被信号中断"),
        Some("73") => ("keys.dangling_authorized_link", "授权文件是悬空符号链接"),
        Some("74") => ("keys.authorized_not_file", "授权文件不是普通文件"),
        Some("75") => ("keys.remote_permissions", "无法读取远程权限"),
        _ => return e,
    };
    let locked = e.params.get("exit_code").is_some_and(|code| code == "71");
    let mut error = failure(text_key, reason).with("cause", e.code);
    if let Some(code) = e.params.get("exit_code") {
        error = error.with("exit_code", code);
    }
    if locked {
        error = error.with("path", "~/.ssh/.pierlink-lock");
    }
    error
}
/// 用密码连接 cmd 追加 public_line；返回安装状态及已有权限警告。
pub fn install(
    program: &SshProgram,
    password_cmd: &SshCommand,
    public_line: &str,
) -> Result<InstallReport, AppError> {
    let result = run(program, password_cmd, "install", public_line)?;
    let outcome = match result["status"].as_str() {
        Some("installed") => InstallOutcome::Installed,
        Some("already_present") => InstallOutcome::AlreadyPresent,
        _ => return Err(failure("keys.invalid_installation_result", "安装结果无效")),
    };
    Ok(InstallReport {
        non_default_authorized_keys: result["non_default_authorized_keys"].as_bool().ok_or_else(
            || {
                failure(
                    "keys.invalid_authorization_path_check_result",
                    "授权路径检查结果无效",
                )
            },
        )?,
        outcome,
        permissions_warning: result["permissions_warning"]
            .as_bool()
            .ok_or_else(|| failure("keys.invalid_permission_check_result", "权限检查结果无效"))?,
    })
}
/// 用仅公钥认证的后台 key_cmd 实际登录；拒绝可能退回密码或复用连接的参数。
pub fn verify(program: &SshProgram, key_cmd: &SshCommand) -> Result<(), AppError> {
    if let Some(context) = &key_cmd.builtin {
        let verification = context.verification()?;
        let manager = verification.manager.clone();
        let mut command = key_cmd.clone();
        command.builtin = Some(std::sync::Arc::new(verification));
        let result = run_script(
            program,
            &command,
            "printf '{\"verified\":true}'",
            RunLimits::default(),
        );
        // 验证连接不进入业务连接池，成功、失败都立即释放。
        let _ = manager.close_all();
        return match result? {
            value if value["verified"] == true => Ok(()),
            _ => Err(AppError::new("keys.verify_failed")),
        };
    }
    for required in [
        "IdentitiesOnly=yes",
        "IdentityAgent=none",
        "PreferredAuthentications=publickey",
        "PasswordAuthentication=no",
        "KbdInteractiveAuthentication=no",
        "ControlPath=none",
    ] {
        if !key_cmd
            .args
            .windows(2)
            .any(|p| p[0] == "-o" && p[1] == required)
        {
            return Err(AppError::new("keys.invalid_verify_command"));
        }
    }
    let result = run_script(
        program,
        key_cmd,
        "printf '{\"verified\":true}'",
        RunLimits::default(),
    )?;
    if result["verified"] != true {
        return Err(AppError::new("keys.verify_failed"));
    }
    Ok(())
}
/// 列出 cmd 目标的有效公钥，按 our_public_line 的类型和数据标记本机密钥。
pub fn list(
    program: &SshProgram,
    cmd: &SshCommand,
    our_public_line: &str,
) -> Result<Vec<AuthorizedKey>, AppError> {
    let our = parse(our_public_line).ok_or_else(|| AppError::new("keys.invalid_public_key"))?;
    let result = run(program, cmd, "list", "")?;
    let hex = result["hex"]
        .as_str()
        .ok_or_else(|| failure("keys.invalid_list_result", "列表结果无效"))?;
    if hex.len() % 2 != 0 || !hex.is_ascii() {
        return Err(failure("keys.invalid_list_encoding", "列表编码无效"));
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|_| failure("keys.invalid_list_encoding", "列表编码无效"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            parse(line).map(|key| AuthorizedKey {
                line_no: (i + 1) as u32,
                ours: key.kind == our.kind && key.data == our.data,
                pierlink: key.comment.starts_with("pierlink-"),
                public_line: format!("{} {}", key.kind, key.data),
                key_type: key.kind,
                fingerprint: key.fingerprint,
                comment: key.comment,
            })
        })
        .collect())
}
/// 删除 cmd 目标中与 public_line 内容相同的全部行；符号链接保持不变。
pub fn remove(
    program: &SshProgram,
    cmd: &SshCommand,
    public_line: &str,
) -> Result<RemoveOutcome, AppError> {
    let result = run(program, cmd, "remove", public_line)?;
    match result["status"].as_str() {
        Some("symlink") => Ok(RemoveOutcome::Symlink),
        Some("not_found") => Ok(RemoveOutcome::NotFound),
        Some("removed") => Ok(RemoveOutcome::Removed {
            count: result["count"]
                .as_u64()
                .and_then(|n| n.try_into().ok())
                .ok_or_else(|| failure("keys.invalid_deletion_count", "删除数量无效"))?,
        }),
        _ => Err(failure("keys.invalid_deletion_result", "删除结果无效")),
    }
}
/// 安装后用 key_cmd 验证；失败仅撤销本次新增内容，不删除安装前已存在的公钥。
pub fn install_and_verify(
    program: &SshProgram,
    password_cmd: &SshCommand,
    key_cmd: &SshCommand,
    public_line: &str,
) -> Result<InstallReport, AppError> {
    let report = install(program, password_cmd, public_line)?;
    if let Err(error) = verify(program, key_cmd) {
        // 密码已经验证过（安装成功），这里失败说明“只用密钥登录”被拒：统一报 keys.verify_failed，
        // 不能把验证步骤的 ssh.auth_failed 原样返回，否则界面会误报成“密码不对”
        let verify_failed = |text_key: &'static str, reason: &str| {
            AppError::new("keys.verify_failed")
                .with_reason(text_key, reason)
                .with("cause", &error.code)
        };
        if report.outcome == InstallOutcome::AlreadyPresent {
            return Err(verify_failed(
                "keys.verification_failed_existing_keys_were_preserved",
                "验证失败；原有密钥未改动",
            ));
        }
        let rollback = remove(program, password_cmd, public_line);
        if matches!(
            rollback,
            Ok(RemoveOutcome::Removed { .. } | RemoveOutcome::NotFound)
        ) {
            return Err(verify_failed(
                "keys.verification_failed_the_added_key_was_removed",
                "验证失败；已撤销",
            ));
        }
        let key = parse(public_line).ok_or_else(|| AppError::new("keys.invalid_public_key"))?;
        let (text_key, reason) = if matches!(rollback, Ok(RemoveOutcome::Symlink)) {
            (
                "keys.rollback_symlink",
                "验证失败；授权文件是符号链接，需手动撤销",
            )
        } else {
            (
                "keys.rollback_failed",
                "验证失败；撤销失败，需手动检查授权文件",
            )
        };
        let mut failure = AppError::new("keys.verify_failed")
            .with_reason(text_key, reason)
            .with("cause", error.code)
            .with("fingerprint", key.fingerprint)
            .with("comment", key.comment);
        if let Err(rollback) = rollback {
            failure = failure.with("rollback_cause", rollback.code);
        }
        return Err(failure);
    }
    Ok(report)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA pierlink-test";
    fn command(home: &std::path::Path) -> SshCommand {
        SshCommand {
            args: vec!["-s".into()],
            env: vec![("HOME".into(), home.to_str().unwrap().into())],
            config_text: None,
            builtin: None,
            askpass: None,
        }
    }
    fn rejected_command(home: &std::path::Path, body: &str) -> SshCommand {
        let mut cmd = command(home);
        cmd.args = vec!["-c".into(), body.into(), "验证".into()];
        for option in [
            "IdentitiesOnly=yes",
            "IdentityAgent=none",
            "PreferredAuthentications=publickey",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "ControlPath=none",
        ] {
            cmd.args.extend(["-o".into(), option.into()]);
        }
        cmd
    }

    #[test]
    fn 仅翻译脚本保留退出码且锁错误包含路径() {
        for code in [
            "ssh.auth_failed",
            "ssh.too_many_auth_attempts",
            "ssh.unreachable",
            "ssh.host_key_changed",
            "ssh.host_key_unknown",
            "remote.timeout",
            "remote.output_too_large",
        ] {
            let original = AppError::new(code)
                .with("exit_code", 71)
                .with("其他", "数据");
            let result = translate_error(original.clone());
            assert_eq!(result.code, original.code);
            assert_eq!(result.params, original.params);
        }
        for status in [1, 70, 76, 255] {
            let error =
                translate_error(AppError::new("remote.script_failed").with("exit_code", status));
            assert_eq!(error.code, "remote.script_failed");
        }
        for status in 71..=75 {
            let error =
                translate_error(AppError::new("remote.script_failed").with("exit_code", status));
            assert_eq!(error.code, "keys.remote_failed");
            assert_eq!(error.params["exit_code"], status.to_string());
            if status == 71 {
                assert_eq!(error.params["path"], "~/.ssh/.pierlink-lock");
            }
        }
    }

    #[test]
    fn s18_认证网络与主机指纹错误原样返回() {
        let root = crate::test_support::tempdir();
        let fake = root.path().join("fake-ssh");
        let program = SshProgram { path: fake.clone() };
        let cmd = command(root.path());
        let key_cmd = rejected_command(root.path(), "不执行");
        for (stderr, code) in [
            ("Permission denied (publickey,password).", "ssh.auth_failed"),
            ("Connection refused", "ssh.unreachable"),
            (
                "REMOTE HOST IDENTIFICATION HAS CHANGED",
                "ssh.host_key_changed",
            ),
            ("Host key verification failed", "ssh.host_key_unknown"),
        ] {
            fs::write(
                &fake,
                format!(
                    "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' {} >&2\nexit 255\n",
                    sh_quote(stderr).unwrap()
                ),
            )
            .unwrap();
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(install(&program, &cmd, KEY).unwrap_err().code, code);
            assert_eq!(list(&program, &cmd, KEY).unwrap_err().code, code);
            assert_eq!(remove(&program, &cmd, KEY).unwrap_err().code, code);
            assert_eq!(verify(&program, &key_cmd).unwrap_err().code, code);
            assert_eq!(
                install_and_verify(&program, &cmd, &key_cmd, KEY)
                    .unwrap_err()
                    .code,
                code
            );
        }
    }

    #[test]
    fn s15_远程陈旧锁清理且新锁不被抢占() {
        for shell in ["/bin/sh", "/bin/dash"] {
            let root = crate::test_support::tempdir();
            let lock = root.path().join(".ssh/.pierlink-lock");
            fs::create_dir_all(&lock).unwrap();
            fs::write(lock.join("new"), "残留").unwrap();
            fs::File::open(&lock)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(
                    std::time::SystemTime::now() - std::time::Duration::from_secs(601),
                ))
                .unwrap();
            let program = SshProgram { path: shell.into() };
            let cmd = command(root.path());
            assert_eq!(
                install(&program, &cmd, KEY).unwrap().outcome,
                InstallOutcome::Installed
            );
            assert!(!lock.exists());
            fs::create_dir(&lock).unwrap();
            // 用无延时的假 sleep 加快十秒等待路径，锁龄仍按真实时钟判断。
            let sleep = root.path().join("sleep");
            fs::write(&sleep, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&sleep, fs::Permissions::from_mode(0o700)).unwrap();
            let mut fresh = cmd.clone();
            fresh.env.push((
                "PATH".into(),
                format!("{}:/usr/bin:/bin", root.path().display()),
            ));
            let error = install(&program, &fresh, KEY).unwrap_err();
            assert_eq!(error.code, "keys.remote_failed");
            assert_eq!(error.params["path"], "~/.ssh/.pierlink-lock");
            assert!(lock.is_dir());
        }
    }
    #[test]
    fn s15_s17_s20_脚本边界与并发() {
        for shell in ["/bin/sh", "/bin/dash"] {
            let root = crate::test_support::tempdir();
            let program = SshProgram { path: shell.into() };
            let cmd = command(root.path());
            assert!(list(&program, &cmd, KEY).unwrap().is_empty());
            assert_eq!(
                remove(&program, &cmd, KEY).unwrap(),
                RemoveOutcome::NotFound
            );
            let first = install(&program, &cmd, KEY).unwrap();
            assert_eq!(first.outcome, InstallOutcome::Installed);
            assert!(!first.permissions_warning);
            let file = root.path().join(".ssh/authorized_keys");
            assert_eq!(
                fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                install(&program, &cmd, KEY).unwrap().outcome,
                InstallOutcome::AlreadyPresent
            );
            let keys = list(&program, &cmd, KEY).unwrap();
            assert_eq!(keys.len(), 1);
            assert!(keys[0].ours && keys[0].pierlink);
            assert_eq!(keys[0].line_no, 1);
            fs::write(
                &file,
                format!("# 保留注释\nfrom=\"host\",command=\"echo hi\" {KEY} extra\n{KEY}\n"),
            )
            .unwrap();
            assert_eq!(
                install(&program, &cmd, KEY).unwrap().outcome,
                InstallOutcome::AlreadyPresent
            );
            assert_eq!(
                remove(&program, &cmd, KEY).unwrap(),
                RemoveOutcome::Removed { count: 2 }
            );
            assert_eq!(fs::read_to_string(&file).unwrap(), "# 保留注释\n");
            fs::write(&file, format!("{KEY}\n# 最后一行没有换行")).unwrap();
            remove(&program, &cmd, KEY).unwrap();
            assert_eq!(fs::read_to_string(&file).unwrap(), "# 最后一行没有换行");
            fs::write(&file, "# 没有换行").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).unwrap();
            assert!(install(&program, &cmd, KEY).unwrap().permissions_warning);
            assert!(
                fs::read_to_string(&file)
                    .unwrap()
                    .starts_with("# 没有换行\nssh-")
            );
            assert_eq!(
                fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o666
            );
            remove(&program, &cmd, KEY).unwrap();
            std::thread::scope(|scope| {
                let a = scope.spawn(|| install(&program, &cmd, KEY).unwrap().outcome);
                let b = scope.spawn(|| install(&program, &cmd, KEY).unwrap().outcome);
                assert_ne!(a.join().unwrap(), b.join().unwrap());
            });
            let target = root.path().join("actual");
            fs::rename(&file, &target).unwrap();
            symlink(&target, &file).unwrap();
            assert_eq!(remove(&program, &cmd, KEY).unwrap(), RemoveOutcome::Symlink);
            assert_eq!(list(&program, &cmd, KEY).unwrap().len(), 1);
        }
    }
    #[test]
    fn s16_验证失败撤销且保留原有密钥() {
        let root = crate::test_support::tempdir();
        let program = SshProgram {
            path: "/bin/sh".into(),
        };
        let cmd = command(root.path());
        assert!(verify(&program, &cmd).is_err());
        // 模拟 SSH 认证退出 255，认证约束保持与真实命令一致。
        let mut rejected = cmd.clone();
        rejected.args = vec![
            "-c".into(),
            "cat >/dev/null; exit 255".into(),
            "验证".into(),
        ];
        for option in [
            "IdentitiesOnly=yes",
            "IdentityAgent=none",
            "PreferredAuthentications=publickey",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "ControlPath=none",
        ] {
            rejected.args.extend(["-o".into(), option.into()]);
        }
        let error = install_and_verify(&program, &cmd, &rejected, KEY).unwrap_err();
        // 验证失败统一报 keys.verify_failed，原始错误码放在 cause（界面不会误报成“密码不对”）
        assert_eq!(error.code, "keys.verify_failed");
        assert_eq!(error.params["cause"], "ssh.failed");
        assert!(format!("{error:?}").contains("已撤销"));
        assert!(list(&program, &cmd, KEY).unwrap().is_empty());
        install(&program, &cmd, KEY).unwrap();
        assert!(install_and_verify(&program, &cmd, &cmd, KEY).is_err());
        assert_eq!(list(&program, &cmd, KEY).unwrap().len(), 1);
    }

    #[test]
    fn s16_s17_撤销失败标明指纹与注释() {
        for symlink in [false, true] {
            let root = crate::test_support::tempdir();
            let program = SshProgram {
                path: "/bin/sh".into(),
            };
            let mut cmd = command(root.path());
            cmd.args = vec!["-c".into(), "if [ -e \"$HOME/fail-remove\" ]; then cat >/dev/null; echo 'Connection refused' >&2; exit 255; fi; exec /bin/sh -s".into()];
            let body = if symlink {
                "cat >/dev/null; mv \"$HOME/.ssh/authorized_keys\" \"$HOME/actual\"; ln -s \"$HOME/actual\" \"$HOME/.ssh/authorized_keys\"; echo 'Permission denied (publickey).' >&2; exit 255"
            } else {
                "cat >/dev/null; touch \"$HOME/fail-remove\"; echo 'Permission denied (publickey).' >&2; exit 255"
            };
            let rejected = rejected_command(root.path(), body);
            let error = install_and_verify(&program, &cmd, &rejected, KEY).unwrap_err();
            assert_eq!(error.code, "keys.verify_failed");
            assert_eq!(error.params["cause"], "ssh.auth_failed");
            assert_eq!(error.params["fingerprint"], parse(KEY).unwrap().fingerprint);
            assert_eq!(error.params["comment"], "pierlink-test");
            assert_eq!(
                fs::read_to_string(root.path().join(".ssh/authorized_keys"))
                    .unwrap()
                    .trim(),
                KEY
            );
        }
    }
}

/// 将 password 写入 password_cmd 登记的服务器内存库，再安装并验证专用密钥，返回安装状态或脱敏错误。
/// password_cmd 使用内置上下文或 IPC AskPass 登记；不创建临时凭据管理器条目。
pub fn install_with_password(
    program: &SshProgram,
    password_cmd: &SshCommand,
    key_cmd: &SshCommand,
    public_line: &str,
    password: &str,
) -> Result<InstallReport, AppError> {
    let secret = zeroize::Zeroizing::new(password.to_owned());
    if let Some(context) = &password_cmd.builtin {
        context.vault.set(&context.server.id, secret)?;
    } else {
        let registration = password_cmd
            .askpass
            .as_ref()
            .ok_or_else(|| AppError::new("keys.invalid_password_command"))?;
        registration.set_password(secret)?;
    }
    install_and_verify(program, password_cmd, key_cmd, public_line).map_err(|mut error| {
        for value in error.params.values_mut() {
            *value = crate::security::redact(value, &[password]);
        }
        error
    })
}

#[cfg(all(test, unix))]
mod credential_tests {
    use super::*;
    #[test]
    fn s13_s18_安装使用内存密码且失败不重试不泄密() {
        let root = crate::test_support::tempdir();

        let cmd = SshCommand {
            args: vec!["-s".into()],
            env: vec![("HOME".into(), root.path().to_str().unwrap().into())],
            config_text: None,
            builtin: None,
            askpass: Some(crate::ssh::askpass_pipe::test_registration("user@host")),
        };
        let fake = root.path().join("fake-ssh");
        std::fs::write(&fake, "#!/bin/sh\ncat >/dev/null\nprintf 'Permission denied (password). 测试秘密\\n' >&2\nexit 255\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = install_with_password(
            &SshProgram { path: fake },
            &cmd,
            &cmd,
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "测试秘密",
        );
        assert_eq!(result.as_ref().unwrap_err().code, "ssh.auth_failed");
        assert!(!format!("{result:?}").contains("测试秘密"));
    }
}
