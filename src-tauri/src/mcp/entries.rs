//! 服务器入口：本机 127.0.0.1 随机端口上的 HTTP 服务 + 这台服务器上的远程转发
//! （服务器 127.0.0.1:端口 → 本机端口）+ 服务器上的 `~/.pierlink/mcp.json`（地址和令牌）。
//! 隧道复用端口转发的那一套（断线重连、等待密码、网络恢复），状态事件由端口转发的事件循环转交过来。
use crate::tunnels::TunnelsState;
use pierlink_core::error::AppError;
use pierlink_core::mcp::McpService;
use pierlink_core::security::vault::Zeroizing;
use pierlink_core::tunnel::{Readiness, TunnelEvent, TunnelForward, TunnelSpec, TunnelState};
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// 服务器入口的隧道 ID 前缀（和端口转发共用一个隧道管理器）
pub(crate) const TUNNEL_PREFIX: &str = "mcp:";

/// 一个服务器入口的运行状态
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpEntryStateDto {
    pub server_id: String,
    /// connecting / running / reconnecting / waiting_password / error / stopped
    pub state: String,
    /// 服务器上监听的端口
    pub port: u16,
    pub error: Option<AppError>,
    /// 服务器上的 ~/.pierlink/mcp.json 已写好
    pub file_written: bool,
}

struct Running {
    port: u16,
    state: String,
    error: Option<AppError>,
    file_written: bool,
    /// 换令牌时加一：写文件的线程写完对一下，令牌在写的途中换过就不算写好（等下次连上再写新令牌）
    file_generation: u64,
    /// 服务器上写过文件（不管是不是当前的令牌）：关掉入口时要删
    file_present: bool,
}

/// 正在运行的服务器入口
#[derive(Default)]
pub struct Entries {
    running: Mutex<HashMap<String, Running>>,
    /// 设置连续保存时，后台的 apply 一个接一个做，免得后一次把前一次刚开的入口关掉
    applying: Mutex<()>,
    /// 每台服务器上 mcp.json 的写和删排队做，免得删在前、晚到的写在后，留下一份已经停掉的入口的令牌
    files: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

fn token_key(server_id: &str) -> String {
    format!("pierlink/mcp/{server_id}/token")
}

/// 这台服务器入口的令牌：凭据管理器里有就用，没有就新建一个存进去
pub(super) fn token(server_id: &str) -> Result<Zeroizing<String>, AppError> {
    let store = crate::sync::credential_store()?;
    if let Some(token) = store.get(&token_key(server_id))? {
        return Ok(Zeroizing::new(token));
    }
    let token = pierlink_core::mcp::token::new_token();
    store.set(&token_key(server_id), &token)?;
    Ok(token)
}

/// 服务器上 agent 用的地址
pub fn url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/mcp")
}

impl Entries {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Running>> {
        self.running.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 这台服务器上 mcp.json 的写删锁
    fn file_lock(&self, server_id: &str) -> Arc<Mutex<()>> {
        self.files
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(server_id.to_owned())
            .or_default()
            .clone()
    }

    /// 各入口的状态（给设置页）
    pub fn states(&self) -> Vec<McpEntryStateDto> {
        let mut all: Vec<McpEntryStateDto> = self
            .lock()
            .iter()
            .map(|(id, r)| McpEntryStateDto {
                server_id: id.clone(),
                state: r.state.clone(),
                port: r.port,
                error: r.error.clone(),
                file_written: r.file_written,
            })
            .collect();
        all.sort_by(|a, b| a.server_id.cmp(&b.server_id));
        all
    }

    /// 让运行中的入口和设置一致：该开的开（端口变了就重开），该关的关
    ///
    /// :param app: 应用句柄
    /// :param service: MCP 服务
    /// :param wanted: 读出应该运行的入口（服务器 ID → 端口）；拿到锁之后才读，连续保存时按最后的设置来
    pub fn apply(
        &self,
        app: &AppHandle,
        service: &Arc<McpService>,
        wanted: impl FnOnce() -> HashMap<String, u16>,
    ) {
        let _applying = self.applying.lock().unwrap_or_else(|p| p.into_inner());
        let wanted = &wanted();
        let current: Vec<(String, u16, bool)> = self
            .lock()
            .iter()
            .map(|(id, r)| (id.clone(), r.port, r.state == "error"))
            .collect();
        // 不要了、端口变了、或者上次出错的：先关掉（出错的随后按设置重开，相当于重试）
        for (id, port, failed) in &current {
            if wanted.get(id) != Some(port) || *failed {
                self.stop(app, service, id);
            }
        }
        for (id, port) in wanted {
            if self.lock().contains_key(id) {
                continue;
            }
            if let Err(e) = self.start(app, service, id, *port) {
                eprintln!("开启 AI 接入服务器入口失败: 服务器={id}, 错误={}", e.code);
                self.lock().insert(
                    id.clone(),
                    Running {
                        port: *port,
                        state: "error".into(),
                        error: Some(e),
                        file_written: false,
                        file_generation: 0,
                        file_present: false,
                    },
                );
            }
        }
        emit(app, self.states());
    }

