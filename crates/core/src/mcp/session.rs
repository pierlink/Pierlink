//! 单个客户端的授权、取消和生命周期。
use super::{
    ClientInfo, Confirmation, Entry, HostEvent, McpHost, Permissions, SessionInfo, Target, lock,
};
use crate::error::AppError;
use std::sync::{
    Arc, Mutex, TryLockError, Weak,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, watch};

pub(super) struct Session {
    pub info: Mutex<SessionInfo>,
    pub accepted: bool,
    pub permits: Arc<Semaphore>,
    pub stopped: AtomicBool,
    pub stop: watch::Sender<bool>,
    calls: Mutex<Vec<Weak<AtomicBool>>>,
    pub(super) last_request: Mutex<Instant>,
    /// 第一次连接的确认结果；服务器入口上同一代令牌的会话共用一份
    confirmation: Arc<Mutex<Option<bool>>>,
    /// 操作审批的“这次连接不再问”；HTTP 同一代令牌共用，本机连接各自独立
    pub actions_allowed: Arc<AtomicBool>,
    opened: Mutex<bool>,
}
impl Session {
    pub(super) fn new(entry: Entry, accepted: bool) -> Self {
        Self::with_confirmation(entry, accepted, Arc::default(), Arc::default())
    }
    /// 共用首次连接确认和操作审批标记；服务器入口的无握手请求每次都会建立新会话。
    pub(super) fn with_confirmation(
        entry: Entry,
        accepted: bool,
        confirmation: Arc<Mutex<Option<bool>>>,
        actions_allowed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            info: Mutex::new(SessionInfo {
                id: uuid::Uuid::new_v4().to_string(),
                entry,
                client: ClientInfo::default(),
                permissions: Permissions::default(),
            }),
            accepted,
            permits: Arc::new(Semaphore::new(8)),
            stopped: AtomicBool::new(false),
            stop: watch::channel(false).0,
            calls: Mutex::new(Vec::new()),
            last_request: Mutex::new(Instant::now()),
            confirmation,
            actions_allowed,
            opened: Mutex::new(false),
        }
    }
    pub(super) fn snapshot(&self) -> SessionInfo {
        lock(&self.info).clone()
    }
    /// 记录一次客户端请求的到达时间；无参数、无返回值。
    pub(super) fn touch(&self) {
        *lock(&self.last_request) = Instant::now();
    }
    /// 无参数；没有进行中的调用时返回最后请求时间，否则返回 None。
    pub(super) fn idle_since(&self) -> Option<Instant> {
        let calls = lock(&self.calls);
        (!calls.iter().any(|call| call.strong_count() > 0)).then(|| *lock(&self.last_request))
    }
    /// 按 now 判断会话是否应显示：最近五分钟有请求或仍有调用，且尚未停止。
    pub(super) fn is_recent(&self, now: Instant) -> bool {
        self.accepted
            && !self.stopped.load(Ordering::Acquire)
            && self.idle_since().is_none_or(|last| {
                now.saturating_duration_since(last) <= Duration::from_secs(5 * 60)
            })
    }
    pub(super) fn observe(&self, client: Option<rmcp::model::Implementation>) {
        self.touch();
        if let Some(client) = client {
            let mut info = lock(&self.info);
            if info.client == ClientInfo::default() {
                info.client = ClientInfo {
                    name: client.name,
                    version: client.version,
                };
            }
        }
    }
    // 宿主方法只在阻塞线程调用；锁确保打开与关闭通知不会颠倒。
    pub(super) fn announce(&self, host: &dyn McpHost) {
        if !self.accepted {
            return;
        }
        let mut opened = lock(&self.opened);
        if !*opened {
            let entry = self.snapshot().entry;
            if let Ok(Some(permissions)) = host.permissions(&entry) {
                lock(&self.info).permissions = permissions;
            }
            host.notify(HostEvent::SessionOpened(self.snapshot()));
            *opened = true;
        }
    }
    pub(super) fn finish(&self, host: &dyn McpHost) {
        self.cancel();
        if self.accepted {
            self.announce(host);
            host.notify(HostEvent::SessionClosed(self.snapshot().id));
        }
    }
    pub(super) fn authorize(
        &self,
        host: &dyn McpHost,
        targets: &[Target],
        cancelled: &AtomicBool,
    ) -> Result<(), AppError> {
        let mut decision = loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(cancelled_error());
            }
            match self.confirmation.try_lock() {
                Ok(decision) => break decision,
                Err(TryLockError::Poisoned(error)) => break error.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    // 排在首次确认后面的调用也须能独立取消。
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        };
        if cancelled.load(Ordering::Acquire) {
            return Err(cancelled_error());
        }
        if decision.is_none() {
            let info = self.snapshot();
            let answer = if host.remembered(&info.entry) {
                Confirmation::Allow
            } else {
                host.confirm_connect(&info, targets, cancelled)?
            };
            *decision = Some(match answer {
                Confirmation::Allow | Confirmation::AllowAlways => true,
                Confirmation::Deny => false,
                Confirmation::NoAnswer => {
                    return Err(if cancelled.load(Ordering::Acquire) {
                        cancelled_error()
                    } else {
                        no_answer_error()
                    });
                }
            });
        }
        if *decision == Some(false) {
            return Err(AppError::new("mcp.connection_denied")
                .with_reason("mcp.the_user_denied_this_connection", "用户拒绝了这个连接"));
        }
        Ok(())
    }
    pub(super) fn call_flag(&self) -> Arc<AtomicBool> {
        let mut calls = lock(&self.calls);
        calls.retain(|weak| weak.strong_count() > 0);
        let flag = Arc::new(AtomicBool::new(self.stopped.load(Ordering::Acquire)));
        calls.push(Arc::downgrade(&flag));
        flag
    }
    pub(super) fn cancel(&self) {
        self.stopped.store(true, Ordering::Release);
        for flag in lock(&self.calls).iter().filter_map(Weak::upgrade) {
            flag.store(true, Ordering::Release);
        }
        self.stop.send_replace(true);
    }
}
pub(super) fn no_answer_error() -> AppError {
    AppError::new("mcp.no_answer").with_reason(
        "mcp.confirmation_no_answer",
        "60 秒内没人回应：请让用户在 Pierlink 里确认后重试",
    )
}

