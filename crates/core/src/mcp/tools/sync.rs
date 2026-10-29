//! 扫描本地目录后等待同步收敛，暂停和离线立即返回原因。
use crate::{
    error::AppError,
    mcp::{McpHost, Permissions, SyncTaskState},
};
use serde::Serialize;
use std::{
    collections::HashSet,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Debug, Serialize)]
pub struct SyncWait {
    pub completed: bool,
    pub timed_out: bool,
    pub reason: Option<String>,
    pub tasks: Vec<SyncTaskState>,
}
/// 先扫描 task（ID、名称或 None 表示全部）的本地目录，再等同步完成；timeout 至多 1800 秒。
/// 返回完成状态或暂停、离线、超时原因；暂时错误重试，cancelled 时返回 mcp.cancelled。
pub fn sync_wait(
    host: &dyn McpHost,
    task: Option<&str>,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<SyncWait, AppError> {
    wait_for(host, None, task, timeout, cancelled)
}
pub(crate) fn wait(
    host: &dyn McpHost,
    permissions: &Permissions,
    task: Option<&str>,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<SyncWait, AppError> {
    wait_for(host, Some(permissions), task, timeout, cancelled)
}
fn transient(e: &AppError) -> bool {
    matches!(
        e.code.as_str(),
        "syncthing.busy" | "sync.busy" | "mcp.busy" | "syncthing.unreachable" | "syncthing.timeout"
    ) || e.code == "syncthing.http_error"
        && e.params.get("status").is_some_and(|s| {
            matches!(
                s.as_str(),
                "408" | "409" | "423" | "429" | "500" | "502" | "503" | "504"
            )
        })
}
fn check_cancel(cancelled: &AtomicBool) -> Result<(), AppError> {
    if cancelled.load(Ordering::Acquire) {
        Err(AppError::new("mcp.cancelled").with("reason", "同步等待已取消"))
    } else {
        Ok(())
    }
}
fn pause(start: Instant, timeout: Duration, cancelled: &AtomicBool) -> Result<(), AppError> {
    let duration = Duration::from_secs(1).min(timeout.saturating_sub(start.elapsed()));
    let at = Instant::now();
    while at.elapsed() < duration {
        check_cancel(cancelled)?;
        std::thread::sleep(Duration::from_millis(20).min(duration.saturating_sub(at.elapsed())));
    }
    check_cancel(cancelled)
}
fn wait_for(
    host: &dyn McpHost,
    permissions: Option<&Permissions>,
    task: Option<&str>,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<SyncWait, AppError> {
    let start = Instant::now();
    let timeout = timeout.min(Duration::from_secs(1800));
    let mut scanned = HashSet::new();
    let mut result = SyncWait {
        completed: false,
        timed_out: false,
        reason: None,
        tasks: vec![],
    };
    loop {
        check_cancel(cancelled)?;
        if start.elapsed() >= timeout {
            result.timed_out = true;
            result.reason = Some("等待同步超时".into());
            return Ok(result);
        }
        let mut states = match host.sync_status(task) {
            Ok(states) => states,
            Err(e) if transient(&e) => {
                pause(start, timeout, cancelled)?;
                continue;
            }
            Err(e) => return Err(e),
        };
        if let Some(permissions) = permissions {
            for state in &mut states {
                state
                    .servers
                    .retain(|s| permissions.servers.contains(&s.server_id));
            }
            states.retain(|s| !s.servers.is_empty());
        }
        if states.is_empty() {
            return Err(AppError::new("mcp.not_found").with("reason", "没有可等待的同步任务"));
        }
        result.tasks = states;
        for state in &result.tasks {
            if state.paused || state.state == "paused" {
                result.reason = Some(format!("同步任务“{}”已暂停", state.name));
                return Ok(result);
            }
            if let Some(server) = state.servers.iter().find(|s| !s.connected) {
                result.reason = Some(format!("服务器“{}”未连接", server.name));
                return Ok(result);
            }
        }
        check_cancel(cancelled)?;
        if start.elapsed() >= timeout {
            continue;
        }
        let mut rescanned = false;
        for state in &result.tasks {
            if !scanned.contains(&state.folder_id) {
                check_cancel(cancelled)?;
                let remaining = timeout.saturating_sub(start.elapsed());
                if remaining.is_zero() {
                    break;
                }
                match host.sync_rescan(&state.folder_id, remaining) {
                    Ok(()) => {
                        scanned.insert(state.folder_id.clone());
                    }
                    Err(e) if transient(&e) => {}
                    Err(e) => return Err(e),
                }
                rescanned = true;
            }
        }
        check_cancel(cancelled)?;
        if !rescanned
            && start.elapsed() < timeout
            && result.tasks.iter().all(|s| {
                scanned.contains(&s.folder_id)
                    && s.state == "idle"
                    && s.need_bytes == 0
                    && !s.servers.is_empty()
                    && s.servers.iter().all(|server| {
                        server.completion == Some(100.0)
                            && server.need_items == Some(0)
                            && server.need_deletes == Some(0)
                    })
            })
        {
            result.completed = true;
            return Ok(result);
        }
        pause(start, timeout, cancelled)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::*;
    use std::sync::{Arc, Mutex, atomic::AtomicUsize};
    struct Host {
        calls: AtomicUsize,
        scans: AtomicUsize,
        states: Mutex<Vec<SyncTaskState>>,
        busy: AtomicBool,
    }
    impl Host {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                scans: AtomicUsize::new(0),
                busy: AtomicBool::new(false),
                states: Mutex::new(vec![SyncTaskState {
                    local_path: "C:\\work".into(),
                    id: "task".into(),
                    name: "任务".into(),
                    folder_id: "folder".into(),
                    state: "idle".into(),
                    paused: false,
                    need_bytes: 0,
                    servers: vec![SyncServerState {
                        remote_path: "/work".into(),
                        server_id: "server".into(),
                        name: "服务器".into(),
                        connected: true,
                        completion: Some(100.0),
                        need_items: Some(0),
                        need_deletes: Some(0),
                    }],
                }]),
            }
        }
    }
    impl McpHost for Host {
        fn sync_status(&self, _: Option<&str>) -> Result<Vec<SyncTaskState>, AppError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.busy.swap(false, Ordering::Relaxed) {
                return Err(AppError::new("syncthing.busy"));
            }
            Ok(self.states.lock().unwrap().clone())
        }
        fn sync_rescan(&self, folder: &str, timeout: Duration) -> Result<(), AppError> {
            assert_eq!(folder, "folder");
            assert!(timeout > Duration::ZERO && timeout <= Duration::from_secs(5));
            self.scans.fetch_add(1, Ordering::Relaxed);
            let mut states = self.states.lock().unwrap();
            states[0].state = "idle".into();
            states[0].servers[0].completion = Some(100.0);
            Ok(())
        }
        fn permissions(&self, _: &Entry) -> Result<Option<Permissions>, AppError> {
            unreachable!()
        }
        fn targets(&self, _: &Entry, _: &Permissions) -> Result<Vec<Target>, AppError> {
            unreachable!()
        }
        fn remembered(&self, _: &Entry) -> bool {
            false
        }
        fn confirm_connect(
            &self,
            _: &SessionInfo,
            _: &[Target],
            _: &AtomicBool,
        ) -> Result<Confirmation, AppError> {
            unreachable!()
        }
        fn confirm_action(
            &self,
            _: &SessionInfo,
            _: &Target,
            _: &str,
            _: bool,
            _: &AtomicBool,
        ) -> Result<Confirmation, AppError> {
            unreachable!()
        }
        fn run_server_command(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: RunLimits,
            _: Arc<AtomicBool>,
        ) -> Result<RunOutput, AppError> {
            unreachable!()
        }
        fn open_terminal(
            &self,
            _: TerminalOpenRequest,
            _: &AtomicBool,
        ) -> Result<String, AppError> {
            unreachable!()
        }
        fn notify(&self, _: HostEvent) {}
    }
    #[test]
    fn 扫描后重新确认完成且暂时错误重试() {
        let host = Host::new();
        host.busy.store(true, Ordering::Relaxed);
        {
            let mut states = host.states.lock().unwrap();
            states[0].state = "scanning".into();
            states[0].servers[0].completion = Some(99.0);
        }
        let result =
            sync_wait(&host, None, Duration::from_secs(5), &AtomicBool::new(false)).unwrap();
        assert!(result.completed);
        assert_eq!(host.scans.load(Ordering::Relaxed), 1);
        assert!(host.calls.load(Ordering::Relaxed) >= 3);
    }
    #[test]
    fn 离线暂停立即返回且不扫描() {
        for paused in [false, true] {
            let host = Host::new();
            {
                let mut states = host.states.lock().unwrap();
                states[0].paused = paused;
                states[0].servers[0].connected = paused;
            }
            let started = Instant::now();
            let result =
                sync_wait(&host, None, Duration::from_secs(5), &AtomicBool::new(false)).unwrap();
            assert!(
                result
                    .reason
                    .unwrap()
                    .contains(if paused { "暂停" } else { "未连接" })
            );
            assert!(!result.completed);
            assert!(started.elapsed() < Duration::from_secs(1));
            assert_eq!(host.scans.load(Ordering::Relaxed), 0);
        }
    }
    #[test]
    fn 缺失计数或待删除时不能完成_超时和取消() {
        for missing in [false, true] {
            let host = Host::new();
            host.states.lock().unwrap()[0].servers[0].need_deletes =
                if missing { None } else { Some(1) };
            let result = sync_wait(
                &host,
                None,
                Duration::from_millis(1100),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert!(result.timed_out && !result.completed);
            assert!(host.calls.load(Ordering::Relaxed) >= 2);
        }
        assert_eq!(
            sync_wait(
                &Host::new(),
                None,
                Duration::from_secs(2),
                &AtomicBool::new(true)
            )
            .unwrap_err()
            .code,
            "mcp.cancelled"
        );
        let host = Host::new();
        assert_eq!(
            wait(
                &host,
                &Permissions::default(),
                None,
                Duration::from_secs(2),
                &AtomicBool::new(false)
            )
            .unwrap_err()
            .code,
            "mcp.not_found"
        );
        assert_eq!(host.scans.load(Ordering::Relaxed), 0);
    }
}
