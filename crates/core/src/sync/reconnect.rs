//! 隧道恢复后只重拨目标设备；不改文件夹、不扫描、不重启实例。
use super::{LocalSync, ServerEndpointInfo, invalid_response};
use crate::{
    error::AppError,
    syncthing::client::Client,
    tunnel::{TunnelControl, TunnelState},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
// 刚重连时远端管理套接字可能晚于 SSH 监听就绪；连续这么久不可达才认为隧道已失效。
const UNREACHABLE_REBUILD_AFTER: Duration = Duration::from_secs(5);

pub(super) struct Reconnector {
    state: Arc<Mutex<ReconnectState>>,
    stopping: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

struct ReconnectState {
    local: Client,
    address_updates: Arc<Mutex<()>>,
    local_id: String,
    remote_id: String,
    api_key: String,
    // 发出暂停前即记录，以便响应丢失时也会补偿恢复；失败时保留，下一次先读状态再恢复。
    resume_remote: bool,
    remote: Option<Client>,
}

impl Reconnector {
    pub(super) fn start(
        control: TunnelControl,
        tunnel_id: &str,
        local: &LocalSync,
        info: &ServerEndpointInfo,
    ) -> Result<Self, AppError> {
        let notices = control.observe_connected(tunnel_id)?;
        let state = Arc::new(Mutex::new(ReconnectState {
            local: local.client().clone(),
            address_updates: local.address_updates.clone(),
            local_id: local.device_id().into(),
            remote_id: info.device_id.clone(),
            api_key: info.api_key.clone(),
            resume_remote: false,
            remote: None,
        }));
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_state = state.clone();
        let worker_stop = stopping.clone();
        let tunnel_id = tunnel_id.to_owned();
        let worker = thread::Builder::new()
            .name("同步设备重连".into())
            .spawn(move || {
                recover_connections(control, &tunnel_id, notices, worker_state, worker_stop);
            })
            .map_err(|_| {
                AppError::new("syncthing.io_failed").with("operation", "创建同步设备重连线程")
            })?;
        Ok(Self {
            state,
            stopping,
            worker: Mutex::new(Some(worker)),
        })
    }

    pub(super) fn stop(&self) -> Result<(), AppError> {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.lock().map_err(|_| invalid_response())?.take() {
            worker.join().map_err(|_| invalid_response())?;
        }
        self.state
            .lock()
            .map_err(|_| invalid_response())?
            .resume_pending()
    }
}

impl Drop for Reconnector {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("释放设备重连线程失败：{}", error.code);
        }
    }
}

