use super::{error, invalid_response};
use crate::{
    builtin::{ConnectionClass, ConnectionManager, ConnectionRequest},
    error::AppError,
    ssh::{
        args::{CommonOpts, ForwardTarget},
        target::SshTarget,
    },
    syncthing::{
        client::{Client, loopback_port},
        folder::validate_device_id,
        home::{self, GuiListen, HomeSpec},
        process::{self, RunningInstance, SyncthingBin},
    },
    tunnel::{TunnelControl, TunnelForward, TunnelManager, TunnelSpec, TunnelState},
};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

pub struct LocalSync {
    pub(super) instance: Option<RunningInstance>,
    pub(super) client: Client,
    pub(super) id: String,
    pub(super) address_updates: std::sync::Arc<std::sync::Mutex<()>>,
}
impl LocalSync {
    /// 以 home、回环端口和凭据初始化并启动本机实例；返回拥有进程生命周期的句柄。
    pub fn start(
        bin: &SyncthingBin,
        home: &Path,
        gui_port: u16,
        listen_port: u16,
        api_key: &str,
        gui_password: &str,
    ) -> Result<Self, AppError> {
        if api_key.is_empty() || gui_port == listen_port {
            return Err(error(
                "sync.key_cannot_be_empty_and_listening_ports_must_differ",
                "密钥不能为空，两个监听端口须不同",
            ));
        }
        Client::new(&format!("http://127.0.0.1:{gui_port}"), api_key)?;
        let spec = HomeSpec {
            home: home.into(),
            gui: GuiListen::Tcp { port: gui_port },
            gui_user: "pierlink".into(),
            listen_port,
        };
        let info = home::init_home(bin, &spec, gui_password)?;
        let instance = process::start(bin, home, &spec.gui, api_key)?;
        Ok(Self {
            address_updates: Default::default(),
            client: instance.client().clone(),
            instance: Some(instance),
            id: info.device_id,
        })
    }
    /// 在 timeout 内扫描本机 folder_id；成功返回空值，失败返回管理接口的错误。
    pub fn rescan(&self, folder_id: &str, timeout: Duration) -> Result<(), AppError> {
        self.client
            .scan_with_timeout(folder_id, timeout)
            .map(|_| ())
    }
    /// 无参数；返回本机设备 ID。
    pub fn device_id(&self) -> &str {
        &self.id
    }
    /// 无参数；返回本机管理客户端。
    pub fn client(&self) -> &Client {
        &self.client
    }
    /// 消费句柄并停止本机全部匹配进程；返回停止结果。
    pub fn stop(self) -> Result<(), AppError> {
        match self.instance {
            Some(instance) => instance.stop(),
            None => Ok(()),
        }
    }
}
// 不派生 Debug，避免凭据出现在调试输出中。
pub struct ServerEndpointInfo {
    pub server_id: String,
    pub name: String,
    pub device_id: String,
    pub socket_path: String,
    pub sync_port: u16,
    pub api_key: String,
    pub state_dir: String,
}
pub struct ServerLink {
    pub(super) estimate_files: Option<super::EstimateFileAccess>,
    pub(super) remote: Client,
    pub(super) server_id: String,
    pub(super) device_id: String,
    pub(super) state_dir: String,
    port: u16,
    api_key: String,
    tunnel: Option<(TunnelControl, String)>,
    reconnect: Option<super::reconnect::Reconnector>,
}
impl ServerLink {
    /// 指定远端原始文件访问方式 access，供预估恢复规则及创建失败恢复标记；无返回值。
    /// connect 会自动设为经 SSH 访问；from_parts（不经 SSH）需要显式设置，远端不得使用 Local。
    pub fn set_estimate_file_access(&mut self, access: super::EstimateFileAccess) {
        self.estimate_files = Some(access);
    }

    /// 根据系统 OpenSSH 参数建立双转发，核对 info 中的身份后配对；失败关闭新隧道。
    /// 成功后自动监听隧道重连，在管理接口可用时重拨目标设备；释放句柄时回收监听线程。
    pub fn connect(
        tunnels: &TunnelManager,
        target: SshTarget,
        opts: CommonOpts,
        info: &ServerEndpointInfo,
        local: &LocalSync,
        computer_name: &str,
    ) -> Result<Self, AppError> {
        Self::connect_inner(tunnels, target, opts, info, local, computer_name, None)
    }

