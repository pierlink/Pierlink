//! 工具定义、参数校验与阻塞分发。后续具体实现沿用这里的权限和取消边界。
mod copy;
mod encoding;
pub mod files;
pub(super) mod jobs;
pub mod local;
pub(super) mod open;
mod read;
mod schema;
mod search;
pub mod sync;
pub mod terminal;
pub mod terminals;
mod transfer;
use super::{
    Approval, Confirmation, HostEvent, Permissions, RunLimits, Target, TargetKind,
    TerminalOpenRequest, lock, safety,
    service::Shared,
    session::{Session, cancelled_error, no_answer_error},
    text,
};
use crate::error::AppError;
use rmcp::{
    ErrorData,
    model::{JsonObject, Tool},
};
use serde_json::{Value, json};
use ssh_key::encoding::base64::{Base64, Encoding};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
pub(super) use terminals::Registry;

/// 无参数；返回全部工具的说明和对象参数 schema。
pub fn definitions() -> Vec<Tool> {
    schema::TOOLS.clone()
}

pub(super) enum Failure {
    Params(ErrorData),
    Tool(AppError),
}
impl From<AppError> for Failure {
    fn from(error: AppError) -> Self {
        Self::Tool(error)
    }
}
impl From<ErrorData> for Failure {
    fn from(error: ErrorData) -> Self {
        Self::Params(error)
    }
}
fn string<'a>(args: &'a JsonObject, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}
fn required<'a>(args: &'a JsonObject, key: &str) -> &'a str {
    string(args, key).unwrap_or_default()
}
fn number(args: &JsonObject, key: &str, default: u64) -> u64 {
    args.get(key).and_then(Value::as_u64).unwrap_or(default)
}
fn timeout(args: &JsonObject) -> Duration {
    Duration::from_secs(number(args, "timeout_secs", 120))
}
/// 按 list_targets 给的 target 名、id（几台电脑上都一样的服务器标识）或这台电脑的记录 ID 找目标
pub(super) fn find_target<'a>(targets: &'a [Target], name: &str) -> Option<&'a Target> {
    targets.iter().find(|target| {
        target.target == name || target.id == name || target.key.as_deref() == Some(name)
    })
}
fn target<'a>(targets: &'a [Target], name: &str) -> Result<&'a Target, Failure> {
    find_target(targets, name).ok_or_else(|| {
        schema::invalid(
            "mcp.target_does_not_exist",
            "目标不存在：用 list_targets 返回的 target 或 id；几台电脑同时接入时用户换了电脑，请重新调用 list_targets",
        )
        .into()
    })
}
fn check_cancel(cancelled: &AtomicBool) -> Result<(), AppError> {
    if cancelled.load(Ordering::Acquire) {
        Err(cancelled_error())
    } else {
        Ok(())
    }
}
// 审批来源只属于本次调用，不能放在会话上，否则并发操作会互相覆盖审计结果。
struct ActionApproval<'a> {
    shared: &'a Shared,
    session: &'a Session,
    mode: Approval,
    source: &'a mut &'static str,
}
impl ActionApproval<'_> {
    fn confirm(
        &mut self,
        target: &Target,
        action: &str,
        dangerous: bool,
        cancelled: &AtomicBool,
    ) -> Result<(), AppError> {
        check_cancel(cancelled)?;
        // 按这个入口的方式本来就不用问的，审计里批准来源留空
        if self.mode != Approval::AskAll && !dangerous {
            return Ok(());
        }
        if self.session.actions_allowed.load(Ordering::Acquire) {
            *self.source = "session";
        } else if self.mode == Approval::Auto {
            *self.source = "auto";
        } else {
            let decision = self.shared.host.confirm_action(
                &self.session.snapshot(),
                target,
                action,
                dangerous,
                cancelled,
            )?;
            check_cancel(cancelled)?;
            match decision {
                Confirmation::Allow => *self.source = "user",
                Confirmation::AllowAlways => {
                    self.session.actions_allowed.store(true, Ordering::Release);
                    *self.source = "session";
                }
                Confirmation::NoAnswer => return Err(no_answer_error()),
                Confirmation::Deny => {
                    *self.source = "";
                    // action 只进本机的审计记录（发给 agent 的只有 reason）
                    return Err(AppError::new("mcp.action_denied")
                        .with_reason("mcp.the_user_denied_execution", "用户拒绝执行")
                        .with("action", action));
                }
            }
        }
        check_cancel(cancelled)
    }
}

