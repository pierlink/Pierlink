//! 本次服务运行期间可操作的终端；用户交出的终端只授权给指定入口。
use super::super::{Entry, HostEvent, McpHost, Target, lock, session::cancelled_error};
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

/// 终端的来源；交出按入口授权，AI 打开的终端保留创建会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalSource {
    Opened { session: String },
    HandedOver { entry: Entry },
}
#[derive(Clone, Serialize)]
pub struct AiTerminal {
    pub source: TerminalSource,
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
                "mcp.terminal_taken_back",
                "用户已收回这个终端：用 list_terminals 查看，或 open_terminal 新开",
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
            self.registration(
                TerminalSource::Opened {
                    session: session.clone(),
                },
                &id,
                target,
            )
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
        source: TerminalSource,
        id: &str,
        target: Target,
    ) -> Result<Registered, AppError> {
        let subscription = self.terminals.subscribe(id, 1)?;
        let generation = self.terminals.control_generation(id)?;
        let unknown = matches!(source, TerminalSource::HandedOver { .. }) || generation != 0;
        Ok(Registered {
            info: AiTerminal {
                source,
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
    pub(in crate::mcp) fn hand_over(
        &self,
        id: &str,
        entry: Entry,
        target: Target,
        host: &dyn McpHost,
    ) -> Result<AiTerminal, AppError> {
        let mut entries = lock(&self.entries);
        if entries.contains_key(id) {
            return Err(already_registered());
        }
        let registered = self.registration(
            TerminalSource::HandedOver {
                entry: entry.clone(),
            },
            id,
            target,
        )?;
        // 收回会暂停自动化；重新交出时恢复，仍遵守用户空闲两秒的规则。
        self.terminals.set_user_control(id, false)?;
        let info = registered.info.clone();
        entries.insert(id.into(), Arc::new(registered));
        host.notify(HostEvent::TerminalHandedOver {
            terminal_id: id.into(),
            entry,
        });
        Ok(info)
    }
    pub(in crate::mcp) fn list(&self, entry: &Entry, targets: &[Target]) -> Vec<AiTerminal> {
        lock(&self.entries)
            .values()
            .filter_map(|registered| visible(&registered.info, entry, targets))
            .collect()
    }
    pub(in crate::mcp) fn get(
        &self,
        id: &str,
        entry: &Entry,
        targets: &[Target],
    ) -> Option<Arc<Registered>> {
        let entries = lock(&self.entries);
        let registered = entries.get(id)?;
        visible(&registered.info, entry, targets)?;
        Some(registered.clone())
    }
    pub(in crate::mcp) fn close(&self, id: &str, host: &dyn McpHost) -> Result<(), AppError> {
        let mut entries = lock(&self.entries);
        let registered = entries.get(id).ok_or_else(not_found)?;
        if matches!(registered.info.source, TerminalSource::HandedOver { .. }) {
            return Err(AppError::new("mcp.terminal_close_denied").with_reason(
                "mcp.ai_cannot_close_a_shared_user_terminal_reclaim_it_or",
                "用户交出的终端不能由 AI 关闭，请用户收回或在终端内退出",
            ));
        }
        registered.revoked.store(true, Ordering::Release);
        entries.remove(id);
        self.terminals.close(id)?;
        host.notify(HostEvent::TerminalClosed(id.into()));
        Ok(())
    }
    pub(in crate::mcp) fn take_back(
        &self,
        id: &str,
        host: &dyn McpHost,
    ) -> Result<AiTerminal, AppError> {
        let mut entries = lock(&self.entries);
        let registered = entries.get(id).ok_or_else(not_found)?;
        if !matches!(registered.info.source, TerminalSource::HandedOver { .. }) {
            return Err(AppError::new("mcp.terminal_not_handed_over").with_reason(
                "mcp.this_terminal_was_not_shared_by_the_user",
                "这个终端不是用户交出的终端",
            ));
        }
        self.revoke(registered)?;
        let info = registered.info.clone();
        entries.remove(id);
        notify_taken_back(host, &info);
        Ok(info)
    }
    fn revoke(&self, registered: &Registered) -> Result<(), AppError> {
        registered.revoked.store(true, Ordering::Release);
        // 等待审批的旧调用可能还持有登记；收回时立即释放订阅额度，不等这些调用返回。
        registered.subscription.unsubscribe();
        match self
            .terminals
            .set_user_control(&registered.info.terminal_id, true)
        {
            Err(error) if error.code == "terminal.not_found" => Ok(()),
            result => result,
        }
    }
    /// 关闭 AI 创建的终端，收回用户终端；返回收回信息供服务记录审计。
    pub(in crate::mcp) fn close_all(&self, host: &dyn McpHost) -> Vec<AiTerminal> {
        let mut entries = lock(&self.entries);
        let mut taken_back = Vec::new();
        for (id, registered) in std::mem::take(&mut *entries) {
            registered.revoked.store(true, Ordering::Release);
            match registered.info.source {
                TerminalSource::HandedOver { .. } => {
                    if self.revoke(&registered).is_err() {
                        eprintln!("MCP 收回用户终端失败");
                    }
                    notify_taken_back(host, &registered.info);
                    taken_back.push(registered.info.clone());
                }
                TerminalSource::Opened { .. } => {
                    if self.terminals.close(&id).is_err() {
                        eprintln!("MCP 关闭 AI 终端失败");
                    }
                    host.notify(HostEvent::TerminalClosed(id));
                }
            }
        }
        taken_back
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
fn visible(info: &AiTerminal, entry: &Entry, targets: &[Target]) -> Option<AiTerminal> {
    if let TerminalSource::HandedOver { entry: owner } = &info.source
        && owner != entry
    {
        return None;
    }
    let target = targets
        .iter()
        .find(|target| target.id == info.target.id && target.kind == info.target.kind)?;
    Some(AiTerminal {
        target: target.clone(),
        ..info.clone()
    })
}
fn notify_taken_back(host: &dyn McpHost, info: &AiTerminal) {
    if let TerminalSource::HandedOver { entry } = &info.source {
        host.notify(HostEvent::TerminalTakenBack {
            terminal_id: info.terminal_id.clone(),
            entry: entry.clone(),
        });
    }
}
fn not_found() -> AppError {
    AppError::new("mcp.terminal_not_found")
        .with_reason("mcp.ai_terminal_does_not_exist", "AI 终端不存在")
}
fn already_registered() -> AppError {
    AppError::new("mcp.terminal_registered").with_reason(
        "mcp.terminal_is_shared_with_ai_reclaim_it_first",
        "终端已交给 AI，请先收回",
    )
}
