//! 将各级目标的有效配置写成不继承用户配置的独立配置。
use crate::error::AppError;

const OMIT: &[&str] = &[
    "localforward",
    "remoteforward",
    "dynamicforward",
    "remotecommand",
    "requesttty",
    "permitlocalcommand",
    "localcommand",
    "forwardagent",
    "forwardx11",
    "controlmaster",
    "controlpath",
    "controlpersist",
    "userknownhostsfile",
    "globalknownhostsfile",
    "updatehostkeys",
    "nohostauthenticationforlocalhost",
    "stricthostkeychecking",
    "batchmode",
    "forkafterauthentication",
    "stdinnull",
    "loglevel",
];

fn invalid() -> AppError {
    AppError::new("ssh.invalid_isolated_config")
}

fn quote(value: &str) -> Result<String, AppError> {
    if value.contains(['\0', '\r', '\n', '"']) {
        return Err(invalid());
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('%', "%%")
    ))
}

/// 将 blocks 中每个 Host 及其 ssh -G 原始输出转换为隔离配置，返回配置文本。
/// existing_identity_files 是调用方确认存在的 identityfile 原文列表；其余身份文件被移除。
/// 调用方必须包含目标和完整跳板链，写入应用数据目录，并在连接结束或失败时删除文件。
/// pins 是当前用途钉死的选项，known_hosts 是应用指定路径；两者写入每一级 Host 块。
/// 探测专用配置应传 StrictHostKeyChecking=ask、LogLevel=ERROR，省略 BatchMode，并禁用密码及键盘交互。
/// ssh -G 不保留多词选项的引用边界；单路径选项整体引用，多参数选项按原有词法透传。
pub fn render_config(
    blocks: &[(String, &str)],
    existing_identity_files: &[String],
    pins: &[(&str, &str)],
    known_hosts: &std::path::Path,
) -> Result<String, AppError> {
    let known_hosts = known_hosts.to_str().ok_or_else(invalid)?;
    let mut output = String::new();
    let mut names = std::collections::HashSet::new();
    for (host, raw) in blocks {
        super::args::name(host, "host", true)?;
        if host.contains(['*', '?', '!']) || !names.insert(host.to_ascii_lowercase()) {
            return Err(invalid());
        }
        output.push_str(&format!("Host {}\n", quote(host)?));
        output.push_str(&format!("    UserKnownHostsFile {}\n", quote(known_hosts)?));
        for (key, value) in pins {
            if !key.bytes().all(|b| b.is_ascii_alphanumeric())
                || key.is_empty()
                || ["host", "match", "include", "userknownhostsfile"]
                    .contains(&key.to_ascii_lowercase().as_str())
            {
                return Err(invalid());
            }
            output.push_str(&format!("    {key} {}\n", quote(value)?));
        }
        let mut has_identity = false;
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            // 没有值的行（个别 OpenSSH 版本会输出）没有可透传的内容，跳过。
            let Some((key, value)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            let key = key.to_ascii_lowercase();
            let value = value.trim();
            if !key.bytes().all(|b| b.is_ascii_alphanumeric())
                || value.is_empty()
                || value.contains(['\0', '\r'])
            {
                return Err(invalid());
            }
            // 拒绝配置控制指令，避免原始输出被伪造为额外配置块。
            // OpenSSH 9.x 的 ssh -G 会输出 host 行；Host/Match/Include 由我们自己生成，
            // 原始输出里的一律跳过（而不是报错），也就不可能借此伪造出额外的配置块。
            if ["host", "match", "include"].contains(&key.as_str()) {
                continue;
            }
            if OMIT.contains(&key.as_str())
                || pins.iter().any(|(p, _)| p.eq_ignore_ascii_case(&key))
                || (key == "identityfile" && !existing_identity_files.iter().any(|p| p == value))
            {
                continue;
            }
            has_identity |= key == "identityfile";
            let rendered = if [
                "proxycommand",
                "proxyjump",
                "ipqos",
                "rekeylimit",
                "canonicaldomains",
                "canonicalizepermittedcnames",
                "sendenv",
                "setenv",
                "permitremoteopen",
                "logverbose",
            ]
            .contains(&key.as_str())
            {
                // 命令和多参数字段有自身词法，整体加引号会改变语义；保留 -G 的原始表示。
                value.to_owned()
            } else {
                quote(value)?
            };
            output.push_str(&format!("    {key} {rendered}\n"));
        }
        // 否则 OpenSSH 会重新补齐默认身份文件，恢复刚过滤掉的缺失路径。
        if !has_identity {
            output.push_str("    IdentityFile none\n");
        }
    }
    Ok(output)
}

