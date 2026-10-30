//! 需要特殊 sshd 配置的验证；缺环境变量时明确输出跳过原因。
mod support;
#[path = "support/totp.rs"]
mod totp;
use pierlink_ssh_engine::{Authentication, ChannelFailure, Engine, Error};
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[test]
fn password_and_keyboard_interactive_are_separate_methods() {
    let Some(values) = support::required(&[
        "PIERLINK_ENGINE_SSHD",
        "PIERLINK_ENGINE_PW_USER",
        "PIERLINK_ENGINE_PW",
    ]) else {
        return;
    };
    let engine = Engine::new().unwrap();
    let connection = engine
        .connect(
            support::options(&values[0], &values[1]),
            Authentication::Password(values[2].clone()),
        )
        .unwrap();
    assert_eq!(
        support::command(&connection, "printf password-ok"),
        b"password-ok"
    );
    let password = values[2].clone();
    let calls = Arc::new(Mutex::new(0usize));
    let count = calls.clone();
    let auth = Authentication::KeyboardInteractive(Arc::new(move |challenge| {
        *count.lock().unwrap() += 1;
        Some(challenge.prompts.iter().map(|_| password.clone()).collect())
    }));
    let connection = engine
        .connect(support::options(&values[0], &values[1]), auth)
        .unwrap();
    assert_eq!(
        support::command(&connection, "printf interactive-ok"),
        b"interactive-ok"
    );
    assert!(*calls.lock().unwrap() >= 1);
}

#[test]
fn keyboard_interactive_password_then_totp_multiple_rounds() {
    let Some(values) = support::required(&[
        "PIERLINK_ENGINE_SSHD",
        "PIERLINK_ENGINE_OTP_USER",
        "PIERLINK_ENGINE_OTP_PW",
        "PIERLINK_ENGINE_OTP_SECRET",
    ]) else {
        return;
    };
    let password = values[2].clone();
    let secret = values[3].clone();
    let seen = Arc::new(Mutex::new((0usize, false, false)));
    let observed = seen.clone();
    let auth = Authentication::KeyboardInteractive(Arc::new(move |challenge| {
        let mut seen = observed.lock().unwrap();
        seen.0 += 1;
        let mut answers = Vec::new();
        for prompt in challenge.prompts {
            let text = prompt.text.to_ascii_lowercase();
            if text.contains("password") || text.contains("密码") {
                seen.1 = true;
                answers.push(password.clone());
            } else if text.contains("verification")
                || text.contains("code")
                || text.contains("验证码")
            {
                seen.2 = true;
                let seconds = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                answers.push(totp::code(&secret, seconds, 6));
            } else {
                return None;
            }
        }
        Some(answers)
    }));
    let connection = Engine::new()
        .unwrap()
        .connect(support::options(&values[0], &values[1]), auth)
        .unwrap();
    assert_eq!(support::command(&connection, "printf otp-ok"), b"otp-ok");
    let seen = seen.lock().unwrap();
    assert!(
        seen.0 >= 2 && seen.1 && seen.2,
        "服务器必须分别询问密码与验证码"
    );
}

#[test]
fn client_alive_30_seconds_and_hostkeys_announcement() {
    let Some((_engine, connection)) = support::key_connection("PIERLINK_ENGINE_SSHD") else {
        return;
    };
    // 客户端默认 15 秒的保活无法掩盖服务端 1 秒间隔、2 次上限的请求处理缺陷。
    std::thread::sleep(Duration::from_secs(30));
    connection.ping().unwrap();
    assert_eq!(support::command(&connection, "printf alive"), b"alive");
    let stats = connection.statistics();
    assert!(stats.hostkey_announcements > 0, "没有收到 hostkeys-00 公告");
    eprintln!(
        "保活 30 秒；主机密钥公告 {} 次",
        stats.hostkey_announcements
    );
}

