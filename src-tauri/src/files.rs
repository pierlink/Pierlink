//! 文件面板的 Tauri 命令：列目录、查询服务器终端当前目录、传输。逻辑在 `pierlink_core::{sftp, fs_local, cwd}`。
//!
//! - 浏览用的 SFTP 会话按机器缓存复用，空闲 10 分钟后关闭；
//! - 每次传输单独开一个 SFTP 会话，传大文件时不影响浏览；
//! - 两台服务器之间传输经本机中转（先下载到数据目录下的临时目录，再上传，结束后删除）。

use crate::terminal::{find_ssh, ssh_config_dir};
use pierlink_core::cwd::{remote_cwd, remote_shell_status};
use pierlink_core::error::AppError;
use pierlink_core::fs_local;
use pierlink_core::process::SshProgram;
use pierlink_core::sftp::transfer::{self, Conflict, TransferProgress, TransferReport};
use pierlink_core::sftp::{EntryKind, SftpSession};
use pierlink_core::ssh::args::{CommonOpts, Purpose, SshCommand, build_args};
use pierlink_core::ssh::target::SshTarget;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};

/// 一次列目录最多返回的条目数（F4）
const LIST_LIMIT: usize = 5000;
/// 浏览用的 SFTP 会话空闲多久后关闭
const IDLE: Duration = Duration::from_secs(600);
/// 中转时本机磁盘至少要留出的空间（字节）
const RELAY_RESERVE: u64 = 1 << 30;
/// 建立 SFTP 会话的超时
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// 文件所在的机器
#[derive(Clone, Debug, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum LocDto {
    Local,
    #[serde(rename_all = "camelCase")]
    Server {
        server_id: String,
    },
    Quick {
        host: String,
        port: u16,
        user: Option<String>,
    },
}

impl LocDto {
    /// 缓存会话用的键
    fn key(&self) -> String {
        match self {
            LocDto::Local => "local".into(),
            LocDto::Server { server_id } => format!("server:{server_id}"),
            LocDto::Quick { host, port, user } => {
                format!("quick:{}@{host}:{port}", user.as_deref().unwrap_or(""))
            }
        }
    }
}

/// 服务器终端的目录标记：终端会话 ID → (所在机器, 标记)
struct Marker {
    loc: LocDto,
    marker: String,
}

struct Cached {
    session: Arc<Mutex<SftpSession>>,
    used: Instant,
}

