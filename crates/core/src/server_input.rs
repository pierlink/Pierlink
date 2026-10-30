//! 界面提交的服务器表单：校验并转成数据库记录。只处理“应用内新建”的服务器
//! （导入自 ssh config 的服务器由导入流程生成，不经这里修改连接参数）。

use crate::error::AppError;
use crate::storage::{AuthMethod, Engine, Server, ServerSource};
use serde::Deserialize;
use specta::Type;
use std::path::Path;

/// 新建或修改服务器时界面提交的内容
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ServerInput {
    /// 为空表示新建
    pub id: Option<String>,
    pub name: String,
    pub group_name: Option<String>,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub auth_method: AuthMethod,
    /// 未传时修改沿用原值，新建默认内置；未知枚举值由反序列化拒绝。
    pub connection_engine: Option<Engine>,
    /// 未传时沿用原说明，传空字符串清除；最多 512 字且不能含控制字符。
    pub engine_note: Option<String>,
    /// 私钥文件的绝对路径（只记录路径，不读内容）
    pub identity_file: Option<String>,
    /// 登录 shell 的绝对路径，空表示服务器默认
    pub login_shell: Option<String>,
    pub notes: String,
}

fn invalid(field: &str) -> AppError {
    AppError::new("server.invalid_field").with("field", field)
}

/// 去掉首尾空白；空字符串视为未填
fn optional(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn has_control(s: &str) -> bool {
    s.chars().any(char::is_control)
}

/// 主机名或 IP：不能为空、不能有空白和控制字符、不能以 “-” 开头（防止被 ssh 当成选项）
fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && !h.starts_with('-')
        && !h.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(
                    c,
                    '@' | '/' | '\\' | '\'' | '"' | '`' | '$' | ';' | '|' | '&'
                )
        })
}

/// 用户名：POSIX 允许的字符，额外允许 “.”、“@” 之外的常见字符；不能以 “-” 开头
fn valid_user(u: &str) -> bool {
    !u.is_empty()
        && u.len() <= 64
        && !u.starts_with('-')
        && u.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '$'))
}

