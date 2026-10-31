//! 同步的 Tauri 命令：本机 Syncthing 实例、每台服务器的隧道连接、任务的创建/暂停/扫描/移除与状态。
//! 编排逻辑在 `pierlink_core::sync`；这里负责凭据、端口、数据库记录和应用生命周期。
//!
//! 所有同步操作经同一把锁串行执行（创建、移除等操作本身就需要互斥）；状态查询在锁忙时直接返回“忙”，
//! 由界面稍后重试，不会卡住。

pub mod endpoints;
pub mod extras;
pub mod syncthing_ui;

use crate::data::{DbState, data_dir};
use crate::terminal::find_ssh;
use pierlink_core::deploy;
use pierlink_core::error::AppError;
use pierlink_core::process::SshProgram;
use pierlink_core::security::{CredentialStore, random_secret};
use pierlink_core::ssh::args::{Purpose, build_args};
use pierlink_core::storage::{ConflictPolicy, EndpointMode, SyncEndpoint, SyncTask};
use pierlink_core::sync::{
    self, EndpointSpec, ExistingEndpoint, ExistingTask, IgnoresSource, LocalEnv, LocalSync,
    PathIssue, ServerEndpointInfo, ServerLink, TaskSpec,
};
use pierlink_core::syncthing::folder::FolderKind;
use pierlink_core::syncthing::process::SyncthingBin;
use pierlink_core::tunnel::{TunnelManager, allocate_port};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

#[derive(Default)]
pub struct SyncState {
    inner: Mutex<Inner>,
    /// 预估的取消标志（预估期间持有同步锁，所以取消标志放在锁外）
    estimate_cancel: std::sync::atomic::AtomicBool,
    /// 各任务最近一次查询到的文件总数（删除保护按比例判断用）
    totals: Mutex<HashMap<String, u64>>,
    /// 删除保护
    guard: Mutex<Option<sync::DeletionGuard>>,
    /// 因删除保护被自动暂停的任务（文件夹 ID）；用户手动继续后移除
    guard_paused: Mutex<std::collections::HashSet<String>>,
    /// 事件线程是否在运行
    pump_running: std::sync::atomic::AtomicBool,
    /// 应用正在退出：启动恢复等后台操作看到后停止，不再启动新的进程
    shutting_down: std::sync::atomic::AtomicBool,
}
#[derive(Default)]
struct Inner {
    local: Option<LocalSync>,
    /// 共享的隧道管理器（见 tunnels.rs）；第一次连接服务器时取得
    tunnels: Option<std::sync::Arc<TunnelManager>>,
    links: HashMap<String, ServerLink>,
    /// 每台服务器最近一次建链失败的原因（成功后清掉）：首次就因为缺密码、要验证码连不上时，状态里据此提示
    link_errors: HashMap<String, AppError>,
    /// 本机 Syncthing 管理页的地址、口令和启动时记下的加固基线（只在内存里）
    local_gui: Option<LocalGuiInfo>,
}

/// 本机 Syncthing 管理页需要的信息
pub struct LocalGuiInfo {
    /// http://127.0.0.1:端口
    pub base: String,
    pub password: String,
    /// 启动时从可信配置记下的加固值；核对时用它，不能从可能被改过的配置里重新取
    pub policy: sync::HardeningPolicy,
}

impl SyncState {
    /// 取消进行中的预估（页面重载、关闭向导、退出应用时）
    pub fn cancel_estimate(&self) {
        self.estimate_cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 应用退出时：断开所有服务器连接（关闭隧道），停止本机实例
    pub fn shutdown(&self) {
        // 先让预估尽快结束、让启动恢复停下，再等同步锁，避免退出时卡住
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.cancel_estimate();
        let mut inner = self.lock();
        let links: Vec<ServerLink> = inner.links.drain().map(|(_, l)| l).collect();
        if let Some(tunnels) = &inner.tunnels {
            for link in links {
                let _ = link.disconnect(tunnels);
            }
        }
        if let Some(local) = inner.local.take()
            && let Err(e) = local.stop()
        {
            eprintln!("停止本机同步服务失败: 错误={e:?}");
        }
        inner.local_gui = None;
        inner.tunnels = None;
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// 凭据存储：Windows 用凭据管理器。其他平台只用于开发调试，用进程内共享的内存存储
/// （正式构建只支持 Windows，非 Windows 的发布构建会在编译时报错，见文件末尾）。
pub(crate) fn credential_store() -> Result<Box<dyn CredentialStore>, AppError> {
    #[cfg(windows)]
    {
        Ok(Box::new(
            pierlink_core::security::WindowsCredentialStore::new()?,
        ))
    }
    #[cfg(not(windows))]
    {
        static MEMORY: std::sync::OnceLock<pierlink_core::security::MemoryCredentialStore> =
            std::sync::OnceLock::new();
        struct Shared(&'static pierlink_core::security::MemoryCredentialStore);
        impl CredentialStore for Shared {
            fn get(&self, key: &str) -> Result<Option<String>, AppError> {
                self.0.get(key)
            }
            fn set(&self, key: &str, secret: &str) -> Result<(), AppError> {
                self.0.set(key, secret)
            }
            fn delete(&self, key: &str) -> Result<(), AppError> {
                self.0.delete(key)
            }
        }
        Ok(Box::new(Shared(MEMORY.get_or_init(Default::default))))
    }
}

/// 读取凭据，没有就生成并保存
fn get_or_create(store: &dyn CredentialStore, key: &str) -> Result<String, AppError> {
    if let Some(v) = store.get(key)? {
        return Ok(v);
    }
    let v = random_secret(24);
    store.set(key, &v)?;
    Ok(v)
}

/// 本电脑名称（用作服务器端 Syncthing 里的设备名）
pub(crate) fn computer_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Pierlink".into())
        .chars()
        .take(64)
        .collect()
}

/// 确保本机实例已启动
fn ensure_local<'a>(app: &AppHandle, inner: &'a mut Inner) -> Result<&'a LocalSync, AppError> {
    if inner.local.is_none() {
        if app
            .state::<SyncState>()
            .shutting_down
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(AppError::new("sync.shutting_down"));
        }
        let exe = if cfg!(windows) {
            "syncthing.exe"
        } else {
            "syncthing"
        };
        let bin = app
            .path()
            .resource_dir()
            .map_err(|e| AppError::new("sync.bundle_missing").with("reason", e.to_string()))?
            .join("syncthing")
            .join(exe);
        if !bin.is_file() {
            return Err(AppError::new("sync.bundle_missing").with("path", bin.display()));
        }
        let store = credential_store()?;
        let api_key = get_or_create(store.as_ref(), "pierlink/local/syncthing-apikey")?;
        let password = get_or_create(store.as_ref(), "pierlink/local/syncthing-gui-password")?;
        let home = data_dir(app)?.join("syncthing");
        let gui_port = allocate_port()?;
        let mut listen_port = allocate_port()?;
        if listen_port == gui_port {
            listen_port = allocate_port()?;
        }
        let local = LocalSync::start(
            &SyncthingBin { path: bin },
            &home,
            gui_port,
            listen_port,
            &api_key,
            &password,
        )?;
        let pump = sync::EventPump::new(&local);
        // 管理页的加固基线：取不到不影响同步，只是管理页打不开
        let spec = pierlink_core::syncthing::home::HomeSpec {
            home: home.clone(),
            gui: pierlink_core::syncthing::home::GuiListen::Tcp { port: gui_port },
            gui_user: "pierlink".into(),
            listen_port,
        };
        inner.local_gui = match sync::capture_hardening_policy(local.client(), &spec) {
            Ok(policy) => Some(LocalGuiInfo {
                base: format!("http://127.0.0.1:{gui_port}"),
                password: password.clone(),
                policy,
            }),
            Err(e) => {
                eprintln!("记录本机 Syncthing 加固基线失败: 错误={}", e.code);
                None
            }
        };
        inner.local = Some(local);
        start_event_thread(app, pump);
    }
    inner
        .local
        .as_ref()
        .ok_or_else(|| AppError::new("sync.local_unavailable"))
}

/// 确保到某台服务器的连接（隧道 + 身份核对 + 配对）已建立
/// 确保和这台服务器的同步链接可用（没有或失效时重建），并记下失败原因
fn ensure_link(app: &AppHandle, inner: &mut Inner, server_id: &str) -> Result<(), AppError> {
    let result = connect_link(app, inner, server_id);
    match &result {
        Ok(()) => {
            inner.link_errors.remove(server_id);
        }
        Err(e) => {
            inner.link_errors.insert(server_id.to_owned(), e.clone());
        }
    }
    result
}

