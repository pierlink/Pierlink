//! 服务器端同步组件部署的 Tauri 命令：检查 → 用户授权 → 上传 → 生成配置 → 启动。
//! 逻辑在 `pierlink_core::deploy`；这里负责凭据生成与保存、随包程序定位、进度推送和数据库记录。
//! 隧道与配对两步在同步编排接入后补上，目前在进度里标为“待后续”。

use crate::data::DbState;
use crate::terminal::find_ssh;
use pierlink_core::deploy::{
    self, Arch, DeployPlan, InstanceSecrets, ServerReport, StartOutcome, SystemdState,
};
use pierlink_core::error::AppError;
use pierlink_core::process::SshProgram;
use pierlink_core::security::random_secret;
use pierlink_core::ssh::args::{Purpose, SshCommand, build_args};
use pierlink_core::storage::ServerInstance;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};

/// 随包程序清单（scripts/fetch-bundled.mjs 生成）
#[derive(Deserialize)]
struct BundledManifest {
    version: String,
    linux: HashMap<String, BundledFile>,
}
#[derive(Deserialize)]
struct BundledFile {
    file: String,
    sha256: String,
    size: u64,
}

/// 进行中的部署：检查结果、计划、取消标志（按服务器 ID）
#[derive(Default)]
pub struct DeployState {
    sessions: Mutex<HashMap<String, Session>>,
}
struct Session {
    report: ServerReport,
    plan: Option<DeployPlan>,
    cancel: Arc<AtomicBool>,
}

/// 检查结果里需要告诉用户的情况（与前端 DeployWarning 对应）
#[derive(Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DeployWarningDto {
    Linger {
        user: String,
    },
    NoSystemd {
        reason: String,
    },
    KillUserProcesses,
    #[serde(rename_all = "camelCase")]
    NetworkFs {
        fs_type: String,
    },
    OwnSyncthing {
        count: u32,
    },
    LowInotify {
        limit: u32,
    },
    LockHeld {
        owner: String,
    },
}

/// 部署无法进行的原因
#[derive(Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DeployBlockerDto {
    UnsupportedArch {
        arch: String,
    },
    Noexec,
    #[serde(rename_all = "camelCase")]
    InstanceNewer {
        server_version: String,
        app_version: String,
    },
}

/// 检查结果（与前端 DeployReportView 对应）
#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DeployReportDto {
    pub server_name: String,
    pub user: String,
    pub os_name: Option<String>,
    pub arch: String,
    pub disk_mb: u32,
    /// none / reuse / start / upgrade
    pub existing: String,
    pub other_computers: Vec<String>,
    pub warnings: Vec<DeployWarningDto>,
    pub blocker: Option<DeployBlockerDto>,
    /// 部署时启动方式是否为 systemd（界面据此显示第 4 步的说明）
    pub uses_systemd: bool,
}

/// 部署进度事件
#[derive(Serialize, Type, Clone)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DeployEventDto {
    /// 某一步的状态变化：step 为 inspect / upload / configure / start / tunnel / pair；state 为 running / done / skipped / failed
    Step { step: String, state: String },
    /// 上传进度（0–1）
    Progress { step: String, fraction: f64 },
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// 随包资源目录里的 syncthing 子目录
fn bundled_dir(app: &AppHandle) -> Result<PathBuf, AppError> {
    Ok(app
        .path()
        .resource_dir()
        .map_err(|e| AppError::new("deploy.bundle_missing").with("reason", e.to_string()))?
        .join("syncthing"))
}

fn read_manifest(app: &AppHandle) -> Result<BundledManifest, AppError> {
    let path = bundled_dir(app)?.join("manifest.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| AppError::new("deploy.bundle_missing").with("path", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|_| AppError::new("deploy.bundle_missing").with("path", path.display()))
}

fn arch_key(arch: Arch) -> &'static str {
    match arch {
        Arch::X86_64 => "amd64",
        Arch::Aarch64 => "arm64",
    }
}

