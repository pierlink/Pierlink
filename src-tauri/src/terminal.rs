//! 终端相关的 Tauri 命令：只做参数转换和转发，逻辑在 `pierlink_core::terminal`。

use crate::pages::PageGenerations;
use base64::Engine;
use pierlink_core::detect::shells::{self, LocalShell};
use pierlink_core::error::AppError;
use pierlink_core::ssh::args::{CommonOpts, Purpose, build_args};
use pierlink_core::ssh::target::SshTarget;
use pierlink_core::terminal::{TerminalEvent, TerminalKind, TerminalManager, TerminalSpec};
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, Webview};

/// 终端相关的应用状态
pub struct TerminalState {
    pub manager: TerminalManager,
    shells: Mutex<Option<Vec<LocalShell>>>,
    /// 会话 ID → 打开它的页面（webview 标签）。页面重载或窗口销毁时据此关闭会话，
    /// 避免旧页面的会话因为没人确认输出而永远暂停、进程一直留着。会话自然退出时条目随之删除。
    owners: Owners,
    /// 会话 ID → 用户终端的元数据（AI 列终端、交给 AI 用）；快速连接不登记
    meta: Mutex<HashMap<String, TerminalMeta>>,
}

/// 一个终端的元数据：标题、目标、本机 shell、界面报来的当前目录
#[derive(Clone, Debug)]
pub struct TerminalMeta {
    pub title: String,
    /// 服务器终端的服务器 ID；本机终端为 None
    pub server_id: Option<String>,
    /// 本机 shell 名称；服务器终端为 None
    pub shell: Option<String>,
    /// 界面从提示符钩子或服务器查到的当前目录
    pub cwd: Option<String>,
    /// AI 自己打开的终端（不算用户终端）
    pub ai_opened: bool,
}

type Owners = Arc<Mutex<HashMap<String, String>>>;

/// 一个会话的归属登记状态。打开命令返回 ID 之前进程就可能已经退出，
/// 所以“登记归属”和“退出时注销”都要在这个槽的锁里进行，避免留下永远不删的条目。
#[derive(Default)]
struct OwnerSlot {
    id: Option<String>,
    exited: bool,
}

impl Default for TerminalState {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalState {
    pub fn new() -> Self {
        Self {
            manager: TerminalManager::new(),
            shells: Mutex::new(None),
            owners: Arc::new(Mutex::new(HashMap::new())),
            meta: Mutex::new(HashMap::new()),
        }
    }

    fn meta(&self) -> MutexGuard<'_, HashMap<String, TerminalMeta>> {
        self.meta.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 登记终端元数据
    fn set_meta(&self, id: &str, meta: TerminalMeta) {
        self.meta().insert(id.to_owned(), meta);
    }

    /// 修改一个终端的元数据；终端没登记时什么也不做
    ///
    /// :param id: 会话 ID
    /// :param f: 修改函数
    /// :return: 终端登记过返回 true
    pub fn update_meta(&self, id: &str, f: impl FnOnce(&mut TerminalMeta)) -> bool {
        match self.meta().get_mut(id) {
            Some(m) => {
                f(m);
                true
            }
            None => false,
        }
    }

    /// 还开着的终端的元数据；顺带删掉已退出终端的条目
    ///
    /// :return: (会话 ID, 元数据) 列表
    pub fn metas(&self) -> Vec<(String, TerminalMeta)> {
        let owners = self.owners();
        let mut meta = self.meta();
        meta.retain(|id, _| owners.contains_key(id));
        meta.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    fn owners(&self) -> MutexGuard<'_, HashMap<String, String>> {
        lock_owners(&self.owners)
    }

    /// 记录会话归属的页面；进程在登记前已经退出时不登记
    fn register(&self, slot: &Mutex<OwnerSlot>, id: &str, webview: &str) {
        let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
        if slot.exited {
            return;
        }
        slot.id = Some(id.to_owned());
        self.owners().insert(id.to_owned(), webview.to_owned());
    }

