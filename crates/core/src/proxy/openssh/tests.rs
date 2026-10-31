use super::*;
use crate::proxy::{ProxyMode, ProxySettings, set_settings};

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
