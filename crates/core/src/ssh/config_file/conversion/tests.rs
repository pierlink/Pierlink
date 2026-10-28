use super::*;
use std::collections::HashMap;
const BASE: &str = include_str!("../../../../tests/data/ssh-g.txt");
fn configured(changes: &[(&str, &str)]) -> String {
    let mut output = BASE
        .lines()
        .filter(|line| {
            !changes
                .iter()
                .any(|(key, _)| line.starts_with(&format!("{key} ")))
        })
        .collect::<Vec<_>>()
        .join("\n");
    for (key, value) in changes {
        output.push_str(&format!("\n{key} {value}"));
    }
    output
}
struct Fixture {
    outputs: HashMap<String, String>,
}
impl Resolve for Fixture {
    fn get(
        &mut self,
        spec: &JumpSpec,
        defaults: bool,
    ) -> Result<(ResolvedConfig, bool, String), AppError> {
        let raw = if defaults {
            BASE.into()
        } else {
            self.outputs
                .get(&spec.alias)
                .cloned()
                .unwrap_or_else(|| BASE.into())
        };
        let mut cfg = parse_ssh_g(&raw)?;
        if let Some(user) = &spec.user {
            cfg.user = user.clone();
        }
        if let Some(port) = spec.port {
            cfg.port = port;
        }
        Ok((cfg, false, raw))
    }
}
fn record(alias: &str) -> Server {
    crate::server_input::build_alias_server(alias, None, alias, 1).unwrap()
}
fn preview(raw: &str) -> ConversionPreview {
    let server = record("target");
    preview_with(
        &server,
        std::slice::from_ref(&server),
        &mut Fixture {
            outputs: HashMap::from([("target".into(), raw.into())]),
        },
    )
    .unwrap()
}

macro_rules! blocked {
    ($name:ident, $key:literal, $value:literal, $option:literal) => {
        #[test]
        fn $name() {
            let p = preview(&configured(&[($key, $value)]));
            assert!(
                p.reasons
                    .iter()
                    .any(|r| r.option == $option && r.hop == "target")
            );
            assert!(
                !serde_json::to_string(&p.reasons)
                    .unwrap()
                    .contains("敏感值")
            );
        }
    };
}
blocked!(
    拒绝代理命令,
    "proxycommand",
    "helper 敏感值",
    "ProxyCommand"
);
blocked!(
    拒绝本地转发,
    "localforward",
    "1000 localhost:80",
    "LocalForward"
);
blocked!(
    拒绝远端转发,
    "remoteforward",
    "1000 localhost:80",
    "RemoteForward"
);
blocked!(拒绝动态转发, "dynamicforward", "1080", "DynamicForward");
blocked!(拒绝证书, "certificatefile", "敏感值", "CertificateFile");
blocked!(拒绝智能卡, "pkcs11provider", "敏感值", "PKCS11Provider");
blocked!(
    拒绝自定义安全密钥提供器,
    "securitykeyprovider",
    "敏感值",
    "SecurityKeyProvider"
);
blocked!(
    拒绝票据认证,
    "gssapiauthentication",
    "yes",
    "GSSAPIAuthentication"
);
blocked!(
    拒绝命令匹配,
    "match",
    "host target exec 敏感值",
    "Match exec"
);
blocked!(
    拒绝主机密钥查询命令,
    "knownhostscommand",
    "敏感值",
    "KnownHostsCommand"
);
blocked!(拒绝加密算法修改, "ciphers", "aes128-ctr", "Ciphers");
blocked!(
    拒绝密钥交换算法修改,
    "kexalgorithms",
    "curve25519-sha256",
    "KexAlgorithms"
);
blocked!(
    拒绝主机密钥算法修改,
    "hostkeyalgorithms",
    "ssh-ed25519",
    "HostKeyAlgorithms"
);
blocked!(拒绝认证码算法修改, "macs", "hmac-sha2-256", "MACs");
blocked!(
    拒绝公钥算法修改,
    "pubkeyacceptedalgorithms",
    "ssh-ed25519",
    "PubkeyAcceptedAlgorithms"
);
blocked!(拒绝自定义代理, "identityagent", "敏感值", "IdentityAgent");
blocked!(
    拒绝自定义主机密钥文件,
    "userknownhostsfile",
    "C:/用户 空格/敏感值",
    "UserKnownHostsFile"
);
blocked!(拒绝绑定地址, "bindaddress", "127.0.0.2", "BindAddress");

