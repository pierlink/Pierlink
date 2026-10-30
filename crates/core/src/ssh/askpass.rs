//! 严格匹配 OpenSSH 提示；不对跳板或主机确认提示提供秘密。
use crate::{error::AppError, security::CredentialStore};
use std::path::PathBuf;

pub enum AskRequest {
    KeyPassphrase { key_path: PathBuf },
    Password { expected: String },
}

// OpenSSH 只把主机名转成 ASCII 小写；用户名区分大小写。
/// 规范化 expected 的用户@主机；只转换主机的 ASCII 大写，返回完整目标字符串。
pub(crate) fn normalize_password_target(expected: &str) -> String {
    match expected.rsplit_once('@') {
        Some((user, host)) => format!("{user}@{}", host.to_ascii_lowercase()),
        None => expected.to_owned(),
    }
}

/// ssh 的提示用 `%.100s` 显示密钥路径：超过 100 字节时只显示前 100 字节（可能截断在多字节字符中间）。
/// 路径不超过 100 字节时必须完全相同；超过时显示内容必须是完整路径的前缀且不短于 96 字节。
fn key_path_shown(full: &str, shown: &str) -> bool {
    const LIMIT: usize = 100;
    if full.len() <= LIMIT {
        return shown == full;
    }
    // 截断在多字节字符中间时，Windows 转换后会出现替换字符，去掉后再比较
    let shown = shown.trim_end_matches('\u{FFFD}');
    shown.len() <= LIMIT && shown.len() + 4 >= LIMIT && full.starts_with(shown)
}