fn connect_link(app: &AppHandle, inner: &mut Inner, server_id: &str) -> Result<(), AppError> {
    if let Some(link) = inner.links.get(server_id) {
        if link.client().ping().is_ok() {
            return Ok(());
        }
        // 连接已失效（隧道被关闭、断网后未恢复等）：丢掉重建
        drop_link(inner, server_id);
    }
    if inner.tunnels.is_none() {
        inner.tunnels = Some(app.state::<crate::tunnels::TunnelsState>().get(app)?);
    }
    let server = crate::servers::load(app, server_id)?;
    let instance = app
        .state::<DbState>()
        .with(|db| db.get_instance(server_id))?
        .ok_or_else(|| AppError::new("sync.not_deployed").with("server", &server.name))?;
    let target = crate::servers::target(&server)?;
    let store = credential_store()?;
    let api_key = match store.get(&instance.apikey_cred)? {
        Some(k) => k,
        None => {
            // 本机没有密钥（别的电脑部署的实例、或重装过）：从服务器读取现行密钥
            let access = crate::keys::Access::Background("文件同步");
            let cmd = crate::builtin::background(app, &server, access, || {
                build_args(
                    &target,
                    &Purpose::Background,
                    &crate::keys::ssh_opts(app, &server, access)?,
                )
            })?;
            let key = deploy::read_api_key(
                &SshProgram {
                    path: find_ssh(app)?,
                },
                &cmd,
                &instance.instance_id,
            )?;
            store.set(&instance.apikey_cred, &key)?;
            key
        }
    };
    let info = ServerEndpointInfo {
        server_id: server_id.to_owned(),
        name: server.name.clone(),
        device_id: instance.device_id.clone(),
        socket_path: instance.socket_path.clone(),
        sync_port: instance.sync_port,
        api_key,
        state_dir: instance.state_dir.clone(),
    };
    ensure_local(app, inner)?;
    let (Some(local), Some(tunnels)) = (inner.local.as_ref(), inner.tunnels.as_ref()) else {
        return Err(AppError::new("sync.local_unavailable"));
    };
    let opts = crate::keys::ssh_opts(app, &server, crate::keys::Access::Background("文件同步"))?;
    // 用内置 SSH 的服务器：两条同步转发开在批量连接上（大量同步数据不拖慢终端）；系统 OpenSSH 照旧
    let link = match (
        app.try_state::<crate::builtin::BuiltinState>(),
        app.try_state::<crate::passwords::PasswordState>(),
    ) {
        (Some(builtin), Some(passwords)) => ServerLink::connect_with_connection(
            tunnels,
            target,
            opts,
            &info,
            local,
            &computer_name(),
            &builtin.manager,
            pierlink_core::builtin::ConnectionRequest {
                server: &server,
                class: pierlink_core::builtin::ConnectionClass::Bulk,
                vault: passwords.vault.clone(),
                callbacks: builtin.callbacks.clone(),
                interactive: false,
                cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        )?,
        _ => ServerLink::connect(tunnels, target, opts, &info, local, &computer_name())?,
    };
    // 上次预估中途退出可能在两端留下临时文件夹配置，连接建立后清掉（不涉及用户文件）
    for client in [link.client(), local.client()] {
        if let Err(e) = sync::cleanup_stale_estimates(client) {
            eprintln!("清理残留的预估临时文件夹失败: 服务器={server_id}, 错误={e:?}");
        }
    }
    inner.links.insert(server_id.to_owned(), link);
    Ok(())
}

/// 断开并丢弃到某台服务器的连接
fn drop_link(inner: &mut Inner, server_id: &str) {
    if let Some(link) = inner.links.remove(server_id) {
        match &inner.tunnels {
            Some(t) => {
                let _ = link.disconnect(t);
            }
            None => drop(link),
        }
    }
}

/// 没有任何任务再用到的服务器，断开连接（下次需要时重建）
fn drop_unused_links(app: &AppHandle, inner: &mut Inner) {
    let used: std::collections::HashSet<String> = app
        .state::<DbState>()
        .with(|db| {
            let mut used = std::collections::HashSet::new();
            for t in db.list_sync_tasks()? {
                if let Some((_, eps)) = db.get_sync_task(&t.id)? {
                    used.extend(eps.into_iter().map(|e| e.server_id));
                }
            }
            Ok(used)
        })
        .unwrap_or_default();
    let unused: Vec<String> = inner
        .links
        .keys()
        .filter(|k| !used.contains(*k))
        .cloned()
        .collect();
    for id in unused {
        drop_link(inner, &id);
    }
}

/// 在同步锁内、后台线程执行一个操作
async fn with_sync<T, F>(app: AppHandle, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&AppHandle, &mut Inner) -> Result<T, AppError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<SyncState>();
        let mut inner = state.lock();
        f(&app, &mut inner)
    })
    .await
    .map_err(|e| AppError::new("sync.task_failed").with("reason", e.to_string()))?
}

// ---------- 数据结构 ----------

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SyncEndpointDto {
    pub server_id: String,
    pub server_name: String,
    pub remote_path: String,
    /// sendReceive / receiveOnly
    pub mode: String,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SyncTaskDto {
    pub id: String,
    pub folder_id: String,
    pub name: String,
    pub local_path: String,
    pub endpoints: Vec<SyncEndpointDto>,
    pub paused: bool,
    pub collaboration: bool,
    pub watch_delay_s: u32,
    /// ask / newest
    pub conflict_mode: String,
    pub versioning: bool,
    pub ignore_rules: Vec<String>,
    /// 本机一侧：sendReceive / receiveOnly（只接收服务器的变化）
    pub local_mode: String,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatusDto {
    pub server_id: String,
    pub connected: bool,
    /// 没连上是因为缺密码（链接报需要密码，或选了密码登录、这次运行还没输、也没记住）：界面给出“输入密码”
    pub needs_password: bool,
    /// 没连上是因为这台服务器要验证码、后台登不上（内置 SSH）：界面给出“登录”，登录一次后同步用这次登录
    pub needs_login: bool,
    /// 0–100；未知为 null
    pub completion: Option<f64>,
    pub error: Option<AppError>,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatusDto {
    /// Syncthing 文件夹状态（idle / scanning / syncing / error 等）；本机同步服务未运行为 stopped；正忙为 busy
    pub state: String,
    pub files: f64,
    pub bytes: f64,
    pub need_bytes: f64,
    pub errors: Vec<SyncErrorDto>,
    pub servers: Vec<ServerStatusDto>,
    /// 因一次删除大量文件被自动暂停（需要用户确认后再继续）
    pub deletion_guard_paused: bool,
    /// 本机只接收时，本机改过、不会上传的项数
    pub local_changes: f64,
}

#[derive(Serialize, Type)]
pub struct SyncErrorDto {
    pub path: String,
    pub reason: String,
}

#[derive(Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PathIssueDto {
    Dangerous { side: String, path: String },
    Overlap { side: String, task: String },
    CloudFolder { provider: String },
    OtherSyncthing { side: String },
    IgnoreFile { side: String },
    GitRepository { side: String },
    Unreadable { side: String },
}

#[derive(Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SyncDraftInput {
    pub server_id: String,
    pub remote_path: String,
    pub local_path: String,
    /// 服务器一侧：sendReceive / receiveOnly（服务器只接收本机的变化）
    pub mode: String,
    /// 本机一侧：sendReceive / receiveOnly（本机只接收服务器的变化）
    pub local_mode: String,
    pub name: String,
    pub rules: Vec<String>,
    pub collaboration: bool,
    /// ask / newest
    pub conflict_mode: String,
    pub watch_delay_s: u32,
    pub versioning: bool,
    /// 加入已有同步且两边排除规则不一致时，用户选择以哪份为准：server / local；未选择为 null
    pub ignores_source: Option<String>,
}

fn to_dto(
    task: SyncTask,
    endpoints: Vec<SyncEndpoint>,
    names: &HashMap<String, String>,
) -> SyncTaskDto {
    SyncTaskDto {
        id: task.id,
        folder_id: task.folder_id,
        name: task.name,
        local_path: task.local_path,
        endpoints: endpoints
            .into_iter()
            .map(|e| SyncEndpointDto {
                server_name: names
                    .get(&e.server_id)
                    .cloned()
                    .unwrap_or_else(|| e.server_id.clone()),
                server_id: e.server_id,
                remote_path: e.remote_path,
                mode: match e.mode {
                    EndpointMode::SendReceive => "sendReceive",
                    EndpointMode::ReceiveOnly => "receiveOnly",
                }
                .into(),
            })
            .collect(),
        paused: task.paused,
        collaboration: task.collab_mode,
        watch_delay_s: u32::try_from(task.watch_delay_s).unwrap_or(10),
        conflict_mode: match task.conflict_policy {
            ConflictPolicy::Ask => "ask",
            ConflictPolicy::Newest => "newest",
        }
        .into(),
        versioning: task.versioning_enabled,
        ignore_rules: task
            .ignore_rules
            .lines()
            .map(str::to_owned)
            .filter(|l| !l.is_empty())
            .collect(),
        local_mode: mode_name(task.local_mode).into(),
    }
}

fn mode_name(m: EndpointMode) -> &'static str {
    match m {
        EndpointMode::SendReceive => "sendReceive",
        EndpointMode::ReceiveOnly => "receiveOnly",
    }
}

fn folder_kind(name: &str) -> FolderKind {
    if name == "receiveOnly" {
        FolderKind::ReceiveOnly
    } else {
        FolderKind::SendReceive
    }
}

fn endpoint_mode(k: FolderKind) -> EndpointMode {
    match k {
        FolderKind::ReceiveOnly => EndpointMode::ReceiveOnly,
        FolderKind::SendReceive => EndpointMode::SendReceive,
    }
}

fn load_task(app: &AppHandle, task_id: &str) -> Result<(SyncTask, Vec<SyncEndpoint>), AppError> {
    app.state::<DbState>()
        .with(|db| db.get_sync_task(task_id))?
        .ok_or_else(|| AppError::new("sync.task_not_found"))
}

/// 确保本机实例和任务涉及的所有服务器连接都已就绪
fn ensure_all(
    app: &AppHandle,
    inner: &mut Inner,
    endpoints: &[SyncEndpoint],
) -> Result<(), AppError> {
    for e in endpoints {
        ensure_link(app, inner, &e.server_id)?;
    }
    ensure_local(app, inner)?;
    Ok(())
}

