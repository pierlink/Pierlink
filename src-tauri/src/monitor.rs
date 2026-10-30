//! 服务器概况的 Tauri 命令：每台服务器的采样会话在应用里只有一份，所有看它的终端共用；
//! 最后一个观察者离开才停。有人看（概况没隐藏）时每秒采样，都隐藏时放慢。
//! 采样会话由核心库 `pierlink_core::monitor` 管理，每次采样经 Channel 推给界面。

use pierlink_core::error::AppError;
use pierlink_core::monitor::{Monitor, MonitorEvent, ResourceSample};
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};

/// 有人看时的采样间隔
const INTERVAL_VISIBLE: Duration = Duration::from_secs(1);
/// 概况都隐藏时的采样间隔（隐藏不停采样，再打开时马上有数）
const INTERVAL_HIDDEN: Duration = Duration::from_secs(5);
/// 同时采样的服务器最多几台
const MAX_SERVERS: usize = 8;

/// 一块磁盘的用量（字节数用 f64，避免前端类型里出现大整数）
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DiskDto {
    pub mount: String,
    pub fs_type: String,
    pub total: f64,
    /// df 的 Used 列（不含给 root 预留的部分）
    pub used: f64,
    /// df 的 Avail 列；界面按 df 的算法显示百分比：used / (used + free)
    pub free: f64,
}

/// 一块网卡每秒接收/发送的字节数；第一帧没有
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NetDto {
    pub name: String,
    pub rx_bps: Option<f64>,
    pub tx_bps: Option<f64>,
}

#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GpuDto {
    pub index: u32,
    pub name: String,
    pub util_percent: Option<u32>,
    pub mem_used: Option<f64>,
    pub mem_total: Option<f64>,
    pub temp_c: Option<u32>,
}

/// 一次采样
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SampleDto {
    pub at_ms: f64,
    pub uptime_s: f64,
    pub load: Vec<f64>,
    pub cpu_percent: Option<f64>,
    pub cpu_cores: u32,
    /// 全部网卡（不含回环）每秒接收/发送的字节数；第一帧没有
    pub net_rx_bps: Option<f64>,
    pub net_tx_bps: Option<f64>,
    /// 每块网卡（不含回环）的速度
    pub nets: Vec<NetDto>,
    pub mem_total: f64,
    pub mem_available: f64,
    /// 交换区总量和剩余；没开交换区时都是 0
    pub swap_total: f64,
    pub swap_free: f64,
    pub disks: Vec<DiskDto>,
    pub gpus: Vec<GpuDto>,
    pub gpu_error: Option<String>,
    pub os_name: Option<String>,
}

/// 推给界面的事件
#[derive(Serialize, Type, Clone)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MonitorEventDto {
    Connecting,
    /// 等用户输入密码（这台服务器要密码登录，本次运行还没输过或输错了）
    WaitingPassword {
        error: AppError,
    },
    Sample {
        sample: SampleDto,
    },
    #[serde(rename_all = "camelCase")]
    Reconnecting {
        attempt: u32,
        next_retry_at_ms: f64,
        error: AppError,
    },
    Unsupported {
        reason: String,
    },
    Failed {
        error: AppError,
    },
    Stopped,
}

fn sample(s: ResourceSample) -> SampleDto {
    SampleDto {
        at_ms: s.at_ms as f64,
        uptime_s: s.uptime_s as f64,
        load: s.load.to_vec(),
        cpu_percent: s.cpu_percent,
        cpu_cores: s.cpu_cores,
        net_rx_bps: s.net_rx_bps,
        net_tx_bps: s.net_tx_bps,
        nets: s
            .nets
            .into_iter()
            .map(|n| NetDto {
                name: n.name,
                rx_bps: n.rx_bps,
                tx_bps: n.tx_bps,
            })
            .collect(),
        mem_total: s.mem_total as f64,
        mem_available: s.mem_available as f64,
        swap_total: s.swap_total as f64,
        swap_free: s.swap_free as f64,
        disks: s
            .disks
            .into_iter()
            .map(|d| DiskDto {
                mount: d.mount,
                fs_type: d.fs_type,
                total: d.total as f64,
                used: d.used as f64,
                free: d.free as f64,
            })
            .collect(),
        gpus: s
            .gpus
            .into_iter()
            .map(|g| GpuDto {
                index: g.index,
                name: g.name,
                util_percent: g.util_percent,
                mem_used: g.mem_used.map(|x| x as f64),
                mem_total: g.mem_total.map(|x| x as f64),
                temp_c: g.temp_c,
            })
            .collect(),
        gpu_error: s.gpu_error,
        os_name: s.os_name,
    }
}

