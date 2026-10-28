use super::*;
use crate::{process::SshProgram, server_input::build_alias_server, ssh::config::parse_ssh_g};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

fn write(root: &Path, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, contents).unwrap();
    path
}
fn aliases(list: &HostList) -> Vec<&str> {
    list.hosts.iter().map(|h| h.alias.as_str()).collect()
}

#[test]
fn 词法和跳过项及来源() {
    let root = crate::test_support::tempdir();
    let path = write(
        root.path(),
        "config",
        "\u{feff}# 注释\r\n\thOsT = \"first\" second * !no a? # 注释\r\nMatch all\n User ignored\nHost=first Third\nHost -oProxyCommand=x a%b \"a b\" \"\"\nHost \"未闭合\n",
    );
    let list = list_hosts(&path, root.path()).unwrap();
    assert_eq!(aliases(&list), ["first", "second", "Third"]);
    assert_eq!(list.hosts[0].file, path);
    assert_eq!(list.hosts[0].line, 2);
    assert_eq!(list.hosts[2].line, 5);
    assert_eq!(
        list.skipped
            .iter()
            .filter(|s| s.reason == SkipReason::Wildcard)
            .count(),
        2
    );
    for reason in [
        SkipReason::Negated,
        SkipReason::InMatchBlock,
        SkipReason::Duplicate,
    ] {
        assert!(list.skipped.iter().any(|s| s.reason == reason));
    }
    assert_eq!(
        list.skipped
            .iter()
            .filter(|s| s.reason == SkipReason::InvalidAlias)
            .count(),
        5
    );
    assert!(
        list_hosts(&root.path().join("missing"), root.path())
            .unwrap()
            .hosts
            .is_empty()
    );
}

#[test]
fn include相对路径家目录排序块内展开循环及重复() {
    let root = crate::test_support::tempdir();
    let config = write(
        root.path(),
        ".ssh/config",
        "Host parent\nInclude = parts/*.conf\nMatch all\nInclude \"~/含 空格\" missing\nHost end parent\n",
    );
    write(root.path(), ".ssh/parts/b.conf", "Host B\nInclude config\n");
    let a = write(root.path(), ".ssh/parts/a.conf", "Host A\nInclude extra?\n");
    write(root.path(), ".ssh/extra1", "Host extra\n");
    let spaced = write(
        root.path(),
        "含 空格",
        "Host spaced\nInclude ~/.ssh/parts/a.conf\n",
    );
    let list = list_hosts(&config, root.path()).unwrap();
    assert_eq!(
        aliases(&list),
        ["parent", "A", "extra", "B", "spaced", "end"]
    );
    assert_eq!(list.hosts[1].file, a);
    assert_eq!(list.hosts[4].file, spaced);
    assert_eq!(list.warnings.len(), 1);
    assert_eq!(list.warnings[0].code, "ssh.config_include_cycle");
    assert_eq!(
        list.skipped
            .iter()
            .filter(|s| s.reason == SkipReason::Duplicate)
            .count(),
        3
    );
}