/// 校验表单并生成要写入的服务器记录。
///
/// :param input: 界面提交的表单
/// :param existing: 修改时的原记录（新建时为 None）；表单里没有的字段沿用原值
/// :param new_id: 新建时使用的 ID
/// :param now_ms: 当前时间（Unix 毫秒）
/// :return: 完整的服务器记录；字段不合法时返回 `server.invalid_field`（附字段名）
pub fn build_server(
    input: &ServerInput,
    existing: Option<&Server>,
    new_id: &str,
    now_ms: i64,
) -> Result<Server, AppError> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 64 || has_control(name) {
        return Err(invalid("name"));
    }
    let group_name = optional(&input.group_name);
    if group_name
        .as_deref()
        .is_some_and(|g| g.chars().count() > 64 || has_control(g))
    {
        return Err(invalid("groupName"));
    }
    let host = input.host.trim();
    if !valid_host(host) {
        return Err(invalid("host"));
    }
    if input.port == 0 {
        return Err(invalid("port"));
    }
    let username = optional(&input.username);
    if username.as_deref().is_some_and(|u| !valid_user(u)) {
        return Err(invalid("username"));
    }
    let identity_file = optional(&input.identity_file);
    if identity_file
        .as_deref()
        .is_some_and(|p| !Path::new(p).is_absolute() || has_control(p) || p.len() > 1024)
    {
        return Err(invalid("identityFile"));
    }
    // 专用密钥只能经安装流程（安装并验证后由后端设置）得到，表单只能保持、不能新设
    if input.auth_method == AuthMethod::DedicatedKey
        && existing.is_none_or(|b| b.auth_method != AuthMethod::DedicatedKey)
    {
        return Err(invalid("authMethod"));
    }
    if input.auth_method == AuthMethod::Key && identity_file.is_none() {
        return Err(invalid("identityFile"));
    }
    let login_shell = optional(&input.login_shell);
    // 与 ssh::args 生成终端命令时的规则一致：只允许路径常用字符，保存时就拒绝，避免打开时才失败
    if login_shell.as_deref().is_some_and(|s| {
        !s.starts_with('/')
            || s.len() > 256
            || !s
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c))
    }) {
        return Err(invalid("loginShell"));
    }
    if input.notes.chars().count() > 4000 {
        return Err(invalid("notes"));
    }
    let engine_note = optional(&input.engine_note);
    if engine_note
        .as_deref()
        .is_some_and(|s| s.chars().count() > 512 || has_control(s))
    {
        return Err(invalid("engineNote"));
    }
    if let Some(old) = existing
        && old.source != ServerSource::Manual
    {
        return Err(AppError::new("server.not_editable"));
    }

    let base = existing.cloned();
    Ok(Server {
        id: base
            .as_ref()
            .map_or_else(|| new_id.to_owned(), |b| b.id.clone()),
        name: name.to_owned(),
        group_name,
        favorite: base.as_ref().is_some_and(|b| b.favorite),
        sort_order: base.as_ref().map_or(0, |b| b.sort_order),
        source: ServerSource::Manual,
        connection_engine: input
            .connection_engine
            .or_else(|| existing.map(|s| s.connection_engine))
            .unwrap_or(Engine::Builtin),
        engine_note: if input.engine_note.is_some() {
            engine_note
        } else {
            existing.and_then(|s| s.engine_note.clone())
        },
        ssh_alias: None,
        host: Some(host.to_owned()),
        port: Some(input.port),
        username,
        auth_method: input.auth_method,
        identity_file: if input.auth_method == AuthMethod::Key {
            identity_file.clone()
        } else {
            base.as_ref()
                .filter(|b| b.auth_method == input.auth_method && !b.identity_files.is_empty())
                .and_then(|b| b.identity_file.clone())
        },
        identity_files: base
            .as_ref()
            .filter(|b| {
                b.auth_method == input.auth_method
                    && (input.auth_method != AuthMethod::Key || b.identity_file == identity_file)
            })
            .map(|b| b.identity_files.clone())
            .unwrap_or_default(),
        identities_only: base
            .as_ref()
            .is_some_and(|b| b.auth_method == input.auth_method && b.identities_only),
        host_key_alias: base.as_ref().and_then(|b| b.host_key_alias.clone()),
        jump_server_id: base.as_ref().and_then(|b| b.jump_server_id.clone()),
        login_shell,
        use_tmux: false,
        // 换成非密码方式时不再保留记住密码的标记
        remember_password: input.auth_method == AuthMethod::Password
            && base.as_ref().is_some_and(|b| b.remember_password),
        password_cred: if input.auth_method == AuthMethod::Password {
            base.as_ref().and_then(|b| b.password_cred.clone())
        } else {
            None
        },
        // 地址或端口变了，之前确认的指纹不再适用
        host_key_sha256: base.as_ref().and_then(|b| {
            (b.host.as_deref() == Some(host) && b.port == Some(input.port))
                .then(|| b.host_key_sha256.clone())
                .flatten()
        }),
        vnc_autostart: base.as_ref().is_some_and(|b| b.vnc_autostart),
        notes: input.notes.clone(),
        created_at: base.as_ref().map_or(now_ms, |b| b.created_at),
        updated_at: now_ms,
    })
}