macro_rules! unsupported {
    ($name:ident, $option:literal, $(($key:literal, $value:literal)),+) => {
        #[test]
        fn $name() {
            let p = preview(&configured(&[$(($key, $value)),+]));
            assert!(p.reasons.iter().any(|r| r.code == "ssh.conversion_unsupported_option" && r.option == $option && r.hop == "target"));
            assert!(!serde_json::to_string(&p.reasons).unwrap().contains("敏感值"));
        }
    };
}
unsupported!(
    拒绝远程命令,
    "RemoteCommand",
    ("remotecommand", "echo 敏感值")
);
unsupported!(拒绝强制终端分配, "RequestTTY", ("requesttty", "force"));
unsupported!(
    拒绝允许执行的本地命令,
    "LocalCommand",
    ("permitlocalcommand", "yes"),
    ("localcommand", "echo 敏感值")
);
unsupported!(拒绝设置环境变量, "SetEnv", ("setenv", "TOKEN=敏感值"));
unsupported!(拒绝代理转发, "ForwardAgent", ("forwardagent", "yes"));
unsupported!(拒绝图形转发, "ForwardX11", ("forwardx11", "yes"));

#[test]
fn 发送环境变量不阻止转换() {
    assert!(
        preview(&configured(&[("sendenv", "LANG LC_*")]))
            .reasons
            .is_empty()
    );
}

#[test]
fn 默认终端选项及未启用的本地命令不阻止转换() {
    assert!(
        preview(&configured(&[
            ("remotecommand", "none"),
            ("requesttty", "auto"),
            ("permitlocalcommand", "no"),
            ("localcommand", "echo 不执行"),
            ("forwardagent", "no"),
            ("forwardx11", "no"),
        ]))
        .reasons
        .is_empty()
    );
    for value in ["yes", "no", "force"] {
        assert!(
            preview(&configured(&[("requesttty", value)]))
                .reasons
                .iter()
                .any(|r| r.option == "RequestTTY")
        );
    }
}

#[test]
fn 主机密钥别名含模式符号或空白时拒绝转换() {
    for character in ",!*?|#@ \t\u{3000}".chars() {
        let alias = format!("first{character}second");
        let p = preview(&configured(&[("hostkeyalias", &alias)]));
        assert!(
            p.reasons.iter().any(
                |r| r.code == "ssh.conversion_unsupported_option" && r.option == "HostKeyAlias"
            ),
            "未拒绝 {alias:?}"
        );
    }
}

#[test]
fn 默认字段与多密钥及令牌路径完整保留() {
    assert!(preview(BASE).reasons.is_empty());
    let p = preview(&configured(&[
        ("hostname", "real.test"),
        ("port", "2200"),
        ("user", "alice"),
        ("identityfile", "~/.ssh/%n-%h-%r-%p-%%"),
        ("hostkeyalias", "known-name"),
        ("identitiesonly", "yes"),
    ]));
    assert!(p.reasons.is_empty());
    assert_eq!(
        (
            p.target.host.as_str(),
            p.target.port,
            p.target.username.as_str()
        ),
        ("real.test", 2200, "alice")
    );
    assert!(p.target.identity_files[0].ends_with("target-real.test-alice-2200-%"));
    assert_eq!(p.target.host_key_alias.as_deref(), Some("known-name"));
    assert!(p.target.identities_only);
    // 绝对路径的写法按平台：Windows 上 /first key 不算绝对路径（没有盘符）
    let (first, second) = if cfg!(windows) {
        ("C:\\first key", "C:\\second key")
    } else {
        ("/first key", "/second key")
    };
    let value = format!("{first}\nidentityfile {second}");
    let multiple = preview(&configured(&[("identityfile", value.as_str())]));
    assert_eq!(multiple.target.identity_files, [first, second]);
    let unresolved = preview(&configured(&[("identityfile", "~/.ssh/%C")]));
    assert_eq!(unresolved.reasons[0].option, "IdentityFile");
}

#[test]
fn 诊断覆盖执行与跳过的匹配且不误判普通主机名() {
    for text in [
        "debug2: checking match for 'exec \"true\"' host target originally target",
        "debug2: checking match for 'host other exec \"true\"' host target originally target",
        "debug2: checking match for '!exec \"true\"' host target originally target",
    ] {
        assert!(match_exec(text));
    }
    assert!(!match_exec(
        "debug2: checking match for 'host execution' host target originally target"
    ));
}

