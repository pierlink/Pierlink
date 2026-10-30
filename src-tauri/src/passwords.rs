//! 密码服务器“连接时输一次密码，所有功能共用”（0.3.1）：内存密码库、askpass 管道服务、密码框请求。
//!
//! ssh 要密码时调用本程序（askpass 模式），本程序经管道把提示转交给这里；这里按登记决定：
//! 库里有密码就直接回答；终端、文件面板这类用户主动打开的连接在库里没有时弹密码框；
//! 资源概览、同步、端口转发这类后台连接不弹框，转为“等待输入密码”，用户输入后自动恢复。
//! 密码只在本次运行的内存里；勾了“记住密码”才存进 Windows 凭据管理器。

use crate::data::DbState;
use crate::servers::password_ref;
use pierlink_core::error::AppError;
use pierlink_core::security::vault::{PasswordVault, Zeroizing};
use pierlink_core::ssh::askpass_pipe::{
    AskHandler, AskpassServer, PromptKind, Registration, RegistrationSpec,
};
use pierlink_core::storage::{AuthMethod, Server, ServerSource};
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// 请求界面弹密码框的事件
pub const REQUEST_EVENT: &str = "pierlink://password-request";
/// 某个请求已结束（超时、退出），界面关掉对应的框
pub const CLOSED_EVENT: &str = "pierlink://password-closed";
/// 某台服务器有了新密码（界面据此重连等待中的终端、刷新状态）
pub const SAVED_EVENT: &str = "pierlink://password-saved";
/// 一个密码框最多等多久（之后当作取消）
const PROMPT_TIMEOUT: Duration = Duration::from_secs(600);

/// 推给界面的密码框请求
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PasswordRequestDto {
    pub id: f64,
    pub server_id: String,
    pub server_name: String,
    /// password（服务器密码）/ other（验证码、私钥口令、跳板机密码等其他提示）/ confirm（是或否）/ notice（只需知道）
    pub kind: String,
    /// 其他提示的原文（来自 ssh 或服务器，只当文字显示）
    pub prompt: Option<String>,
    /// 发起的功能：终端 / 文件面板 …
    pub purpose: String,
    /// 能否“记住密码”（应用内新建、填了用户名的服务器）
    pub can_remember: bool,
}

struct Answer {
    secret: Zeroizing<String>,
    remember: bool,
}

struct Pending {
    dto: PasswordRequestDto,
    tx: mpsc::SyncSender<Option<Answer>>,
}

/// 等待界面回答的密码框
#[derive(Default)]
struct Prompts {
    seq: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    stopped: AtomicBool,
}