pub(super) fn cancelled_error() -> AppError {
    AppError::new("mcp.cancelled").with_reason("mcp.operation_cancelled", "操作已取消")
}

/// 后台会话句柄；丢弃句柄不主动断开客户端。
pub struct SessionHandle {
    pub(super) session: Arc<Session>,
    pub(super) done: mpsc::Receiver<Result<(), AppError>>,
}
impl SessionHandle {
    /// 无参数、无返回值；取消此连接上的调用并断开连接，可重复调用。
    pub fn cancel(&self) {
        self.session.cancel();
    }
    /// 消耗句柄，在同步线程等待会话结束；返回传输或后台任务错误，不应在异步工作线程调用。
    pub fn wait(self) -> Result<(), AppError> {
        self.done.recv().unwrap_or_else(|_| {
            Err(AppError::new("mcp.session_failed")
                .with_reason("mcp.session_worker_thread_has_exited", "会话工作线程已结束"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 活动时间边界和调用存活决定会话可见性() {
        let session = Session::new(Entry::Windows, true);
        let last = *lock(&session.last_request);
        assert!(session.is_recent(last + Duration::from_secs(300)));
        assert!(!session.is_recent(last + Duration::from_secs(301)));
        let call = session.call_flag();
        assert!(session.idle_since().is_none());
        assert!(session.is_recent(last + Duration::from_secs(1800)));
        drop(call);
        assert_eq!(session.idle_since(), Some(last));
        *lock(&session.last_request) = last - Duration::from_secs(301);
        session.observe(None);
        assert!(session.is_recent(Instant::now()));
        session.cancel();
        assert!(!session.is_recent(Instant::now()));
        assert!(!Session::new(Entry::Windows, false).is_recent(Instant::now()));
    }
}
