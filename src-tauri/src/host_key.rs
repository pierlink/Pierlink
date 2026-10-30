//! 主机密钥确认的 Tauri 命令：首次连接时把 ssh 给出的指纹转成应用内确认框（设计 3.1）。
//!
//! 流程：`host_key_check` 启动探测 → 返回“已信任”或“待确认（含指纹）” →
//! 前端逐级调用 `host_key_answer`（有跳板机时会有多级）→ 直到“已信任”或出错。
//! 用户回答的是指纹本身（由核心写给 ssh），由 ssh 自己写入用户默认的 known_hosts。

use crate::pages::PageGenerations;
use crate::terminal::find_ssh;
use pierlink_core::error::AppError;
use pierlink_core::process::SshProgram;
use pierlink_core::ssh::args::{CommonOpts, Purpose, build_args};
use pierlink_core::ssh::host_key::{self, HostKeyPrompt};
use pierlink_core::ssh::target::SshTarget;
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use tauri::{AppHandle, Manager, Webview};

/// 等待用户回答的探测：探测 ID → (打开它的页面, 当前提示)
#[derive(Default)]
pub struct HostKeyState {
    probes: Mutex<HashMap<String, (String, HostKeyPrompt)>>,
    seq: AtomicU64,
}

impl HostKeyState {
    fn probes(&self) -> MutexGuard<'_, HashMap<String, (String, HostKeyPrompt)>> {
        self.probes.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 取消某个页面所有未回答的探测（页面重载、窗口销毁时调用）。
    ///
    /// :param webview: 页面标签
    /// :return: 取消的探测数
    pub fn cancel_owned_by(&self, webview: &str) -> usize {
        let taken: Vec<HostKeyPrompt> = {
            let mut probes = self.probes();
            let ids: Vec<String> = probes
                .iter()
                .filter(|(_, (owner, _))| owner == webview)
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter()
                .filter_map(|id| probes.remove(id).map(|(_, p)| p))
                .collect()
        };
        let n = taken.len();
        // 丢弃提示会结束对应的 ssh 进程，可能要等一两秒，放到后台线程做
        if n > 0 {
            std::thread::spawn(move || drop(taken));
        }
        n
    }
}

/// 确认流程的一步。注意 Trusted 只表示应用内不需要再弹确认框；真正的终端连接仍由 ssh 自己校验指纹。
#[derive(Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum HostKeyStep {
    /// 指纹已在 known_hosts 里且一致（或刚刚确认完），可以连接
    Trusted,
    /// 需要用户确认这一级的指纹
    #[serde(rename_all = "camelCase")]
    Confirm {
        probe_id: String,
        host: String,
        /// 第几级（有跳板机时从跳板机开始数，从 1 开始）
        level: u32,
        key_type: String,
        fingerprint: String,
    },
}

/// 认证失败是否能证明“主机密钥已通过校验”。
///
/// SSH 协议里主机密钥校验在认证之前，探测又只允许 none 认证，所以真正的认证失败说明指纹已通过。
/// 但在校验之前 ssh 也会原样打印服务器控制的文字（断开原因、算法协商失败时对方的列表），
/// 中间人可以在里面放 “Permission denied (”。因此还要求退出码是 255，且输出里没有这两类文字。
/// 即使这里误判，真正的终端连接仍由 ssh 自己校验指纹，不会静默接受。
fn auth_failure_proves_trust(e: &AppError) -> bool {
    let tail = e
        .params
        .get("stderr_tail")
        .map(String::as_str)
        .unwrap_or("");
    e.code == "ssh.auth_failed"
        && e.params.get("exit_code").map(String::as_str) == Some("255")
        && !tail.contains("Received disconnect")
        && !tail.contains("Unable to negotiate")
}

/// 把核心返回的结果转成给前端的一步；待确认的提示存起来等用户回答。
/// 页面在探测期间重载过（代数变了）时直接丢弃结果，结束对应的 ssh。
fn settle(
    app: &AppHandle,
    owner: &str,
    generation: u64,
    result: Result<HostKeyPrompt, AppError>,
) -> Result<HostKeyStep, AppError> {
    if app.state::<PageGenerations>().current(owner) != generation {
        drop(result);
        return Err(AppError::new("ssh.host_key_expired"));
    }
    let state = app.state::<HostKeyState>();
    match result {
        Ok(HostKeyPrompt::Trusted) => Ok(HostKeyStep::Trusted),
        Ok(HostKeyPrompt::Confirm(c)) => {
            let info = c.info.clone();
            let probe_id = format!("hk-{}", state.seq.fetch_add(1, Ordering::Relaxed) + 1);
            state.probes().insert(
                probe_id.clone(),
                (owner.to_owned(), HostKeyPrompt::Confirm(c)),
            );
            Ok(HostKeyStep::Confirm {
                probe_id,
                host: info.host,
                level: u32::try_from(info.level).unwrap_or(u32::MAX),
                key_type: info.key_type,
                fingerprint: info.fingerprint,
            })
        }
        // 探测不使用任何凭据（只允许 none 认证），SSH 协议里主机密钥校验在认证之前完成，
        // 所以“认证失败”说明指纹已经通过校验
        Err(e) if auth_failure_proves_trust(&e) => Ok(HostKeyStep::Trusted),
        Err(e) => Err(e),
    }
}