impl Prompts {
    /// 弹框并阻塞等待回答；取消、超时、退出时返回 None。cancelled 置位时（调用方放弃等待）也关掉框返回 None
    fn ask(
        &self,
        app: &AppHandle,
        mut dto: PasswordRequestDto,
        cancelled: Option<&AtomicBool>,
    ) -> Option<Answer> {
        if self.stopped.load(Ordering::Acquire) {
            return None;
        }
        let id = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        dto.id = id as f64;
        let (tx, rx) = mpsc::sync_channel(1);
        self.lock().insert(
            id,
            Pending {
                dto: dto.clone(),
                tx,
            },
        );
        eprintln!(
            "请求输入密码: 编号={id}, 服务器={}, 类型={}, 用途={}",
            dto.server_name, dto.kind, dto.purpose
        );
        if let Err(e) = app.emit(REQUEST_EVENT, &dto) {
            eprintln!("推送密码框失败: 编号={id}, 错误={e}");
        }
        let deadline = Instant::now() + PROMPT_TIMEOUT;
        let answer = loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(a) => break Ok(a),
                Err(mpsc::RecvTimeoutError::Disconnected) => break Err(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline
                        || cancelled.is_some_and(|c| c.load(Ordering::Acquire))
                    {
                        break Err(());
                    }
                }
            }
        };
        self.lock().remove(&id);
        match answer {
            Ok(a) => a,
            Err(()) => {
                eprintln!("密码框超时或已取消: 编号={id}");
                let _ = app.emit(CLOSED_EVENT, id as f64);
                None
            }
        }
    }

    /// 回答一个请求；找不到（已超时、已回答）时返回 false
    fn answer(&self, id: u64, answer: Option<Answer>) -> bool {
        match self.lock().remove(&id) {
            Some(p) => p.tx.try_send(answer).is_ok(),
            None => false,
        }
    }

    /// 退出时：取消全部等待中的框，之后不再弹新框
    fn cancel_all(&self, app: &AppHandle) {
        self.stopped.store(true, Ordering::Release);
        let all: Vec<(u64, Pending)> = self.lock().drain().collect();
        for (id, p) in all {
            let _ = p.tx.try_send(None);
            let _ = app.emit(CLOSED_EVENT, id as f64);
        }
    }

    fn list(&self) -> Vec<PasswordRequestDto> {
        let mut v: Vec<PasswordRequestDto> = self.lock().values().map(|p| p.dto.clone()).collect();
        v.sort_by(|a, b| a.id.total_cmp(&b.id));
        v
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Pending>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// 管道服务回调：把 ssh 的提示变成界面上的框
struct UiHandler {
    app: AppHandle,
    prompts: Arc<Prompts>,
}

impl UiHandler {
    fn request(
        &self,
        server_id: &str,
        kind: &str,
        prompt: Option<&str>,
        purpose: &str,
    ) -> PasswordRequestDto {
        request_dto(&self.app, server_id, kind, prompt, purpose)
    }
}

/// 构造推给界面的框
fn request_dto(
    app: &AppHandle,
    server_id: &str,
    kind: &str,
    prompt: Option<&str>,
    purpose: &str,
) -> PasswordRequestDto {
    let server = crate::servers::load(app, server_id).ok();
    PasswordRequestDto {
        id: 0.0,
        server_id: server_id.to_owned(),
        server_name: server
            .as_ref()
            .map_or_else(|| server_id.to_owned(), |s| s.name.clone()),
        kind: kind.to_owned(),
        prompt: prompt.map(str::to_owned),
        purpose: purpose.to_owned(),
        can_remember: server.as_ref().is_some_and(can_remember),
    }
}

impl AskHandler for UiHandler {
    fn ask_password(
        &self,
        server_id: &str,
        purpose: &str,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        let dto = self.request(server_id, "password", None, purpose);
        let Some(answer) = self.prompts.ask(&self.app, dto, None) else {
            return Ok(None);
        };
        if answer.remember
            && let Err(e) = remember(&self.app, server_id, &answer.secret)
        {
            // 没存进凭据管理器不影响这次登录，只是下次还要输
            eprintln!("记住密码失败: 服务器={server_id}, 错误={}", e.code);
        }
        // 核心库在本函数返回后才把密码放进库；等它放好再通知界面
        notify_saved_when_ready(&self.app, server_id);
        Ok(Some(answer.secret))
    }

    fn ask_other(
        &self,
        server_id: &str,
        prompt: &str,
        kind: PromptKind,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        let kind = match kind {
            PromptKind::Password => "other",
            PromptKind::Confirm => "confirm",
            PromptKind::None => "notice",
        };
        let dto = self.request(server_id, kind, Some(prompt), "");
        Ok(self.prompts.ask(&self.app, dto, None).map(|a| a.secret))
    }

    fn cancel_all(&self) {
        self.prompts.cancel_all(&self.app);
    }
}

/// 密码相关的应用状态
pub struct PasswordState {
    pub vault: Arc<PasswordVault>,
    server: Mutex<Option<AskpassServer>>,
    prompts: Arc<Prompts>,
}

impl PasswordState {
    /// 建立内存密码库和 askpass 管道服务；管道建不起来时仍返回状态（退回旧行为：密码在终端里输）
    pub fn start(app: &AppHandle) -> Self {
        let loader_app = app.clone();
        let hook_app = app.clone();
        let vault = Arc::new(
            PasswordVault::new(Some(Arc::new(move |server_id: &str| {
                load_remembered(&loader_app, server_id)
            })))
            .with_on_invalidate(Arc::new(move |server_id: &str| {
                clear_remembered(&hook_app, server_id)
            })),
        );
        let prompts = Arc::new(Prompts::default());
        let handler = Arc::new(UiHandler {
            app: app.clone(),
            prompts: prompts.clone(),
        });
        let temp_root = crate::data::data_dir(app)
            .map(|d| d.join("tmp"))
            .unwrap_or_else(|_| std::env::temp_dir());
        let _ = std::fs::create_dir_all(&temp_root);
        let server = match AskpassServer::start(vault.clone(), handler, &temp_root) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("建立密码管道失败，密码服务器退回在终端里输密码: 错误={e:?}");
                None
            }
        };
        Self {
            vault,
            server: Mutex::new(server),
            prompts,
        }
    }

    /// 内置 SSH 连接要用户回答时弹框（密码、私钥口令、验证码等），返回回答和是否勾了“记住密码”。
    /// cancelled 置位（连接被取消）时关掉框返回 None。
    pub(crate) fn ask_user(
        &self,
        app: &AppHandle,
        server_id: &str,
        kind: &str,
        prompt: Option<&str>,
        purpose: &str,
        cancelled: &AtomicBool,
    ) -> Option<(Zeroizing<String>, bool)> {
        let dto = request_dto(app, server_id, kind, prompt, purpose);
        self.prompts
            .ask(app, dto, Some(cancelled))
            .map(|a| (a.secret, a.remember))
    }

    /// 应用退出：停掉管道（取消等待中的框），再清空内存里的密码
    pub fn shutdown(&self, app: &AppHandle) {
        self.prompts.cancel_all(app);
        drop(self.server.lock().unwrap_or_else(|p| p.into_inner()).take());
        self.vault.clear_all();
    }
}