#[test]
fn 大小数量深度限制与有损读取() {
    let root = crate::test_support::tempdir();
    let config = write(root.path(), "config", vec![b' '; 1024 * 1024 + 1]);
    assert_eq!(
        list_hosts(&config, root.path()).unwrap_err().code,
        "ssh.config_too_large"
    );
    fs::write(&config, vec![b' '; 1024 * 1024]).unwrap();
    assert!(list_hosts(&config, root.path()).is_ok());
    fs::write(&config, b"Host valid\n# \xff\nHost other\n").unwrap();
    assert_eq!(
        aliases(&list_hosts(&config, root.path()).unwrap()),
        ["valid", "other"]
    );
    for n in 0..=17 {
        write(
            root.path(),
            &format!(".ssh/{n}"),
            format!("Host alias{n}\nInclude {}\n", n + 1),
        );
    }
    assert_eq!(
        list_hosts(&root.path().join(".ssh/0"), root.path())
            .unwrap_err()
            .code,
        "ssh.config_too_large"
    );
    fs::write(root.path().join(".ssh/16"), "Host last").unwrap();
    assert_eq!(
        list_hosts(&root.path().join(".ssh/0"), root.path())
            .unwrap()
            .hosts
            .len(),
        17
    );
    let hosts = (0..5000)
        .map(|n| format!("Host h{n}\n"))
        .collect::<String>();
    fs::write(&config, &hosts).unwrap();
    assert_eq!(list_hosts(&config, root.path()).unwrap().hosts.len(), 5000);
    fs::write(&config, format!("{hosts}Host excess\n")).unwrap();
    assert_eq!(
        list_hosts(&config, root.path()).unwrap_err().code,
        "ssh.config_too_large"
    );
    for n in 0..64 {
        write(root.path(), &format!(".ssh/files/{n}"), "");
    }
    fs::write(&config, "Include files/*").unwrap();
    assert_eq!(
        list_hosts(&config, root.path()).unwrap_err().code,
        "ssh.config_too_large"
    );
    fs::write(
        &config,
        (0..63)
            .map(|n| format!("Include files/{n}\n"))
            .collect::<String>(),
    )
    .unwrap();
    assert!(list_hosts(&config, root.path()).is_ok());
}

#[cfg(unix)]
#[test]
fn 无权限文件记录警告后继续() {
    use std::os::unix::fs::PermissionsExt;
    let root = crate::test_support::tempdir();
    let file = write(root.path(), ".ssh/denied", "Host hidden");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    let config = write(root.path(), "config", "Include denied\nHost visible");
    let list = list_hosts(&config, root.path()).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(aliases(&list), ["visible"]);
    assert_eq!(list.warnings[0].code, "ssh.config_read_failed");
}

#[test]
fn 别名校验和截断以及链接记录默认值() {
    for alias in [
        "",
        "-oProxyCommand=x",
        "a%b",
        "a b",
        "a\tb",
        "a\0b",
        "*",
        "a?",
        "!a",
        &"a".repeat(256),
    ] {
        assert!(build_alias_server(alias, None, "id", 1).is_err());
    }
    let s = build_alias_server(&"a".repeat(255), Some("  分组  "), "id", 123).unwrap();
    assert_eq!(s.source, crate::storage::ServerSource::SshConfig);
    assert_eq!(s.name, s.ssh_alias.clone().unwrap());
    assert_eq!(s.group_name.as_deref(), Some("分组"));
    assert_eq!(s.auth_method, crate::storage::AuthMethod::Agent);
    assert!(
        s.host.is_none() && s.port.is_none() && s.username.is_none() && s.identity_file.is_none()
    );
    assert!(!s.favorite && !s.use_tmux && !s.remember_password && !s.vnc_autostart);
    assert!(
        s.password_cred.is_none()
            && s.jump_server_id.is_none()
            && s.login_shell.is_none()
            && s.host_key_sha256.is_none()
    );
    assert!(s.notes.is_empty());
    assert_eq!((s.created_at, s.updated_at, s.sort_order), (123, 123, 0));
    assert!(build_alias_server("ok", Some("a\nb"), "id", 0).is_err());
    assert!(build_alias_server("ok", Some(&"a".repeat(65)), "id", 0).is_err());
    assert!(
        build_alias_server("ok", Some("  "), "id", 0)
            .unwrap()
            .group_name
            .is_none()
    );
    let root = crate::test_support::tempdir();
    let file = write(
        root.path(),
        "config",
        format!("Host {}\n", "界".repeat(300)),
    );
    let list = list_hosts(&file, root.path()).unwrap();
    assert_eq!(list.skipped[0].pattern.chars().count(), 64);
}

