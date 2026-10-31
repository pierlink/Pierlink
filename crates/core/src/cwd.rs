//! 通过 Linux procfs 查询带终端标记的最新 shell 当前目录。
use crate::{
    error::AppError,
    process::SshProgram,
    remote::run::{RunLimits, run_script},
    ssh::{args::SshCommand, quote::sh_quote},
};

pub(crate) fn validate_marker(marker: &str) -> Result<(), AppError> {
    if marker.is_empty()
        || marker.len() > 64
        || !marker
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err(AppError::new("ssh.invalid_argument").with("field", "cwd_marker"));
    }
    Ok(())
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct RemoteShellStatus {
    pub cwd: String,
    pub busy: bool,
}
/// 通过 program 和后台用途 cmd 查询 marker 对应 shell 的目录和前台忙碌状态；无法查询时返回 None。
pub fn remote_shell_status(
    program: &SshProgram,
    cmd: &SshCommand,
    marker: &str,
) -> Result<Option<RemoteShellStatus>, AppError> {
    validate_marker(marker)?;
    let body = format!(
        "marker={}\n{}",
        sh_quote(&format!("LC_PIERLINK_TERM={marker}"))?,
        include_str!("cwd.sh")
    );
    let value = run_script(program, cmd, &body, RunLimits::default())?;
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value).map(Some).map_err(|_| {
        AppError::new("cwd.invalid_response").with_reason(
            "cwd.terminal_status_response_must_be_an_object_or_null",
            "终端状态响应不是有效对象或空值",
        )
    })
}
/// 通过 program 和后台用途 cmd 查询 marker 对应 shell 的目录；找不到或无 procfs 返回 None。
pub fn remote_cwd(
    program: &SshProgram,
    cmd: &SshCommand,
    marker: &str,
) -> Result<Option<String>, AppError> {
    Ok(remote_shell_status(program, cmd, marker)?.map(|status| status.cwd))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::terminal::{TerminalEvent, TerminalKind, TerminalManager, TerminalSpec};
    use std::{
        sync::{Arc, mpsc},
        time::{Duration, Instant},
    };
    #[test]
    fn 标记校验及不存在的终端() {
        let program = SshProgram {
            path: "/bin/sh".into(),
        };
        let cmd = SshCommand {
            args: vec!["-s".into()],
            env: vec![],
            config_text: None,
            hooked: false,
            builtin: None,
            askpass: None,
        };
        assert!(
            remote_cwd(&program, &cmd, &uuid::Uuid::new_v4().to_string())
                .unwrap()
                .is_none()
        );
        for marker in ["", "a b", "$(id)", "a\nb", &"a".repeat(65)] {
            assert_eq!(
                remote_cwd(&program, &cmd, marker).unwrap_err().code,
                "ssh.invalid_argument"
            );
        }
    }
    #[test]
    fn 真实伪终端切换目录并查询最新shell() {
        let root = crate::test_support::tempdir();
        let directory = root.path().join("中文 ' 目录\n换行\n");
        std::fs::create_dir(&directory).unwrap();
        let marker = uuid::Uuid::new_v4().to_string();
        let manager = TerminalManager::new();
        let (tx, rx) = mpsc::channel();
        let id = manager
            .open(
                TerminalSpec {
                    kind: TerminalKind::Local {
                        program: "/bin/bash".into(),
                        args: vec!["--noprofile".into(), "--norc".into(), "-i".into()],
                        cwd: None,
                        env: vec![("LC_PIERLINK_TERM".into(), marker.clone())],
                    },
                    cols: 80,
                    rows: 24,
                },
                Arc::new(move |event| {
                    let _ = tx.send(event);
                }),
            )
            .unwrap();
        let program = SshProgram {
            path: "/bin/sh".into(),
        };
        let cmd = SshCommand {
            args: vec!["-s".into()],
            env: vec![],
            config_text: None,
            hooked: false,
            builtin: None,
            askpass: None,
        };
        let start = Instant::now();
        loop {
            if remote_cwd(&program, &cmd, &marker).unwrap().is_some() {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10));
        }
        // 再启动一层交互 shell，确保选到最新者；父 shell 的目录保持不变。
        let command = format!(
            "bash --noprofile --norc -i\ncd -- {}\nprintf '\\nCWD-READY\\n'\n",
            sh_quote(directory.to_str().unwrap()).unwrap()
        );
        manager.write(&id, command.as_bytes()).unwrap();
        let mut output = Vec::new();
        loop {
            if let TerminalEvent::Output(bytes) = rx.recv_timeout(Duration::from_secs(10)).unwrap()
            {
                manager.ack(&id, bytes.len()).unwrap();
                output.extend(bytes);
                if output
                    .windows(b"\r\nCWD-READY\r\n".len())
                    .any(|v| v == b"\r\nCWD-READY\r\n")
                {
                    break;
                }
            }
        }
        assert_eq!(
            remote_cwd(&program, &cmd, &marker).unwrap(),
            Some(directory.to_str().unwrap().to_owned())
        );
        assert!(
            !remote_shell_status(&program, &cmd, &marker)
                .unwrap()
                .unwrap()
                .busy
        );
        manager.write(&id, b"bash --noprofile --norc -c 'cd /; sleep 30; :' & printf '\\nNONINTERACTIVE-READY\\n'\n").unwrap();
        let mut output = Vec::new();
        loop {
            if let TerminalEvent::Output(bytes) = rx.recv_timeout(Duration::from_secs(10)).unwrap()
            {
                manager.ack(&id, bytes.len()).unwrap();
                output.extend(bytes);
                if output
                    .windows(b"\r\nNONINTERACTIVE-READY\r\n".len())
                    .any(|v| v == b"\r\nNONINTERACTIVE-READY\r\n")
                {
                    break;
                }
            }
        }
        assert_eq!(
            remote_cwd(&program, &cmd, &marker).unwrap(),
            Some(directory.to_str().unwrap().to_owned())
        );
        assert!(
            !remote_shell_status(&program, &cmd, &marker)
                .unwrap()
                .unwrap()
                .busy
        );
        manager.write(&id, b"sleep 30\n").unwrap();
        let start = Instant::now();
        loop {
            if remote_shell_status(&program, &cmd, &marker)
                .unwrap()
                .unwrap()
                .busy
            {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(20));
        }
        manager.close(&id).unwrap();
    }
}