/// 文件面板的应用状态
#[derive(Default)]
pub struct FilesState {
    sessions: Mutex<HashMap<String, Cached>>,
    transfers: Mutex<HashMap<String, Arc<AtomicBool>>>,
    markers: Mutex<HashMap<String, Marker>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl FilesState {
    /// 登记服务器终端的目录标记（打开远程终端成功后调用）。顺便清掉已经不存在的终端的标记。
    ///
    /// :param session_id: 终端会话 ID
    /// :param live: 判断某个终端会话是否还在
    pub fn remember_marker(
        &self,
        session_id: &str,
        loc: LocDto,
        marker: String,
        live: impl Fn(&str) -> bool,
    ) {
        let mut map = lock(&self.markers);
        map.retain(|id, _| live(id));
        map.insert(session_id.to_owned(), Marker { loc, marker });
    }
}

/// 生成终端目录标记：32 个十六进制字符
pub fn new_marker() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 远程机器 → ssh 目标和公共选项（已保存服务器用它的密钥、保存的密码）
fn remote(app: &AppHandle, loc: &LocDto) -> Result<(SshTarget, CommonOpts), AppError> {
    match loc {
        LocDto::Local => Err(AppError::new("files.not_remote")),
        LocDto::Server { server_id } => {
            let server = crate::servers::load(app, server_id)?;
            Ok((
                crate::servers::target(&server)?,
                crate::keys::ssh_opts(app, &server, crate::keys::Access::User("文件面板"))?,
            ))
        }
        LocDto::Quick { host, port, user } => Ok((
            SshTarget::Manual {
                host: host.clone(),
                port: *port,
                user: user.clone(),
                identity_file: None,
                jump: None,
            },
            CommonOpts::default(),
        )),
    }
}

/// 构造某种用途的 ssh 命令。需要临时配置文件的（手动跳板）暂不支持文件功能。
fn command(
    app: &AppHandle,
    loc: &LocDto,
    purpose: &Purpose,
) -> Result<(SshProgram, SshCommand), AppError> {
    let (target, opts) = remote(app, loc)?;
    let cmd = match (purpose, loc) {
        // 文件面板的后台脚本（统计目录、读写小文件等）按服务器的连接方式选内置 SSH 或系统 OpenSSH
        (Purpose::Background, LocDto::Server { server_id }) => {
            let server = crate::servers::load(app, server_id)?;
            crate::builtin::background(
                app,
                &server,
                crate::keys::Access::User("文件面板"),
                || build_args(&target, purpose, &opts),
            )?
        }
        // 文件面板本身（SFTP）：内置模式走批量连接，大文件传输不拖慢终端
        (Purpose::Sftp, LocDto::Server { server_id }) => {
            let server = crate::servers::load(app, server_id)?;
            crate::builtin::sftp(app, &server, || build_args(&target, purpose, &opts))?
        }
        _ => build_args(&target, purpose, &opts)?,
    };
    if cmd.config_text.is_some() {
        return Err(AppError::new("files.jump_unsupported"));
    }
    // 只是为了让后续可能用到的目录存在；失败不影响
    let _ = ssh_config_dir(app).map(std::fs::create_dir_all);
    Ok((
        SshProgram {
            path: find_ssh(app)?,
        },
        cmd,
    ))
}

/// 新开一个 SFTP 会话
fn open_session(app: &AppHandle, loc: &LocDto) -> Result<SftpSession, AppError> {
    let (program, cmd) = command(app, loc, &Purpose::Sftp)?;
    SftpSession::open(&program, &cmd, OPEN_TIMEOUT)
}

/// 用缓存的浏览会话执行操作；连接已断开时重新建立一次再试
fn with_session<T>(
    app: &AppHandle,
    loc: &LocDto,
    f: impl Fn(&mut SftpSession) -> Result<T, AppError>,
) -> Result<T, AppError> {
    let state = app.state::<FilesState>();
    let key = loc.key();
    for attempt in 0..2 {
        let session = {
            let mut map = lock(&state.sessions);
            // 关闭空闲太久的会话（放到锁外丢弃，关闭进程可能要等一会儿）
            let stale: Vec<String> = map
                .iter()
                .filter(|(k, c)| **k != key && c.used.elapsed() > IDLE)
                .map(|(k, _)| k.clone())
                .collect();
            let dropped: Vec<Cached> = stale.iter().filter_map(|k| map.remove(k)).collect();
            drop(dropped);
            map.get_mut(&key).map(|c| {
                c.used = Instant::now();
                c.session.clone()
            })
        };
        let session = match session {
            Some(s) => s,
            None => {
                let s = Arc::new(Mutex::new(open_session(app, loc)?));
                lock(&state.sessions).insert(
                    key.clone(),
                    Cached {
                        session: s.clone(),
                        used: Instant::now(),
                    },
                );
                s
            }
        };
        let result = f(&mut lock(&session));
        match result {
            Err(e) if e.code == "sftp.connection_lost" && attempt == 0 => {
                lock(&state.sessions).remove(&key);
            }
            other => return other,
        }
    }
    Err(AppError::new("sftp.connection_lost"))
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FileEntryDto {
    pub name: String,
    /// file / dir / symlink / other
    pub kind: String,
    pub size: f64,
    pub mtime: f64,
    pub hidden: bool,
    pub target_is_dir: Option<bool>,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ListingDto {
    pub path: String,
    pub entries: Vec<FileEntryDto>,
    pub truncated: bool,
}

fn kind_name(kind: EntryKind) -> String {
    match kind {
        EntryKind::File => "file",
        EntryKind::Dir => "dir",
        EntryKind::Symlink => "symlink",
        EntryKind::Other => "other",
    }
    .into()
}

async fn blocking<T, F>(app: AppHandle, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&AppHandle) -> Result<T, AppError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || f(&app))
        .await
        .map_err(|e| AppError::new("files.task_failed").with("reason", e.to_string()))?
}

/// 本机用户目录
fn local_home() -> Result<String, AppError> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(var).map_err(|_| AppError::new("files.home_unknown"))
}