#[test]
fn 多级跳板新建和复用在同一事务中保存() {
    let root = crate::test_support::tempdir();
    let mut db = crate::storage::open(&root.path().join("db")).unwrap();
    let target = record("target");
    let mut middle = record("middle");
    middle.notes = "保留分组收藏说明".into();
    middle.favorite = true;
    db.insert_server(&target).unwrap();
    db.insert_server(&middle).unwrap();
    let mut f = Fixture {
        outputs: HashMap::from([
            (
                "target".into(),
                configured(&[
                    ("proxyjump", "outer,middle"),
                    ("hostkeyalias", "target-known"),
                ]),
            ),
            (
                "middle".into(),
                configured(&[
                    ("hostname", "middle.test"),
                    ("identityfile", "/middle key"),
                    ("hostkeyalias", "middle-known"),
                ]),
            ),
            ("outer".into(), configured(&[("hostname", "outer.test")])),
        ]),
    };
    let records = db.list_servers().unwrap();
    let p = preview_with(&target, &records, &mut f).unwrap();
    // 带跳板的服务器暂不转换（连接前的指纹确认还不能按跳板走）：只有这一条原因，跳板照样列出
    assert_eq!(p.reasons.len(), 1);
    assert_eq!(
        (
            p.reasons[0].code.as_str(),
            p.reasons[0].option.as_str(),
            p.reasons[0].hop.as_str()
        ),
        ("ssh.conversion_unsupported_option", "ProxyJump", "target")
    );
    assert_eq!(
        p.jumps
            .iter()
            .map(|j| j.fields.alias.as_str())
            .collect::<Vec<_>>(),
        ["outer", "middle"]
    );
    assert_eq!(p.jumps[0].existing_server_id, None);
    assert_eq!(p.jumps[1].existing_server_id.as_deref(), Some("middle"));
    assert!(!p.jumps[0].converts_existing);
    assert!(p.jumps[1].converts_existing);
    assert_eq!(
        serde_json::to_value(&p.jumps[1]).unwrap()["converts_existing"],
        true
    );
    assert_eq!(db.list_servers().unwrap(), records);
    let result = apply(&mut db, &target, &records, &p, 2).unwrap();
    assert_eq!(result.jumps[0].name, "outer");
    assert_eq!(
        result.jumps[1].jump_server_id.as_deref(),
        Some(result.jumps[0].id.as_str())
    );
    assert_eq!(result.server.jump_server_id.as_deref(), Some("middle"));
    assert_eq!(result.jumps[1].notes, middle.notes);
    assert!(result.jumps[1].favorite);
    assert_eq!(
        result.server.host_key_alias.as_deref(),
        Some("target-known")
    );
    for server in result.jumps.iter().chain([&result.server]) {
        assert_eq!(server.source, ServerSource::Manual);
        assert_eq!(server.connection_engine, Engine::Builtin);
        assert_eq!(db.get_server(&server.id).unwrap().as_ref(), Some(server));
    }
    // 另一台目标应复用已经转换的同一链。
    let mut next = target.clone();
    next.id = "next".into();
    db.insert_server(&next).unwrap();
    let p = preview_with(&next, &db.list_servers().unwrap(), &mut f).unwrap();
    assert_eq!(
        p.jumps[0].existing_server_id.as_deref(),
        Some(result.jumps[0].id.as_str())
    );
    assert_eq!(p.jumps[1].existing_server_id.as_deref(), Some("middle"));
    assert!(p.jumps.iter().all(|jump| !jump.converts_existing));
}

#[test]
fn 跳板覆盖用户名端口递归链循环和每级黑名单() {
    let target = record("target");
    let mut f = Fixture {
        outputs: HashMap::from([
            (
                "target".into(),
                configured(&[("proxyjump", "bob@jump:2222")]),
            ),
            (
                "jump".into(),
                configured(&[("proxyjump", "outer"), ("knownhostscommand", "secret")]),
            ),
        ]),
    };
    let p = preview_with(&target, std::slice::from_ref(&target), &mut f).unwrap();
    assert_eq!(
        (p.jumps[1].fields.username.as_str(), p.jumps[1].fields.port),
        ("bob", 2222)
    );
    assert_eq!(p.reasons[0].hop, "jump");
    f.outputs
        .insert("outer".into(), configured(&[("proxyjump", "target")]));
    let p = preview_with(&target, std::slice::from_ref(&target), &mut f).unwrap();
    assert!(
        p.reasons
            .iter()
            .any(|r| r.code == "ssh.conversion_jump_cycle_or_depth")
    );
    let ipv6 = jump_spec("alice@[::1]:2222").unwrap();
    assert_eq!(ipv6.alias, "::1");
    assert_eq!(ipv6.port, Some(2222));
    for bad in [
        "-oBad", "a:0", "a:65536", "a,", "user@", "[::1", "::1", "a:b", "u@u@host",
    ] {
        // 逗号由链解析器分隔，单个主机的逗号不在这里处理。
        if bad != "a," {
            assert!(jump_spec(bad).is_err(), "{bad}");
        }
    }
}