/// 取出本机实例和任务各端的连接（先调用 ensure_all）
fn parts<'a>(
    inner: &'a Inner,
    endpoints: &[SyncEndpoint],
) -> Result<(&'a LocalSync, Vec<&'a ServerLink>), AppError> {
    let local = inner
        .local
        .as_ref()
        .ok_or_else(|| AppError::new("sync.local_unavailable"))?;
    let links = endpoints
        .iter()
        .filter_map(|e| inner.links.get(&e.server_id))
        .collect();
    Ok((local, links))
}

// ---------- 命令 ----------

/// 已部署同步组件的服务器 ID
#[tauri::command]
#[specta::specta]
pub async fn sync_deployed_servers(app: AppHandle) -> Result<Vec<String>, AppError> {
    app.state::<DbState>().with(|db| {
        let mut out = Vec::new();
        for s in db.list_servers()? {
            if db.get_instance(&s.id)?.is_some() {
                out.push(s.id);
            }
        }
        Ok(out)
    })
}

/// 全部同步任务（只读数据库，不连接服务器）
#[tauri::command]
#[specta::specta]
pub async fn sync_tasks(app: AppHandle) -> Result<Vec<SyncTaskDto>, AppError> {
    app.state::<DbState>().with(|db| {
        let names: HashMap<String, String> = db
            .list_servers()?
            .into_iter()
            .map(|s| (s.id, s.name))
            .collect();
        let mut out = Vec::new();
        for t in db.list_sync_tasks()? {
            if let Some((task, eps)) = db.get_sync_task(&t.id)? {
                out.push(to_dto(task, eps, &names));
            }
        }
        Ok(out)
    })
}

/// 等同步服务的锁：正忙（创建任务、部署……）时每 100 毫秒再试，最多等 timeout
fn lock_within(
    state: &SyncState,
    timeout: std::time::Duration,
) -> Result<MutexGuard<'_, Inner>, AppError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match state.inner.try_lock() {
            Ok(g) => return Ok(g),
            Err(TryLockError::Poisoned(p)) => return Ok(p.into_inner()),
            Err(TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(AppError::new("sync.busy")
                    .with_reason("sync.service_busy_check_later", "同步服务正忙，请稍后再查"));
            }
        }
    }
}

/// AI 接入用：同步任务的状态。
///
/// :param app: 应用句柄
/// :param task: 任务 ID 或名称；None 表示全部
/// :return: 每个任务的状态；本机同步服务没在运行时状态为 stopped
pub(crate) fn mcp_task_states(
    app: &AppHandle,
    task: Option<&str>,
) -> Result<Vec<pierlink_core::mcp::SyncTaskState>, AppError> {
    use pierlink_core::mcp::{SyncServerState, SyncTaskState};
    let (tasks, names, devices) = app.state::<DbState>().with(|db| {
        let names: HashMap<String, String> = db
            .list_servers()?
            .into_iter()
            .map(|s| (s.id, s.name))
            .collect();
        let mut tasks = Vec::new();
        let mut devices = HashMap::new();
        for t in db.list_sync_tasks()? {
            if task.is_some_and(|q| q != t.id && q != t.name) {
                continue;
            }
            if let Some((t, eps)) = db.get_sync_task(&t.id)? {
                for e in &eps {
                    if let Some(instance) = db.get_instance(&e.server_id)? {
                        devices.insert(e.server_id.clone(), instance.device_id);
                    }
                }
                tasks.push((t, eps));
            }
        }
        Ok((tasks, names, devices))
    })?;
    let state = app.state::<SyncState>();
    let inner = lock_within(&state, std::time::Duration::from_secs(5))?;
    let mut out = Vec::new();
    for (t, eps) in tasks {
        let servers: Vec<(String, String)> = eps
            .iter()
            .filter_map(|e| {
                devices
                    .get(&e.server_id)
                    .map(|d| (e.server_id.clone(), d.clone()))
            })
            .collect();
        // 每台服务器上的路径（告诉 agent 服务器路径和 Windows 路径怎么对应）
        let remote_paths: HashMap<String, String> = eps
            .iter()
            .map(|e| (e.server_id.clone(), e.remote_path.clone()))
            .collect();
        let (state, need_bytes, server_states, errors) = match inner.local.as_ref() {
            Some(local) => {
                let s = sync::task_status(local, &t.folder_id, &servers)?;
                // 同步出错的文件：让 AI 的 sync_wait 立即知道，不用等到超时
                let errors = s
                    .errors
                    .iter()
                    .map(|e| format!("{}: {}", e.path, e.reason))
                    .collect();
                (s.state, s.need_bytes, s.servers, errors)
            }
            None => ("stopped".to_owned(), 0, Vec::new(), Vec::new()),
        };
        out.push(SyncTaskState {
            servers: server_states
                .into_iter()
                .map(|s| SyncServerState {
                    name: names.get(&s.server_id).cloned().unwrap_or_default(),
                    remote_path: remote_paths.get(&s.server_id).cloned().unwrap_or_default(),
                    server_id: s.server_id,
                    connected: s.connected,
                    completion: s.completion,
                    need_items: s.need_items,
                    need_deletes: s.need_deletes,
                })
                .collect(),
            id: t.id,
            name: t.name,
            folder_id: t.folder_id,
            local_path: t.local_path,
            state,
            errors,
            paused: t.paused,
            need_bytes,
        });
    }
    Ok(out)
}

/// AI 接入用：让本机同步服务立即扫描这个文件夹，扫完返回
///
/// :param app: 应用句柄
/// :param folder_id: 本机文件夹 ID
/// :param timeout: 最多等多久（含等同步服务空出来的时间）
pub(crate) fn mcp_rescan(
    app: &AppHandle,
    folder_id: &str,
    timeout: std::time::Duration,
) -> Result<(), AppError> {
    let started = std::time::Instant::now();
    // 只在锁里取出管理接口的句柄；扫描大目录可能要几分钟，不能一直占着同步的锁
    let client = {
        let state = app.state::<SyncState>();
        let inner = lock_within(&state, timeout.min(std::time::Duration::from_secs(10)))?;
        inner
            .local
            .as_ref()
            .ok_or_else(|| AppError::new("sync.local_unavailable"))?
            .client()
            .clone()
    };
    client
        .scan_with_timeout(folder_id, timeout.saturating_sub(started.elapsed()))
        .map(|_| ())
}

/// 任务的实时状态。同步服务正忙（例如正在创建任务）时返回 state=busy，界面稍后再查。
#[tauri::command]
#[specta::specta]
pub async fn sync_task_status(app: AppHandle, task_id: String) -> Result<SyncStatusDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (task, eps) = load_task(&app, &task_id)?;
        let state = app.state::<SyncState>();
        let inner = match state.inner.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return Ok(empty_status("busy")),
        };
        let Some(local) = inner.local.as_ref() else {
            return Ok(empty_status("stopped"));
        };
        let servers: Vec<(String, String)> = eps
            .iter()
            .filter_map(|e| {
                app.state::<DbState>()
                    .with(|db| db.get_instance(&e.server_id))
                    .ok()
                    .flatten()
                    .map(|i| (e.server_id.clone(), i.device_id))
            })
            .collect();
        let s = sync::task_status(local, &task.folder_id, &servers)?;
        let state = app.state::<SyncState>();
        state
            .totals
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(task.folder_id.clone(), s.global_files);
        let guard_paused = state
            .guard_paused
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&task.folder_id);
        Ok(SyncStatusDto {
            deletion_guard_paused: guard_paused,
            local_changes: s.local_changed_items as f64,
            state: s.state,
            files: s.global_files as f64,
            bytes: s.global_bytes as f64,
            need_bytes: s.need_bytes as f64,
            errors: s
                .errors
                .into_iter()
                .map(|e| SyncErrorDto {
                    path: e.path,
                    reason: e.reason,
                })
                .collect(),
            servers: s
                .servers
                .into_iter()
                .map(|mut x| {
                    if !x.connected && x.error.is_none() {
                        x.error = inner.link_errors.get(&x.server_id).cloned();
                    }
                    x
                })
                .map(|x| ServerStatusDto {
                    // 链接报“需要密码”（含 ssh-agent 方式退回要密码、保存的密码已失效），或选了密码登录又没有密码
                    needs_password: !x.connected
                        && (x
                            .error
                            .as_ref()
                            .is_some_and(|e| e.code == "ssh.password_needed")
                            || crate::passwords::needs_password(&app, &x.server_id)),
                    needs_login: !x.connected
                        && x.error
                            .as_ref()
                            .is_some_and(|e| e.code == "ssh.two_factor_required")
                        && builtin_sync(&app, &x.server_id),
                    server_id: x.server_id,
                    connected: x.connected,
                    completion: x.completion,
                    error: x.error,
                })
                .collect(),
        })
    })
    .await
    .map_err(|e| AppError::new("sync.task_failed").with("reason", e.to_string()))?
}

/// 退出确认框里的一行：某个任务还有多少没同步完
#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExitTaskDto {
    pub name: String,
    /// 本机待接收与各服务器待接收的条目数之和
    pub need_files: f64,
    pub need_bytes: f64,
}

/// 等同步锁的最长时间（退出确认框不能一直转圈）
const EXIT_SUMMARY_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// 托盘菜单顶部的同步摘要
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TraySummary {
    pub tasks: usize,
    pub paused: usize,
    pub syncing: usize,
    /// 出错、服务器离线、被删除保护暂停的任务
    pub problems: usize,
    /// 有没暂停的任务，但本机同步服务没有运行
    pub stopped: bool,
}