/// 按 req 严格匹配完整 prompt；主机名规范为小写，密钥路径允许反斜杠加倍。
/// 成功时按 credential_id 从 store 读取秘密，否则返回脱敏错误。
pub fn answer(
    prompt: &str,
    req: &AskRequest,
    store: &dyn CredentialStore,
    credential_id: &str,
) -> Result<String, AppError> {
    let matches = match req {
        AskRequest::KeyPassphrase { key_path } => {
            let path = key_path
                .to_str()
                .ok_or_else(|| AppError::new("ssh.askpass_invalid_path"))?;
            // Windows OpenSSH 对 IdentityFile 的反斜杠显示可能保留配置转义；
            // 只接受这两种路径写法，不放宽提示的其他部分。
            let doubled = path.replace('\\', "\\\\");
            prompt
                .strip_prefix("Enter passphrase for key '")
                .and_then(|p| p.strip_suffix("': "))
                .is_some_and(|shown| {
                    [path, doubled.as_str()]
                        .iter()
                        .any(|c| key_path_shown(c, shown))
                })
        }
        AskRequest::Password { expected } => {
            // 两种都是 OpenSSH 自己加上“用户@主机”的密码提示：密码认证为 `用户@主机's password: `，
            // 键盘交互认证（PAM）为 `(用户@主机) Password: `
            let target = normalize_password_target(expected);
            prompt == format!("{target}'s password: ") || prompt == format!("({target}) Password: ")
        }
    };
    if !matches {
        return Err(AppError::new("ssh.askpass_prompt_rejected"));
    }
    store
        .get(credential_id)
        .map_err(|_| AppError::new("ssh.askpass_store_failed"))?
        .filter(|s| !s.is_empty() && !s.contains(['\n', '\r', '\0']))
        .ok_or_else(|| AppError::new("ssh.askpass_credential_missing"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::MemoryCredentialStore;
    #[test]
    fn s23_仅回答完全匹配提示() {
        let store = MemoryCredentialStore::default();
        store.set("ref", "秘密").unwrap();
        let req = AskRequest::Password {
            expected: "alice@host".into(),
        };
        assert_eq!(
            answer("alice@host's password: ", &req, &store, "ref").unwrap(),
            "秘密"
        );
        for prompt in [
            "alice@host's password:  ",
            "alice@jump's password: ",
            "yes/no",
            "alice@other's password: ",
        ] {
            assert!(answer(prompt, &req, &store, "ref").is_err());
        }
        for path in ["/a b/key", r"C:\Users\a\key"] {
            let req = AskRequest::KeyPassphrase {
                key_path: path.into(),
            };
            assert!(
                answer(
                    &format!("Enter passphrase for key '{path}': "),
                    &req,
                    &store,
                    "ref"
                )
                .is_ok()
            );
        }
        assert!(answer("alice@host's password: ", &req, &store, "absent").is_err());
    }

    #[test]
    fn s23_主机名规范化且用户名仍区分大小写() {
        let store = MemoryCredentialStore::default();
        store.set("ref", "秘密").unwrap();
        for host in ["LOCALHOST", "Example.COM", "127.0.0.1", "2001:DB8::A"] {
            let req = AskRequest::Password {
                expected: format!("Alice@{host}"),
            };
            let expected = format!("Alice@{}'s password: ", host.to_ascii_lowercase());
            assert!(answer(&expected, &req, &store, "ref").is_ok());
            for prompt in [expected.replace("Alice", "alice"), format!("{expected} ")] {
                assert!(answer(&prompt, &req, &store, "ref").is_err());
            }
        }
    }

    #[test]
    fn s23_仅密钥路径接受反斜杠加倍() {
        let store = MemoryCredentialStore::default();
        store.set("ref", "秘密").unwrap();
        let path = r"C:\Users\Alice Smith\密钥";
        let req = AskRequest::KeyPassphrase {
            key_path: path.into(),
        };
        for spelling in [path.to_owned(), path.replace('\\', "\\\\")] {
            let prompt = format!("Enter passphrase for key '{spelling}': ");
            assert!(answer(&prompt, &req, &store, "ref").is_ok());
            for bad in [
                format!("{prompt} "),
                prompt.replace("密钥", "其他密钥"),
                prompt.replace("Enter", "enter"),
            ] {
                assert!(answer(&bad, &req, &store, "ref").is_err());
            }
        }
        assert!(
            answer(
                &format!("Enter passphrase for key '{}': ", path.replace('\\', "/")),
                &req,
                &store,
                "ref"
            )
            .is_err()
        );
    }

    #[test]
    fn 长路径按_ssh_的_100_字节截断匹配() {
        let long = format!(
            r"C:\Users\{}\AppData\Local\Pierlink\keys\pierlink_ed25519",
            "a".repeat(80)
        );
        assert!(long.len() > 100);
        let shown: String = long.chars().take(100).collect();
        assert!(key_path_shown(&long, &shown));
        assert!(!key_path_shown(&long, &long[..50]));
        assert!(!key_path_shown(&long, &format!("{}x", &long[..99])));
        assert!(key_path_shown("C:\\k", "C:\\k"));
        assert!(!key_path_shown("C:\\k", "C:\\"));
        let store = MemoryCredentialStore::default();
        store.set("id", "secret").unwrap();
        let req = AskRequest::KeyPassphrase {
            key_path: PathBuf::from(&long),
        };
        assert_eq!(
            answer(
                &format!("Enter passphrase for key '{shown}': "),
                &req,
                &store,
                "id"
            )
            .unwrap(),
            "secret"
        );
    }

    #[test]
    fn 键盘交互密码提示也回答且仍然限定用户和主机() {
        let store = MemoryCredentialStore::default();
        store.set("id", "pw").unwrap();
        let req = AskRequest::Password {
            expected: "dev@Example.com".into(),
        };
        assert_eq!(
            answer("(dev@example.com) Password: ", &req, &store, "id").unwrap(),
            "pw"
        );
        assert!(answer("(root@example.com) Password: ", &req, &store, "id").is_err());
        assert!(answer("(dev@other.com) Password: ", &req, &store, "id").is_err());
        assert!(answer("Password: ", &req, &store, "id").is_err());
        assert!(answer("(dev@example.com) Verification code: ", &req, &store, "id").is_err());
    }
}