pub(super) fn execute(
    shared: &Arc<Shared>,
    session: &Session,
    name: &str,
    args: &JsonObject,
    cancelled: Arc<AtomicBool>,
    approval_source: &mut &'static str,
) -> Result<Value, Failure> {
    session.announce(shared.host.as_ref());
    check_cancel(&cancelled)?;
    let info = session.snapshot();
    let permissions = shared.host.permissions(&info.entry)?.ok_or_else(|| {
        AppError::new("mcp.entry_disabled").with_reason(
            "mcp.enable_this_entry_in_pierlink_settings_ai_access",
            "Pierlink 没有打开这个入口：在 Pierlink 顶部的 MCP 页里打开",
        )
    })?;
    let targets = shared.host.targets(&info.entry, &permissions)?;
    session.authorize(shared.host.as_ref(), &targets, &cancelled)?;
    check_cancel(&cancelled)?;
    schema::validate(name, args)?;
    if let Some(name) = string(args, "target") {
        target(&targets, name)?;
    }
    let _permit = session.permits.try_acquire().map_err(|_| {
        AppError::new("mcp.busy")
            .with_reason("mcp.too_many_concurrent_operations", "同时进行的操作太多")
    })?;
    let mut approval = ActionApproval {
        shared,
        session,
        mode: permissions.approval,
        source: approval_source,
    };
    match name {
        "list_targets" => {
            let available: Vec<_> = local::shells()
                .into_iter()
                .map(|(name, _)| name)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let mut values = Vec::new();
            for target in &targets {
                let mut value = json!(target);
                // id 给几台电脑上都一样的服务器标识（用户换了电脑也能用）
                if let Some(key) = &target.key {
                    value["id"] = json!(key);
                }
                value["commands"] =
                    json!(target.kind == TargetKind::Server || permissions.windows_commands);
                if target.kind == TargetKind::Windows {
                    let sandbox = files::Sandbox::new(&permissions.folders);
                    value["folders"] = json!(sandbox.folders);
                    value["unavailable_folders"] = json!(sandbox.unavailable_folders);
                    value["home"] = json!(local::home()?);
                    value["shells"] = json!(available);
                    value["path_style"] = json!("windows");
                    value["computer"] = json!(local::computer_name());
                }
                values.push(value);
            }
            Ok(json!(values))
        }
        "job_output" | "job_stop" | "list_jobs" => {
            shared.jobs.maintain(shared.host.as_ref());
            match name {
                "job_output" => {
                    shared
                        .jobs
                        .authorize(&info.entry, required(args, "job_id"), &permissions)?;
                    Ok(shared.jobs.output(
                        &info.entry,
                        required(args, "job_id"),
                        number(args, "since", 0),
                    )?)
                }
                "job_stop" => Ok(shared.jobs.stop(&info.entry, required(args, "job_id"))?),
                _ => {
                    let mut jobs = shared.jobs.list(&info.entry);
                    if let Some(jobs) = jobs.as_array_mut() {
                        jobs.retain(|job| {
                            job["job_id"].as_str().is_some_and(|id| {
                                shared.jobs.authorize(&info.entry, id, &permissions).is_ok()
                            })
                        });
                    }
                    Ok(jobs)
                }
            }
        }
        "run_command" => {
            let target = target(&targets, required(args, "target"))?;
            windows_commands_allowed(&permissions, target)?;
            run_command(shared, target, args, cancelled, &mut approval)
        }
        "open_terminal" => {
            let target = target(&targets, required(args, "target"))?;
            // 本机终端也是执行命令
            windows_commands_allowed(&permissions, target)?;
            let id = shared.host.open_terminal(
                TerminalOpenRequest {
                    session: info.id.clone(),
                    target: target.clone(),
                    cwd: string(args, "cwd").map(str::to_owned),
                    title: string(args, "title").map(str::to_owned),
                },
                &cancelled,
            )?;
            shared.registry.register(
                info.id,
                id.clone(),
                target.clone(),
                &cancelled,
                shared.host.as_ref(),
            )?;
            Ok(json!({"terminal_id":id}))
        }
        "list_terminals" => list_terminals(shared, &terminal_targets(&permissions, &targets)),
        "close_terminal" | "terminal_send" | "terminal_read" | "terminal_screen"
        | "terminal_wait" => terminal_call(
            shared,
            &terminal_targets(&permissions, &targets),
            &permissions.folders,
            name,
            args,
            &cancelled,
            &mut approval,
        ),
        "read_file" | "read_files" | "write_file" | "edit_file" | "make_dir" | "delete_path"
        | "rename_path" | "copy_path" | "search" | "find_files" | "list_dir" => {
            file_call(&permissions, name, args, &cancelled, &mut approval)
        }
        "open_path" => {
            // 打开文件会启动程序：和执行命令一样要单独的“执行命令”权限
            windows_commands_allowed(&permissions, &windows_target())?;
            let opening = open::check(
                &permissions.folders,
                string(args, "path"),
                string(args, "url"),
            )?;
            approval.confirm(
                &windows_target(),
                &open::action(&opening),
                false,
                &cancelled,
            )?;
            check_cancel(&cancelled)?;
            open::open(&opening)?;
            Ok(json!({"opened": match &opening {
                open::Opening::Path(path) => crate::fs_local::display_path(path),
                open::Opening::Url(url) => url.clone(),
            }}))
        }
        "transfer_file" => transfer::run(
            shared,
            &permissions,
            &targets,
            args,
            &cancelled,
            &mut approval,
        ),
        "sync_status" => {
            let mut states = shared.host.sync_status(string(args, "task"))?;
            // 同步状态也遵守服务器授权，不能借状态查询枚举别的服务器。
            for state in &mut states {
                state
                    .servers
                    .retain(|server| permissions.servers.contains(&server.server_id));
            }
            states.retain(|state| !state.servers.is_empty());
            Ok(json!(states))
        }
        "pierlink_status" => {
            let mut value = shared.host.pierlink_status()?;
            if let Value::Object(map) = &mut value {
                map.insert("version".into(), json!(env!("CARGO_PKG_VERSION")));
                map.insert("computer".into(), json!(local::computer_name()));
            }
            Ok(value)
        }
        "sync_wait" => Ok(json!(sync::wait(
            shared.host.as_ref(),
            &permissions,
            string(args, "task"),
            Duration::from_secs(number(args, "timeout_secs", 300)),
            &cancelled,
        )?)),
        _ => Err(schema::invalid("mcp.unknown_tool", "未知工具").into()),
    }
}
/// 终端工具能看到的目标：本机 Windows 的终端等于执行命令，没有“执行命令”权限时看不到也不能操作
/// （包括别的会话打开的 Windows AI 终端）
pub(super) fn terminal_targets(permissions: &Permissions, targets: &[Target]) -> Vec<Target> {
    targets
        .iter()
        .filter(|t| t.kind != TargetKind::Windows || permissions.windows_commands)
        .cloned()
        .collect()
}
fn list_terminals(shared: &Shared, targets: &[Target]) -> Result<Value, Failure> {
    shared.registry.reap(shared.host.as_ref());
    let mut ai = Vec::new();
    for info in shared.registry.list(targets) {
        let Some(access) = shared.registry.get(&info.terminal_id, targets) else {
            continue;
        };
        let cwd = shared.host.terminal_cwd(&info.terminal_id)?;
        // 宿主查询目录期间终端可能被关掉，不能返回已关闭的终端。
        if access.check().is_err() {
            continue;
        }
        let mut value = json!(info);
        if let Some(target) = value["target"].as_object_mut() {
            target.remove("address");
            if let Some(key) = &info.target.key {
                target.insert("id".into(), json!(key));
            }
        }
        value["cwd"] = json!(cwd);
        ai.push(value);
    }
    Ok(json!({"ai_terminals": ai}))
}
/// 运行 f 期间同时盯着 revoked：调用被取消或终端被关掉时，f 看到的取消标志都会立起来
fn with_revocation<T>(
    cancelled: &AtomicBool,
    revoked: &AtomicBool,
    f: impl FnOnce(&AtomicBool) -> T,
) -> T {
    let combined = AtomicBool::new(false);
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            while !done.load(Ordering::Acquire) {
                if cancelled.load(Ordering::Acquire) || revoked.load(Ordering::Acquire) {
                    combined.store(true, Ordering::Release);
                    break;
                }
                // f 结束时会叫醒它，不用等满 50 毫秒
                std::thread::park_timeout(std::time::Duration::from_millis(50));
            }
        });
        let result = f(&combined);
        done.store(true, Ordering::Release);
        watcher.thread().unpark();
        result
    })
}
/// 前台命令运行期间每半秒核对一次权限：入口关闭、或收回了这个目标的执行权限时，
/// 立起交给 f 的停止标志（命令随之结束），并返回 true。用户取消也会立起停止标志。
fn with_permission_watch<T>(
    shared: &Shared,
    entry: &super::Entry,
    target: &Target,
    cancelled: &AtomicBool,
    f: impl FnOnce(&Arc<AtomicBool>) -> T,
) -> (T, bool) {
    let stop = Arc::new(AtomicBool::new(false));
    let revoked = AtomicBool::new(false);
    let done = AtomicBool::new(false);
    let result = std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            let mut next_check = std::time::Instant::now();
            while !done.load(Ordering::Acquire) {
                if cancelled.load(Ordering::Acquire) {
                    stop.store(true, Ordering::Release);
                    break;
                }
                if std::time::Instant::now() >= next_check {
                    next_check = std::time::Instant::now() + Duration::from_millis(500);
                    if !command_allowed(shared.host.as_ref(), entry, target) {
                        revoked.store(true, Ordering::Release);
                        stop.store(true, Ordering::Release);
                        break;
                    }
                }
                // f 结束时会叫醒它
                std::thread::park_timeout(Duration::from_millis(50));
            }
        });
        let result = f(&stop);
        done.store(true, Ordering::Release);
        watcher.thread().unpark();
        result
    });
    (result, revoked.load(Ordering::Acquire))
}
/// 这个入口现在还能不能在 target 上执行命令；读不到设置时按允许处理（不因读取失败中断命令）
fn command_allowed(host: &dyn super::McpHost, entry: &super::Entry, target: &Target) -> bool {
    match host.permissions(entry) {
        Ok(Some(permissions)) => match target.kind {
            TargetKind::Windows => permissions.windows_commands,
            TargetKind::Server => permissions.servers.contains(&target.id),
        },
        Ok(None) => false,
        Err(_) => true,
    }
}
/// Windows 目标可能只因为允许了文件夹才出现：执行命令和开本机终端还要单独的“执行命令”权限
fn windows_commands_allowed(permissions: &Permissions, target: &Target) -> Result<(), AppError> {
    if target.kind == TargetKind::Windows && !permissions.windows_commands {
        return Err(AppError::new("mcp.commands_denied").with_reason(
            "mcp.this_entry_cannot_execute_commands_on_windows",
            "这个入口没有允许在 Windows 上执行命令",
        ));
    }
    Ok(())
}
fn run_command(
    shared: &Arc<Shared>,
    target: &Target,
    args: &JsonObject,
    cancelled: Arc<AtomicBool>,
    approval: &mut ActionApproval<'_>,
) -> Result<Value, Failure> {
    let command = required(args, "command");
    let prepared = if target.kind == TargetKind::Windows {
        Some(local::prepare(
            command,
            string(args, "cwd").map(std::path::Path::new),
            string(args, "shell"),
        )?)
    } else {
        if args.contains_key("shell") {
            return Err(AppError::new("mcp.shell_unavailable")
                .with_reason(
                    "mcp.server_commands_use_bin_sh_the_shell_parameter_is_only",
                    "服务器命令使用 /bin/sh，shell 参数只用于 Windows",
                )
                .into());
        }
        None
    };
    if target.kind == TargetKind::Server
        && args
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return Err(AppError::new("mcp.background_unsupported").with_reason("mcp.server_commands_cannot_guarantee_process_cleanup_on_cancellation_use_an", "服务器执行通道仅返回最终输出，无法保证取消时回收远端进程树；请使用服务器 AI 终端运行长任务").into());
    }
    let action = match string(args, "cwd") {
        Some(cwd) => format!("{command}\n（在目录 {cwd} 里执行）"),
        None => command.into(),
    };
    approval.confirm(target, &action, safety::is_dangerous(command), &cancelled)?;
    check_cancel(&cancelled)?;
    let limits = RunLimits {
        timeout: timeout(args),
        output_bytes: 256 * 1024,
    };
    let cwd = prepared
        .as_ref()
        .map(|p| p.cwd.to_string_lossy().to_string())
        .or_else(|| string(args, "cwd").map(str::to_owned));
    let shell = prepared
        .as_ref()
        .map(|p| p.shell.clone())
        .unwrap_or_else(|| "/bin/sh".into());
    if args
        .get("background")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let prepared = prepared.ok_or_else(|| {
            AppError::new("mcp.background_unsupported").with_reason(
                "mcp.this_target_does_not_support_background_tasks",
                "这个目标不支持后台任务",
            )
        })?;
        let weak = Arc::downgrade(shared);
        let info = approval.session.snapshot();
        let source = *approval.source;
        let entry = info.entry.clone();
        let job_id = shared.jobs.start(
            jobs::Spec {
                entry,
                target: target.clone(),
                cwd: cwd.clone(),
                shell: shell.clone(),
                command: shared.redact_command(command),
            },
            &cancelled,
            // 后台任务（开发服务器等）不受 timeout_secs 限制：只由 job_stop、入口关闭或 Pierlink 退出结束
            move |stop, sink| {
                let limits = RunLimits {
                    timeout: std::time::Duration::MAX,
                    ..limits
                };
                local::run_prepared(&prepared, limits, &stop, Some(sink))
            },
            move |value| {
                if let Some(shared) = weak.upgrade() {
                    shared.job_completed(&info, source, value);
                }
            },
        )?;
        return Ok(
            json!({"job_id":job_id,"cwd":cwd,"shell":shell,"running":true,"output_streaming":target.kind==TargetKind::Windows}),
        );
    }
    let entry = approval.session.snapshot().entry;
    let (output, revoked) =
        with_permission_watch(shared, &entry, target, &cancelled, |stop| match &prepared {
            Some(prepared) => local::run_prepared(prepared, limits, stop, None),
            None => shared.host.run_server_command(
                &target.id,
                command,
                string(args, "cwd"),
                limits,
                stop.clone(),
            ),
        });
    if revoked {
        return Err(AppError::new("mcp.target_denied")
            .with_reason(
                "mcp.command_permission_was_revoked_and_the_command_stopped",
                "执行命令的权限已收回，命令已停止",
            )
            .into());
    }
    let output = output
        .inspect_err(|error| login_needed(shared.host.as_ref(), target, error))
        .map_err(|error| {
            if target.kind == TargetKind::Server
                && (cancelled.load(Ordering::Acquire)
                    || error.code.contains("timeout")
                    || error.code == "mcp.cancelled")
            {
                error.with_text(
                    "note",
                    "mcp.remote_process_may_still_run",
                    "远端进程可能仍在运行，可以打开终端查看",
                )
            } else {
                error
            }
        })?;
    let stopped = cancelled.load(Ordering::Acquire);
    if target.kind != TargetKind::Server {
        check_cancel(&cancelled)?;
    }
    let mut value = json!({"exit_code":output.exit_code,"stdout":text::strip_ansi(&output.stdout),"stderr":text::strip_ansi(&output.stderr),"truncated":output.truncated,"timed_out":output.timed_out,"duration_ms":output.duration_ms,"cwd":cwd,"shell":shell});
    if target.kind == TargetKind::Server && (output.timed_out || stopped) {
        value["cancelled"] = json!(stopped);
        value["note"] = json!("远端进程可能仍在运行，可以打开终端查看");
    }
    Ok(value)
}
fn login_needed(host: &dyn super::McpHost, target: &Target, error: &AppError) {
    if matches!(
        error.code.as_str(),
        "ssh.password_needed" | "ssh.two_factor_required"
    ) {
        host.notify(HostEvent::LoginNeeded {
            server_id: target.id.clone(),
        });
    }
}
#[allow(clippy::too_many_arguments)]
fn terminal_call(
    shared: &Shared,
    targets: &[Target],
    folders: &[std::path::PathBuf],
    name: &str,
    args: &JsonObject,
    cancelled: &AtomicBool,
    approval: &mut ActionApproval<'_>,
) -> Result<Value, Failure> {
    let id = required(args, "terminal_id");
    let registered = shared
        .registry
        .get(id, targets)
        .ok_or_else(|| schema::invalid("mcp.ai_terminal_does_not_exist", "AI 终端不存在"))?;
    registered.check()?;
    let target = targets
        .iter()
        .find(|target| {
            target.id == registered.info.target.id && target.kind == registered.info.target.kind
        })
        .ok_or_else(|| schema::invalid("mcp.target_does_not_exist", "目标不存在"))?;
    let result = match name {
        "close_terminal" => {
            shared.registry.close(id, shared.host.as_ref())?;
            Ok(json!({"closed":true}))
        }
        "terminal_send" => {
            let paste = args.get("paste").and_then(Value::as_bool).unwrap_or(false);
            let image = string(args, "image_path");
            // 图片只发给服务器终端（传到那台服务器）：本机终端暂不支持，注定失败的不问用户
            if image.is_some() && target.kind != TargetKind::Server {
                return Err(AppError::new("mcp.image_unsupported")
                    .with_reason(
                        "mcp.images_only_go_to_server_terminals",
                        "本机终端暂不支持发图片：image_path 只用于服务器终端",
                    )
                    .into());
            }
            let normalized = string(args, "text").map(terminal::normalize_text);
            let text = normalized.as_deref();
            let keys: Vec<String> = args
                .get("keys")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(terminal::normalized_key)
                .collect();
            if (image.is_some() || text.is_some_and(|text| !text.is_empty()))
                && safety::looks_like_password_prompt(&shared.terminals.screen_text(id)?)
            {
                shared.host.notify(HostEvent::PasswordPrompt {
                    terminal_id: id.into(),
                });
                return Err(AppError::new("mcp.password_prompt")
                    .with_reason(
                        "mcp.terminal_is_waiting_for_a_password_enter_it_in_pierlink",
                        "终端正在等待密码，请用户在 Pierlink 里输入",
                    )
                    .into());
            }
            // 程序不接受粘贴、不在全屏程序里却发滚轮：注定发不出去，不问用户，直接告诉 AI（发送时还会再查一次）
            if paste && !shared.terminals.bracketed_paste(id)? {
                return Err(terminal::paste_unsupported().into());
            }
            if keys.iter().any(|key| key.starts_with("Wheel"))
                && !shared.terminals.alternate_screen(id)?
            {
                return Err(terminal::wheel_unavailable().into());
            }
            // 同一个终端的发送排队，待提交行的读、判断、发送、写回一次做完
            let _sending = lock(&registered.send);
            registered.check()?;
            // 文字和回车可能分几次发；用户输入后作废已记下的行，并保留“内容未知”直到提交。
            let generation = shared.terminals.control_generation(id)?;
            // 图片先在本机读出来，确认之后才传到服务器：被拒绝时服务器上什么都不留
            let picture = image.map(|path| read_image(folders, path)).transpose()?;
            // 判断、确认时图片路径先用占位（真的文件名传上去时才定，是随机名），传上去以后换成真的
            let placeholder = picture
                .as_ref()
                .map(|(_, extension)| format!("/tmp/pierlink-paste-….{extension}"));
            let (pending, unknown) = registered.pending_line(generation);
            // 粘贴的图片路径和文字只插进输入行、不提交：之后按回车时，和前面没提交的部分一起算一次提交
            let typed = text.filter(|_| !paste);
            let mut pending = format!("{pending}{}", placeholder.as_deref().unwrap_or_default());
            if let Some(pasted) = text.filter(|_| paste) {
                pending.push_str(&terminal::pasted_line(pasted));
            }
            let newline_submits = terminal::newline_submits(&shared.terminals, id)?;
            let mut simulated =
                terminal::submitted_lines_with(&pending, unknown, typed, &keys, newline_submits);
            let mut lines = std::mem::take(&mut simulated.lines);
            let unknown_submission = simulated.unknown;
            // 等用户确认时终端被收回：确认框立即关掉，按取消处理（不用等用户点了“允许”才被拒）
            with_revocation(cancelled, &registered.revoked, |cancelled| {
                if unknown_submission {
                    let action = if unknown {
                        "（终端里可能有交给 AI 之前或你刚打的、还没回车的文字，AI 按回车，实际执行的内容未知）"
                    } else {
                        "（AI 撤销了修改或粘回了删掉的文字，按回车实际执行的内容未知）"
                    };
                    // 记录未知提交，不把用户输入或过期的自动化行写入审计。
                    lines.insert(0, action.into());
                    approval.confirm(target, &lines.join("\n"), true, cancelled)?;
                }
                // “每次都问”：翻历史后回车提交的命令看不到内容，也要问一次
                if !unknown_submission
                    && lines.is_empty()
                    && approval.mode == Approval::AskAll
                    && terminal::submits_recalled_history(typed, &keys)
                {
                    approval.confirm(target, "（翻历史后回车，内容未知）", true, cancelled)?;
                }
                // “每次都问”：只发图片、不回车也问一次（图片会传到服务器上）
                if let Some(path) = image
                    && approval.mode == Approval::AskAll
                    && lines.is_empty()
                    && !unknown_submission
                {
                    approval.confirm(
                        target,
                        &format!("把图片传到服务器的临时目录，并把路径粘贴进终端：{path}"),
                        false,
                        cancelled,
                    )?;
                }
                if !lines.is_empty() && !unknown_submission {
                    if approval.mode == Approval::Dangerous {
                        for line in &lines {
                            approval.confirm(
                                target,
                                line,
                                safety::is_dangerous(line),
                                cancelled,
                            )?;
                        }
                    } else {
                        approval.confirm(
                            target,
                            &lines.join("\n"),
                            lines.iter().any(|line| safety::is_dangerous(line)),
                            cancelled,
                        )?;
                    }
                }
                Ok::<(), AppError>(())
            }).map_err(|error| {
                registered.check().err().unwrap_or(error)
            })?;
            check_cancel(cancelled)?;
            registered.check()?;
            // 确认过了才传（最多等 30 秒）：传的时候用户在终端里打了字，下面按代数发送时会被拒绝，
            // 不会把路径粘到用户正在打的字里
            let uploaded = picture
                .map(|(bytes, extension)| {
                    upload_image(shared, target, &bytes, extension, cancelled)
                })
                .transpose()?;
            if let (Some(placeholder), Some(real)) = (&placeholder, &uploaded) {
                for line in &mut lines {
                    *line = line.replace(placeholder.as_str(), real);
                }
                simulated.rest = simulated.rest.replace(placeholder.as_str(), real);
            }
            let input = terminal::Input {
                image: uploaded.as_deref(),
                text,
                paste,
                keys: &keys,
            };
            let sent = terminal::send_at_generation(&shared.terminals, id, input, Some(generation))
                .inspect_err(|error| {
                    if error.code == "mcp.password_prompt" {
                        shared.host.notify(HostEvent::PasswordPrompt {
                            terminal_id: id.into(),
                        });
                    }
                })?;
            registered.set_pending_line(generation, simulated.rest, simulated.rest_unknown);
            // 这次按回车提交的整行（文字和回车分开发时，回车那一次也能看到提交了什么）
            let mut value = json!(sent);
            if !lines.is_empty() {
                value["submitted"] = json!(lines);
            }
            if let Some(path) = &uploaded {
                value["image"] = json!(path);
            }
            Ok(value)
        }
        "terminal_read" => Ok(json!(terminal::terminal_read(
            &shared.terminals,
            id,
            args.get("since").and_then(Value::as_u64),
            args.get("tail_bytes").and_then(Value::as_u64)
        )?)),
        "terminal_screen" => Ok(json!({"text": terminal::terminal_screen(&shared.terminals, id)?})),
        "terminal_wait" => {
            let pattern = string(args, "pattern")
                .map(|pattern| regex::RegexBuilder::new(pattern).multi_line(true).build())
                .transpose()
                .map_err(|_| schema::invalid("mcp.invalid_regular_expression", "正则表达式无效"))?;
            Ok(json!(
                terminal::wait_cancellable(
                    &shared.terminals,
                    id,
                    pattern.as_ref(),
                    args.get("quiet_ms")
                        .and_then(Value::as_u64)
                        .map(Duration::from_millis),
                    args.get("since").and_then(Value::as_u64),
                    Duration::from_secs(number(args, "timeout_secs", 60)),
                    || cancelled.load(Ordering::Acquire)
                        || registered.revoked.load(Ordering::Acquire)
                )
                .map_err(|error| registered.check().err().unwrap_or(error))?
            ))
        }
        _ => Err(schema::invalid("mcp.unknown_terminal_tool", "未知终端工具").into()),
    };
    if name != "close_terminal" {
        registered.check()?;
    }
    result
}
/// 把 image_path 的图片传到服务器终端所在的服务器，返回服务器上的路径
/// 读授权文件夹里要发给终端的图片（只读本机，不传）：返回内容和扩展名
fn read_image(
    folders: &[std::path::PathBuf],
    path: &str,
) -> Result<(Vec<u8>, &'static str), AppError> {
    if folders.is_empty() {
        return Err(no_folders());
    }
    files::read_terminal_image(&files::Sandbox::new(folders), path)
}
/// 把图片传到服务器的临时目录，返回服务器上的路径
fn upload_image(
    shared: &Shared,
    target: &Target,
    bytes: &[u8],
    extension: &str,
    cancelled: &AtomicBool,
) -> Result<String, AppError> {
    check_cancel(cancelled)?;
    shared
        .host
        .upload_image(&target.id, bytes, extension)
        .inspect_err(|error| login_needed(shared.host.as_ref(), target, error))
}
/// read_files 一次读的文字太多：剩下的不读
fn read_files_full() -> AppError {
    AppError::new("mcp.file_too_large").with_reason(
        "mcp.read_files_total_limit",
        "这次读的文件加起来太多（文字最多 8 MiB）：这个和后面的请用 read_file 分段读",
    )
}
/// 这个入口没有授权文件夹：文件工具都用不了
fn no_folders() -> AppError {
    AppError::new("mcp.path_denied").with_reason(
        "mcp.this_entry_has_no_allowed_folders",
        "这个入口没有允许访问的文件夹",
    )
}
fn file_call(
    permissions: &Permissions,
    name: &str,
    args: &JsonObject,
    cancelled: &AtomicBool,
    approval: &mut ActionApproval<'_>,
) -> Result<Value, Failure> {
    let path = match name {
        "read_file" | "write_file" | "edit_file" => required(args, "file_path"),
        "rename_path" => required(args, "from"),
        "copy_path" => required(args, "source"),
        _ => required(args, "path"),
    };
    if permissions.folders.is_empty() {
        return Err(no_folders().into());
    }
    let sandbox = files::Sandbox::new(&permissions.folders);
    check_cancel(cancelled)?;
    match name {
        "read_file" => {
            if let Some(image) = files::read_image(&sandbox, path, cancelled)? {
                return Ok(image);
            }
            if args.contains_key("byte_offset") || args.contains_key("max_bytes") {
                Ok(json!(files::read_cancelled(
                    &sandbox,
                    path,
                    number(args, "byte_offset", 0),
                    number(args, "max_bytes", 1024 * 1024),
                    cancelled,
                )?))
            } else {
                Ok(read::lines(
                    &sandbox,
                    path,
                    number(args, "offset", 1),
                    number(args, "limit", 2000),
                    cancelled,
                )?)
            }
        }
        "read_files" => {
            let limit = number(args, "limit", 2000);
            let mut budget = read::IMAGE_BUDGET;
            // 文字部分一次最多这么多（图片另算）：超了的不再读，让 AI 改用 read_file 分段读
            let mut text_left: usize = 8 * 1024 * 1024;
            let mut results = Vec::new();
            for file in args
                .get("file_paths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                check_cancel(cancelled)?;
                // 单个文件出错只记在这一项里，不影响别的；取消则整次调用停下
                let mut result = if text_left == 0 {
                    json!({ "error": read_files_full() })
                } else {
                    match read::listed(&sandbox, file, limit, &mut budget, cancelled) {
                        Ok(value) => value,
                        Err(error) if error.code == "mcp.cancelled" => return Err(error.into()),
                        Err(error) => json!({ "error": error }),
                    }
                };
                let size: usize = result
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(key, _)| *key != "image")
                    .map(|(_, value)| value.to_string().len())
                    .sum();
                if size > text_left {
                    result = json!({ "error": read_files_full() });
                    text_left = 0;
                } else {
                    text_left -= size;
                }
                result["file_path"] = json!(file);
                results.push(result);
            }
            Ok(json!({ "files": results }))
        }
        "search" => Ok(search::search(&sandbox, args, cancelled)?),
        "find_files" => Ok(search::find_files(&sandbox, args, cancelled)?),
        "write_file" | "edit_file" => {
            // content_base64 先解出来：无效的不问用户，确认框里写字节数
            let binary = string(args, "content_base64")
                .map(decode_content)
                .transpose()?;
            let resolved = sandbox.resolve_write(path)?;
            let reason = files::write_needs_confirmation(&resolved)
                .or_else(|| files::write_needs_confirmation(std::path::Path::new(path)));
            approval.confirm(
                &windows_target(),
                &file_action(name, &resolved, reason.as_deref(), args, binary.as_deref()),
                reason.is_some(),
                cancelled,
            )?;
            check_cancel(cancelled)?;
            if let Some(edits) = edits(args) {
                Ok(json!(files::edit_file_many(
                    &sandbox,
                    path,
                    &edits,
                    true,
                    string(args, "expected_sha256")
                )?))
            } else if name == "edit_file" {
                Ok(json!(files::edit_file(
                    &sandbox,
                    path,
                    required(args, "old_string"),
                    required(args, "new_string"),
                    args.get("replace_all")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    string(args, "expected_sha256")
                )?))
            } else if let Some(bytes) = &binary {
                Ok(json!(files::write_bytes(
                    &sandbox,
                    path,
                    bytes,
                    string(args, "expected_sha256")
                )?))
            } else {
                Ok(json!(files::write_checked(
                    &sandbox,
                    path,
                    required(args, "content"),
                    string(args, "expected_sha256")
                )?))
            }
        }
        "make_dir" | "delete_path" | "rename_path" => {
            let resolved = if name == "make_dir" {
                sandbox.resolve_directory(std::path::Path::new(path))?
            } else {
                sandbox.resolve_entry(path)?
            };
            let mut dangerous = files::write_needs_confirmation(&resolved).is_some();
            let recursive = args
                .get("recursive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let overwrite = args
                .get("overwrite")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let action = match name {
                "make_dir" => format!("创建目录（含父目录）：{}", resolved.display()),
                "delete_path" => {
                    let metadata = std::fs::symlink_metadata(&resolved).map_err(|_| {
                        AppError::new("mcp.not_found")
                            .with_reason("mcp.path_missing", "文件或目录不存在")
                    })?;
                    // 注定失败的（授权文件夹本身、非空又没给 recursive）不问用户，直接告诉 AI
                    files::check_delete(&sandbox, &resolved, recursive)?;
                    dangerous |= metadata.is_dir();
                    #[cfg(windows)]
                    {
                        use std::os::windows::fs::FileTypeExt;
                        dangerous |= metadata.file_type().is_symlink_dir();
                    }
                    format!("删除路径：{}\n递归删除：{recursive}", resolved.display())
                }
                _ => {
                    files::check_move(&sandbox, &resolved)?;
                    let to = sandbox.resolve_entry(required(args, "to"))?;
                    dangerous |= files::write_needs_confirmation(&to).is_some();
                    format!(
                        "重命名：{} → {}\n覆盖已有目标：{overwrite}（目录连同内容移动，不递归删除）",
                        resolved.display(),
                        to.display()
                    )
                }
            };
            approval.confirm(&windows_target(), &action, dangerous, cancelled)?;
            check_cancel(cancelled)?;
            let path = match name {
                "make_dir" => files::make_dir(&sandbox, path)?,
                "delete_path" => files::delete_path(&sandbox, path, recursive)?,
                _ => files::rename_path(&sandbox, path, required(args, "to"), overwrite)?,
            };
            Ok(json!({"path":path}))
        }
        "copy_path" => {
            let overwrite = args
                .get("overwrite")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // 先统计和检查（注定失败的不问用户），确认框里写明数量和要确认的目标
            let plan = copy::plan(
                &sandbox,
                path,
                required(args, "destination"),
                overwrite,
                cancelled,
            )?;
            approval.confirm(
                &windows_target(),
                &copy::action(&plan),
                plan.dangerous(),
                cancelled,
            )?;
            check_cancel(cancelled)?;
            Ok(copy::copy(&sandbox, &plan, cancelled)?)
        }
        "list_dir" => Ok(json!(files::list_dir(&sandbox, path)?)),
        _ => Err(schema::invalid("mcp.unknown_file_tool", "未知文件工具").into()),
    }
}

