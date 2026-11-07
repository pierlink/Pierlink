//! 服务器入口：本机 127.0.0.1 随机端口上的 HTTP 服务 + 这台服务器上的远程转发
//! （服务器 127.0.0.1:端口 → 本机端口）+ 服务器上的 `~/.pierlink/mcp.json`（地址和令牌）。
//! 隧道复用端口转发的那一套（断线重连、等待密码、网络恢复），状态事件由端口转发的事件循环转交过来。
//!
//! 几台电脑同时接入同一台服务器：先占着设置端口的是主入口（AI 客户端连它，写 mcp.json）；后来的电脑
//! 发现端口被另一台电脑的 Pierlink 占着时自动换一个空闲端口接入（从属入口，不写 mcp.json）。每台都在服务器上
//! 写自己的伙伴文件 `~/.pierlink/peers/<实例号>.json`，主入口据此把 AI 的操作转给用户眼前那台。
//! 主入口那台关掉后，从属入口接过设置的端口和 mcp.json 里的令牌（AI 客户端的配置不用改）。
use crate::tunnels::TunnelsState;
use pierlink_core::error::AppError;
use pierlink_core::mcp::McpService;
use pierlink_core::security::vault::Zeroizing;
use pierlink_core::ssh::args::ForwardTarget;
use pierlink_core::tunnel::{Readiness, TunnelEvent, TunnelForward, TunnelSpec, TunnelState};
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// 服务器入口的隧道 ID 前缀（和端口转发共用一个隧道管理器）
pub(crate) const TUNNEL_PREFIX: &str = "mcp:";
/// 端口一直被占着时，隔多久再去服务器上查一次是谁占着
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// 从属入口隔多久看一次设置的端口空出来没有（主入口那台关了就接过来）
const TAKEOVER_EVERY: Duration = Duration::from_secs(60);

/// 服务器上的端口被占着时查到的情况
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpPortConflictDto {
    /// this_pc（这台电脑的另一条连接）/ other_pc（另一台电脑上的 Pierlink）/ stale（没断干净的旧连接）/
    /// other_program（其他程序或其他用户）/ not_listening（没人在听，服务器却拒绝了转发）/ unknown（查不出来）
    pub kind: String,
    /// 另一台电脑的名字（知道的话）
    pub computer: Option<String>,
    /// 建议换到的空闲端口
    pub free_port: Option<u16>,
}

/// 换令牌或端口后，在服务器上自动重新添加（一键添加过的客户端）的结果
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpReaddDto {
    /// token / port
    pub reason: String,
    /// 更新好的客户端（claude / codex / gemini）
    pub ok: Vec<String>,
    /// 没更新成的客户端
    pub failed: Vec<String>,
    /// 完成的时间（Unix 毫秒）
    pub at_ms: f64,
}

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
    /// 端口被占着时查到的情况（state 为 port_busy 且查过时才有）
    pub conflict: Option<McpPortConflictDto>,
    /// 最近一次自动重新添加的结果
    pub readded: Option<McpReaddDto>,
    /// 另一台电脑占着设置的端口，这台换了一个端口接入（从属入口）；主入口为 null
    pub secondary: Option<McpSecondaryDto>,
}

/// 从属入口：经另一台电脑的主入口接入
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpSecondaryDto {
    /// 占着设置端口的那台电脑（知道的话）
    pub via: Option<String>,
    /// 这台在服务器上实际监听的端口
    pub port: u16,
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
    conflict: Option<McpPortConflictDto>,
    /// 上次去服务器上查端口的时间（一直被占着时隔 CHECK_EVERY 再查）
    checked_at: Option<Instant>,
    readded: Option<McpReaddDto>,
    /// 本机 HTTP 服务的端口（换服务器上的端口时用它重开转发）
    local_port: u16,
    /// 从属入口（换了端口接入）
    secondary: Option<McpSecondaryDto>,
    /// 服务器上写过这台电脑的伙伴文件：关掉入口时要删
    peer_present: bool,
    /// 盯着主入口什么时候关掉的那个后台线程的编号（入口重开后旧线程自己退出）
    watch: u64,
}

