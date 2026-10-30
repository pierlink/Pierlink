//! 每个订阅者只保存各服务器最新状态，最多保留 256 条，避免界面停读导致积压。
use super::*;
use std::{collections::VecDeque, sync::Condvar, time::Duration};

/// 每台服务器的连接状态；断线终端由界面提示手动重连。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting,
    WaitingPassword,
    WaitingTwoFactor,
    Failed,
    Disconnected,
}

/// 不含凭据或服务端原文的连接事件；error 只含受控错误码与参数。
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct ConnectionEvent {
    pub server_id: String,
    pub state: ConnectionState,
    pub error: Option<AppError>,
}

#[derive(Default)]
struct Inbox {
    pending: Mutex<VecDeque<ConnectionEvent>>,
    changed: Condvar,
}

/// 有界状态订阅；释放后自动注销，慢订阅者只保留最新状态。
pub struct StateSubscription(Arc<Inbox>);

impl StateSubscription {
    /// 至多等待 timeout 返回下一台服务器的最新事件；没有变化返回 None。
    pub fn wait(&self, timeout: Duration) -> Option<ConnectionEvent> {
        let pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
        let (mut pending, _) = self
            .0
            .changed
            .wait_timeout_while(pending, timeout, |p| p.is_empty())
            .unwrap_or_else(|p| p.into_inner());
        pending.pop_front()
    }
}

#[derive(Default)]
pub(super) struct Events {
    subscribers: Mutex<Vec<Weak<Inbox>>>,
}
impl Events {
    pub(super) fn subscribe(&self) -> StateSubscription {
        let inbox = Arc::new(Inbox::default());
        let mut subscribers = self.subscribers.lock().unwrap_or_else(|p| p.into_inner());
        subscribers.retain(|s| s.strong_count() > 0);
        subscribers.push(Arc::downgrade(&inbox));
        StateSubscription(inbox)
    }

    pub(super) fn publish(&self, event: ConnectionEvent) {
        self.subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|subscriber| {
                let Some(inbox) = subscriber.upgrade() else {
                    return false;
                };
                let mut pending = inbox.pending.lock().unwrap_or_else(|p| p.into_inner());
                pending.retain(|old| old.server_id != event.server_id);
                if pending.len() == 256 {
                    pending.pop_front();
                }
                pending.push_back(event.clone());
                inbox.changed.notify_one();
                true
            });
    }
}

impl ConnectionManager {
    /// 无参数；订阅之后的每服务器状态变化，返回释放即注销的有界句柄。
    pub fn subscribe_states(&self) -> StateSubscription {
        self.pool.events.subscribe()
    }

    pub(super) fn publish(&self, id: &str, state: ConnectionState, error: Option<AppError>) {
        self.pool.publish(id, state, error);
    }
}