/// 托盘用的同步摘要。同步服务正忙（创建任务等操作持锁）时返回 None，下次再查。
pub fn tray_summary(app: &AppHandle) -> Option<TraySummary> {
    let tasks: Vec<(SyncTask, Vec<SyncEndpoint>)> = app
        .try_state::<DbState>()?
        .with(|db| {
            let mut out = Vec::new();
            for t in db.list_sync_tasks()? {
                if let Some(full) = db.get_sync_task(&t.id)? {
                    out.push(full);
                }
            }
            Ok(out)
        })
        .ok()?;
    let mut sum = TraySummary {
        tasks: tasks.len(),
        paused: tasks.iter().filter(|(t, _)| t.paused).count(),
        ..Default::default()
    };
    if sum.paused == sum.tasks {
        return Some(sum);
    }
    let state = app.state::<SyncState>();
    let inner = match state.inner.try_lock() {
        Ok(g) => g,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        Err(TryLockError::WouldBlock) => return None,
    };
    let Some(local) = inner.local.as_ref() else {
        sum.stopped = true;
        return Some(sum);
    };
    let guard_paused = lock_of(&state.guard_paused).clone();
    for (task, eps) in tasks.iter().filter(|(t, _)| !t.paused) {
        let servers: Vec<(String, String)> = eps
            .iter()
            .filter_map(|e| {
                app.state::<DbState>()
                    .with(|db| db.get_instance(&e.server_id))
                    .ok()
                    .flatten()
                    .map(|i| (e.server_id.clone(), i.device_id))
            })
            .collect();
        match sync::task_status(local, &task.folder_id, &servers) {
            Ok(s) => {
                if guard_paused.contains(&task.folder_id)
                    || s.state == "error"
                    || !s.errors.is_empty()
                    || s.servers.iter().any(|x| !x.connected)
                {
                    sum.problems += 1;
                } else if s.need_bytes > 0 || s.state.starts_with("sync") {
                    sum.syncing += 1;
                }
            }
            Err(_) => sum.problems += 1,
        }
    }
    Some(sum)
}

/// 暂停或继续全部同步任务（托盘菜单）。逐个执行，某个失败不影响其他，返回失败的个数。
pub async fn set_all_paused(app: AppHandle, paused: bool) -> usize {
    let ids: Vec<String> = match app.state::<DbState>().with(|db| db.list_sync_tasks()) {
        Ok(list) => list
            .into_iter()
            .filter(|t| t.paused != paused)
            .map(|t| t.id)
            .collect(),
        Err(e) => {
            eprintln!("读取同步任务失败: 错误={e:?}");
            return 1;
        }
    };
    let mut failed = 0;
    for id in ids {
        if let Err(e) = sync_set_paused(app.clone(), id.clone(), paused).await {
            eprintln!("托盘：设置同步暂停失败: 任务={id}, 错误={e:?}");
            failed += 1;
        }
    }
    failed
}

/// 退出前汇总各任务未同步的文件（L3、L11）。
///
/// 只统计正在运行的任务：本机同步服务没启动或任务已暂停时，退出不会改变它们的状态。
/// 未同步 = 本机还没收到的条目 + 每台服务器还没收到的条目（服务器离线时按最后已知的情况计算）。
///
/// :param app: 应用句柄
/// :return: (有未同步内容的任务, 是否有任务状态没查到)
pub fn exit_summary(app: &AppHandle) -> (Vec<ExitTaskDto>, bool) {
    let tasks: Vec<(SyncTask, Vec<SyncEndpoint>)> = match app.state::<DbState>().with(|db| {
        let mut out = Vec::new();
        for t in db.list_sync_tasks()? {
            if let Some(full) = db.get_sync_task(&t.id)? {
                out.push(full);
            }
        }
        Ok(out)
    }) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("读取同步任务失败: 错误={e:?}");
            return (Vec::new(), true);
        }
    };
    if tasks.iter().all(|(t, _)| t.paused) {
        return (Vec::new(), false);
    }
    let state = app.state::<SyncState>();
    let deadline = std::time::Instant::now() + EXIT_SUMMARY_LOCK_WAIT;
    let inner = loop {
        match state.inner.try_lock() {
            Ok(g) => break g,
            Err(TryLockError::Poisoned(p)) => break p.into_inner(),
            Err(TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(TryLockError::WouldBlock) => {
                eprintln!("汇总未同步文件: 同步服务正忙，跳过");
                return (Vec::new(), true);
            }
        }
    };
    // 只在取客户端时持锁：之后的查询可能很慢（同步服务无响应时每次最长 30 秒），
    // 持锁会让用户此时确认退出时，退出清理卡在等锁上
    let Some(client) = inner.local.as_ref().map(|l| l.client().clone()) else {
        return (Vec::new(), false);
    };
    drop(inner);
    let stopping = || {
        state
            .shutting_down
            .load(std::sync::atomic::Ordering::SeqCst)
    };
    let mut out = Vec::new();
    let mut unknown = false;
    for (task, eps) in tasks.iter().filter(|(t, _)| !t.paused) {
        if stopping() {
            return (out, true);
        }
        let mut files = 0u64;
        let mut bytes = 0u64;
        match client.db_status(&task.folder_id) {
            Ok(v) => {
                files += v["needTotalItems"].as_u64().unwrap_or(0);
                bytes += v["needBytes"].as_u64().unwrap_or(0);
            }
            Err(e) => {
                eprintln!("查询任务状态失败: 任务={}, 错误={e:?}", task.id);
                unknown = true;
                continue;
            }
        }
        for ep in eps {
            if stopping() {
                return (out, true);
            }
            let device = app
                .state::<DbState>()
                .with(|db| db.get_instance(&ep.server_id))
                .ok()
                .flatten()
                .map(|i| i.device_id);
            let Some(device) = device else { continue };
            match client.db_completion(&task.folder_id, &device) {
                Ok(v) => {
                    files += v["needItems"].as_u64().unwrap_or(0);
                    bytes += v["needBytes"].as_u64().unwrap_or(0);
                }
                Err(e) => {
                    eprintln!(
                        "查询服务器完成度失败: 任务={}, 服务器={}, 错误={e:?}",
                        task.id, ep.server_id
                    );
                    unknown = true;
                }
            }
        }
        if files > 0 || bytes > 0 {
            out.push(ExitTaskDto {
                name: task.name.clone(),
                need_files: files as f64,
                need_bytes: bytes as f64,
            });
        }
    }
    (out, unknown)
}

fn empty_status(state: &str) -> SyncStatusDto {
    SyncStatusDto {
        state: state.into(),
        files: 0.0,
        bytes: 0.0,
        need_bytes: 0.0,
        errors: vec![],
        servers: vec![],
        deletion_guard_paused: false,
        local_changes: 0.0,
    }
}

/// 本机环境（用于本机目录检查）
fn local_env(app: &AppHandle) -> LocalEnv {
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let mut env = LocalEnv {
        home: var("USERPROFILE")
            .or_else(|| var("HOME"))
            .unwrap_or_default(),
        local_app_data: data_dir(app)
            .ok()
            .and_then(|d| d.parent().map(PathBuf::from))
            .unwrap_or_default(),
        windows_dir: var("SystemRoot").unwrap_or_else(|| PathBuf::from(r"C:\Windows")),
        ..Default::default()
    };
    for k in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
        if let Some(p) = var(k) {
            env.onedrive.push(p);
        }
    }
    if let Some(home) = var("USERPROFILE") {
        env.dropbox.push(home.join("Dropbox"));
    }
    env
}

fn issue_dto(i: PathIssue, task_names: &HashMap<String, String>) -> PathIssueDto {
    match i {
        PathIssue::Dangerous { side, path } => PathIssueDto::Dangerous { side, path },
        PathIssue::Overlap { side, task_id } => PathIssueDto::Overlap {
            side,
            task: task_names.get(&task_id).cloned().unwrap_or(task_id),
        },
        PathIssue::CloudFolder { provider } => PathIssueDto::CloudFolder { provider },
        PathIssue::OtherSyncthing { side } => PathIssueDto::OtherSyncthing { side },
        // 核心报告的“可加入”来自本电脑自己已有的同路径任务：再建一个会冲突，按重叠阻止
        PathIssue::JoinExisting { folder_id } => PathIssueDto::Overlap {
            side: "remote".into(),
            task: task_names.get(&folder_id).cloned().unwrap_or(folder_id),
        },
        PathIssue::IgnoreFile { side } => PathIssueDto::IgnoreFile { side },
        PathIssue::GitRepository { side } => PathIssueDto::GitRepository { side },
        PathIssue::Unreadable { side } => PathIssueDto::Unreadable { side },
    }
}

