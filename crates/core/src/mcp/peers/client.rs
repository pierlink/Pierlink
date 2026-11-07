//! 经本机转发端口访问另一台电脑的入口（伙伴接口、转发接口）。在阻塞线程上调用，借 MCP 运行时收发。
use super::{FORWARD_PATH, FORWARDED_HEADER, PEER_PATH};
use crate::{
    error::AppError,
    mcp::{ClientInfo, session::cancelled_error},
};
use bytes::Bytes;
use http::{Request, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use rmcp::{ErrorData, model::CallToolResult};
use serde::{Deserialize, Serialize};
use std::{
    net::Ipv4Addr,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::runtime::Handle;

/// 转发回应最多这么多字节
const MAX_REPLY: usize = 8 * 1024 * 1024;
/// 伙伴接口的回应最多这么多字节
const MAX_INFO: usize = 64 * 1024;
/// 转发途中这么久收不到任何字节就算断了（对方每 10 秒发一次心跳）
const REPLY_IDLE: Duration = Duration::from_secs(45);
/// 一次转发最长等这么久（最长的命令 30 分钟，加上确认和打开终端的时间）
pub(super) const FORWARD_LIMIT: Duration = Duration::from_secs(60 * 60);
/// 合并列表时每台最多等这么久（只是顺带列别的电脑的，不能拖慢本机的结果）
pub(super) const MERGE_LIMIT: Duration = Duration::from_secs(10);
/// 问伙伴接口最多等这么久
const INFO_LIMIT: Duration = Duration::from_secs(4);

/// 发给另一台电脑的一次工具调用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::mcp) struct ForwardCall {
    pub tool: String,
    #[serde(default)]
    pub arguments: rmcp::model::JsonObject,
    /// AI 客户端自报的名称和版本（只用于显示）
    #[serde(default)]
    pub client: ClientInfo,
    /// 发起那台电脑的名字（对方的操作记录里显示“由 X 转来”）
    #[serde(default)]
    pub computer: String,
    /// 参数 target 指的服务器在几台电脑上都一样的标识：对方按名字、ID 找不到时按它找自己那边的同一台
    #[serde(default)]
    pub target_key: Option<String>,
    /// 只是为了合并列表（列终端、列后台任务）：对方还没确认过这个连接时不弹确认框、直接跳过
    #[serde(default)]
    pub merge: bool,
}

/// 另一台电脑做完后的回应：工具结果，或参数错误
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::mcp) enum ForwardReply {
    Result(CallToolResult),
    Error(ErrorData),
}

/// 伙伴接口的回应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::mcp) struct PeerInfo {
    pub instance: String,
    #[serde(default)]
    pub computer: String,
    /// 多久没有键盘鼠标输入；不知道为 None
    #[serde(default)]
    pub idle_ms: Option<u64>,
}