    /// 用 manager 和 request 按服务器偏好建立双转发，返回核对身份并完成配对的连接。
    /// target、opts 保留系统 OpenSSH 参数；内置同步强制使用 Bulk、禁用交互询问。
    /// info、local、computer_name 用于配对；重连复用同一隧道规格，失败时关闭新隧道。
    #[allow(clippy::too_many_arguments)]
    pub fn connect_with_connection(
        tunnels: &TunnelManager,
        target: SshTarget,
        opts: CommonOpts,
        info: &ServerEndpointInfo,
        local: &LocalSync,
        computer_name: &str,
        manager: &ConnectionManager,
        mut request: ConnectionRequest<'_>,
    ) -> Result<Self, AppError> {
        request.class = ConnectionClass::Bulk;
        request.interactive = false;
        Self::connect_inner(
            tunnels,
            target,
            opts,
            info,
            local,
            computer_name,
            Some((manager, request)),
        )
    }

    fn connect_inner(
        tunnels: &TunnelManager,
        target: SshTarget,
        opts: CommonOpts,
        info: &ServerEndpointInfo,
        local: &LocalSync,
        computer_name: &str,
        connection: Option<(&ConnectionManager, ConnectionRequest<'_>)>,
    ) -> Result<Self, AppError> {
        validate(info)?;
        // 预估和创建失败回滚要经 SSH 读写远端的 .stignore / .stfolder：用与隧道相同的程序和目标
        let openssh =
            || crate::ssh::args::build_args(&target, &crate::ssh::args::Purpose::Background, &opts);
        let command = match &connection {
            Some((manager, request)) => {
                crate::remote::command::background(manager, request.clone(), openssh)?
            }
            None => openssh()?,
        };
        let file_access = super::EstimateFileAccess::Ssh {
            program: tunnels.program().clone(),
            command,
        };
        let id = format!("sync:{}", info.server_id);
        let spec = TunnelSpec {
            builtin: None,
            id: id.clone(),
            server_id: info.server_id.clone(),
            target,
            options: opts,
            readiness: crate::ssh::args::Readiness::ProbeLocalPorts,
            forwards: vec![
                TunnelForward::Local {
                    local_port: None,
                    remote: ForwardTarget::Tcp {
                        port: info.sync_port,
                    },
                    replace_on_conflict: true,
                },
                TunnelForward::Local {
                    local_port: None,
                    remote: ForwardTarget::Unix {
                        path: info.socket_path.clone(),
                    },
                    replace_on_conflict: true,
                },
            ],
        };
        let spec = match connection {
            Some((manager, request)) => crate::tunnel::builtin_spec(
                manager,
                request,
                crate::ssh::engine::Feature::SyncTunnel,
                spec,
            )?,
            None => spec,
        };
        tunnels.start(spec)?;
        let control = tunnels.control();
        let result = (|| {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let status = wait_connected(&control, &id, deadline)?;
                let [a, b] = status.local_ports.as_slice() else {
                    return Err(invalid_response());
                };
                let client = Client::new(&format!("http://127.0.0.1:{b}"), &info.api_key)?;
                match client.ping_with_timeout(Duration::from_millis(500)) {
                    Ok(()) => {
                        return Self::from_parts(
                            client,
                            format!("tcp://127.0.0.1:{a}"),
                            info,
                            local,
                            computer_name,
                        );
                    }
                    Err(e) if e.code != "syncthing.unreachable" => return Err(e),
                    Err(_) if Instant::now() >= deadline => {
                        return Err(status
                            .error
                            .filter(|e| e.code == "ssh.streamlocal_open_failed")
                            .unwrap_or_else(|| AppError::new("sync.connect_timeout")));
                    }
                    _ => thread::sleep(Duration::from_millis(50)),
                }
            }
        })();
        match result {
            Ok(mut link) => {
                link.tunnel = Some((control.clone(), id.clone()));
                link.reconnect = Some(super::reconnect::Reconnector::start(
                    control.clone(),
                    &id,
                    local,
                    info,
                )?);
                link.estimate_files = Some(file_access);
                Ok(link)
            }
            Err(e) => {
                control.stop(&id).map_err(|cleanup| {
                    AppError::new("sync.cleanup_failed")
                        .with("cause", &e.code)
                        .with("cleanup", cleanup.code)
                })?;
                Err(e)
            }
        }
    }
    /// 用已有 remote 和回环 sync_addr 核对身份并双向配对；不创建隧道。
    pub fn from_parts(
        remote: Client,
        sync_addr: String,
        info: &ServerEndpointInfo,
        local: &LocalSync,
        computer_name: &str,
    ) -> Result<Self, AppError> {
        validate(info)?;
        let port = loopback_port(&sync_addr, "tcp://127.0.0.1:")?;
        verify(&remote, &info.device_id)?;
        pair_device(
            &remote,
            local.device_id(),
            &short_name(computer_name),
            &["dynamic".into()],
        )?;
        let _guard = local
            .address_updates
            .lock()
            .map_err(|_| invalid_response())?;
        pair_device(
            local.client(),
            &info.device_id,
            &short_name(&info.name),
            &[sync_addr],
        )?;
        Ok(Self {
            remote,
            estimate_files: None,
            server_id: info.server_id.clone(),
            device_id: info.device_id.clone(),
            state_dir: info.state_dir.clone(),
            port,
            api_key: info.api_key.clone(),
            tunnel: None,
            reconnect: None,
        })
    }
    /// 无参数；返回经隧道访问的远程客户端。
    pub fn client(&self) -> &Client {
        &self.remote
    }
    /// 无参数；返回服务器实例所有文件夹的规范路径，供远程标记归属检查使用。
    pub fn folder_paths(&self) -> Result<Vec<String>, AppError> {
        self.remote
            .list_folders()?
            .as_array()
            .ok_or_else(invalid_response)?
            .iter()
            .map(|f| {
                super::paths::remote_normalized(f["path"].as_str().ok_or_else(invalid_response)?)
            })
            .collect()
    }
    /// 无参数；返回最近一次刷新确认的同步端口 A。
    pub fn local_sync_port(&self) -> u16 {
        self.port
    }
    /// 隧道重连后读取实际 A/B 端口，核对身份并更新 local 的设备地址；返回地址刷新结果。
    /// 设备重拨由后台线程负责，本方法只更新地址及客户端，不暂停或恢复设备。
    pub fn refresh_address(&mut self, local: &LocalSync) -> Result<(), AppError> {
        let Some((control, id)) = &self.tunnel else {
            return Ok(());
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = wait_connected(control, id, deadline)?;
        let [a, b] = status.local_ports.as_slice() else {
            return Err(invalid_response());
        };
        let remote = Client::new(&format!("http://127.0.0.1:{b}"), &self.api_key)?;
        self.update_address(local, remote, *a)
    }
    fn update_address(
        &mut self,
        local: &LocalSync,
        remote: Client,
        port: u16,
    ) -> Result<(), AppError> {
        verify(&remote, &self.device_id)?;
        super::address::update_device_address(
            local.client(),
            &local.address_updates,
            &self.device_id,
            port,
            false,
        )?;
        self.remote = remote;
        self.port = port;
        Ok(())
    }
    /// 消费连接并关闭其隧道；tunnels 为创建连接时的全局管理器。
    pub fn disconnect(mut self, _tunnels: &TunnelManager) -> Result<(), AppError> {
        self.stop_tunnel()?;
        self.tunnel = None;
        Ok(())
    }
    pub(super) fn stop_if_unused(&self, local: &LocalSync) -> Result<(), AppError> {
        let folders = local.client().list_folders()?;
        let folders = folders.as_array().ok_or_else(invalid_response)?;
        if !folders.iter().any(|f| {
            f["devices"]
                .as_array()
                .is_some_and(|ds| ds.iter().any(|d| d["deviceID"] == self.device_id))
        }) {
            self.stop_tunnel()?;
        }
        Ok(())
    }
    fn stop_tunnel(&self) -> Result<(), AppError> {
        let recovery = self.reconnect.as_ref().map_or(Ok(()), |r| r.stop());
        if let Some((control, id)) = &self.tunnel {
            match control.stop(id) {
                Err(e) if e.code == "tunnel.not_found" => {}
                other => return recovery.and(other),
            }
        }
        recovery
    }
}
impl Drop for ServerLink {
    fn drop(&mut self) {
        if let Err(error) = self.stop_tunnel() {
            eprintln!("释放同步连接失败：{}", error.code);
        }
    }
}
fn short_name(name: &str) -> String {
    name.chars().take(64).collect()
}
fn validate(info: &ServerEndpointInfo) -> Result<(), AppError> {
    validate_device_id(&info.device_id)?;
    if info.server_id.is_empty()
        || info.sync_port == 0
        || !info.state_dir.starts_with('/')
        || info.state_dir.contains('\0')
    {
        return Err(error(
            "sync.server_record_is_incomplete",
            "服务器记录不完整",
        ));
    }
    Ok(())
}
fn verify(client: &Client, expected: &str) -> Result<(), AppError> {
    let actual = client.my_device_id()?;
    if actual != expected {
        return Err(AppError::new("sync.device_mismatch")
            .with("expected", expected.chars().take(7).collect::<String>())
            .with("actual", actual.chars().take(7).collect::<String>()));
    }
    Ok(())
}

fn wait_connected(
    control: &TunnelControl,
    id: &str,
    deadline: Instant,
) -> Result<crate::tunnel::TunnelEvent, AppError> {
    loop {
        if Instant::now() >= deadline {
            return Err(control
                .status(id)
                .ok()
                .and_then(|status| status.error)
                .filter(|e| e.code == "ssh.streamlocal_open_failed")
                .unwrap_or_else(|| AppError::new("sync.connect_timeout")));
        }
        let status = control.status(id)?;
        if status.state == TunnelState::Connected {
            return Ok(status);
        }
        if matches!(status.state, TunnelState::Error | TunnelState::Stopped) {
            return Err(status
                .error
                .unwrap_or_else(|| error("sync.tunnel_is_not_connected", "隧道未连接")));
        }
        // 等待凭据时不占着同步操作线程；保留验证码错误，供调用方选择登录入口。
        if status.state == TunnelState::WaitingPassword {
            return Err(status
                .error
                .filter(|e| {
                    matches!(
                        e.code.as_str(),
                        "ssh.password_needed" | "ssh.two_factor_required"
                    )
                })
                .unwrap_or_else(|| AppError::new("ssh.password_needed")));
        }
        thread::sleep(Duration::from_millis(50));
    }
}
fn pair_device(
    client: &Client,
    id: &str,
    name: &str,
    addresses: &[String],
) -> Result<(), AppError> {
    let devices = client.list_devices()?;
    let existing = devices
        .as_array()
        .ok_or_else(invalid_response)?
        .iter()
        .find(|d| d["deviceID"] == id);
    if let Some(existing) = existing {
        let mut addresses = addresses.to_vec();
        if let ([wanted], Some(current)) = (
            addresses.as_mut_slice(),
            existing["addresses"].as_array().filter(|a| a.len() == 1),
        ) && let Some(current) = current[0].as_str()
            && let Ok(port) = loopback_port(wanted, "tcp://127.0.0.1:")
            && let Ok(preserved) = super::address::device_address(current, port, false)
        {
            *wanted = preserved;
        }
        let mut patch = serde_json::Map::new();
        if existing["name"] != name {
            patch.insert("name".into(), serde_json::json!(name));
        }
        if existing["addresses"] != serde_json::json!(addresses) {
            patch.insert("addresses".into(), serde_json::json!(addresses));
        }
        if !patch.is_empty() {
            client.call(
                "PATCH",
                &format!("/rest/config/devices/{id}"),
                &[],
                Some(patch.into()),
            )?;
        }
        Ok(())
    } else {
        client.add_device(id, name, addresses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::TunnelEvent;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[test]
    fn 地址刷新只写地址_不重拨且身份失败保持原端口() {
        let id = "AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA";
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let address = Arc::new(std::sync::Mutex::new("tcp4://127.0.0.1:12345".to_owned()));
        let current = address.clone();
        let mut client = Client::new("http://127.0.0.1:1", "test-key").unwrap();
        client.test_handler = Some(Arc::new(move |method, path, _, body| {
            assert_eq!(path, format!("/rest/config/devices/{id}"));
            match method {
                "GET" => Ok(serde_json::json!({"addresses": [*current.lock().unwrap()]})),
                "PATCH" => {
                    let body = body.unwrap();
                    assert_eq!(body.as_object().unwrap().len(), 1);
                    *current.lock().unwrap() = body["addresses"][0].as_str().unwrap().into();
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(serde_json::Value::Null)
                }
                _ => panic!("刷新地址不应暂停或重启设备"),
            }
        }));
        let local = LocalSync {
            address_updates: Default::default(),
            instance: None,
            client: client.clone(),
            id: "本机".into(),
        };
        let mut link = ServerLink {
            remote: client,
            estimate_files: None,
            server_id: "服务器".into(),
            device_id: id.into(),
            state_dir: "/state".into(),
            port: 12345,
            api_key: "test-key".into(),
            tunnel: None,
            reconnect: None,
        };
        let mut remote = Client::new("http://127.0.0.1:2", "test-key").unwrap();
        remote.test_handler = Some(Arc::new(move |method, path, _, _| {
            assert_eq!((method, path), ("GET", "/rest/system/status"));
            Ok(serde_json::json!({"myID": id}))
        }));
        link.update_address(&local, remote.clone(), 23456).unwrap();
        assert_eq!(*address.lock().unwrap(), "tcp4://127.0.0.1:23456");
        link.update_address(&local, remote.clone(), 23456).unwrap();
        assert_eq!(link.local_sync_port(), 23456);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // 后台重拨切回 tcp 后，再次刷新应保留新方案，不产生反向切换。
        super::super::address::update_device_address(
            local.client(),
            &local.address_updates,
            id,
            23456,
            true,
        )
        .unwrap();
        link.update_address(&local, remote.clone(), 23456).unwrap();
        assert_eq!(*address.lock().unwrap(), "tcp://127.0.0.1:23456");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        link.device_id = "BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB".into();
        assert_eq!(
            link.update_address(&local, remote, 34567).unwrap_err().code,
            "sync.device_mismatch"
        );
        assert_eq!(link.local_sync_port(), 23456);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn 重复配对及端口变化保留设备当前方案() {
        let device = Arc::new(std::sync::Mutex::new(serde_json::json!({
            "deviceID": "服务器", "name": "服务器", "addresses": ["tcp4://127.0.0.1:12345"],
        })));
        let state = device.clone();
        let mut client = Client::new("http://127.0.0.1:1", "test-key").unwrap();
        client.test_handler = Some(Arc::new(move |method, path, _, body| {
            match (method, path) {
                ("GET", "/rest/config/devices") => Ok(serde_json::json!([*state.lock().unwrap()])),
                ("PATCH", "/rest/config/devices/服务器") => {
                    let body = body.unwrap();
                    assert_eq!(
                        body,
                        serde_json::json!({"addresses": ["tcp4://127.0.0.1:23456"]})
                    );
                    state.lock().unwrap()["addresses"] = body["addresses"].clone();
                    Ok(serde_json::Value::Null)
                }
                _ => panic!("重复配对不应覆盖现有方案：{method} {path}"),
            }
        }));
        for port in [12345, 23456, 23456] {
            pair_device(
                &client,
                "服务器",
                "服务器",
                &[format!("tcp://127.0.0.1:{port}")],
            )
            .unwrap();
        }
        assert_eq!(
            device.lock().unwrap()["addresses"],
            serde_json::json!(["tcp4://127.0.0.1:23456"])
        );
    }
    #[test]
    fn 连接轮询遵守总时限() {
        let (control, worker) = crate::tunnel::test_control(Default::default());
        assert_eq!(
            wait_connected(&control, "test", Instant::now())
                .unwrap_err()
                .code,
            "sync.connect_timeout"
        );
        drop(control);
        worker.join().unwrap();
    }
    #[test]
    fn 套接字转发被拒或等待超时保留具体错误() {
        for state in [TunnelState::Error, TunnelState::Connected] {
            let error = crate::builtin::error::map_engine_error(
                &pierlink_ssh_engine::Error::ChannelRejected(
                    pierlink_ssh_engine::ChannelFailure::ConnectFailed,
                ),
                crate::builtin::error::Operation::OpenStreamLocal,
            );
            let event = TunnelEvent {
                tunnel_id: "test".into(),
                server_id: "test".into(),
                state,
                local_ports: vec![12345, 12346],
                attempt: 0,
                next_retry_at_ms: None,
                last_connected_at_ms: None,
                timestamp_ms: 0,
                error: Some(error.clone()),
            };
            let (control, worker) = crate::tunnel::test_control(vec![event].into());
            let deadline = if state == TunnelState::Error {
                Instant::now() + Duration::from_secs(5)
            } else {
                Instant::now()
            };
            let actual = wait_connected(&control, "test", deadline).unwrap_err();
            assert_eq!(actual.code, error.code);
            assert_eq!(actual.params, error.params);
            drop(control);
            worker.join().unwrap();
        }
    }
    #[test]
    fn 建立连接时隧道等密码就直接报需要密码() {
        let events = vec![TunnelEvent {
            tunnel_id: "test".into(),
            server_id: "test".into(),
            state: TunnelState::WaitingPassword,
            local_ports: vec![],
            attempt: 0,
            next_retry_at_ms: None,
            last_connected_at_ms: None,
            timestamp_ms: 0,
            error: Some(AppError::new("ssh.auth_failed")),
        }];
        let (control, worker) = crate::tunnel::test_control(events.into());
        let started = Instant::now();
        assert_eq!(
            wait_connected(&control, "test", Instant::now() + Duration::from_secs(5))
                .unwrap_err()
                .code,
            "ssh.password_needed"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(control);
        worker.join().unwrap();
    }
    #[test]
    fn 同步等待验证码保留错误码和实际等待的跳板服务器() {
        let expected = AppError::new("ssh.two_factor_required").with("server_id", "jump");
        let event = TunnelEvent {
            tunnel_id: "test".into(),
            server_id: "target".into(),
            state: TunnelState::WaitingPassword,
            local_ports: vec![],
            attempt: 0,
            next_retry_at_ms: None,
            last_connected_at_ms: None,
            timestamp_ms: 0,
            error: Some(expected.clone()),
        };
        let (control, worker) = crate::tunnel::test_control(vec![event].into());
        let started = Instant::now();
        let actual =
            wait_connected(&control, "test", started + Duration::from_secs(5)).unwrap_err();
        assert_eq!(actual.code, expected.code);
        assert_eq!(actual.params, expected.params);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(control);
        worker.join().unwrap();
    }
    #[test]
    fn 重连等待期间不发送密钥() {
        let calls = Arc::new(AtomicUsize::new(0));
        for ending in [TunnelState::Error, TunnelState::Stopped] {
            let events = [TunnelState::Connecting, TunnelState::Reconnecting, ending]
                .into_iter()
                .map(|state| TunnelEvent {
                    tunnel_id: "test".into(),
                    server_id: "test".into(),
                    state,
                    local_ports: vec![12345, 12346],
                    attempt: 0,
                    next_retry_at_ms: None,
                    last_connected_at_ms: None,
                    timestamp_ms: 0,
                    error: Some(AppError::new("tunnel.test_failure")),
                })
                .collect();
            let (control, worker) = crate::tunnel::test_control(events);
            let mut client = Client::new("http://127.0.0.1:12346", "secret").unwrap();
            let count = calls.clone();
            client.test_handler = Some(Arc::new(move |_, _, _, _| {
                count.fetch_add(1, Ordering::SeqCst);
                Err(AppError::new("测试禁止请求"))
            }));
            let local = LocalSync {
                address_updates: Default::default(),
                instance: None,
                client: client.clone(),
                id: "unused".into(),
            };
            let mut link = ServerLink {
                remote: client,
                estimate_files: None,
                server_id: "test".into(),
                device_id: "unused".into(),
                state_dir: "/state".into(),
                port: 12345,
                api_key: "secret".into(),
                tunnel: Some((control, "test".into())),
                reconnect: None,
            };
            let error = link.refresh_address(&local).unwrap_err();
            assert_eq!(error.code, "tunnel.test_failure");
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            drop(link);
            worker.join().unwrap();
        }
    }
}