/// 只有应用内新建、填了用户名的服务器能自动填密码（要严格匹配“用户@主机”的提示）
fn can_remember(server: &Server) -> bool {
    server.source == ServerSource::Manual
        && server.username.is_some()
        && server.host.is_some()
        && matches!(server.auth_method, AuthMethod::Password | AuthMethod::Agent)
}

/// 为一次连接登记（ssh 要密码时按这个登记回答）。
/// 应用内新建、填了用户名、登录方式是“密码”或“ssh-agent / 默认密钥”的服务器才登记；其他返回 None，照旧。
///
/// :param interactive: 库里没有密码时能否弹框（终端、文件面板、用户刚点的操作）
/// :param purpose: 发起的功能，显示在密码框里
pub fn registration(
    app: &AppHandle,
    server: &Server,
    interactive: bool,
    purpose: &str,
) -> Result<Option<Arc<Registration>>, AppError> {
    if !can_remember(server) {
        return Ok(None);
    }
    let (Some(user), Some(host)) = (&server.username, &server.host) else {
        return Ok(None);
    };
    let state = app.state::<PasswordState>();
    let guard = state.server.lock().unwrap_or_else(|p| p.into_inner());
    let Some(pipe) = guard.as_ref() else {
        return Ok(None);
    };
    pipe.register(RegistrationSpec {
        server_id: server.id.clone(),
        expected_prompt: format!("{user}@{}", host.to_ascii_lowercase()),
        interactive,
        purpose: purpose.to_owned(),
        // 选了“密码”的服务器：后台在没有密码时不去连（免得白白失败一次）；默认密钥方式照常连，真要密码时再等
        password_only: server.auth_method == AuthMethod::Password,
    })
    .map(Some)
}