    fn start(
        &self,
        app: &AppHandle,
        service: &Arc<McpService>,
        server_id: &str,
        port: u16,
    ) -> Result<(), AppError> {
        let server = crate::servers::load(app, server_id)?;
        // 开启入口是用户明确的操作（或程序启动）：之前因为登录上限停下的，照常再去服务器登录一次
        crate::builtin::clear_auth_limit(app, server_id);
        let token = token(server_id)?;
        service.add_secret(token.clone());
        let entry = service.start_http_entry(server_id, token)?;
        // 后面任何一步失败都要把刚起来的本机服务停掉
        if let Err(e) = start_tunnel(app, &server, port, entry.local_port) {
            service.stop_http_entry(server_id);
            return Err(e);
        }
        crate::forwards::ensure_pump(app);
        self.lock().insert(
            server_id.to_owned(),
            Running {
                port,
                state: "connecting".into(),
                error: None,
                file_written: false,
                file_generation: 0,
                file_present: false,
            },
        );
        eprintln!(
            "开启 AI 接入服务器入口: 服务器={server_id}, 服务器端口={port}, 本机端口={}",
            entry.local_port
        );
        Ok(())
    }

    /// 关掉一个入口：撤销转发、停本机服务、删服务器上的 mcp.json（尽力而为）。
    /// 在后台线程里调用：删文件要等它做完，免得端口改了以后把新写的文件删掉
    pub fn stop(&self, app: &AppHandle, service: &Arc<McpService>, server_id: &str) {
        let removed = self.lock().remove(server_id);
        if let Some(m) = app.state::<TunnelsState>().existing()
            && let Err(e) = m.stop(&format!("{TUNNEL_PREFIX}{server_id}"))
            && e.code != "tunnel.not_found"
        {
            eprintln!(
                "关闭 AI 接入服务器入口的转发失败: 服务器={server_id}, 错误={}",
                e.code
            );
        }
        service.stop_http_entry(server_id);
        // 正在写的那一次写完再删；写的那边发现入口已经不在了会自己再删一次
        let file = self.file_lock(server_id);
        let _file = file.lock().unwrap_or_else(|p| p.into_inner());
        if removed.is_some_and(|r| r.file_written || r.file_present)
            && let Err(e) = run_on_server(
                app,
                server_id,
                &pierlink_core::mcp::token::remove_token_file_script(),
            )
        {
            eprintln!(
                "删除服务器上的 AI 接入地址文件失败: 服务器={server_id}, 错误={}",
                e.code
            );
        }
        eprintln!("关闭 AI 接入服务器入口: 服务器={server_id}");
    }

    /// 换令牌：存新令牌、让本机服务只认新令牌、重写服务器上的文件
    pub fn rotate(
        &self,
        app: &AppHandle,
        service: &Arc<McpService>,
        server_id: &str,
    ) -> Result<(), AppError> {
        let token = pierlink_core::mcp::token::new_token();
        crate::sync::credential_store()?.set(&token_key(server_id), &token)?;
        service.add_secret(token.clone());
        service.set_token(server_id, token);
        let connected = {
            let mut running = self.lock();
            match running.get_mut(server_id) {
                Some(r) => {
                    r.file_written = false;
                    r.file_generation += 1;
                    r.state == "running"
                }
                None => false,
            }
        };
        if connected {
            self.write_file(app, server_id);
        }
        super::install::reinstall_after_rotate(app, server_id);
        Ok(())
    }

    /// 端口转发的事件循环转交过来的隧道状态
    pub fn on_tunnel_event(&self, app: &AppHandle, event: &TunnelEvent) {
        let Some(server_id) = event.tunnel_id.strip_prefix(TUNNEL_PREFIX) else {
            return;
        };
        let write = {
            let mut running = self.lock();
            let Some(r) = running.get_mut(server_id) else {
                return;
            };
            r.state = match event.state {
                TunnelState::Connecting => "connecting",
                TunnelState::Connected => "running",
                TunnelState::Reconnecting => "reconnecting",
                TunnelState::WaitingPassword => "waiting_password",
                TunnelState::Stopped => "stopped",
                TunnelState::Error => "error",
            }
            .into();
            r.error = event.error.clone();
            event.state == TunnelState::Connected && !r.file_written
        };
        if write {
            self.write_file(app, server_id);
        }
        emit(app, self.states());
    }