    /// 打开完成后检查页面是否已在此期间重载：是则立即关闭这个没人接收的会话。
    fn finish_open(
        &self,
        app: &AppHandle,
        slot: &Mutex<OwnerSlot>,
        id: String,
        webview: &str,
        generation: u64,
    ) -> Result<String, AppError> {
        self.register(slot, &id, webview);
        if app.state::<PageGenerations>().current(webview) != generation {
            self.owners().remove(&id);
            let _ = self.manager.close(&id);
            return Err(AppError::new("terminal.page_reloaded"));
        }
        Ok(id)
    }

    /// 关闭某个页面打开的全部会话（页面重载、窗口销毁时调用）。
    ///
    /// :param webview: 页面标签
    /// :return: 关闭的会话数
    pub fn close_owned_by(&self, webview: &str) -> usize {
        // 先取出 ID 再关闭，关闭期间不持有归属表的锁
        let ids: Vec<String> = {
            let mut owners = self.owners();
            let ids: Vec<String> = owners
                .iter()
                .filter(|(_, w)| w.as_str() == webview)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &ids {
                owners.remove(id);
            }
            ids
        };
        let n = ids.len();
        if n > 0 {
            // 结束进程树可能要等内核，放到后台线程，不卡住调用方（页面加载钩子在界面线程上）
            let manager = self.manager.clone();
            std::thread::spawn(move || {
                for id in &ids {
                    // 会话可能已自然退出，找不到属于正常情况
                    let _ = manager.close(id);
                }
            });
        }
        n
    }

    /// 正在运行的终端会话数（退出确认框用）
    pub fn open_count(&self) -> usize {
        self.owners().len()
    }

    /// 本机终端列表（首次调用时检测并缓存）。Bash 的目录钩子包装脚本放在应用数据目录下的 shell-hooks。
    fn shells(&self, app: &AppHandle) -> Vec<LocalShell> {
        let mut guard = self.shells.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .get_or_insert_with(|| {
                let dir = crate::data::data_dir(app)
                    .map(|d| d.join("shell-hooks"))
                    .unwrap_or_else(|_| std::env::temp_dir().join("pierlink-shell-hooks"));
                shells::detect_local_shells(&dir)
            })
            .clone()
    }
}

/// 推给前端的终端事件。输出按 base64 传输，避免多字节字符在分块处被拆坏。
#[derive(Clone, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TerminalEventDto {
    Output {
        data: String,
    },
    #[serde(rename_all = "camelCase")]
    Exited {
        code: Option<i32>,
        closed_by_user: bool,
        /// 远程终端因为密码没登录上：ssh.auth_failed（输的密码不对，已作废）/
        /// ssh.password_needed（没输密码、取消或等太久）/ ssh.two_factor_required；其他退出为空
        auth: Option<String>,
    },
}

/// 远程终端：记下最后一段输出和这次连接的密码登记，退出时判断是不是因为密码没登录上
#[derive(Default)]
struct LoginWatch {
    registration: Mutex<Option<Arc<pierlink_core::ssh::askpass_pipe::Registration>>>,
    /// 这次连接带了密码登记（打开前就知道）；ssh 很快退出时要等登记填好再归类
    expects_registration: std::sync::atomic::AtomicBool,
    tail: Mutex<Vec<u8>>,
}

impl LoginWatch {
    /// 取这次连接的密码登记；带登记的连接如果 ssh 退出得比“打开”返回还快，最多等 1 秒
    fn wait_registration(&self) -> Option<Arc<pierlink_core::ssh::askpass_pipe::Registration>> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(r) = self
                .registration
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
            {
                return Some(r);
            }
            if !self
                .expects_registration
                .load(std::sync::atomic::Ordering::Acquire)
                || Instant::now() >= deadline
            {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn record(&self, bytes: &[u8]) {
        let mut tail = self.tail.lock().unwrap_or_else(|p| p.into_inner());
        tail.extend_from_slice(bytes);
        if tail.len() > 4096 {
            let cut = tail.len() - 4096;
            tail.drain(..cut);
        }
    }

    /// 退出时归类：只在这次连接确实用到密码登记时才给结果
    fn classify(&self, app: &AppHandle, code: Option<i32>) -> Option<String> {
        let registration = self.wait_registration()?;
        let status = registration.status();
        let denied = code == Some(255)
            && String::from_utf8_lossy(&self.tail.lock().unwrap_or_else(|p| p.into_inner()))
                .contains("Permission denied");
        if denied && status.used_generation.is_some() {
            // 用库里的密码登录被拒：密码不对或已改，作废它（下次连接重新问）
            return Some(
                registration
                    .finish_error(AppError::new("ssh.auth_failed"))
                    .code,
            );
        }
        if status.other_required {
            return Some("ssh.two_factor_required".into());
        }
        // 用户取消了密码框，或密码框开着时 ssh 先等不及退出了
        if status.password_needed
            || (denied && crate::passwords::prompt_open(app, registration.server_id()))
        {
            return Some("ssh.password_needed".into());
        }
        None
    }
}

/// 前端显示用的本机终端信息
#[derive(Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct LocalShellInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

fn lock_owners(owners: &Owners) -> MutexGuard<'_, HashMap<String, String>> {
    owners.lock().unwrap_or_else(|p| p.into_inner())
}

