//! 服务器入口收到 AI 调用时决定在哪台电脑执行；要转的整次转过去，结果加上电脑名和 ID 前缀。
use super::{
    Current, PeerState, PinRecord, client, decide, discover_script, display, effective_pin,
    parse_record, prefix_ids, short_id, split_id,
};
use crate::{
    error::AppError,
    mcp::{Entry, lock, service::Shared, session::Session, tools},
};
use rmcp::{
    ErrorData,
    model::{CallToolResult, ContentBlock, JsonObject},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, Instant},
};
use tokio::runtime::Handle;

/// 有别的电脑时，一次判断用多久
const FRESH: Duration = Duration::from_secs(10);
/// 没有别的电脑（或查不了）时，一次判断用多久
const FRESH_ALONE: Duration = Duration::from_secs(30);
/// 服务器上读伙伴文件、找名牌的脚本最多等这么久
const SCRIPT_LIMIT: Duration = Duration::from_secs(10);
/// 有别的电脑时，判断过期不到这么久还先用着（后台重新查）；更久了就当场重新查
const STALE_OK: Duration = Duration::from_secs(60);

/// 按入口缓存的判断结果
#[derive(Default)]
pub(in crate::mcp) struct Router {
    views: Mutex<HashMap<String, Cached>>,
    /// 同一个入口同时只查一次
    gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// 正在后台重新查的入口
    refreshing: Mutex<std::collections::HashSet<String>>,
}

struct Cached {
    at: Instant,
    fresh: Duration,
    view: Arc<View>,
}

/// 一个入口这一轮查到的情况
pub(super) struct View {
    /// 别的电脑
    pub peers: Vec<PeerState>,
    pub current: Current,
    /// 生效的指定
    pub pin: Option<PinRecord>,
}

impl Router {
    /// 这个入口现在的判断。过期了：只有这一台电脑（或过期不久）时先用旧的、后台重新查，
    /// AI 的调用不等服务器上的脚本；有别的电脑且过期很久了才当场重新查
    fn view(&self, shared: &Arc<Shared>, server_id: &str, runtime: &Handle) -> Arc<View> {
        let cached = lock(&self.views)
            .get(server_id)
            .map(|c| (c.at.elapsed(), c.fresh, c.view.clone()));
        if let Some((age, fresh, view)) = cached {
            if age < fresh {
                return view;
            }
            let alone = !view.peers.iter().any(|p| p.online) && view.pin.is_none();
            if alone || age < STALE_OK {
                self.refresh_later(shared, server_id, runtime);
                return view;
            }
        }
        self.refresh_now(shared, server_id, runtime)
    }

    /// 在后台重新查一次（同一个入口同时只查一次）
    fn refresh_later(&self, shared: &Arc<Shared>, server_id: &str, runtime: &Handle) {
        if !lock(&self.refreshing).insert(server_id.to_owned()) {
            return;
        }
        let (shared, id, runtime) = (shared.clone(), server_id.to_owned(), runtime.clone());
        std::thread::spawn(move || {
            shared.router.refresh_now(&shared, &id, &runtime);
            lock(&shared.router.refreshing).remove(&id);
        });
    }

    /// 当场重新查（别的调用正在查时等它查完，用它的结果）
    fn refresh_now(&self, shared: &Shared, server_id: &str, runtime: &Handle) -> Arc<View> {
        let gate = lock(&self.gates)
            .entry(server_id.to_owned())
            .or_default()
            .clone();
        let _gate = lock(&gate);
        if let Some(view) = self.fresh(server_id) {
            return view;
        }
        let (view, fresh) = discover(shared, server_id, runtime);
        let view = Arc::new(view);
        lock(&self.views).insert(
            server_id.to_owned(),
            Cached {
                at: Instant::now(),
                fresh,
                view: view.clone(),
            },
        );
        view
    }

    fn fresh(&self, server_id: &str) -> Option<Arc<View>> {
        lock(&self.views)
            .get(server_id)
            .filter(|c| c.at.elapsed() < c.fresh)
            .map(|c| c.view.clone())
    }

    /// 忘掉一个入口的判断（入口停了、转发失败了）：下次调用重新查
    pub(in crate::mcp) fn forget(&self, server_id: &str) {
        lock(&self.views).remove(server_id);
    }

    /// 忘掉所有入口的判断（用户改了指定）
    pub(in crate::mcp) fn clear(&self) {
        lock(&self.views).clear();
    }
}