/// 家目录：本机为用户目录，服务器为登录后的初始目录。
#[tauri::command]
#[specta::specta]
pub async fn files_home(app: AppHandle, loc: LocDto) -> Result<String, AppError> {
    blocking(app, move |app| match loc {
        LocDto::Local => local_home(),
        _ => with_session(app, &loc, |s| s.realpath(".")),
    })
    .await
}

/// 列目录（含隐藏文件），目录项过多时截断。
#[tauri::command]
#[specta::specta]
pub async fn files_list(app: AppHandle, loc: LocDto, path: String) -> Result<ListingDto, AppError> {
    blocking(app, move |app| match loc {
        LocDto::Local => {
            let listing = fs_local::list_dir(Path::new(&path), LIST_LIMIT)?;
            Ok(ListingDto {
                path,
                truncated: listing.truncated,
                entries: listing
                    .entries
                    .into_iter()
                    .map(|e| FileEntryDto {
                        name: e.name,
                        kind: kind_name(e.kind),
                        size: e.size as f64,
                        mtime: e.mtime as f64,
                        hidden: e.hidden,
                        target_is_dir: None,
                    })
                    .collect(),
            })
        }
        _ => with_session(app, &loc, |s| {
            let resolved = s.realpath(&path)?;
            let listing = s.list_dir(&resolved, LIST_LIMIT)?;
            Ok(ListingDto {
                truncated: listing.truncated,
                entries: listing
                    .entries
                    .into_iter()
                    .map(|e| FileEntryDto {
                        hidden: e.name.starts_with('.'),
                        name: e.name,
                        kind: kind_name(e.kind),
                        size: e.size as f64,
                        mtime: e.mtime as f64,
                        target_is_dir: e.target_is_dir,
                    })
                    .collect(),
                path: resolved,
            })
        }),
    })
    .await
}

/// 服务器终端当前所在目录；服务器不支持或找不到时返回 null。
#[tauri::command]
#[specta::specta]
pub async fn files_terminal_cwd(
    app: AppHandle,
    session_id: String,
) -> Result<Option<String>, AppError> {
    blocking(app, move |app| {
        let state = app.state::<FilesState>();
        let (loc, marker) = {
            let map = lock(&state.markers);
            match map.get(&session_id) {
                Some(m) => (m.loc.clone(), m.marker.clone()),
                None => return Ok(None),
            }
        };
        let (program, cmd) = command(app, &loc, &Purpose::Background)?;
        remote_cwd(&program, &cmd, &marker)
    })
    .await
}

#[derive(Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TransferRequestDto {
    pub from: LocDto,
    pub sources: Vec<String>,
    pub to: LocDto,
    pub dest_dir: String,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TransferPlanDto {
    pub files: f64,
    pub bytes: f64,
    pub conflicts: Vec<String>,
}

