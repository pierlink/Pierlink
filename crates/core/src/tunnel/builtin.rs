//! 内置隧道上下文与一次连接尝试；退避和状态仍由隧道调度器统一管理。
use super::*;
use crate::{
    builtin::{
        ConnectionCallbacks, ConnectionClass, ConnectionManager, ConnectionRequest,
        ConnectionState, StateSubscription,
        error::{Operation, map_engine_error},
    },
    security::vault::{PasswordVault, VaultSubscription},
    ssh::engine::{Engine, Feature, engine_for_feature},
    storage::Server,
};
use pierlink_ssh_engine::{Forwarding, ForwardingRule, ForwardingTarget};

/// 可跨线程保存的内置转发上下文；只持有密码库引用，不保存明文凭据。
#[derive(Clone)]
pub struct BuiltinTunnel {
    manager: ConnectionManager,
    server: Server,
    vault: Arc<PasswordVault>,
    callbacks: Arc<dyn ConnectionCallbacks>,
    class: ConnectionClass,
    cancelled: Arc<AtomicBool>,
}

/// 按 request.server 和 feature 选择引擎，返回附内置上下文的 spec 或原 OpenSSH spec。
/// 保留 request.class；后台转发始终禁用交互式询问，不受 request.interactive 影响。
pub fn builtin_spec(
    manager: &ConnectionManager,
    request: ConnectionRequest<'_>,
    feature: Feature,
    mut spec: TunnelSpec,
) -> Result<TunnelSpec, AppError> {
    if engine_for_feature(request.server, feature).engine == Engine::OpenSsh {
        return Ok(spec);
    }
    if spec.server_id != request.server.id {
        return Err(AppError::new("tunnel.invalid_spec").with_reason(
            "tunnel.tunnel_and_connection_request_refer_to_different_servers",
            "隧道与连接请求的服务器不一致",
        ));
    }
    spec.builtin = Some(Arc::new(BuiltinTunnel {
        manager: manager.clone(),
        server: request.server.clone(),
        vault: request.vault,
        callbacks: request.callbacks,
        class: request.class,
        cancelled: request.cancelled,
    }));
    Ok(spec)
}