/// 到服务器上读伙伴文件、找 AI 所在终端的名牌，问一遍别的电脑在不在，判断当前电脑
fn discover(shared: &Shared, server_id: &str, runtime: &Handle) -> (View, Duration) {
    let host = shared.host.as_ref();
    let own = lock(&shared.instance).clone();
    let own_pin = host.pin();
    let mut peers: Vec<PeerState> = Vec::new();
    let mut markers: Vec<String> = Vec::new();
    match host
        .entry_port(server_id)
        .map(|port| host.entry_script(server_id, &discover_script(Some(port)), SCRIPT_LIMIT))
    {
        Some(Ok(output)) => {
            markers = output["markers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .take(16)
                .map(str::to_owned)
                .collect();
            let own_short = short_id(&own);
            for text in output["peers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .take(16)
            {
                let Some(record) = parse_record(text) else {
                    continue;
                };
                let Some(owner) = short_id(&record.instance) else {
                    continue;
                };
                // 自己的文件不算；编号重复的（几乎不可能）只认第一份，免得 ID 前缀认错电脑
                if record.instance == own
                    || own_short.as_deref() == Some(owner.as_str())
                    || peers.iter().any(|p| p.owner == owner)
                {
                    continue;
                }
                peers.push(PeerState {
                    record,
                    owner,
                    online: false,
                    idle_ms: None,
                    local_port: None,
                });
            }
        }
        Some(Err(error)) => {
            eprintln!(
                "读服务器上的伙伴文件失败: 入口={server_id}, 错误={}",
                error.code
            );
        }
        None => {}
    }
    for peer in &mut peers {
        let local = match host.peer_tunnel(server_id, peer.record.port) {
            Ok(local) => local,
            Err(error) => {
                eprintln!(
                    "连另一台电脑的入口失败: 入口={server_id}, 电脑={}, 端口={}, 错误={}",
                    peer.record.computer, peer.record.port, error.code
                );
                continue;
            }
        };
        peer.local_port = Some(local);
        match client::peer_info(runtime, local, &peer.record.token) {
            Ok(info) if info.instance == peer.record.instance => {
                // 问通了才登记成要遮蔽的秘密：服务器上随便写的“令牌”不能用来遮掉操作记录里的字
                shared.add_secret(peer.record.token.clone());
                peer.online = true;
                peer.idle_ms = info.idle_ms;
                if !info.computer.trim().is_empty() {
                    peer.record.computer = display(&info.computer);
                }
            }
            Ok(_) => eprintln!(
                "另一台电脑的入口实例号对不上，当作不在线: 入口={server_id}, 电脑={}, 端口={}",
                peer.record.computer, peer.record.port
            ),
            Err(error) => eprintln!(
                "另一台电脑的入口没有回应: 入口={server_id}, 电脑={}, 端口={}, 错误={}",
                peer.record.computer, peer.record.port, error.code
            ),
        }
    }
    let pin = effective_pin(own_pin.as_ref(), &peers);
    let (current, basis) = decide(&own, host.idle_ms(), &peers, pin.as_ref(), &markers);
    if !peers.is_empty() || pin.is_some() {
        let name = match &current {
            Current::Here => "这台".to_owned(),
            Current::Peer(index) => peers[*index].record.computer.clone(),
            Current::PinnedOffline(name) => format!("{name}（不在线）"),
        };
        eprintln!(
            "判断 AI 操作哪台电脑: 入口={server_id}, 结果={name}, 依据={basis:?}, 别的电脑={}, 在线={}, 名牌={}",
            peers.len(),
            peers.iter().filter(|p| p.online).count(),
            markers.len()
        );
    }
    let fresh = if peers.iter().any(|p| p.online) {
        FRESH
    } else {
        FRESH_ALONE
    };
    (
        View {
            peers,
            current,
            pin,
        },
        fresh,
    )
}

/// 服务器入口收到的调用：要转给别的电脑的转过去并返回给 AI 的结果；照常在本机执行的返回 None
///
/// :param shared: MCP 共享状态
/// :param session: 这次调用的会话
/// :param name: 工具名
/// :param args: 工具参数
/// :param cancelled: 调用被取消的标志
/// :param started: 调用开始的时间
/// :return: 已经处理好的结果；None 表示交给本机照常执行
pub(in crate::mcp) fn route(
    shared: &Arc<Shared>,
    session: &Arc<Session>,
    name: &str,
    args: &JsonObject,
    cancelled: &Arc<AtomicBool>,
    started: Instant,
) -> Option<Result<CallToolResult, ErrorData>> {
    let info = session.snapshot();
    let Entry::Server { server_id } = &info.entry else {
        return None;
    };
    // 入口关着时照常由本机报错；不在 MCP 运行时里（拿不到网络）时不转
    if !matches!(shared.host.permissions(&info.entry), Ok(Some(_))) {
        return None;
    }
    let runtime = Handle::try_current().ok()?;
    // 转走的调用也算这个入口连着的 AI（界面显示已连接）
    session.announce(shared.host.as_ref());
    let call = Call {
        shared,
        session,
        server_id,
        runtime: &runtime,
        name,
        args,
        cancelled,
        started,
    };
    match name {
        "close_terminal" | "terminal_send" | "terminal_read" | "terminal_screen"
        | "terminal_wait" | "job_output" | "job_stop" => {
            let key = if name.starts_with("job_") {
                "job_id"
            } else {
                "terminal_id"
            };
            let (owner, inner) = split_id(args.get(key).and_then(Value::as_str)?)?;
            let mut inner_args = args.clone();
            inner_args.insert(key.into(), Value::String(inner.to_owned()));
            if short_id(&lock(&shared.instance)).as_deref() == Some(owner) {
                return Some(call.local(&inner_args));
            }
            let view = shared.router.view(shared, server_id, &runtime);
            Some(
                match view.peers.iter().find(|p| p.online && p.owner == owner) {
                    Some(peer) => call.send(peer, &inner_args),
                    None => call.fail(
                        AppError::new("mcp.peer_offline")
                            .with_reason(
                                "mcp.the_computer_with_this_id_is_offline",
                                "这个终端或任务所在的电脑现在不在线",
                            )
                            .with("owner", owner),
                    ),
                },
            )
        }
        "list_terminals" | "list_jobs" => {
            let view = shared.router.view(shared, server_id, &runtime);
            if !view.peers.iter().any(|p| p.online) {
                return None;
            }
            Some(call.merge(&view))
        }
        _ => {
            let view = shared.router.view(shared, server_id, &runtime);
            let result = match &view.current {
                Current::Here if name == "list_targets" && !view.peers.is_empty() => {
                    call.local(args)
                }
                Current::Here => return None,
                Current::Peer(index) => call.send(&view.peers[*index], args),
                Current::PinnedOffline(computer) => {
                    return Some(call.fail(pinned_offline(computer)));
                }
            };
            Some(if name == "list_targets" {
                result.map(|r| with_computers(r, shared, &view))
            } else {
                result
            })
        }
    }
}

fn pinned_offline(computer: &str) -> AppError {
    AppError::new("mcp.pinned_computer_offline")
        .with("computer", computer)
        .with_reason(
            "mcp.pinned_computer_offline",
            format!(
                "用户指定 AI 操作电脑 {computer}，它现在不在线：请让用户在 Pierlink 里取消指定，或打开那台电脑的 Pierlink"
            ),
        )
}

/// 一次要处理的调用
struct Call<'a> {
    shared: &'a Arc<Shared>,
    session: &'a Arc<Session>,
    server_id: &'a str,
    runtime: &'a Handle,
    name: &'a str,
    args: &'a JsonObject,
    cancelled: &'a Arc<AtomicBool>,
    started: Instant,
}

impl Call<'_> {
    /// 在本机照常执行（参数可能改过：去掉了自己的 ID 前缀）
    fn local(&self, args: &JsonObject) -> Result<CallToolResult, ErrorData> {
        let mut approval = "";
        let result = tools::execute(
            self.shared,
            self.session,
            self.name,
            args,
            self.cancelled.clone(),
            &mut approval,
        );
        self.shared.complete(
            self.session,
            self.name,
            args,
            (result, approval),
            self.started,
        )
    }

    /// 本机就报错（不执行），照常记录
    fn fail(&self, error: AppError) -> Result<CallToolResult, ErrorData> {
        self.shared.complete(
            self.session,
            self.name,
            self.args,
            (Err(tools::Failure::Tool(error)), ""),
            self.started,
        )
    }

    /// 转给 peer 执行，记录“转给 X”，结果加上电脑名和 ID 前缀
    fn send(&self, peer: &PeerState, args: &JsonObject) -> Result<CallToolResult, ErrorData> {
        let response = self.forward(peer, args);
        self.shared.record_forwarded(
            self.session,
            self.name,
            self.args,
            &response,
            &peer.record.computer,
            self.started,
        );
        response
    }

    /// 转给 peer 执行，返回加好前缀的结果；连不上时返回工具错误（不改到别的电脑执行）
    fn forward(&self, peer: &PeerState, args: &JsonObject) -> Result<CallToolResult, ErrorData> {
        let info = self.session.snapshot();
        let own = lock(&self.shared.instance).clone();
        let call = client::ForwardCall {
            tool: self.name.to_owned(),
            arguments: args.clone(),
            client: info.client,
            computer: tools::local::computer_name().unwrap_or_default(),
            target_key: self.target_key(args),
            merge: false,
        };
        let reply = match peer.local_port {
            Some(port) => client::forward(
                self.runtime,
                port,
                &peer.record.token,
                &own,
                &call,
                self.cancelled,
                client::FORWARD_LIMIT,
            ),
            None => Err(AppError::new("mcp.peer_unreachable")
                .with_reason("mcp.peer_connection_failed", "连不上另一台电脑的 Pierlink")),
        };
        match reply {
            Ok(client::ForwardReply::Result(result)) => Ok(adopt(result, peer)),
            Ok(client::ForwardReply::Error(error)) => Err(error),
            Err(error) => {
                if error.code != "mcp.cancelled" {
                    // 转不过去：下次调用重新查一遍（那台电脑可能刚关了 Pierlink）
                    self.shared.router.forget(self.server_id);
                }
                Ok(self
                    .shared
                    .error_text(error.with("computer", &peer.record.computer)))
            }
        }
    }

    /// 参数 target 在这台电脑上指的服务器的通用标识（对方按它找自己那边的同一台）；
    /// 这台找不到时，如果写的本来就是通用标识就原样给
    fn target_key(&self, args: &JsonObject) -> Option<String> {
        let name = args.get("target")?.as_str()?;
        let entry = self.session.snapshot().entry;
        let found = self
            .shared
            .host
            .permissions(&entry)
            .ok()
            .flatten()
            .and_then(|permissions| self.shared.host.targets(&entry, &permissions).ok())
            .and_then(|targets| tools::find_target(&targets, name).map(|t| t.key.clone()));
        match found {
            Some(key) => key,
            None => name.starts_with("srv-").then(|| name.to_owned()),
        }
    }

    /// 列终端、列后台任务：本机的加上每台在线电脑的（带 ID 前缀和电脑名）
    fn merge(&self, view: &View) -> Result<CallToolResult, ErrorData> {
        let mut approval = "";
        let result = tools::execute(
            self.shared,
            self.session,
            self.name,
            self.args,
            self.cancelled.clone(),
            &mut approval,
        );
        let here = tools::local::computer_name().unwrap_or_default();
        let result = result.map(|mut value| {
            mark_computer(&mut value, &here);
            for peer in view.peers.iter().filter(|p| p.online) {
                let Ok(client::ForwardReply::Result(reply)) = self.forward_raw(peer) else {
                    continue;
                };
                if reply.is_error == Some(true) {
                    continue;
                }
                let Some(mut theirs) = reply.structured_content else {
                    continue;
                };
                prefix_ids(&mut theirs, &peer.owner);
                mark_computer(&mut theirs, &peer.record.computer);
                append(&mut value, theirs);
            }
            value
        });
        self.shared.complete(
            self.session,
            self.name,
            self.args,
            (result, approval),
            self.started,
        )
    }

    /// 转给 peer、原样返回回应（列表合并用，不单独记录）
    fn forward_raw(&self, peer: &PeerState) -> Result<client::ForwardReply, AppError> {
        let port = peer
            .local_port
            .ok_or_else(|| AppError::new("mcp.peer_unreachable"))?;
        let info = self.session.snapshot();
        let call = client::ForwardCall {
            tool: self.name.to_owned(),
            arguments: self.args.clone(),
            client: info.client,
            computer: tools::local::computer_name().unwrap_or_default(),
            target_key: None,
            merge: true,
        };
        let own = lock(&self.shared.instance).clone();
        client::forward(
            self.runtime,
            port,
            &peer.record.token,
            &own,
            &call,
            self.cancelled,
            client::MERGE_LIMIT,
        )
    }
}