/// 检查要同步的两个目录（危险目录、重叠、网盘、已被别的 Syncthing 管理、可加入已有同步等）
#[tauri::command]
#[specta::specta]
pub async fn sync_check_paths(
    app: AppHandle,
    input: SyncDraftInput,
) -> Result<Vec<PathIssueDto>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (tasks, names, endpoints) = app.state::<DbState>().with(|db| {
            let tasks = db.list_sync_tasks()?;
            // 任务 ID 和文件夹 ID 都能查到任务名（重叠提示里显示任务名）
            let names: HashMap<String, String> = tasks
                .iter()
                .flat_map(|t| {
                    [
                        (t.id.clone(), t.name.clone()),
                        (t.folder_id.clone(), t.name.clone()),
                    ]
                })
                .collect();
            let mut eps = Vec::new();
            for t in &tasks {
                if let Some((task, list)) = db.get_sync_task(&t.id)? {
                    for e in list.into_iter().filter(|e| e.server_id == input.server_id) {
                        eps.push(ExistingEndpoint {
                            task_id: task.id.clone(),
                            folder_id: task.folder_id.clone(),
                            remote_path: e.remote_path,
                        });
                    }
                }
            }
            Ok((tasks, names, eps))
        })?;
        let existing: Vec<ExistingTask> = tasks
            .iter()
            .map(|t| ExistingTask {
                task_id: t.id.clone(),
                local_path: PathBuf::from(&t.local_path),
            })
            .collect();
        let mut issues: Vec<PathIssueDto> = sync::check_local_path(
            &PathBuf::from(&input.local_path),
            &existing,
            &local_env(&app),
        )
        .into_iter()
        .map(|i| issue_dto(i, &names))
        .collect();
        let server = crate::servers::load(&app, &input.server_id)?;
        let access = crate::keys::Access::User("检查同步目录");
        let cmd = crate::builtin::background(&app, &server, access, || {
            build_args(
                &crate::servers::target(&server)?,
                &Purpose::Background,
                &crate::keys::ssh_opts(&app, &server, access)?,
            )
        })?;
        // 服务器实例里已有的文件夹路径（已连接时才能取到）：目录里有我们实例留下的标记时不误报“被别的 Syncthing 管理”
        // 同步服务正忙（创建、预估等）时最多等 10 秒；仍忙就让用户稍后重试，不能用空列表误判
        let managed = {
            let state = app.state::<SyncState>();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                match state.inner.try_lock() {
                    Ok(inner) => {
                        break inner
                            .links
                            .get(&input.server_id)
                            .and_then(|l| l.folder_paths().ok())
                            .unwrap_or_default();
                    }
                    Err(TryLockError::Poisoned(p)) => {
                        break p
                            .into_inner()
                            .links
                            .get(&input.server_id)
                            .and_then(|l| l.folder_paths().ok())
                            .unwrap_or_default();
                    }
                    Err(TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err(TryLockError::WouldBlock) => return Err(AppError::new("sync.busy")),
                }
            }
        };
        let (_, remote) = sync::check_remote_path(
            &SshProgram {
                path: find_ssh(&app)?,
            },
            &cmd,
            &input.remote_path,
            &endpoints,
            &managed,
        )?;
        issues.extend(remote.into_iter().map(|i| issue_dto(i, &names)));
        Ok(issues)
    })
    .await
    .map_err(|e| AppError::new("sync.task_failed").with("reason", e.to_string()))?
}

/// 创建同步任务（先暂停；start=true 时创建后立即开始）。服务器上已有同一目录的同步时，加入它而不是另建。
#[tauri::command]
#[specta::specta]
pub async fn sync_create_task(
    app: AppHandle,
    input: SyncDraftInput,
    start: bool,
) -> Result<String, AppError> {
    with_sync(app, move |app, inner| {
        let mode = if input.mode == "receiveOnly" {
            FolderKind::ReceiveOnly
        } else {
            FolderKind::SendReceive
        };
        ensure_link(app, inner, &input.server_id)?;
        ensure_local(app, inner)?;
        let link = inner
            .links
            .get(&input.server_id)
            .ok_or_else(|| AppError::new("sync.not_connected"))?;
        // 先解析服务器上的真实路径，再查是否已有同一目录的同步（加入已有同步，Y18）
        let server = crate::servers::load(app, &input.server_id)?;
        let access = crate::keys::Access::User("新建同步");
        let cmd = crate::builtin::background(app, &server, access, || {
            build_args(
                &crate::servers::target(&server)?,
                &Purpose::Background,
                &crate::keys::ssh_opts(app, &server, access)?,
            )
        })?;
        let (info, _) = sync::check_remote_path(
            &SshProgram {
                path: find_ssh(app)?,
            },
            &cmd,
            &input.remote_path,
            &[],
            &link.folder_paths()?,
        )?;
        let joinable = sync::find_joinable(link, &info.canonical_path)?;
        let ignores_source = match input.ignores_source.as_deref() {
            Some("server") => IgnoresSource::Server,
            Some("local") => IgnoresSource::Local,
            _ => IgnoresSource::Unconfirmed,
        };
        // 记进数据库的规则：加入已有同步时，除非用户选了“以本机为准”，否则以服务器的为准
        let rules = match &joinable {
            Some(j) if !matches!(ignores_source, IgnoresSource::Local) => j.ignores.clone(),
            _ => input.rules.clone(),
        };
        let spec = TaskSpec {
            data_dir: data_dir(app)?,
            folder_id: joinable.as_ref().map(|j| j.folder_id.clone()),
            name: input.name.trim().to_owned(),
            local_path: PathBuf::from(&input.local_path),
            endpoints: vec![EndpointSpec {
                server_id: input.server_id.clone(),
                remote_path: info.canonical_path.clone(),
                mode,
            }],
            ignore_rules: input.rules.clone(),
            ignores_source,
            watch_delay_s: if input.collaboration {
                1
            } else {
                input.watch_delay_s.clamp(1, 60)
            },
            collab: input.collaboration,
            versioning: input.versioning,
            versions_root: data_dir(app)?.join("versions"),
            local_mode: folder_kind(&input.local_mode),
        };
        let local = inner
            .local
            .as_ref()
            .ok_or_else(|| AppError::new("sync.local_unavailable"))?;
        let folder_id = sync::create_task(local, &[link], &spec)?;
        let now = now_ms();
        let task = SyncTask {
            id: uuid::Uuid::new_v4().to_string(),
            folder_id: folder_id.clone(),
            name: spec.name.clone(),
            local_path: input.local_path.clone(),
            conflict_policy: if input.conflict_mode == "newest" {
                ConflictPolicy::Newest
            } else {
                ConflictPolicy::Ask
            },
            local_mode: endpoint_mode(folder_kind(&input.local_mode)),
            collab_mode: input.collaboration,
            watch_delay_s: i64::from(spec.watch_delay_s),
            versioning_enabled: input.versioning,
            versioning_days: 30,
            ignore_rules: rules.join("\n"),
            editor_id: None,
            paused: true,
            created_at: now,
            updated_at: now,
        };
        let endpoint = SyncEndpoint {
            task_id: task.id.clone(),
            server_id: input.server_id.clone(),
            remote_path: info.canonical_path.clone(),
            mode: if matches!(mode, FolderKind::ReceiveOnly) {
                EndpointMode::ReceiveOnly
            } else {
                EndpointMode::SendReceive
            },
        };
        if let Err(e) = app
            .state::<DbState>()
            .with(|db| db.insert_sync_task(&task, std::slice::from_ref(&endpoint)))
        {
            // 数据库写失败：撤销刚建的同步配置（不删任何文件），避免出现应用里看不到的同步
            let _ = sync::remove_task(local, &[link], &folder_id);
            drop_unused_links(app, inner);
            return Err(e);
        }
        if start {
            // 任务已建好（暂停中）；开始失败时单独报告，界面关闭向导、刷新列表，由用户手动继续
            if let Err(e) = sync::set_paused(local, &[link], &folder_id, false) {
                return Err(AppError::new("sync.created_not_started")
                    .with("task_id", &task.id)
                    .with("cause", e.code));
            }
            app.state::<DbState>().with(|db| {
                let mut t = task.clone();
                t.paused = false;
                t.updated_at = now_ms();
                db.update_sync_task(&t)
            })?;
        }
        Ok(task.id)
    })
    .await
}

/// 暂停或继续同步
#[tauri::command]
#[specta::specta]
pub async fn sync_set_paused(
    app: AppHandle,
    task_id: String,
    paused: bool,
) -> Result<(), AppError> {
    with_sync(app, move |app, inner| {
        let (mut task, eps) = load_task(app, &task_id)?;
        ensure_all(app, inner, &eps)?;
        let (local, links) = parts(inner, &eps)?;
        sync::set_paused(local, &links, &task.folder_id, paused)?;
        if !paused {
            // 用户确认后继续：清除删除保护的暂停标记，并重新开始计数
            let state = app.state::<SyncState>();
            state
                .guard_paused
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&task.folder_id);
            if let Some(g) = state
                .guard
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_mut()
            {
                g.reset(&task.folder_id);
            }
        }
        task.paused = paused;
        task.updated_at = now_ms();
        app.state::<DbState>().with(|db| db.update_sync_task(&task))
    })
    .await
}

/// 重新扫描（所有端）
#[tauri::command]
#[specta::specta]
pub async fn sync_rescan(app: AppHandle, task_id: String) -> Result<(), AppError> {
    with_sync(app, move |app, inner| {
        let (task, eps) = load_task(app, &task_id)?;
        ensure_all(app, inner, &eps)?;
        let (local, links) = parts(inner, &eps)?;
        sync::rescan(local, &links, &task.folder_id)
    })
    .await
}

/// 编辑任务时某台服务器的方式
#[derive(Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct EndpointModeInput {
    pub server_id: String,
    /// sendReceive / receiveOnly
    pub mode: String,
}

/// 编辑任务：可修改的设置
#[derive(Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TaskSettingsInput {
    /// 本机一侧：sendReceive / receiveOnly
    pub local_mode: String,
    pub endpoints: Vec<EndpointModeInput>,
    pub collaboration: bool,
    pub watch_delay_s: u32,
    pub versioning: bool,
    /// ask / newest
    pub conflict_mode: String,
    pub rules: Vec<String>,
}

