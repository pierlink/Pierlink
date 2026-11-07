//! 几台电脑同时接入同一台服务器（不用配对）。
//!
//! 每台电脑的服务器入口都在服务器上留一份伙伴文件 `~/.pierlink/peers/<实例号>.json`（端口、令牌、指定）。
//! AI 的调用到了哪个入口，那个入口就判断用户在哪台电脑前（指定 > 终端名牌 > 最近操作 > 自己），
//! 把整次调用转给那台执行：确认框弹在那台，按那台的权限做。
//! 服务器上的文件和脚本输出都可能被 AI 改写，只当路由提示：不影响任何权限、审批和本机设置；转发只一跳。
mod client;
mod route;
mod script;
#[cfg(test)]
mod tests;

pub(super) use client::{ForwardCall, ForwardReply};
pub(super) use route::{Router, route, translate_target};
pub use script::{discover_script, peer_file_script, remove_peer_file_script};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroizing;

/// 伙伴接口：回报实例号、电脑名和多久没操作（要入口令牌）
pub const PEER_PATH: &str = "/.pierlink-peer";
/// 转发接口：别的电脑把整次工具调用转来，只在本机执行、绝不再转（要入口令牌）
pub const FORWARD_PATH: &str = "/.pierlink-forward";
/// 转来的调用带这个头，值是发起那台电脑的实例号
pub const FORWARDED_HEADER: &str = "x-pierlink-forwarded";
/// 别的电脑上的终端、后台任务 ID 的前缀：`pc:<电脑编号>/<那台电脑上的 ID>`
const ID_PREFIX: &str = "pc:";
/// 伙伴文件里的令牌至少这么长（入口令牌是 43 个字符）：太短的当伪造，免得被登记成要遮蔽的秘密
const MIN_TOKEN: usize = 32;

/// 电脑编号：实例号的前 8 个字母数字。终端名牌的前缀、别的电脑上的 ID 前缀都用它
///
/// :param instance: 实例号
/// :return: 编号；实例号不够 8 个字母数字时为 None
pub fn short_id(instance: &str) -> Option<String> {
    let id: String = instance
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    (id.len() == 8).then_some(id)
}

/// 几台电脑上都一样的服务器标识：按地址（用户@主机:端口，不分大小写）算的短哈希。
/// 不同电脑上给同一台服务器起的名字、记录 ID 都可能不同，地址一样就是同一台
///
/// :param address: 服务器地址
/// :return: `srv-` 加 12 位十六进制
pub fn server_key(address: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(address.trim().to_lowercase().as_bytes());
    let hex: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("srv-{hex}")
}

/// 用户指定 AI 操作哪台电脑（或取消指定）；几台电脑各自指定过时以最新的为准
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinRecord {
    /// 指定的电脑的实例号；None 表示取消指定（自动判断）
    pub target: Option<String>,
    /// 指定的电脑名（显示用）
    #[serde(default)]
    pub computer: String,
    /// 指定的时间（Unix 毫秒）
    pub set_at: u64,
}

/// 一台电脑在服务器上的伙伴文件
#[derive(Debug, Clone)]
pub struct PeerRecord {
    /// 实例号
    pub instance: String,
    /// 电脑名（显示用）
    pub computer: String,
    /// 这台电脑的入口在服务器上监听的端口
    pub port: u16,
    /// 这台电脑入口的令牌
    pub token: Zeroizing<String>,
    /// 写文件的时间（Unix 毫秒）
    pub written_at: u64,
    /// 这台电脑上的用户做的指定
    pub pin: Option<PinRecord>,
}