fn event(e: MonitorEvent) -> MonitorEventDto {
    match e {
        MonitorEvent::Connecting => MonitorEventDto::Connecting,
        MonitorEvent::WaitingPassword { error } => MonitorEventDto::WaitingPassword { error },
        MonitorEvent::Sample(s) => MonitorEventDto::Sample { sample: sample(s) },
        MonitorEvent::Reconnecting {
            attempt,
            next_retry_at_ms,
            error,
        } => MonitorEventDto::Reconnecting {
            attempt,
            next_retry_at_ms: next_retry_at_ms as f64,
            error,
        },
        MonitorEvent::Unsupported { reason } => MonitorEventDto::Unsupported { reason },
        MonitorEvent::Failed { error } => MonitorEventDto::Failed { error },
        MonitorEvent::Stopped => MonitorEventDto::Stopped,
    }
}

/// 一个观察者（一个终端窗格的概况）
struct Subscriber {
    channel: Channel<MonitorEventDto>,
    visible: bool,
}

/// 一台服务器的观察者和最近的事件（新观察者加入时先补发，不用等下一帧）
#[derive(Default)]
struct Hub {
    subs: HashMap<u64, Subscriber>,
    last_sample: Option<MonitorEventDto>,
    /// 最近一帧之后的状态（连接中、重连、等密码、失败……）；来了新帧就清掉
    last_status: Option<MonitorEventDto>,
    /// 采样已经终止（失败、不支持）：下次有人来看时重新开始
    dead: bool,
}

impl Hub {
    fn send(&mut self, e: MonitorEventDto) {
        for sub in self.subs.values() {
            let _ = sub.channel.send(e.clone());
        }
        match e {
            MonitorEventDto::Sample { .. } => {
                self.last_sample = Some(e);
                self.last_status = None;
            }
            MonitorEventDto::Stopped => {}
            MonitorEventDto::Failed { .. } | MonitorEventDto::Unsupported { .. } => {
                self.dead = true;
                self.last_status = Some(e);
            }
            other => self.last_status = Some(other),
        }
    }

    fn replay(&self, channel: &Channel<MonitorEventDto>) {
        for e in [&self.last_sample, &self.last_status].into_iter().flatten() {
            let _ = channel.send(e.clone());
        }
    }