pub(super) fn validate(spec: &TunnelSpec) -> Result<(), AppError> {
    if spec.forwards.len() > 64 {
        return Err(AppError::new("tunnel.invalid_spec"));
    }
    let mut remote_ports = Vec::new();
    for forward in &spec.forwards {
        match forward {
            TunnelForward::Local { remote, .. } => {
                crate::ssh::args::forward_target(remote)?;
            }
            TunnelForward::Remote {
                remote_port,
                local_port,
            } => {
                if *remote_port == 0 || *local_port == 0 {
                    return Err(AppError::new("tunnel.invalid_port"));
                }
                if remote_ports.contains(remote_port) {
                    return Err(AppError::new("tunnel.duplicate_port"));
                }
                remote_ports.push(*remote_port);
            }
            TunnelForward::Dynamic { local_port } if *local_port == 0 => {
                return Err(AppError::new("tunnel.invalid_port"));
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) struct Watch {
    passwords: Vec<(String, VaultSubscription)>,
    password_servers: Vec<String>,
    waiting_server: Option<String>,
    waiting_login: bool,
    states: StateSubscription,
    context: Arc<BuiltinTunnel>,
    pub(super) waiting: bool,
}
impl Watch {
    pub(super) fn new(context: Arc<BuiltinTunnel>) -> Result<Self, AppError> {
        let chain = context.manager.chain(&context.server)?;
        let password_servers = chain
            .iter()
            .filter(|server| {
                matches!(
                    server.auth_method,
                    crate::storage::AuthMethod::Password | crate::storage::AuthMethod::Agent
                )
            })
            .map(|server| server.id.clone())
            .collect();
        let passwords = chain
            .into_iter()
            .map(|s| (s.id.clone(), context.vault.subscribe(&s.id)))
            .collect();
        Ok(Self {
            passwords,
            password_servers,
            waiting_server: None,
            waiting_login: false,
            states: context.manager.subscribe_states(),
            context,
            waiting: false,
        })
    }
    pub(super) fn wait_for(&mut self, error: &AppError) -> bool {
        let server = error
            .params
            .get("server_id")
            .unwrap_or(&self.context.server.id);
        if error.code == "ssh.two_factor_required"
            || error.code == "ssh.password_needed"
            || error.code == "ssh.auth_failed" && self.password_servers.contains(server)
        {
            self.waiting = true;
            self.waiting_server = Some(server.clone());
            self.waiting_login = error.code == "ssh.two_factor_required";
            true
        } else {
            false
        }
    }
    pub(super) fn available(&self) -> bool {
        let mut ready = false;
        for (id, subscription) in &self.passwords {
            ready |= subscription.wait(Duration::ZERO)
                // 验证码不入库；更新密码不能代替主连接完成交互登录。
                && !self.waiting_login
                && self.context.vault.has(id)
                && self.waiting_server.as_ref() == Some(id);
        }
        while let Some(event) = self.states.wait(Duration::ZERO) {
            ready |= event.state == ConnectionState::Connected
                && self.waiting_server.as_ref() == Some(&event.server_id);
        }
        ready
    }
}

pub(super) struct Attempt {
    cancelled: Arc<AtomicBool>,
    result: Receiver<Result<Forwarding, AppError>>,
    worker: Option<JoinHandle<()>>,
    pub(super) forwarding: Option<Forwarding>,
    pub(super) ready: bool,
    pub(super) started: Instant,
}
impl Attempt {
    pub(super) fn start(
        context: Arc<BuiltinTunnel>,
        spec: &TunnelSpec,
        ports: &[u16],
    ) -> Result<Self, AppError> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let forwards = spec.forwards.clone();
        let ports = ports.to_vec();
        let (tx, result) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("内置隧道连接".into())
            .spawn(move || {
                let result = (|| {
                    let connection = context.manager.get_connection(ConnectionRequest {
                        progress: None,
                        server: &context.server,
                        class: context.class,
                        vault: context.vault.clone(),
                        callbacks: context.callbacks.clone(),
                        interactive: false,
                        cancelled: stop.clone(),
                    })?;
                    if stop.load(Ordering::Acquire) || context.cancelled.load(Ordering::Acquire) {
                        return Err(AppError::new("ssh.auth_cancelled"));
                    }
                    let mut ports = ports.into_iter();
                    let mut rules = Vec::new();
                    for forward in forwards {
                        rules.push(match forward {
                            TunnelForward::Local { remote, .. } => {
                                let listener = bind(ports.next())?;
                                let target = match remote {
                                    ForwardTarget::Tcp { port } => ForwardingTarget::Tcp {
                                        host: "127.0.0.1".into(),
                                        port,
                                    },
                                    ForwardTarget::Host { host, port } => ForwardingTarget::Tcp {
                                        host: host
                                            .trim_start_matches('[')
                                            .trim_end_matches(']')
                                            .into(),
                                        port,
                                    },
                                    ForwardTarget::Unix { path } => ForwardingTarget::Unix { path },
                                };
                                ForwardingRule::Local { listener, target }
                            }
                            TunnelForward::Dynamic { .. } => ForwardingRule::Dynamic {
                                listener: bind(ports.next())?,
                            },
                            TunnelForward::Remote {
                                remote_port,
                                local_port,
                            } => ForwardingRule::Remote {
                                remote_port,
                                local_port,
                            },
                        });
                    }
                    connection.forwarding(rules).map_err(map_error)
                })();
                let _ = tx.send(result);
            })
            .map_err(|_| io_error("tunnel.create_built_in_tunnel_thread", "创建内置隧道线程"))?;
        Ok(Self {
            cancelled,
            result,
            worker: Some(worker),
            forwarding: None,
            ready: false,
            started: Instant::now(),
        })
    }
    pub(super) fn poll(&mut self) -> Result<bool, AppError> {
        if self.forwarding.is_none() {
            match self.result.try_recv() {
                Ok(result) => self.forwarding = Some(result?),
                Err(mpsc::TryRecvError::Empty) => return Ok(false),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(io_error(
                        "tunnel.built_in_tunnel_connection_thread_exited",
                        "内置隧道连接线程退出",
                    ));
                }
            }
        }
        self.forwarding
            .as_ref()
            .ok_or_else(|| io_error("tunnel.read_built_in_tunnel", "读取内置隧道"))?
            .status()
            .map_err(map_error)
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.forwarding.take();
        if let Some(worker) = self.worker.take() {
            crate::process::join_bounded(worker, Duration::from_secs(1));
        }
    }
}
fn bind(port: Option<u16>) -> Result<TcpListener, AppError> {
    let port = port.ok_or_else(|| AppError::new("tunnel.invalid_spec"))?;
    TcpListener::bind(("127.0.0.1", port)).map_err(|e| bind_error(port, e.kind()))
}
pub(super) fn bind_error(port: u16, kind: std::io::ErrorKind) -> AppError {
    match kind {
        std::io::ErrorKind::PermissionDenied => {
            AppError::new("tunnel.port_reserved").with("port", port)
        }
        std::io::ErrorKind::AddrInUse => port_error(port),
        _ => AppError::new("tunnel.port_unavailable").with("port", port),
    }
}
pub(super) fn map_error(error: pierlink_ssh_engine::Error) -> AppError {
    if error == pierlink_ssh_engine::Error::RequestRejected("远程转发") {
        AppError::new("tunnel.remote_forward_failed")
            .with_reason(
                "tunnel.server_refused_remote_listening_port_may_be_in_use_or",
                "服务器拒绝远程监听，可能端口已被占用或禁止转发",
            )
            .with_text(
                "stderr_tail",
                "tunnel.server_refused_remote_listening_port_may_be_in_use_or",
                "服务器拒绝远程监听，可能端口已被占用或禁止转发",
            )
    } else {
        map_engine_error(&error, Operation::OpenForward)
    }
}

impl Entry {
    pub(super) fn builtin_cancelled(&self) -> bool {
        self.spec
            .builtin
            .as_ref()
            .is_some_and(|c| c.cancelled.load(Ordering::Acquire))
    }
    pub(super) fn poll_builtin(&mut self, events: &Events) -> Result<(), AppError> {
        if self.builtin_cancelled() {
            return Err(AppError::new("ssh.auth_cancelled"));
        }
        let Some(attempt) = self.builtin_connection.as_mut() else {
            return Ok(());
        };
        if attempt.poll()? && !attempt.ready {
            attempt.ready = true;
            self.binding_retries = 0;
            self.event.last_connected_at_ms = Some(
                now_ms().max(
                    self.event
                        .last_connected_at_ms
                        .map_or(0, |last| last.saturating_add(1)),
                ),
            );
            self.change(TunnelState::Connected, None, false, events);
        }
        if let Some(attempt) = &self.builtin_connection {
            if attempt.ready && attempt.started.elapsed() >= Duration::from_secs(3) {
                self.event.attempt = 0;
            }
            if let Some((error, streamlocal)) = attempt
                .forwarding
                .as_ref()
                .and_then(Forwarding::take_client_error)
            {
                // 单个目标拒绝不拆除其他转发，但把可翻译的原因交给已有事件泵。
                let prohibited = matches!(
                    error,
                    pierlink_ssh_engine::Error::ChannelRejected(
                        pierlink_ssh_engine::ChannelFailure::Prohibited
                    )
                ) || streamlocal
                    && matches!(
                        error,
                        pierlink_ssh_engine::Error::ChannelRejected(
                            pierlink_ssh_engine::ChannelFailure::UnknownType
                        )
                    );
                let error = if streamlocal {
                    map_engine_error(&error, Operation::OpenStreamLocal)
                } else {
                    map_error(error)
                };
                if self.spec.readiness == Readiness::ProbeLocalPorts && prohibited {
                    return Err(error);
                }
                self.change(TunnelState::Connected, Some(error), false, events);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoCallbacks;
    impl ConnectionCallbacks for NoCallbacks {
        fn check_host_key(
            &self,
            _: &str,
            _: &pierlink_ssh_engine::HostKey,
            _: &AtomicBool,
        ) -> Result<bool, AppError> {
            panic!("状态测试不建立连接")
        }
        fn password(
            &self,
            _: &str,
            _: &AtomicBool,
        ) -> Result<Option<zeroize::Zeroizing<String>>, AppError> {
            panic!("状态测试不询问密码")
        }
        fn passphrase(
            &self,
            _: &str,
            _: &std::path::Path,
            _: &AtomicBool,
        ) -> Result<Option<zeroize::Zeroizing<String>>, AppError> {
            panic!("状态测试不读取私钥")
        }
        fn dedicated_key(
            &self,
            _: &str,
            _: &AtomicBool,
        ) -> Result<Option<crate::builtin::DedicatedCredential>, AppError> {
            panic!("状态测试不读取专用密钥")
        }
        fn keyboard_interactive(
            &self,
            _: &str,
            _: pierlink_ssh_engine::Challenge,
            _: &AtomicBool,
        ) -> Result<Option<Vec<zeroize::Zeroizing<String>>>, AppError> {
            panic!("状态测试不询问验证码")
        }
    }

    #[test]
    fn 认证取消仅在隧道自身取消时致命否则退避() {
        for cancelled in [false, true] {
            let mut server = crate::server_input::build_alias_server("test", None, "s", 0).unwrap();
            server.source = crate::storage::ServerSource::Manual;
            server.ssh_alias = None;
            server.host = Some("example.test".into());
            server.username = Some("tester".into());
            let spec = TunnelSpec {
                id: "forward:test".into(),
                server_id: server.id.clone(),
                builtin: Some(Arc::new(BuiltinTunnel {
                    manager: ConnectionManager::default(),
                    server,
                    vault: Arc::new(PasswordVault::default()),
                    callbacks: Arc::new(NoCallbacks),
                    class: ConnectionClass::Main,
                    cancelled: Arc::new(AtomicBool::new(cancelled)),
                })),
                target: SshTarget::Manual {
                    host: "example.test".into(),
                    port: 22,
                    user: None,
                    identity_file: None,
                    jump: None,
                },
                options: CommonOpts::default(),
                readiness: Readiness::AuthMarker,
                forwards: vec![TunnelForward::Remote {
                    remote_port: 32123,
                    local_port: 80,
                }],
            };
            let mut entry = Entry::new(spec).unwrap();
            let events = Arc::new(Mutex::new(BTreeMap::new()));
            entry.fail(AppError::new("ssh.auth_cancelled"), &events);
            assert_eq!(entry.fatal, cancelled);
            assert_eq!(
                entry.event.state,
                if cancelled {
                    TunnelState::Error
                } else {
                    TunnelState::Reconnecting
                }
            );
            assert_eq!(entry.event.next_retry_at_ms.is_some(), !cancelled);
        }
    }

    #[test]
    fn 本机保留端口占用及远程拒绝沿用错误码且不回显原文() {
        for (kind, code) in [
            (std::io::ErrorKind::PermissionDenied, "tunnel.port_reserved"),
            (std::io::ErrorKind::AddrInUse, "tunnel.port_in_use"),
            (std::io::ErrorKind::Other, "tunnel.port_unavailable"),
        ] {
            let error = bind_error(12345, kind);
            assert_eq!(error.code, code);
            assert_eq!(error.params["port"], "12345");
        }
        assert_eq!(
            map_error(pierlink_ssh_engine::Error::RequestRejected("远程转发")).code,
            "tunnel.remote_forward_failed"
        );
        let error = map_error(pierlink_ssh_engine::Error::RequestRejected(
            "password=秘密 api_key=秘密",
        ));
        assert!(!format!("{error:?}").contains("秘密"));
    }
}
