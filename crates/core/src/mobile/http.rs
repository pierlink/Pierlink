//! 有界的 HTTP/1 服务；内存测试和实际监听共用同一连接驱动。
use super::*;
use ::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{
    body::{Body, Frame, Incoming},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    io,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{broadcast, mpsc},
    time::Instant,
};
use zeroize::Zeroizing;

const MAX_BODY: usize = 64 * 1024;
pub(super) type HttpResponse = Response<UnsyncBoxBody<Bytes, Infallible>>;

enum Route<'a> {
    Pair,
    Me,
    Overview,
    Terminals,
    Screen(&'a str),
    Input(&'a str),
    Image(&'a str),
    Size(&'a str),
    Activity,
    Confirms,
    Answer(&'a str),
    StopAll,
    Logout,
    Events,
    Static(&'a str),
    Missing,
}
impl<'a> Route<'a> {
    fn new(method: &Method, path: &'a str) -> Self {
        match (method.as_str(), path) {
            ("POST", "/api/pair") => Self::Pair,
            ("GET", "/api/me") => Self::Me,
            ("GET", "/api/overview") => Self::Overview,
            ("GET", "/api/terminals") => Self::Terminals,
            ("GET", "/api/activity") => Self::Activity,
            ("GET", "/api/confirms") => Self::Confirms,
            ("POST", "/api/stop-all") => Self::StopAll,
            ("POST", "/api/logout") => Self::Logout,
            ("GET", "/api/events") => Self::Events,
            _ => {
                let parts: Vec<_> = path.split('/').collect();
                match parts.as_slice() {
                    ["", "api", "terminals", id, "screen"]
                        if method == Method::GET && valid_id(id) =>
                    {
                        Self::Screen(id)
                    }
                    ["", "api", "terminals", id, "input"]
                        if method == Method::POST && valid_id(id) =>
                    {
                        Self::Input(id)
                    }
                    ["", "api", "terminals", id, "image"]
                        if method == Method::POST && valid_id(id) =>
                    {
                        Self::Image(id)
                    }
                    ["", "api", "terminals", id, "size"]
                        if method == Method::POST && valid_id(id) =>
                    {
                        Self::Size(id)
                    }
                    ["", "api", "confirms", id] if method == Method::POST && valid_id(id) => {
                        Self::Answer(id)
                    }
                    _ if (method == Method::GET || method == Method::HEAD)
                        && path != "/api"
                        && !path.starts_with("/api/") =>
                    {
                        Self::Static(path)
                    }
                    _ => Self::Missing,
                }
            }
        }
    }
    fn audit(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Pair => ("pair", None),
            Self::Me => ("me", None),
            Self::Overview => ("overview", None),
            Self::Terminals => ("terminals", None),
            Self::Screen(id) => ("terminal_screen", Some(id)),
            Self::Input(id) => ("terminal_input", Some(id)),
            Self::Image(id) => ("terminal_image", Some(id)),
            Self::Size(id) => ("terminal_size", Some(id)),
            Self::Activity => ("activity", None),
            Self::Confirms => ("confirms", None),
            Self::Answer(id) => ("answer_confirm", Some(id)),
            Self::StopAll => ("stop_all", None),
            Self::Logout => ("logout", None),
            Self::Events => ("events", None),
            Self::Static(_) => ("assets", None),
            Self::Missing => ("unknown", None),
        }
    }
    fn permission(&self, p: Permissions) -> bool {
        match self {
            Self::Input(_) | Self::Image(_) | Self::Size(_) => p.input,
            Self::Answer(_) | Self::StopAll => p.confirm,
            Self::Overview
            | Self::Terminals
            | Self::Screen(_)
            | Self::Activity
            | Self::Confirms
            | Self::Events => p.view,
            _ => true,
        }
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

struct Audit {
    host: Arc<dyn MobileHost>,
    record: AuditRecord,
}
impl Audit {
    fn new(host: Arc<dyn MobileHost>, route: &Route<'_>, peer: IpAddr) -> Self {
        let (action, target) = route.audit();
        Self {
            host,
            record: AuditRecord {
                device_name: String::new(),
                action: action.into(),
                target: target.map(str::to_owned),
                result: "cancelled".into(),
                peer: peer.to_string(),
                bytes: None,
            },
        }
    }
}
impl Drop for Audit {
    fn drop(&mut self) {
        self.host.audit(self.record.clone());
    }
}

impl Shared {
    async fn handle(
        self: Arc<Self>,
        request: Request<Incoming>,
        connection: u64,
        peer: IpAddr,
        activity: Arc<Activity>,
    ) -> HttpResponse {
        let (mut parts, body) = request.into_parts();
        let route = Route::new(&parts.method, parts.uri.path());
        let mut audit = Audit::new(self.host.clone(), &route, peer);
        let mut renewal = None;
        let result = async {
            validate_origin(&parts.headers, &parts.method)?;
            if parts.uri.scheme().is_some() || parts.uri.authority().is_some() {
                return Err(error(
                    "mobile.invalid_path",
                    "mobile.absolute_request_urls_are_not_accepted",
                    "不接受绝对形式的请求地址",
                ));
            }
            let device = if matches!(route, Route::Pair | Route::Static(_)) {
                None
            } else {
                let cookie = single_header(&parts.headers, header::COOKIE.as_str());
                let authenticated = self.authenticate(cookie, connection, peer);
                if authenticated.is_ok() {
                    renewal = cookie
                        .and_then(|cookie| {
                            cookie
                                .split(';')
                                .find_map(|part| part.trim().strip_prefix("pl_device="))
                        })
                        .map(|token| Zeroizing::new(token.to_owned()));
                }
                parts.headers.remove(header::COOKIE);
                let device = match authenticated {
                    Ok(device) => device,
                    Err(e) => {
                        tokio::time::sleep(self.limits.failure_delay).await;
                        return Err(e);
                    }
                };
                audit.record.device_name = device.name.clone();
                if !route.permission(device.permissions) {
                    return Err(error(
                        "mobile.forbidden",
                        "mobile.this_device_is_not_allowed_to_perform_this_operation",
                        "此设备没有执行该操作的权限",
                    ));
                }
                Some(device)
            };
            let body_limit = if matches!(route, Route::Image(_)) {
                if let Some(device) = &device {
                    lock(&self.image_rate).take(&device.id, Instant::now())?;
                }
                image::MAX_IMAGE_BODY
            } else {
                MAX_BODY
            };
            let body = read_body(body, self.limits.idle, body_limit).await?;
            if matches!(route, Route::Image(_)) {
                audit.record.bytes = Some(body.len());
            }
            if let Route::Pair = route {
                let _approval = ApprovalWait::new(activity.clone());
                let pairing: auth::PairRequest = parse(&body)?;
                if !pairing.device_name.trim().is_empty()
                    && pairing.device_name.chars().count() <= 80
                    && !pairing.device_name.chars().any(char::is_control)
                {
                    audit.record.device_name = pairing.device_name.trim().into();
                }
                let user_agent = parts
                    .headers
                    .get(header::USER_AGENT)
                    .map(|value| String::from_utf8_lossy(value.as_bytes()));
                let (device, token) = self.pair(pairing, peer, user_agent.as_deref()).await?;
                audit.record.device_name = device.name.clone();
                let mut response = json_response(
                    StatusCode::OK,
                    json!({"id":device.id,"name":device.name,"permissions":device.permissions}),
                );
                response
                    .headers_mut()
                    .insert(header::SET_COOKIE, device_cookie(&token)?);
                return Ok(response);
            }
            tokio::time::timeout(self.limits.idle, async {
                if let Route::Static(path) = route {
                    let accept_encoding = parts
                        .headers
                        .get_all(header::ACCEPT_ENCODING)
                        .iter()
                        .filter_map(|v| v.to_str().ok())
                        .collect::<Vec<_>>()
                        .join(",");
                    return assets::serve(self.assets.clone(), path, Some(&accept_encoding)).await;
                }
                self.dispatch(
                    &route,
                    parts.uri.query(),
                    single_header(&parts.headers, header::CONTENT_TYPE.as_str()),
                    &body,
                    device.as_ref(),
                    connection,
                )
                .await
            })
            .await
            .map_err(|_| {
                error(
                    "mobile.request_timeout",
                    "mobile.desktop_operation_timed_out",
                    "桌面操作超时",
                )
            })?
        }
        .await;
        let mut response = match result {
            Ok(response) => {
                audit.record.result = if response.status() == StatusCode::CONFLICT {
                    "needs_confirm"
                } else {
                    "ok"
                }
                .into();
                response
            }
            Err(e) => {
                audit.record.result = e.code.clone();
                let mut response = json_response(error_status(&e.code), json!(e));
                // 不继续复用有未读请求体或认证失败的连接。
                response
                    .headers_mut()
                    .insert(header::CONNECTION, HeaderValue::from_static("close"));
                response
            }
        };
        // 活跃设备续期浏览器 Cookie，避免从首次配对开始固定三十天就被浏览器删除。
        if !matches!(route, Route::Logout)
            && let Some(token) = renewal
            && let Ok(cookie) = device_cookie(&token)
        {
            response.headers_mut().insert(header::SET_COOKIE, cookie);
        }
        security_headers(response.headers_mut());
        response
    }

    async fn dispatch(
        self: &Arc<Self>,
        route: &Route<'_>,
        query: Option<&str>,
        content_type: Option<&str>,
        body: &[u8],
        device: Option<&Device>,
        connection: u64,
    ) -> Result<HttpResponse, AppError> {
        let value = match route {
            Route::Me => {
                json!(device.map(|d| json!({"id":d.id,"name":d.name,"permissions":d.permissions})))
            }
            Route::Overview => host(self.host.overview()).await?,
            Route::Terminals => host(self.host.terminals()).await?,
            Route::Screen(id) => host(self.host.terminal_screen(id, screen_lines(query)?)).await?,
            Route::Activity => host(self.host.activity(activity_limit(query)?)).await?,
            Route::Confirms => host(self.host.confirms()).await?,
            Route::Answer(id) => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Answer {
                    answer: String,
                }
                let answer: Answer = parse(body)?;
                let allow = match answer.answer.as_str() {
                    "allow" => true,
                    "deny" => false,
                    _ => {
                        return Err(error(
                            "mobile.invalid_request",
                            "mobile.confirmation_must_be_allow_or_deny",
                            "确认回答只能是 allow 或 deny",
                        ));
                    }
                };
                host(self.host.answer_confirm(id, allow)).await?
            }
            Route::StopAll => host(self.host.stop_all()).await?,
            Route::Input(id) => match self.input(id, parse(body)?).await? {
                input::InputResult::Sent(value) => value,
                input::InputResult::Confirm(lines) => {
                    return Ok(json_response(
                        StatusCode::CONFLICT,
                        input::confirmation(lines),
                    ));
                }
            },
            Route::Image(id) => {
                let ext = image::extension(content_type, body)?;
                self.image(id, body.to_vec(), ext).await?
            }
            Route::Size(id) => {
                host(self.host.terminal_size(id, terminal_size(parse(body)?)?)).await?
            }
            Route::Logout => {
                let device = device.ok_or_else(|| {
                    error(
                        "mobile.unauthorized",
                        "mobile.device_is_not_paired",
                        "设备未配对",
                    )
                })?;
                self.revoke(Some(&device.id), Some(connection))?;
                let mut response = json_response(StatusCode::OK, json!({"ok":true}));
                response.headers_mut().insert(
                    header::SET_COOKIE,
                    HeaderValue::from_static(
                        "pl_device=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
                    ),
                );
                response
                    .headers_mut()
                    .insert(header::CONNECTION, HeaderValue::from_static("close"));
                return Ok(response);
            }
            Route::Events => {
                return Ok(self.events(device.ok_or_else(|| {
                    error(
                        "mobile.unauthorized",
                        "mobile.device_is_not_paired",
                        "设备未配对",
                    )
                })?));
            }
            _ => {
                return Err(error(
                    "mobile.not_found",
                    "mobile.mobile_endpoint_does_not_exist",
                    "手机接口不存在",
                ));
            }
        };
        Ok(json_response(StatusCode::OK, value))
    }

    fn events(self: &Arc<Self>, device: &Device) -> HttpResponse {
        let mut subscription = self.host.subscribe();
        let (sender, receiver) = mpsc::channel(4);
        let shared = self.clone();
        let id = device.id.clone();
        let worker = tokio::spawn(async move {
            if sender
                .send(Bytes::from_static(b": connected\n\n"))
                .await
                .is_err()
            {
                return;
            }
            let mut heartbeat = tokio::time::interval_at(
                Instant::now() + shared.limits.heartbeat,
                shared.limits.heartbeat,
            );
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let frame = tokio::select! {
                    _ = heartbeat.tick() => {
                        if !shared.touch_event_device(&id) { break; }
                        Bytes::from_static(b": heartbeat\n\n")
                    }
                    event = next_event(&mut subscription) => match event {
                        Ok(name) if matches!(name.as_str(), "confirms" | "terminals" | "overview" | "activity") => Bytes::from(format!("event: {name}\ndata: {{}}\n\n")),
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Closed) => { subscription = None; continue; }
                        Err(broadcast::error::RecvError::Lagged(_)) => break,
                    },
                };
                if sender.send(frame).await.is_err() {
                    break;
                }
            }
        });
        let mut response = Response::new(EventBody { receiver, worker }.boxed_unsync());
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
    fn touch_event_device(&self, id: &str) -> bool {
        let state = lock(&self.state);
        let db = lock(&state.db);
        db.mobile_devices()
            .ok()
            .and_then(|devices| devices.into_iter().find(|d| d.device.id == id))
            .is_some_and(|d| {
                d.device.active(now())
                    && d.device.permissions.view
                    && (now() - d.device.last_active_at < auth::TOUCH_INTERVAL_SECONDS
                        || db
                            .touch_mobile_device(id, now(), d.device.last_ip.as_deref())
                            .is_ok())
            })
    }
}

async fn host(future: HostFuture<'_, Value>) -> Result<Value, AppError> {
    future.await.map_err(|_| {
        error(
            "mobile.host_failed",
            "mobile.desktop_operation_failed_check_its_status_on_the_computer",
            "桌面操作失败，请在电脑上查看状态",
        )
    })
}
async fn next_event(
    subscription: &mut Option<broadcast::Receiver<String>>,
) -> Result<String, broadcast::error::RecvError> {
    match subscription {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}
struct EventBody {
    receiver: mpsc::Receiver<Bytes>,
    worker: JoinHandle<()>,
}
impl Body for EventBody {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.receiver
            .poll_recv(cx)
            .map(|v| v.map(|bytes| Ok(Frame::data(bytes))))
    }
}
impl Drop for EventBody {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

/// 手机“按手机宽度显示”的请求：{cols, rows} 改尺寸，{restore: true} 恢复
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SizeRequest {
    cols: Option<u16>,
    rows: Option<u16>,
    #[serde(default)]
    restore: bool,
}

/// 检查改尺寸的请求；返回要改成的列数和行数，恢复时为 None
fn terminal_size(request: SizeRequest) -> Result<Option<(u16, u16)>, AppError> {
    let invalid = || {
        error(
            "mobile.invalid_request",
            "mobile.terminal_size_must_be_20_300_columns_and_5_100_rows",
            "终端尺寸必须是 20 到 300 列、5 到 100 行，或者只给 restore",
        )
    };
    match (request.restore, request.cols, request.rows) {
        (true, None, None) => Ok(None),
        (false, Some(cols), Some(rows))
            if (20..=300).contains(&cols) && (5..=100).contains(&rows) =>
        {
            Ok(Some((cols, rows)))
        }
        _ => Err(invalid()),
    }
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, AppError> {
    serde_json::from_slice(body).map_err(|_| {
        error(
            "mobile.invalid_request",
            "mobile.invalid_request_json_or_fields",
            "请求 JSON 格式或字段不正确",
        )
    })
}
async fn read_body(
    mut body: Incoming,
    timeout: Duration,
    limit: usize,
) -> Result<Zeroizing<Vec<u8>>, AppError> {
    if body
        .size_hint()
        .upper()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(body_too_large(limit));
    }
    tokio::time::timeout(timeout, async move {
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| {
                error(
                    "mobile.invalid_request",
                    "mobile.request_body_was_not_fully_received",
                    "请求体未完整接收",
                )
            })?;
            if let Ok(data) = frame.into_data() {
                if data.len() > limit - bytes.len() {
                    return Err(body_too_large(limit));
                }
                bytes.extend_from_slice(&data);
            }
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| {
        error(
            "mobile.request_timeout",
            "mobile.request_body_reception_timed_out",
            "请求体接收超时",
        )
    })?
}
fn body_too_large(limit: usize) -> AppError {
    if limit == image::MAX_IMAGE_BODY {
        error(
            "mobile.body_too_large",
            "mobile.image_body_exceeds_8_mib",
            "图片请求体超过八 MiB",
        )
    } else {
        error(
            "mobile.body_too_large",
            "mobile.request_body_exceeds_64_kib",
            "请求体超过六十四 KiB",
        )
    }
    .with("limit", limit)
}
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    if headers.get_all(name).iter().count() != 1 {
        return None;
    }
    headers.get(name)?.to_str().ok()
}
fn device_cookie(token: &str) -> Result<HeaderValue, AppError> {
    let cookie = Zeroizing::new(format!(
        "pl_device={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={INACTIVE_SECONDS}"
    ));
    let mut value = HeaderValue::from_str(&cookie).map_err(|_| {
        error(
            "mobile.internal",
            "mobile.couldn_t_create_the_device_cookie",
            "无法生成设备 Cookie",
        )
    })?;
    value.set_sensitive(true);
    Ok(value)
}
fn validate_origin(headers: &HeaderMap, method: &Method) -> Result<(), AppError> {
    let denied = || {
        error(
            "mobile.cross_site",
            "mobile.invalid_request_origin_or_cross_site_request_headers",
            "请求来源或防跨站请求头不合法",
        )
    };
    let host = single_header(headers, "host").ok_or_else(denied)?;
    let authority: ::http::uri::Authority = host.parse().map_err(|_| denied())?;
    if host.contains('@')
        || (authority.port().is_some() && authority.port_u16().is_none())
        || !host_allowed(authority.host())
    {
        return Err(denied());
    }
    if headers.contains_key(header::ORIGIN) {
        let origin: ::http::Uri = single_header(headers, "origin")
            .ok_or_else(denied)?
            .parse()
            .map_err(|_| denied())?;
        let origin_authority = origin.authority().ok_or_else(denied)?;
        let scheme = origin.scheme_str().ok_or_else(denied)?;
        if !matches!(scheme, "http" | "https")
            || origin_authority.as_str().contains('@')
            || (origin_authority.port().is_some() && origin_authority.port_u16().is_none())
            || origin.path() != "/"
            || origin.query().is_some()
            || !origin_authority
                .host()
                .eq_ignore_ascii_case(authority.host())
            || origin_authority
                .port_u16()
                .unwrap_or(if scheme == "https" { 443 } else { 80 })
                != authority
                    .port_u16()
                    .unwrap_or(if scheme == "https" { 443 } else { 80 })
        {
            return Err(denied());
        }
    }
    if method == Method::POST && single_header(headers, "x-pierlink") != Some("1") {
        return Err(denied());
    }
    Ok(())
}
/// Host 只接受 IP 字面量、localhost、不带点的本机名和 Tailscale 的 *.ts.net；
/// 挡住“DNS 重绑定”：别的网站把自己的域名解析到这台电脑时，浏览器带的 Host 是那个域名。
fn host_allowed(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    let lower = host.to_ascii_lowercase();
    lower == "localhost" || !lower.contains('.') || lower.ends_with(".ts.net")
}
fn activity_limit(query: Option<&str>) -> Result<usize, AppError> {
    let Some(query) = query else {
        return Ok(50);
    };
    let invalid = || {
        error(
            "mobile.invalid_request",
            "mobile.record_count_must_be_an_integer_from_1_to_200",
            "记录数量必须为一至二百之间的整数",
        )
    };
    let limit = query
        .strip_prefix("limit=")
        .ok_or_else(invalid)?
        .parse::<usize>()
        .map_err(|_| invalid())?;
    if !(1..=200).contains(&limit) {
        return Err(invalid());
    }
    Ok(limit)
}
fn screen_lines(query: Option<&str>) -> Result<usize, AppError> {
    let Some(query) = query else { return Ok(300) };
    let invalid = || {
        error(
            "mobile.invalid_request",
            "mobile.screen_lines_must_be_an_integer_from_1_to_2000",
            "屏幕行数必须为一至二千之间的整数",
        )
    };
    let lines = query
        .strip_prefix("lines=")
        .ok_or_else(invalid)?
        .parse::<usize>()
        .map_err(|_| invalid())?;
    if !(1..=2000).contains(&lines) {
        return Err(invalid());
    }
    Ok(lines)
}
fn error_status(code: &str) -> StatusCode {
    match code {
        "mobile.unauthorized" | "mobile.invalid_invite" => StatusCode::UNAUTHORIZED,
        "mobile.forbidden"
        | "mobile.cross_site"
        | "mobile.pair_denied"
        | "mobile.password_prompt"
        | "mobile.invalid_path" => StatusCode::FORBIDDEN,
        "mobile.rate_limited" | "mobile.device_limit" | "mobile.input_limit" => {
            StatusCode::TOO_MANY_REQUESTS
        }
        "mobile.pair_timeout" => StatusCode::GATEWAY_TIMEOUT,
        "mobile.request_timeout" => StatusCode::REQUEST_TIMEOUT,
        "mobile.body_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
        "mobile.invalid_request" | "mobile.invalid_image" => StatusCode::BAD_REQUEST,
        "mobile.unsupported_image_type" => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "mobile.not_found" => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
pub(super) fn bytes_response(status: StatusCode, bytes: Vec<u8>) -> HttpResponse {
    let mut response = Response::new(Full::new(Bytes::from(bytes)).boxed_unsync());
    *response.status_mut() = status;
    response
}
fn json_response(status: StatusCode, value: Value) -> HttpResponse {
    let mut response = bytes_response(status, value.to_string().into_bytes());
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
fn security_headers(headers: &mut HeaderMap) {
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; manifest-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
}

pub(super) async fn serve<S>(
    stream: S,
    shared: Arc<Shared>,
    mut connection: Connection,
    peer: IpAddr,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    if *connection.stopped.borrow() {
        return;
    }
    let activity = Arc::new(Activity {
        last: Mutex::new(Instant::now()),
        approval: AtomicBool::new(false),
    });
    let io = ActivityIo {
        stream,
        activity: activity.clone(),
    };
    let observed = activity.clone();
    let limits = shared.limits;
    let id = connection.id;
    let service = service_fn(move |request| {
        let shared = shared.clone();
        let activity = observed.clone();
        async move { Ok::<_, Infallible>(shared.handle(request, id, peer, activity).await) }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header)
        .max_buf_size(16 * 1024);
    tokio::select! {
        biased;
        _ = connection.stopped.changed() => {},
        _ = builder.serve_connection(TokioIo::new(io), service) => {},
        _ = async {
            loop {
                let deadline = if activity.approval.load(Ordering::Acquire) { Instant::now() + limits.idle } else { *lock(&activity.last) + limits.idle };
                tokio::time::sleep_until(deadline).await;
                if !activity.approval.load(Ordering::Acquire) && lock(&activity.last).elapsed() >= limits.idle { break; }
            }
        } => {},
    }
}
struct Activity {
    last: Mutex<Instant>,
    approval: AtomicBool,
}
struct ApprovalWait(Arc<Activity>);
impl ApprovalWait {
    fn new(activity: Arc<Activity>) -> Self {
        activity.approval.store(true, Ordering::Release);
        Self(activity)
    }
}
impl Drop for ApprovalWait {
    fn drop(&mut self) {
        *lock(&self.0.last) = Instant::now();
        self.0.approval.store(false, Ordering::Release);
    }
}
struct ActivityIo<S> {
    stream: S,
    activity: Arc<Activity>,
}
impl<S: AsyncRead + Unpin> AsyncRead for ActivityIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        if buf.filled().len() > before {
            *lock(&self.activity.last) = Instant::now();
        }
        result
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for ActivityIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, bytes);
        if let Poll::Ready(Ok(count)) = result
            && count > 0
        {
            *lock(&self.activity.last) = Instant::now();
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod origin_tests {
    use super::*;

    #[test]
    fn 来源省略端口按协议默认值匹配且主机必须一致() {
        for (host, origin, allowed) in [
            ("pc.ts.net", "https://pc.ts.net", true),
            ("pc.ts.net", "https://pc.ts.net:443", true),
            ("pc.ts.net:443", "https://pc.ts.net", true),
            ("pc.ts.net", "http://pc.ts.net", true),
            ("pc.ts.net", "http://pc.ts.net:80", true),
            ("pc.ts.net:80", "http://pc.ts.net", true),
            ("pc.ts.net:12345", "https://pc.ts.net:12345", true),
            ("[::1]", "https://[::1]", true),
            ("pc.ts.net", "https://other.ts.net", false),
            ("pc.ts.net", "https://pc.ts.net:80", false),
            ("pc.ts.net", "http://pc.ts.net:443", false),
            ("pc.ts.net:12345", "https://pc.ts.net", false),
            ("pc.ts.net:80", "https://pc.ts.net", false),
            ("pc.ts.net", "https://pc.ts.net/other", false),
            ("pc.ts.net", "https://pc.ts.net?x=1", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, HeaderValue::from_str(host).unwrap());
            headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
            headers.insert("x-pierlink", HeaderValue::from_static("1"));
            assert_eq!(
                validate_origin(&headers, &Method::POST).is_ok(),
                allowed,
                "{host} {origin}"
            );
        }
    }
}

#[cfg(test)]
mod size_tests {
    use super::*;

    fn request(json: &str) -> Result<Option<(u16, u16)>, AppError> {
        terminal_size(parse(json.as_bytes())?)
    }

    #[test]
    fn 改尺寸要么给列数和行数要么只给恢复() {
        assert_eq!(request(r#"{"cols":48,"rows":30}"#).unwrap(), Some((48, 30)));
        assert_eq!(request(r#"{"restore":true}"#).unwrap(), None);
        assert_eq!(request(r#"{"cols":20,"rows":5}"#).unwrap(), Some((20, 5)));
        assert_eq!(
            request(r#"{"cols":300,"rows":100}"#).unwrap(),
            Some((300, 100))
        );
        for bad in [
            r#"{"cols":19,"rows":30}"#,
            r#"{"cols":301,"rows":30}"#,
            r#"{"cols":48,"rows":4}"#,
            r#"{"cols":48,"rows":101}"#,
            r#"{"cols":48}"#,
            r#"{"restore":true,"cols":48,"rows":30}"#,
            r#"{"restore":false}"#,
            r#"{}"#,
            r#"{"cols":48,"rows":30,"extra":1}"#,
            r#"{"cols":-1,"rows":30}"#,
        ] {
            assert_eq!(
                request(bad).unwrap_err().code,
                "mobile.invalid_request",
                "{bad}"
            );
        }
    }
}