    /// 在后台把地址和令牌写到服务器的 ~/.pierlink/mcp.json
    fn write_file(&self, app: &AppHandle, server_id: &str) {
        let (app, id) = (app.clone(), server_id.to_owned());
        std::thread::spawn(move || {
            let Some(state) = app.try_state::<super::McpState>() else {
                return;
            };
            let file = state.entries.file_lock(&id);
            let _file = file.lock().unwrap_or_else(|p| p.into_inner());
            // 拿到写删锁之后再看：入口已经关了就不写
            let Some((port, generation)) = state
                .entries
                .lock()
                .get(&id)
                .map(|r| (r.port, r.file_generation))
            else {
                return;
            };
            let result = token(&id).and_then(|token| {
                run_on_server(
                    &app,
                    &id,
                    &pierlink_core::mcp::token::token_file_script(&url(port), &token),
                )
            });
            match result {
                Ok(()) => {
                    let still_running = match state.entries.lock().get_mut(&id) {
                        Some(r) => {
                            // 写的途中换过令牌：服务器上是旧令牌，不算写好，下次连上或换令牌那边会再写
                            r.file_written = r.file_generation == generation;
                            r.file_present = true;
                            true
                        }
                        None => false,
                    };
                    if still_running {
                        eprintln!("已在服务器上写好 AI 接入地址文件: 服务器={id}");
                    } else if let Err(e) = run_on_server(
                        &app,
                        &id,
                        &pierlink_core::mcp::token::remove_token_file_script(),
                    ) {
                        // 写的时候入口被关了：令牌不能留在服务器上
                        eprintln!(
                            "入口已关闭，删除刚写的 AI 接入地址文件失败: 服务器={id}, 错误={}",
                            e.code
                        );
                    }
                }
                Err(e) => {
                    eprintln!(
                        "在服务器上写 AI 接入地址文件失败: 服务器={id}, 错误={}",
                        e.code
                    );
                    if let Some(r) = state.entries.lock().get_mut(&id) {
                        r.error = Some(e);
                    }
                }
            }
            emit(&app, state.entries.states());
        });
    }
}

/// 在这台服务器上建远程转发：服务器 127.0.0.1:port → 本机 local_port
fn start_tunnel(
    app: &AppHandle,
    server: &pierlink_core::storage::Server,
    port: u16,
    local_port: u16,
) -> Result<(), AppError> {
    let target = crate::servers::target(server)?;
    let opts = crate::keys::ssh_opts(app, server, crate::keys::Access::Background("AI 接入"))?;
    let spec = TunnelSpec {
        builtin: None,
        id: format!("{TUNNEL_PREFIX}{}", server.id),
        server_id: server.id.clone(),
        target,
        options: opts,
        forwards: vec![TunnelForward::Remote {
            remote_port: port,
            local_port,
        }],
        readiness: Readiness::AuthMarker,
    };
    // 用内置 SSH 的服务器：转发开在主连接上（和终端共用一次登录）
    let spec = crate::builtin::tunnel(
        app,
        server,
        pierlink_core::ssh::engine::Feature::PortForward,
        pierlink_core::builtin::ConnectionClass::Main,
        spec,
    )?;
    app.state::<TunnelsState>()
        .get(app)?
        .start(spec)
        .map(|_| ())
}

/// 在服务器上执行一段脚本（后台方式：不弹密码框；内置 SSH 走主连接）
fn run_on_server(app: &AppHandle, server_id: &str, script: &str) -> Result<(), AppError> {
    let (program, cmd, _) = crate::deploy::background_command(
        app,
        server_id,
        crate::keys::Access::Background("AI 接入"),
    )?;
    pierlink_core::remote::run::run_script(
        &program,
        &cmd,
        script,
        pierlink_core::remote::run::RunLimits {
            timeout: Duration::from_secs(30),
            stdout_bytes: 64 * 1024,
            stderr_bytes: 4096,
        },
    )
    .map(|_| ())
}

fn emit(app: &AppHandle, states: Vec<McpEntryStateDto>) {
    if let Err(e) = app.emit(super::ENTRIES_EVENT, states) {
        eprintln!("推送 AI 接入服务器入口状态失败: 错误={e}");
    }
}