/// 安装专用密钥用的登记：安装框里输入的密码由核心放进内存密码库，再按“用户@主机”严格回答 ssh
pub fn install_registration(
    app: &AppHandle,
    server: &Server,
    expected_prompt: &str,
) -> Result<Arc<Registration>, AppError> {
    let state = app.state::<PasswordState>();
    let guard = state.server.lock().unwrap_or_else(|p| p.into_inner());
    let pipe = guard
        .as_ref()
        .ok_or_else(|| AppError::new("password.pipe_unavailable"))?;
    pipe.register(RegistrationSpec {
        server_id: server.id.clone(),
        expected_prompt: expected_prompt.to_owned(),
        interactive: false,
        purpose: "安装专用密钥".into(),
        password_only: true,
    })
}

/// 凭据载入回调：勾过“记住密码”的服务器从凭据管理器读出密码
fn load_remembered(
    app: &AppHandle,
    server_id: &str,
) -> Result<Option<Zeroizing<String>>, AppError> {
    let Some(server) = app.state::<DbState>().with(|db| db.get_server(server_id))? else {
        return Ok(None);
    };
    let Some(cred) = server.password_cred.filter(|_| server.remember_password) else {
        return Ok(None);
    };
    Ok(crate::sync::credential_store()?
        .get(&cred)?
        .map(Zeroizing::new))
}

/// 记住密码：存进凭据管理器，服务器改为“密码”登录并标记已保存
pub(crate) fn remember(app: &AppHandle, server_id: &str, secret: &str) -> Result<(), AppError> {
    let mut server = crate::servers::load(app, server_id)?;
    if !can_remember(&server) {
        return Err(AppError::new("password.cannot_remember"));
    }
    let key = password_ref(&server.id);
    crate::sync::credential_store()?.set(&key, secret)?;
    server.auth_method = AuthMethod::Password;
    server.remember_password = true;
    server.password_cred = Some(key);
    server.updated_at = now_ms();
    app.state::<DbState>()
        .with(|db| db.update_server(&server))?;
    eprintln!("已记住密码: 服务器={}", server.name);
    Ok(())
}

/// 内存里的密码登录被拒、已作废：如果它是保存过的密码，也从凭据管理器删掉（否则下次启动每个后台连接都会先用错密码失败一次）
fn clear_remembered(app: &AppHandle, server_id: &str) {
    let Ok(mut server) = crate::servers::load(app, server_id) else {
        return;
    };
    if !server.remember_password {
        return;
    }
    if let Some(cred) = server.password_cred.take()
        && let Ok(store) = crate::sync::credential_store()
    {
        let _ = store.delete(&cred);
    }
    server.remember_password = false;
    server.updated_at = now_ms();
    match app.state::<DbState>().with(|db| db.update_server(&server)) {
        Ok(()) => eprintln!("保存的密码登录被拒，已删除: 服务器={}", server.name),
        Err(e) => eprintln!(
            "删除保存的错误密码失败: 服务器={}, 错误={}",
            server.name, e.code
        ),
    }
}

/// 后台功能连不上是不是因为缺密码：选了“密码”登录、本次运行里没有密码、也没记住密码
pub fn needs_password(app: &AppHandle, server_id: &str) -> bool {
    let Ok(server) = crate::servers::load(app, server_id) else {
        return false;
    };
    if !can_remember(&server) || server.auth_method != AuthMethod::Password {
        return false;
    }
    let Some(state) = app.try_state::<PasswordState>() else {
        return false;
    };
    // 记住了密码的：需要时自动从凭据管理器读，不算缺
    !state.vault.has(server_id) && !(server.remember_password && server.password_cred.is_some())
}

/// 这台服务器现在是否有等待回答的密码框
pub fn prompt_open(app: &AppHandle, server_id: &str) -> bool {
    app.try_state::<PasswordState>().is_some_and(|state| {
        state
            .prompts
            .lock()
            .values()
            .any(|p| p.dto.server_id == server_id && p.dto.kind == "password")
    })
}