/// 后台线程编号
static WATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Running {
    fn new(port: u16, local_port: u16, state: &str, error: Option<AppError>) -> Self {
        Self {
            port,
            state: state.into(),
            error,
            file_written: false,
            file_generation: 0,
            file_present: false,
            conflict: None,
            checked_at: None,
            readded: None,
            local_port,
            secondary: None,
            peer_present: false,
            watch: 0,
        }
    }

    /// 在服务器上实际监听的端口：从属入口是换过的那个
    fn actual_port(&self) -> u16 {
        self.secondary.as_ref().map_or(self.port, |s| s.port)
    }
}

/// 正在运行的服务器入口
#[derive(Default)]
pub struct Entries {
    /// 这个 Pierlink 的实例号（写进服务器上的地址文件；只删自己写的文件）
    instance: String,
    running: Mutex<HashMap<String, Running>>,
    /// 设置连续保存时，后台的 apply 一个接一个做，免得后一次把前一次刚开的入口关掉
    applying: Mutex<()>,
    /// 每台服务器上 mcp.json 的写和删排队做，免得删在前、晚到的写在后，留下一份已经停掉的入口的令牌
    files: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// 到别的电脑入口的本地转发：（服务器 ID, 服务器上的端口）→ 本机端口
    peer_tunnels: Mutex<HashMap<(String, u16), u16>>,
}

fn token_key(server_id: &str) -> String {
    format!("pierlink/mcp/{server_id}/token")
}

/// 令牌的读、建、换排队做：第一次添加入口时，开入口和设置页的检测会同时来要令牌，
/// 不排队的话两边各建一个、后存的盖掉先存的，服务器上写的令牌就和正在运行的入口对不上（AI 连上来被拒）
static TOKENS: Mutex<()> = Mutex::new(());

fn tokens_lock() -> MutexGuard<'static, ()> {
    TOKENS.lock().unwrap_or_else(|p| p.into_inner())
}

/// 这台服务器入口的令牌：凭据管理器里有就用，没有就新建一个存进去
pub(super) fn token(server_id: &str) -> Result<Zeroizing<String>, AppError> {
    let _tokens = tokens_lock();
    token_locked(server_id)
}

/// token() 的正文：调用方已经拿着 TOKENS
fn token_locked(server_id: &str) -> Result<Zeroizing<String>, AppError> {
    let store = crate::sync::credential_store()?;
    if let Some(token) = store.get(&token_key(server_id))? {
        return Ok(Zeroizing::new(token));
    }
    let token = pierlink_core::mcp::token::new_token();
    store.set(&token_key(server_id), &token)?;
    Ok(token)
}

/// 凭据管理器里存着的令牌（只读：没有就是没有，不像 token() 那样新建一个）；删服务器上的文件时比较用
fn saved_token(server_id: &str) -> Option<Zeroizing<String>> {
    crate::sync::credential_store()
        .and_then(|store| store.get(&token_key(server_id)))
        .ok()
        .flatten()
        .map(Zeroizing::new)
}

/// 服务器上 agent 用的地址
pub fn url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/mcp")
}

impl Entries {
    /// :param instance: 这个 Pierlink 的实例号
    pub fn new(instance: String) -> Self {
        Self {
            instance,
            ..Default::default()
        }
    }