/// 根据 alias、可选分组、new_id 和 Unix 毫秒时间生成链接型记录；非法别名或分组返回错误。
/// 连接参数不入库，后续使用时重新读取 SSH 配置。
pub fn build_alias_server(
    alias: &str,
    group_name: Option<&str>,
    new_id: &str,
    now_ms: i64,
) -> Result<Server, AppError> {
    crate::ssh::config_file::validate_alias(alias)?;
    let group_name = group_name.map(str::trim).filter(|g| !g.is_empty());
    if group_name.is_some_and(|g| g.chars().count() > 64 || has_control(g)) {
        return Err(invalid("groupName"));
    }
    Ok(Server {
        id: new_id.into(),
        name: alias.into(),
        group_name: group_name.map(str::to_owned),
        favorite: false,
        sort_order: 0,
        source: ServerSource::SshConfig,
        connection_engine: Engine::OpenSsh,
        engine_note: Some("从 ssh config 导入，按原配置连接".into()),
        ssh_alias: Some(alias.into()),
        host: None,
        port: None,
        username: None,
        auth_method: AuthMethod::Agent,
        identity_file: None,
        identity_files: vec![],
        identities_only: false,
        host_key_alias: None,
        jump_server_id: None,
        login_shell: None,
        use_tmux: false,
        remember_password: false,
        password_cred: None,
        host_key_sha256: None,
        vnc_autostart: false,
        notes: String::new(),
        created_at: now_ms,
        updated_at: now_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ServerInput {
        ServerInput {
            id: None,
            name: " gpu-dev ".into(),
            group_name: Some("  ".into()),
            host: "203.0.113.18".into(),
            port: 22,
            username: Some("dev".into()),
            auth_method: AuthMethod::Agent,
            connection_engine: None,
            engine_note: None,
            identity_file: None,
            login_shell: None,
            notes: String::new(),
        }
    }

    #[test]
    fn 编辑转换后的记录保留身份列表且切换认证清理限制() {
        let mut old = build_server(&input(), None, "s", 1).unwrap();
        old.identity_file = Some("/first".into());
        old.identity_files = vec!["/first".into(), "/second".into()];
        old.identities_only = true;
        old.host_key_alias = Some("known-alias".into());
        let kept = build_server(&input(), Some(&old), "s", 2).unwrap();
        assert_eq!(kept.identity_file, old.identity_file);
        assert_eq!(kept.identity_files, old.identity_files);
        assert!(kept.identities_only);
        assert_eq!(kept.host_key_alias, old.host_key_alias);
        let mut changed = input();
        changed.auth_method = AuthMethod::Password;
        let changed = build_server(&changed, Some(&old), "s", 3).unwrap();
        assert!(changed.identity_files.is_empty());
        assert!(!changed.identities_only);
    }

    #[test]
    fn 新建时去空白并补默认值() {
        let s = build_server(&input(), None, "id-1", 100).unwrap();
        assert_eq!(s.id, "id-1");
        assert_eq!(s.name, "gpu-dev");
        assert_eq!(s.group_name, None);
        assert_eq!(s.created_at, 100);
        assert_eq!(s.source, ServerSource::Manual);
        assert_eq!(s.connection_engine, Engine::Builtin);
        assert_eq!(s.engine_note, None);
    }

    #[test]
    fn 连接方式及说明可修改且旧表单保留原值() {
        let mut form = input();
        form.connection_engine = Some(Engine::OpenSsh);
        form.engine_note = Some("  内置连接失败后改用  ".into());
        let old = build_server(&form, None, "s", 1).unwrap();
        assert_eq!(old.connection_engine, Engine::OpenSsh);
        assert_eq!(old.engine_note.as_deref(), Some("内置连接失败后改用"));
        let kept = build_server(&input(), Some(&old), "s", 2).unwrap();
        assert_eq!(kept.connection_engine, old.connection_engine);
        assert_eq!(kept.engine_note, old.engine_note);
        form.connection_engine = Some(Engine::Builtin);
        form.engine_note = Some(String::new());
        let changed = build_server(&form, Some(&old), "s", 3).unwrap();
        assert_eq!(changed.connection_engine, Engine::Builtin);
        assert_eq!(changed.engine_note, None);
        for note in ["字".repeat(513), "说明\n第二行".into(), "说明\0".into()] {
            form.engine_note = Some(note);
            assert_eq!(
                build_server(&form, Some(&old), "s", 4).unwrap_err().params["field"],
                "engineNote"
            );
        }
        let imported = build_alias_server("dev", Some("分组"), "a", 5).unwrap();
        assert_eq!(imported.connection_engine, Engine::OpenSsh);
        assert_eq!(
            imported.engine_note.as_deref(),
            Some("从 ssh config 导入，按原配置连接")
        );
    }

    #[test]
    fn 表单接受两种引擎且拒绝未知引擎() {
        let mut value = serde_json::json!({
            "name": "服务器", "host": "example.test", "port": 22,
            "authMethod": "agent", "notes": ""
        });
        let legacy: ServerInput = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            build_server(&legacy, None, "s", 0)
                .unwrap()
                .connection_engine,
            Engine::Builtin
        );
        for (text, expected) in [("builtin", Engine::Builtin), ("openssh", Engine::OpenSsh)] {
            value["connectionEngine"] = text.into();
            let parsed: ServerInput = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(parsed.connection_engine, Some(expected));
        }
        value["connectionEngine"] = "unknown".into();
        assert!(serde_json::from_value::<ServerInput>(value).is_err());
    }

    #[test]
    fn 拒绝可能被当成参数或注入的值() {
        for host in ["", "-oProxyCommand=x", "a b", "h;rm", "user@h", "h\nx"] {
            let mut i = input();
            i.host = host.into();
            assert_eq!(
                build_server(&i, None, "x", 0).unwrap_err().params["field"],
                "host",
                "{host:?}"
            );
        }
        for user in ["-x", "a b", "a@b", "名字"] {
            let mut i = input();
            i.username = Some(user.into());
            assert_eq!(
                build_server(&i, None, "x", 0).unwrap_err().params["field"],
                "username",
                "{user:?}"
            );
        }
        for shell in [
            "zsh",
            "/bin/z sh",
            "/bin/zsh;id",
            "/opt/my+shell",
            "/bin/$SHELL",
        ] {
            let mut i = input();
            i.login_shell = Some(shell.into());
            assert_eq!(
                build_server(&i, None, "x", 0).unwrap_err().params["field"],
                "loginShell",
                "{shell:?}"
            );
        }
        let mut i = input();
        i.login_shell = Some("/usr/local/bin/fish-3.7_x".into());
        build_server(&i, None, "x", 0).unwrap();
        let mut i = input();
        i.auth_method = AuthMethod::DedicatedKey;
        assert_eq!(
            build_server(&i, None, "x", 0).unwrap_err().params["field"],
            "authMethod"
        );
        let mut base = build_server(&input(), None, "x", 0).unwrap();
        assert!(build_server(&i, Some(&base), "x", 0).is_err());
        base.auth_method = AuthMethod::DedicatedKey;
        assert_eq!(
            build_server(&i, Some(&base), "x", 0).unwrap().auth_method,
            AuthMethod::DedicatedKey
        );
        let mut i = input();
        i.port = 0;
        assert_eq!(
            build_server(&i, None, "x", 0).unwrap_err().params["field"],
            "port"
        );
    }

    #[test]
    fn 私钥方式必须给绝对路径() {
        let mut i = input();
        i.auth_method = AuthMethod::Key;
        assert_eq!(
            build_server(&i, None, "x", 0).unwrap_err().params["field"],
            "identityFile"
        );
        i.identity_file = Some("id_ed25519".into());
        assert_eq!(
            build_server(&i, None, "x", 0).unwrap_err().params["field"],
            "identityFile"
        );
        i.identity_file = Some(if cfg!(windows) {
            r"C:\keys\id".into()
        } else {
            "/keys/id".into()
        });
        assert!(
            build_server(&i, None, "x", 0)
                .unwrap()
                .identity_file
                .is_some()
        );
    }

    #[test]
    fn 修改时沿用原字段且地址变化清除指纹() {
        let mut old = build_server(&input(), None, "id-1", 1).unwrap();
        old.favorite = true;
        old.host_key_sha256 = Some("SHA256:abc".into());
        let same = build_server(&input(), Some(&old), "ignored", 2).unwrap();
        assert_eq!(same.id, "id-1");
        assert!(same.favorite);
        assert_eq!(same.created_at, 1);
        assert_eq!(same.host_key_sha256.as_deref(), Some("SHA256:abc"));
        let mut moved = input();
        moved.host = "198.51.100.7".into();
        assert_eq!(
            build_server(&moved, Some(&old), "x", 3)
                .unwrap()
                .host_key_sha256,
            None
        );
    }
}