/// 把核心的终端事件转发到前端通道；会话退出时注销归属。
fn forward(
    app: AppHandle,
    channel: Channel<TerminalEventDto>,
    owners: Owners,
    slot: Arc<Mutex<OwnerSlot>>,
    watch: Option<Arc<LoginWatch>>,
) -> Arc<dyn Fn(TerminalEvent) + Send + Sync> {
    Arc::new(move |event| {
        if matches!(event, TerminalEvent::Exited { .. }) {
            let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
            slot.exited = true;
            if let Some(id) = slot.id.take() {
                lock_owners(&owners).remove(&id);
            }
        }
        let dto = match event {
            TerminalEvent::Output(bytes) => {
                if let Some(w) = &watch {
                    w.record(&bytes);
                }
                TerminalEventDto::Output {
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                }
            }
            TerminalEvent::Exited {
                code,
                closed_by_user,
            } => TerminalEventDto::Exited {
                code,
                closed_by_user,
                auth: if closed_by_user {
                    None
                } else {
                    watch.as_ref().and_then(|w| w.classify(&app, code))
                },
            },
        };
        // 前端窗口已关闭时发送会失败，忽略即可
        let _ = channel.send(dto);
    })
}

/// 在后台线程执行可能阻塞的终端操作，避免卡住界面线程（Tauri 的同步命令跑在主线程上）。
async fn blocking<T, F>(app: AppHandle, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&AppHandle, &TerminalState) -> Result<T, AppError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<TerminalState>();
        f(&app, &state)
    })
    .await
    .map_err(|e| AppError::new("terminal.task_failed").with("reason", e.to_string()))?
}

/// 查找 ssh 程序：优先随包自带的，其次 Windows 自带的 OpenSSH，最后 PATH。
pub(crate) fn find_ssh(app: &AppHandle) -> Result<PathBuf, AppError> {
    find_openssh_tool(app, "ssh")
}

/// 查找 OpenSSH 的某个程序（ssh、ssh-keygen），顺序同 [`find_ssh`]。
///
/// :param name: 不带扩展名的程序名
pub(crate) fn find_openssh_tool(app: &AppHandle, name: &str) -> Result<PathBuf, AppError> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    let exe = exe.as_str();
    if let Ok(dir) = app.path().resource_dir() {
        let bundled = dir.join("openssh").join(exe);
        if bundled.is_file() {
            return Ok(bundled);
        }
    }
    #[cfg(windows)]
    {
        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let system = system_root.join("System32").join("OpenSSH").join(exe);
        if system.is_file() {
            return Ok(system);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            // 只接受绝对路径，避免 PATH 里的相对目录（如 "."）导致劫持
            if dir.is_absolute() && dir.join(exe).is_file() {
                return Ok(dir.join(exe));
            }
        }
    }
    Err(AppError::new("terminal.ssh_not_found"))
}

/// 列出本机终端，并标出默认项（PowerShell 7 → Windows PowerShell → Git Bash → 其他 → 命令提示符）。
#[tauri::command]
#[specta::specta]
pub async fn list_local_shells(app: AppHandle) -> Result<Vec<LocalShellInfo>, AppError> {
    blocking(app, |app, state| {
        let all = state.shells(app);
        let default_id = shells::pick_default(&all).map(|s| s.id.clone());
        Ok(all
            .into_iter()
            .map(|s| LocalShellInfo {
                is_default: Some(&s.id) == default_id.as_ref(),
                id: s.id,
                name: s.name,
            })
            .collect())
    })
    .await
}