/// 问另一台电脑的伙伴接口：在不在、是哪个实例、多久没操作
///
/// :param runtime: MCP 运行时
/// :param port: 本机到那台电脑入口的转发端口
/// :param token: 那台电脑入口的令牌
/// :return: 伙伴信息；连不上、令牌不对等返回错误
pub(super) fn peer_info(runtime: &Handle, port: u16, token: &str) -> Result<PeerInfo, AppError> {
    let request = Request::get(PEER_PATH)
        .header(header::HOST, format!("127.0.0.1:{port}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Full::new(Bytes::new()))
        .map_err(|_| failed("mcp.peer_request_invalid", "伙伴请求不合规"))?;
    let never = AtomicBool::new(false);
    let body = block_on(
        runtime,
        &never,
        INFO_LIMIT,
        send(port, request, MAX_INFO, INFO_LIMIT),
    )?;
    serde_json::from_slice(&body).map_err(|_| {
        failed(
            "mcp.peer_reply_invalid",
            "另一台电脑的 Pierlink 回应格式不对",
        )
    })
}

/// 把一次工具调用转给另一台电脑，等它做完
///
/// :param runtime: MCP 运行时
/// :param port: 本机到那台电脑入口的转发端口
/// :param token: 那台电脑入口的令牌
/// :param origin: 这台电脑的实例号
/// :param call: 工具调用
/// :param cancelled: 变为 true 时断开（对方随之取消这次调用）
/// :param limit: 最长等多久
/// :return: 对方的回应
pub(super) fn forward(
    runtime: &Handle,
    port: u16,
    token: &str,
    origin: &str,
    call: &ForwardCall,
    cancelled: &AtomicBool,
    limit: Duration,
) -> Result<ForwardReply, AppError> {
    let body = serde_json::to_vec(call)
        .map_err(|_| failed("mcp.peer_request_invalid", "伙伴请求不合规"))?;
    let request = Request::post(FORWARD_PATH)
        .header(header::HOST, format!("127.0.0.1:{port}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(FORWARDED_HEADER, origin)
        .body(Full::new(Bytes::from(body)))
        .map_err(|_| failed("mcp.peer_request_invalid", "伙伴请求不合规"))?;
    let body = block_on(
        runtime,
        cancelled,
        limit,
        send(port, request, MAX_REPLY, REPLY_IDLE.min(limit)),
    )?;
    serde_json::from_slice(&body).map_err(|_| {
        failed(
            "mcp.peer_reply_invalid",
            "另一台电脑的 Pierlink 回应格式不对",
        )
    })
}

/// 在阻塞线程上跑 future：cancelled 变为 true 或超过 limit 时放弃（连接随 future 一起断开）
fn block_on<T>(
    runtime: &Handle,
    cancelled: &AtomicBool,
    limit: Duration,
    future: impl Future<Output = Result<T, AppError>>,
) -> Result<T, AppError> {
    runtime.block_on(async {
        tokio::select! {
            result = tokio::time::timeout(limit, future) => result.unwrap_or_else(|_| Err(no_reply())),
            () = async {
                while !cancelled.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            } => Err(cancelled_error()),
        }
    })
}

/// 发一个请求，读完回应正文；回应不是 200、正文超过 limit、idle 内没收到任何字节都返回错误
async fn send(
    port: u16,
    request: Request<Full<Bytes>>,
    limit: usize,
    idle: Duration,
) -> Result<Vec<u8>, AppError> {
    let stream = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
    )
    .await
    .map_err(|_| unreachable())?
    .map_err(|_| unreachable())?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| unreachable())?;
    let driver = tokio::spawn(connection);
    let result = async {
        let response = tokio::time::timeout(idle, sender.send_request(request))
            .await
            .map_err(|_| no_reply())?
            .map_err(|_| unreachable())?;
        match response.status() {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED => {
                return Err(failed(
                    "mcp.peer_token_rejected",
                    "另一台电脑的 Pierlink 不认这个令牌",
                ));
            }
            StatusCode::NOT_FOUND => {
                return Err(failed(
                    "mcp.peer_too_old",
                    "另一台电脑的 Pierlink 版本太旧，不能转发",
                ));
            }
            status => {
                return Err(
                    failed("mcp.peer_refused", "另一台电脑的 Pierlink 拒绝了这次转发")
                        .with("status", status.as_u16()),
                );
            }
        }
        let mut body = response.into_body();
        let mut data = Vec::new();
        loop {
            let frame = tokio::time::timeout(idle, body.frame())
                .await
                .map_err(|_| no_reply())?;
            let Some(frame) = frame else { break };
            let frame = frame.map_err(|_| unreachable())?;
            if let Some(chunk) = frame.data_ref() {
                if data.len() + chunk.len() > limit {
                    return Err(failed("mcp.peer_reply_too_large", "另一台电脑的回应太大"));
                }
                data.extend_from_slice(chunk);
            }
        }
        Ok(data)
    }
    .await;
    driver.abort();
    result
}

fn failed(key: &'static str, reason: &str) -> AppError {
    AppError::new("mcp.peer_failed").with_reason(key, reason)
}

fn unreachable() -> AppError {
    AppError::new("mcp.peer_unreachable")
        .with_reason("mcp.peer_connection_failed", "连不上另一台电脑的 Pierlink")
}

fn no_reply() -> AppError {
    AppError::new("mcp.peer_unreachable")
        .with_reason("mcp.peer_no_reply", "另一台电脑的 Pierlink 没有回应")
}