/// 修改任务设置：方向、协作模式、改动后多久同步、旧版本保留、冲突处理、排除规则。
/// 先改各端的 Syncthing 配置（任一端失败都恢复原样，例如服务器离线），成功后再写数据库。
#[tauri::command]
#[specta::specta]
pub async fn sync_update_settings(
    app: AppHandle,
    task_id: String,
    input: TaskSettingsInput,
) -> Result<(), AppError> {
    with_sync(app, move |app, inner| {
        let (mut task, eps) = load_task(app, &task_id)?;
        ensure_all(app, inner, &eps)?;
        let (local, links) = parts(inner, &eps)?;
        let mode_of = |server_id: &str, old: EndpointMode| {
            input
                .endpoints
                .iter()
                .find(|e| e.server_id == server_id)
                .map(|e| folder_kind(&e.mode))
                .unwrap_or(match old {
                    EndpointMode::ReceiveOnly => FolderKind::ReceiveOnly,
                    EndpointMode::SendReceive => FolderKind::SendReceive,
                })
        };
        let watch_delay_s = if input.collaboration {
            1
        } else {
            input.watch_delay_s.clamp(1, 60)
        };
        let settings = sync::TaskSettings {
            local_mode: folder_kind(&input.local_mode),
            endpoints: eps
                .iter()
                .map(|e| sync::EndpointSettings {
                    server_id: e.server_id.clone(),
                    mode: mode_of(&e.server_id, e.mode),
                })
                .collect(),
            watch_delay_s,
            collab: input.collaboration,
            versioning: input.versioning,
            versions_root: data_dir(app)?.join("versions"),
        };
        sync::update_task_settings(local, &links, &task.folder_id, &settings)?;
        task.local_mode = endpoint_mode(settings.local_mode);
        task.collab_mode = input.collaboration;
        task.watch_delay_s = i64::from(watch_delay_s);
        task.versioning_enabled = input.versioning;
        task.conflict_policy = if input.conflict_mode == "newest" {
            ConflictPolicy::Newest
        } else {
            ConflictPolicy::Ask
        };
        task.updated_at = now_ms();
        let new_eps: Vec<SyncEndpoint> = eps
            .iter()
            .map(|e| SyncEndpoint {
                mode: endpoint_mode(mode_of(&e.server_id, e.mode)),
                ..e.clone()
            })
            .collect();
        app.state::<DbState>()
            .with(|db| db.update_sync_task_with_endpoints(&task, &new_eps))?;
        // 排除规则变了才下发（规则单独一步：上面的设置已经生效，这一步失败时单独报告）
        let rules: Vec<String> = input
            .rules
            .iter()
            .map(|r| r.trim().to_owned())
            .filter(|r| !r.is_empty())
            .collect();
        let old: Vec<String> = task
            .ignore_rules
            .lines()
            .map(str::to_owned)
            .filter(|l| !l.is_empty())
            .collect();
        if rules != old {
            sync::update_ignores(local, &links, &task.folder_id, &rules)
                .map_err(|e| AppError::new("sync.rules_update_failed").with("cause", e.code))?;
            task.ignore_rules = rules.join("\n");
            task.updated_at = now_ms();
            app.state::<DbState>()
                .with(|db| db.update_sync_task(&task))?;
        }
        Ok(())
    })
    .await
}

/// 本机只接收的任务：把本机改过、没有上传的文件还原成服务器上的版本
#[tauri::command]
#[specta::specta]
pub async fn sync_revert_local(app: AppHandle, task_id: String) -> Result<(), AppError> {
    with_sync(app, move |app, inner| {
        let (task, eps) = load_task(app, &task_id)?;
        ensure_all(app, inner, &eps)?;
        let (local, _) = parts(inner, &eps)?;
        sync::revert_local_changes(local, &task.folder_id)
    })
    .await
}

/// 移除同步任务：各端取消这个同步，两端的文件都不删
#[tauri::command]
#[specta::specta]
pub async fn sync_remove_task(app: AppHandle, task_id: String) -> Result<(), AppError> {
    with_sync(app, move |app, inner| {
        let (task, eps) = load_task(app, &task_id)?;
        ensure_all(app, inner, &eps)?;
        let (local, links) = parts(inner, &eps)?;
        sync::remove_task(local, &links, &task.folder_id)?;
        app.state::<DbState>()
            .with(|db| db.delete_sync_task(&task.id))?;
        drop_unused_links(app, inner);
        Ok(())
    })
    .await
}

/// 应用启动时：有同步任务就在后台启动本机实例并连接相关服务器，恢复同步。失败只记日志，界面状态会显示原因。
pub fn resume_on_startup(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let tasks = match app.state::<DbState>().with(|db| {
            let mut all = Vec::new();
            for t in db.list_sync_tasks()? {
                if let Some(x) = db.get_sync_task(&t.id)? {
                    all.push(x);
                }
            }
            Ok(all)
        }) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("读取同步任务失败: 错误={e:?}");
                return;
            }
        };
        if tasks.is_empty() {
            return;
        }
        let state = app.state::<SyncState>();
        let quitting = || {
            state
                .shutting_down
                .load(std::sync::atomic::Ordering::SeqCst)
        };
        {
            let mut inner = state.lock();
            if quitting() {
                return;
            }
            if let Err(e) = ensure_local(&app, &mut inner) {
                eprintln!("启动本机同步服务失败: 错误={e:?}");
                return;
            }
        }
        let mut servers: Vec<String> = tasks
            .iter()
            .flat_map(|(_, eps)| eps.iter().map(|e| e.server_id.clone()))
            .collect();
        servers.sort();
        servers.dedup();
        // 每台服务器单独加锁：连不上的服务器最多占 30 秒，其间其他操作可以穿插；退出时立即停下
        for id in servers {
            let mut inner = state.lock();
            if quitting() {
                return;
            }
            if let Err(e) = ensure_link(&app, &mut inner, &id) {
                eprintln!("连接服务器同步失败: 服务器={id}, 错误={e:?}");
            }
        }
    });
}

/// 某台服务器换了连接方式：断开它的同步链接，再按新方式重连（没有同步任务时什么也不做）
pub fn reconnect_server(app: &AppHandle, server_id: &str) {
    {
        let state = app.state::<SyncState>();
        let mut inner = state.lock();
        drop_link(&mut inner, server_id);
        inner.link_errors.remove(server_id);
    }
    resume_server(app, server_id);
}

