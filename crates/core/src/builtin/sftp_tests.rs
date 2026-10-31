//! 内存对端验证文件通道分派、协议异常与共享连接隔离。
use super::*;
use crate::{
    process::SshProgram,
    sftp::{self, SftpSession},
};
use pierlink_ssh_engine::test_support::{LoginMode, LoginPeer, ScriptReply};

fn program() -> SshProgram {
    SshProgram {
        path: "/不存在的ssh程序".into(),
    }
}
fn command(f: &Fixture) -> crate::ssh::args::SshCommand {
    sftp::command(&f.manager, f.request(false), || panic!("不能回退系统进程")).unwrap()
}
fn packet(body: &[u8]) -> Vec<u8> {
    (body.len() as u32)
        .to_be_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect()
}
fn reply(stdout: Vec<u8>) -> ScriptReply {
    ScriptReply {
        stdout,
        stderr: vec![],
        exit_code: None,
    }
}
fn attach(f: &Fixture, peer: LoginPeer) {
    let engine = Engine::new().unwrap();
    *f.manager.pool.connector.lock().unwrap() = Some(Arc::new(move |_, _, options, auth| {
        peer.connect(&engine, options, auth)
    }));
}
fn fixture() -> Fixture {
    let mut f = Fixture::new(AuthMethod::Password);
    f.server.connection_engine = crate::storage::Engine::Builtin;
    f.vault.set("s", Zeroizing::new("测试密码".into())).unwrap();
    f
}
fn stat_reply(request: &[u8]) -> ScriptReply {
    if request[0] == 1 {
        return reply(packet(&[2, 0, 0, 0, 3]));
    }
    let mut body = vec![105];
    body.extend_from_slice(&request[1..5]);
    body.extend_from_slice(&0u32.to_be_bytes());
    reply(packet(&body))
}

#[test]
fn 文件分派保留系统参数和别名回退且内置等待密码() {
    let mut f = fixture();
    let original = crate::ssh::args::SshCommand {
        args: vec!["原参数".into()],
        env: vec![],
        config_text: None,
        hooked: false,
        builtin: None,
        askpass: None,
    };
    for alias in [false, true] {
        f.server.connection_engine = if alias {
            crate::storage::Engine::Builtin
        } else {
            crate::storage::Engine::OpenSsh
        };
        f.server.ssh_alias = alias.then(|| "别名".into());
        assert_eq!(
            sftp::command(&f.manager, f.request(false), || Ok(original.clone())).unwrap(),
            original
        );
    }
    f.server.ssh_alias = None;
    f.vault = Arc::new(PasswordVault::default());
    let cmd = command(&f);
    assert!(cmd.args.is_empty() && cmd.env.is_empty() && cmd.askpass.is_none());
    assert_eq!(
        SftpSession::open(&program(), &cmd, Duration::from_secs(1))
            .err()
            .unwrap()
            .code,
        "ssh.password_needed"
    );
    assert_eq!(
        SftpSession::open(&program(), &cmd, Duration::ZERO)
            .err()
            .unwrap()
            .code,
        "sftp.protocol"
    );
}

#[test]
fn 文件复用批量连接且关闭取消不影响主连接及其他通道() {
    let f = fixture();
    attach(
        &f,
        LoginPeer::new(LoginMode::Password).with_sftp(|request| {
            let mut result = stat_reply(request);
            // 超过共享窗口的错误输出必须持续排空，不能污染 SFTP stdout。
            if request[0] == 1 {
                result.stderr = vec![b'x'; 3 * 1024 * 1024];
            }
            result
        }),
    );
    let main = f.manager.get_connection(f.request(false)).unwrap();
    let cmd = command(&f);
    let mut first = SftpSession::open(&program(), &cmd, Duration::from_secs(3)).unwrap();
    let mut second = SftpSession::open(&program(), &cmd, Duration::from_secs(3)).unwrap();
    let mut request = f.request(false);
    request.class = ConnectionClass::Bulk;
    let bulk = f.manager.get_connection(request).unwrap();
    assert_eq!(main.statistics().opened_sessions, 0);
    assert_eq!(bulk.statistics().opened_sessions, 2);
    assert!(first.stat(".").unwrap().is_some());
    let cancel = AtomicBool::new(true);
    assert_eq!(
        first
            .read_to("file", &mut vec![], &mut |_| {}, &cancel)
            .unwrap_err()
            .code,
        "sftp.cancelled"
    );
    drop(first);
    assert!(!main.is_closed() && !bulk.is_closed());
    assert!(second.stat(".").unwrap().is_some());
    bulk.disconnect().unwrap();
    assert_eq!(second.stat(".").unwrap_err().code, "sftp.connection_lost");
    main.ping().unwrap();
    let mut reconnected = SftpSession::open(&program(), &cmd, Duration::from_secs(3)).unwrap();
    assert!(reconnected.stat(".").unwrap().is_some());
    f.manager.close_all().unwrap();
}

