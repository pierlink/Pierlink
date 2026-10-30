//! 仅供协议测试启用的内存对端，跨 core 与引擎验证真实登录流程。
use crate::{Authentication, Connection, ConnectionOptions, Engine, Error, Result};
use russh::{
    MethodKind, MethodSet,
    keys::{PrivateKey, PublicKey, ssh_key::private::Ed25519Keypair},
    server,
};
use std::{
    borrow::Cow,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

/// 内存终端对端实际收到的协议请求；只记录测试参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalRequest {
    Env(String, String),
    Pty(String, u32, u32),
    Shell,
    Exec(Vec<u8>),
    Resize(u32, u32),
}

/// 按 options 和 auth 建立回显终端对端，将 PTY、环境、shell、exec 和尺寸请求写入 requests。
/// sshid 指定服务端 SSH 版本字符串，为 None 时使用默认值。
/// 返回真实已认证的内存连接；不启动本机进程或监听端口。
pub fn connect_terminal(
    engine: &Engine,
    options: ConnectionOptions,
    auth: Authentication,
    requests: Arc<Mutex<Vec<TerminalRequest>>>,
    sshid: Option<&str>,
) -> Result<Connection> {
    // 小传输缓冲确保终端回显与换钥在写入背压下仍能推进。
    crate::memory_peer::connect_peer(
        engine,
        options,
        auth,
        crate::memory_peer::Peer {
            server_id: sshid.map(str::to_owned),
            terminal_requests: requests,
            ..Default::default()
        },
        64 * 1024,
    )
}

/// 测试对端允许的认证方式；不会接受其他方式。
#[derive(Clone)]
pub enum LoginMode {
    Password,
    /// 声明 KI 但在提出问题前拒绝，用于验证 password 的安全回退。
    UnavailableKeyboard,
    KeyboardPassword,
    PasswordAndOtp,
    OtpAndPassword,
    InformationAndPassword,
    PublicKey(PublicKey),
    /// 收到密码后直接断开连接，模拟认证途中网络断了或服务器的登录时限到了。
    DisconnectOnPassword,
}

/// 不监听端口的 SSH 测试对端，记录实际收到的认证请求，不记录回答。
#[derive(Clone)]
pub struct LoginPeer {
    mode: LoginMode,
    auth_limit: Option<(usize, Option<(u32, String)>)>,
    offered: usize,
    public_key_requests: Arc<std::sync::atomic::AtomicUsize>,
    handle: Arc<Mutex<Option<server::Handle>>>,
    attempts: Arc<Mutex<Vec<&'static str>>>,
    round: usize,
    answers_valid: bool,
    host_keys: Vec<PrivateKey>,
    scripts: Option<Arc<ScriptHandler>>,
    forwarding: bool,
    sftp: Option<Arc<ScriptHandler>>,
    sftp_channels: std::collections::HashSet<russh::ChannelId>,
    channels: std::collections::HashMap<russh::ChannelId, Vec<u8>>,
    writers: std::collections::HashMap<russh::ChannelId, Arc<russh::ChannelWriteHalf<server::Msg>>>,
}

type ScriptHandler = dyn Fn(&[u8]) -> ScriptReply + Send + Sync;

/// 测试脚本的双输出和退出码；None 保持通道打开，供超时和监控取消测试使用。
pub struct ScriptReply {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<u32>,
}

impl LoginPeer {
    /// 按 mode 创建对端；返回没有建立任何连接的测试状态。
    pub fn new(mode: LoginMode) -> Self {
        Self {
            mode,
            auth_limit: None,
            offered: 0,
            public_key_requests: Arc::default(),
            handle: Arc::default(),
            attempts: Arc::new(Mutex::new(Vec::new())),
            round: 0,
            answers_valid: true,
            host_keys: vec![Ed25519Keypair::from_seed(&[17; 32]).into()],
            scripts: None,
            forwarding: false,
            sftp: None,
            sftp_channels: Default::default(),
            channels: Default::default(),
            writers: Default::default(),
        }
    }