#[test]
fn 数据库只查询链接别名并拒绝插入和更新重复() {
    let root = crate::test_support::tempdir();
    let db = crate::storage::open(&root.path().join("db")).unwrap();
    assert!(db.list_ssh_aliases().unwrap().is_empty());
    let a = build_alias_server("Host", None, "1", 0).unwrap();
    db.insert_server(&a).unwrap();
    db.update_server(&a).unwrap();
    let mut duplicate = build_alias_server("Host", None, "2", 0).unwrap();
    assert_eq!(
        db.insert_server(&duplicate).unwrap_err().code,
        "storage.alias_exists"
    );
    duplicate.ssh_alias = Some("host".into());
    db.insert_server(&duplicate).unwrap();
    duplicate.ssh_alias = Some("Host".into());
    assert_eq!(
        db.update_server(&duplicate).unwrap_err().code,
        "storage.alias_exists"
    );
    let mut manual = build_alias_server("ignored", None, "3", 0).unwrap();
    manual.source = crate::storage::ServerSource::Manual;
    manual.host = Some("example.com".into());
    db.insert_server(&manual).unwrap();
    assert_eq!(
        db.list_ssh_aliases().unwrap(),
        HashSet::from(["Host".into(), "host".into()])
    );
}

#[test]
fn s22_解析命令与agent风险字段() {
    let sample = include_str!("../../../tests/data/ssh-g.txt");
    let plain = parse_ssh_g(sample).unwrap();
    assert!(!plain.has_remote_command && !plain.forward_agent);
    let modified = sample
        .lines()
        .filter(|l| !l.starts_with("forwardagent "))
        .collect::<Vec<_>>()
        .join("\n");
    for agent in ["yes", "/path/to/agent"] {
        let cfg = parse_ssh_g(&format!(
            "{modified}\nremotecommand echo hello\nforwardagent {agent}\n"
        ))
        .unwrap();
        assert!(cfg.has_remote_command && cfg.forward_agent);
    }
    assert!(
        !parse_ssh_g(&format!("{sample}\nremotecommand none\n"))
            .unwrap()
            .has_remote_command
    );
}

#[cfg(unix)]
fn script(root: &Path, name: &str, body: &str) -> SshProgram {
    use std::os::unix::fs::PermissionsExt;
    let file = write(root, name, format!("#!/bin/sh\n{body}\n"));
    fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).unwrap();
    SshProgram { path: file }
}
#[cfg(unix)]
fn sample_script() -> String {
    format!(
        "cat <<'CONFIG'\n{}\nCONFIG",
        include_str!("../../../tests/data/ssh-g.txt")
    )
}