#[derive(Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgressDto {
    pub total_bytes: f64,
    pub done_bytes: f64,
    pub current: String,
    pub files_done: u32,
    pub files_total: u32,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RenamedDto {
    pub from: String,
    pub to: String,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FailedDto {
    pub path: String,
    pub reason: AppError,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TransferReportDto {
    pub done: u32,
    pub skipped: Vec<String>,
    pub renamed: Vec<RenamedDto>,
    pub failed: Vec<FailedDto>,
}

impl From<TransferReport> for TransferReportDto {
    fn from(r: TransferReport) -> Self {
        Self {
            done: r.succeeded,
            skipped: r.skipped.into_iter().map(|x| x.path).collect(),
            renamed: r
                .renamed
                .into_iter()
                .map(|x| RenamedDto {
                    from: x.from,
                    to: x.to,
                })
                .collect(),
            failed: r
                .failed
                .into_iter()
                .map(|x| FailedDto {
                    path: x.path,
                    reason: x.reason,
                })
                .collect(),
        }
    }
}

/// 传输方向
enum Direction {
    Upload,
    Download,
    /// 两台服务器之间，经本机中转
    Relay,
}

fn direction(req: &TransferRequestDto) -> Result<Direction, AppError> {
    match (&req.from, &req.to) {
        (LocDto::Local, LocDto::Local) => Err(AppError::new("files.same_machine")),
        (LocDto::Local, _) => Ok(Direction::Upload),
        (_, LocDto::Local) => Ok(Direction::Download),
        (a, b) if a.key() == b.key() => Err(AppError::new("files.same_machine")),
        _ => Ok(Direction::Relay),
    }
}

fn local_paths(sources: &[String]) -> Vec<PathBuf> {
    sources.iter().map(PathBuf::from).collect()
}

/// 传输前统计：文件数、总字节、目标里已存在的同名项。
#[tauri::command]
#[specta::specta]
pub async fn files_plan(
    app: AppHandle,
    req: TransferRequestDto,
) -> Result<TransferPlanDto, AppError> {
    blocking(app, move |app| {
        let plan = match direction(&req)? {
            Direction::Upload => with_session(app, &req.to, |s| {
                transfer::plan_upload(s, &local_paths(&req.sources), &req.dest_dir)
            })?,
            Direction::Download => with_session(app, &req.from, |s| {
                transfer::plan_download(s, &req.sources, Path::new(&req.dest_dir))
            })?,
            // 中转：只统计来源服务器上的文件，同名项在目标服务器上查（见下）
            Direction::Relay => {
                with_session(app, &req.from, |s| transfer::plan_remote(s, &req.sources))?
            }
        };
        let mut conflicts = plan.existing;
        if let Direction::Relay = direction(&req)? {
            // 中转时下载到空的临时目录，同名项要在目标服务器上查
            let names: Vec<String> = req
                .sources
                .iter()
                .map(|s| {
                    s.trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or(s)
                        .to_owned()
                })
                .collect();
            conflicts = with_session(app, &req.to, |s| {
                let mut found = Vec::new();
                for name in &names {
                    let path = format!("{}/{name}", req.dest_dir.trim_end_matches('/'));
                    if s.stat(&path)?.is_some() {
                        found.push(name.clone());
                    }
                }
                Ok(found)
            })?;
        }
        Ok(TransferPlanDto {
            files: plan.files_total as f64,
            bytes: plan.total_bytes as f64,
            conflicts,
        })
    })
    .await
}

fn conflict_of(name: &str) -> Result<Conflict, AppError> {
    match name {
        "error" => Ok(Conflict::Error),
        "overwrite" => Ok(Conflict::Overwrite),
        "skip" => Ok(Conflict::Skip),
        _ => Err(AppError::new("files.invalid_conflict").with("value", name)),
    }
}

/// 把核心的进度转成发给前端的进度（中转时两段各占一半）
fn progress_sender(
    channel: Channel<TransferProgressDto>,
    phase: Option<(u8, &'static str)>,
) -> impl FnMut(&TransferProgress) {
    let mut last = Instant::now() - Duration::from_secs(1);
    move |p: &TransferProgress| {
        // 最多每 100 毫秒发一次；最后一块总会发出
        if last.elapsed() < Duration::from_millis(100) && p.done_bytes < p.total_bytes {
            return;
        }
        last = Instant::now();
        let (done, total, current) = match phase {
            None => (p.done_bytes as f64, p.total_bytes as f64, p.current.clone()),
            Some((n, label)) => (
                p.done_bytes as f64 + if n == 2 { p.total_bytes as f64 } else { 0.0 },
                p.total_bytes as f64 * 2.0,
                format!("{label}{}", p.current),
            ),
        };
        let _ = channel.send(TransferProgressDto {
            total_bytes: total,
            done_bytes: done,
            current,
            files_done: p.files_done,
            files_total: p.files_total,
        });
    }
}

/// 中转用的临时目录：数据目录下 transfer-tmp/<传输 ID>，结束时删除（成功、失败、取消都删）
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 执行传输。transfer_id 由前端生成，用于取消；进度经 on_progress 推送。
#[tauri::command]
#[specta::specta]
pub async fn files_transfer(
    app: AppHandle,
    transfer_id: String,
    req: TransferRequestDto,
    conflict: String,
    on_progress: Channel<TransferProgressDto>,
) -> Result<TransferReportDto, AppError> {
    let conflict = conflict_of(&conflict)?;
    // 取消命令可能比本命令先到（两个异步命令的执行顺序不保证）：那时标记已经由取消命令建好并置位
    let cancel = lock(&app.state::<FilesState>().transfers)
        .entry(transfer_id.clone())
        .or_insert_with(|| Arc::new(AtomicBool::new(false)))
        .clone();
    let id = transfer_id.clone();
    let result = blocking(app.clone(), move |app| {
        let report = match direction(&req)? {
            Direction::Upload => {
                let mut s = open_session(app, &req.to)?;
                transfer::upload(
                    &mut s,
                    &local_paths(&req.sources),
                    &req.dest_dir,
                    conflict,
                    &mut progress_sender(on_progress, None),
                    &cancel,
                )?
            }
            Direction::Download => {
                let mut s = open_session(app, &req.from)?;
                transfer::download(
                    &mut s,
                    &req.sources,
                    Path::new(&req.dest_dir),
                    conflict,
                    &mut progress_sender(on_progress, None),
                    &cancel,
                )?
            }
            Direction::Relay => relay(app, &req, &id, conflict, on_progress, &cancel)?,
        };
        Ok(TransferReportDto::from(report))
    })
    .await;
    lock(&app.state::<FilesState>().transfers).remove(&transfer_id);
    result
}

/// 两台服务器之间：下载到本机临时目录，再上传到目标
fn relay(
    app: &AppHandle,
    req: &TransferRequestDto,
    id: &str,
    conflict: Conflict,
    on_progress: Channel<TransferProgressDto>,
    cancel: &AtomicBool,
) -> Result<TransferReport, AppError> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') || id.is_empty() || id.len() > 64
    {
        return Err(AppError::new("files.invalid_id"));
    }
    let dir = crate::data::data_dir(app)?.join("transfer-tmp").join(id);
    std::fs::create_dir_all(&dir).map_err(|e| {
        AppError::new("fs.failure")
            .with("operation", "create_dir")
            .with("reason", e.to_string())
    })?;
    let staging = Staging(dir);
    let mut src = open_session(app, &req.from)?;
    // 中转要先完整存到本机（应用数据目录所在的盘），空间不够就不开始，免得把系统盘写满
    let need = transfer::plan_remote(&mut src, &req.sources)?.total_bytes;
    if let Some(free) = fs_local::free_space(&staging.0)
        && free < need.saturating_add(RELAY_RESERVE)
    {
        return Err(AppError::new("files.relay_no_space")
            .with("need", format_bytes(need))
            .with("free", format_bytes(free)));
    }
    let down = transfer::download(
        &mut src,
        &req.sources,
        &staging.0,
        Conflict::Overwrite,
        &mut progress_sender(on_progress.clone(), Some((1, "↓ "))),
        cancel,
    )?;
    drop(src);
    // 上传下载下来的顶层项（名字可能因 Windows 规则被改过）
    let items: Vec<PathBuf> = std::fs::read_dir(&staging.0)
        .map_err(|e| {
            AppError::new("fs.failure")
                .with("operation", "read_dir")
                .with("reason", e.to_string())
        })?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    let mut dst = open_session(app, &req.to)?;
    let mut up = transfer::upload(
        &mut dst,
        &items,
        &req.dest_dir,
        conflict,
        &mut progress_sender(on_progress, Some((2, "↑ "))),
        cancel,
    )?;
    up.renamed.extend(down.renamed);
    up.failed.extend(down.failed);
    up.skipped.extend(down.skipped);
    Ok(up)
}

/// 字节数显示成 MB/GB（错误提示用）
fn format_bytes(n: u64) -> String {
    let gb = n as f64 / (1u64 << 30) as f64;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", n as f64 / (1u64 << 20) as f64)
    }
}

/// 取消传输（已经结束的忽略）。
#[tauri::command]
#[specta::specta]
pub async fn files_cancel(app: AppHandle, transfer_id: String) -> Result<(), AppError> {
    // 传输还没登记时先建好标记，传输命令开始时会用到它。传输刚结束时取消会留下一个标记，只占几十字节，不影响
    lock(&app.state::<FilesState>().transfers)
        .entry(transfer_id)
        .or_insert_with(|| Arc::new(AtomicBool::new(false)))
        .store(true, Ordering::Relaxed);
    Ok(())
}

/// 删除上次残留的中转临时目录（数据目录下 transfer-tmp）。启动时调用，失败只记日志。
pub fn cleanup_staging(app: &AppHandle) {
    let Ok(dir) = crate::data::data_dir(app).map(|d| d.join("transfer-tmp")) else {
        return;
    };
    if dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&dir)
    {
        eprintln!("清理中转临时目录失败: 目录={}, 错误={e}", dir.display());
    }
}