/// 打开本机终端，返回会话 ID。
#[tauri::command]
#[specta::specta]
pub async fn terminal_open_local(
    app: AppHandle,
    webview: Webview,
    shell_id: String,
    cols: u16,
    rows: u16,
    prediction: bool,
    on_event: Channel<TerminalEventDto>,
) -> Result<String, AppError> {
    let owner = webview.label().to_owned();
    let generation = app.state::<PageGenerations>().current(&owner);
    blocking(app, move |app, state| {
        let all = state.shells(app);
        let shell = all
            .iter()
            .find(|s| s.id == shell_id)
            .or_else(|| shells::fallback_default(&all, &shell_id))
            .ok_or_else(|| AppError::new("terminal.shell_not_found").with("shell_id", &shell_id))?;
        let spec = TerminalSpec {
            kind: TerminalKind::Local {
                program: shell.program.clone(),
                args: shell.args.clone(),
                // 和 Windows Terminal 一样从用户主目录开始；不设置时会继承 Pierlink 进程的目录
                //（从开始菜单启动时是安装目录）
                cwd: user_home(),
                // 提示符钩子（报告当前目录给文件面板）需要的环境变量；
                // Git Bash 打开命令预测时再装上提示符标记和命令记录
                env: local_env(shell, prediction),
            },
            cols,
            rows,
        };
        let shell_name = shell.name.clone();
        let slot = Arc::new(Mutex::new(OwnerSlot::default()));
        let id = state.manager.open(
            spec,
            forward(
                app.clone(),
                on_event,
                state.owners.clone(),
                slot.clone(),
                None,
            ),
        )?;
        let id = state.finish_open(app, &slot, id, &owner, generation)?;
        state.set_meta(
            &id,
            TerminalMeta {
                title: shell_name.clone(),
                server_id: None,
                shell: Some(shell_name),
                cwd: None,
                ai_opened: false,
            },
        );
        Ok(id)
    })
    .await
}

/// 快速连接：按 `用户@主机:端口` 打开远程终端（应用内新建的连接，不读用户 ssh config）。
/// 前端应先用 `host_key_check` 在应用内确认指纹；若未确认，ssh 仍会在终端里自行询问。
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)] // Tauri 命令的参数直接对应前端调用，拆成结构体反而不直观
pub async fn terminal_open_quick(
    app: AppHandle,
    webview: Webview,
    host: String,
    port: u16,
    user: Option<String>,
    cols: u16,
    rows: u16,
    prediction: bool,
    on_event: Channel<TerminalEventDto>,
) -> Result<String, AppError> {
    let loc = crate::files::LocDto::Quick {
        host: host.clone(),
        port,
        user: user.clone(),
    };
    let target = SshTarget::Manual {
        host,
        port,
        user,
        identity_file: None,
        jump: None,
    };
    open_remote(
        app,
        webview,
        target,
        CommonOpts::default(),
        None,
        loc,
        cols,
        rows,
        prediction,
        on_event,
    )
    .await
}

/// 按已保存的服务器打开远程终端（使用它的私钥、登录 shell、保存的密码等设置）。
#[tauri::command]
#[specta::specta]
pub async fn terminal_open_server(
    app: AppHandle,
    webview: Webview,
    server_id: String,
    cols: u16,
    rows: u16,
    prediction: bool,
    on_event: Channel<TerminalEventDto>,
) -> Result<String, AppError> {
    let server = crate::servers::load(&app, &server_id)?;
    let target = crate::servers::target(&server)?;
    let opts = crate::keys::ssh_opts(&app, &server, crate::keys::Access::User("终端"))?;
    open_remote(
        app,
        webview,
        target,
        opts,
        server.login_shell.clone(),
        crate::files::LocDto::Server { server_id },
        cols,
        rows,
        prediction,
        on_event,
    )
    .await
}

/// 本机终端的环境变量：Git Bash 打开命令预测时加上 PIERLINK_PREDICT=1（包装脚本据此装上提示符标记）。
fn local_env(shell: &LocalShell, prediction: bool) -> Vec<(String, String)> {
    let mut env = shell.env.clone();
    if prediction && shell.kind == shells::ShellKind::GitBash {
        env.push(("PIERLINK_PREDICT".into(), "1".into()));
    }
    env
}