/// 检查快速连接目标的主机密钥。
///
/// :return: 已信任，或第一级待确认的指纹
#[tauri::command]
#[specta::specta]
pub async fn host_key_check(
    app: AppHandle,
    webview: Webview,
    host: String,
    port: u16,
    user: Option<String>,
) -> Result<HostKeyStep, AppError> {
    let target = SshTarget::Manual {
        host,
        port,
        user,
        identity_file: None,
        jump: None,
    };
    check_target(app, webview, target, None).await
}

/// 检查已保存服务器的主机密钥。
#[tauri::command]
#[specta::specta]
pub async fn host_key_check_server(
    app: AppHandle,
    webview: Webview,
    server_id: String,
) -> Result<HostKeyStep, AppError> {
    let server = crate::servers::load(&app, &server_id)?;
    let target = crate::servers::target(&server)?;
    // 从 ssh config 转换来、带 HostKeyAlias 的服务器：指纹按别名查和写，和内置 SSH、原来的系统 ssh 一致
    let alias = server.host_key_alias.clone().filter(|a| valid_alias(a));
    check_target(app, webview, target, alias).await
}

/// HostKeyAlias 只允许没有空白和控制字符、不以 - 开头的名字（作为 ssh 的 -o 参数值）
fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 255
        && !alias.starts_with('-')
        && !alias.chars().any(|c| c.is_whitespace() || c.is_control())
}

async fn check_target(
    app: AppHandle,
    webview: Webview,
    target: SshTarget,
    host_key_alias: Option<String>,
) -> Result<HostKeyStep, AppError> {
    let owner = webview.label().to_owned();
    let generation = app.state::<PageGenerations>().current(&owner);
    let mut command = build_args(&target, &Purpose::Background, &CommonOpts::default())?;
    // 只做主机密钥校验，不尝试任何认证方式：不读私钥、不弹口令、不消耗服务器的登录失败次数
    command.args.splice(
        0..0,
        ["-o".to_owned(), "PreferredAuthentications=none".to_owned()],
    );
    if let Some(alias) = host_key_alias {
        command
            .args
            .splice(0..0, ["-o".to_owned(), format!("HostKeyAlias={alias}")]);
    }
    tauri::async_runtime::spawn_blocking(move || {
        let program = SshProgram {
            path: find_ssh(&app)?,
        };
        settle(
            &app,
            &owner,
            generation,
            host_key::probe(&program, &command),
        )
    })
    .await
    .map_err(|e| AppError::new("ssh.task_failed").with("reason", e.to_string()))?
}

/// 回答一级指纹确认。
///
/// :param probe_id: `host_key_check` 或上一次回答返回的探测 ID
/// :param accept: 用户是否信任这个指纹
/// :return: 下一级待确认的指纹，或已信任；拒绝时返回 `ssh.host_key_rejected`
#[tauri::command]
#[specta::specta]
pub async fn host_key_answer(
    app: AppHandle,
    probe_id: String,
    accept: bool,
) -> Result<HostKeyStep, AppError> {
    let (owner, prompt) = app
        .state::<HostKeyState>()
        .probes()
        .remove(&probe_id)
        .ok_or_else(|| AppError::new("ssh.host_key_expired"))?;
    let generation = app.state::<PageGenerations>().current(&owner);
    tauri::async_runtime::spawn_blocking(move || {
        settle(&app, &owner, generation, prompt.answer(accept))
    })
    .await
    .map_err(|e| AppError::new("ssh.task_failed").with("reason", e.to_string()))?
}

/// 放弃确认（关闭对话框时调用），结束对应的 ssh 进程。
#[tauri::command]
#[specta::specta]
pub async fn host_key_cancel(app: AppHandle, probe_id: String) {
    let taken = app.state::<HostKeyState>().probes().remove(&probe_id);
    if let Some((_, prompt)) = taken {
        let _ = tauri::async_runtime::spawn_blocking(move || drop(prompt)).await;
    }
}