    /// 在第 limit 次公钥探询时断开；reason 为 None 时直接关闭传输，否则发送指定 DISCONNECT。
    /// 返回配置后的对端；此前的陌生公钥均收到真实拒绝。
    pub fn with_auth_limit(mut self, limit: usize, reason: Option<(u32, String)>) -> Self {
        self.auth_limit = Some((limit, reason));
        self
    }

    /// 用 keys 替换服务器测试主机密钥；返回可协商多种算法的内存对端。
    pub fn with_host_keys(mut self, keys: Vec<PrivateKey>) -> Self {
        self.host_keys = keys;
        self
    }

    /// 设置 handler 接收 /bin/sh -s 的完整标准输入并返回测试输出；返回配置后的内存对端。
    pub fn with_scripts(
        mut self,
        handler: impl Fn(&[u8]) -> ScriptReply + Send + Sync + 'static,
    ) -> Self {
        self.scripts = Some(Arc::new(handler));
        self
    }

    /// 无参数；返回允许确认回环远程监听的对端，不实际创建本机监听端口。
    pub fn with_forwarding(mut self) -> Self {
        self.forwarding = true;
        self
    }

    /// 设置 handler 接收完整 SFTP 包体并返回原始双输出；输出须自行添加包长度。
    /// 返回内存子系统对端；None 退出码保持通道打开，未设置时拒绝子系统请求。
    pub fn with_sftp(
        mut self,
        handler: impl Fn(&[u8]) -> ScriptReply + Send + Sync + 'static,
    ) -> Self {
        self.sftp = Some(Arc::new(handler));
        self
    }

    /// 读取 path 的测试私钥（phrase 为可选口令），返回接受其公钥的对端。
    pub fn key(path: &Path, phrase: Option<&str>) -> Result<Self> {
        let key = russh::keys::load_secret_key(path, phrase).map_err(|_| Error::PrivateKey)?;
        Ok(Self::new(LoginMode::PublicKey(key.public_key().clone())))
    }

    /// 无参数；返回服务器收到的公钥探询次数，包括随后进行签名认证的身份。
    pub fn public_key_requests(&self) -> usize {
        self.public_key_requests
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 返回已接收的认证方法快照，不包含秘密或服务器文本。
    pub fn attempts(&self) -> Vec<&'static str> {
        self.attempts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// 在 engine 上建立内存流并执行 options 和 auth，返回真实已登录连接或引擎错误。
    pub fn connect(
        &self,
        engine: &Engine,
        options: ConnectionOptions,
        auth: Authentication,
    ) -> Result<Connection> {
        let methods: &[MethodKind] = match self.mode {
            LoginMode::Password => &[MethodKind::PublicKey, MethodKind::Password],
            LoginMode::UnavailableKeyboard
            | LoginMode::KeyboardPassword
            | LoginMode::PasswordAndOtp
            | LoginMode::OtpAndPassword
            | LoginMode::InformationAndPassword => {
                &[MethodKind::KeyboardInteractive, MethodKind::Password]
            }
            LoginMode::PublicKey(_) => &[MethodKind::PublicKey, MethodKind::Password],
            LoginMode::DisconnectOnPassword => &[MethodKind::Password],
        };
        let config = server::Config {
            keys: self.host_keys.clone(),
            methods: MethodSet::from(methods),
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        };
        let (client, server) = tokio::io::duplex(64 * 1024);
        let mut peer = self.clone();
        peer.handle = Arc::default();
        let handle = peer.handle.clone();
        engine.executor.handle()?.spawn(async move {
            if let Ok(session) = server::run_stream(Arc::new(config), server, peer).await {
                *handle.lock().unwrap_or_else(|p| p.into_inner()) = Some(session.handle());
                let _ = session.await;
                handle.lock().unwrap_or_else(|p| p.into_inner()).take();
            }
        });
        engine.connect_stream(options, auth, client, None)
    }

    /// 以 paths 中的无口令测试密钥构造内存 agent，执行 auth 的列举和签名流程。
    /// 返回真实已登录连接或脱敏引擎错误；只替换 agent 的传输流。
    pub fn connect_with_agent(
        &self,
        engine: &Engine,
        options: ConnectionOptions,
        auth: Authentication,
        paths: &[std::path::PathBuf],
    ) -> Result<Connection> {
        let Authentication::Configured(config) = auth else {
            return Err(Error::InvalidInput);
        };
        let keys = paths
            .iter()
            .map(|path| russh::keys::load_secret_key(path, None).map_err(|_| Error::PrivateKey))
            .collect::<Result<Vec<_>>>()?;
        let (client, server) = tokio::io::duplex(64 * 1024);
        engine.executor.handle()?.spawn(async move {
            let _ = memory_agent(server, keys).await;
        });
        self.connect(
            engine,
            options,
            Authentication::MemoryAgent(config, Arc::new(Mutex::new(Some(client)))),
        )
    }

    fn reply_script(&mut self, id: russh::ChannelId) {
        if let (Some(handler), Some(script), Some(writer)) = (
            &self.scripts,
            self.channels.remove(&id),
            self.writers.get(&id),
        ) {
            let reply = handler(&script);
            let writer = writer.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                for (bytes, stream) in [(reply.stdout, None), (reply.stderr, Some(1))] {
                    if writer
                        .make_writer_ext(stream)
                        .write_all(&bytes)
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                if let Some(code) = reply.exit_code {
                    let _ = writer.exit_status(code).await;
                    let _ = writer.eof().await;
                    let _ = writer.close().await;
                }
            });
        }
    }