    /// 这个 Pierlink 的实例号
    pub(super) fn instance(&self) -> &str {
        &self.instance
    }

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
                conflict: r.conflict.clone(),
                readded: r.readded.clone(),
                secondary: r.secondary.clone(),
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
        let mut moved = Vec::new();
        for (id, port, failed) in &current {
            if wanted.get(id) != Some(port) || *failed {
                if wanted.get(id).is_some_and(|p| p != port) {
                    moved.push(id.clone());
                }
                self.stop(app, service, id);
            }
        }
        for (id, port) in wanted {
            if self.lock().contains_key(id) {
                continue;
            }
            if let Err(e) = self.start(app, service, id, *port) {
                eprintln!("开启 AI 接入服务器入口失败: 服务器={id}, 错误={}", e.code);
                self.lock()
                    .insert(id.clone(), Running::new(*port, 0, "error", Some(e)));
            }
        }
        emit(app, self.states());
        // 换了端口：服务器上一键添加过的客户端用新地址重新添加（配置里写着端口）
        for id in moved {
            super::install::readd(app, &id, "port");
        }
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
        // 取令牌和用它开本机服务在同一把锁里：换令牌插不进两步中间（插进来的话新令牌存下了，本机服务却还认旧的）
        let entry = {
            let _tokens = tokens_lock();
            let token = token_locked(server_id)?;
            service.add_secret(token.clone());
            service.start_http_entry(server_id, token)?
        };
        // 后面任何一步失败都要把刚起来的本机服务停掉
        if let Err(e) = start_tunnel(app, &server, port, entry.local_port) {
            service.stop_http_entry(server_id);
            return Err(e);
        }
        crate::forwards::ensure_pump(app);
        self.lock().insert(
            server_id.to_owned(),
            Running::new(port, entry.local_port, "connecting", None),
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
        self.stop_peer_tunnels(app, server_id);
        // 正在写的那一次写完再删；写的那边发现入口已经不在了会自己再删一次
        let file = self.file_lock(server_id);
        let _file = file.lock().unwrap_or_else(|p| p.into_inner());
        if removed.as_ref().is_some_and(|r| r.peer_present)
            && let Err(e) = run_on_server(
                app,
                server_id,
                &pierlink_core::mcp::peers::remove_peer_file_script(&self.instance),
            )
        {
            eprintln!(
                "删除服务器上的伙伴文件失败: 服务器={server_id}, 错误={}",
                e.code
            );
        }
        let current = saved_token(server_id);
        if removed.is_some_and(|r| r.file_written || r.file_present)
            && let Err(e) = run_on_server(
                app,
                server_id,
                &pierlink_core::mcp::token::remove_token_file_script(
                    &self.instance,
                    current.as_deref().map(String::as_str).unwrap_or_default(),
                ),
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
        {
            let _tokens = tokens_lock();
            let token = pierlink_core::mcp::token::new_token();
            crate::sync::credential_store()?.set(&token_key(server_id), &token)?;
            service.add_secret(token.clone());
            service.set_token(server_id, token);
        }
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
            self.write_peer_file(app, server_id);
        }
        super::install::readd(app, server_id, "token");
        Ok(())
    }

