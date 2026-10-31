use super::*;

pub(super) fn server() -> ProxyServer {
    ProxyServer {
        kind: ProxyKind::Http,
        host: "proxy.example".into(),
        port: 8080,
    }
}

#[test]
fn 名单覆盖通配cidr本地主机和强制回环且不解析域名() {
    let mut settings = ProxySettings {
        mode: ProxyMode::Custom(server()),
        ..Default::default()
    };
    for host in [
        "localhost",
        "LOCALHOST.",
        "127.42.0.1",
        "::1",
        "[::1]",
        "::ffff:127.0.0.2",
        "10.1.2.3",
        "172.20.0.1",
        "192.168.1.1",
        "100.64.1.2",
        "fd12::1",
        "fe80::12",
        "printer.local",
        "server",
    ] {
        assert!(select(&settings, host, || panic!()).is_none(), "{host}");
    }
    for host in [
        "public.invalid",
        "172.32.1.1",
        "100.128.1.1",
        "2001:db8::1",
        "printer.local.net",
    ] {
        assert!(select(&settings, host, || panic!()).is_some(), "{host}");
    }
    settings.bypass = [
        "*.CORP.com",
        "10.*",
        "192.168.*",
        "exact.example",
        "<local>",
        "2001:db8::/32",
        "bad/999",
    ]
    .map(String::from)
    .to_vec();
    for host in [
        "dev.corp.com",
        "10.4.5.6",
        "192.168.2.2",
        "EXACT.example.",
        "local",
        "2001:db8::4",
        "127.0.0.2",
    ] {
        assert!(select(&settings, host, || panic!()).is_none(), "{host}");
    }
    for host in [
        "corp.com",
        "exact.example.org",
        "2001:db9::1",
        "10.example.org.invalid",
    ] {
        // WinINET 的 10.* 同样匹配域名，不对名字做 DNS 解析。
        assert_eq!(
            select(&settings, host, || panic!()).is_some(),
            !host.starts_with("10.")
        );
    }
    settings.bypass.clear();
    assert!(select(&settings, "10.1.2.3", || panic!()).is_some());
    assert!(select(&settings, "127.2.3.4", || panic!()).is_none());
}

#[test]
fn 公开回环判断识别映射地址且不解析普通域名() {
    for host in [
        "localhost",
        "LOCALHOST.",
        "127.42.0.1",
        "::1",
        "[::1]",
        "::ffff:127.0.0.1",
        "[::ffff:7f00:2]",
    ] {
        assert!(crate::proxy::loopback(host), "{host}");
    }
    for host in [
        "",
        "localhost.example",
        "localhost:80",
        "127.0.0.1.example",
        "10.0.0.1",
        "::ffff:192.0.2.1",
        "2001:db8::1",
        "host.invalid",
    ] {
        assert!(!crate::proxy::loopback(host), "{host}");
    }
}