fn recover_connections(
    control: TunnelControl,
    tunnel_id: &str,
    notices: Receiver<()>,
    state: Arc<Mutex<ReconnectState>>,
    stopping: Arc<AtomicBool>,
) {
    while !stopping.load(Ordering::Acquire) {
        match notices.recv_timeout(Duration::from_millis(100)) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // SSH 监听就绪不代表 Unix 管理套接字已经可用；独立线程重试，不阻塞其他服务器调度。
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut unreachable_since = None;
        while !stopping.load(Ordering::Acquire) {
            let result = (|| {
                let status = control.status(tunnel_id)?;
                if status.state != TunnelState::Connected {
                    return Ok(());
                }
                let [a, b] = status.local_ports.as_slice() else {
                    return Err(invalid_response());
                };
                let mut state = state.lock().map_err(|_| invalid_response())?;
                let remote = Client::new(&format!("http://127.0.0.1:{b}"), &state.api_key)?;
                if !remote_reachable(
                    &control,
                    tunnel_id,
                    status.last_connected_at_ms,
                    &remote,
                    &mut unreachable_since,
                )? {
                    return Ok(());
                }
                state.recover(&remote, *a)
            })();
            match result {
                Ok(()) => break,
                Err(error) => {
                    if Instant::now() >= deadline || error.code == "sync.device_mismatch" {
                        // 只输出受控错误码，服务器响应及凭据不进入日志。
                        eprintln!("隧道恢复后设备重连失败：{}", error.code);
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

/// 探测远端管理接口；可达返回 true。刚重连时远端管理套接字可能还没就绪，
/// 所以不可达先返回错误让调用方重试，连续不可达超过 UNREACHABLE_REBUILD_AFTER 才请求按退避重建隧道并返回 false。
/// :param unreachable_since: 本轮第一次不可达的时间，由调用方在每次通知时清空
fn remote_reachable(
    control: &TunnelControl,
    tunnel_id: &str,
    connected_at: Option<u64>,
    remote: &Client,
    unreachable_since: &mut Option<Instant>,
) -> Result<bool, AppError> {
    match remote.ping_with_timeout(REQUEST_TIMEOUT) {
        Ok(()) => {
            *unreachable_since = None;
            control.probe_succeeded_if_current(tunnel_id, connected_at)?;
            Ok(true)
        }
        Err(error) if error.code == "syncthing.unreachable" => {
            let since = *unreachable_since.get_or_insert_with(Instant::now);
            if since.elapsed() < UNREACHABLE_REBUILD_AFTER {
                return Err(error);
            }
            control.reconnect_if_current(tunnel_id, connected_at)?;
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

impl ReconnectState {
    fn recover(&mut self, remote: &Client, port: u16) -> Result<(), AppError> {
        let status = remote.request("GET", "/rest/system/status", &[], None, REQUEST_TIMEOUT)?;
        if status["myID"].as_str() != Some(&self.remote_id) {
            return Err(AppError::new("sync.device_mismatch"));
        }
        self.remote = Some(remote.clone());
        self.resume_pending()?;
        let local = connection(&self.local, &self.remote_id)?;
        let peer = connection(remote, &self.local_id)?;
        // 用户主动暂停的设备保持暂停；已恢复的连接也不打断。
        if local.1 || peer.1 || (local.0 && peer.0) {
            return super::address::update_device_address(
                &self.local,
                &self.address_updates,
                &self.remote_id,
                port,
                false,
            );
        }
        // 远端还把旧连接视为在线时先释放它；离线远端只需接收本机拨号，无须改配置。
        if peer.0 {
            self.resume_remote = true;
            let pause = set_paused(remote, &self.local_id, true);
            self.resume_pending()?;
            pause?;
        }
        super::address::update_device_address(
            &self.local,
            &self.address_updates,
            &self.remote_id,
            port,
            true,
        )
    }

    fn resume_pending(&mut self) -> Result<(), AppError> {
        if !self.resume_remote {
            return Ok(());
        }
        let client = self.remote.as_ref().ok_or_else(invalid_response)?;
        let id = &self.local_id;
        // Pierlink 自己从不暂停设备，也不开放 Syncthing 界面，设备上的暂停只会来自这里的重拨；
        // 遗留标记也恢复，避免一次恢复失败后同步永久停住。先读状态，已恢复的不再写配置。
        let mut result: Result<(), AppError> = Ok(());
        for _ in 0..3 {
            result = (|| {
                let device = client.request(
                    "GET",
                    &format!("/rest/config/devices/{id}"),
                    &[],
                    None,
                    REQUEST_TIMEOUT,
                )?;
                let paused = device["paused"].as_bool().ok_or_else(invalid_response)?;
                if paused {
                    set_paused(client, id, false)?;
                }
                Ok(())
            })();
            if result.is_ok() {
                self.resume_remote = false;
                return Ok(());
            }
        }
        result.map_err(|error| {
            AppError::new("sync.cleanup_failed")
                .with(
                    "reason",
                    "设备临时暂停后恢复失败，将在下次处理时重新核对暂停状态",
                )
                .with("cause", error.code)
                .with("device_id", id)
        })
    }
}

fn connection(client: &Client, id: &str) -> Result<(bool, bool), AppError> {
    let value: Value = client.request(
        "GET",
        "/rest/system/connections",
        &[],
        None,
        REQUEST_TIMEOUT,
    )?;
    let device = &value["connections"][id];
    Ok((
        device["connected"].as_bool().ok_or_else(invalid_response)?,
        device["paused"].as_bool().ok_or_else(invalid_response)?,
    ))
}

fn set_paused(client: &Client, id: &str, paused: bool) -> Result<(), AppError> {
    // 配置 PATCH 等待订阅者完成应用，再恢复，避免暂停/恢复在连接服务里合并。
    client.request(
        "PATCH",
        &format!("/rest/config/devices/{id}"),
        &[],
        Some(json!({"paused": paused})),
        REQUEST_TIMEOUT,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