/// 收到别的电脑转来的调用：参数 target 在这台电脑上找不到（两台电脑给同一台服务器起的名字、记录 ID 不同）时，
/// 按发起方给的通用标识换成这台电脑上的名字
///
/// :param shared: MCP 共享状态
/// :param session: 转来的调用所在的会话
/// :param args: 工具参数（就地改）
/// :param key: 发起方给的服务器通用标识
pub(in crate::mcp) fn translate_target(
    shared: &Shared,
    session: &Session,
    args: &mut JsonObject,
    key: Option<&str>,
) {
    let (Some(key), Some(name)) = (key, args.get("target").and_then(Value::as_str)) else {
        return;
    };
    let entry = session.snapshot().entry;
    let Ok(Some(permissions)) = shared.host.permissions(&entry) else {
        return;
    };
    let Ok(targets) = shared.host.targets(&entry, &permissions) else {
        return;
    };
    if tools::find_target(&targets, name).is_some() {
        return;
    }
    if let Some(target) = targets.iter().find(|t| t.key.as_deref() == Some(key)) {
        args.insert("target".into(), Value::String(target.target.clone()));
    }
}

/// 别的电脑的结果：ID 加前缀，对象结果加上执行的电脑名，文字版本跟着改
fn adopt(mut result: CallToolResult, peer: &PeerState) -> CallToolResult {
    let failed = result.is_error == Some(true);
    let Some(value) = result.structured_content.as_mut() else {
        return result;
    };
    let before = value.to_string();
    prefix_ids(value, &peer.owner);
    if !failed && let Value::Object(map) = value {
        map.entry("computer")
            .or_insert_with(|| json!(peer.record.computer));
    }
    let after = value.to_string();
    for block in &mut result.content {
        if let ContentBlock::Text(text) = block
            && text.text == before
        {
            text.text = after.clone();
        }
    }
    result
}

