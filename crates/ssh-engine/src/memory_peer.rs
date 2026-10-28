//! 内存双工流上的真实 SSH 协议测试，沙箱内不监听端口。
#[cfg(test)]
use crate::HostKeyCheck;
use crate::{Authentication, Connection, ConnectionOptions, Engine, Result};
#[cfg(test)]
use russh::keys::HashAlg;
use russh::{
    Channel, ChannelId,
    keys::{PrivateKey, ssh_key::private::Ed25519Keypair},
    server,
};
use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
pub(super) struct Peer {
    pub(super) closed_channels: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) terminal_requests: Arc<Mutex<Vec<crate::test_support::TerminalRequest>>>,
    pub(super) round: usize,
    pub(super) channels: HashMap<ChannelId, &'static str>,
    pub(super) malformed_sftp: bool,
    pub(super) silent_sftp: bool,
    pub(super) stop_replenishing: bool,
    pub(super) forwarded_flood: bool,
    pub(super) reject_forwarding: bool,
    pub(super) streamlocal_failure: Option<russh::ChannelOpenFailure>,
    pub(super) adjustments: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) adjusted_channels: Arc<Mutex<HashMap<ChannelId, usize>>>,
    pub(super) uploaded: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) writers: HashMap<ChannelId, Arc<russh::ChannelWriteHalf<server::Msg>>>,
    pub(super) received_sizes: Arc<Mutex<Vec<(u32, u32)>>>,
}