#[test]
fn 系统代理字符串选择优先协议及pac并采用系统名单() {
    for (raw, expected, kind, port) in [
        ("proxy:80", "proxy", ProxyKind::Http, 80),
        ("http://127.0.0.1:7890", "127.0.0.1", ProxyKind::Http, 7890),
        ("HTTP://127.0.0.1:7890/", "127.0.0.1", ProxyKind::Http, 7890),
        ("http=http://h:1;https=http://h:2", "h", ProxyKind::Http, 2),
        ("http=HtTp://h:1/", "h", ProxyKind::Http, 1),
        ("socks=socks5://h:1080", "h", ProxyKind::Socks5, 1080),
        ("socks=SOCKS://h:1080/", "h", ProxyKind::Socks5, 1080),
        ("socks=SoCkS5://h:1080/", "h", ProxyKind::Socks5, 1080),
        (
            "socks=SoCkS5h://[::1]:1080/",
            "::1",
            ProxyKind::Socks5,
            1080,
        ),
        (
            "http=plain:81;https=secure:82;socks=socks:83",
            "secure",
            ProxyKind::Http,
            82,
        ),
        ("socks=socks:83;http=plain:81", "plain", ProxyKind::Http, 81),
        (
            " socks=[2001:db8::1]:1080 ",
            "2001:db8::1",
            ProxyKind::Socks5,
            1080,
        ),
    ] {
        let SystemProxy::Server { server, bypass } =
            system::parse(true, raw, " *.corp.com ; <local>;;10.* ", "")
        else {
            panic!()
        };
        assert_eq!(
            (server.host.as_str(), server.kind, server.port),
            (expected, kind, port)
        );
        assert_eq!(bypass, ["*.corp.com", "<local>", "10.*"]);
    }
    assert!(matches!(
        system::parse(false, "host:80", "", ""),
        SystemProxy::None
    ));
    for value in [
        "host",
        "host:0",
        "host:bad",
        "user:secret@host:80",
        "https=bad;http=good:80",
        "https://h:1",
        "HTTPS://h:1/",
        "https=https://h:1;http=http://h:2",
        "http=socks5://h:1080",
        "socks=http://h:80",
        "socks=https://h:1",
        "http://h:80/path",
        "http://user:secret@h:80/",
    ] {
        assert!(matches!(
            system::parse(true, value, "", ""),
            SystemProxy::None
        ));
    }
    for enabled in [true, false] {
        assert!(matches!(
            system::parse(enabled, "host:80", "", "http://pac/script"),
            SystemProxy::Pac { .. }
        ));
    }
    let settings = ProxySettings {
        mode: ProxyMode::System,
        ..Default::default()
    };
    assert!(
        select(&settings, "10.1.2.3", || system::parse(
            true, "host:80", "", ""
        ))
        .is_some()
    );
    assert!(
        select(&settings, "10.1.2.3", || system::parse(
            true, "host:80", "10.*", ""
        ))
        .is_none()
    );
    assert!(
        select(&settings, "example.com", || system::parse(
            true,
            "host:80",
            "",
            "http://pac"
        ))
        .is_none()
    );
    #[cfg(not(windows))]
    assert!(matches!(system_proxy(), SystemProxy::None));
}

#[test]
fn 代理url和帮手环境只含协议主机端口() {
    for (kind, scheme) in [(ProxyKind::Http, "http"), (ProxyKind::Socks5, "socks5h")] {
        let mut proxy = server();
        proxy.kind = kind;
        proxy.host = "2001:db8::1".into();
        let value = format!("{scheme}://[2001:db8::1]:8080");
        assert_eq!(url(&proxy), value);
        assert_eq!(
            environment(&proxy),
            [("PIERLINK_PROXY".into(), value.clone())]
        );
        let decoded = stdio::from_env_values(Some(&value)).unwrap().unwrap();
        assert_eq!(decoded.kind, kind);
        assert_eq!(decoded.host, proxy.host);
        assert_eq!(decoded.port, proxy.port);
        assert_eq!(
            serde_json::to_string(&kind).unwrap(),
            if kind == ProxyKind::Http {
                "\"http\""
            } else {
                "\"socks5\""
            }
        );
    }
    assert!(stdio::from_env_values(None).unwrap().is_none());
    for value in [
        "bad",
        "http://user:secret@host:80",
        "http://secret@host:80",
        "http://@host:80",
        "socks5h://user:secret@host:1080",
        "http://host:80/path",
        "http://host:80?secret",
        "http://host:80#secret",
        "ftp://host:80",
        "socks5h://host",
        "http://bad%00@host:80",
    ] {
        let error = stdio::from_env_values(Some(value)).unwrap_err();
        assert_eq!(error.code, "proxy.invalid_argument");
        assert!(!format!("{error:?}").contains("secret"));
    }
    assert_eq!(url(&server()), "http://proxy.example:8080");
    assert_eq!(environment(&server()).len(), 1);
}

