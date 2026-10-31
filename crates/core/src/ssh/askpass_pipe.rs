//! 一次连接一枚 nonce 的本地 askpass 协议；所有身份与交互权限只取自主进程登记。
use crate::{
    error::AppError,
    security::{
        random_secret,
        vault::{Password, PasswordVault, VaultSubscription, Zeroizing},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::Path,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
#[cfg(unix)]
#[path = "askpass_pipe/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "askpass_pipe/windows.rs"]
mod platform;
const VERSION: u8 = 1;
const MAX_MESSAGE: usize = 4096;
const TIMEOUT: Duration = Duration::from_secs(120);

/// OpenSSH 的提示类别；confirm 与 none 均不得自动读取服务器密码。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    Password,
    Confirm,
    None,
}
/// 界面处理器在工作线程调用；返回值自动擦除，取消返回 None，错误内容不会传入协议或日志。
pub trait AskHandler: Send + Sync + 'static {
    /// 为 server_id 和 purpose 请求服务器密码；返回密码或取消。同服务器的并发请求只弹一次。
    fn ask_password(
        &self,
        server_id: &str,
        purpose: &str,
    ) -> Result<Option<Zeroizing<String>>, AppError>;
    /// 请求其他提示的答案；prompt 是不可信文本，只能当文本展示，答案不入密码库。
    fn ask_other(
        &self,
        server_id: &str,
        prompt: &str,
        kind: PromptKind,
    ) -> Result<Option<Zeroizing<String>>, AppError>;
    /// 服务退出时取消尚未关闭的界面；默认无操作，界面实现应让阻塞的处理器及时返回。
    fn cancel_all(&self) {}
}
/// 本次 SSH 连接的主进程登记信息；expected_prompt 必须为解析后的用户@主机。
#[derive(Clone)]
pub struct RegistrationSpec {
    pub server_id: String,
    pub expected_prompt: String,
    pub interactive: bool,
    pub purpose: String,
    /// 这台服务器只能用密码登录（登录方式选的是“密码”）：后台连接在库里没密码时不启动 ssh。
    /// 为 false 时（默认密钥等方式，密码只是服务器的退路）照常启动，ssh 真来要密码时再拒绝并转入等待。
    pub password_only: bool,
}
/// 本次连接的认证结果；只包含代数和固定拒绝原因，不包含提示或密码。
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct RegistrationStatus {
    pub used_generation: Option<u64>,
    pub password_needed: bool,
    pub other_required: bool,
}
struct Binding {
    spec: RegistrationSpec,
    status: Mutex<RegistrationStatus>,
    request: Mutex<()>,
}
type FlightResult = Result<Option<Password>, ()>;
#[derive(Default)]
struct Flight {
    result: Mutex<Option<FlightResult>>,
    done: Condvar,
}
struct Inner {
    endpoint: String,
    vault: Arc<PasswordVault>,
    handler: Arc<dyn AskHandler>,
    bindings: Mutex<HashMap<String, Weak<Binding>>>,
    flights: Mutex<HashMap<String, Weak<Flight>>>,
    stopped: Arc<AtomicBool>,
}
/// 主进程启动一次并持有至退出；Drop 关闭监听、取消处理器并回收可结束的工作线程。
pub struct AskpassServer {
    inner: Arc<Inner>,
    worker: Option<JoinHandle<()>>,
}
/// 登记守卫；最后一个 Arc 释放时注销 nonce。命令与进程都持有它，保证存活期间可回答提示。
pub struct Registration {
    inner: Arc<Inner>,
    binding: Arc<Binding>,
    nonce: String,
}
impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Registration { .. }")
    }
}
impl PartialEq for Registration {
    fn eq(&self, other: &Self) -> bool {
        self.nonce == other.nonce
    }
}
impl Eq for Registration {}
impl AskpassServer {
    /// 使用 vault、界面 handler 和数据盘 temp_root 建立私有 IPC；返回服务端或脱敏错误。
    /// Unix 临时目录由服务自动删除；Windows 使用显式用户 DACL 的命名管道。
    pub fn start(
        vault: Arc<PasswordVault>,
        handler: Arc<dyn AskHandler>,
        temp_root: &Path,
    ) -> Result<Self, AppError> {
        Self::start_with_client(vault, handler, temp_root, None)
    }
    /// 同 [`Self::start`]，另外指定只接受哪个程序来要密码（Windows 校验客户端程序路径）。
    /// None 表示本程序自己；测试用打包出的 Pierlink.exe 当客户端时传它的路径。
    pub fn start_with_client(
        vault: Arc<PasswordVault>,
        handler: Arc<dyn AskHandler>,
        temp_root: &Path,
        client: Option<&Path>,
    ) -> Result<Self, AppError> {
        let listener = platform::Listener::bind(temp_root, client)?;
        let inner = Arc::new(Inner {
            endpoint: listener.endpoint().into(),
            vault,
            handler,
            bindings: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
            stopped: Arc::new(AtomicBool::new(false)),
        });
        let shared = inner.clone();
        let worker = thread::Builder::new()
            .name("密码管道监听".into())
            .spawn(move || serve(listener, shared))
            .map_err(|_| failed("ssh.create_listener_thread", "创建监听线程"))?;
        Ok(Self {
            inner,
            worker: Some(worker),
        })
    }
    /// 登记 spec 并返回守卫；尚缺密码时也可创建等待中的后台命令，真正启动前由 prepare 检查。
    pub fn register(&self, spec: RegistrationSpec) -> Result<Arc<Registration>, AppError> {
        register(&self.inner, spec)
    }
}
impl Drop for AskpassServer {
    fn drop(&mut self) {
        self.inner.stopped.store(true, Ordering::Release);
        self.inner.handler.cancel_all();
        if let Some(worker) = self.worker.take() {
            crate::process::join_bounded(worker, Duration::from_secs(2));
        }
    }
}
fn register(inner: &Arc<Inner>, mut spec: RegistrationSpec) -> Result<Arc<Registration>, AppError> {
    if inner.stopped.load(Ordering::Acquire) {
        return Err(failed("ssh.service_has_stopped", "服务已停止"));
    }
    if spec.server_id.is_empty()
        || !spec.expected_prompt.contains('@')
        || spec.expected_prompt.contains(['\0', '\n', '\r'])
    {
        return Err(failed("ssh.invalid_registered_target", "登记目标无效"));
    }
    spec.expected_prompt = super::askpass::normalize_password_target(&spec.expected_prompt);
    let binding = Arc::new(Binding {
        spec,
        status: Mutex::new(RegistrationStatus::default()),
        request: Mutex::new(()),
    });
    let nonce = random_secret(16);
    let mut bindings = inner.bindings.lock().unwrap_or_else(|e| e.into_inner());
    if bindings.len() >= 4096 {
        return Err(failed("ssh.too_many_registrations", "登记数量超限"));
    }
    bindings.insert(nonce.clone(), Arc::downgrade(&binding));
    Ok(Arc::new(Registration {
        inner: inner.clone(),
        binding,
        nonce,
    }))
}
impl Registration {
    /// 无参数；返回随机管道名或私有套接字路径，传给子进程。
    pub fn pipe(&self) -> &str {
        &self.inner.endpoint
    }
    /// 无参数；返回本次连接的随机 nonce，不可写入日志。
    pub fn nonce(&self) -> &str {
        &self.nonce
    }
    /// 无参数；返回登记的服务器 ID。
    pub fn server_id(&self) -> &str {
        &self.binding.spec.server_id
    }
    /// 无参数；返回主进程登记的规范化用户@主机。
    pub fn expected_prompt(&self) -> &str {
        &self.binding.spec.expected_prompt
    }
    /// 无参数；返回该连接使用代数与拒绝原因的快照，不含任何秘密。
    pub fn status(&self) -> RegistrationStatus {
        self.binding
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    /// 无参数；为下一次实际启动重新登记一次一用 nonce，后台在缺密码时返回 password_needed，不启动进程。
    pub fn prepare(&self) -> Result<Arc<Self>, AppError> {
        let guard = register(&self.inner, self.binding.spec.clone())?;
        guard.ensure_ready()?;
        Ok(guard)
    }
    fn ensure_ready(&self) -> Result<(), AppError> {
        if !self.binding.spec.interactive {
            if self
                .inner
                .vault
                .seen_two_factor(&self.binding.spec.server_id)
            {
                return Err(AppError::new("ssh.two_factor_required"));
            }
            if self.binding.spec.password_only
                && self
                    .inner
                    .vault
                    .get(&self.binding.spec.server_id)
                    .map_err(|_| {
                        AppError::new("ssh.password_needed").with_reason(
                            "ssh.couldn_t_load_the_saved_password_enter_it_again",
                            "已保存密码载入失败，需要重新输入",
                        )
                    })?
                    .is_none()
            {
                return Err(AppError::new("ssh.password_needed"));
            }
        }
        Ok(())
    }
    /// 无参数；订阅此服务器的新密码；等待连接应收到通知后重新 prepare。
    pub fn subscribe(&self) -> VaultSubscription {
        self.inner.vault.subscribe(&self.binding.spec.server_id)
    }
    /// 无参数；查询当前库是否已有密码，不触发载入，用于避免订阅与写入之间丢失唤醒。
    pub fn has_password(&self) -> bool {
        self.inner.vault.has(&self.binding.spec.server_id)
    }
    /// 将本次 SSH 错误映射为缺密码/二次验证；认证失败仅作废本次用过的代数，移除所有远端错误原文。
    pub fn finish_error(&self, mut error: AppError) -> AppError {
        let status = self.status();
        error.params.remove("stderr_tail");
        if error.code == "ssh.too_many_auth_attempts" {
            return error;
        }
        if error.code == "ssh.auth_failed"
            && let Some(generation) = status.used_generation
        {
            self.inner
                .vault
                .invalidate(&self.binding.spec.server_id, generation);
            error = error.with("password_generation", generation);
        }
        if status.other_required {
            AppError::new("ssh.two_factor_required")
        } else if status.password_needed {
            AppError::new("ssh.password_needed")
        } else {
            error
        }
    }
    /// 把 password 写入此登记的服务器密码库，返回新代数；供安装专用密钥等已有密码的流程复用。
    pub fn set_password(&self, password: Zeroizing<String>) -> Result<u64, AppError> {
        self.inner.vault.set(&self.binding.spec.server_id, password)
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.inner
            .bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.nonce);
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u8,
    nonce: String,
    prompt: String,
    kind: PromptKind,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u8,
    answer: Option<String>,
}
fn failed(text_key: &'static str, reason: &str) -> AppError {
    AppError::new("ssh.askpass_failed").with_reason(text_key, reason)
}
fn read_line(input: &mut impl Read) -> Result<Zeroizing<Vec<u8>>, AppError> {
    let mut line = Zeroizing::new(Vec::new());
    let deadline = Instant::now() + TIMEOUT;
    while line.len() < MAX_MESSAGE && Instant::now() < deadline {
        let mut byte = [0];
        if input
            .read(&mut byte)
            .map_err(|_| failed("ssh.couldn_t_read_the_message", "读取消息失败"))?
            != 1
        {
            return Err(failed("ssh.incomplete_message", "消息未完成"));
        }
        if byte[0] == b'\n' {
            return Ok(line);
        }
        line.push(byte[0]);
    }
    Err(failed(
        "ssh.message_is_too_long_or_timed_out",
        "消息超长或超时",
    ))
}
fn write_json(output: &mut impl Write, value: &impl Serialize) -> Result<(), AppError> {
    let bytes = Zeroizing::new(
        serde_json::to_vec(value)
            .map_err(|_| failed("ssh.couldn_t_encode_the_message", "编码消息失败"))?,
    );
    if bytes.len() >= MAX_MESSAGE {
        return Err(failed("ssh.message_is_too_long", "消息超长"));
    }
    output
        .write_all(&bytes)
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
        .map_err(|_| failed("ssh.couldn_t_write_the_message", "写入消息失败"))
}
fn serve(mut listener: platform::Listener, inner: Arc<Inner>) {
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while !inner.stopped.load(Ordering::Acquire) {
        workers.retain_mut(|worker| !worker.is_finished());
        match listener.accept(&inner.stopped) {
            Ok(Some(mut stream)) if workers.len() < 32 => {
                let shared = inner.clone();
                if let Ok(worker) =
                    thread::Builder::new()
                        .name("密码请求".into())
                        .spawn(move || {
                            let _ = handle_connection(&mut stream, &shared);
                        })
                {
                    workers.push(worker);
                }
            }
            Ok(Some(_)) => {}
            // 没有请求时低频轮询；askpass 请求对几十毫秒的延迟不敏感
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(_) => {
                // 建下一个管道实例失败等：记下来稍后重试，不能让整个服务停掉（之后所有密码连接都会失败）
                eprintln!("密码管道接受连接失败，稍后重试");
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
    inner.stopped.store(true, Ordering::Release);
    inner.handler.cancel_all();
    let deadline = Instant::now() + Duration::from_secs(1);
    for worker in workers {
        crate::process::join_bounded(worker, deadline.saturating_duration_since(Instant::now()));
    }
}
fn handle_connection(stream: &mut (impl Read + Write), inner: &Arc<Inner>) -> Result<(), AppError> {
    let line = read_line(stream)?;
    let request: Request =
        serde_json::from_slice(&line).map_err(|_| failed("ssh.invalid_protocol", "无效协议"))?;
    let nonce = request.nonce.clone();
    let answer = dispatch(inner, request).filter(|_| {
        !inner.stopped.load(Ordering::Acquire)
            && inner
                .bindings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&nonce)
    });
    let mut response = Response {
        version: VERSION,
        answer: answer.as_ref().map(|a| a.to_string()),
    };
    let result = write_json(stream, &response);
    if let Some(secret) = response.answer.as_mut() {
        use zeroize::Zeroize;
        secret.zeroize();
    }
    result
}
fn dispatch(inner: &Arc<Inner>, request: Request) -> Option<Zeroizing<String>> {
    if request.version != VERSION || inner.stopped.load(Ordering::Acquire) {
        return None;
    }
    let binding = inner
        .bindings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&request.nonce)
        .and_then(Weak::upgrade);
    let Some(binding) = binding else {
        eprintln!("拒绝未知或已注销的密码请求");
        return None;
    };
    let _serial = binding.request.lock().unwrap_or_else(|e| e.into_inner());
    let target = &binding.spec.expected_prompt;
    let password_prompt = request.kind == PromptKind::Password
        && (request.prompt == format!("{target}'s password: ")
            || request.prompt == format!("({target}) Password: "));
    if password_prompt {
        // 同一连接不再次发送密码，防止多轮认证重复使用并已知可能失效的密码。
        if binding
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .used_generation
            .is_some()
        {
            return None;
        }
        let password = inner
            .vault
            .get(&binding.spec.server_id)
            .ok()
            .flatten()
            .or_else(|| {
                if binding.spec.interactive {
                    interactive_password(inner, &binding.spec)
                } else {
                    None
                }
            });
        let mut status = binding.status.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(password) = password {
            status.used_generation = Some(password.generation);
            return Some(password.secret);
        }
        status.password_needed = true;
        return None;
    }
    // 加密私钥的口令提示不是二次验证：后台不弹框，拒绝后 ssh 会跳过这把私钥接着试密码等方式
    let passphrase = request.prompt.starts_with("Enter passphrase for key '");
    if passphrase && !binding.spec.interactive {
        return None;
    }
    if !passphrase
        && binding
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .used_generation
            .is_some()
    {
        inner.vault.mark_two_factor(&binding.spec.server_id);
    }
    if !binding.spec.interactive {
        binding
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .other_required = true;
        return None;
    }
    let answer = inner
        .handler
        .ask_other(&binding.spec.server_id, &request.prompt, request.kind)
        .ok()
        .flatten();
    answer.filter(|answer| answer.len() <= 1024 && !answer.contains(['\0', '\n', '\r']))
}
fn interactive_password(inner: &Arc<Inner>, spec: &RegistrationSpec) -> Option<Password> {
    let (flight, leader) = {
        let mut flights = inner.flights.lock().unwrap_or_else(|e| e.into_inner());
        flights.retain(|_, value| value.strong_count() > 0);
        match flights.get(&spec.server_id).and_then(Weak::upgrade) {
            Some(flight) => (flight, false),
            None => {
                let flight = Arc::new(Flight::default());
                flights.insert(spec.server_id.clone(), Arc::downgrade(&flight));
                (flight, true)
            }
        }
    };
    if leader {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // 其他请求可能已在上锁前完成写入。
            if let Some(password) = inner.vault.get(&spec.server_id).map_err(|_| ())? {
                return Ok(Some(password));
            }
            let answer = inner
                .handler
                .ask_password(&spec.server_id, &spec.purpose)
                .map_err(|_| ())?;
            if inner.stopped.load(Ordering::Acquire) {
                return Ok(None);
            }
            if let Some(answer) = answer {
                inner.vault.set(&spec.server_id, answer).map_err(|_| ())?;
            }
            inner.vault.get(&spec.server_id).map_err(|_| ())
        }))
        .unwrap_or(Err(()));
        *flight.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        flight.done.notify_all();
    }
    let result = flight.result.lock().unwrap_or_else(|e| e.into_inner());
    let (result, _) = flight
        .done
        .wait_timeout_while(result, TIMEOUT, |r| r.is_none())
        .unwrap_or_else(|e| e.into_inner());
    result.as_ref()?.as_ref().ok()?.clone()
}
/// 连接 pipe 并发送 nonce、原始 prompt 与 kind；拒绝、超时或无效协议返回固定错误，绝不输出秘密。
pub fn request_answer(
    pipe: &str,
    nonce: &str,
    prompt: &str,
    kind: PromptKind,
) -> Result<Zeroizing<String>, AppError> {
    let mut stream = platform::connect(pipe)?;
    exchange(&mut stream, nonce, prompt, kind)
}
fn exchange(
    stream: &mut (impl Read + Write),
    nonce: &str,
    prompt: &str,
    kind: PromptKind,
) -> Result<Zeroizing<String>, AppError> {
    write_json(
        stream,
        &Request {
            version: VERSION,
            nonce: nonce.into(),
            prompt: prompt.into(),
            kind,
        },
    )?;
    let line = read_line(stream)?;
    let response: Response =
        serde_json::from_slice(&line).map_err(|_| failed("ssh.invalid_response", "无效响应"))?;
    let answer = Zeroizing::new(
        response
            .answer
            .ok_or_else(|| failed("ssh.request_was_rejected", "请求被拒绝"))?,
    );
    if response.version != VERSION || answer.contains(['\0', '\n', '\r']) {
        return Err(failed("ssh.invalid_response", "无效响应"));
    }
    Ok(answer)
}
/// askpass 模式入口：prompt 为 argv 中的提示，读取 IPC 与 SSH_ASKPASS_PROMPT 环境；成功仅向 stdout 写答案并返回 0，失败返回 1。
/// 专用密钥的 PIERLINK_KEY_PATH 分支仍由调用方先走原来的 askpass::answer。
pub fn client_main(prompt: &str) -> i32 {
    run_client(
        prompt,
        |key| std::env::var(key).ok(),
        &mut std::io::stdout().lock(),
        request_answer,
    )
}
fn run_client(
    prompt: &str,
    env: impl Fn(&str) -> Option<String>,
    stdout: &mut impl Write,
    request: impl Fn(&str, &str, &str, PromptKind) -> Result<Zeroizing<String>, AppError>,
) -> i32 {
    let result = (|| {
        let pipe = env("PIERLINK_ASKPASS_PIPE")
            .ok_or_else(|| failed("ssh.pipe_is_missing", "缺少管道"))?;
        let nonce = env("PIERLINK_ASKPASS_NONCE")
            .ok_or_else(|| failed("ssh.registration_is_missing", "缺少登记"))?;
        let kind = match env("SSH_ASKPASS_PROMPT").as_deref() {
            Some("confirm") => PromptKind::Confirm,
            Some("none") => PromptKind::None,
            _ => PromptKind::Password,
        };
        let answer = request(&pipe, &nonce, prompt, kind)?;
        stdout
            .write_all(answer.as_bytes())
            .and_then(|()| stdout.write_all(b"\n"))
            .and_then(|()| stdout.flush())
            .map_err(|_| failed("ssh.couldn_t_write_the_answer", "输出答案失败"))
    })();
    if result.is_ok() { 0 } else { 1 }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) fn test_registration(expected: &str) -> Arc<Registration> {
    test_registration_mode(expected, true)
}
#[cfg(test)]
pub(crate) fn test_registration_mode(expected: &str, interactive: bool) -> Arc<Registration> {
    struct Cancel;
    impl AskHandler for Cancel {
        fn ask_password(&self, _: &str, _: &str) -> Result<Option<Zeroizing<String>>, AppError> {
            Ok(None)
        }
        fn ask_other(
            &self,
            _: &str,
            _: &str,
            _: PromptKind,
        ) -> Result<Option<Zeroizing<String>>, AppError> {
            Ok(None)
        }
    }
    let inner = Arc::new(Inner {
        endpoint: "test-pipe".into(),
        vault: Arc::new(PasswordVault::default()),
        handler: Arc::new(Cancel),
        bindings: Mutex::new(HashMap::new()),
        flights: Mutex::new(HashMap::new()),
        stopped: Arc::new(AtomicBool::new(false)),
    });
    register(
        &inner,
        RegistrationSpec {
            server_id: "test-server".into(),
            expected_prompt: expected.into(),
            interactive,
            purpose: "测试".into(),
            password_only: true,
        },
    )
    .unwrap()
}

#[cfg(test)]
pub(crate) fn test_answer(
    guard: &Registration,
    prompt: &str,
    kind: PromptKind,
) -> Option<Zeroizing<String>> {
    dispatch(
        &guard.inner,
        Request {
            version: VERSION,
            nonce: guard.nonce().into(),
            prompt: prompt.into(),
            kind,
        },
    )
}
