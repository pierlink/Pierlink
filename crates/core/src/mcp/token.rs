//! 入口令牌、服务器配置文件和供用户复制的接入命令。
use crate::ssh::quote::sh_quote;
use ssh_key::encoding::base64::{Base64Unpadded, Encoding};
use zeroize::Zeroizing;

/// 无参数；返回 32 字节安全随机数的无填充 base64url 编码，释放时清零。
pub fn new_token() -> Zeroizing<String> {
    let bytes = Zeroizing::new(rand::random::<[u8; 32]>());
    let encoded = Zeroizing::new(Base64Unpadded::encode_string(bytes.as_ref()));
    Zeroizing::new(
        encoded
            .chars()
            .map(|ch| match ch {
                '+' => '-',
                '/' => '_',
                ch => ch,
            })
            .collect(),
    )
}

/// 谁写的 ~/.pierlink/mcp.json：同一台服务器、同一个用户可能被几台电脑上的 Pierlink 连着，
/// 停用入口时只删自己写的那份
#[derive(Debug, Clone, Copy)]
pub struct FileOwner<'a> {
    /// Pierlink 的实例号（随机、长期不变；只含字母数字和 -_）
    pub instance: &'a str,
    /// 电脑名（只用于显示：端口被另一台电脑占着时告诉用户是哪台）
    pub computer: &'a str,
}

/// 将 url、token、Claude 接入命令和写入者写入 ~/.pierlink/mcp.json，返回交给 run_script 的 POSIX sh 正文。
/// 正文须经标准输入发送；成功仅输出 {"ok":true}，临时文件自动清理，不将令牌放进进程参数。
/// url 含 NUL 时返回明确失败的脚本；token 由 JSON 编码，可安全包含引号和换行。
pub fn token_file_script(url: &str, token: &str, owner: FileOwner) -> String {
    if sh_quote(url).is_err() {
        return invalid_url_script();
    }
    let commands = connect_commands(url);
    let value = serde_json::json!({
        "url": url,
        "token": token,
        "claude": commands[0],
        "instance": clean_instance(owner.instance),
        "token_id": token_id(token),
        "computer": owner.computer,
    });
    format!(
        r#"set -eu
umask 077
dir="$HOME/.pierlink"
part=''
fail() {{ printf '%s\n' '无法安全写入 MCP 配置文件' >&2; exit 1; }}
trap 'if [ -n "$part" ]; then rm -f -- "$part"; fi' 0
trap 'exit 1' HUP INT TERM
[ ! -L "$dir" ] || fail
mkdir -p -- "$dir" || fail
chmod 700 -- "$dir" || fail
[ ! -d "$dir/mcp.json" ] || fail
part=$(mktemp "$dir/.mcp.json.XXXXXX") || fail
cat > "$part" <<'PIERLINK_MCP_JSON'
{value}
PIERLINK_MCP_JSON
chmod 600 -- "$part" || fail
mv -f -- "$part" "$dir/mcp.json" || fail
part=''
printf '%s\n' '{{"ok":true}}'
"#
    )
}

/// 返回删除 ~/.pierlink/mcp.json 的 run_script 正文，文件不存在也输出成功 JSON。
/// 文件是另一个实例写的（另一台电脑上的 Pierlink 正用着）就不删，输出 {"ok":true,"kept":true}；
/// 实例号对得上、或令牌的短哈希对得上（数据目录被清过、实例号变了，但令牌还是这台电脑的）都算自己的；
/// 旧版本写的文件没有实例号，照旧删。
/// :param instance: 这个 Pierlink 的实例号
/// :param token: 这个入口现在的令牌（只用它的短哈希比较；不知道时传空串）
pub fn remove_token_file_script(instance: &str, token: &str) -> String {
    let marker = format!("\"instance\":\"{}\"", clean_instance(instance));
    // 令牌为空时给一个不会出现在文件里的标记
    let token_marker = if token.is_empty() {
        "\"token_id\":\"-\"".to_owned()
    } else {
        format!("\"token_id\":\"{}\"", token_id(token))
    };
    // 只含字母数字和 -_ 加上引号冒号：放进单引号里不需要转义
    format!(
        r#"set -eu
dir="$HOME/.pierlink"
if [ -L "$dir" ]; then printf '%s\n' 'MCP 配置目录不能是符号链接' >&2; exit 1; fi
f="$dir/mcp.json"
if [ -f "$f" ] && grep -q '"instance"' "$f" && ! grep -qF '{marker}' "$f" && ! grep -qF '{token_marker}' "$f"; then
  printf '%s\n' '{{"ok":true,"kept":true}}'
  exit 0
fi
rm -f -- "$f"
printf '%s\n' '{{"ok":true}}'
"#
    )
}