    fn record(&self, method: &'static str) {
        self.attempts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(method);
    }
}

impl server::Handler for LoginPeer {
    type Error = russh::Error;

    async fn tcpip_forward(
        &mut self,
        address: &str,
        _: &mut u32,
        _: &mut server::Session,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(self.forwarding && address == "127.0.0.1")
    }

    async fn cancel_tcpip_forward(
        &mut self,
        address: &str,
        _: u32,
        _: &mut server::Session,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(self.forwarding && address == "127.0.0.1")
    }

    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        if self.scripts.is_none() && self.sftp.is_none() {
            reply
                .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        let (reader, writer) = channel.split();
        drop(reader);
        self.channels.insert(writer.id(), Vec::new());
        self.writers.insert(writer.id(), Arc::new(writer));
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        id: russh::ChannelId,
        name: &str,
        session: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        if name == "sftp" && self.sftp.is_some() {
            self.sftp_channels.insert(id);
            session.channel_success(id)
        } else {
            session.channel_failure(id)
        }
    }

    async fn exec_request(
        &mut self,
        id: russh::ChannelId,
        command: &[u8],
        session: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        // 后台命令固定经 POSIX sh 执行，脚本不能拼入 exec 请求。
        if command == br#"/bin/sh -c ''"'"'/bin/sh'"'"' '"'"'-s'"'"''"# {
            session.channel_success(id)
        } else {
            session.channel_failure(id)
        }
    }

    async fn data(
        &mut self,
        id: russh::ChannelId,
        bytes: &[u8],
        session: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        if self.sftp_channels.contains(&id) {
            let buffer = self.channels.entry(id).or_default();
            buffer.extend_from_slice(bytes);
            if buffer.len() > 2 * 1024 * 1024 {
                return Err(russh::Error::Inconsistent);
            }
            while buffer.len() >= 4 {
                let length = u32::from_be_bytes(
                    buffer[..4]
                        .try_into()
                        .map_err(|_| russh::Error::Inconsistent)?,
                ) as usize;
                if length > 1024 * 1024 {
                    return Err(russh::Error::Inconsistent);
                }
                if buffer.len() < length + 4 {
                    break;
                }
                if let Some(handler) = &self.sftp {
                    let reply = handler(&buffer[4..4 + length]);
                    if !reply.stderr.is_empty() {
                        session.extended_data(id, 1, reply.stderr)?;
                    }
                    if !reply.stdout.is_empty() {
                        session.data(id, reply.stdout)?;
                    }
                    if let Some(code) = reply.exit_code {
                        session.exit_status_request(id, code)?;
                        session.eof(id)?;
                        session.close(id)?;
                    }
                }
                buffer.drain(..4 + length);
            }
            return Ok(());
        }
        if let Some(script) = self.channels.get_mut(&id) {
            if script.len().saturating_add(bytes.len()) > 8 * 1024 * 1024 {
                return Err(russh::Error::Inconsistent);
            }
            script.extend_from_slice(bytes);
            if script.ends_with(b"plmon_cleanup\n}\n") {
                self.reply_script(id);
            }
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        id: russh::ChannelId,
        _: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        self.reply_script(id);
        Ok(())
    }

    async fn channel_close(
        &mut self,
        id: russh::ChannelId,
        session: &mut server::Session,
    ) -> std::result::Result<(), Self::Error> {
        self.sftp_channels.remove(&id);
        self.channels.remove(&id);
        self.writers.remove(&id);
        session.close(id)
    }

    async fn auth_none(&mut self, _: &str) -> std::result::Result<server::Auth, Self::Error> {
        self.record("none");
        Ok(server::Auth::reject())
    }

    async fn auth_password(
        &mut self,
        _: &str,
        password: &str,
    ) -> std::result::Result<server::Auth, Self::Error> {
        self.record("password");
        if matches!(self.mode, LoginMode::DisconnectOnPassword) {
            // 处理函数返回错误时 russh 服务端结束会话、关闭传输流
            return Err(russh::Error::Disconnect);
        }
        Ok(
            if matches!(
                self.mode,
                LoginMode::Password | LoginMode::UnavailableKeyboard
            ) && password == "测试密码"
            {
                server::Auth::Accept
            } else {
                server::Auth::reject()
            },
        )
    }

    async fn auth_publickey_offered(
        &mut self,
        _: &str,
        key: &PublicKey,
    ) -> std::result::Result<server::Auth, Self::Error> {
        self.public_key_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.offered += 1;
        if matches!(&self.mode, LoginMode::PublicKey(expected) if key.key_data() == expected.key_data())
        {
            return Ok(server::Auth::Accept);
        }
        if let Some((limit, reason)) = &self.auth_limit {
            if self.offered >= *limit {
                let Some((code, message)) = reason else {
                    return Err(russh::Error::Disconnect);
                };
                let handle = self
                    .handle
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
                    .ok_or(russh::Error::Disconnect)?;
                handle
                    .disconnect(
                        russh::Disconnect::try_from(*code)?,
                        message.clone(),
                        String::new(),
                    )
                    .await
                    .map_err(|_| russh::Error::Disconnect)?;
                // 先排队断开，不再发送 USERAUTH_FAILURE；PK_OK 后签名请求无法完成登录。
                return Ok(server::Auth::Accept);
            }
            return Ok(server::Auth::reject());
        }
        // 保留既有测试对端先验签再拒绝的行为。
        Ok(server::Auth::Accept)
    }

    async fn auth_publickey(
        &mut self,
        _: &str,
        key: &PublicKey,
    ) -> std::result::Result<server::Auth, Self::Error> {
        self.record("publickey");
        Ok(
            if matches!(&self.mode, LoginMode::PublicKey(expected) if key.key_data() == expected.key_data())
            {
                server::Auth::Accept
            } else {
                server::Auth::reject()
            },
        )
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _: &str,
        _: &str,
        response: Option<server::Response<'a>>,
    ) -> std::result::Result<server::Auth, Self::Error> {
        self.record("keyboard-interactive");
        if matches!(self.mode, LoginMode::UnavailableKeyboard) {
            return Ok(server::Auth::reject());
        }
        let rounds: &[(&str, &str)] = match self.mode {
            LoginMode::KeyboardPassword => &[("PaSsWoRd:", "测试密码")],
            LoginMode::OtpAndPassword => {
                &[("Verification code:", "123456"), ("PaSsWoRd:", "测试密码")]
            }
            LoginMode::InformationAndPassword => &[("", ""), ("PaSsWoRd:", "测试密码")],
            _ => &[("PaSsWoRd:", "测试密码"), ("Verification code:", "123456")],
        };
        if let Some(mut response) = response {
            let (prompt, expected) = rounds[self.round - 1];
            self.answers_valid &= if prompt.is_empty() {
                response.next().is_none()
            } else {
                response.next().as_deref() == Some(expected.as_bytes()) && response.next().is_none()
            };
            // 模拟 PAM：密码错误也继续询问验证码，最后统一拒绝。
            if self.round == rounds.len() {
                return Ok(if self.answers_valid {
                    server::Auth::Accept
                } else {
                    server::Auth::reject()
                });
            }
        }
        let (prompt, _) = rounds[self.round];
        self.round += 1;
        Ok(server::Auth::Partial {
            name: Cow::Borrowed("测试认证"),
            instructions: Cow::Borrowed("不可信提示"),
            prompts: Cow::Owned(if prompt.is_empty() {
                vec![]
            } else {
                vec![(Cow::Borrowed(prompt), false)]
            }),
        })
    }
}