    /// 有人看就快采，都隐藏就慢采
    fn interval(&self) -> Duration {
        if self.subs.values().any(|s| s.visible) {
            INTERVAL_VISIBLE
        } else {
            INTERVAL_HIDDEN
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// 一台服务器的采样
struct Watch {
    hub: Arc<Mutex<Hub>>,
    monitor: Monitor,
    /// 采样会话当前的间隔
    interval: Duration,
}

/// 所有在采样的服务器
#[derive(Default)]
pub struct MonitorState {
    servers: Mutex<HashMap<String, Watch>>,
    seq: AtomicU64,
    /// 开始观察的过程一次只走一个：两个窗格几乎同时连同一台服务器时，不会各开一份、丢掉先来的观察者
    starting: Mutex<()>,
}

impl MonitorState {
    /// 应用退出时全部停掉
    pub fn shutdown(&self) {
        let all: Vec<Watch> = lock(&self.servers).drain().map(|(_, w)| w).collect();
        drop(all);
    }
}

/// 手机概况用：这台服务器最近一帧的 CPU 和内存占用（百分比）；没在采样时为 None
pub(crate) fn latest_usage(app: &AppHandle, server_id: &str) -> Option<(Option<f64>, Option<f64>)> {
    let state = app.state::<MonitorState>();
    let servers = lock(&state.servers);
    let watch = servers.get(server_id)?;
    let hub = lock(&watch.hub);
    match hub.last_sample.as_ref()? {
        MonitorEventDto::Sample { sample } => {
            let mem = (sample.mem_total > 0.0)
                .then(|| (sample.mem_total - sample.mem_available) / sample.mem_total * 100.0);
            Some((sample.cpu_percent, mem))
        }
        _ => None,
    }
}

/// 服务器被删除：停掉它的采样
pub fn stop_server(app: &AppHandle, server_id: &str) {
    let old = lock(&app.state::<MonitorState>().servers).remove(server_id);
    drop(old);
}

/// 网络恢复：立即重连所有采样会话
pub fn network_restored(app: &AppHandle) {
    for w in lock(&app.state::<MonitorState>().servers).values() {
        w.monitor.network_restored();
    }
}

/// 按观察者的显示情况调整采样间隔
fn retune(server_id: &str, w: &mut Watch) {
    let want = lock(&w.hub).interval();
    if want != w.interval {
        // 不重连，旧间隔一轮之内生效
        match w.monitor.set_interval(want) {
            Ok(()) => {
                w.interval = want;
                eprintln!(
                    "调整概况采样间隔: 服务器={server_id}, 间隔秒={}",
                    want.as_secs_f64()
                );
            }
            Err(e) => eprintln!("调整概况采样间隔失败: 服务器={server_id}, 错误={}", e.code),
        }
    }
}

/// 开始一台服务器的采样会话，事件发给 hub 里的所有观察者
fn start_watch(app: &AppHandle, server_id: &str, hub: Arc<Mutex<Hub>>) -> Result<Watch, AppError> {
    let (program, command, name) = crate::deploy::background_command(
        app,
        server_id,
        crate::keys::Access::Background("资源概览"),
    )?;
    // 内置 SSH 的失败标上 engine=builtin：要验证码时界面提供“登录”，登录后自动重开采样
    let builtin = command.builtin.is_some();
    let interval = lock(&hub).interval();
    let target = hub.clone();
    let on_event = Arc::new(move |e: MonitorEvent| {
        let e = match e {
            MonitorEvent::Failed { error } if builtin => MonitorEvent::Failed {
                error: error.with("engine", "builtin"),
            },
            other => other,
        };
        lock(&target).send(event(e));
    });
    let monitor = Monitor::start(program, command, interval, on_event)?;
    eprintln!(
        "开始服务器概况采样: 服务器={name}, 间隔秒={}",
        interval.as_secs_f64()
    );
    Ok(Watch {
        hub,
        monitor,
        interval,
    })
}

/// 开始看一台服务器的概况：已经有人在看就共用那份采样，先补发最近一帧；返回观察编号，停止时传回
///
/// :param server_id: 服务器 ID
/// :param visible: 概况是否显示（隐藏时也接着采样，只是放慢）
/// :param on_event: 采样事件通道
/// :return: 观察编号；同时采样的服务器超过上限时返回 monitor.too_many
#[tauri::command]
#[specta::specta]
pub async fn monitor_watch(
    app: AppHandle,
    server_id: String,
    visible: bool,
    on_event: Channel<MonitorEventDto>,
) -> Result<f64, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<MonitorState>();
        let _starting = lock(&state.starting);
        let id = state.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let sub = Subscriber {
            channel: on_event,
            visible,
        };
        // 已经在采样（且没终止）：加入观察者，补发最近的事件
        {
            let mut servers = lock(&state.servers);
            if let Some(w) = servers.get_mut(&server_id) {
                let mut hub = lock(&w.hub);
                if !hub.dead {
                    hub.replay(&sub.channel);
                    hub.subs.insert(id, sub);
                    drop(hub);
                    retune(&server_id, w);
                    return Ok(id as f64);
                }
            }
            let running = servers.len() - usize::from(servers.contains_key(&server_id));
            if running >= MAX_SERVERS {
                return Err(AppError::new("monitor.too_many").with("max", MAX_SERVERS as u64));
            }
        }
        // 新开（或重开已终止的）采样：原来的观察者一并接到新会话上
        let old = lock(&state.servers).remove(&server_id);
        let hub = Arc::new(Mutex::new(Hub::default()));
        if let Some(old) = old {
            let subs = std::mem::take(&mut lock(&old.hub).subs);
            lock(&hub).subs = subs;
            drop(old);
        }
        lock(&hub).subs.insert(id, sub);
        let watch = start_watch(&app, &server_id, hub)?;
        lock(&state.servers).insert(server_id, watch);
        Ok(id as f64)
    })
    .await
    .map_err(|e| AppError::new("monitor.task_failed").with("reason", e.to_string()))?
}

/// 某个观察者的概况显示或隐藏（只影响采样快慢，不停采样）
///
/// :param id: 观察编号
/// :param visible: 是否显示
#[tauri::command]
#[specta::specta]
pub async fn monitor_visible(app: AppHandle, id: f64, visible: bool) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<MonitorState>();
        let mut servers = lock(&state.servers);
        for (server_id, w) in servers.iter_mut() {
            let found = match lock(&w.hub).subs.get_mut(&(id as u64)) {
                Some(sub) => {
                    sub.visible = visible;
                    true
                }
                None => false,
            };
            if found {
                retune(server_id, w);
                break;
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| AppError::new("monitor.task_failed").with("reason", e.to_string()))?
}

/// 停止看概况；这台服务器没人看了才停掉采样
///
/// :param id: 观察编号
#[tauri::command]
#[specta::specta]
pub async fn monitor_stop(app: AppHandle, id: f64) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<MonitorState>();
        let old = {
            let mut servers = lock(&state.servers);
            let mut empty = None;
            for (server_id, w) in servers.iter_mut() {
                let mut hub = lock(&w.hub);
                if hub.subs.remove(&(id as u64)).is_some() {
                    if hub.subs.is_empty() {
                        empty = Some(server_id.clone());
                    } else {
                        drop(hub);
                        retune(server_id, w);
                    }
                    break;
                }
            }
            empty.and_then(|s| servers.remove(&s).map(|w| (s, w)))
        };
        // 在锁外停：要等进程回收
        if let Some((server_id, w)) = old {
            drop(w);
            eprintln!("停止服务器概况采样: 服务器={server_id}");
        }
        Ok(())
    })
    .await
    .map_err(|e| AppError::new("monitor.task_failed").with("reason", e.to_string()))?
}
