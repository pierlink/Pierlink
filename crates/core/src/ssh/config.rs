//! 解析 OpenSSH 的有效配置输出。
use crate::error::AppError;
use std::collections::HashMap;

#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedConfig {
    pub(crate) conversion_options: std::collections::BTreeMap<String, Vec<String>>,
    pub has_forwards: bool,
    pub has_remote_command: bool,
    pub forward_agent: bool,
    pub no_host_authentication_for_localhost: bool,
    pub identities_only: bool,
    pub identity_agent: Option<String>,
    pub certificate_files: Vec<String>,
    pub global_known_hosts_files: Vec<String>,
    pub hostname: String,
    pub port: u16,
    pub user: String,
    pub identity_files: Vec<String>,
    pub proxy_jump: Option<String>,
    pub proxy_command: Option<String>,
    pub host_key_alias: Option<String>,
    pub strict_host_key_checking: String,
    pub user_known_hosts_files: Vec<String>,
}

impl std::fmt::Debug for ResolvedConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedConfig")
            .field("hostname", &crate::security::redact(&self.hostname, &[]))
            .field("port", &self.port)
            .field("proxy_command", &self.proxy_command.as_ref().map(|_| "***"))
            .finish_non_exhaustive()
    }
}

fn invalid(field: &str) -> AppError {
    AppError::new("ssh.invalid_config").with("field", field)
}

fn words(s: &str) -> Result<Vec<String>, AppError> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    for c in s.chars() {
        if Some(c) == quote {
            quote = None;
        } else if quote.is_none() && word.is_empty() && matches!(c, '\'' | '"') {
            quote = Some(c);
        } else if quote.is_none() && c.is_whitespace() {
            if !word.is_empty() {
                result.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if quote.is_some() {
        return Err(invalid("userknownhostsfile"));
    }
    if !word.is_empty() {
        result.push(word);
    }
    Ok(result)
}

/// 解析 `ssh -G` 标准输出；必填字段缺失、重复或端口无效时返回错误，不回显配置值。
/// OpenSSH 输出的多路径列表不保留引号；原始路径含空格时无法无歧义恢复，按空白分隔。
pub fn parse_ssh_g(output: &str) -> Result<ResolvedConfig, AppError> {
    let mut values = HashMap::new();
    let mut conversion_options = std::collections::BTreeMap::<String, Vec<String>>::new();
    let mut identity_files = Vec::new();
    let mut certificate_files = Vec::new();
    let mut has_forwards = false;
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let (key, value) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| invalid("line"))?;
        let value = value.trim();
        if value.is_empty() || value.contains('\0') {
            return Err(invalid("value"));
        }
        if [
            "proxycommand",
            "localforward",
            "remoteforward",
            "dynamicforward",
            "certificatefile",
            "pkcs11provider",
            "securitykeyprovider",
            "gssapiauthentication",
            "knownhostscommand",
            "ciphers",
            "kexalgorithms",
            "hostkeyalgorithms",
            "macs",
            "pubkeyacceptedalgorithms",
            "identityagent",
            "userknownhostsfile",
            "bindaddress",
            "match",
            "remotecommand",
            "requesttty",
            "localcommand",
            "permitlocalcommand",
            "setenv",
            "forwardagent",
            "forwardx11",
        ]
        .contains(&key)
        {
            conversion_options
                .entry(key.into())
                .or_default()
                .push(value.into());
        }
        if ["localforward", "remoteforward", "dynamicforward"].contains(&key) {
            has_forwards = true;
        } else if key == "certificatefile" {
            certificate_files.push(value.to_owned());
        } else if key == "identityfile" {
            identity_files.push(value.to_owned());
        } else if [
            "nohostauthenticationforlocalhost",
            "identitiesonly",
            "identityagent",
            "globalknownhostsfile",
            "remotecommand",
            "forwardagent",
            "hostname",
            "port",
            "user",
            "proxyjump",
            "proxycommand",
            "hostkeyalias",
            "stricthostkeychecking",
            "userknownhostsfile",
        ]
        .contains(&key)
            && values.insert(key, value).is_some()
        {
            return Err(invalid(key));
        }
    }
    let required = |key| values.get(key).copied().ok_or_else(|| invalid(key));
    let optional = |key| {
        values
            .get(key)
            .filter(|v| !v.eq_ignore_ascii_case("none"))
            .map(|v| (*v).to_owned())
    };
    let port = required("port")?
        .parse::<u16>()
        .map_err(|_| invalid("port"))?;
    if port == 0 {
        return Err(invalid("port"));
    }
    let checking = required("stricthostkeychecking")?;
    if !["yes", "true", "no", "false", "off", "ask", "accept-new"].contains(&checking) {
        return Err(invalid("stricthostkeychecking"));
    }
    let files = words(required("userknownhostsfile")?)?;
    if files.is_empty() {
        return Err(invalid("userknownhostsfile"));
    }
    let identities_only = match values.get("identitiesonly").copied().unwrap_or("no") {
        "yes" => true,
        "no" => false,
        _ => return Err(invalid("identitiesonly")),
    };
    Ok(ResolvedConfig {
        conversion_options,
        has_forwards,
        has_remote_command: optional("remotecommand").is_some(),
        forward_agent: values
            .get("forwardagent")
            .is_some_and(|v| !matches!(*v, "no" | "false" | "none")),
        no_host_authentication_for_localhost: match values
            .get("nohostauthenticationforlocalhost")
            .copied()
            .unwrap_or("no")
        {
            "yes" => true,
            "no" => false,
            _ => return Err(invalid("nohostauthenticationforlocalhost")),
        },
        identities_only,
        identity_agent: values.get("identityagent").map(|v| (*v).to_owned()),
        certificate_files,
        global_known_hosts_files: values
            .get("globalknownhostsfile")
            .map(|v| words(v))
            .transpose()?
            .unwrap_or_default(),
        hostname: required("hostname")?.to_owned(),
        port,
        user: required("user")?.to_owned(),
        identity_files,
        proxy_jump: optional("proxyjump"),
        proxy_command: optional("proxycommand"),
        host_key_alias: optional("hostkeyalias"),
        strict_host_key_checking: checking.to_owned(),
        user_known_hosts_files: files,
    })
}

