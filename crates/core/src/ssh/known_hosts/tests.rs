use super::*;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use ssh_key::encoding::base64::{Base64, Encoding};
use std::sync::{Arc, Barrier};

const ED: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILagOJFgwaMNhBWQINinKOXmqS4Gh5NgxgriXwdOoINJ";
const OTHER_ED: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
const EC: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBMxBTpMIGvo7CnordO7wP0QQRqpBwUjOLl4eMhfucfE1sjTYyK5wmTl1UqoSDS1PtRVTBdl+0+9pquFb46U7fwg=";

fn key(host: &str, port: u16, public: &str) -> HostKey {
    let parsed = PublicKey::from_openssh(public).unwrap();
    let (algorithm, public_key) = public.split_once(' ').unwrap();
    HostKey {
        host: host.into(),
        port,
        algorithm: algorithm.into(),
        public_key: public_key.into(),
        fingerprint: parsed.fingerprint(HashAlg::Sha256).to_string(),
    }
}

fn store() -> (tempfile::TempDir, KnownHosts) {
    let root = crate::test_support::tempdir();
    let hosts = KnownHosts::new(
        root.path().join(".ssh/known_hosts"),
        Some(root.path().join(".ssh/known_hosts2")),
    );
    (root, hosts)
}

fn write(hosts: &KnownHosts, text: &str) {
    fs::create_dir_all(hosts.primary.parent().unwrap()).unwrap();
    fs::write(&hosts.primary, text).unwrap();
}

fn new_status(hosts: &KnownHosts, key: &HostKey) -> Vec<String> {
    let HostKeyStatus::New { other_key_types } = hosts.check(key, None).unwrap().status else {
        panic!("应当需要首次确认");
    };
    other_key_types
}

#[test]
fn 默认路径只构造两个用户文件且缺目录的查询不写盘() {
    let defaults = KnownHosts::for_user().unwrap();
    assert!(defaults.primary.ends_with(".ssh/known_hosts"));
    assert!(defaults.secondary.unwrap().ends_with(".ssh/known_hosts2"));
    let (_root, hosts) = store();
    assert!(new_status(&hosts, &key("host", 22, ED)).is_empty());
    assert!(
        hosts
            .preferred_algorithms("host", 22, None)
            .unwrap()
            .is_empty()
    );
    assert!(!hosts.primary.parent().unwrap().exists());
}

#[test]
fn 解析空行注释多主机通配否定行尾注释和制表符() {
    let (_root, hosts) = store();
    write(
        &hosts,
        &format!("\n  # 注释\n  alias,*.Example.???,!bad.example.com\t{ED}\t行尾注释\r\n"),
    );
    for host in ["alias", "www.example.com", "WWW.EXAMPLE.COM"] {
        let report = hosts.check(&key(host, 22, ED), None).unwrap();
        assert_eq!(report.status, HostKeyStatus::Trusted, "{host}");
        assert_eq!(report.invalid_lines, 0);
    }
    for host in [
        "bad.example.com",
        "www.example.toolong",
        "example.com",
        "unrelated",
    ] {
        assert!(new_status(&hosts, &key(host, 22, ED)).is_empty(), "{host}");
    }
    write(&hosts, &format!("!blocked {ED}\n"));
    assert!(new_status(&hosts, &key("other", 22, ED)).is_empty());
    write(&hosts, &format!("*,!blocked {ED}\n"));
    assert!(new_status(&hosts, &key("blocked", 22, ED)).is_empty());
    assert_eq!(
        hosts.check(&key("other", 22, ED), None).unwrap().status,
        HostKeyStatus::Trusted
    );
}