impl server::Handler for Peer {
    type Error = russh::Error;
    async fn auth_password(
        &mut self,
        _: &str,
        password: &str,
    ) -> std::result::Result<server::Auth, Self::Error> {
        Ok(if password == "测试密码" {
            server::Auth::Accept
        } else {
            server::Auth::reject()
        })
    }
    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _: &str,
        _: &str,
        response: Option<server::Response<'a>>,
    ) -> std::result::Result<server::Auth, Self::Error> {
        if let Some(mut response) = response {
            let expected = if self.round == 1 {
                b"password".as_slice()
            } else {
                b"123456".as_slice()
            };
            if response.next().as_deref() != Some(expected) {
                return Ok(server::Auth::reject());
            }
            if self.round == 2 {
                return Ok(server::Auth::Accept);
            }
        }
        self.round += 1;
        Ok(server::Auth::Partial {
            name: Cow::Borrowed("多轮认证"),
            instructions: Cow::Borrowed("测试"),
            prompts: Cow::Owned(vec![(
                Cow::Borrowed(if self.round == 1 {
                    "Password:"
                } else {
                    "Verification code:"
                }),
                false,
            )]),
        })
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        if self.channels.len() >= 2 {
            reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
        } else {
            self.channels.insert(channel.id(), "session");
            let (reader, writer) = channel.split();
            drop(reader);
            self.writers.insert(writer.id(), Arc::new(writer));
            reply.accept().await;
        }
        Ok(())
    }
    async fn channel_close(
        &mut self,
        id: ChannelId,
        session: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        self.closed_channels
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.channels.remove(&id);
        self.writers.remove(&id);
        session.close(id)
    }
    async fn env_request(
        &mut self,
        id: ChannelId,
        name: &str,
        value: &str,
        session: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        self.terminal_requests
            .lock()
            .unwrap()
            .push(crate::test_support::TerminalRequest::Env(
                name.into(),
                value.into(),
            ));
        if name == "ACCEPTED" {
            session.channel_success(id)
        } else {
            session.channel_failure(id)
        }
    }
    async fn exec_request(
        &mut self,
        id: ChannelId,
        command: &[u8],
        session: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        self.terminal_requests
            .lock()
            .unwrap()
            .push(crate::test_support::TerminalRequest::Exec(command.to_vec()));
        let command = command
            .strip_prefix(b"/bin/sh -c '")
            .and_then(|v| v.strip_suffix(b"'"))
            .unwrap_or(command);
        if command == b"signal" {
            session.channel_success(id)?;
            session.exit_signal_request(id, russh::Sig::TERM, false, "", "")?;
            session.eof(id)?;
            self.channels.remove(&id);
            return session.close(id);
        }
        if command == b"reject" {
            return session.channel_failure(id);
        }
        session.channel_success(id)?;
        if command == b"output" {
            session.data(id, b"stdout".to_vec())?;
            session.extended_data(id, 1, b"stderr".to_vec())?;
            session.exit_status_request(id, 7)?;
            session.eof(id)?;
            self.channels.remove(&id);
            session.close(id)?;
        } else if matches!(
            command,
            b"flood" | b"flood-large" | b"mixed-flood" | b"stderr-first"
        ) {
            self.channels.insert(id, "command");
            let writer = self.writers[&id].clone();
            let mixed = command == b"mixed-flood";
            let stderr_first = command == b"stderr-first";
            let blocks = if command == b"flood-large" { 256 } else { 128 };
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                for index in 0..blocks {
                    let mut tx =
                        writer.make_writer_ext(if stderr_first || mixed && index % 2 == 1 {
                            Some(1)
                        } else {
                            None
                        });
                    if tx.write_all(&vec![b'x'; 32 * 1024]).await.is_err() {
                        return;
                    }
                }
                if stderr_first && writer.data_bytes(b"stdout".as_slice()).await.is_err() {
                    return;
                }
                writer.exit_status(0).await.unwrap();
                writer.eof().await.unwrap();
                writer.close().await.unwrap();
            });
        } else if command == b"idle" {
            self.channels.insert(id, "idle");
        } else if command == b"blocked" {
            self.channels.insert(id, "discard");
            let handle = session.handle();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                handle.close(id).await.unwrap();
            });
        } else if command == b"delayed" {
            self.channels.insert(id, "command");
            let handle = session.handle();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                handle.data(id, b"late".to_vec()).await.unwrap();
                handle.exit_status_request(id, 9).await.unwrap();
                handle.eof(id).await.unwrap();
                handle.close(id).await.unwrap();
            });
        } else if command == b"upload" {
            self.channels.insert(id, "upload");
        } else {
            self.channels.insert(id, "echo");
        }
        Ok(())
    }
    async fn shell_request(
        &mut self,
        id: ChannelId,
        session: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        self.terminal_requests
            .lock()
            .unwrap()
            .push(crate::test_support::TerminalRequest::Shell);
        self.channels.insert(id, "terminal");
        session.channel_success(id)
    }
    async fn pty_request(
        &mut self,
        id: ChannelId,
        term: &str,
        columns: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        session: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        self.received_sizes.lock().unwrap().push((columns, rows));
        self.terminal_requests
            .lock()
            .unwrap()
            .push(crate::test_support::TerminalRequest::Pty(
                term.into(),
                columns,
                rows,
            ));
        session.channel_success(id)
    }
    async fn window_change_request(
        &mut self,
        _: ChannelId,
        columns: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        self.received_sizes.lock().unwrap().push((columns, rows));
        self.terminal_requests
            .lock()
            .unwrap()
            .push(crate::test_support::TerminalRequest::Resize(columns, rows));
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        _: &str,
        session: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        self.channels.insert(id, "sftp");
        session.channel_success(id)
    }
    async fn data(
        &mut self,
        id: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        self.uploaded
            .fetch_add(data.len(), std::sync::atomic::Ordering::Relaxed);
        if !self.channels.contains_key(&id) {
            return Ok(());
        }
        if self.channels.get(&id) == Some(&"terminal") && data == b"\x1d" {
            return session.disconnect(russh::Disconnect::ByApplication, "测试对端断开", "");
        }
        if self.channels.get(&id) == Some(&"terminal") && data == b"\x04" {
            session.data(id, b"bye".to_vec())?;
            session.exit_status_request(id, 7)?;
            session.eof(id)?;
            self.channels.remove(&id);
            return session.close(id);
        }
        if self.channels.get(&id) == Some(&"upload") {
            return Ok(());
        }
        if matches!(
            self.channels.get(&id),
            Some(&"discard") | Some(&"idle") | Some(&"command")
        ) {
            return Ok(());
        }
        if self.channels.get(&id) == Some(&"sftp") {
            if self.silent_sftp {
                return Ok(());
            }
            assert_eq!(data, &[0, 0, 0, 5, 1, 0, 0, 0, 3]);
            let response = if self.malformed_sftp {
                vec![0xff; 4]
            } else {
                vec![0, 0, 0, 5, 2, 0, 0, 0, 3]
            };
            session.data(id, response)
        } else {
            session.data(id, data.to_vec())
        }
    }
    async fn window_adjusted(
        &mut self,
        id: ChannelId,
        _: u32,
        _: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        *self
            .adjusted_channels
            .lock()
            .unwrap()
            .entry(id)
            .or_default() += 1;
        self.adjustments
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
    fn adjust_window(&mut self, id: ChannelId, window: u32) -> u32 {
        if self.stop_replenishing || self.channels.get(&id) == Some(&"idle") {
            1
        } else {
            window
        }
    }

    async fn channel_eof(
        &mut self,
        id: ChannelId,
        session: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        // stdin EOF 只是半关闭；异步命令由输出任务发送退出状态、EOF 和 Close。
        if matches!(
            self.channels.get(&id),
            None | Some(&"idle") | Some(&"command")
        ) {
            return Ok(());
        }
        self.channels.remove(&id);
        session.exit_status_request(id, 0)?;
        session.eof(id)?;
        session.close(id)
    }
    async fn channel_open_direct_streamlocal(
        &mut self,
        channel: Channel<server::Msg>,
        path: &str,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        if let Some(reason) = &self.streamlocal_failure {
            reply.reject(reason.clone()).await;
            return Ok(());
        }
        if path == "delayed" {
            reply.accept().await;
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut stream = channel.into_stream();
                tokio::time::sleep(Duration::from_millis(300)).await;
                stream.write_all(b"late").await.unwrap();
                stream.shutdown().await.unwrap();
            });
            return Ok(());
        }
        self.channels.insert(channel.id(), "echo");
        reply.accept().await;
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<server::Msg>,
        host: &str,
        _: u32,
        _: &str,
        _: u32,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        if matches!(host, "echo" | "flood" | "late-echo") {
            let flood = host == "flood";
            let late = host == "late-echo";
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                if late {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                reply.accept().await;
                let mut stream = channel.into_stream();
                if flood {
                    let _ = stream.write_all(&vec![b'x'; 4 * 1024 * 1024]).await;
                } else {
                    let (mut reader, mut writer) = tokio::io::split(stream);
                    let _ = tokio::io::copy(&mut reader, &mut writer).await;
                    let _ = writer.shutdown().await;
                }
            });
            return Ok(());
        }
        let seed = match host {
            "hop2" => 2,
            "hop3" => 3,
            "hop4" => 4,
            _ => {
                reply
                    .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                    .await;
                return Ok(());
            }
        };
        reply.accept().await;
        let mut config = peer_config(Ed25519Keypair::from_seed(&[seed; 32]).into());
        config.methods = russh::MethodSet::from(&[russh::MethodKind::Password][..]);
        tokio::spawn(async move {
            if let Ok(session) =
                server::run_stream(Arc::new(config), channel.into_stream(), Peer::default()).await
            {
                let _ = session.await;
            }
        });
        Ok(())
    }

    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut server::Session,
    ) -> std::result::Result<bool, Self::Error> {
        assert_eq!(address, "127.0.0.1");
        if self.reject_forwarding {
            return Ok(false);
        }
        if *port == 0 {
            *port = 32123;
        }
        if self.stop_replenishing {
            return Ok(true);
        }
        let port = *port;
        let flood = self.forwarded_flood;
        let handle = session.handle();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Ok(channel) = handle
                .channel_open_forwarded_tcpip("127.0.0.1", port, "127.0.0.1", 12345)
                .await
            {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut stream = channel.into_stream();
                if flood {
                    let _ = stream.write_all(&vec![b'f'; 4 * 1024 * 1024]).await;
                    return;
                }
                let _ = stream.write_all(b"forwarded").await;
                let mut response = [0; 4];
                stream.read_exact(&mut response).await.unwrap();
                assert_eq!(&response, b"pong");
            }
        });
        Ok(true)
    }

    async fn cancel_tcpip_forward(
        &mut self,
        address: &str,
        port: u32,
        _: &mut server::Session,
    ) -> std::result::Result<bool, Self::Error> {
        assert_eq!(address, "127.0.0.1");
        assert_ne!(port, 0);
        Ok(true)
    }
}