/// 文件工具审批时的目标：本机 Windows
fn windows_target() -> Target {
    Target {
        target: "windows".into(),
        id: "windows".into(),
        name: "Windows".into(),
        kind: TargetKind::Windows,
        online: None,
        engine: None,
        address: None,
        key: None,
    }
}

pub(super) fn summary(name: &str, args: &JsonObject, password_denied: bool) -> String {
    if password_denied {
        return "拒绝".into();
    }
    match name {
        "run_command" => required(args, "command").into(),
        "open_terminal" => format!(
            "{} cwd={}",
            required(args, "target"),
            string(args, "cwd").unwrap_or_default()
        ),
        "terminal_send" => {
            let keys: Vec<&str> = args
                .get("keys")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let paste = if args.get("paste").and_then(Value::as_bool) == Some(true) {
                "粘贴："
            } else {
                ""
            };
            let image = string(args, "image_path")
                .map(|path| format!("图片：{path} "))
                .unwrap_or_default();
            let mut summary = format!(
                "{} {image}{paste}{}",
                required(args, "terminal_id"),
                string(args, "text").unwrap_or_default()
            );
            if !keys.is_empty() {
                summary.push_str(&format!(" [{}]", keys.join(" ")));
            }
            summary
        }
        "read_file" | "write_file" | "edit_file" => required(args, "file_path").into(),
        "read_files" => args
            .get("file_paths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        "search" | "find_files" | "list_dir" | "make_dir" | "delete_path" => {
            required(args, "path").into()
        }
        "rename_path" => format!("{} → {}", required(args, "from"), required(args, "to")),
        "copy_path" => format!(
            "{} → {}",
            required(args, "source"),
            required(args, "destination")
        ),
        "open_path" => string(args, "path")
            .or_else(|| string(args, "url"))
            .unwrap_or_default()
            .into(),
        "transfer_file" => format!(
            "{}:{} → {}:{}",
            required(args, "from_target"),
            required(args, "from_path"),
            required(args, "to_target"),
            required(args, "to_path")
        ),
        "job_output" | "job_stop" => required(args, "job_id").into(),
        "sync_status" | "sync_wait" => string(args, "task").unwrap_or_default().into(),
        _ => string(args, "terminal_id")
            .or_else(|| string(args, "target"))
            .unwrap_or_default()
            .into(),
    }
}
pub(super) fn audit_target(args: &JsonObject) -> Option<String> {
    string(args, "target")
        // 传文件记服务器那一端
        .or_else(|| {
            ["to_target", "from_target"]
                .into_iter()
                .filter_map(|key| string(args, key))
                .find(|target| *target != "windows")
        })
        .map(str::to_owned)
}