/// 列表里每一项（终端、任务）标上所在的电脑
fn mark_computer(value: &mut Value, computer: &str) {
    let items = match value {
        Value::Array(items) => items,
        Value::Object(map) => match map.get_mut("ai_terminals") {
            Some(Value::Array(items)) => items,
            _ => return,
        },
        _ => return,
    };
    for item in items {
        if let Value::Object(item) = item {
            item.entry("computer").or_insert_with(|| json!(computer));
        }
    }
}

/// 把别的电脑的列表接到本机的后面
fn append(value: &mut Value, theirs: Value) {
    match (value, theirs) {
        (Value::Array(mine), Value::Array(theirs)) => mine.extend(theirs),
        (Value::Object(mine), Value::Object(mut theirs)) => {
            if let (Some(Value::Array(mine)), Some(Value::Array(theirs))) =
                (mine.get_mut("ai_terminals"), theirs.remove("ai_terminals"))
            {
                mine.extend(theirs);
            }
        }
        _ => {}
    }
}

/// list_targets 的 Windows 一项加上几台电脑的情况：哪台在用、指定了哪台
fn with_computers(mut result: CallToolResult, shared: &Shared, view: &View) -> CallToolResult {
    if result.is_error == Some(true) {
        return result;
    }
    let Some(value) = result.structured_content.as_mut() else {
        return result;
    };
    let own = lock(&shared.instance).clone();
    let pinned = view.pin.as_ref().and_then(|p| p.target.clone());
    let mut computers = vec![json!({
        "name": tools::local::computer_name().unwrap_or_default(),
        "online": true,
        "current": view.current == Current::Here,
        "pinned": pinned.as_deref() == Some(own.as_str()),
    })];
    for (index, peer) in view.peers.iter().enumerate() {
        computers.push(json!({
            "name": peer.record.computer,
            "online": peer.online,
            "current": view.current == Current::Peer(index),
            "pinned": pinned.as_deref() == Some(peer.record.instance.as_str()),
        }));
    }
    let before = value.to_string();
    if let Value::Array(targets) = value {
        for target in targets.iter_mut() {
            if target["kind"] == "windows"
                && let Value::Object(target) = target
            {
                target.insert("computers".into(), Value::Array(computers.clone()));
            }
        }
    }
    let after = value.to_string();
    for block in &mut result.content {
        if let ContentBlock::Text(text) = block
            && text.text == before
        {
            text.text = after.clone();
        }
    }
    result
}