// ---------- 文件右键菜单：重命名、新建、删除、编辑文本、权限、打开 ----------

/// 软件内编辑的文本文件大小上限
const TEXT_LIMIT: u64 = 5 << 20;

/// 文件版本：大小、修改时间（Unix 秒）和内容哈希。保存前用它检查文件是否被别人改过
/// （同一秒内、大小不变的修改靠哈希发现）
#[derive(Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FileVersionDto {
    pub size: f64,
    pub mtime: f64,
    /// 内容的 SHA-256（十六进制）；不知道时为 null
    pub hash: Option<String>,
}

impl From<fs_local::FileVersion> for FileVersionDto {
    fn from(v: fs_local::FileVersion) -> Self {
        Self {
            size: v.size as f64,
            mtime: v.mtime as f64,
            hash: v.hash,
        }
    }
}

impl From<FileVersionDto> for fs_local::FileVersion {
    fn from(v: FileVersionDto) -> Self {
        Self {
            size: v.size as u64,
            mtime: v.mtime as i64,
            hash: v.hash,
        }
    }
}

/// 读出来的文本文件
#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TextFileDto {
    pub text: String,
    /// 原文件开头有 UTF-8 BOM（保存时照样写回）
    pub bom: bool,
    pub version: FileVersionDto,
}

