//! 工具定义、参数校验与阻塞分发。后续具体实现沿用这里的权限和取消边界。
mod encoding;
pub mod files;
pub(super) mod jobs;
pub mod local;
mod read;
mod schema;
mod search;
pub mod sync;
pub mod terminal;
pub mod terminals;
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
fn target<'a>(targets: &'a [Target], name: &str) -> Result<&'a Target, Failure> {
    targets
        .iter()
        .find(|target| target.target == name || target.id == name)
        .ok_or_else(|| schema::invalid("mcp.target_does_not_exist", "目标不存在").into())
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
                value["commands"] =
                    json!(target.kind == TargetKind::Server || permissions.windows_commands);
                if target.kind == TargetKind::Windows {
                    let sandbox = files::Sandbox::new(&permissions.folders);
                    value["folders"] = json!(sandbox.folders);
                    value["unavailable_folders"] = json!(sandbox.unavailable_folders);
                    value["home"] = json!(local::home()?);
                    value["shells"] = json!(available);
                    value["path_style"] = json!("windows");
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
        "list_terminals" => list_terminals(
            shared,
            &info.entry,
            &terminal_targets(&permissions, &targets),
        ),
        "close_terminal" | "terminal_send" | "terminal_read" | "terminal_screen"
        | "terminal_wait" => terminal_call(
            shared,
            &terminal_targets(&permissions, &targets),
            name,
            args,
            &cancelled,
            &mut approval,
        ),
        "read_file" | "write_file" | "edit_file" | "search" | "find_files" | "list_dir" => {
            file_call(&permissions, name, args, &cancelled, &mut approval)
        }
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
fn list_terminals(
    shared: &Shared,
    entry: &super::Entry,
    targets: &[Target],
) -> Result<Value, Failure> {
    shared.registry.reap(shared.host.as_ref());
    let registered = shared.registry.list(entry, targets);
    let mut users = shared.host.user_terminals()?;
    users.retain(|terminal| {
        !terminal.target_id.is_empty()
            && targets
                .iter()
                .any(|target| target.id == terminal.target_id && target.kind == terminal.kind)
    });
    for user in &mut users {
        user.handed_over = registered.iter().any(|terminal| {
            terminal.terminal_id == user.terminal_id
                && matches!(
                    terminal.source,
                    terminals::TerminalSource::HandedOver { .. }
                )
        });
        if user.kind == TargetKind::Server {
            user.busy = None;
            user.shell = None;
        }
    }
    let mut ai = Vec::new();
    for info in registered {
        let Some(access) = shared.registry.get(&info.terminal_id, entry, targets) else {
            continue;
        };
        let cwd = shared.host.terminal_cwd(&info.terminal_id)?;
        // 宿主查询目录期间可能收回终端，不能返回已撤销的内容。
        if access.check().is_err() {
            continue;
        }
        let mut value = json!(info);
        if let Some(target) = value["target"].as_object_mut() {
            target.remove("address");
        }
        value["cwd"] = json!(cwd);
        ai.push(value);
    }
    Ok(json!({"ai_terminals": ai, "user_terminals": users}))
}
/// 运行 f 期间同时盯着 revoked：调用被取消或终端被收回时，f 看到的取消标志都会立起来
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
                command: command.into(),
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
    let output = match prepared {
        Some(prepared) => local::run_prepared(&prepared, limits, &cancelled, None),
        None => shared.host.run_server_command(
            &target.id,
            command,
            string(args, "cwd"),
            limits,
            cancelled.clone(),
        ),
    }
    .inspect_err(|error| login_needed(shared.host.as_ref(), target, error))?;
    check_cancel(&cancelled)?;
    Ok(
        json!({"exit_code":output.exit_code,"stdout":text::strip_ansi(&output.stdout),"stderr":text::strip_ansi(&output.stderr),"truncated":output.truncated,"timed_out":output.timed_out,"duration_ms":output.duration_ms,"cwd":cwd,"shell":shell}),
    )
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
fn terminal_call(
    shared: &Shared,
    targets: &[Target],
    name: &str,
    args: &JsonObject,
    cancelled: &AtomicBool,
    approval: &mut ActionApproval<'_>,
) -> Result<Value, Failure> {
    let id = required(args, "terminal_id");
    let registered = shared
        .registry
        .get(id, &approval.session.snapshot().entry, targets)
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
            let normalized = string(args, "text").map(terminal::normalize_text);
            let text = normalized.as_deref();
            let keys: Vec<String> = args
                .get("keys")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            if text.is_some_and(|text| !text.is_empty())
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
            // 同一个终端的发送排队，待提交行的读、判断、发送、写回一次做完
            let _sending = lock(&registered.send);
            registered.check()?;
            // 文字和回车可能分几次发；用户输入后作废已记下的行，并保留“内容未知”直到提交。
            let generation = shared.terminals.control_generation(id)?;
            let (pending, unknown) = registered.pending_line(generation);
            let (mut lines, rest) = terminal::submitted_lines(&pending, text, &keys);
            let submits = text.is_some_and(|text| text.contains(['\r', '\n']))
                || keys
                    .iter()
                    .any(|key| matches!(key.as_str(), "Enter" | "Ctrl+M" | "Ctrl+J"));
            let unknown_submission = unknown && submits;
            // 等用户确认时终端被收回：确认框立即关掉，按取消处理（不用等用户点了“允许”才被拒）
            with_revocation(cancelled, &registered.revoked, |cancelled| {
                if unknown_submission {
                    let action = "（终端里可能有交给 AI 之前或你刚打的、还没回车的文字，AI 按回车，实际执行的内容未知）";
                    // 记录未知提交，不把用户输入或过期的自动化行写入审计。
                    lines.insert(0, action.into());
                    approval.confirm(target, &lines.join("\n"), true, cancelled)?;
                }
                // “每次都问”：翻历史后回车提交的命令看不到内容，也要问一次
                if !unknown_submission
                    && lines.is_empty()
                    && approval.mode == Approval::AskAll
                    && terminal::submits_recalled_history(text, &keys)
                {
                    approval.confirm(target, "（翻历史后回车，内容未知）", true, cancelled)?;
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
            let sent =
                terminal::send_at_generation(&shared.terminals, id, text, &keys, Some(generation))
                    .inspect_err(|error| {
                        if error.code == "mcp.password_prompt" {
                            shared.host.notify(HostEvent::PasswordPrompt {
                                terminal_id: id.into(),
                            });
                        }
                    })?;
            registered.set_pending_line(generation, rest, unknown && !submits);
            // 这次按回车提交的整行（文字和回车分开发时，回车那一次也能看到提交了什么）
            let mut value = json!(sent);
            if !lines.is_empty() {
                value["submitted"] = json!(lines);
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
fn file_call(
    permissions: &Permissions,
    name: &str,
    args: &JsonObject,
    cancelled: &AtomicBool,
    approval: &mut ActionApproval<'_>,
) -> Result<Value, Failure> {
    let path = if matches!(name, "read_file" | "write_file" | "edit_file") {
        required(args, "file_path")
    } else {
        required(args, "path")
    };
    if permissions.folders.is_empty() {
        return Err(AppError::new("mcp.path_denied")
            .with_reason(
                "mcp.this_entry_has_no_allowed_folders",
                "这个入口没有允许访问的文件夹",
            )
            .into());
    }
    let sandbox = files::Sandbox::new(&permissions.folders);
    check_cancel(cancelled)?;
    match name {
        "read_file" => {
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
        "search" => Ok(search::search(&sandbox, args, cancelled)?),
        "find_files" => Ok(search::find_files(&sandbox, args, cancelled)?),
        "write_file" | "edit_file" => {
            let resolved = sandbox.resolve_write(path)?;
            let reason = files::write_needs_confirmation(&resolved)
                .or_else(|| files::write_needs_confirmation(std::path::Path::new(path)));
            let windows = Target {
                target: "windows".into(),
                id: "windows".into(),
                name: "Windows".into(),
                kind: TargetKind::Windows,
                online: None,
                engine: None,
                address: None,
            };
            approval.confirm(
                &windows,
                &format!(
                    "{}：{}",
                    reason.as_deref().unwrap_or("写入文件"),
                    resolved.display()
                ),
                reason.is_some(),
                cancelled,
            )?;
            check_cancel(cancelled)?;
            if name == "edit_file" {
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
            } else {
                Ok(json!(files::write_checked(
                    &sandbox,
                    path,
                    required(args, "content"),
                    string(args, "expected_sha256")
                )?))
            }
        }
        "list_dir" => Ok(json!(files::list_dir(&sandbox, path)?)),
        _ => Err(schema::invalid("mcp.unknown_file_tool", "未知文件工具").into()),
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
            let mut summary = format!(
                "{} {}",
                required(args, "terminal_id"),
                string(args, "text").unwrap_or_default()
            );
            if !keys.is_empty() {
                summary.push_str(&format!(" [{}]", keys.join(" ")));
            }
            summary
        }
        "read_file" | "write_file" | "edit_file" => required(args, "file_path").into(),
        "search" | "find_files" | "list_dir" => required(args, "path").into(),
        "job_output" | "job_stop" => required(args, "job_id").into(),
        "sync_status" | "sync_wait" => string(args, "task").unwrap_or_default().into(),
        _ => string(args, "terminal_id")
            .or_else(|| string(args, "target"))
            .unwrap_or_default()
            .into(),
    }
}
pub(super) fn audit_target(args: &JsonObject) -> Option<String> {
    string(args, "target").map(str::to_owned)
}