/// 后台远程命令用的 ssh 参数。
///
/// :param access: 用户操作（部署：缺密码时弹框）还是后台功能（资源概览：缺密码时等待）
pub(crate) fn background_command(
    app: &AppHandle,
    server_id: &str,
    access: crate::keys::Access,
) -> Result<(SshProgram, SshCommand, String), AppError> {
    let server = crate::servers::load(app, server_id)?;
    let target = crate::servers::target(&server)?;
    let cmd = crate::builtin::background(app, &server, access, || {
        build_args(
            &target,
            &Purpose::Background,
            &crate::keys::ssh_opts(app, &server, access)?,
        )
    })?;
    Ok((
        SshProgram {
            path: find_ssh(app)?,
        },
        cmd,
        server.name,
    ))
}

/// 由检查结果得出界面要显示的提醒
fn warnings(r: &ServerReport) -> Vec<DeployWarningDto> {
    let mut out = Vec::new();
    match r.systemd {
        SystemdState::Available => {
            if r.linger == Some(false) {
                out.push(DeployWarningDto::Linger {
                    user: r.user.clone(),
                });
            }
        }
        other => out.push(DeployWarningDto::NoSystemd {
            reason: match other {
                SystemdState::None => "none",
                SystemdState::UserManagerNotRunning => "userManagerNotRunning",
                _ => "busUnreachable",
            }
            .into(),
        }),
    }
    if r.kill_user_processes == Some(true) && r.linger != Some(true) {
        out.push(DeployWarningDto::KillUserProcesses);
    }
    let fs = r.home_fs_type.to_ascii_lowercase();
    if [
        "nfs",
        "cifs",
        "smb",
        "fuse.sshfs",
        "9p",
        "lustre",
        "gpfs",
        "ceph",
    ]
    .iter()
    .any(|n| fs.starts_with(n))
    {
        out.push(DeployWarningDto::NetworkFs {
            fs_type: r.home_fs_type.clone(),
        });
    }
    if r.own_syncthing_running > 0 {
        out.push(DeployWarningDto::OwnSyncthing {
            count: r.own_syncthing_running,
        });
    }
    if let Some(limit) = r.inotify_max_user_watches.filter(|l| *l < 65_536) {
        out.push(DeployWarningDto::LowInotify {
            limit: u32::try_from(limit).unwrap_or(u32::MAX),
        });
    }
    if let Some(lock) = &r.lock {
        out.push(DeployWarningDto::LockHeld {
            owner: lock.owner.clone(),
        });
    }
    out
}

/// 检查服务器并生成部署计划。部署无法进行时（架构不支持、home 不能执行程序、服务器版本更新）放在 blocker 里返回，不算错误。
#[tauri::command]
#[specta::specta]
pub async fn deploy_inspect(
    app: AppHandle,
    server_id: String,
) -> Result<DeployReportDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let manifest = read_manifest(&app)?;
        let (program, cmd, server_name) =
            background_command(&app, &server_id, crate::keys::Access::User("部署同步组件"))?;
        let report = deploy::inspect(&program, &cmd)?;
        let (plan, blocker) = match deploy::plan(&report, &manifest.version) {
            Ok(p) => (Some(p), None),
            Err(e) if e.code == "deploy.unsupported_arch" => (
                None,
                Some(DeployBlockerDto::UnsupportedArch {
                    arch: report.arch.clone(),
                }),
            ),
            Err(e) if e.code == "deploy.home_noexec" => (None, Some(DeployBlockerDto::Noexec)),
            Err(e) if e.code == "deploy.instance_newer" => (
                None,
                Some(DeployBlockerDto::InstanceNewer {
                    server_version: e
                        .params
                        .get("instance_version")
                        .cloned()
                        .unwrap_or_default(),
                    app_version: manifest.version.clone(),
                }),
            ),
            Err(e) => return Err(e),
        };
        let existing = match (&report.instance, &plan) {
            (None, _) => "none",
            (Some(_), Some(p)) if p.upgrade => "upgrade",
            (Some(i), _) if !i.running => "start",
            _ => "reuse",
        };
        let size = plan
            .as_ref()
            .and_then(|p| manifest.linux.get(arch_key(p.arch)))
            .map_or(0, |f| f.size);
        let dto = DeployReportDto {
            server_name,
            user: report.user.clone(),
            os_name: report.os_name.clone(),
            arch: report.arch.clone(),
            disk_mb: u32::try_from(size / (1024 * 1024) + 2).unwrap_or(u32::MAX),
            existing: existing.into(),
            other_computers: plan
                .as_ref()
                .map(|p| {
                    p.other_devices
                        .iter()
                        .map(|d| d.chars().take(7).collect())
                        .collect()
                })
                .unwrap_or_default(),
            warnings: warnings(&report),
            blocker,
            uses_systemd: report.systemd == SystemdState::Available,
        };
        app.state::<DeployState>()
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                server_id,
                Session {
                    report,
                    plan,
                    cancel: Arc::new(AtomicBool::new(false)),
                },
            );
        Ok(dto)
    })
    .await
    .map_err(|e| AppError::new("deploy.task_failed").with("reason", e.to_string()))?
}