/// 忘掉某台服务器在内存里的密码（删除服务器、改了保存的密码、清除保存的密码时调用）
pub fn forget(app: &AppHandle, server_id: &str) {
    if let Some(state) = app.try_state::<PasswordState>()
        && state.vault.forget(server_id)
    {
        eprintln!("忘掉内存里的密码: 服务器={server_id}");
    }
}

/// 等核心库把密码放进库后，通知界面“这台服务器有密码了”
fn notify_saved_when_ready(app: &AppHandle, server_id: &str) {
    let app = app.clone();
    let server_id = server_id.to_owned();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let vault = app.state::<PasswordState>().vault.clone();
        while !vault.has(&server_id) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if vault.has(&server_id) {
            let _ = app.emit(SAVED_EVENT, &server_id);
            crate::sync::resume_server(&app, &server_id);
        }
    });
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn check_password(password: &str) -> Result<(), AppError> {
    if password.is_empty() || password.len() > 1024 || password.contains(['\0', '\n', '\r']) {
        return Err(AppError::new("server.invalid_field").with("field", "password"));
    }
    Ok(())
}

/// 回答一个密码框。
///
/// :param id: 请求编号（事件里给的）
/// :param password: 输入的内容；None 表示取消
/// :param remember: 是否记住密码（只对服务器密码有效）
#[tauri::command]
#[specta::specta]
pub async fn password_answer(
    app: AppHandle,
    id: f64,
    password: Option<String>,
    remember: bool,
) -> Result<bool, AppError> {
    let state = app.state::<PasswordState>();
    let answer = match password {
        Some(p) => {
            // 确认框（是/否）、通知框的回答不是密码，只检查不含换行等控制字符
            if p.len() > 1024 || p.contains(['\0', '\n', '\r']) {
                return Err(AppError::new("server.invalid_field").with("field", "password"));
            }
            Some(Answer {
                secret: Zeroizing::new(p),
                remember,
            })
        }
        None => None,
    };
    Ok(state.prompts.answer(id as u64, answer))
}

/// 用户主动输入某台服务器的密码（转发、同步、资源概览“等待输入密码”时的入口）。
/// 存进本次运行的内存，等待中的后台功能随即自动恢复；勾了记住密码时同时存进凭据管理器。
#[tauri::command]
#[specta::specta]
pub async fn password_enter(
    app: AppHandle,
    server_id: String,
    password: String,
    remember: bool,
) -> Result<(), AppError> {
    check_password(&password)?;
    tauri::async_runtime::spawn_blocking(move || {
        let server = crate::servers::load(&app, &server_id)?;
        if !can_remember(&server) {
            return Err(AppError::new("password.cannot_remember"));
        }
        if remember {
            remember_or_log(&app, &server_id, &password);
        }
        app.state::<PasswordState>()
            .vault
            .set(&server_id, Zeroizing::new(password))?;
        eprintln!("已输入密码: 服务器={}", server.name);
        let _ = app.emit(SAVED_EVENT, &server_id);
        crate::sync::resume_server(&app, &server_id);
        Ok(())
    })
    .await
    .map_err(|e| AppError::new("password.task_failed").with("reason", e.to_string()))?
}

fn remember_or_log(app: &AppHandle, server_id: &str, password: &str) {
    if let Err(e) = remember(app, server_id, password) {
        eprintln!("记住密码失败: 服务器={server_id}, 错误={}", e.code);
    }
}

/// 现在等待回答的密码框（页面重新加载后用来恢复显示）
#[tauri::command]
#[specta::specta]
pub async fn password_pending(app: AppHandle) -> Result<Vec<PasswordRequestDto>, AppError> {
    Ok(app.state::<PasswordState>().prompts.list())
}

/// 某台服务器本次运行里是否已有密码（不触发读取凭据管理器）
#[tauri::command]
#[specta::specta]
pub async fn password_known(app: AppHandle, server_id: String) -> Result<bool, AppError> {
    Ok(app.state::<PasswordState>().vault.has(&server_id))
}