fn preview_text(text: &str, max: usize) -> String {
    let count = text.chars().count();
    let mut preview: String = text.chars().take(max).collect();
    if count > max {
        preview.push_str(&format!("…（共 {count} 字）"));
    }
    preview
}
/// 解出 write_file 的 content_base64（标准 base64，可以带换行等空白），解出后最多 5 MiB
fn decode_content(text: &str) -> Result<Vec<u8>, Failure> {
    let compact: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let bytes = Base64::decode_vec(&compact).map_err(|_| {
        schema::invalid(
            "mcp.content_base64_is_not_valid_base64",
            "content_base64 不是有效的 base64（标准字母表，末尾用 = 补齐）",
        )
    })?;
    if bytes.len() > 5 * 1024 * 1024 {
        return Err(schema::invalid("mcp.file_content_exceeds_5_mib", "文件内容超过 5 MiB").into());
    }
    Ok(bytes)
}
/// edit_file 的 edits 参数（多处修改）；没给时为 None。类型已由 schema 校验过
fn edits(args: &JsonObject) -> Option<Vec<files::Edit<'_>>> {
    let items = args.get("edits")?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(Value::as_object)
            .map(|item| files::Edit {
                old: required(item, "old_string"),
                new: required(item, "new_string"),
                all: item
                    .get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
            .collect(),
    )
}
/// 确认框里最多列出几处修改
const EDITS_SHOWN: usize = 5;
fn file_action(
    name: &str,
    path: &std::path::Path,
    reason: Option<&str>,
    args: &JsonObject,
    binary: Option<&[u8]>,
) -> String {
    let heading = format!("{}：{}", reason.unwrap_or("写入文件"), path.display());
    if let Some(bytes) = binary {
        format!(
            "{heading}\n内容：{} 字节（content_base64，原样写入）",
            bytes.len()
        )
    } else if let Some(edits) = edits(args) {
        let mut text = format!("{heading}\n共 {} 处修改", edits.len());
        for (index, edit) in edits.iter().take(EDITS_SHOWN).enumerate() {
            text.push_str(&format!(
                "\n第 {n} 处旧文字：\n{}\n第 {n} 处新文字：\n{}",
                preview_text(edit.old, 300),
                preview_text(edit.new, 300),
                n = index + 1
            ));
        }
        if edits.len() > EDITS_SHOWN {
            text.push_str(&format!("\n…（还有 {} 处）", edits.len() - EDITS_SHOWN));
        }
        text
    } else if name == "edit_file" {
        format!(
            "{heading}\n旧文字：\n{}\n新文字：\n{}",
            preview_text(required(args, "old_string"), 300),
            preview_text(required(args, "new_string"), 300)
        )
    } else {
        let content = required(args, "content");
        let preview = content
            .lines()
            .take(6)
            .map(|line| preview_text(line, 300))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "{heading}\n内容：{} 字节，前 6 行：\n{preview}",
            content.len()
        )
    }
}
