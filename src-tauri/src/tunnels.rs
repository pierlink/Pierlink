//! 全应用共用的 SSH 隧道管理器：同步隧道和端口转发用同一个调度器，
//! 这样“每台服务器最多同时建立 3 个连接”、网络恢复后立即重连等规则对两者一起生效。
//! 第一次需要隧道时才创建（没有同步任务、没有端口转发时不启动任何 ssh）。

use crate::terminal::find_ssh;
use pierlink_core::error::AppError;
use pierlink_core::process::SshProgram;
use pierlink_core::tunnel::TunnelManager;
use std::sync::{Arc, Mutex, MutexGuard};
use tauri::{AppHandle, Manager};

#[derive(Default)]
pub struct TunnelsState {
    manager: Mutex<Option<Arc<TunnelManager>>>,
}

impl TunnelsState {
    fn lock(&self) -> MutexGuard<'_, Option<Arc<TunnelManager>>> {
        self.manager.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 取得隧道管理器，还没有时创建。
    ///
    /// :param app: 应用句柄（找 ssh 程序用）
    /// :return: 共享的管理器；找不到 ssh 或创建失败时返回错误
    pub fn get(&self, app: &AppHandle) -> Result<Arc<TunnelManager>, AppError> {
        let mut guard = self.lock();
        if let Some(m) = guard.as_ref() {
            return Ok(m.clone());
        }
        let manager = Arc::new(TunnelManager::new(SshProgram {
            path: find_ssh(app)?,
        })?);
        eprintln!("创建隧道管理器");
        *guard = Some(manager.clone());
        Ok(manager)
    }

    /// 已经创建的管理器；没有时返回 None（不创建）
    pub fn existing(&self) -> Option<Arc<TunnelManager>> {
        self.lock().clone()
    }

    /// 应用退出：放开管理器（最后一个引用释放时关闭全部隧道）
    pub fn shutdown(&self) {
        self.lock().take();
    }
}

/// 网络从断开恢复：立即唤醒等待重连的所有隧道（在网络通知的后台线程里调用）
pub fn network_restored(app: &AppHandle) {
    if let Some(m) = app.state::<TunnelsState>().existing()
        && let Err(e) = m.network_restored()
    {
        eprintln!("网络恢复后唤醒隧道失败: 错误={e:?}");
    }
}