#[test]
fn bidirectional_over_one_gib_with_multiple_rekeys() {
    let Some((_engine, connection)) = support::key_connection("PIERLINK_ENGINE_SSHD") else {
        return;
    };
    let size = 1024u64 * 1024 * 1024 + 1024 * 1024;
    let before = connection.statistics().key_exchanges;
    let started = Instant::now();
    let mut upload = connection
        .exec("cat >/dev/null; printf uploaded", &[])
        .unwrap();
    let mut download = connection
        .exec(&format!("head -c {size} /dev/zero"), &[])
        .unwrap();
    std::thread::scope(|scope| {
        let upload = scope.spawn(|| {
            let block = vec![0x5a; 64 * 1024];
            let mut sent = 0;
            while sent < size {
                let count = (size - sent).min(block.len() as u64) as usize;
                upload.stdin.write_all(&block[..count]).unwrap();
                sent += count as u64;
            }
            upload.stdin.shutdown().unwrap();
        });
        let mut block = vec![0; 64 * 1024];
        let mut received = 0u64;
        loop {
            let count = download.stdout.read(&mut block).unwrap();
            if count == 0 {
                break;
            }
            assert!(
                block[..count].iter().all(|byte| *byte == 0),
                "下载内容不一致"
            );
            received += count as u64;
        }
        assert_eq!(received, size);
        upload.join().unwrap();
    });
    let mut reply = String::new();
    upload.stdout.read_to_string(&mut reply).unwrap();
    assert_eq!(reply, "uploaded");
    assert_eq!(upload.wait().unwrap(), Some(0));
    assert_eq!(download.wait().unwrap(), Some(0));
    let rekeys = connection.statistics().key_exchanges - before;
    assert!(rekeys >= 8, "必须配置 RekeyLimit 64M，确认确实多次换钥");
    eprintln!(
        "每方向 {size} 字节；耗时 {:.3} 秒；换钥 {rekeys} 次",
        started.elapsed().as_secs_f64()
    );
}

#[test]
fn unread_channel_has_bounded_buffering() {
    let Some((_engine, connection)) = support::key_connection("PIERLINK_ENGINE_SSHD") else {
        return;
    };
    let rss_before = rss_bytes();
    let flood = connection.exec("exec yes", &[]).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let first = connection.statistics().received_bytes;
    std::thread::sleep(Duration::from_secs(3));
    let second = connection.statistics().received_bytes;
    assert!(second - first < 512 * 1024, "停止消费后引擎仍持续排空网络");
    if let (Some(before), Some(after)) = (rss_before, rss_bytes()) {
        assert!(
            after.saturating_sub(before) < 32 * 1024 * 1024,
            "单个不读通道导致内存增长超过 32 MiB"
        );
        eprintln!(
            "背压：已接收 {second} 字节；后 3 秒增加 {} 字节；RSS 增加 {} 字节",
            second - first,
            after.saturating_sub(before)
        );
    }
    drop(flood);
    connection.ping().unwrap();
}

fn rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|kb| kb * 1024)
    })
}

#[test]
fn maxsessions_two_rejects_third_session() {
    let Some((_engine, connection)) = support::key_connection("PIERLINK_ENGINE_MAXSESS") else {
        return;
    };
    let first = connection.shell(&Default::default(), &[]).unwrap();
    let second = connection.shell(&Default::default(), &[]).unwrap();
    let error = match connection.exec("true", &[]) {
        Ok(_) => panic!("第 3 个会话应当被拒绝"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        Error::ChannelRejected(
            ChannelFailure::ConnectFailed
                | ChannelFailure::ResourceShortage
                | ChannelFailure::Prohibited
        )
    ));
    eprintln!("MaxSessions 拒绝类型：{error}");
    first.close().unwrap();
    second.close().unwrap();
}

#[test]
fn old_openssh_default_and_explicit_legacy_algorithms() {
    let Some(values) = support::required(&[
        "PIERLINK_ENGINE_OLD",
        "PIERLINK_ENGINE_KEY_USER",
        "PIERLINK_ENGINE_KEY",
    ]) else {
        return;
    };
    let engine = Engine::new().unwrap();
    for target in values[0].split(',') {
        let (name, endpoint) = target
            .split_once('=')
            .expect("旧服务器必须为 名字=host:port");
        let options = support::options(endpoint, &values[1]);
        match engine.connect(options.clone(), support::key_auth(&values[2])) {
            Ok(connection) => {
                assert_eq!(support::command(&connection, "printf old-ok"), b"old-ok");
                eprintln!("旧服务器 {name}：默认算法连接成功");
            }
            Err(error) => {
                eprintln!("旧服务器 {name}：默认连接失败（{error}），测试显式旧算法");
                let mut options = options;
                options.legacy_algorithms = true;
                match engine.connect(options, support::key_auth(&values[2])) {
                    Ok(connection) => {
                        assert_eq!(
                            support::command(&connection, "printf legacy-ok"),
                            b"legacy-ok"
                        );
                        eprintln!("旧服务器 {name}：显式旧算法连接成功");
                    }
                    // OpenSSH 7.4（CentOS 7）必须能连；更老的（如 CentOS 6 的 5.3）连不上时归入系统 OpenSSH 兼容模式
                    Err(error) if name != "openssh74" => {
                        eprintln!(
                            "旧服务器 {name}：显式旧算法也连不上（{error}），需用系统 OpenSSH 兼容模式"
                        );
                    }
                    Err(error) => panic!("旧服务器 {name} 应能连接：{error}"),
                }
            }
        }
    }
}