/// 用户授权后执行部署。进度经 on_event 推送；失败时停在出错的那一步并返回错误，界面可重试（会从头再检查一遍，已完成的部分会被复用）。
#[tauri::command]
#[specta::specta]
pub async fn deploy_run(
    app: AppHandle,
    server_id: String,
    upgrade_confirmed: bool,
    on_event: Channel<DeployEventDto>,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        run(&app, &server_id, upgrade_confirmed, &on_event)
    })
    .await
    .map_err(|e| AppError::new("deploy.task_failed").with("reason", e.to_string()))?
}

fn step(ch: &Channel<DeployEventDto>, step: &str, state: &str) {
    let _ = ch.send(DeployEventDto::Step {
        step: step.into(),
        state: state.into(),
    });
}

fn run(
    app: &AppHandle,
    server_id: &str,
    upgrade_confirmed: bool,
    ch: &Channel<DeployEventDto>,
) -> Result<(), AppError> {
    let (report, plan, cancel) = {
        let sessions = app.state::<DeployState>();
        let guard = sessions.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let s = guard
            .get(server_id)
            .ok_or_else(|| AppError::new("deploy.not_inspected"))?;
        (
            s.report.clone(),
            s.plan
                .clone()
                .ok_or_else(|| AppError::new("deploy.blocked"))?,
            s.cancel.clone(),
        )
    };
    cancel.store(false, Ordering::SeqCst);
    let mut plan = plan;
    // 用户点了“授权并部署”：记录自动启动授权；升级时是否确认由界面传入
    plan.autostart_consent = true;
    plan.upgrade_confirmed = upgrade_confirmed;

    let manifest = read_manifest(app)?;
    let (program, cmd, _) =
        background_command(app, server_id, crate::keys::Access::User("部署同步组件"))?;
    let reuse_running = report.instance.as_ref().is_some_and(|i| i.running) && !plan.upgrade;
    step(ch, "inspect", "done");

    // 上传
    if plan.upload_required {
        step(ch, "upload", "running");
        let file = manifest
            .linux
            .get(arch_key(plan.arch))
            .ok_or_else(|| AppError::new("deploy.bundle_missing"))?;
        let local = bundled_dir(app)?.join(&file.file);
        let progress = |done: u64, total: u64| {
            if total > 0 {
                let _ = ch.send(DeployEventDto::Progress {
                    step: "upload".into(),
                    fraction: done as f64 / total as f64,
                });
            }
        };
        if let Err(e) = deploy::upload_binary(
            &program,
            &cmd,
            &local,
            &file.sha256,
            &plan.version,
            plan.arch,
            &progress,
            &cancel,
        ) {
            step(ch, "upload", "failed");
            return Err(e);
        }
        step(ch, "upload", "done");
    } else {
        step(ch, "upload", "skipped");
    }

    // 生成配置（新建或升级时）。服务器确认写入后才把凭据存进凭据管理器；
    // 万一存储失败，下次连接时会用 read_api_key 从服务器读回，不会丢失管理权限。
    let store = crate::sync::credential_store()?;
    let apikey_ref = format!("pierlink/instance/{server_id}/apikey");
    let password_ref = format!("pierlink/instance/{server_id}/gui-password");
    let info = if plan.new_instance || plan.upgrade {
        step(ch, "configure", "running");
        let secrets = InstanceSecrets {
            api_key: random_secret(24),
            gui_user: "pierlink".into(),
            gui_password: random_secret(24),
        };
        match deploy::create_instance(&program, &cmd, &plan, &secrets) {
            Ok(outcome) => {
                if outcome.secrets_applied {
                    store.set(&apikey_ref, &secrets.api_key)?;
                    store.set(&password_ref, &secrets.gui_password)?;
                } else {
                    // 复用或升级已有实例：服务器保留了原有密钥（不能换，否则其他电脑失去管理权限），读取现行密钥
                    let key = deploy::read_api_key(&program, &cmd, &outcome.info.id)?;
                    store.set(&apikey_ref, &key)?;
                }
                step(ch, "configure", "done");
                outcome.info
            }
            Err(e) => {
                step(ch, "configure", "failed");
                return Err(e);
            }
        }
    } else {
        step(ch, "configure", "skipped");
        let info = report
            .instance
            .as_ref()
            .map(|i| i.info.clone())
            .ok_or_else(|| AppError::new("deploy.not_found"))?;
        // 别的电脑部署的实例、或本机重装过：本机没有密钥时从服务器读取，不重建配置
        if store.get(&apikey_ref)?.is_none() {
            let key = deploy::read_api_key(&program, &cmd, &info.id)?;
            store.set(&apikey_ref, &key)?;
        }
        info
    };

    // 启动（已在运行且无需升级时复用，不打断其他电脑的同步）
    let info = if reuse_running {
        step(ch, "start", "skipped");
        info
    } else {
        step(ch, "start", "running");
        match deploy::start_instance(&program, &cmd, &info) {
            Ok(
                StartOutcome::AlreadyRunning { instance }
                | StartOutcome::Systemd { instance, .. }
                | StartOutcome::Nohup { instance, .. },
            ) => {
                step(ch, "start", "done");
                instance
            }
            Err(e) => {
                step(ch, "start", "failed");
                return Err(e);
            }
        }
    };

    let now = now_ms();
    app.state::<DbState>().with(|db| {
        let deployed_at = db
            .get_instance(server_id)?
            .map_or(now, |old| old.deployed_at);
        db.upsert_instance(&ServerInstance {
            server_id: server_id.to_owned(),
            instance_id: info.id.clone(),
            syncthing_version: info.version.clone(),
            arch: report.arch.clone(),
            state_dir: info.state_dir.clone(),
            socket_path: info.socket_path.clone(),
            sync_port: info.sync_port,
            device_id: info.device_id.clone(),
            apikey_cred: apikey_ref.clone(),
            gui_password_cred: password_ref.clone(),
            autostart_consent: true,
            deployed_at,
            updated_at: now,
        })
    })?;
    // 隧道与配对由同步编排接入后完成
    step(ch, "tunnel", "skipped");
    step(ch, "pair", "skipped");
    Ok(())
}

/// 取消进行中的部署（在两步之间、上传的块之间生效）
#[tauri::command]
#[specta::specta]
pub async fn deploy_cancel(app: AppHandle, server_id: String) {
    let state = app.state::<DeployState>();
    let guard = state.sessions.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(s) = guard.get(&server_id) {
        s.cancel.store(true, Ordering::SeqCst);
    }
}