/// 重命名（目标已存在时报错，不覆盖）
#[tauri::command]
#[specta::specta]
pub async fn files_rename(
    app: AppHandle,
    loc: LocDto,
    from: String,
    to: String,
) -> Result<(), AppError> {
    blocking(app, move |app| match loc {
        LocDto::Local => fs_local::rename(Path::new(&from), Path::new(&to)),
        _ => with_session(app, &loc, |s| s.rename(&from, &to)),
    })
    .await
}

/// 新建文件夹（已存在时报错）
#[tauri::command]
#[specta::specta]
pub async fn files_create_dir(app: AppHandle, loc: LocDto, path: String) -> Result<(), AppError> {
    blocking(app, move |app| match loc {
        LocDto::Local => fs_local::create_dir(Path::new(&path)),
        _ => with_session(app, &loc, |s| s.create_dir(&path)),
    })
    .await
}

/// 新建空文件（已存在时报错）
#[tauri::command]
#[specta::specta]
pub async fn files_create_file(app: AppHandle, loc: LocDto, path: String) -> Result<(), AppError> {
    blocking(app, move |app| match loc {
        LocDto::Local => fs_local::create_file(Path::new(&path)),
        _ => with_session(app, &loc, |s| s.create_file(&path)),
    })
    .await
}

/// 删除：本机放进回收站；服务器上直接删除（目录递归，不跟随符号链接）。返回删除的项数
#[tauri::command]
#[specta::specta]
pub async fn files_delete(
    app: AppHandle,
    loc: LocDto,
    paths: Vec<String>,
) -> Result<f64, AppError> {
    blocking(app, move |app| {
        let n = match loc {
            LocDto::Local => {
                fs_local::delete(&paths.iter().map(PathBuf::from).collect::<Vec<_>>())?
            }
            _ => with_session(app, &loc, |s| s.delete(&paths))?,
        };
        Ok(n as f64)
    })
    .await
}

/// 读取文本文件（软件内编辑）：超过 5 MiB、不是 UTF-8 文本时报错
#[tauri::command]
#[specta::specta]
pub async fn files_read_text(
    app: AppHandle,
    loc: LocDto,
    path: String,
) -> Result<TextFileDto, AppError> {
    blocking(app, move |app| {
        let f = match loc {
            LocDto::Local => fs_local::read_text(Path::new(&path), TEXT_LIMIT)?,
            _ => with_session(app, &loc, |s| s.read_text(&path, TEXT_LIMIT))?,
        };
        Ok(TextFileDto {
            text: f.text,
            bom: f.bom,
            version: FileVersionDto {
                size: f.size as f64,
                mtime: f.mtime as f64,
                hash: f.hash,
            },
        })
    })
    .await
}