/// 导入自 ssh config 的服务器有没有配 RemoteCommand：要装命令预测的钩子时先用 ssh -G 查一下，
/// 有的话不装（不能覆盖用户自己的远程命令）；查不出来返回 None，同样不装。
fn alias_remote_command(app: &AppHandle, target: &SshTarget) -> Option<bool> {
    let SshTarget::Alias { alias } = target else {
        return None;
    };
    let program = pierlink_core::process::SshProgram {
        path: find_ssh(app).ok()?,
    };
    pierlink_core::ssh::config_file::resolve_alias(&program, alias, Duration::from_secs(5))
        .map(|cfg| cfg.has_remote_command)
        .map_err(|e| {
            eprintln!("查询 RemoteCommand 失败，不装命令预测的钩子: 别名={alias}, 错误={e:?}")
        })
        .ok()
}

/// 打开远程终端的公共部分
#[allow(clippy::too_many_arguments)]
async fn open_remote(
    app: AppHandle,
    webview: Webview,
    target: SshTarget,
    opts: CommonOpts,
    login_shell: Option<String>,
    loc: crate::files::LocDto,
    cols: u16,
    rows: u16,
    prediction: bool,
    on_event: Channel<TerminalEventDto>,
) -> Result<String, AppError> {
    let owner = webview.label().to_owned();
    let generation = app.state::<PageGenerations>().current(&owner);
    blocking(app, move |app, state| {
        // 给这个终端一个标记，文件面板据此在服务器上找到它的 shell、查当前目录
        let marker = crate::files::new_marker();
        let mut opts = opts;
        if prediction && matches!(target, SshTarget::Alias { .. }) {
            opts.has_remote_command = alias_remote_command(app, &target);
        }
        let purpose = Purpose::Terminal {
            login_shell,
            cwd_marker: Some(marker.clone()),
            integration: prediction,
        };
        let openssh = || -> Result<TerminalKind, AppError> {
            Ok(TerminalKind::Remote {
                ssh: find_ssh(app)?,
                command: build_args(&target, &purpose, &opts)?,
                config_dir: ssh_config_dir(app)?,
            })
        };
        // 保存过的服务器按它的连接方式选内置 SSH 或系统 OpenSSH；临时输入地址的快速连接照旧用系统 OpenSSH
        let kind = match &loc {
            crate::files::LocDto::Server { server_id } => {
                let server = crate::servers::load(app, server_id)?;
                match (
                    app.try_state::<crate::builtin::BuiltinState>(),
                    app.try_state::<crate::passwords::PasswordState>(),
                ) {
                    (Some(builtin), Some(passwords)) => pierlink_core::terminal::remote(
                        &builtin.manager,
                        pierlink_core::builtin::ConnectionRequest {
                            server: &server,
                            class: pierlink_core::builtin::ConnectionClass::Main,
                            vault: passwords.vault.clone(),
                            callbacks: builtin.callbacks.clone(),
                            interactive: true,
                            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        },
                        Some(marker.clone()),
                        prediction,
                        openssh,
                    )?,
                    _ => openssh()?,
                }
            }
            _ => openssh()?,
        };
        // 内置 SSH 在打开时就连接和登录：失败时标上 engine=builtin，界面据此提供“改用系统 OpenSSH 重试”
        let builtin_engine = matches!(kind, TerminalKind::BuiltinRemote(_));
        let spec = TerminalSpec { kind, cols, rows };
        let slot = Arc::new(Mutex::new(OwnerSlot::default()));
        let watch = Arc::new(LoginWatch::default());
        if let TerminalKind::Remote { command, .. } = &spec.kind {
            watch.expects_registration.store(
                command.askpass.is_some(),
                std::sync::atomic::Ordering::Release,
            );
        }
        let (id, registration) = state
            .manager
            .open_with_registration(
                spec,
                forward(
                    app.clone(),
                    on_event,
                    state.owners.clone(),
                    slot.clone(),
                    Some(watch.clone()),
                ),
            )
            .map_err(|e| {
                if builtin_engine {
                    e.with("engine", "builtin")
                } else {
                    e
                }
            })?;
        *watch.registration.lock().unwrap_or_else(|p| p.into_inner()) = registration;
        let id = state.finish_open(app, &slot, id, &owner, generation)?;
        if let crate::files::LocDto::Server { server_id } = &loc {
            let title = crate::servers::load(app, server_id)
                .map(|s| s.name)
                .unwrap_or_else(|_| server_id.clone());
            state.set_meta(
                &id,
                TerminalMeta {
                    title,
                    server_id: Some(server_id.clone()),
                    shell: None,
                    cwd: None,
                    ai_opened: false,
                },
            );
        }
        let owners = state.owners();
        app.state::<crate::files::FilesState>()
            .remember_marker(&id, loc, marker, |x| x == id || owners.contains_key(x));
        Ok(id)
    })
    .await
}