/// 某台服务器刚有了密码：它有同步任务、还没连上时在后台连一次（缺密码时连接会失败，不会自己重试）
pub fn resume_server(app: &AppHandle, server_id: &str) {
    let app = app.clone();
    let id = server_id.to_owned();
    std::thread::spawn(move || {
        let used = app
            .state::<DbState>()
            .with(|db| {
                for t in db.list_sync_tasks()? {
                    if let Some((_, eps)) = db.get_sync_task(&t.id)?
                        && eps.iter().any(|e| e.server_id == id)
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
            .unwrap_or(false);
        if !used {
            return;
        }
        let state = app.state::<SyncState>();
        if state
            .shutting_down
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let mut inner = state.lock();
        if inner.links.contains_key(&id) {
            return;
        }
        if let Err(e) = ensure_local(&app, &mut inner) {
            eprintln!("启动本机同步服务失败: 错误={e:?}");
            return;
        }
        match ensure_link(&app, &mut inner, &id) {
            Ok(()) => eprintln!("输入密码后连上服务器同步: 服务器={id}"),
            Err(e) => eprintln!("输入密码后连接服务器同步失败: 服务器={id}, 错误={}", e.code),
        }
    });
}

// ---------- 事件、删除保护、冲突、预估、旧版本 ----------

fn lock_of<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// 冲突存档根目录（%LOCALAPPDATA%\Pierlink\conflicts）
fn archive_root(app: &AppHandle) -> Result<PathBuf, AppError> {
    Ok(data_dir(app)?.join("conflicts"))
}

/// 本机同步服务启动后在后台持续读取事件：记录最近事件、做删除保护、自动模式下处理冲突
fn start_event_thread(app: &AppHandle, mut pump: sync::EventPump) {
    let state = app.state::<SyncState>();
    if state
        .pump_running
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    *lock_of(&state.guard) = Some(sync::DeletionGuard::new(
        100,
        0.2,
        std::time::Duration::from_secs(60),
    ));
    let app = app.clone();
    std::thread::spawn(move || {
        let state = app.state::<SyncState>();
        loop {
            // 本机同步服务已停止（退出应用）就结束线程
            if lock_of(&state.inner).local.is_none() {
                break;
            }
            let events = match pump.poll() {
                Ok(e) => e,
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    continue;
                }
            };
            if events.is_empty() {
                continue;
            }
            store_events(&app, &events);
            // 删除保护：一次删除大量文件时暂停任务，等用户确认
            let totals = lock_of(&state.totals).clone();
            let tripped = lock_of(&state.guard)
                .as_mut()
                .map(|g| g.observe(&events, &totals))
                .unwrap_or_default();
            for folder_id in tripped {
                guard_pause(&app, &folder_id);
            }
            // 冲突处理选了“自动保留最新修改的版本”的任务：自动处理，被替换的一方移入冲突存档
            let conflict_folders: std::collections::HashSet<&str> = events
                .iter()
                .filter(|e| e.kind == sync::SyncEventKind::Conflict)
                .map(|e| e.folder_id.as_str())
                .collect();
            for folder_id in conflict_folders {
                auto_resolve(&app, folder_id);
            }
        }
        state
            .pump_running
            .store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

/// 按文件夹 ID 找任务
fn task_by_folder(app: &AppHandle, folder_id: &str) -> Option<(SyncTask, Vec<SyncEndpoint>)> {
    app.state::<DbState>()
        .with(|db| {
            for t in db.list_sync_tasks()? {
                if t.folder_id == folder_id {
                    return db.get_sync_task(&t.id);
                }
            }
            Ok(None)
        })
        .ok()
        .flatten()
}

/// 删除保护触发：暂停任务（本机与服务器端），记下原因供界面提示
fn guard_pause(app: &AppHandle, folder_id: &str) {
    let Some((mut task, eps)) = task_by_folder(app, folder_id) else {
        return;
    };
    let state = app.state::<SyncState>();
    lock_of(&state.guard_paused).insert(folder_id.to_owned());
    let mut inner = state.lock();
    let result = (|| {
        ensure_all(app, &mut inner, &eps)?;
        let (local, links) = parts(&inner, &eps)?;
        sync::set_paused(local, &links, folder_id, true)?;
        task.paused = true;
        task.updated_at = now_ms();
        app.state::<DbState>().with(|db| db.update_sync_task(&task))
    })();
    if let Err(e) = result {
        eprintln!("删除保护暂停任务失败: 任务={}, 错误={e:?}", task.name);
    }
}

fn auto_resolve(app: &AppHandle, folder_id: &str) {
    let Some((task, _)) = task_by_folder(app, folder_id) else {
        return;
    };
    if task.conflict_policy != ConflictPolicy::Newest {
        return;
    }
    let Ok(root) = archive_root(app) else {
        return;
    };
    let rules: Vec<String> = task.ignore_rules.lines().map(str::to_owned).collect();
    if let Err(e) = sync::auto_resolve_newest_for_folder(
        &PathBuf::from(&task.local_path),
        &root,
        folder_id,
        &rules,
        200,
    ) {
        eprintln!("自动处理冲突失败: 任务={}, 错误={e:?}", task.name);
    }
}

/// 应用启动时清理超过 30 天的冲突存档
pub fn cleanup_conflict_archive(app: &AppHandle) {
    if let Ok(root) = archive_root(app)
        && root.is_dir()
    {
        match sync::cleanup_archive(&root, 30) {
            Ok(n) if n > 0 => eprintln!("清理冲突存档: 目录={}, 数量={n}", root.display()),
            Ok(_) => {}
            Err(e) => eprintln!("清理冲突存档失败: 错误={e:?}"),
        }
    }
}

/// 任务页“最近事件”最多显示多少条（数据库里每个任务最多存 200 条、30 天）
const EVENTS_SHOWN: usize = 200;

/// 把事件按任务存进数据库（重启后仍能看到）；不属于任何任务的事件丢弃
fn store_events(app: &AppHandle, events: &[sync::SyncEvent]) {
    let mut by_folder: HashMap<&str, Vec<sync::SyncEvent>> = HashMap::new();
    for e in events {
        by_folder
            .entry(e.folder_id.as_str())
            .or_default()
            .push(e.clone());
    }
    let result = app.state::<DbState>().with(|db| {
        let tasks = db.list_sync_tasks()?;
        for (folder, list) in by_folder {
            if let Some(t) = tasks.iter().find(|t| t.folder_id == folder) {
                db.insert_sync_events(&t.id, &list)?;
            }
        }
        Ok(())
    });
    if let Err(e) = result {
        eprintln!("保存同步事件失败: 错误={e:?}");
    }
}

/// 启动时清理超过 30 天的同步事件
pub fn prune_events(app: &AppHandle) {
    if let Err(e) = app
        .state::<DbState>()
        .with(|db| db.prune_sync_events(now_ms()))
    {
        eprintln!("清理同步事件失败: 错误={e:?}");
    }
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SyncEventDto {
    /// 数据库里的编号：前端用它去重，并和 time_ms 一起作为“加载更多”的位置
    pub id: f64,
    pub time_ms: f64,
    /// downloaded / uploaded / deleted / conflict / error / deviceConnected / deviceDisconnected / paused / stateChanged
    pub kind: String,
    /// 相关服务器的名称（多服务器任务里区分来源）
    pub server: Option<String>,
    pub path: Option<String>,
    pub detail: Option<String>,
    /// 改动是谁做的：`local` 表示这台电脑，否则是服务器名称；不知道时为空
    pub by: Option<String>,
}

/// “加载更多”的位置：上一页最后一条的时间和编号
#[derive(Deserialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SyncEventCursorDto {
    pub time_ms: f64,
    pub id: f64,
}

/// 任务的同步事件（新的在前），一次一页；before 为空时取最新的一页
#[tauri::command]
#[specta::specta]
pub async fn sync_events(
    app: AppHandle,
    task_id: String,
    before: Option<SyncEventCursorDto>,
) -> Result<Vec<SyncEventDto>, AppError> {
    let (task, eps) = load_task(&app, &task_id)?;
    // 设备 ID → 服务器名称
    let names: HashMap<String, String> = app.state::<DbState>().with(|db| {
        let mut m = HashMap::new();
        for e in &eps {
            if let (Some(i), Some(s)) =
                (db.get_instance(&e.server_id)?, db.get_server(&e.server_id)?)
            {
                m.insert(i.device_id, s.name);
            }
        }
        Ok(m)
    })?;
    let list = app.state::<DbState>().with(|db| {
        db.list_sync_events(
            &task.id,
            EVENTS_SHOWN,
            before.map(|c| (c.time_ms as i64, c.id as i64)),
        )
    })?;
    Ok(list
        .into_iter()
        .map(|e| SyncEventDto {
            id: e.id as f64,
            time_ms: e.time_ms as f64,
            kind: match e.kind {
                sync::SyncEventKind::Downloaded => "downloaded",
                sync::SyncEventKind::Uploaded => "uploaded",
                sync::SyncEventKind::Deleted => "deleted",
                sync::SyncEventKind::Conflict => "conflict",
                sync::SyncEventKind::Error => "error",
                sync::SyncEventKind::DeviceConnected => "deviceConnected",
                sync::SyncEventKind::DeviceDisconnected => "deviceDisconnected",
                sync::SyncEventKind::Paused => "paused",
                sync::SyncEventKind::StateChanged => "stateChanged",
            }
            .into(),
            server: e.device.as_ref().and_then(|d| names.get(d).cloned()),
            by: e.by.as_deref().and_then(|by| {
                if by == "local" {
                    Some("local".to_owned())
                } else {
                    names.get(by).cloned()
                }
            }),
            path: e.path,
            detail: e.detail,
        })
        .collect())
}

#[derive(Serialize, Deserialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ConflictDto {
    pub path: String,
    pub conflict_file: String,
    pub from_device: Option<String>,
    pub conflict_mtime_ms: f64,
    pub current_mtime_ms: f64,
}

/// 任务本机目录里的冲突文件（最多 200 个）
#[tauri::command]
#[specta::specta]
pub async fn sync_conflicts(app: AppHandle, task_id: String) -> Result<Vec<ConflictDto>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (task, _) = load_task(&app, &task_id)?;
        let rules: Vec<String> = task.ignore_rules.lines().map(str::to_owned).collect();
        Ok(
            sync::list_conflicts(&PathBuf::from(&task.local_path), &rules, 200)?
                .conflicts
                .into_iter()
                .map(|c| ConflictDto {
                    path: c.path,
                    conflict_file: c.conflict_file,
                    from_device: c.from_device,
                    conflict_mtime_ms: c.conflict_mtime_ms as f64,
                    current_mtime_ms: c.current_mtime_ms as f64,
                })
                .collect(),
        )
    })
    .await
    .map_err(|e| AppError::new("sync.task_failed").with("reason", e.to_string()))?
}

/// 处理一个冲突：keep=current 保留当前文件，keep=other 改用另一版本；被替换的一方移入冲突存档（30 天），不会直接删除
#[tauri::command]
#[specta::specta]
pub async fn sync_resolve_conflict(
    app: AppHandle,
    task_id: String,
    conflict: ConflictDto,
    keep: String,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (task, _) = load_task(&app, &task_id)?;
        let c = sync::Conflict {
            path: conflict.path,
            conflict_file: conflict.conflict_file,
            from_device: conflict.from_device,
            conflict_mtime_ms: conflict.conflict_mtime_ms as i64,
            current_mtime_ms: conflict.current_mtime_ms as i64,
        };
        let keep = if keep == "other" {
            sync::Keep::Other
        } else {
            sync::Keep::Current
        };
        sync::resolve_conflict_for_folder(
            &PathBuf::from(&task.local_path),
            &archive_root(&app)?,
            &task.folder_id,
            &c,
            keep,
        )
    })
    .await
    .map_err(|e| AppError::new("sync.task_failed").with("reason", e.to_string()))?
}