#[cfg(unix)]
#[test]
fn 解析成功失败超时取消以及秘密保护() {
    let root = crate::test_support::tempdir();
    let ok = script(
        root.path(),
        "ok",
        &format!(
            "[ \"$1\" = -G ] && [ \"$2\" = -- ] && [ \"$3\" = alias ] || exit 2\n{}",
            sample_script()
        ),
    );
    assert_eq!(
        resolve_alias(&ok, "alias", Duration::from_secs(2))
            .unwrap()
            .hostname,
        "example.com"
    );
    assert_eq!(
        resolve_alias(&ok, "-bad", Duration::from_secs(2))
            .unwrap_err()
            .code,
        "ssh.invalid_alias"
    );
    let bad = script(
        root.path(),
        "bad",
        "echo '任意秘密 password=hidden 私钥内容' >&2\nexit 7",
    );
    let error = resolve_alias(&bad, "alias", Duration::from_secs(2)).unwrap_err();
    assert_eq!(error.code, "ssh.resolve_failed");
    assert_eq!(error.params["exit_code"], "7");
    assert!(!format!("{error:?}").contains("hidden"));
    assert!(!format!("{error:?}").contains("任意秘密"));
    let slow = script(root.path(), "slow", "sleep 30");
    let start = Instant::now();
    assert_eq!(
        resolve_alias(&slow, "alias", Duration::from_millis(60))
            .unwrap_err()
            .code,
        "ssh.resolve_timeout"
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    let absent = SshProgram {
        path: root.path().join("absent"),
    };
    assert_eq!(
        resolve_alias(&absent, "alias", Duration::from_secs(1))
            .unwrap_err()
            .code,
        "ssh.resolve_failed"
    );
    let flood = script(root.path(), "flood", "head -c 2000000 /dev/zero");
    assert_eq!(
        resolve_alias(&flood, "alias", Duration::from_secs(2))
            .unwrap_err()
            .code,
        "ssh.resolve_failed"
    );
}

#[cfg(unix)]
#[test]
fn s10_s12_s21_s25_s26_真实解析和预览() {
    if !Path::new("/usr/bin/ssh").exists() {
        return;
    }
    let root = crate::test_support::tempdir();
    let config = write(
        root.path(),
        "含 空格/config",
        "Host linked\n HostName target.example\n User tester\n Port 2222\n IdentityFile \"/含 空格/key\"\n ProxyJump gateway\n LocalForward 18000 localhost:80\n RemoteCommand echo hello\n ForwardAgent yes\n StrictHostKeyChecking no\n UserKnownHostsFile \"/含 空格/known_hosts\"\n NoHostAuthenticationForLocalhost yes\n ControlMaster auto\n UpdateHostKeys yes\n ForkAfterAuthentication yes\n StdinNull yes\n LogLevel QUIET\nHost cmd\n ProxyCommand helper --host %h\n",
    );
    let ssh = script(
        root.path(),
        "ssh-wrapper",
        &format!(
            "exec /usr/bin/ssh -F {} \"$@\"",
            crate::ssh::quote::sh_quote(config.to_str().unwrap()).unwrap()
        ),
    );
    let cfg = resolve_alias(&ssh, "linked", Duration::from_secs(2)).unwrap();
    assert_eq!(
        (&*cfg.hostname, &*cfg.user, cfg.port),
        ("target.example", "tester", 2222)
    );
    assert!(cfg.has_remote_command && cfg.forward_agent && cfg.has_forwards);
    assert_eq!(cfg.identity_files, ["/含 空格/key"]);
    assert_eq!(cfg.proxy_jump.as_deref(), Some("gateway"));
    let list = list_hosts(&config, root.path()).unwrap();
    let candidates = preview(
        &ssh,
        &list,
        &HashSet::from(["linked".into(), "CMD".into()]),
        &AtomicBool::new(false),
    );
    assert!(candidates[0].already_imported && !candidates[1].already_imported);
    assert!(
        candidates[0].via_jump
            && candidates[0].has_forwards
            && candidates[0].host_key_check_weakened
    );
    assert!(candidates[1].via_jump);
    assert!(candidates.iter().all(|c| c.error.is_none()));
}

#[cfg(unix)]
#[test]
fn 预览并发上限独立失败和中途取消() {
    let root = crate::test_support::tempdir();
    let config = write(
        root.path(),
        "config",
        (0..12).map(|n| format!("Host h{n}\n")).collect::<String>(),
    );
    let list = list_hosts(&config, root.path()).unwrap();
    // 每次启动写时间戳标记，第一波阻塞直到父测试放行，从而直接观测并发上限。
    let dir = crate::ssh::quote::sh_quote(root.path().to_str().unwrap()).unwrap();
    let body = format!(
        "cd {dir}\ntouch \"started-$3\"\nwhile [ ! -f release ]; do sleep 0.02; done\n[ \"$3\" = h1 ] && exit 1\n{}",
        sample_script()
    );
    let ssh = script(root.path(), "parallel", &body);
    std::thread::scope(|scope| {
        let job = scope.spawn(|| preview(&ssh, &list, &HashSet::new(), &AtomicBool::new(false)));
        let start = Instant::now();
        loop {
            let count = fs::read_dir(root.path())
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("started-")
                })
                .count();
            if count == 4 {
                break;
            }
            assert!(count <= 4);
            assert!(start.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            fs::read_dir(root.path())
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("started-"))
                .count(),
            4
        );
        write(root.path(), "release", "");
        let result = job.join().unwrap();
        assert_eq!(result.iter().filter(|c| c.error.is_some()).count(), 1);
        assert_eq!(result[1].error.as_ref().unwrap().code, "ssh.resolve_failed");
        assert_eq!(
            result.iter().map(|c| c.alias.as_str()).collect::<Vec<_>>(),
            aliases(&list)
        );
    });
    let slow = script(root.path(), "slow", "sleep 30");
    let cancel = AtomicBool::new(false);
    let start = Instant::now();
    std::thread::scope(|scope| {
        let job = scope.spawn(|| preview(&slow, &list, &HashSet::new(), &cancel));
        std::thread::sleep(Duration::from_millis(80));
        cancel.store(true, Ordering::Release);
        assert!(
            job.join()
                .unwrap()
                .iter()
                .all(|c| c.error.as_ref().unwrap().code == "ssh.resolve_cancelled")
        );
    });
    assert!(start.elapsed() < Duration::from_secs(2));
    let absent = SshProgram {
        path: root.path().join("does-not-exist"),
    };
    assert!(
        preview(&absent, &list, &HashSet::new(), &cancel)
            .iter()
            .all(|c| c.error.as_ref().unwrap().code == "ssh.resolve_cancelled")
    );
}