/// 令牌的短哈希（SHA-256 的前 8 字节，十六进制）：写进 mcp.json，删除时认出是不是自己写的；不能反推出令牌
pub fn token_id(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 实例号只留字母数字和 -_（放进脚本和 JSON 都不用转义）
fn clean_instance(instance: &str) -> String {
    instance
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect()
}

/// 按 url 返回 jq 与无 jq 的 sed 两种 Claude 接入命令；令牌在运行时从配置读取。
/// 先删同名的再加：换令牌后重新运行同一条命令也能成功（不删的话 claude 报已存在）。
/// sed 读法用于 new_token 生成的 base64url 令牌；url 含 NUL 时返回失败脚本。
pub fn connect_commands(url: &str) -> Vec<String> {
    let Ok(url) = sh_quote(url) else {
        return vec![invalid_url_script()];
    };
    [
        r#"jq -r .token "$HOME/.pierlink/mcp.json""#,
        r#"sed -n 's/.*"token"[[:space:]]*:[[:space:]]*"\([A-Za-z0-9_-]*\)".*/\1/p' "$HOME/.pierlink/mcp.json""#,
    ].into_iter().map(|read| {
        format!("claude mcp remove --scope user pierlink >/dev/null 2>&1; claude mcp add --transport http --scope user pierlink {url} --header \"Authorization: Bearer $({read})\"")
    }).collect()
}

fn invalid_url_script() -> String {
    "printf '%s\\n' 'MCP 接入地址不能包含 NUL 字符' >&2; exit 1\n".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 令牌为三十二字节随机数的无填充编码() {
        let one = new_token();
        let two = new_token();
        assert_eq!(one.len(), 43);
        assert!(
            one.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        );
        assert_eq!(
            Base64Unpadded::decode_vec(&one.replace('-', "+").replace('_', "/"))
                .unwrap()
                .len(),
            32
        );
        assert_ne!(one, two);
    }

    #[cfg(unix)]
    mod shell {
        use super::*;
        use std::{
            io::Write,
            os::unix::fs::PermissionsExt,
            path::Path,
            process::{Command, Stdio},
        };

        const OWNER: FileOwner = FileOwner {
            instance: "inst-a",
            computer: "电脑 A",
        };

        fn run(home: &Path, script: &str) -> std::process::Output {
            let mut command = Command::new("/bin/sh");
            command.arg("-s");
            assert_eq!(command.get_args().collect::<Vec<_>>(), ["-s"]);
            let mut child = command
                .env("HOME", home)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        }

        fn framed(home: &Path, script: &str) {
            let output = run(home, &crate::remote::framing::wrap_script(script, "测试"));
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                crate::remote::framing::parse_framed(
                    std::str::from_utf8(&output.stdout).unwrap(),
                    "测试",
                    1024
                )
                .unwrap(),
                serde_json::json!({"ok":true})
            );
        }

        #[test]
        fn 文件权限原子替换特殊家目录和重复删除() {
            let root = crate::test_support::tempdir();
            let home = root.path().join("家 目录'$(printf 不执行)");
            std::fs::create_dir(&home).unwrap();
            let url = "http://127.0.0.1:47821/mcp?x='$(touch 不执行)";
            for token in [
                new_token().to_string(),
                "引号\"'\nPIERLINK_MCP_JSON\n$(id)\\\0".into(),
            ] {
                framed(&home, &token_file_script(url, &token, OWNER));
                let dir = home.join(".pierlink");
                let path = dir.join("mcp.json");
                assert_eq!(
                    std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                    0o700
                );
                assert_eq!(
                    std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
                let data: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                assert_eq!(data["url"], url);
                assert_eq!(data["token"], token);
                assert_eq!(data["claude"], connect_commands(url)[0]);
                assert_eq!(data["instance"], "inst-a");
                assert_eq!(data["token_id"], token_id(&token));
                assert_eq!(token_id(&token).len(), 16);
                assert_eq!(data["computer"], "电脑 A");
                assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
            }
            framed(&home, &remove_token_file_script("inst-a", ""));
            framed(&home, &remove_token_file_script("inst-a", ""));
            assert!(!home.join(".pierlink/mcp.json").exists());
        }

        #[test]
        fn 只删自己写的地址文件_旧版本写的照删() {
            let root = crate::test_support::tempdir();
            let home = root.path();
            let path = home.join(".pierlink/mcp.json");
            framed(
                home,
                &token_file_script("http://127.0.0.1:47821/mcp", "t", OWNER),
            );
            // 另一台电脑（另一个实例、另一个令牌）停用入口：不删
            let result = run(home, &remove_token_file_script("inst-b", "other"));
            assert!(result.status.success());
            assert!(String::from_utf8_lossy(&result.stdout).contains("kept"));
            assert!(path.exists());
            // 实例号变了（数据目录被清过）但令牌还是自己的：删
            framed(home, &remove_token_file_script("inst-new", "t"));
            assert!(!path.exists());
            // 旧版本写的（没有实例号）：照删
            std::fs::write(&path, r#"{"url":"u","token":"t"}"#).unwrap();
            framed(home, &remove_token_file_script("inst-b", ""));
            assert!(!path.exists());
        }

        #[test]
        fn 接入命令读取令牌并保持地址为单个参数() {
            let root = crate::test_support::tempdir();
            let home = root.path().join("home ' 空格");
            std::fs::create_dir(&home).unwrap();
            let url = "http://127.0.0.1:47821/mcp?'\"$(exit 7)";
            let token = new_token();
            framed(&home, &token_file_script(url, &token, OWNER));
            for command in connect_commands(url) {
                // 以函数接收参数，验证 shell 转义和两种真实读取程序，不执行 Claude。
                let script = format!(
                    "claude() {{ [ \"$2\" = add ] || return 0; printf '%s\\n' \"$#\" \"$8\" \"${{10}}\"; }}\n{command}\n"
                );
                let result = run(&home, &script);
                assert!(result.status.success());
                assert_eq!(
                    String::from_utf8(result.stdout).unwrap(),
                    format!("10\n{url}\nAuthorization: Bearer {}\n", token.as_str())
                );
            }
        }

        #[test]
        fn 非法地址和不安全目录失败且无临时文件残留() {
            let root = crate::test_support::tempdir();
            assert!(
                !run(root.path(), &token_file_script("x\0y", "秘密", OWNER))
                    .status
                    .success()
            );
            assert!(
                !run(root.path(), &connect_commands("x\0y")[0])
                    .status
                    .success()
            );
            std::fs::create_dir(root.path().join(".pierlink")).unwrap();
            std::fs::create_dir(root.path().join(".pierlink/mcp.json")).unwrap();
            let result = run(
                root.path(),
                &token_file_script("http://127.0.0.1/mcp", "秘密", OWNER),
            );
            assert!(!result.status.success());
            assert!(!String::from_utf8_lossy(&result.stderr).contains("秘密"));
            assert_eq!(
                std::fs::read_dir(root.path().join(".pierlink"))
                    .unwrap()
                    .count(),
                1
            );
        }

        #[test]
        fn 写入中途失败保留旧文件并删除临时文件() {
            let root = crate::test_support::tempdir();
            let url = "http://127.0.0.1:47821/mcp";
            framed(root.path(), &token_file_script(url, "old-token", OWNER));
            let script = token_file_script(url, "new-token", OWNER)
                .replace("chmod 600 --", "false\nchmod 600 --");
            assert!(!run(root.path(), &script).status.success());
            let dir = root.path().join(".pierlink");
            let data: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dir.join("mcp.json")).unwrap()).unwrap();
            assert_eq!(data["token"], "old-token");
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 1);
        }
    }
}
