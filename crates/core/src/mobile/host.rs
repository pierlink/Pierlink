//! 手机入口与桌面宿主的异步接口；宿主不应在异步方法中阻塞运行时。
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{future::Future, net::IpAddr, pin::Pin};
use tokio::sync::broadcast;

/// 可取消的宿主调用；连接关闭或超时会释放 future，宿主须同时释放等待框及未执行的工作。
pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AppError>> + Send + 'a>>;

/// 手机终端输入；keys 只接受 Enter、Tab、Shift+Tab、Backspace、Escape、方向键、Home、End 和 Ctrl+C/D/J/M/U。
/// text 与 keys 二选一，confirmed 只表示手机已完成危险命令的二次确认。
/// paste 只能用于 text，将含换行和制表符的正文作为一次粘贴；推荐再单独发送 Enter 提交。
/// 兼容正文末尾的一个回车：宿主须将该回车放在括号粘贴结束标记之后发送。
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalInput {
    pub text: Option<String>,
    pub keys: Option<Vec<String>>,
    #[serde(default)]
    pub paste: bool,
    #[serde(default)]
    pub confirmed: bool,
}
impl Drop for TerminalInput {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        if let Some(text) = &mut self.text {
            text.zeroize();
        }
    }
}

/// 手机操作记录；不包含邀请码、Cookie、输入文字、屏幕或宿主错误详情。
#[derive(Debug, Clone, Serialize)]
pub struct AuditRecord {
    pub device_name: String,
    pub action: String,
    pub target: Option<String>,
    /// ok、cancelled、needs_confirm 或稳定的错误码。
    pub result: String,
    /// 实际 TCP 来源地址。
    pub peer: String,
    /// 图片请求完整接收后的字节数；其他操作为 None，不记录图片内容。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<usize>,
}

/// 桌面是唯一执行者。JSON 查询结果原样透传，字段由宿主和网页约定；不得包含凭据。
/// 默认只读查询返回空对象或数组；写操作与安全读屏默认拒绝，配对默认拒绝。
pub trait MobileHost: Send + Sync + 'static {
    /// 用 device_name 和实际 TCP 来源 peer_ip 请求桌面批准，返回是否允许；入口最多等 120 秒。
    fn approve_pairing<'a>(
        &'a self,
        _device_name: &'a str,
        _peer_ip: IpAddr,
    ) -> HostFuture<'a, bool> {
        Box::pin(async { Ok(false) })
    }
    /// 无参数；返回服务器和同步概览 JSON（建议对象）。
    fn overview(&self) -> HostFuture<'_, Value> {
        Box::pin(async { Ok(json!({})) })
    }
    /// 无参数；返回已打开的终端 JSON（建议数组，含 id 和 title）。
    fn terminals(&self) -> HostFuture<'_, Value> {
        Box::pin(async { Ok(json!([])) })
    }
    /// 按 id 返回最近 lines 行的屏幕 JSON（建议对象）；lines 为 1..=2000，默认 300。
    /// 入口不解析其业务字段，宿主可同时返回纯文本和仅含 SGR 的彩色行。
    fn terminal_screen<'a>(&'a self, _id: &'a str, _lines: usize) -> HostFuture<'a, Value> {
        Box::pin(async { Err(unavailable()) })
    }
    /// 按 id 返回最新屏幕纯文本，用于密码提示及编辑后提交检查，末尾非空行为当前输入行。
    /// 不可用必须报错，不得用空文本代替失败。
    fn terminal_text<'a>(&'a self, _id: &'a str) -> HostFuture<'a, String> {
        Box::pin(async { Err(unavailable()) })
    }
    /// 把 input 发送到已打开的 id 终端，成功返回任意 JSON（建议 {"ok":true}）。
    /// 宿主须在实际写入前再次核对密码提示及终端状态，避免读屏后状态变化。
    /// paste 为真时保留正文的换行，按终端的 bracketed_paste 状态包裹正文；不得逐行提交。
    fn terminal_input<'a>(&'a self, _id: &'a str, _input: TerminalInput) -> HostFuture<'a, Value> {
        Box::pin(async { Err(unavailable()) })
    }
    /// 将已校验的 bytes 图片保存并把路径输入 id，成功返回 JSON；ext 仅为 png/jpg/webp/gif。
    /// 默认拒绝。宿主负责安全存储、过期清理，并在写入路径前复查密码提示和输入是否已继续。
    fn terminal_image<'a>(
        &'a self,
        _id: &'a str,
        _bytes: Vec<u8>,
        _ext: &'static str,
    ) -> HostFuture<'a, Value> {
        Box::pin(async { Err(unavailable()) })
    }
    /// 按手机宽度显示：size 为 Some((列, 行)) 时把 id 终端临时改成这个尺寸（手机定期再发一次续期），
    /// None 时恢复成电脑界面的尺寸。电脑上有人正在用这个终端时可以不改，返回 {"ok":false}。默认拒绝。
    fn terminal_size<'a>(
        &'a self,
        _id: &'a str,
        _size: Option<(u16, u16)>,
    ) -> HostFuture<'a, Value> {
        Box::pin(async { Err(unavailable()) })
    }
    /// 在电脑上打开一个终端：server 为服务器 ID，None 时打开本机终端；返回 {"id": 终端 ID}。
    /// 要等电脑界面连上服务器（可能要在电脑上确认指纹），宿主限定等待时间并短于请求期限。默认拒绝。
    fn open_terminal<'a>(&'a self, _server: Option<&'a str>) -> HostFuture<'a, Value> {
        Box::pin(async { Err(unavailable()) })
    }
    /// 无参数；返回待确认项目 JSON（建议数组，含 id）。
    fn confirms(&self) -> HostFuture<'_, Value> {
        Box::pin(async { Ok(json!([])) })
    }
    /// 按 id 回答确认框，allow 为允许或拒绝；返回 JSON（建议 {"ok":true}）。
    fn answer_confirm<'a>(&'a self, _id: &'a str, _allow: bool) -> HostFuture<'a, Value> {
        Box::pin(async { Err(unavailable()) })
    }
    /// 无参数；停止所有 AI 操作，返回 JSON（建议 {"ok":true}）。
    fn stop_all(&self) -> HostFuture<'_, Value> {
        Box::pin(async { Err(unavailable()) })
    }
    /// 返回最多 limit 条最近操作记录 JSON（建议数组），limit 在 1..=200，默认 50。
    fn activity(&self, _limit: usize) -> HostFuture<'_, Value> {
        Box::pin(async { Ok(json!([])) })
    }
    /// 非阻塞地接收一条 record，无返回值；如需落盘，由宿主脱敏并按现有审计保留策略写入。
    fn audit(&self, _record: AuditRecord) {}
    /// 无参数；返回新的事件订阅，无历史重放。None 表示仅发送心跳。
    /// 名称限定 confirms、terminals、overview、activity；广播滞后时断开，网页重连后重新拉取全部状态。
    fn subscribe(&self) -> Option<broadcast::Receiver<String>> {
        None
    }
}

fn unavailable() -> AppError {
    AppError::new("mobile.host_unavailable").with_reason(
        "mobile.desktop_does_not_provide_this_capability",
        "桌面未提供此项能力",
    )
}