/// 用 stream 接收列举和签名请求，keys 只包含测试私钥；连接结束时返回传输错误。
pub(crate) async fn memory_agent(
    mut stream: tokio::io::DuplexStream,
    keys: Vec<PrivateKey>,
) -> Result<()> {
    use russh::keys::{signature::Signer, ssh_encoding::Encode};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let length = stream.read_u32().await? as usize;
        if length > 256 * 1024 {
            return Err(Error::Protocol);
        }
        let mut request = vec![0; length];
        stream.read_exact(&mut request).await?;
        let mut response = Vec::new();
        match request.first() {
            Some(11) => {
                response.push(12);
                response.extend_from_slice(&(keys.len() as u32).to_be_bytes());
                for key in &keys {
                    let bytes = key.public_key().to_bytes().map_err(|_| Error::PrivateKey)?;
                    agent_string(&mut response, &bytes);
                    agent_string(&mut response, b"");
                }
            }
            Some(13) => {
                let mut request = &request[1..];
                let public = agent_read_string(&mut request)?;
                let data = agent_read_string(&mut request)?;
                let mut signed = None;
                for key in &keys {
                    if key.public_key().to_bytes().map_err(|_| Error::PrivateKey)? == public {
                        let signature = key.try_sign(data).map_err(|_| Error::Agent)?;
                        let mut bytes = Vec::new();
                        signature.encode(&mut bytes).map_err(|_| Error::Agent)?;
                        signed = Some(bytes);
                        break;
                    }
                }
                if let Some(signature) = signed {
                    response.push(14);
                    agent_string(&mut response, &signature);
                } else {
                    response.push(5);
                }
            }
            _ => response.push(5),
        }
        stream.write_u32(response.len() as u32).await?;
        stream.write_all(&response).await?;
    }
}