/// 保存文本文件（原子替换）。expected 为打开时的版本：文件之后被改过时报 files.changed；传 null 表示直接覆盖
#[tauri::command]
#[specta::specta]
pub async fn files_write_text(
    app: AppHandle,
    loc: LocDto,
    path: String,
    text: String,
    bom: bool,
    expected: Option<FileVersionDto>,
) -> Result<FileVersionDto, AppError> {
    blocking(app, move |app| {
        let expected = expected.map(fs_local::FileVersion::from);
        let v = match loc {
            LocDto::Local => fs_local::write_text(Path::new(&path), &text, bom, expected)?,
            _ => with_session(app, &loc, |s| {
                s.write_text(&path, &text, bom, expected.clone())
            })?,
        };
        Ok(v.into())
    })
    .await
}

/// 服务器文件的权限位（如 0o644）；不知道时返回 null
#[tauri::command]
#[specta::specta]
pub async fn files_mode(
    app: AppHandle,
    loc: LocDto,
    path: String,
) -> Result<Option<u32>, AppError> {
    blocking(app, move |app| match loc {
        LocDto::Local => Ok(None),
        _ => with_session(app, &loc, |s| Ok(s.stat(&path)?.map(|e| e.mode & 0o7777))),
    })
    .await
}

/// 修改服务器文件的权限
#[tauri::command]
#[specta::specta]
pub async fn files_chmod(
    app: AppHandle,
    loc: LocDto,
    path: String,
    mode: u32,
) -> Result<(), AppError> {
    blocking(app, move |app| match loc {
        LocDto::Local => Err(AppError::new("files.not_remote")),
        _ => with_session(app, &loc, |s| s.chmod(&path, mode)),
    })
    .await
}

/// 用系统默认程序打开本机文件
#[tauri::command]
#[specta::specta]
pub async fn files_open_default(app: AppHandle, path: String) -> Result<(), AppError> {
    blocking(app, move |_| fs_local::open_with_default(Path::new(&path))).await
}

/// 在资源管理器中显示本机文件（选中它）
#[tauri::command]
#[specta::specta]
pub async fn files_reveal(app: AppHandle, path: String) -> Result<(), AppError> {
    blocking(app, move |_| fs_local::reveal_in_explorer(Path::new(&path))).await
}

/// 弹出系统的“选择文件夹”对话框
///
/// :return: 选中的文件夹绝对路径；用户取消时返回 null
#[tauri::command]
#[specta::specta]
pub async fn pick_folder(app: AppHandle) -> Result<Option<String>, AppError> {
    use tauri_plugin_dialog::DialogExt;
    tauri::async_runtime::spawn_blocking(move || {
        let picked = app.dialog().file().blocking_pick_folder();
        picked
            .map(|p| {
                p.into_path()
                    .map(|p| p.to_string_lossy().into_owned())
                    .map_err(|e| AppError::new("files.pick_failed").with("reason", e.to_string()))
            })
            .transpose()
    })
    .await
    .map_err(|e| AppError::new("files.pick_failed").with("reason", e.to_string()))?
}

/// 终端里是否有程序在运行（关闭前决定要不要确认）：
/// 服务器终端到服务器上查 shell 的前台进程组；本机终端查 shell 有没有子进程。查不到时返回 null
#[tauri::command]
#[specta::specta]
pub async fn terminal_busy(app: AppHandle, id: String) -> Result<Option<bool>, AppError> {
    blocking(app, move |app| {
        let remote = {
            let state = app.state::<FilesState>();
            let map = lock(&state.markers);
            map.get(&id).map(|m| (m.loc.clone(), m.marker.clone()))
        };
        match remote {
            Some((loc, marker)) => {
                let (program, cmd) = command(app, &loc, &Purpose::Background)?;
                Ok(remote_shell_status(&program, &cmd, &marker)?.map(|s| s.busy))
            }
            None => app
                .state::<crate::terminal::TerminalState>()
                .manager
                .busy(&id),
        }
    })
    .await
}