/// 根据有效配置判断是否关闭主机密钥校验或丢弃用户主机密钥记录。
pub fn host_key_check_weakened(cfg: &ResolvedConfig) -> bool {
    ["no", "off", "false", "accept-new"]
        .iter()
        .any(|v| cfg.strict_host_key_checking.eq_ignore_ascii_case(v))
        || cfg.no_host_authentication_for_localhost
        || cfg.user_known_hosts_files.iter().any(|p| {
            ["/dev/null", "none", "nul", "nul:", "\\\\.\\nul"]
                .iter()
                .any(|v| p.eq_ignore_ascii_case(v))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    const SAMPLE: &str = include_str!("../../tests/data/ssh-g.txt");
    #[test]
    fn s21_解析转发和补充认证字段() {
        let cfg = parse_ssh_g(SAMPLE).unwrap();
        assert!(!cfg.has_forwards);
        assert!(!cfg.identities_only);
        assert_eq!(cfg.identity_agent, None);
        assert!(cfg.certificate_files.is_empty());
        assert_eq!(cfg.global_known_hosts_files.len(), 2);
        for forward in ["localforward", "remoteforward", "dynamicforward"] {
            let input = format!(
                "{}\n{forward} 1234 localhost:5678\n{forward} 2345 localhost:6789\nidentityagent none\ncertificatefile /a b\ncertificatefile /c d",
                SAMPLE.replace("identitiesonly no", "identitiesonly yes")
            );
            let cfg = parse_ssh_g(&input).unwrap();
            assert!(cfg.has_forwards);
            assert!(cfg.identities_only);
            assert_eq!(cfg.identity_agent.as_deref(), Some("none"));
            assert_eq!(cfg.certificate_files, ["/a b", "/c d"]);
        }
        assert!(
            parse_ssh_g(&SAMPLE.replace("identitiesonly no", "identitiesonly invalid")).is_err()
        );
        assert!(parse_ssh_g(&format!("{SAMPLE}\nidentityagent a\nidentityagent b")).is_err());
    }
    #[test]
    fn 解析真实输出() {
        // 样例来自 ssh -G -F /dev/null -l pierlink example.com，无需网络连接。
        let cfg = parse_ssh_g(SAMPLE).unwrap();
        assert_eq!(cfg.hostname, "example.com");
        assert_eq!(cfg.port, 22);
        assert_eq!(cfg.user, "pierlink");
        assert!(cfg.identity_files.len() > 1);
        assert_eq!(cfg.proxy_jump, None);
        assert!(!host_key_check_weakened(&cfg));
    }
    #[test]
    fn 路径和代理保留空格() {
        let sample = SAMPLE
            .lines()
            .filter(|l| !l.starts_with("userknownhostsfile "))
            .collect::<Vec<_>>()
            .join("\n");
        let cfg = parse_ssh_g(&format!("{sample}\nuserknownhostsfile \"C:\\用户 空格\\hosts\" /dev/null\nproxycommand ssh gateway nc %h %p\nproxyjump none\nhostkeyalias actual\nidentityfile /a b/密钥\n")).unwrap();
        assert_eq!(cfg.user_known_hosts_files[0], "C:\\用户 空格\\hosts");
        assert_eq!(cfg.identity_files.last().unwrap(), "/a b/密钥");
        assert_eq!(cfg.proxy_command.as_deref(), Some("ssh gateway nc %h %p"));
        assert_eq!(cfg.host_key_alias.as_deref(), Some("actual"));
        assert_eq!(cfg.proxy_jump, None);
    }
    #[test]
    fn s12_检测弱化校验() {
        for mode in ["no", "false", "off", "accept-new"] {
            let cfg = parse_ssh_g(&SAMPLE.replace(
                "stricthostkeychecking ask",
                &format!("stricthostkeychecking {mode}"),
            ))
            .unwrap();
            assert!(host_key_check_weakened(&cfg));
        }
        let weak = parse_ssh_g(&SAMPLE.replace(
            "nohostauthenticationforlocalhost no",
            "nohostauthenticationforlocalhost yes",
        ))
        .unwrap();
        assert!(host_key_check_weakened(&weak));
        let cfg = parse_ssh_g(&format!("{SAMPLE}\nproxycommand helper sensitive-value")).unwrap();
        assert!(!format!("{cfg:?}").contains("sensitive-value"));
        let mut cfg = parse_ssh_g(SAMPLE).unwrap();
        for p in ["/dev/null", "NUL", "none", "NUL:", "\\\\.\\NUL"] {
            cfg.user_known_hosts_files = vec![p.into()];
            assert!(host_key_check_weakened(&cfg));
        }
        cfg.user_known_hosts_files = vec!["~/.ssh/known_hosts".into()];
        for mode in ["yes", "ask"] {
            cfg.strict_host_key_checking = mode.into();
            assert!(!host_key_check_weakened(&cfg));
        }
    }
    #[test]
    fn 损坏配置不回显内容() {
        for sample in [
            String::new(),
            "垃圾".into(),
            SAMPLE.replace("port 22", "port 0"),
            SAMPLE.replace("port 22", "port 65536"),
            SAMPLE.replace("port 22", "port secret"),
            format!("{SAMPLE}\nport 22"),
            SAMPLE.replace("user pierlink\n", ""),
            SAMPLE.replace("stricthostkeychecking ask", "stricthostkeychecking secret"),
        ] {
            let e = parse_ssh_g(&sample).unwrap_err();
            assert!(!format!("{e:?}").contains("secret"));
        }
    }
}