/// 实例号、令牌只允许字母数字和 -_（放进请求头、脚本都不用转义）
fn plain(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 显示用的名字：去掉控制字符，最多 128 个字符
fn display(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(128).collect()
}

/// 读一份伙伴文件的内容；格式不对、字段不合规的返回 None（服务器上的文件不可信）
///
/// :param text: 文件内容
/// :return: 伙伴记录
pub fn parse_record(text: &str) -> Option<PeerRecord> {
    let value: Value = serde_json::from_str(text).ok()?;
    let instance = value["instance"].as_str()?;
    let token = value["token"].as_str()?;
    if !plain(instance, 64)
        || short_id(instance).is_none()
        || !plain(token, 256)
        || token.len() < MIN_TOKEN
    {
        return None;
    }
    let port = u16::try_from(value["port"].as_u64()?)
        .ok()
        .filter(|p| *p != 0)?;
    let pin = parse_pin(&value["pin"]);
    Some(PeerRecord {
        instance: instance.to_owned(),
        computer: display(value["computer"].as_str().unwrap_or_default()),
        port,
        token: Zeroizing::new(token.to_owned()),
        written_at: value["written_at"].as_u64().unwrap_or(0),
        pin,
    })
}

/// 读指定记录；格式不对的当没有指定
fn parse_pin(value: &Value) -> Option<PinRecord> {
    let set_at = value["set_at"].as_u64()?;
    let target = match &value["target"] {
        Value::Null => None,
        Value::String(target) if plain(target, 64) => Some(target.clone()),
        _ => return None,
    };
    Some(PinRecord {
        target,
        computer: display(value["computer"].as_str().unwrap_or_default()),
        set_at,
    })
}

/// 终端名牌对应的电脑编号：新格式 `<8 位编号>-<随机>`；旧格式（没有编号）返回 None
///
/// :param marker: 终端名牌 LC_PIERLINK_TERM
/// :return: 电脑编号
pub fn marker_owner(marker: &str) -> Option<&str> {
    let (owner, rest) = marker.split_once('-')?;
    (owner.len() == 8 && owner.bytes().all(|b| b.is_ascii_alphanumeric()) && !rest.is_empty())
        .then_some(owner)
}

/// 给别的电脑上的 ID 加前缀
fn prefixed(owner: &str, id: &str) -> String {
    format!("{ID_PREFIX}{owner}/{id}")
}

/// 拆开带前缀的 ID：返回（电脑编号, 那台电脑上的 ID）；没有前缀返回 None
fn split_id(id: &str) -> Option<(&str, &str)> {
    let rest = id.strip_prefix(ID_PREFIX)?;
    let (owner, inner) = rest.split_once('/')?;
    (owner.len() == 8 && !inner.is_empty()).then_some((owner, inner))
}

/// 一台别的电脑这一轮查到的状态
#[derive(Debug, Clone)]
pub(super) struct PeerState {
    pub record: PeerRecord,
    /// 电脑编号
    pub owner: String,
    /// 伙伴接口答了、实例号对得上
    pub online: bool,
    /// 那台电脑多久没有键盘鼠标输入
    pub idle_ms: Option<u64>,
    /// 本机到它入口的转发端口
    pub local_port: Option<u16>,
}

/// 判断出的当前电脑
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Current {
    /// 这台电脑自己
    Here,
    /// 别的电脑（伙伴列表里的序号）
    Peer(usize),
    /// 用户指定的电脑不在线（电脑名）
    PinnedOffline(String),
}

/// 判断依据（写日志、给界面看）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Basis {
    /// 用户指定
    Pin,
    /// AI 所在终端的名牌
    Marker,
    /// 最近有键盘鼠标输入
    Idle,
    /// 认不出来：这台电脑自己
    Default,
}

/// 几台电脑里最新的指定（自己的设置 + 别的电脑伙伴文件里的）
///
/// :param own: 这台电脑的指定设置
/// :param peers: 别的电脑
/// :return: 最新的指定；没有指定过为 None
pub(super) fn effective_pin(own: Option<&PinRecord>, peers: &[PeerState]) -> Option<PinRecord> {
    own.into_iter()
        .chain(peers.iter().filter_map(|p| p.record.pin.as_ref()))
        .max_by_key(|pin| pin.set_at)
        .cloned()
}

/// 判断用户在哪台电脑前：指定 > 终端名牌 > 最近有键盘鼠标输入的那台（不管隔了多久）> 自己。
/// 指定的电脑不在线时报出来，不改到别的电脑。
///
/// :param own_instance: 这台电脑的实例号
/// :param own_idle: 这台电脑多久没有输入
/// :param peers: 别的电脑
/// :param pin: 生效的指定
/// :param markers: AI 所在终端的名牌
/// :return: 当前电脑和判断依据
pub(super) fn decide(
    own_instance: &str,
    own_idle: Option<u64>,
    peers: &[PeerState],
    pin: Option<&PinRecord>,
    markers: &[String],
) -> (Current, Basis) {
    if let Some(PinRecord {
        target: Some(target),
        computer,
        ..
    }) = pin
    {
        if target == own_instance {
            return (Current::Here, Basis::Pin);
        }
        return match peers
            .iter()
            .position(|p| p.online && p.record.instance == *target)
        {
            Some(index) => (Current::Peer(index), Basis::Pin),
            None => (
                Current::PinnedOffline(if computer.is_empty() {
                    target.clone()
                } else {
                    computer.clone()
                }),
                Basis::Pin,
            ),
        };
    }
    let own = short_id(own_instance);
    let mut found: Vec<Current> = Vec::new();
    for owner in markers.iter().filter_map(|m| marker_owner(m)) {
        let hit = if own.as_deref() == Some(owner) {
            Some(Current::Here)
        } else {
            peers
                .iter()
                .position(|p| p.online && p.owner == owner)
                .map(Current::Peer)
        };
        if let Some(hit) = hit
            && !found.contains(&hit)
        {
            found.push(hit);
        }
    }
    if let [only] = found.as_slice() {
        return (only.clone(), Basis::Marker);
    }
    // 都很久没操作时也选最近用过的那台：用户多半回到那台，不能回落到可能没人的“自己”
    let mut best: Option<(u64, Current)> = own_idle.map(|ms| (ms, Current::Here));
    for (index, peer) in peers.iter().enumerate() {
        if let Some(ms) = peer.idle_ms.filter(|_| peer.online)
            && best.as_ref().is_none_or(|(b, _)| ms < *b)
        {
            best = Some((ms, Current::Peer(index)));
        }
    }
    match best {
        Some((_, current)) => (current, Basis::Idle),
        None => (Current::Here, Basis::Default),
    }
}

/// 给结果里所有 terminal_id、job_id 加上电脑编号前缀（别的电脑上的终端和任务）
fn prefix_ids(value: &mut Value, owner: &str) {
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                match item {
                    Value::String(id)
                        if (key == "terminal_id" || key == "job_id") && split_id(id).is_none() =>
                    {
                        *id = prefixed(owner, id);
                    }
                    _ => prefix_ids(item, owner),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                prefix_ids(item, owner);
            }
        }
        _ => {}
    }
}
