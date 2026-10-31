//! POSIX shell 参数转义。
use crate::error::AppError;

/// 将参数转为 POSIX 单引号字面量；含 NUL 时返回错误，不在错误中回显参数。
pub fn sh_quote(s: &str) -> Result<String, AppError> {
    if s.contains('\0') {
        return Err(AppError::new("ssh.argument_contains_nul"));
    }
    Ok(format!("'{}'", s.replace('\'', "'\"'\"'")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn s9_转义与空值() {
        assert_eq!(sh_quote("").unwrap(), "''");
        assert_eq!(sh_quote("a'b").unwrap(), "'a'\"'\"'b'");
        assert!(sh_quote("秘密\0").is_err());
    }
    #[test]
    #[cfg(unix)]
    fn s9_真实_shell_逐字节往返() {
        for value in [
            "",
            "a b",
            "'\"",
            "$(printf hacked)",
            "`id`",
            "a\nb\r\t",
            "*?[]",
            "中文",
            "\u{1}\u{1b}",
            "-n",
            "\\",
        ] {
            // 验收要求的 shell 真实性检查；生产代码不启动 shell。
            let out = std::process::Command::new("/bin/sh")
                .args(["-c", &format!("printf %s {}", sh_quote(value).unwrap())])
                .output()
                .unwrap();
            assert!(out.status.success());
            assert_eq!(out.stdout, value.as_bytes());
        }
    }
}