#[test]
fn 事务失败回滚且数据库变化拒绝执行() {
    let root = crate::test_support::tempdir();
    let mut db = crate::storage::open(&root.path().join("db")).unwrap();
    let target = record("target");
    db.insert_server(&target).unwrap();
    let records = db.list_servers().unwrap();
    let mut p = preview(&configured(&[("proxyjump", "jump")]));
    p.target.username = "alice".into();
    // 目标外键故障发生于插入跳板之后，整个事务仍应回滚。
    let mut broken = target.clone();
    broken.password_cred = Some("非法凭据引用".into());
    assert!(apply(&mut db, &broken, &records, &p, 2).is_err());
    assert_eq!(db.list_servers().unwrap(), records);
    let mut changed = target.clone();
    changed.notes = "已修改".into();
    db.update_server(&changed).unwrap();
    assert_eq!(
        apply(&mut db, &target, &records, &p, 2).unwrap_err().code,
        "ssh.conversion_changed"
    );
}

#[cfg(unix)]
#[test]
fn 公开接口重新运行解析并拒绝旧预览及伪造字段() {
    use std::os::unix::fs::PermissionsExt;
    let root = crate::test_support::tempdir();
    let program = root.path().join("ssh");
    let output = root.path().join("output");
    std::fs::write(root.path().join("defaults"), BASE).unwrap();
    std::fs::write(&output, configured(&[("hostkeyalias", "old")])).unwrap();
    std::fs::write(&program, "#!/usr/bin/python3\nimport sys,pathlib\np=pathlib.Path(__file__).parent\nprint((p/('defaults' if '-F' in sys.argv else 'output')).read_text())\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let program = SshProgram { path: program };
    let mut db = crate::storage::open(&root.path().join("db")).unwrap();
    let server = record("target");
    db.insert_server(&server).unwrap();
    let p = preview_conversion(&program, &db, &server).unwrap();
    let mut forged = p.clone();
    forged.target.host = "evil".into();
    assert_eq!(
        execute_conversion(&program, &mut db, &server, &forged, 2)
            .unwrap_err()
            .code,
        "ssh.conversion_changed"
    );
    std::fs::write(&output, configured(&[("hostkeyalias", "new")])).unwrap();
    assert_eq!(
        execute_conversion(&program, &mut db, &server, &p, 2)
            .unwrap_err()
            .code,
        "ssh.conversion_changed"
    );
    assert_eq!(db.get_server(&server.id).unwrap().as_ref(), Some(&server));
    let p = preview_conversion(&program, &db, &server).unwrap();
    let result = execute_conversion(&program, &mut db, &server, &p, 2).unwrap();
    assert_eq!(result.server.host_key_alias.as_deref(), Some("new"));
    assert_eq!(
        preview_conversion(&program, &db, &result.server)
            .unwrap_err()
            .code,
        "ssh.conversion_not_imported"
    );
}

#[cfg(unix)]
#[test]
fn 真实解析包含文件的命令匹配和默认基线() {
    use std::os::unix::fs::PermissionsExt;
    let root = crate::test_support::tempdir();
    let config = root.path().join("config");
    let include = root.path().join("included");
    std::fs::write(&config, format!("Host target\n HostName example.test\n User alice\n Port 2222\n IdentityFile /first\\ key\n HostKeyAlias target-known\nInclude {}\n", include.display())).unwrap();
    std::fs::write(&include, "Match host other exec \"true\"\n User ignored\n").unwrap();
    let program = root.path().join("ssh");
    std::fs::write(&program, "#!/usr/bin/python3\nimport sys,os,pathlib\nargs=sys.argv[1:]\nif '-F' not in args: args=['-F',str(pathlib.Path(__file__).parent/'config')]+args\nos.execv('/usr/bin/ssh',['ssh']+args)\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let program = SshProgram { path: program };
    let mut db = crate::storage::open(&root.path().join("db")).unwrap();
    let server = record("target");
    db.insert_server(&server).unwrap();
    let p = preview_conversion(&program, &db, &server).unwrap();
    assert!(p.reasons.iter().any(|r| r.option == "Match exec"));
    assert_eq!(
        execute_conversion(&program, &mut db, &server, &p, 2)
            .unwrap_err()
            .code,
        "ssh.conversion_unsupported"
    );
    std::fs::write(&include, "").unwrap();
    let p = preview_conversion(&program, &db, &server).unwrap();
    assert!(p.reasons.is_empty(), "{:?}", p.reasons);
    assert_eq!(p.target.host, "example.test");
    assert_eq!(p.target.port, 2222);
    assert_eq!(p.target.username, "alice");
    assert_eq!(p.target.host_key_alias.as_deref(), Some("target-known"));
    assert_eq!(p.target.identity_files, ["/first key"]);
}

#[test]
fn 超过内置链深度时返回预览原因且不写入() {
    let p = preview(&configured(&[("proxyjump", "one,two,three,four")]));
    assert!(
        p.reasons
            .iter()
            .any(|r| r.code == "ssh.conversion_jump_cycle_or_depth")
    );
}