#[test]
fn 代理错误带中文原因及必要地址且不混为ssh认证失败() {
    for (error, code) in [
        (ProxyError::Io, "proxy.io_failed"),
        (ProxyError::InvalidInput, "proxy.invalid_argument"),
        (
            ProxyError::Unreachable {
                proxy: "host:8080".into(),
            },
            "proxy.unreachable",
        ),
        (ProxyError::AuthRequired, "proxy.auth_required"),
        (
            ProxyError::Refused {
                target: "target:22".into(),
                reason: RefuseReason::SocksReply(5),
            },
            "proxy.refused",
        ),
        (ProxyError::Timeout, "proxy.timeout"),
    ] {
        let mapped = map_error(&error);
        assert_eq!(mapped.code, code);
        assert!(
            mapped.params["reason"]
                .chars()
                .any(|c| ('\u{3400}'..='\u{9fff}').contains(&c))
        );
        assert_eq!(
            crate::builtin::error::map_engine_error(
                &pierlink_ssh_engine::Error::Proxy(error.clone()),
                crate::builtin::error::Operation::Other
            )
            .code,
            code
        );
        if matches!(error, ProxyError::AuthRequired) {
            assert_eq!(
                mapped.params["reason"],
                "代理要求用户名和密码，Pierlink 不支持带密码的代理"
            );
        }
        if matches!(error, ProxyError::Unreachable { .. }) {
            assert_eq!(mapped.params["proxy"], "host:8080");
        }
        if matches!(error, ProxyError::Refused { .. }) {
            assert_eq!(mapped.params["target"], "target:22");
        }
    }
}

#[test]
fn 拒绝原因映射保留中文翻译键及数字插值() {
    use RefuseReason::*;
    for reason in [
        ConnectionUnavailable,
        ConnectionSetupFailed,
        RequestWriteFailed,
        ResponseIncomplete,
        ResponseHeaderTooLarge,
        InvalidHttpStatusLine,
        InvalidHttpStatus,
        HttpStatus(403),
        SocksHandshakeInterrupted,
        InvalidSocksVersion,
        InvalidSocksAuthMethod,
        InvalidSocksResponse,
        SocksReply(2),
        InvalidSocksAddressType,
    ] {
        let error = ProxyError::Refused {
            target: "host:22".into(),
            reason,
        };
        let mapped = map_error(&error);
        assert_eq!(mapped.code, "proxy.refused");
        assert_eq!(mapped.params["target"], "host:22");
        assert_eq!(mapped.params["reason"], reason.to_string());
        assert_eq!(mapped.params["reason_key"], reason.key());
        let numeric = match reason {
            HttpStatus(status) => Some(("status", status.to_string())),
            SocksReply(reply) => Some(("reply", reply.to_string())),
            _ => None,
        };
        assert_eq!(mapped.params.len(), if numeric.is_some() { 4 } else { 3 });
        if let Some((key, value)) = numeric {
            assert_eq!(mapped.params[key], value);
        }
        let builtin = crate::builtin::error::map_engine_error(
            &pierlink_ssh_engine::Error::Proxy(error),
            crate::builtin::error::Operation::Other,
        );
        assert_eq!(builtin.params, mapped.params);
    }
}

pub(super) fn isolated(name: &str) -> bool {
    if std::env::var_os("PIERLINK_PROXY_TEST_PROCESS").is_some() {
        return false;
    }
    // 子进程的临时根仍放在本次测试根下面，强制结束时最多残留这一棵目录。
    let root = crate::test_support::tempdir();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--include-ignored"])
        .env("PIERLINK_PROXY_TEST_PROCESS", "1")
        .env("PIERLINK_TEST_TMP", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[test]
fn 设置副本与新连接使用当前模式() {
    if isolated("proxy::tests::设置副本与新连接使用当前模式") {
        return;
    }
    set_settings(ProxySettings {
        mode: ProxyMode::Custom(server()),
        bypass: vec![],
    });
    assert!(matches!(settings().mode, ProxyMode::Custom(_)));
    let env: std::collections::HashMap<_, _> = terminal_env().into_iter().collect();
    assert_eq!(env["HTTP_PROXY"], "http://proxy.example:8080");
    assert_eq!(env["NO_PROXY"], "localhost,127.0.0.1,::1");
    assert!(for_target("unresolved.invalid").is_some());
    assert!(for_target("localhost").is_none());
    let mut copy = settings();
    copy.mode = ProxyMode::Off;
    assert!(for_target("unresolved.invalid").is_some());
    set_settings(copy);
    assert!(for_target("unresolved.invalid").is_none());
    assert!(terminal_env().is_empty());
}

#[cfg(unix)]
mod real;