/// 从手动目标及完整跳板链生成隔离配置和目标 Host 名，不调用 ssh -G。
/// purpose 决定每一级的安全选项，opts 提供同一个 known_hosts 和目标的 askpass 设置。
/// 返回文本须由调用方写入 opts.config_file（应用数据目录），连接结束或失败后删除；
/// 本函数不写文件。链从最外层开始编号，别名或显式配置混入链时返回错误。
pub fn render_manual_config(
    target: &super::target::SshTarget,
    purpose: &super::args::Purpose,
    opts: &super::args::CommonOpts,
) -> Result<(String, String), AppError> {
    use super::{
        args::{Purpose, build_args},
        target::SshTarget,
    };
    if opts.known_hosts.is_none() {
        return Err(AppError::new("ssh.known_hosts_required"));
    }
    let mut chain = Vec::new();
    let mut current = target;
    loop {
        match current {
            SshTarget::Manual { jump, .. } => {
                chain.push(current);
                match jump {
                    Some(next) => current = next,
                    None => break,
                }
            }
            SshTarget::Alias { .. } => return Err(AppError::new("ssh.jump_alias_needs_resolved")),
            SshTarget::Explicit(_) => return Err(invalid()),
        }
    }
    let mut output = String::new();
    for (index, node) in chain.iter().rev().enumerate() {
        let SshTarget::Manual {
            host,
            port,
            user,
            identity_file,
            ..
        } = node
        else {
            return Err(invalid());
        };
        let direct = SshTarget::Manual {
            host: host.clone(),
            port: *port,
            user: user.clone(),
            identity_file: identity_file.clone(),
            jump: None,
        };
        let mut node_opts = opts.clone();
        if index + 1 != chain.len() {
            node_opts.askpass = None;
            node_opts.dedicated_key = None;
        }
        let command = build_args(&direct, purpose, &node_opts)?;
        output.push_str(&format!(
            "Host pierlink-{index}\n    HostName {}\n    Port {port}\n",
            quote(host)?
        ));
        if let Some(user) = user {
            output.push_str(&format!("    User {}\n", quote(user)?));
        }
        if node_opts.dedicated_key.is_some() {
            // 目标身份文件由下方专用密钥参数唯一指定。
        } else if let Some(identity) = identity_file {
            let identity = identity.to_str().ok_or_else(invalid)?;
            output.push_str(&format!(
                "    IdentityFile {}\n    IdentitiesOnly yes\n",
                quote(identity)?
            ));
        } else {
            output.push_str("    IdentityFile none\n    IdentitiesOnly no\n");
        }
        if index > 0 {
            output.push_str(&format!("    ProxyJump pierlink-{}\n", index - 1));
        }
        if !matches!(purpose, Purpose::HostKeyProbe) {
            output.push_str("    StrictHostKeyChecking yes\n");
            if node_opts.askpass.is_none() && node_opts.dedicated_key.is_none() {
                output.push_str("    BatchMode yes\n");
            }
        }
        if node_opts.askpass.is_none() || matches!(purpose, Purpose::HostKeyProbe) {
            output.push_str("    PasswordAuthentication no\n    KbdInteractiveAuthentication no\n");
        }
        // 参数构造器是各用途钉死选项的唯一来源；其配置值已完成路径转义。
        for pair in command.args.windows(2).filter(|pair| pair[0] == "-o") {
            let (key, value) = pair[1].split_once('=').ok_or_else(invalid)?;
            // 没用专用密钥时 IdentitiesOnly 已经按这一级的私钥写在上面，不再重复
            if key == "IdentitiesOnly" && node_opts.dedicated_key.is_none() {
                continue;
            }
            output.push_str(&format!("    {key} {value}\n"));
        }
    }
    Ok((output, format!("pierlink-{}", chain.len() - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 修订3_每一级跳板实际写入钉死选项和主机库() {
        let root = crate::test_support::tempdir();
        let file = root.path().join("isolated");
        let pins = [
            ("StrictHostKeyChecking", "yes"),
            ("LogLevel", "ERROR"),
            ("RemoteCommand", "none"),
            ("PermitLocalCommand", "no"),
            ("ForwardAgent", "no"),
            ("ForwardX11", "no"),
            ("RequestTTY", "no"),
            ("NoHostAuthenticationForLocalhost", "no"),
            ("ControlMaster", "no"),
            ("ControlPath", "none"),
            ("UpdateHostKeys", "no"),
            ("ForkAfterAuthentication", "no"),
            ("StdinNull", "no"),
            ("ClearAllForwardings", "yes"),
            ("BatchMode", "yes"),
            ("PasswordAuthentication", "no"),
            ("KbdInteractiveAuthentication", "no"),
        ];
        let raw = "hostname example.com\nstricthostkeychecking no\nloglevel DEBUG3\npasswordauthentication yes\nuserknownhostsfile /dev/null";
        let blocks = [
            ("target".into(), raw),
            ("jump".into(), raw),
            ("outer".into(), raw),
        ];
        let rendered = render_config(
            &blocks,
            &[],
            &pins,
            std::path::Path::new("/中文 路径/known_hosts"),
        )
        .unwrap();
        assert_eq!(rendered.matches("UserKnownHostsFile").count(), 3);
        for (key, value) in pins {
            assert_eq!(rendered.matches(&format!("{key} \"{value}\"")).count(), 3);
        }
        std::fs::write(&file, rendered).unwrap();
        #[cfg(unix)]
        for host in ["target", "jump", "outer"] {
            let out = std::process::Command::new("ssh")
                .args(["-G", "-F"])
                .arg(&file)
                .arg(host)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let out = String::from_utf8(out.stdout).unwrap();
            assert!(out.contains("stricthostkeychecking true"));
            assert!(out.contains("loglevel ERROR"));
            assert!(out.contains("passwordauthentication no"));
            assert!(out.contains("userknownhostsfile /中文 路径/known_hosts"));
        }
        assert!(
            render_config(
                &blocks,
                &[],
                &[("Host", "*")],
                std::path::Path::new("hosts")
            )
            .is_err()
        );
    }
    #[test]
    fn s21_s26_过滤并正确引用路径() {
        let path = r"C:\Users\John Smith\100%\key";
        let raw = format!(
            "hostname example.com\nidentityfile {path}\nidentityfile missing\ncertificatefile {path}\nuserknownhostsfile bad path\nlocalforward 1 localhost:2\nremotecommand echo bad"
        );
        let config = render_config(
            &[("target".into(), &raw)],
            &[path.into()],
            &[],
            std::path::Path::new("/app/hosts"),
        )
        .unwrap();
        assert!(config.contains(r#"identityfile "C:\\Users\\John Smith\\100%%\\key""#));
        for removed in [
            "missing",
            "userknownhostsfile",
            "localforward",
            "remotecommand",
        ] {
            assert!(!config.contains(removed));
        }
        let raw = OMIT
            .iter()
            .map(|key| format!("{key} removed-value"))
            .collect::<Vec<_>>()
            .join("\n");
        let config = render_config(
            &[("target".into(), &raw)],
            &[],
            &[],
            std::path::Path::new("/app/hosts"),
        )
        .unwrap();
        assert!(!config.contains("removed-value"));
        assert!(config.contains("IdentityFile none"));
        assert!(
            render_config(
                &[
                    ("target".into(), "hostname x"),
                    ("TARGET".into(), "hostname y")
                ],
                &[],
                &[],
                std::path::Path::new("/app/hosts")
            )
            .is_err()
        );
        for bad in ["*", "a\nHost *", "!foo", "a%h"] {
            assert!(
                render_config(
                    &[(bad.into(), "hostname a")],
                    &[],
                    &[],
                    std::path::Path::new("/app/hosts")
                )
                .is_err()
            );
        }
        for bad in ["port 22\rxxx", "hostname a\0b"] {
            assert!(
                render_config(
                    &[("good".into(), bad)],
                    &[],
                    &[],
                    std::path::Path::new("/app/hosts")
                )
                .is_err()
            );
        }
        // ssh -G 原始输出里的 host/match/include 行和无值行被跳过，不会出现在生成的配置里。
        let out = render_config(
            &[(
                "good".into(),
                "host evil\ninclude /bad\nmatch all\nobscurekeystroketiming\nport 22",
            )],
            &[],
            &[],
            std::path::Path::new("/app/hosts"),
        )
        .unwrap();
        assert!(!out.to_lowercase().contains("evil"));
        assert!(!out.contains("/bad"));
        assert!(!out.to_lowercase().contains("match"));
        assert_eq!(out.lines().filter(|l| l.starts_with("Host ")).count(), 1);
        assert!(out.contains("port \"22\""));
    }
}

#[cfg(test)]
mod manual_tests {
    use super::*;
    use crate::ssh::{
        args::{AskPass, CommonOpts, Purpose, build_args},
        target::SshTarget,
    };

    fn chain(jumps: usize) -> SshTarget {
        let mut target = None;
        for index in 0..=jumps {
            target = Some(Box::new(SshTarget::Manual {
                host: format!("host{index}.example"),
                port: 2200 + index as u16,
                user: Some(format!("user{index}")),
                identity_file: Some(format!("C:\\用户 空格\\100%\\key{index}").into()),
                jump: target,
            }));
        }
        *target.unwrap()
    }
    fn opts() -> CommonOpts {
        CommonOpts {
            config_file: Some("isolated config".into()),
            known_hosts: Some(r"C:\用户 空格\100%\hosts".into()),
            connect_timeout: Some(9),
            ..Default::default()
        }
    }
    #[test]
    fn s6_s10_s26_两级三级跳板配置快照() {
        for (jumps, expected) in [
            (2, include_str!("../../tests/data/manual-jump-2.conf")),
            (3, include_str!("../../tests/data/manual-jump-3.conf")),
        ] {
            let cmd = build_args(&chain(jumps), &Purpose::Background, &opts()).unwrap();
            assert_eq!(cmd.config_text.as_deref(), Some(expected));
            assert!(cmd.args.windows(2).any(|p| p == ["-F", "isolated config"]));
            assert_eq!(cmd.args[cmd.args.len() - 2], format!("pierlink-{jumps}"));
            assert!(
                !cmd.args
                    .iter()
                    .any(|a| a.contains("ProxyCommand") || a == "-i" || a == "-p")
            );
        }
    }
    #[test]
    fn s9_拒绝非法链和配置注入() {
        let mut settings = opts();
        settings.known_hosts = None;
        assert_eq!(
            render_manual_config(&chain(2), &Purpose::Background, &settings)
                .unwrap_err()
                .code,
            "ssh.known_hosts_required"
        );
        settings = opts();
        settings.config_file = None;
        assert!(build_args(&chain(1), &Purpose::Background, &settings).is_err());
        for bad in ["a\nHost *", "a\"b", "a\0b"] {
            let mut target = chain(1);
            if let SshTarget::Manual { identity_file, .. } = &mut target {
                *identity_file = Some(bad.into());
            }
            assert!(render_manual_config(&target, &Purpose::Background, &opts()).is_err());
        }
        for jump_target in [
            SshTarget::Alias {
                alias: "gateway".into(),
            },
            SshTarget::Manual {
                host: "-bad".into(),
                port: 22,
                user: None,
                identity_file: None,
                jump: None,
            },
        ] {
            let mut target = chain(1);
            if let SshTarget::Manual { jump, .. } = &mut target {
                *jump = Some(Box::new(jump_target));
            }
            assert!(build_args(&target, &Purpose::Background, &opts()).is_err());
        }
    }
    #[test]
    fn s23_仅目标启用自动填密码且探测全部禁用() {
        let mut settings = opts();
        settings.askpass = Some(AskPass {
            program: "askpass".into(),
            registration: crate::ssh::askpass_pipe::test_registration("user2@host2.example"),
        });
        for purpose in [Purpose::Background, Purpose::HostKeyProbe] {
            let cmd = build_args(&chain(2), &purpose, &settings).unwrap();
            let text = cmd.config_text.as_ref().unwrap();
            assert!(!text.contains("秘密引用"));
            assert!(!format!("{cmd:?}").contains("秘密引用"));
            for (index, block) in text.split("Host pierlink-").skip(1).enumerate() {
                if matches!(purpose, Purpose::HostKeyProbe) {
                    assert!(block.contains("StrictHostKeyChecking ask"));
                    assert!(!block.contains("BatchMode"));
                    assert!(block.contains("PasswordAuthentication no"));
                    assert!(cmd.env.is_empty());
                } else if index == 2 {
                    assert!(block.contains("BatchMode no"));
                    assert!(block.contains("NumberOfPasswordPrompts 1"));
                    assert!(!block.contains("PasswordAuthentication no"));
                } else {
                    assert!(block.contains("BatchMode yes"));
                    assert!(block.contains("PasswordAuthentication no"));
                    assert!(block.contains("KbdInteractiveAuthentication no"));
                }
            }
        }
    }
    #[cfg(unix)]
    #[test]
    fn s3_s21_s22_s24_s25_真实解析每级用途及路径() {
        let root = crate::test_support::tempdir();
        let mut settings = opts();
        settings.config_file = Some(root.path().join("config"));
        for purpose in [
            Purpose::Background,
            Purpose::Sftp,
            Purpose::Tunnel {
                forwards: vec![],
                readiness: crate::ssh::args::Readiness::ProbeLocalPorts,
            },
            Purpose::HostKeyProbe,
            Purpose::Terminal {
                login_shell: None,
                cwd_marker: None,
                integration: false,
            },
        ] {
            let cmd = build_args(&chain(3), &purpose, &settings).unwrap();
            std::fs::write(
                settings.config_file.as_ref().unwrap(),
                cmd.config_text.as_ref().unwrap(),
            )
            .unwrap();
            for index in 0..=3 {
                let output = std::process::Command::new("ssh")
                    .args(["-G", "-F"])
                    .arg(settings.config_file.as_ref().unwrap())
                    .arg(format!("pierlink-{index}"))
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let output = String::from_utf8(output.stdout).unwrap();
                for expected in [
                    format!("hostname host{index}.example"),
                    format!("user user{index}"),
                    format!("port {}", 2200 + index),
                    format!(r"identityfile C:\用户 空格\100%%\key{index}"),
                    r"userknownhostsfile C:\用户 空格\100%\hosts".into(),
                    "identitiesonly yes".into(),
                    "serveraliveinterval 15".into(),
                    "serveralivecountmax 3".into(),
                    "connecttimeout 9".into(),
                    "passwordauthentication no".into(),
                    "kbdinteractiveauthentication no".into(),
                ] {
                    assert!(
                        output.lines().any(|l| l == expected),
                        "{expected}: {output}"
                    );
                }
                if index > 0 {
                    assert!(output.contains(&format!("proxyjump pierlink-{}\n", index - 1)));
                }
                assert!(
                    output.contains(if matches!(purpose, Purpose::HostKeyProbe) {
                        "stricthostkeychecking ask"
                    } else {
                        "stricthostkeychecking true"
                    })
                );
                assert!(
                    output.contains(if matches!(purpose, Purpose::HostKeyProbe) {
                        "batchmode no"
                    } else {
                        "batchmode yes"
                    })
                );
                if !matches!(purpose, Purpose::Terminal { .. }) {
                    for setting in [
                        "requesttty false",
                        "permitlocalcommand no",
                        "forwardagent no",
                        "forwardx11 no",
                        "nohostauthenticationforlocalhost no",
                        "controlmaster false",
                        "updatehostkeys false",
                        "forkafterauthentication no",
                        "stdinnull no",
                    ] {
                        assert!(output.contains(setting), "{setting}: {output}");
                    }
                    assert!(!output.contains("remotecommand "));
                    assert!(!output.contains("controlpath "));
                    assert!(
                        output.contains(if matches!(purpose, Purpose::Tunnel { .. }) {
                            "loglevel INFO"
                        } else {
                            "loglevel ERROR"
                        })
                    );
                }
                if matches!(purpose, Purpose::Tunnel { .. }) {
                    assert!(output.contains("exitonforwardfailure yes"));
                }
                if matches!(
                    purpose,
                    Purpose::Background | Purpose::Sftp | Purpose::HostKeyProbe
                ) {
                    assert!(output.contains("clearallforwardings yes"));
                }
            }
        }
    }
}

// 只接受本模块生成的无通配 Host 块，移除目标的所有累加身份选项；跳板保持原样。
pub(crate) fn dedicated_config(source: &str, target: &str) -> Result<String, AppError> {
    let mut output = String::new();
    let mut selected = false;
    let mut found = false;
    let mut in_host = false;
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once(char::is_whitespace).ok_or_else(invalid)?;
        let key = key.to_ascii_lowercase();
        if key == "match" || key == "include" {
            return Err(invalid());
        }
        if key == "host" {
            in_host = true;
            let host = value.trim().trim_matches('"');
            super::args::name(host, "host", true)?;
            if host.contains(['*', '?', '!']) {
                return Err(invalid());
            }
            selected = host.eq_ignore_ascii_case(target);
            found |= selected;
        } else if !in_host {
            return Err(invalid());
        } else if selected && ["identityfile", "certificatefile"].contains(&key.as_str()) {
            continue;
        }
        output.push_str(line);
        output.push('\n');
    }
    if !found {
        return Err(invalid());
    }
    Ok(output)
}