/// 远程终端临时 ssh 配置文件所在目录（应用本地数据目录下的 ssh-configs）。
pub fn ssh_config_dir(app: &AppHandle) -> Result<PathBuf, AppError> {
    Ok(crate::data::data_dir(app)?.join("ssh-configs"))
}

/// 界面报告终端的当前目录（本机来自提示符钩子，服务器来自文件面板的查询），AI 列终端时用
///
/// :param id: 会话 ID
/// :param cwd: 当前目录；不知道时为 null
#[tauri::command]
#[specta::specta]
pub async fn terminal_report_cwd(
    app: AppHandle,
    id: String,
    cwd: Option<String>,
) -> Result<(), AppError> {
    app.state::<TerminalState>()
        .update_meta(&id, |m| m.cwd = cwd);
    Ok(())
}

/// 向终端写入键盘输入（xterm.js 给出的文本）。
#[tauri::command]
#[specta::specta]
pub async fn terminal_write(app: AppHandle, id: String, data: String) -> Result<(), AppError> {
    // 写入只是放进有界队列，不会阻塞，直接调用
    app.state::<TerminalState>()
        .manager
        .write(&id, data.as_bytes())
}

/// 调整终端尺寸。
#[tauri::command]
#[specta::specta]
pub async fn terminal_resize(
    app: AppHandle,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<(), AppError> {
    blocking(app, move |_, state| state.manager.resize(&id, cols, rows)).await
}

/// 确认前端已处理的输出字节数（流量控制）。
#[tauri::command]
#[specta::specta]
pub async fn terminal_ack(app: AppHandle, id: String, bytes: u32) -> Result<(), AppError> {
    // 确认只更新计数并唤醒读取线程，不会阻塞，直接调用
    app.state::<TerminalState>()
        .manager
        .ack(&id, bytes as usize)
}

/// 关闭终端（结束整个进程树）。
#[tauri::command]
#[specta::specta]
pub async fn terminal_close(app: AppHandle, id: String) -> Result<(), AppError> {
    blocking(app, move |_, state| {
        state.owners().remove(&id);
        state.manager.close(&id)
    })
    .await
}

/// 读取系统剪贴板的文字（不经过网页权限，不会弹出“是否允许读取剪贴板”）。没有文字时返回 null。
#[tauri::command]
#[specta::specta]
pub async fn clipboard_read_text() -> Result<Option<String>, AppError> {
    tauri::async_runtime::spawn_blocking(pierlink_core::clipboard::read_text)
        .await
        .map_err(|e| AppError::new("clipboard.read_failed").with("reason", e.to_string()))?
}

/// 把文字写进系统剪贴板（网页方式要求窗口在前台，程序在窗口不在前台时发来的复制请求会失败）
#[tauri::command]
#[specta::specta]
pub async fn clipboard_write_text(text: String) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || pierlink_core::clipboard::write_text(&text))
        .await
        .map_err(|e| AppError::new("clipboard.write_failed").with("reason", e.to_string()))?
}

/// 用系统默认浏览器打开网址（只允许 http 和 https）
#[tauri::command]
#[specta::specta]
pub async fn open_in_browser(url: String) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || pierlink_core::links::open_in_browser(&url))
        .await
        .map_err(|e| AppError::new("link.open_failed").with("reason", e.to_string()))?
}

/// 用户主目录（Windows 为 %USERPROFILE%）；取不到或不是目录时返回 None，沿用进程当前目录
fn user_home() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
}