#[test]
fn 内置握手拒绝畸形超时断线且不回显错误输出() {
    for (mode, code) in [
        (0, "sftp.unavailable"),
        (1, "sftp.protocol"),
        (2, "sftp.connection_lost"),
        (3, "sftp.connection_lost"),
        (4, "sftp.protocol"),
    ] {
        let f = fixture();
        let peer = LoginPeer::new(LoginMode::Password);
        let peer = if mode == 0 {
            peer.with_scripts(|_| reply(vec![]))
        } else {
            peer.with_sftp(move |_| ScriptReply {
                stdout: match mode {
                    1 => packet(&[2, 0, 0, 0, 4]),
                    3 => vec![0, 0, 0, 5, 2],
                    4 => vec![0, 16, 0, 1],
                    _ => vec![],
                },
                stderr: "password=不能泄露的口令".as_bytes().to_vec(),
                exit_code: (mode == 3).then_some(1),
            })
        };
        attach(&f, peer);
        let start = Instant::now();
        let error = SftpSession::open(&program(), &command(&f), Duration::from_millis(200))
            .err()
            .unwrap();
        assert_eq!(error.code, code, "模式 {mode}");
        assert!(!format!("{error:?}").contains("不能泄露"));
        assert!(start.elapsed() < Duration::from_secs(3));
        f.manager.close_all().unwrap();
    }
}

#[test]
fn 内置在途请求缺失超时后释放阻塞读写() {
    let f = fixture();
    attach(
        &f,
        LoginPeer::new(LoginMode::Password).with_sftp(|request| {
            if request[0] == 1 {
                stat_reply(request)
            } else {
                reply(vec![])
            }
        }),
    );
    let mut session =
        SftpSession::open(&program(), &command(&f), Duration::from_millis(100)).unwrap();
    let start = Instant::now();
    assert_eq!(session.stat(".").unwrap_err().code, "sftp.connection_lost");
    drop(session);
    assert!(start.elapsed() < Duration::from_secs(2));
    f.manager.close_all().unwrap();
}

// 每个文件在内存中保留有界内容；八个数据请求收齐后逆序回包，检验实际 SSH 流上的流水线。
#[derive(Default)]
struct FilePeer {
    bytes: Vec<u8>,
    pending: Vec<Vec<u8>>,
}
impl FilePeer {
    fn status(id: &[u8], code: u32) -> Vec<u8> {
        let mut body = vec![101];
        body.extend(id);
        body.extend(code.to_be_bytes());
        body.extend([0; 8]);
        packet(&body)
    }
    fn respond(&mut self, request: &[u8]) -> ScriptReply {
        if request[0] == 1 {
            return stat_reply(request);
        }
        let id = &request[1..5];
        match request[0] {
            3 => {
                let mut body = vec![102];
                body.extend(id);
                body.extend([0, 0, 0, 1, b'h']);
                reply(packet(&body))
            }
            5 | 6 => {
                let offset = u64::from_be_bytes(request[10..18].try_into().unwrap()) as usize;
                let length = u32::from_be_bytes(request[18..22].try_into().unwrap()) as usize;
                assert!(offset <= 8 * 1024 * 1024 && length <= 32768);
                let response = if request[0] == 6 {
                    self.bytes.resize(self.bytes.len().max(offset + length), 0);
                    self.bytes[offset..offset + length].copy_from_slice(&request[22..]);
                    Self::status(id, 0)
                } else if offset >= self.bytes.len() {
                    Self::status(id, 1)
                } else {
                    let count = length.min(self.bytes.len() - offset);
                    let mut body = vec![103];
                    body.extend(id);
                    body.extend((count as u32).to_be_bytes());
                    body.extend(&self.bytes[offset..offset + count]);
                    packet(&body)
                };
                self.pending.push(response);
                if self.pending.len() == 8 {
                    reply(self.pending.drain(..).rev().flatten().collect())
                } else {
                    reply(vec![])
                }
            }
            7 | 17 => stat_reply(request),
            _ => reply(Self::status(id, 0)),
        }
    }
}

#[test]
fn 内置流水线逆序回包上传下载取消及断线错误保持一致() {
    let f = fixture();
    let state = Arc::new(Mutex::new(FilePeer::default()));
    let shared = state.clone();
    attach(
        &f,
        LoginPeer::new(LoginMode::Password)
            .with_sftp(move |request| shared.lock().unwrap().respond(request)),
    );
    let cmd = command(&f);
    let mut session = SftpSession::open(&program(), &cmd, Duration::from_secs(2)).unwrap();
    let bytes: Vec<_> = (0..32768 * 32).map(|n| (n % 251) as u8).collect();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        session
            .write_from("file", &mut &bytes[..], None, &mut |_| {}, &cancel)
            .unwrap(),
        bytes.len() as u64
    );
    let mut output = Vec::new();
    assert_eq!(
        session
            .read_to("file", &mut output, &mut |_| {}, &cancel)
            .unwrap(),
        bytes.len() as u64
    );
    assert_eq!(output, bytes);
    assert_eq!(
        session
            .write_from(
                "file",
                &mut &bytes[..],
                None,
                &mut |_| cancel.store(true, Ordering::Release),
                &cancel
            )
            .unwrap_err()
            .code,
        "sftp.cancelled"
    );
    assert!(session.stat(".").unwrap().is_some());
    cancel.store(false, Ordering::Release);
    assert_eq!(
        session
            .read_to(
                "file",
                &mut std::io::sink(),
                &mut |_| cancel.store(true, Ordering::Release),
                &cancel
            )
            .unwrap_err()
            .code,
        "sftp.cancelled"
    );
    assert!(session.stat(".").unwrap().is_some());
    cancel.store(false, Ordering::Release);
    let mut request = f.request(false);
    request.class = ConnectionClass::Bulk;
    let bulk = f.manager.get_connection(request).unwrap();
    assert_eq!(
        session
            .write_from(
                "file",
                &mut &bytes[..],
                None,
                &mut |_| {
                    let _ = bulk.disconnect();
                },
                &cancel
            )
            .unwrap_err()
            .code,
        "sftp.connection_lost"
    );
    assert_eq!(session.stat(".").unwrap_err().code, "sftp.connection_lost");
    f.manager.close_all().unwrap();
}