    /// 记下自动重新添加的结果，推给界面
    pub(super) fn set_readded(&self, app: &AppHandle, server_id: &str, readded: McpReaddDto) {
        if let Some(r) = self.lock().get_mut(server_id) {
            r.readded = Some(readded);
        }
        emit(app, self.states());
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
            // 服务器上的端口被占着（SSH 是通的，只是转发被拒）：不说“正在重连”，说“端口被占用”，并去查是谁占着
            let busy = event
                .error
                .as_ref()
                .is_some_and(|e| e.code == "tunnel.remote_forward_failed");
            r.state = match event.state {
                TunnelState::Connecting => "connecting",
                TunnelState::Connected => "running",
                TunnelState::Reconnecting if busy => "port_busy",
                TunnelState::Reconnecting => "reconnecting",
                TunnelState::WaitingPassword => "waiting_password",
                TunnelState::Stopped => "stopped",
                TunnelState::Error => "error",
            }
            .into();
            r.error = event.error.clone();
            if !busy {
                r.conflict = None;
                r.checked_at = None;
            }
            let check = busy && r.checked_at.is_none_or(|t| t.elapsed() >= CHECK_EVERY);
            if check {
                r.checked_at = Some(Instant::now());
            }
            let connected = event.state == TunnelState::Connected;
            (
                // 从属入口不写 mcp.json（AI 客户端连的是主入口）
                connected && !r.file_written && r.secondary.is_none(),
                connected,
                check.then_some(r.actual_port()),
            )
        };
        let (write, peer, check) = (write.0, write.1, write.2);
        if write {
            self.write_file(app, server_id);
        }
        if peer {
            self.write_peer_file(app, server_id);
        }
        if let Some(port) = check {
            self.check_port(app, server_id, port);
        }
        emit(app, self.states());
    }

    /// 服务器上的端口被占着：在后台到服务器上查是谁占着（另一台电脑的 Pierlink、没断干净的旧连接、其他程序），
    /// 顺便找一个空闲端口，结果放进入口状态
    fn check_port(&self, app: &AppHandle, server_id: &str, port: u16) {
        let (app, id) = (app.clone(), server_id.to_owned());
        std::thread::spawn(move || {
            let Some(state) = app.try_state::<super::McpState>() else {
                return;
            };
            let script = pierlink_core::mcp::port_check::port_check_script(port);
            let found = match run_on_server_value(&app, &id, &script, Duration::from_secs(30)) {
                Ok(v) => pierlink_core::mcp::port_check::classify(&v, &state.entries.instance),
                Err(e) => {
                    eprintln!(
                        "查服务器端口被谁占着失败: 服务器={id}, 端口={port}, 错误={}",
                        e.code
                    );
                    // 查不出来也要告诉界面，别一直显示“正在查”
                    state
                        .entries
                        .set_conflict(&app, &id, port, "unknown", None, None);
                    return;
                }
            };
            eprintln!(
                "查服务器端口被谁占着: 服务器={id}, 端口={port}, 结果={:?}, 空闲端口={:?}",
                found.occupant, found.free_port
            );
            use pierlink_core::mcp::port_check::Occupant;
            // 占着的是另一台电脑上的 Pierlink：换一个空闲端口接入，不报冲突
            if found.occupant == Occupant::OtherPc
                && let Some(free) = found.free_port
            {
                state
                    .entries
                    .become_secondary(&app, &id, port, free, found.computer);
                return;
            }
            let kind = match found.occupant {
                Occupant::ThisPc => "this_pc",
                Occupant::OtherPc => "other_pc",
                Occupant::Stale => "stale",
                Occupant::OtherProgram => "other_program",
                Occupant::NotListening => "not_listening",
            };
            state
                .entries
                .set_conflict(&app, &id, port, kind, found.computer, found.free_port);
        });
    }

    /// 记下查到的端口占用情况，推给界面；查的途中换了端口或已经连上时作废
    fn set_conflict(
        &self,
        app: &AppHandle,
        server_id: &str,
        port: u16,
        kind: &str,
        computer: Option<String>,
        free_port: Option<u16>,
    ) {
        {
            let mut running = self.lock();
            let Some(r) = running.get_mut(server_id) else {
                return;
            };
            if r.actual_port() != port || r.state != "port_busy" {
                return;
            }
            r.conflict = Some(McpPortConflictDto {
                kind: kind.into(),
                computer,
                free_port,
            });
        }
        emit(app, self.states());
    }

    /// 入口在服务器上实际监听的端口（从属入口是换过的那个）；还没连上为 None
    ///
    /// :param server_id: 服务器 ID
    /// :return: 端口
    pub(super) fn actual_port(&self, server_id: &str) -> Option<u16> {
        self.lock()
            .get(server_id)
            .filter(|r| r.state == "running")
            .map(Running::actual_port)
    }

    /// 设置的端口被另一台电脑上的 Pierlink 占着：换到空闲端口接入（从属入口），并开始盯着设置的端口什么时候空出来
    ///
    /// :param busy: 查的是哪个端口（查的途中换过端口、已经连上就作废）
    /// :param free: 换到的空闲端口
    /// :param via: 占着端口的那台电脑的名字
    fn become_secondary(
        &self,
        app: &AppHandle,
        server_id: &str,
        busy: u16,
        free: u16,
        via: Option<String>,
    ) {
        let _applying = self.applying.lock().unwrap_or_else(|p| p.into_inner());
        let (local, watch) = {
            let mut running = self.lock();
            let Some(r) = running.get_mut(server_id) else {
                return;
            };
            if r.actual_port() != busy || r.state != "port_busy" || r.local_port == 0 {
                return;
            }
            r.secondary = Some(McpSecondaryDto {
                via: via.clone(),
                port: free,
            });
            r.state = "connecting".into();
            r.error = None;
            r.conflict = None;
            r.checked_at = None;
            // 服务器上的 mcp.json 现在是占着端口那台的：关掉入口时不能当自己的删掉
            r.file_written = false;
            r.file_present = false;
            let watch = if r.watch == 0 {
                r.watch = WATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Some(r.watch)
            } else {
                None
            };
            (r.local_port, watch)
        };
        eprintln!(
            "服务器上的端口被另一台电脑占着，换端口接入: 服务器={server_id}, 原端口={busy}, 新端口={free}, 占着的电脑={}",
            via.as_deref().unwrap_or("未知")
        );
        match restart_tunnel(app, server_id, free, local) {
            Ok(()) => {
                if let Some(watch) = watch {
                    watch_primary(app, server_id, watch);
                }
            }
            Err(e) => {
                eprintln!(
                    "换端口接入失败: 服务器={server_id}, 端口={free}, 错误={}",
                    e.code
                );
                if let Some(r) = self.lock().get_mut(server_id) {
                    r.state = "error".into();
                    r.error = Some(e);
                }
            }
        }
        emit(app, self.states());
    }

    /// 设置的端口空出来了（主入口那台关了）：接过来当主入口。先接过 mcp.json 里 AI 客户端在用的令牌，
    /// 连上后照常写 mcp.json
    fn become_primary(&self, app: &AppHandle, service: &Arc<McpService>, server_id: &str) {
        let _applying = self.applying.lock().unwrap_or_else(|p| p.into_inner());
        let Some((port, local, secondary)) = self
            .lock()
            .get(server_id)
            .filter(|r| r.secondary.is_some())
            .map(|r| (r.port, r.local_port, r.secondary.clone()))
        else {
            return;
        };
        match run_on_server_value(
            app,
            server_id,
            &pierlink_core::mcp::token::read_token_file_script(),
            Duration::from_secs(30),
        ) {
            Ok(file) => {
                let theirs = file["instance"].as_str().unwrap_or_default();
                if let Some(token) = file["token"].as_str().filter(|t| {
                    !t.is_empty()
                        && t.len() <= 256
                        && t.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                }) && theirs != self.instance
                    && let Err(e) =
                        adopt_token(service, server_id, Zeroizing::new(token.to_owned()))
                {
                    eprintln!(
                        "接过 AI 客户端在用的令牌失败: 服务器={server_id}, 错误={}",
                        e.code
                    );
                }
            }
            Err(e) => eprintln!(
                "读服务器上的 AI 接入地址文件失败: 服务器={server_id}, 错误={}",
                e.code
            ),
        }
        {
            let mut running = self.lock();
            let Some(r) = running.get_mut(server_id) else {
                return;
            };
            r.secondary = None;
            r.watch = 0;
            r.state = "connecting".into();
            r.error = None;
            r.file_written = false;
            r.file_generation += 1;
        }
        eprintln!("主入口那台已关，接过设置的端口: 服务器={server_id}, 端口={port}");
        if let Err(e) = restart_tunnel(app, server_id, port, local) {
            eprintln!(
                "接过设置的端口失败，回到原来的端口接着等: 服务器={server_id}, 端口={port}, 错误={}",
                e.code
            );
            // 回到从属入口，下一轮再试
            let back = secondary.as_ref().map_or(port, |s| s.port);
            let watch = {
                let mut running = self.lock();
                match running.get_mut(server_id) {
                    Some(r) => {
                        r.secondary = secondary.clone();
                        r.watch = WATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        Some(r.watch)
                    }
                    None => None,
                }
            };
            match restart_tunnel(app, server_id, back, local) {
                Ok(()) => {
                    if let Some(watch) = watch {
                        watch_primary(app, server_id, watch);
                    }
                }
                Err(e) => {
                    if let Some(r) = self.lock().get_mut(server_id) {
                        r.state = "error".into();
                        r.error = Some(e);
                    }
                }
            }
        }
        emit(app, self.states());
    }

    /// 在后台把这台电脑的伙伴文件（端口、令牌、指定）写到服务器上；入口已经关了就不写
    pub(super) fn write_peer_file(&self, app: &AppHandle, server_id: &str) {
        let (app, id) = (app.clone(), server_id.to_owned());
        std::thread::spawn(move || {
            let Some(state) = app.try_state::<super::McpState>() else {
                return;
            };
            let file = state.entries.file_lock(&id);
            let _file = file.lock().unwrap_or_else(|p| p.into_inner());
            let Some(port) = state.entries.actual_port(&id) else {
                return;
            };
            let computer = crate::sync::computer_name();
            let pin = super::settings::load_pin(&app);
            let written_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default();
            let result = token(&id).and_then(|token| {
                run_on_server(
                    &app,
                    &id,
                    &pierlink_core::mcp::peers::peer_file_script(
                        &state.entries.instance,
                        &computer,
                        port,
                        &token,
                        written_at,
                        pin.as_ref(),
                    ),
                )
            });
            match result {
                Ok(()) => {
                    let still = match state.entries.lock().get_mut(&id) {
                        Some(r) => {
                            r.peer_present = true;
                            true
                        }
                        None => false,
                    };
                    if still {
                        eprintln!("已在服务器上写好伙伴文件: 服务器={id}, 端口={port}");
                    } else if let Err(e) = run_on_server(
                        &app,
                        &id,
                        &pierlink_core::mcp::peers::remove_peer_file_script(
                            &state.entries.instance,
                        ),
                    ) {
                        // 写的时候入口被关了：令牌不能留在服务器上
                        eprintln!(
                            "入口已关闭，删除刚写的伙伴文件失败: 服务器={id}, 错误={}",
                            e.code
                        );
                    }
                }
                Err(e) => eprintln!("在服务器上写伙伴文件失败: 服务器={id}, 错误={}", e.code),
            }
        });
    }

    /// 退出时删掉服务器上这台电脑的伙伴文件：几台服务器同时删，最多等 4 秒（不拖慢退出）
    pub(super) fn remove_peer_files(&self, app: &AppHandle) {
        let ids: Vec<String> = self
            .lock()
            .iter()
            .filter(|(_, r)| r.peer_present)
            .map(|(id, _)| id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let (done, finished) = std::sync::mpsc::channel();
        for id in &ids {
            let (app, id, done) = (app.clone(), id.clone(), done.clone());
            let script = pierlink_core::mcp::peers::remove_peer_file_script(&self.instance);
            std::thread::spawn(move || {
                if let Err(e) = run_on_server_value(&app, &id, &script, Duration::from_secs(3)) {
                    eprintln!(
                        "退出时删除服务器上的伙伴文件失败: 服务器={id}, 错误={}",
                        e.code
                    );
                }
                let _ = done.send(());
            });
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        for _ in &ids {
            let left = deadline.saturating_duration_since(Instant::now());
            if finished.recv_timeout(left).is_err() {
                eprintln!("退出时删除伙伴文件没等完: 服务器数={}", ids.len());
                break;
            }
        }
    }

    /// 所有连着的入口重写伙伴文件（用户改了“AI 操作哪台电脑”、定时刷新）
    pub(super) fn rewrite_peer_files(&self, app: &AppHandle) {
        for id in self.running_ids() {
            self.write_peer_file(app, &id);
        }
    }

    /// 已经连上的入口的服务器 ID
    pub(super) fn running_ids(&self) -> Vec<String> {
        self.lock()
            .iter()
            .filter(|(_, r)| r.state == "running")
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 本机到服务器上 127.0.0.1:port（另一台电脑的入口）的本地转发端口：有就复用，没有或断了就重建
    ///
    /// :param server_id: 服务器 ID
    /// :param port: 服务器上的端口
    /// :return: 本机端口
    pub(super) fn peer_tunnel(
        &self,
        app: &AppHandle,
        server_id: &str,
        port: u16,
    ) -> Result<u16, AppError> {
        let key = (server_id.to_owned(), port);
        let cached = self
            .peer_tunnels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .copied();
        if let Some(local) = cached
            && reachable(local)
        {
            return Ok(local);
        }
        let id = format!("{TUNNEL_PREFIX}peer:{server_id}:{port}");
        let manager = app.state::<TunnelsState>().get(app)?;
        if let Err(e) = manager.stop(&id)
            && e.code != "tunnel.not_found"
        {
            eprintln!("关闭到另一台电脑的旧转发失败: 隧道={id}, 错误={}", e.code);
        }
        let server = crate::servers::load(app, server_id)?;
        let target = crate::servers::target(&server)?;
        let opts = crate::keys::ssh_opts(app, &server, crate::keys::Access::Background("AI 接入"))?;
        let spec = TunnelSpec {
            builtin: None,
            id: id.clone(),
            server_id: server.id.clone(),
            target,
            options: opts,
            forwards: vec![TunnelForward::Local {
                local_port: None,
                remote: ForwardTarget::Tcp { port },
                replace_on_conflict: true,
            }],
            readiness: Readiness::AuthMarker,
        };
        let spec = crate::builtin::tunnel(
            app,
            &server,
            pierlink_core::ssh::engine::Feature::PortForward,
            pierlink_core::builtin::ConnectionClass::Main,
            spec,
        )?;
        let local = manager
            .start(spec)?
            .first()
            .copied()
            .ok_or_else(|| AppError::new("mcp.peer_unreachable"))?;
        crate::forwards::ensure_pump(app);
        self.peer_tunnels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key, local);
        eprintln!(
            "开好到另一台电脑入口的转发: 服务器={server_id}, 服务器端口={port}, 本机端口={local}"
        );
        // 系统 OpenSSH 要先登录才开始监听：等它能连上
        let deadline = Instant::now() + Duration::from_secs(10);
        while !reachable(local) {
            if Instant::now() >= deadline {
                return Err(AppError::new("mcp.peer_unreachable")
                    .with_reason("mcp.peer_connection_failed", "连不上另一台电脑的 Pierlink"));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(local)
    }

    /// 关掉这台服务器上到别的电脑入口的转发（入口停用时）
    fn stop_peer_tunnels(&self, app: &AppHandle, server_id: &str) {
        let ports: Vec<u16> = {
            let mut tunnels = self.peer_tunnels.lock().unwrap_or_else(|p| p.into_inner());
            let keys: Vec<(String, u16)> = tunnels
                .keys()
                .filter(|(id, _)| id == server_id)
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|key| tunnels.remove(&key).map(|_| key.1))
                .collect()
        };
        if let Some(m) = app.state::<TunnelsState>().existing() {
            for port in ports {
                if let Err(e) = m.stop(&format!("{TUNNEL_PREFIX}peer:{server_id}:{port}"))
                    && e.code != "tunnel.not_found"
                {
                    eprintln!(
                        "关闭到另一台电脑的转发失败: 服务器={server_id}, 端口={port}, 错误={}",
                        e.code
                    );
                }
            }
        }
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
            let computer = crate::sync::computer_name();
            let owner = pierlink_core::mcp::token::FileOwner {
                instance: &state.entries.instance,
                computer: &computer,
            };
            let result = token(&id).and_then(|token| {
                run_on_server(
                    &app,
                    &id,
                    &pierlink_core::mcp::token::token_file_script(&url(port), &token, owner),
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
                        &pierlink_core::mcp::token::remove_token_file_script(
                            &state.entries.instance,
                            saved_token(&id)
                                .as_deref()
                                .map(String::as_str)
                                .unwrap_or_default(),
                        ),
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

/// 停掉这个入口的远程转发，按 port 重开（本机 HTTP 服务不动）
fn restart_tunnel(app: &AppHandle, server_id: &str, port: u16, local: u16) -> Result<(), AppError> {
    let server = crate::servers::load(app, server_id)?;
    if let Some(m) = app.state::<TunnelsState>().existing()
        && let Err(e) = m.stop(&format!("{TUNNEL_PREFIX}{server_id}"))
        && e.code != "tunnel.not_found"
    {
        return Err(e);
    }
    start_tunnel(app, &server, port, local)
}

/// 从属入口：每分钟看一次设置的端口空出来没有；空出来了就接过来当主入口。入口重开或已经接过来时退出
fn watch_primary(app: &AppHandle, server_id: &str, watch: u64) {
    let (app, id) = (app.clone(), server_id.to_owned());
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(TAKEOVER_EVERY);
            let Some(state) = app.try_state::<super::McpState>() else {
                return;
            };
            let Some(port) = state
                .entries
                .lock()
                .get(&id)
                .filter(|r| r.secondary.is_some() && r.watch == watch)
                .map(|r| r.port)
            else {
                return;
            };
            let script = pierlink_core::mcp::port_check::port_check_script(port);
            let found = match run_on_server_value(&app, &id, &script, Duration::from_secs(30)) {
                Ok(v) => pierlink_core::mcp::port_check::classify(&v, &state.entries.instance),
                Err(e) => {
                    eprintln!(
                        "查主入口还在不在失败: 服务器={id}, 端口={port}, 错误={}",
                        e.code
                    );
                    continue;
                }
            };
            use pierlink_core::mcp::port_check::Occupant;
            match found.occupant {
                Occupant::NotListening => state.entries.become_primary(&app, &state.service, &id),
                Occupant::OtherPc => {
                    // 主入口还在：显示的电脑名跟着更新（那台改过名、或换了一台）
                    let changed = {
                        let mut running = state.entries.lock();
                        match running.get_mut(&id).and_then(|r| r.secondary.as_mut()) {
                            Some(secondary)
                                if found.computer.is_some() && secondary.via != found.computer =>
                            {
                                secondary.via = found.computer.clone();
                                true
                            }
                            _ => false,
                        }
                    };
                    if changed {
                        emit(&app, state.entries.states());
                    }
                }
                _ => {}
            }
        }
    });
}

/// 用 AI 客户端在用的令牌（主入口那台写在 mcp.json 里的）作为这台入口的令牌
fn adopt_token(
    service: &Arc<McpService>,
    server_id: &str,
    token: Zeroizing<String>,
) -> Result<(), AppError> {
    let _tokens = tokens_lock();
    crate::sync::credential_store()?.set(&token_key(server_id), &token)?;
    service.add_secret(token.clone());
    service.set_token(server_id, token);
    eprintln!("已接过 AI 客户端在用的令牌: 服务器={server_id}");
    Ok(())
}

/// 本机端口能不能连上
fn reachable(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(300),
    )
    .is_ok()
}

/// 在服务器上执行一段脚本（后台方式：不弹密码框；内置 SSH 走主连接）
fn run_on_server(app: &AppHandle, server_id: &str, script: &str) -> Result<(), AppError> {
    run_on_server_value(app, server_id, script, Duration::from_secs(30)).map(|_| ())
}

/// 在服务器上执行一段脚本，返回它输出的 JSON；timeout 内没做完返回错误
pub(super) fn run_on_server_value(
    app: &AppHandle,
    server_id: &str,
    script: &str,
    timeout: Duration,
) -> Result<serde_json::Value, AppError> {
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
            timeout,
            stdout_bytes: 256 * 1024,
            stderr_bytes: 4096,
        },
    )
}

fn emit(app: &AppHandle, states: Vec<McpEntryStateDto>) {
    if let Err(e) = app.emit(super::ENTRIES_EVENT, states) {
        eprintln!("推送 AI 接入服务器入口状态失败: 错误={e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 同时第一次要令牌时大家拿到同一个且和存下的一致() {
        const N: usize = 16;
        let id = "并发要令牌的服务器";
        let barrier = std::sync::Barrier::new(N);
        let got: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..N)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        token(id).unwrap().to_string()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(got.iter().all(|t| *t == got[0]));
        let saved = saved_token(id).unwrap();
        assert_eq!(saved.as_str(), got[0]);
        // Windows 上是真的凭据管理器：测完删掉
        crate::sync::credential_store()
            .unwrap()
            .delete(&token_key(id))
            .unwrap();
    }
}
