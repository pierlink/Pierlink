//! 密码服务器的专用密钥（设计 3.1）：Tauri 命令、连接参数、以及 ssh 调用的“自动填口令”模式。
//! 核心逻辑在 `pierlink_core::keys`、`pierlink_core::ssh::askpass`。

use crate::data::{DbState, data_dir};
use crate::sync::{computer_name, credential_store};
use crate::terminal::find_ssh;
use pierlink_core::error::AppError;
use pierlink_core::keys::{local, remote};
use pierlink_core::process::SshProgram;
use pierlink_core::ssh::args::{AskPass, CommonOpts, DedicatedKey, Purpose, build_args};
use pierlink_core::ssh::askpass::{self, AskRequest};
use pierlink_core::ssh::askpass_pipe;
use pierlink_core::storage::{AuthMethod, Server, ServerSource};
use serde::Serialize;
use specta::Type;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

/// 私钥文件名（与核心约定一致）
const KEY_FILE: &str = "pierlink_ed25519";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// 本次运行中已经检查过的本机密钥（检查要解开私钥、核对 ACL，较慢；一次运行只检查一次）。
/// 安装时重新生成会更新它；私钥文件或口令条目不见了时作废。
static CHECKED: std::sync::Mutex<Option<local::LocalKey>> = std::sync::Mutex::new(None);

fn cached_key() -> Option<local::LocalKey> {
    let key = CHECKED.lock().unwrap_or_else(|p| p.into_inner()).clone()?;
    let passphrase = credential_store()
        .ok()
        .and_then(|s| s.get(local::CREDENTIAL_ID).ok().flatten());
    (key.private_path.is_file() && passphrase.is_some()).then_some(key)
}

fn remember_key(key: &local::LocalKey) {
    *CHECKED.lock().unwrap_or_else(|p| p.into_inner()) = Some(key.clone());
}

/// 读取本机密钥：本次运行检查过就直接用，否则完整检查一次
fn load_key(app: &AppHandle) -> Result<Option<local::LocalKey>, AppError> {
    if let Some(key) = cached_key() {
        return Ok(Some(key));
    }
    let store = credential_store()?;
    let key = local::load_local_key(&key_dir(app)?, store.as_ref())?;
    if let Some(k) = &key {
        remember_key(k);
    }
    Ok(key)
}

