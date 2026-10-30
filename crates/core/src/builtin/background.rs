//! 后台持有者按退避恢复连接；验证码和缺失密码只等输入，不反复认证。
use super::*;
use std::{
    sync::{Condvar, atomic::Ordering},
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Default)]
struct BackgroundState {
    connection: Option<Connection>,
    error: Option<AppError>,
    finished: bool,
}
#[derive(Default)]
struct Shared {
    state: Mutex<BackgroundState>,
    changed: Condvar,
}

/// 后台连接的持有者；重连只恢复传输，使用者收到新连接后自行重建采样或转发通道。
/// 释放句柄会取消登录和输入等待；不自动重放远程命令，也不恢复终端。
pub struct BackgroundConnection {
    shared: Arc<Shared>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl BackgroundConnection {
    /// 无参数；返回当前有效连接快照，连接中、等待输入或断线时返回 None。
    pub fn connection(&self) -> Option<Connection> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .connection
            .as_ref()
            .filter(|connection| !connection.is_closed())
            .cloned()
    }

    /// 至多等待 timeout 返回有效连接；超时返回最近的受控错误或连接等待超时。
    pub fn wait_connected(&self, timeout: Duration) -> Result<Connection, AppError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(connection) = state.connection.as_ref().filter(|c| !c.is_closed()) {
                return Ok(connection.clone());
            }
            if state.finished || Instant::now() >= deadline {
                return Err(state.error.clone().unwrap_or_else(|| {
                    AppError::new("ssh.unreachable").with_reason("builtin.waiting_for_a_background_connection_timed_out_or_was_cancelled", "等待后台连接超时或已取消").with_text("stderr_tail", "builtin.waiting_for_a_background_connection_timed_out_or_was_cancelled", "等待后台连接超时或已取消")
                }));
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
}

impl Drop for BackgroundConnection {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        let (state, _) = self
            .shared
            .changed
            .wait_timeout_while(state, Duration::from_secs(3), |s| !s.finished)
            .unwrap_or_else(|p| p.into_inner());
        let finished = state.finished;
        drop(state);
        if finished && let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl ConnectionManager {
    /// 为 request 建立后台持有句柄；忽略 interactive，绝不主动弹密码或验证码框。
    /// 返回后可 wait_connected；句柄存活期间持续重连，输入凭据和网络恢复会立即唤醒。
    pub fn background(
        &self,
        request: ConnectionRequest<'_>,
    ) -> Result<BackgroundConnection, AppError> {
        let chain = self.chain(request.server)?;
        let slots: Vec<_> = chain.iter().map(|s| self.pool.slot(&s.id)).collect();
        let subscriptions: Vec<_> = chain
            .iter()
            .map(|s| request.vault.subscribe(&s.id))
            .collect();
        let server = request.server.clone();
        let vault = request.vault;
        let callbacks = request.callbacks;
        let class = request.class;
        let cancelled = request.cancelled;
        let registrations = slots
            .iter()
            .map(|slot| self.pool.register(slot.clone(), &cancelled))
            .collect::<Result<Vec<_>, _>>()?;
        let shared = Arc::new(Shared::default());
        let manager = self.clone();
        let shared_worker = shared.clone();
        let cancel_worker = cancelled.clone();
        let worker = std::thread::Builder::new()
            .name("ssh-reconnect".into())
            .spawn(move || {
                let _registrations = registrations;
                let shared = shared_worker;
                let cancelled = cancel_worker;
                let mut attempt = 0;
                let mut ever_connected = false;
                let mut next = Instant::now();
                let mut waiting = false;
                let mut network = manager.pool.network.load(Ordering::Acquire);
                let mut revisions: Vec<_> = slots
                    .iter()
                    .map(|s| s.revision.load(Ordering::Acquire))
                    .collect();
                while !cancelled.load(Ordering::Acquire)
                    && !slots.iter().any(|s| s.closed.load(Ordering::Acquire))
                {
                    let mut changed = false;
                    for subscription in &subscriptions {
                        changed |= subscription.wait(Duration::ZERO);
                    }
                    let connected = slots.iter().zip(&mut revisions).fold(
                        false,
                        |changed, (slot, revision)| {
                            let current = slot.revision.load(Ordering::Acquire);
                            let differs = current != *revision;
                            *revision = current;
                            changed || differs
                        },
                    );
                    let restored = manager.pool.network.load(Ordering::Acquire);
                    let immediate = changed || connected || restored != network;
                    if immediate {
                        next = Instant::now();
                        waiting = false;
                        network = restored;
                    }
                    {
                        let mut state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
                        if state.connection.as_ref().is_some_and(Connection::is_closed) {
                            state.connection = None;
                            attempt = 0;
                            next = Instant::now()
                                + if immediate {
                                    Duration::ZERO
                                } else {
                                    backoff(attempt)
                                };
                            attempt = 1;
                            shared.changed.notify_all();
                            manager.publish(&server.id, ConnectionState::Reconnecting, None);
                        }
                        if state.connection.is_some() {
                            drop(state);
                            std::thread::sleep(Duration::from_millis(50));
                            continue;
                        }
                    }
                    if waiting || Instant::now() < next {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    if ever_connected {
                        manager.publish(&server.id, ConnectionState::Reconnecting, None);
                    }
                    let result = manager.get_connection(ConnectionRequest {
                        server: &server,
                        class,
                        vault: vault.clone(),
                        callbacks: callbacks.clone(),
                        interactive: false,
                        cancelled: cancelled.clone(),
                    });
                    let mut state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
                    match result {
                        Ok(connection) => {
                            state.connection = Some(connection);
                            state.error = None;
                            ever_connected = true;
                            for (slot, revision) in slots.iter().zip(&mut revisions) {
                                *revision = slot.revision.load(Ordering::Acquire);
                            }
                            attempt = 0;
                        }
                        Err(error) => {
                            // 登录上限也在这里等：连接池记着这个结果，等待期间的重试不会再去服务器登录；
                            // 用户主动连接成功后（版本号变化）再连
                            waiting = matches!(
                                pool::error_state(&error),
                                ConnectionState::WaitingPassword
                                    | ConnectionState::WaitingTwoFactor
                            ) || !matches!(error.code.as_str(), "ssh.unreachable");
                            next = Instant::now() + backoff(attempt);
                            attempt = attempt.saturating_add(1);
                            state.error = Some(error);
                        }
                    }
                    shared.changed.notify_all();
                }
                let mut state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
                state.connection = None;
                state.finished = true;
                shared.changed.notify_all();
            })
            .map_err(|_| {
                AppError::new("ssh.task_failed").with_reason(
                    "builtin.couldn_t_start_the_background_reconnect_thread",
                    "无法启动后台重连线程",
                )
            })?;
        Ok(BackgroundConnection {
            shared,
            cancelled,
            worker: Some(worker),
        })
    }
}

fn backoff(attempt: usize) -> Duration {
    Duration::from_secs([1, 2, 5, 10, 30][attempt.min(4)])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 退避在长时间断网时持续保持三十秒() {
        assert_eq!(
            (0..8).map(|n| backoff(n).as_secs()).collect::<Vec<_>>(),
            vec![1, 2, 5, 10, 30, 30, 30, 30]
        );
    }
}