#[test]
fn s5_一致变化多类型多行以及撤销优先不受文件顺序影响() {
    let (_root, hosts) = store();
    let ed = key("host", 22, ED);
    write(&hosts, &format!("host {OTHER_ED}\nhost {EC}\n"));
    let error = hosts.check(&ed, None).unwrap_err();
    assert_eq!(error.code, "ssh.host_key_changed");
    assert_eq!(error.params["line"], "1");
    assert!(error.params["fingerprint"].starts_with("SHA256:"));
    write(&hosts, &format!("host {OTHER_ED}\nhost {ED}\nhost {EC}\n"));
    assert_eq!(
        hosts.check(&ed, None).unwrap().status,
        HostKeyStatus::Trusted
    );
    assert_eq!(
        hosts.check(&key("host", 22, EC), None).unwrap().status,
        HostKeyStatus::Trusted
    );
    for text in [
        format!("host {ED}\n@revoked host {ED}\n"),
        format!("@revoked host {ED}\nhost {ED}\n"),
    ] {
        write(&hosts, &text);
        let error = hosts.check(&ed, None).unwrap_err();
        assert_eq!(error.code, "ssh.host_key_changed");
        assert!(error.params["reason"].contains("撤销"));
    }
    // 撤销的是特定公钥，不是将这台主机所有其他公钥一并撤销。
    write(&hosts, &format!("@revoked host {OTHER_ED}\nhost {ED}\n"));
    assert_eq!(
        hosts.check(&ed, None).unwrap().status,
        HostKeyStatus::Trusted
    );
    write(&hosts, &format!("host {EC}\nhost {EC}\n"));
    assert_eq!(new_status(&hosts, &ed), ["ecdsa-sha2-nistp256"]);
    write(
        &hosts,
        &format!("@cert-authority host {ED}\n@cert-authority host {EC}\n"),
    );
    assert!(new_status(&hosts, &ed).is_empty());
    assert!(
        hosts
            .preferred_algorithms("host", 22, None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn 哈希按_hmac_sha1_生成并核对非标准端口和别名() {
    let (_root, hosts) = store();
    let salt = [7u8; 20];
    for (host, port, alias, name) in [
        ("Host", 22, None, "host"),
        ("::1", 2222, None, "[::1]:2222"),
        ("ignored", 2222, Some("Alias"), "alias"),
    ] {
        let hash = Hmac::<Sha1>::new_from_slice(&salt)
            .unwrap()
            .chain_update(name.as_bytes())
            .finalize()
            .into_bytes();
        let record = format!(
            "|1|{}|{} {ED}\n",
            Base64::encode_string(&salt),
            Base64::encode_string(&hash)
        );
        write(&hosts, &record);
        assert_eq!(
            hosts.check(&key(host, port, ED), alias).unwrap().status,
            HostKeyStatus::Trusted
        );
        assert_eq!(
            hosts.preferred_algorithms(host, port, alias).unwrap(),
            ["ssh-ed25519"]
        );
        assert!(new_status(&hosts, &key("unrelated", port, ED)).is_empty());
        write(&hosts, &format!("@revoked {record}"));
        assert_eq!(
            hosts.check(&key(host, port, ED), alias).unwrap_err().code,
            "ssh.host_key_changed"
        );
    }
}

#[test]
fn 坏行逐条计数超长行整体跳过且后续有效行仍生效() {
    let (_root, hosts) = store();
    let bad = [
        "host ssh-ed25519",
        "host ssh-ed25519 !!!",
        "@unknown host",
        "@revoked",
        "host ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAILagOJFgwaMNhBWQINinKOXmqS4Gh5NgxgriXwdOoINJ",
        ",host",
        "host,,name",
        "!",
        "!|1|bad|bad",
        "|2|bad|bad",
        "|1|AA==|AA==",
        "|1|not-base64|also-bad",
    ];
    let mut text = String::new();
    for line in bad {
        text.push_str(&format!("{line} {ED}\n"));
    }
    text.push_str(&"x".repeat(MAX_LINE + 100));
    text.push_str(&format!("\nhost {ED}\n"));
    write(&hosts, &text);
    let report = hosts.check(&key("host", 22, ED), None).unwrap();
    assert_eq!(report.status, HostKeyStatus::Trusted);
    assert_eq!(report.invalid_lines, bad.len() + 1);
    let mut bytes = vec![0xff, b'\n'];
    bytes.extend_from_slice(format!("host {ED}\n").as_bytes());
    fs::write(&hosts.primary, bytes).unwrap();
    assert_eq!(
        hosts
            .check(&key("host", 22, ED), None)
            .unwrap()
            .invalid_lines,
        1
    );
}

#[test]
fn known_hosts2_只读参与核对变化撤销和算法偏好() {
    let (_root, hosts) = store();
    write(
        &hosts,
        &format!("@cert-authority host {ED}\n@revoked other {EC}\n"),
    );
    let second = hosts.secondary.as_ref().unwrap();
    let text = format!("host {ED}\nhost {EC}\nhost {ED}\n");
    fs::write(second, &text).unwrap();
    assert_eq!(
        hosts.check(&key("host", 22, ED), None).unwrap().status,
        HostKeyStatus::Trusted
    );
    assert_eq!(
        hosts.preferred_algorithms("host", 22, None).unwrap(),
        ["ssh-ed25519", "ecdsa-sha2-nistp256"]
    );
    let cancelled = AtomicBool::new(false);
    assert!(
        hosts
            .verify(&key("new", 22, ED), None, &cancelled, |_, _| Ok(true))
            .unwrap()
    );
    assert_eq!(fs::read_to_string(second).unwrap(), text);
    fs::write(second, format!("@revoked host {ED}\n")).unwrap();
    write(&hosts, &format!("host {ED}\n"));
    assert_eq!(
        hosts.check(&key("host", 22, ED), None).unwrap_err().code,
        "ssh.host_key_changed"
    );
    fs::write(second, format!("host {OTHER_ED}\n")).unwrap();
    write(&hosts, "");
    assert_eq!(
        hosts.check(&key("host", 22, ED), None).unwrap_err().code,
        "ssh.host_key_changed"
    );
}

#[test]
fn s6_追加读回换行权限非标准端口_ipv6_别名和跳板逐级隔离() {
    let (_root, hosts) = store();
    let cancelled = AtomicBool::new(false);
    for (host, port, alias, name) in [
        ("Host", 22, None, "host"),
        ("host", 2222, None, "[host]:2222"),
        ("::1", 2222, None, "[::1]:2222"),
        ("actual", 2222, Some("Alias"), "alias"),
    ] {
        let key = key(host, port, ED);
        assert!(
            hosts
                .verify(&key, alias, &cancelled, |_, report| {
                    assert!(matches!(report.status, HostKeyStatus::New { .. }));
                    Ok(true)
                })
                .unwrap()
        );
        assert!(
            fs::read_to_string(&hosts.primary)
                .unwrap()
                .lines()
                .any(|line| line == format!("{name} {ED}"))
        );
        assert!(
            hosts
                .verify(&key, alias, &cancelled, |_, _| panic!("已知不能再弹框"))
                .unwrap()
        );
    }
    assert!(new_status(&hosts, &key("jump", 22, ED)).is_empty());
    assert!(new_status(&hosts, &key("host", 2200, ED)).is_empty());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(hosts.primary.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&hosts.primary).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    for prefix in ["# 没有换行", "# 已有换行\n", ""] {
        write(&hosts, prefix);
        assert!(
            hosts
                .verify(&key("new", 22, ED), None, &cancelled, |_, _| Ok(true))
                .unwrap()
        );
        let expected = if prefix.is_empty() || prefix.ends_with('\n') {
            prefix.to_owned()
        } else {
            format!("{prefix}\n")
        };
        assert_eq!(
            fs::read_to_string(&hosts.primary).unwrap(),
            format!("{expected}new {ED}\n")
        );
    }
}

#[test]
fn 确认拒绝取消回调错误不写盘且其他类型提示保留() {
    let (_root, hosts) = store();
    let cancelled = AtomicBool::new(true);
    let ed = key("host", 22, ED);
    assert!(
        !hosts
            .verify(&ed, None, &cancelled, |_, _| panic!("取消不弹框"))
            .unwrap()
    );
    cancelled.store(false, Ordering::Release);
    assert!(
        !hosts
            .verify(&ed, None, &cancelled, |_, _| Ok(false))
            .unwrap()
    );
    assert_eq!(
        hosts
            .verify(&ed, None, &cancelled, |_, _| Err(AppError::new(
                "test.cancelled"
            )))
            .unwrap_err()
            .code,
        "test.cancelled"
    );
    assert!(!hosts.primary.exists());
    assert!(
        !hosts
            .verify(&ed, None, &cancelled, |_, _| {
                cancelled.store(true, Ordering::Release);
                Ok(true)
            })
            .unwrap()
    );
    assert!(!hosts.primary.exists());
    cancelled.store(false, Ordering::Release);
    write(&hosts, &format!("host {EC}\n"));
    assert!(
        hosts
            .verify(&ed, None, &cancelled, |seen, report| {
                assert_eq!(
                    seen.fingerprint,
                    "SHA256:ldyiXa1JQakitNU5tErauu8DvWQ1dZ7aXu+rm7KQuog"
                );
                assert_eq!(
                    report.status,
                    HostKeyStatus::New {
                        other_key_types: vec!["ecdsa-sha2-nistp256".into()]
                    }
                );
                Ok(true)
            })
            .unwrap()
    );
}

#[test]
fn s5_等待确认期间变更或撤销不能因已确认而放行() {
    let (_root, hosts) = store();
    let cancelled = AtomicBool::new(false);
    let ed = key("host", 22, ED);
    for record in [
        format!("host {OTHER_ED}\n"),
        format!("@revoked host {ED}\n"),
    ] {
        write(&hosts, "");
        let error = hosts
            .verify(&ed, None, &cancelled, |_, _| {
                write(&hosts, &record);
                Ok(true)
            })
            .unwrap_err();
        assert_eq!(error.code, "ssh.host_key_changed");
        assert_eq!(fs::read_to_string(&hosts.primary).unwrap(), record);
    }
}

#[test]
fn 并发追加不同主机不会粘行相同主机只写一条() {
    let (_root, hosts) = store();
    write(&hosts, "# 无换行");
    for same in [false, true] {
        let barrier = Arc::new(Barrier::new(12));
        std::thread::scope(|scope| {
            for index in 0..12 {
                let barrier = barrier.clone();
                let hosts = &hosts;
                scope.spawn(move || {
                    let key = key(
                        &format!("parallel-{}", if same { 99 } else { index }),
                        2222,
                        ED,
                    );
                    assert!(
                        hosts
                            .verify(&key, None, &AtomicBool::new(false), |_, _| {
                                barrier.wait();
                                Ok(true)
                            })
                            .unwrap()
                    );
                });
            }
        });
    }
    let saved = fs::read_to_string(&hosts.primary).unwrap();
    assert_eq!(saved.lines().count(), 14);
    for index in (0..12).chain([99]) {
        let report = hosts
            .check(&key(&format!("parallel-{index}"), 2222, ED), None)
            .unwrap();
        assert_eq!(report.status, HostKeyStatus::Trusted);
        assert_eq!(report.invalid_lines, 0);
    }
}

#[test]
fn 不可信主机公钥和读取写入错误明确失败且不泄露内容() {
    let (_root, hosts) = store();
    for host in [
        "",
        "a\nhost",
        "*",
        "host,other",
        "!host",
        "|1|bad",
        "#host",
        "@host",
    ] {
        assert_eq!(
            hosts.check(&key(host, 22, ED), None).unwrap_err().code,
            "ssh.invalid_host"
        );
    }
    assert_eq!(
        hosts.check(&key("host", 0, ED), None).unwrap_err().code,
        "ssh.invalid_host"
    );
    for value in [
        "秘密口令",
        "x\nhost ssh-ed25519 内容",
        &"a".repeat(MAX_LINE),
    ] {
        let mut ed = key("host", 22, ED);
        ed.public_key = value.to_owned();
        let error = hosts.check(&ed, None).unwrap_err();
        assert_eq!(error.code, "ssh.invalid_host_key");
        assert!(!format!("{error:?}").contains(value));
    }
    let mut ed = key("host", 22, ED);
    ed.fingerprint = "MD5:不可信指纹".into();
    assert_eq!(
        hosts.check(&ed, None).unwrap_err().code,
        "ssh.invalid_host_key"
    );
    write(&hosts, "");
    // 用一个目录当第二个 known_hosts：Linux 和 Windows 上读它都会报错（文件下面的路径在 Windows 上会被当成“不存在”）
    let unreadable = hosts.primary.with_file_name("known_hosts_dir");
    std::fs::create_dir_all(&unreadable).unwrap();
    let blocked = KnownHosts::new(hosts.primary.clone(), Some(unreadable));
    assert_eq!(
        blocked.check(&key("host", 22, ED), None).unwrap_err().code,
        "ssh.known_hosts_failed"
    );
    let blocked = KnownHosts::new(hosts.primary.join("child/known_hosts"), None);
    assert_eq!(
        blocked
            .verify(
                &key("host", 22, ED),
                None,
                &AtomicBool::new(false),
                |_, _| Ok(true)
            )
            .unwrap_err()
            .code,
        "ssh.known_hosts_failed"
    );
}

#[test]
fn 文件锁等待可取消() {
    let (_root, hosts) = store();
    write(&hosts, "");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&hosts.primary)
        .unwrap();
    file.lock().unwrap();
    let cancelled = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| hosts.append_confirmed(&key("host", 22, ED), None, &cancelled));
        std::thread::sleep(std::time::Duration::from_millis(30));
        cancelled.store(true, Ordering::Release);
        assert!(!worker.join().unwrap().unwrap());
    });
    assert_eq!(fs::metadata(&hosts.primary).unwrap().len(), 0);
}
