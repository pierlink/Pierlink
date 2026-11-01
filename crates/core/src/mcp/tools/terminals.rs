//! 本次服务运行期间 AI 打开的终端。
use super::super::{HostEvent, McpHost, Target, lock, session::cancelled_error};
use crate::{
    error::AppError,
    terminal::{OutputSubscription, TerminalEvent, TerminalManager},
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Serialize)]
pub struct AiTerminal {
    /// 打开它的会话
    #[serde(skip)]
    pub session: String,
    pub terminal_id: String,
    pub target: Target,
}
#[derive(Default)]
struct Pending {
    generation: u64,
    line: String,
    unknown: bool,
}
pub(in crate::mcp) struct Registered {
    pub info: AiTerminal,
    subscription: OutputSubscription,
    /// 自动化输入还没提交的行；用户修改后只保留“内容未知”状态，不读取用户输入。
    pending: Mutex<Pending>,
    pub send: Mutex<()>,
    pub revoked: AtomicBool,
}
impl Registered {
    pub(super) fn check(&self) -> Result<(), AppError> {
        if self.revoked.load(Ordering::Acquire) {
            Err(AppError::new("mcp.cancelled").with_reason(
                "mcp.terminal_was_closed",
                "终端已关闭：用 list_terminals 查看，或 open_terminal 新开",
            ))
        } else {
            Ok(())
        }
    }
    pub(super) fn pending_line(&self, generation: u64) -> (String, bool) {
        let mut pending = lock(&self.pending);
        if pending.generation != generation {
            pending.generation = generation;
            pending.line.clear();
            pending.unknown = true;
        }
        (pending.line.clone(), pending.unknown)
    }
    pub(super) fn set_pending_line(&self, generation: u64, line: String, unknown: bool) {
        *lock(&self.pending) = Pending {
            generation,
            line,
            unknown,
        };
    }
}
pub(in crate::mcp) struct Registry {
    terminals: Arc<TerminalManager>,
    entries: Mutex<BTreeMap<String, Arc<Registered>>>,
}
impl Registry {
    pub(in crate::mcp) fn new(terminals: Arc<TerminalManager>) -> Self {
        Self {
            terminals,
            entries: Mutex::new(BTreeMap::new()),
        }
    }
    pub(in crate::mcp) fn register(
        &self,
        session: String,
        id: String,
        target: Target,
        cancelled: &AtomicBool,
        host: &dyn McpHost,
    ) -> Result<(), AppError> {
        let mut entries = lock(&self.entries);
        if entries.contains_key(&id) {
            return Err(already_registered());
        }
        let result = if cancelled.load(Ordering::Acquire) {
            Err(cancelled_error())
        } else {
            self.registration(session.clone(), &id, target)
        };
        let registered = match result {
            Ok(registered) => registered,
            Err(error) => {
                self.terminals.close(&id)?;
                return Err(error);
            }
        };
        entries.insert(id.clone(), Arc::new(registered));
        host.notify(HostEvent::TerminalOpened {
            session,
            terminal_id: id,
        });
        Ok(())
    }
    fn registration(
        &self,
        session: String,
        id: &str,
        target: Target,
    ) -> Result<Registered, AppError> {
        let subscription = self.terminals.subscribe(id, 1)?;
        let generation = self.terminals.control_generation(id)?;
        let unknown = generation != 0;
        Ok(Registered {
            info: AiTerminal {
                session,
                terminal_id: id.into(),
                target,
            },
            subscription,
            pending: Mutex::new(Pending {
                generation,
                line: String::new(),
                unknown,
            }),
            send: Mutex::new(()),
            revoked: AtomicBool::new(false),
        })
    }
    pub(in crate::mcp) fn list(&self, targets: &[Target]) -> Vec<AiTerminal> {
        lock(&self.entries)
            .values()
            .filter_map(|registered| visible(&registered.info, targets))
            .collect()
    }
    pub(in crate::mcp) fn get(&self, id: &str, targets: &[Target]) -> Option<Arc<Registered>> {
        let entries = lock(&self.entries);
        let registered = entries.get(id)?;
        visible(&registered.info, targets)?;
        Some(registered.clone())
    }
    pub(in crate::mcp) fn close(&self, id: &str, host: &dyn McpHost) -> Result<(), AppError> {
        let mut entries = lock(&self.entries);
        let registered = entries.get(id).ok_or_else(not_found)?;
        registered.revoked.store(true, Ordering::Release);
        entries.remove(id);
        self.terminals.close(id)?;
        host.notify(HostEvent::TerminalClosed(id.into()));
        Ok(())
    }
    /// 关闭全部 AI 终端（全部停止时用）。
    pub(in crate::mcp) fn close_all(&self, host: &dyn McpHost) {
        let mut entries = lock(&self.entries);
        for (id, registered) in std::mem::take(&mut *entries) {
            registered.revoked.store(true, Ordering::Release);
            if self.terminals.close(&id).is_err() {
                eprintln!("MCP 关闭 AI 终端失败");
            }
            host.notify(HostEvent::TerminalClosed(id));
        }
    }
    pub(in crate::mcp) fn is_empty(&self) -> bool {
        lock(&self.entries).is_empty()
    }
    pub(in crate::mcp) fn reap(&self, host: &dyn McpHost) {
        let mut entries = lock(&self.entries);
        entries.retain(|id, registered| {
            while let Some(event) = registered.subscription.recv_timeout(Duration::ZERO) {
                if matches!(event.event, TerminalEvent::Exited { .. }) {
                    registered.revoked.store(true, Ordering::Release);
                    host.notify(HostEvent::TerminalClosed(id.clone()));
                    return false;
                }
            }
            true
        });
    }
}
fn visible(info: &AiTerminal, targets: &[Target]) -> Option<AiTerminal> {
    let target = targets
        .iter()
        .find(|target| target.id == info.target.id && target.kind == info.target.kind)?;
    Some(AiTerminal {
        target: target.clone(),
        ..info.clone()
    })
}
fn not_found() -> AppError {
    AppError::new("mcp.terminal_not_found")
        .with_reason("mcp.ai_terminal_does_not_exist", "AI 终端不存在")
}
fn already_registered() -> AppError {
    AppError::new("mcp.terminal_registered")
        .with_reason("mcp.terminal_is_already_registered", "终端已登记")
}
