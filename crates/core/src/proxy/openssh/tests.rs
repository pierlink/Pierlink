use super::*;
#[cfg(unix)]
use crate::proxy::set_settings;
use crate::proxy::{ProxyMode, ProxySettings};

#[test]
fn 系统未设代理或使用pac时原样返回且不启动ssh() {
    let root = crate::test_support::tempdir();
    // 不存在的可执行文件保证误走 ssh -G 时测试失败。
    let program = SshProgram {
        path: root.path().join("不存在的ssh"),
    };
    let command = SshCommand {
        args: vec!["-F".into(), "existing.conf".into(), "alias".into()],
        env: vec![("CUSTOM".into(), "value".into())],
        config_text: Some("Host alias\n HostName unresolved.invalid\n".into()),
        hooked: true,
        builtin: None,
        askpass: None,
    };
    let settings = ProxySettings {
        mode: ProxyMode::System,
        ..Default::default()
    };
    for proxy in [
        super::super::SystemProxy::None,
        super::super::SystemProxy::Pac {
            url: "http://pac/script".into(),
        },
    ] {
        let mut reads = 0;
        let prepared =
            prepare_with_system(&program, &command, Path::new("/helper"), &settings, || {
                reads += 1;
                proxy
            })
            .unwrap();
        assert_eq!(reads, 1);
        assert_eq!(prepared.command.args, command.args);
        assert_eq!(prepared.command.env, command.env);
        assert_eq!(prepared.command.config_text, command.config_text);
        assert_eq!(prepared.command.hooked, command.hooked);
        assert!(prepared.config.is_none());
    }
    let prepared = prepare_with_system(
        &program,
        &command,
        Path::new("/helper"),
        &ProxySettings::default(),
        || panic!("关闭代理时不读取系统设置"),
    )
    .unwrap();
    assert_eq!(prepared.command.args, command.args);
    assert!(prepared.config.is_none());
}

#[cfg(unix)]
#[test]
fn 系统静态代理按解析后的主机和系统名单决定注入() {
    let program = SshProgram {
        path: "/usr/bin/ssh".into(),
    };
    let command = SshCommand {
        args: vec!["-F".into(), "unused.conf".into(), "alias".into()],
        env: vec![],
        config_text: Some("Host alias\n HostName public.invalid\n".into()),
        hooked: false,
        builtin: None,
        askpass: None,
    };
    let settings = ProxySettings {
        mode: ProxyMode::System,
        bypass: vec!["public.invalid".into()],
    };
    for bypass in [vec![], vec!["public.invalid".into()]] {
        let expected = bypass.is_empty();
        let mut reads = 0;
        let prepared =
            prepare_with_system(&program, &command, Path::new("/helper"), &settings, || {
                reads += 1;
                super::super::SystemProxy::Server {
                    server: crate::proxy::tests::server(),
                    bypass,
                }
            })
            .unwrap();
        assert_eq!(reads, 1);
        assert_eq!(
            prepared
                .command
                .args
                .iter()
                .any(|a| a.starts_with("ProxyCommand=")),
            expected
        );
        assert_eq!(
            prepared
                .command
                .env
                .iter()
                .any(|(key, _)| key == "PIERLINK_PROXY"),
            expected
        );
    }
}

#[test]
fn 帮手路径兼容空格中文百分号且拒绝换行() {
    assert_eq!(
        helper_for_platform(Path::new(r"C:\程序 文件\Pierlink.exe"), true).unwrap(),
        r#""C:\程序 文件\Pierlink.exe" --proxy-connect %h %p"#
    );
    assert_eq!(
        helper_for_platform(Path::new("/程序 空格/a'b%/app"), false).unwrap(),
        "'/程序 空格/a'\"'\"'b%%/app' --proxy-connect %h %p"
    );
    for path in ["", "bad\npath", "bad\0path"] {
        assert!(helper(Path::new(path)).is_err());
    }
    assert!(helper_for_platform(Path::new("bad\"path"), true).is_err());
}

#[cfg(unix)]
#[test]
fn 有效用户配置优先并且手动跳板仅第一跳注入() {
    if crate::proxy::tests::isolated(
        "proxy::openssh::tests::有效用户配置优先并且手动跳板仅第一跳注入",
    ) {
        return;
    }
    use crate::ssh::{
        args::{CommonOpts, Purpose, build_args},
        target::SshTarget,
    };
    set_settings(ProxySettings {
        mode: ProxyMode::Custom(crate::proxy::tests::server()),
        bypass: vec![],
    });
    let program = SshProgram {
        path: "/usr/bin/ssh".into(),
    };
    let root = crate::test_support::tempdir();
    let config = root.path().join("user.conf");
    for routing in ["ProxyCommand /bin/false", "ProxyJump jump.example", ""] {
        std::fs::write(
            &config,
            format!("Host alias\n HostName unresolved.invalid\n {routing}\n"),
        )
        .unwrap();
        let mut command = build_args(
            &SshTarget::Alias {
                alias: "alias".into(),
            },
            &Purpose::Background,
            &CommonOpts::default(),
        )
        .unwrap();
        command
            .args
            .splice(0..0, ["-F".into(), config.to_str().unwrap().into()]);
        let prepared =
            prepare_with_exe(&program, &command, Path::new("/程序 空格/Pierlink")).unwrap();
        assert_eq!(
            prepared
                .command
                .args
                .iter()
                .any(|s| s.starts_with("ProxyCommand=")),
            routing.is_empty()
        );
        assert_eq!(
            prepared
                .command
                .env
                .iter()
                .any(|(k, _)| k == "PIERLINK_PROXY"),
            routing.is_empty()
        );
        assert!(!format!("{:?}", prepared.command).contains("password"));
    }
    let node = |host: &str, jump| SshTarget::Manual {
        host: host.into(),
        port: 22,
        user: Some("user".into()),
        identity_file: None,
        jump,
    };
    let target = node(
        "target.invalid",
        Some(Box::new(node(
            "middle.invalid",
            Some(Box::new(node("first.invalid", None))),
        ))),
    );
    let options = CommonOpts {
        config_file: Some(root.path().join("manual.conf")),
        known_hosts: Some(root.path().join("hosts")),
        ..Default::default()
    };
    let command = build_args(&target, &Purpose::Background, &options).unwrap();
    let prepared = prepare_with_exe(&program, &command, Path::new("/程序 空格/Pierlink")).unwrap();
    let path = prepared.config.as_ref().unwrap().path().join("ssh.conf");
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.matches("ProxyCommand").count(), 1);
    assert!(text.starts_with("Host pierlink-0\n    ProxyCommand "));
    assert_eq!(text.matches("ProxyJump").count(), 2);
    let cfg = crate::ssh::config_file::resolve_command(
        &program,
        &[
            "-F".into(),
            path.to_str().unwrap().into(),
            "pierlink-0".into(),
        ],
        Duration::from_secs(3),
    )
    .unwrap();
    assert!(cfg.proxy_command.unwrap().contains("/程序 空格/Pierlink"));
    drop(prepared);
    assert!(!path.exists());
    set_settings(ProxySettings {
        mode: ProxyMode::Custom(crate::proxy::tests::server()),
        bypass: vec!["first.invalid".into()],
    });
    let direct = prepare_with_exe(&program, &command, Path::new("/unused")).unwrap();
    assert!(direct.config.is_none());
    assert!(
        !direct
            .command
            .env
            .iter()
            .any(|(k, _)| k == "PIERLINK_PROXY")
    );
}