/// 内置 SSH 用的本机专用密钥：私钥路径和凭据管理器里的口令；本机密钥丢失时返回 None
pub(crate) fn dedicated_credential(
    app: &AppHandle,
) -> Result<Option<(PathBuf, pierlink_core::security::vault::Zeroizing<String>)>, AppError> {
    let path = key_dir(app)?.join(KEY_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(credential_store()?
        .get(local::CREDENTIAL_ID)?
        .map(|p| (path, pierlink_core::security::vault::Zeroizing::new(p))))
}

fn key_dir(app: &AppHandle) -> Result<PathBuf, AppError> {
    Ok(data_dir(app)?.join("keys"))
}

fn askpass_program() -> Result<PathBuf, AppError> {
    std::env::current_exe()
        .map_err(|e| AppError::new("keys.local_failed").with("reason", e.to_string()))
}

/// 连接从哪里发起：决定内存里没有密码时能不能弹密码框
#[derive(Clone, Copy, Debug)]
pub enum Access {
    /// 用户刚做的操作（打开终端、文件面板、部署、新建同步……）：缺密码时弹框，参数是显示在框里的功能名
    User(&'static str),
    /// 后台功能（资源概览、同步连接、端口转发）：缺密码时不弹框，转为“等待输入密码”
    Background(&'static str),
}

/// 服务器的连接公共参数。
/// 应用内新建、填了用户名、用密码或默认密钥登录的服务器：ssh 要密码时经管道向 Pierlink 要（输一次，所有功能共用）；
/// 用专用密钥的服务器：带上密钥和自动填口令；其他服务器用默认值。
///
/// :param app: 应用句柄
/// :param server: 服务器记录
/// :param access: 用户操作还是后台功能
/// :return: 给 `build_args` 的公共参数
pub fn ssh_opts(app: &AppHandle, server: &Server, access: Access) -> Result<CommonOpts, AppError> {
    if server.auth_method != AuthMethod::DedicatedKey {
        let (interactive, purpose) = match access {
            Access::User(p) => (true, p),
            Access::Background(p) => (false, p),
        };
        return Ok(
            match crate::passwords::registration(app, server, interactive, purpose)? {
                Some(registration) => CommonOpts {
                    askpass: Some(AskPass {
                        program: askpass_program()?,
                        registration,
                    }),
                    ..CommonOpts::default()
                },
                None => CommonOpts::default(),
            },
        );
    }
    let private_path = key_dir(app)?.join(KEY_FILE);
    // 私钥文件或口令条目任一缺失（重装、清理凭据管理器）：提示重新安装，而不是让 ssh 报看不懂的认证失败
    // 读取出错（凭据管理器暂时不可用）直接报错，只有确实没有这条才算丢失
    let passphrase_ok = credential_store()?.get(local::CREDENTIAL_ID)?.is_some();
    if !private_path.is_file() || !passphrase_ok {
        // 本机密钥丢失（重装、手动删除）：提示重新安装，而不是让 ssh 报看不懂的错
        return Err(AppError::new("keys.local_missing").with("server", &server.name));
    }
    Ok(CommonOpts {
        dedicated_key: Some(DedicatedKey {
            private_path,
            askpass_program: askpass_program()?,
            credential_id: local::CREDENTIAL_ID.into(),
        }),
        ..CommonOpts::default()
    })
}

/// 安装需要“用户@主机”来严格匹配密码提示（S23），只支持应用内新建且填了用户名的服务器
fn expected_prompt(server: &Server) -> Result<String, AppError> {
    if server.source != ServerSource::Manual {
        return Err(AppError::new("keys.alias_unsupported"));
    }
    let user = server
        .username
        .as_deref()
        .ok_or_else(|| AppError::new("keys.username_required"))?;
    let host = server
        .host
        .as_deref()
        .ok_or_else(|| AppError::new("server.invalid_field").with("field", "host"))?;
    // ssh 的密码提示里主机名是小写的（用户名保持原样），必须按同样规则才能完全匹配
    Ok(format!("{user}@{}", host.to_ascii_lowercase()))
}

fn program(app: &AppHandle) -> Result<SshProgram, AppError> {
    Ok(SshProgram {
        path: find_ssh(app)?,
    })
}

/// 用专用密钥登录的后台命令参数
fn key_command(
    app: &AppHandle,
    server: &Server,
) -> Result<pierlink_core::ssh::args::SshCommand, AppError> {
    let mut s = server.clone();
    s.auth_method = AuthMethod::DedicatedKey;
    let access = Access::Background("专用密钥");
    crate::builtin::background(app, &s, access, || {
        build_args(
            &crate::servers::target(&s)?,
            &Purpose::Background,
            &ssh_opts(app, &s, access)?,
        )
    })
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizedKeyDto {
    pub line_no: f64,
    pub key_type: String,
    pub fingerprint: String,
    pub comment: String,
    /// “类型 公钥数据”，移除时原样传回
    pub public_line: String,
    pub ours: bool,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KeyStatusDto {
    pub local_fingerprint: Option<String>,
    pub keys: Vec<AuthorizedKeyDto>,
    pub warnings: Vec<AppError>,
}

#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KeyInstallDto {
    /// installed / alreadyPresent
    pub outcome: String,
    /// 服务器 ~/.ssh 或 authorized_keys 权限过宽（不擅自修改，S20）
    pub permissions_warning: bool,
    /// 服务器的 AuthorizedKeysFile 不在默认位置（以验证结果为准，S16）
    pub non_default_authorized_keys: bool,
    /// 本电脑的专用密钥这次被重新生成（旧的丢失或损坏）
    pub regenerated: bool,
    /// 密钥重新生成后，之前装过旧密钥、需要重新安装的其他服务器
    pub affected_servers: Vec<String>,
}

async fn blocking<T, F>(app: AppHandle, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&AppHandle) -> Result<T, AppError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || f(&app))
        .await
        .map_err(|e| AppError::new("keys.task_failed").with("reason", e.to_string()))?
}

/// 服务器上与 Pierlink 有关的密钥（本电脑的、注释以 pierlink- 开头的）。需要这台服务器已改用专用密钥登录。
#[tauri::command]
#[specta::specta]
pub async fn key_status(app: AppHandle, server_id: String) -> Result<KeyStatusDto, AppError> {
    blocking(app, move |app| {
        let server = crate::servers::load(app, &server_id)?;
        let Some(key) = load_key(app)? else {
            return Ok(KeyStatusDto {
                local_fingerprint: None,
                keys: Vec::new(),
                warnings: Vec::new(),
            });
        };
        let access = Access::User("专用密钥");
        let cmd = crate::builtin::background(app, &server, access, || {
            build_args(
                &crate::servers::target(&server)?,
                &Purpose::Background,
                &ssh_opts(app, &server, access)?,
            )
        })?;
        let keys = remote::list(&program(app)?, &cmd, &key.public_line)?
            .into_iter()
            .filter(|k| k.ours || k.pierlink)
            .map(|k| AuthorizedKeyDto {
                line_no: f64::from(k.line_no),
                key_type: k.key_type,
                fingerprint: k.fingerprint,
                comment: k.comment,
                public_line: k.public_line,
                ours: k.ours,
            })
            .collect();
        Ok(KeyStatusDto {
            local_fingerprint: Some(key.fingerprint),
            keys,
            warnings: Vec::new(),
        })
    })
    .await
}

/// 用一次密码安装本电脑的专用密钥并验证；成功后这台服务器改用专用密钥登录。
/// 密码只放进本次运行的内存密码库，由自动填口令程序按完全匹配的提示经管道交给 ssh。
///
/// :param password: 服务器密码（不保存）
#[tauri::command]
#[specta::specta]
pub async fn key_install(
    app: AppHandle,
    server_id: String,
    password: String,
) -> Result<KeyInstallDto, AppError> {
    blocking(app, move |app| {
        let mut server = crate::servers::load(app, &server_id)?;
        let expected = expected_prompt(&server)?;
        let store = credential_store()?;
        let short_id = app.state::<DbState>().with(|db| db.device_short_id())?;
        let comment = local::key_comment(&computer_name(), &short_id)?;
        let key = local::ensure_local_key(&key_dir(app)?, &comment, store.as_ref())?;
        remember_key(&key);
        // 密钥重新生成后，其他用专用密钥登录的服务器上装的是旧公钥，需要重新安装
        let affected_servers: Vec<String> = if key.regenerated {
            eprintln!("本机专用密钥已重新生成: 指纹={}", key.fingerprint);
            app.state::<DbState>()
                .with(|db| db.list_servers())?
                .into_iter()
                .filter(|s| s.id != server.id && s.auth_method == AuthMethod::DedicatedKey)
                .map(|s| s.name)
                .collect()
        } else {
            Vec::new()
        };
        let target = crate::servers::target(&server)?;
        // 安装时输入的密码经内存密码库交给 ssh（不再写凭据管理器的临时条目）
        // 内置 SSH：安装时输入的密码由核心库直接放进内存密码库，不经 askpass 管道
        let password_cmd = crate::builtin::background(
            app,
            &server,
            Access::Background("安装专用密钥"),
            || {
                build_args(
                    &target,
                    &Purpose::Background,
                    &CommonOpts {
                        askpass: Some(AskPass {
                            program: askpass_program()?,
                            registration: crate::passwords::install_registration(
                                app, &server, &expected,
                            )?,
                        }),
                        ..CommonOpts::default()
                    },
                )
            },
        )?;
        let key_cmd = key_command(app, &server)?;
        let report = remote::install_with_password(
            &program(app)?,
            &password_cmd,
            &key_cmd,
            &key.public_line,
            &password,
        )?;
        server.auth_method = AuthMethod::DedicatedKey;
        server.updated_at = now_ms();
        app.state::<DbState>()
            .with(|db| db.update_server(&server))?;
        // 之后改用专用密钥登录，内存里的密码用不上了
        crate::passwords::forget(app, &server.id);
        eprintln!(
            "安装专用密钥: 服务器={}, 指纹={}",
            server.name, key.fingerprint
        );
        Ok(KeyInstallDto {
            outcome: match report.outcome {
                remote::InstallOutcome::Installed => "installed".into(),
                remote::InstallOutcome::AlreadyPresent => "alreadyPresent".into(),
            },
            permissions_warning: report.permissions_warning,
            non_default_authorized_keys: report.non_default_authorized_keys,
            regenerated: key.regenerated,
            affected_servers,
        })
    })
    .await
}

/// 从服务器移除一行密钥（只删内容完全一致的行）。移除的是本电脑的密钥时，这台服务器改回密码登录。
///
/// :param public_line: `key_status` 返回的“类型 公钥数据”
/// :return: removed / notFound / symlink
#[tauri::command]
#[specta::specta]
pub async fn key_remove(
    app: AppHandle,
    server_id: String,
    public_line: String,
) -> Result<String, AppError> {
    blocking(app, move |app| {
        let mut server = crate::servers::load(app, &server_id)?;
        let ours = load_key(app)?.is_some_and(|k| same_key(&k.public_line, &public_line));
        let access = Access::User("专用密钥");
        let cmd = crate::builtin::background(app, &server, access, || {
            build_args(
                &crate::servers::target(&server)?,
                &Purpose::Background,
                &ssh_opts(app, &server, access)?,
            )
        })?;
        let outcome = match remote::remove(&program(app)?, &cmd, &public_line)? {
            remote::RemoveOutcome::Removed { .. } => "removed",
            remote::RemoveOutcome::NotFound => "notFound",
            remote::RemoveOutcome::Symlink => "symlink",
        };
        if ours && outcome != "symlink" && server.auth_method == AuthMethod::DedicatedKey {
            server.auth_method = AuthMethod::Password;
            server.updated_at = now_ms();
            app.state::<DbState>()
                .with(|db| db.update_server(&server))?;
        }
        eprintln!("移除专用密钥: 服务器={}, 结果={outcome}", server.name);
        Ok(outcome.into())
    })
    .await
}

/// 两行公钥的“类型 数据”是否相同（不看注释）
fn same_key(a: &str, b: &str) -> bool {
    a.split_whitespace()
        .take(2)
        .eq(b.split_whitespace().take(2))
}

/// ssh 调用本程序作为 SSH_ASKPASS 时的处理。在应用启动的最开始调用（单实例、窗口都还没初始化）。
/// 专用密钥的口令由本机凭据管理器直接回答（只认这把密钥的口令提示）；
/// 服务器密码等其他提示经管道交给正在运行的 Pierlink 决定（内存里有就回答，终端里没有就弹框）。
///
/// :return: 是自动填口令调用时返回进程退出码；普通启动返回 None
pub fn run_askpass_if_requested() -> Option<i32> {
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    // ssh 调用 askpass 时只传一个参数：提示原文
    if args.len() != 2 {
        return None;
    }
    let prompt = &args[1];
    if let Some(key_path) = std::env::var_os("PIERLINK_KEY_PATH") {
        return Some(answer_key_passphrase(prompt, PathBuf::from(key_path)));
    }
    if std::env::var_os("PIERLINK_ASKPASS_PIPE").is_some() {
        return Some(askpass_pipe::client_main(prompt));
    }
    None
}

/// 回答专用密钥的口令提示：成功时把口令写到标准输出并返回 0，否则返回 1
fn answer_key_passphrase(prompt: &str, key_path: PathBuf) -> i32 {
    // 只允许读取专用密钥口令，其他凭据（服务器密码、同步组件密钥等）一律拒绝
    if std::env::var("PIERLINK_CREDENTIAL_ID").ok().as_deref() != Some(local::CREDENTIAL_ID) {
        eprintln!("Pierlink: 拒绝读取这个凭据");
        return 1;
    }
    let req = AskRequest::KeyPassphrase { key_path };
    let result = credential_store()
        .and_then(|store| askpass::answer(prompt, &req, store.as_ref(), local::CREDENTIAL_ID));
    match result {
        Ok(secret) => {
            let mut out = std::io::stdout().lock();
            if writeln!(out, "{secret}")
                .and_then(|()| out.flush())
                .is_err()
            {
                return 1;
            }
            0
        }
        Err(e) => {
            // 不回显提示原文和凭据引用
            eprintln!("Pierlink: 未自动填写（{}）", e.code);
            1
        }
    }
}