#[test]
fn include通配不隐式匹配隐藏文件且支持多参数() {
    let root = crate::test_support::tempdir();
    let config = write(
        root.path(),
        "config",
        "Include parts/* parts/.* other\nHost After after\n",
    );
    write(root.path(), ".ssh/parts/.hidden", "Host hidden\n");
    write(root.path(), ".ssh/parts/z", "Host visible\n");
    write(root.path(), ".ssh/other", "Host other\n");
    let list = list_hosts(&config, root.path()).unwrap();
    assert_eq!(
        aliases(&list),
        ["visible", "hidden", "other", "After", "after"]
    );
    assert!(list.skipped.is_empty());
}

#[test]
fn 并发数据库连接不能重复导入() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("db");
    let first = crate::storage::open(&path).unwrap();
    let second = crate::storage::open(&path).unwrap();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let barrier = &barrier;
        let a = scope.spawn(move || {
            barrier.wait();
            first.insert_server(&build_alias_server("alias", None, "1", 0).unwrap())
        });
        let b = scope.spawn(move || {
            barrier.wait();
            second.insert_server(&build_alias_server("alias", None, "2", 0).unwrap())
        });
        let results = [a.join().unwrap(), b.join().unwrap()];
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results.iter().find_map(|r| r.as_ref().err()).unwrap().code,
            "storage.alias_exists"
        );
    });
    assert_eq!(
        crate::storage::open(&path)
            .unwrap()
            .list_ssh_aliases()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn s24_导入保留链接而手动目标继续隔离配置() {
    use crate::ssh::{
        args::{CommonOpts, Purpose, build_args},
        target::SshTarget,
    };
    let linked = build_alias_server("linked", None, "id", 0).unwrap();
    let alias = SshTarget::Alias {
        alias: linked.ssh_alias.unwrap(),
    };
    let args = build_args(&alias, &Purpose::Background, &CommonOpts::default()).unwrap();
    assert!(!args.args.contains(&"-F".into()));
    let manual = SshTarget::Manual {
        host: "host".into(),
        port: 22,
        user: None,
        identity_file: None,
        jump: None,
    };
    let args = build_args(&manual, &Purpose::Background, &CommonOpts::default()).unwrap();
    assert!(args.args.windows(2).any(|p| p == ["-F", "none"]));
}
