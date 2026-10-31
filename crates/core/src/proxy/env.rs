use super::{ProxyKind, ProxySettings, SystemProxy, active, settings, system_proxy, url};
use std::net::IpAddr;

/// 无参数；按当前全局设置返回本机终端的代理环境变量，大小写各一份。
/// HTTP 代理设 HTTP_PROXY、HTTPS_PROXY、ALL_PROXY；SOCKS5 只设 ALL_PROXY（curl、git 认它）：
/// Claude Code、npm、wget 等只支持 HTTP 代理的程序遇到 socks5h:// 的 HTTP(S)_PROXY 会连不上网，不设时它们照常直连。
/// 关闭代理、系统未设代理或使用 PAC 时返回空；系统设置每次现读。
pub fn terminal_env() -> Vec<(String, String)> {
    build(&settings(), system_proxy)
}

fn build(settings: &ProxySettings, system: impl FnOnce() -> SystemProxy) -> Vec<(String, String)> {
    let Some((server, bypass)) = active(settings, system) else {
        return vec![];
    };
    let proxy = url(&server);
    let mut rules: Vec<String> = ["localhost", "127.0.0.1", "::1"].map(String::from).to_vec();
    for rule in bypass.iter().filter_map(|rule| no_proxy_rule(rule)) {
        if !rules
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&rule))
        {
            rules.push(rule);
        }
    }
    let bypass = rules.join(",");
    let keys: &[&str] = match server.kind {
        ProxyKind::Http => &["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"],
        ProxyKind::Socks5 => &["ALL_PROXY"],
    };
    let mut env = Vec::with_capacity(keys.len() * 2 + 2);
    for (key, value) in keys
        .iter()
        .map(|k| (*k, &proxy))
        .chain([("NO_PROXY", &bypass)])
    {
        env.push((key.into(), value.clone()));
        env.push((key.to_ascii_lowercase(), value.clone()));
    }
    env
}

fn no_proxy_rule(rule: &str) -> Option<String> {
    let rule = rule.trim();
    if let Some((network, prefix)) = rule.split_once('/') {
        let address = network.parse::<IpAddr>().ok()?;
        let bits = prefix.parse::<u8>().ok()?;
        return (bits <= if address.is_ipv4() { 32 } else { 128 }).then(|| rule.into());
    }
    if let Some(domain) = rule.strip_prefix("*.") {
        return hostname(domain).then(|| format!(".{domain}"));
    }
    (rule.parse::<IpAddr>().is_ok() || hostname(rule)).then(|| rule.into())
}

fn hostname(value: &str) -> bool {
    !value.is_empty()
        && value.chars().any(char::is_alphanumeric)
        && value
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::ProxyMode;
    use std::collections::HashMap;

    #[test]
    fn 无代理或系统没有可用代理时环境为空() {
        assert!(build(&ProxySettings::default(), || panic!()).is_empty());
        let settings = ProxySettings {
            mode: ProxyMode::System,
            ..Default::default()
        };
        for system in [
            SystemProxy::None,
            SystemProxy::Pac {
                url: "http://pac/script".into(),
            },
        ] {
            assert!(build(&settings, || system).is_empty());
        }
    }

    #[test]
    fn 两种协议生成大小写变量并正确表示ipv6() {
        // SOCKS5 只设 ALL_PROXY
        for (kind, scheme, keys) in [
            (
                ProxyKind::Http,
                "http",
                &["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"][..],
            ),
            (ProxyKind::Socks5, "socks5h", &["ALL_PROXY"][..]),
        ] {
            for host in ["proxy.example", "192.0.2.1", "2001:db8::1"] {
                let mut server = crate::proxy::tests::server();
                server.kind = kind;
                server.host = host.into();
                let settings = ProxySettings {
                    mode: ProxyMode::Custom(server),
                    bypass: vec![],
                };
                let env = build(&settings, || panic!());
                assert_eq!(env.len(), keys.len() * 2 + 2);
                let env: HashMap<_, _> = env.into_iter().collect();
                let host = if host.contains(':') {
                    format!("[{host}]")
                } else {
                    host.into()
                };
                for key in keys {
                    assert_eq!(env[*key], format!("{scheme}://{host}:8080"));
                    assert_eq!(env[*key], env[&key.to_ascii_lowercase()]);
                }
                if kind == ProxyKind::Socks5 {
                    assert!(!env.contains_key("HTTP_PROXY") && !env.contains_key("https_proxy"));
                }
                assert_eq!(env["NO_PROXY"], "localhost,127.0.0.1,::1");
                assert_eq!(env["NO_PROXY"], env["no_proxy"]);
            }
        }
    }

    #[test]
    fn 名单保留主机后缀cidr并跳过不可表达及无效条目() {
        let settings = ProxySettings {
            mode: ProxyMode::Custom(crate::proxy::tests::server()),
            bypass: [
                " LOCALHOST ",
                "127.0.0.1",
                "::1",
                "exact.example",
                "printer",
                "*.example.com",
                "10.0.0.0/8",
                "fc00::/7",
                "192.0.2.4",
                "2001:db8::1",
                "EXACT.example",
                "10.*",
                "192.168.*",
                "mid.*.example",
                "*.bad.*",
                "?host",
                "<local>",
                "*",
                "",
                "*.",
                "bad/8",
                "10.0.0.0/33",
                "::/129",
                "a,b",
                "a\nb",
            ]
            .map(String::from)
            .to_vec(),
        };
        let env: HashMap<_, _> = build(&settings, || panic!()).into_iter().collect();
        assert_eq!(
            env["NO_PROXY"],
            "localhost,127.0.0.1,::1,exact.example,printer,.example.com,10.0.0.0/8,fc00::/7,192.0.2.4,2001:db8::1"
        );
        assert_eq!(env["NO_PROXY"], env["no_proxy"]);
    }

    #[test]
    fn 跟随系统现读代理及名单且不混入自定义默认值() {
        let settings = ProxySettings {
            mode: ProxyMode::System,
            ..Default::default()
        };
        for host in ["first", "second"] {
            let env: HashMap<_, _> = build(&settings, || {
                super::super::system::parse(
                    true,
                    &format!("{host}:8080"),
                    "*.corp.com;exact.corp;10.0.0.0/8;10.*;<local>",
                    "",
                )
            })
            .into_iter()
            .collect();
            assert_eq!(env["HTTP_PROXY"], format!("http://{host}:8080"));
            assert_eq!(
                env["NO_PROXY"],
                "localhost,127.0.0.1,::1,.corp.com,exact.corp,10.0.0.0/8"
            );
        }
    }
}