fn agent_string(buffer: &mut Vec<u8>, value: &[u8]) {
    buffer.extend_from_slice(&(value.len() as u32).to_be_bytes());
    buffer.extend_from_slice(value);
}

fn agent_read_string<'a>(buffer: &mut &'a [u8]) -> Result<&'a [u8]> {
    let length = buffer.get(..4).ok_or(Error::Protocol)?;
    let length = u32::from_be_bytes(length.try_into().map_err(|_| Error::Protocol)?) as usize;
    let value = buffer.get(4..4 + length).ok_or(Error::Protocol)?;
    *buffer = &buffer[4 + length..];
    Ok(value)
}

/// 按 options 和 auth 建立内存对端；每连接允许两个会话，支持跳板与可控 exec 输出。
/// 返回真实 SSH 连接，不监听端口。
pub fn connect_sessions(
    engine: &Engine,
    options: ConnectionOptions,
    auth: Authentication,
) -> Result<Connection> {
    crate::memory_peer::connect(engine, options, auth)
}

/// 按 options 和 auth 建立仅确认远程监听的内存对端，不主动发起本机网络连接。
/// 返回可测试转发状态、共享连接和断线恢复的已认证连接。
pub fn connect_forwarding(
    engine: &Engine,
    options: ConnectionOptions,
    auth: Authentication,
) -> Result<Connection> {
    crate::memory_peer::connect_peer(
        engine,
        options,
        auth,
        crate::memory_peer::Peer {
            stop_replenishing: true,
            ..Default::default()
        },
        64 * 1024,
    )
}