#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EstimateProgressDto {
    pub scanned_local: f64,
    pub scanned_remote: f64,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AmountDto {
    pub files: f64,
    pub bytes: f64,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct EstimateConflictDto {
    pub path: String,
    pub local_mtime_ms: f64,
    pub remote_mtime_ms: f64,
    /// local / remote
    pub newer: String,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DirSummaryDto {
    pub path: String,
    pub files: f64,
    pub bytes: f64,
    /// 排除原因：dependency / cache / sensitive / git / custom；将同步的为 null
    pub reason: Option<String>,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct EstimateDto {
    pub upload: AmountDto,
    pub download: AmountDto,
    pub same: AmountDto,
    pub conflict: AmountDto,
    pub local_total: AmountDto,
    pub remote_total: AmountDto,
    pub local_sync: AmountDto,
    pub remote_sync: AmountDto,
    pub conflicts: Vec<EstimateConflictDto>,
    pub syncing: Vec<DirSummaryDto>,
    pub excluded: Vec<DirSummaryDto>,
    pub scanned_local: f64,
    pub scanned_remote: f64,
}

fn amount(a: &sync::EstimateAmount) -> AmountDto {
    AmountDto {
        files: a.files as f64,
        bytes: a.bytes as f64,
    }
}

fn reason_key(r: &str) -> &'static str {
    match r {
        "依赖目录" => "dependency",
        "版本库" => "git",
        "敏感文件" => "sensitive",
        "缓存" => "cache",
        _ => "custom",
    }
}

/// 开始前预估：两端各建一个不共享的临时文件夹扫描后比较；结束后两端目录保持原样。进度经 on_progress 推送。
#[tauri::command]
#[specta::specta]
pub async fn sync_estimate(
    app: AppHandle,
    input: SyncDraftInput,
    on_progress: tauri::ipc::Channel<EstimateProgressDto>,
) -> Result<EstimateDto, AppError> {
    // 在排队等锁之前重置取消标志：等锁期间用户关闭向导，取消依然有效
    app.state::<SyncState>()
        .estimate_cancel
        .store(false, std::sync::atomic::Ordering::SeqCst);
    with_sync(app, move |app, inner| {
        let state = app.state::<SyncState>();
        ensure_link(app, inner, &input.server_id)?;
        ensure_local(app, inner)?;
        let local = inner
            .local
            .as_ref()
            .ok_or_else(|| AppError::new("sync.local_unavailable"))?;
        let link = inner
            .links
            .get(&input.server_id)
            .ok_or_else(|| AppError::new("sync.not_connected"))?;
        let spec = sync::EstimateSpec {
            data_dir: data_dir(app)?,
            local_path: PathBuf::from(&input.local_path),
            remote_path: input.remote_path.clone(),
            ignore_rules: input.rules.clone(),
        };
        let progress = |p: sync::EstimateProgress| {
            let _ = on_progress.send(EstimateProgressDto {
                scanned_local: p.scanned_local as f64,
                scanned_remote: p.scanned_remote as f64,
            });
        };
        let e = sync::estimate(local, link, &spec, &progress, &state.estimate_cancel)?;
        let dir = |d: &sync::DirectorySummary| DirSummaryDto {
            path: d.path.clone(),
            files: d.files as f64,
            bytes: d.bytes as f64,
            reason: d.reason.as_deref().map(|r| reason_key(r).to_owned()),
        };
        Ok(EstimateDto {
            upload: amount(&e.upload),
            download: amount(&e.download),
            same: amount(&e.same),
            conflict: amount(&e.conflict),
            local_total: amount(&e.local_total),
            remote_total: amount(&e.remote_total),
            local_sync: amount(&e.local_sync),
            remote_sync: amount(&e.remote_sync),
            conflicts: e
                .conflicts
                .iter()
                .map(|c| EstimateConflictDto {
                    path: c.path.clone(),
                    local_mtime_ms: c.local_mtime_ms as f64,
                    remote_mtime_ms: c.remote_mtime_ms as f64,
                    newer: c.newer.clone(),
                })
                .collect(),
            syncing: e.syncing.iter().map(dir).collect(),
            excluded: e.excluded.iter().map(dir).collect(),
            scanned_local: e.scanned.scanned_local as f64,
            scanned_remote: e.scanned.scanned_remote as f64,
        })
    })
    .await
}

/// 取消进行中的预估（1 秒内生效）
#[tauri::command]
#[specta::specta]
pub async fn sync_estimate_cancel(app: AppHandle) {
    app.state::<SyncState>().cancel_estimate();
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct VersionDto {
    pub path: String,
    pub version_time_ms: f64,
    pub modified_ms: f64,
    pub size: f64,
}

#[derive(Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct VersionPick {
    pub path: String,
    pub version_time_ms: f64,
}

/// 任务的旧版本（被覆盖或删除的文件；关闭旧版本保留时为空）
#[tauri::command]
#[specta::specta]
pub async fn sync_versions(
    app: AppHandle,
    task_id: String,
    prefix: Option<String>,
) -> Result<Vec<VersionDto>, AppError> {
    with_sync(app, move |app, inner| {
        let (task, _) = load_task(app, &task_id)?;
        ensure_local(app, inner)?;
        let local = inner
            .local
            .as_ref()
            .ok_or_else(|| AppError::new("sync.local_unavailable"))?;
        Ok(
            sync::list_versions(local, &task.folder_id, prefix.as_deref())?
                .into_iter()
                .map(|v| VersionDto {
                    path: v.path,
                    version_time_ms: v.version_time_ms as f64,
                    modified_ms: v.modified_ms as f64,
                    size: v.size as f64,
                })
                .collect(),
        )
    })
    .await
}

/// 恢复选中的旧版本；当前文件存在时会先被保存为新的旧版本。返回逐个文件的失败原因。
#[tauri::command]
#[specta::specta]
pub async fn sync_restore_versions(
    app: AppHandle,
    task_id: String,
    picks: Vec<VersionPick>,
) -> Result<Vec<SyncErrorDto>, AppError> {
    with_sync(app, move |app, inner| {
        let (task, _) = load_task(app, &task_id)?;
        ensure_local(app, inner)?;
        let local = inner
            .local
            .as_ref()
            .ok_or_else(|| AppError::new("sync.local_unavailable"))?;
        let picks: Vec<(String, i64)> = picks
            .into_iter()
            .map(|p| (p.path, p.version_time_ms as i64))
            .collect();
        Ok(sync::restore_versions(local, &task.folder_id, &picks)?
            .into_iter()
            .map(|e| SyncErrorDto {
                path: e.path,
                reason: e.reason,
            })
            .collect())
    })
    .await
}

// 非 Windows 只能做开发调试构建：内存凭据存储在进程退出后就丢失，不能用于正式版本
#[cfg(all(not(windows), not(debug_assertions)))]
compile_error!("Pierlink 正式构建只支持 Windows（非 Windows 平台没有持久的凭据存储）");

/// 这台服务器的同步是否走内置 SSH（要验证码时才能在界面上“登录”一次给同步用）
fn builtin_sync(app: &AppHandle, server_id: &str) -> bool {
    crate::servers::load(app, server_id).is_ok_and(|server| {
        pierlink_core::ssh::engine::engine_for_feature(
            &server,
            pierlink_core::ssh::engine::Feature::SyncTunnel,
        )
        .engine
            == pierlink_core::storage::Engine::Builtin
    })
}

/// 一端（本机或某台服务器）的回收站占用
#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TrashEndpointDto {
    /// None 表示本机
    pub server_id: Option<String>,
    /// 服务器名称；本机为空串（界面翻译）
    pub name: String,
    pub files: Option<f64>,
    pub bytes: Option<f64>,
    /// 这一端查不到的原因（离线等）；其他端照常显示
    pub error: Option<AppError>,
}

/// 清空后删掉的量
#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TrashClearedDto {
    pub files: f64,
    pub bytes: f64,
}

/// 查询任务在本机和各服务器上的回收站（旧版本目录）占用；某台服务器连不上不影响其他端
///
/// :param task_id: 任务 ID
/// :return: 每端的占用或错误，本机在最前
#[tauri::command]
#[specta::specta]
pub async fn sync_trash(
    app: AppHandle,
    task_id: String,
) -> Result<Vec<TrashEndpointDto>, AppError> {
    with_sync(app, move |app, inner| {
        let (task, eps) = load_task(app, &task_id)?;
        let mut failed: Vec<(String, AppError)> = Vec::new();
        for e in &eps {
            if let Err(err) = ensure_link(app, inner, &e.server_id) {
                failed.push((e.server_id.clone(), err));
            }
        }
        ensure_local(app, inner)?;
        let (local, links) = parts(inner, &eps)?;
        let versions_root = data_dir(app)?.join("versions");
        let usage = sync::query_trash(local, &versions_root, &links, &task.folder_id)?;
        let name_of = |id: &str| {
            crate::servers::load(app, id)
                .map(|s| s.name)
                .unwrap_or_else(|_| id.to_owned())
        };
        let mut out: Vec<TrashEndpointDto> = usage
            .into_iter()
            .map(|u| TrashEndpointDto {
                name: u.server_id.as_deref().map(name_of).unwrap_or_default(),
                server_id: u.server_id,
                files: u.usage.map(|x| x.files as f64),
                bytes: u.usage.map(|x| x.bytes as f64),
                error: u.error,
            })
            .collect();
        out.extend(failed.into_iter().map(|(id, error)| TrashEndpointDto {
            name: name_of(&id),
            server_id: Some(id),
            files: None,
            bytes: None,
            error: Some(error),
        }));
        Ok(out)
    })
    .await
}

/// 立即清空一端的回收站（被同步覆盖或删除的旧版本），不用暂停同步
///
/// :param task_id: 任务 ID
/// :param server_id: 服务器 ID；null 表示本机
/// :return: 删掉的文件数和字节数
#[tauri::command]
#[specta::specta]
pub async fn sync_clear_trash(
    app: AppHandle,
    task_id: String,
    server_id: Option<String>,
) -> Result<TrashClearedDto, AppError> {
    with_sync(app, move |app, inner| {
        let (task, eps) = load_task(app, &task_id)?;
        let usage = match &server_id {
            None => {
                let local = ensure_local(app, inner)?;
                sync::clear_local_trash(local, &data_dir(app)?.join("versions"), &task.folder_id)?
            }
            Some(id) => {
                if !eps.iter().any(|e| &e.server_id == id) {
                    return Err(AppError::new("sync.server_not_in_task"));
                }
                ensure_link(app, inner, id)?;
                let link = inner
                    .links
                    .get(id)
                    .ok_or_else(|| AppError::new("sync.not_connected"))?;
                sync::clear_remote_trash(link, &task.folder_id)?
            }
        };
        eprintln!(
            "清空同步回收站: 任务={task_id}, 端={}, 文件数={}, 字节={}",
            server_id.as_deref().unwrap_or("本机"),
            usage.files,
            usage.bytes
        );
        Ok(TrashClearedDto {
            files: usage.files as f64,
            bytes: usage.bytes as f64,
        })
    })
    .await
}