pub(super) fn peer_config(key: PrivateKey) -> server::Config {
    server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        // 服务器端保活：太短会在满负载的测试机上误判客户端已死、在认证途中断开（Windows CI 上见过）
        keepalive_interval: Some(Duration::from_secs(1)),
        keepalive_max: 3,
        limits: russh::Limits::new(64 * 1024, 64 * 1024, Duration::from_secs(60)),
        ..Default::default()
    }
}

#[cfg(test)]
pub(super) fn start(
    engine: &Engine,
    peer: Peer,
    auth: Authentication,
    check: Option<HostKeyCheck>,
) -> Result<Connection> {
    start_options(
        engine,
        peer,
        auth,
        check,
        Duration::from_secs(5),
        256 * 1024,
    )
}

#[cfg(test)]
pub(super) fn start_options(
    engine: &Engine,
    peer: Peer,
    auth: Authentication,
    check: Option<HostKeyCheck>,
    timeout: Duration,
    window_size: u32,
) -> Result<Connection> {
    let key: PrivateKey = Ed25519Keypair::from_seed(&[7; 32]).into();
    let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
    let mut config = peer_config(key);
    if peer.stop_replenishing {
        config.window_size = 32 * 1024;
    }
    let (client, server) = tokio::io::duplex(64 * 1024);
    engine.executor.handle()?.spawn(async move {
        if let Ok(session) = server::run_stream(Arc::new(config), server, peer).await {
            let _ = session.await;
        }
    });
    let mut options = ConnectionOptions::new(
        "内存测试",
        22,
        "测试",
        check.unwrap_or(HostKeyCheck::Fingerprint(fingerprint)),
    );
    options.io_timeout = timeout;
    options.window_size = window_size;
    engine.connect_stream(options, auth, client, None)
}

#[cfg(test)]
pub(super) fn password() -> Authentication {
    Authentication::Password("测试密码".into())
}

/// 在内存双工流上建立支持会话上限、双输出、延时和嵌套跳板的测试连接。
pub(crate) fn connect(
    engine: &Engine,
    options: ConnectionOptions,
    auth: Authentication,
) -> Result<Connection> {
    connect_peer(engine, options, auth, Peer::default(), 64 * 1024)
}

pub(super) fn connect_peer(
    engine: &Engine,
    options: ConnectionOptions,
    auth: Authentication,
    peer: Peer,
    wire_capacity: usize,
) -> Result<Connection> {
    let mut config = peer_config(Ed25519Keypair::from_seed(&[7; 32]).into());
    config.methods = russh::MethodSet::from(&[russh::MethodKind::Password][..]);
    let (client, server) = tokio::io::duplex(wire_capacity);
    engine.executor.handle()?.spawn(async move {
        if let Ok(session) = server::run_stream(Arc::new(config), server, peer).await {
            let _ = session.await;
        }
    });
    engine.connect_stream(options, auth, client, None)
}
