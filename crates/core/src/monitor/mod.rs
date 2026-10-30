//! 常驻资源采样；仅在内存中保留有界帧队列，不写日志或采样存档。
use crate::{
    error::AppError,
    process::{SshProgram, io_error},
    ssh::{args::SshCommand, quote::sh_quote},
    tunnel::retry_delay,
};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::{
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

mod session;
use session::{Frame, Session};

/// 一次资源快照；内存、磁盘及显存的单位均为字节，时间为 Unix 毫秒。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ResourceSample {
    pub at_ms: u64,
    pub uptime_s: u64,
    pub load: [f64; 3],
    pub cpu_percent: Option<f64>,
    pub cpu_cores: u32,
    /// 各非 lo 网卡的接收/发送字节每秒；首帧、缺失或计数回绕时为 None。
    #[serde(default)]
    pub net_rx_bps: Option<f64>,
    #[serde(default)]
    pub net_tx_bps: Option<f64>,
    /// 最多 16 个非 lo 网卡；速率单位为字节每秒。
    #[serde(default)]
    pub nets: Vec<NetUsage>,
    pub mem_total: u64,
    pub mem_available: u64,
    /// 交换区总量和剩余（字节）；没开交换区时都是 0，旧帧缺失时也按 0
    #[serde(default)]
    pub swap_total: u64,
    #[serde(default)]
    pub swap_free: u64,
    pub disks: Vec<DiskUsage>,
    pub gpus: Vec<GpuUsage>,
    pub gpu_error: Option<String>,
    /// 仅每条新会话的首帧提供系统名，后续帧为 None。
    pub os_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct NetUsage {
    pub name: String,
    pub rx_bps: Option<f64>,
    pub tx_bps: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DiskUsage {
    pub mount: String,
    pub fs_type: String,
    pub total: u64,
    /// df 的 Used 列，不包含文件系统为 root 预留的空闲块；旧帧缺失时为零。
    #[serde(default)]
    pub used: u64,
    pub free: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct GpuUsage {
    pub index: u32,
    pub name: String,
    pub util_percent: Option<u32>,
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    pub temp_c: Option<u32>,
}

/// 事件保持枚举默认的外部标签序列化；Failed 与 Unsupported 均终止自动重试。
#[derive(Debug, Clone, Serialize, Type)]
pub enum MonitorEvent {
    Connecting,
    /// 缺少有效密码；没有退避和重试次数，新密码到来后立即恢复。
    WaitingPassword {
        error: AppError,
    },
    Sample(ResourceSample),
    Reconnecting {
        attempt: u32,
        next_retry_at_ms: u64,
        error: AppError,
    },
    /// reason 为 no_proc 或 no_posix_shell，均不自动重试。
    Unsupported {
        reason: String,
    },
    /// 认证或主机密钥失败；只返回错误码及安全参数，不包含远端错误输出。
    Failed {
        error: AppError,
    },
    /// 正常停止及终止性错误之后均发送一次。
    Stopped,
}

/// 采样线程的所有者；stop 与 Drop 都会终止并回收本地 SSH 进程树。
pub struct Monitor {
    control: Arc<Control>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Signals {
    stopped: bool,
    generation: u64,
    interval: Duration,
}
#[derive(Default)]
struct Control {
    signals: Mutex<Signals>,
    changed: Condvar,
}
impl Control {
    fn signal(&self, stop: bool) {
        let mut signals = self.signals.lock().unwrap_or_else(|e| e.into_inner());
        signals.stopped |= stop;
        signals.generation = signals.generation.wrapping_add(1);
        self.changed.notify_all();
    }
    fn snapshot(&self) -> (bool, u64) {
        let signals = self.signals.lock().unwrap_or_else(|e| e.into_inner());
        (signals.stopped, signals.generation)
    }
    fn wait(&self, duration: Duration, generation: u64) {
        let signals = self.signals.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self.changed.wait_timeout_while(signals, duration, |s| {
            !s.stopped && s.generation == generation
        });
    }
}

impl Monitor {
    /// 以 program 和后台用途 command 启动采样，返回会话所有者或参数、线程创建错误。
    /// interval 为零时使用 15 秒；超过三倍间隔无有效帧时重连（间隔 1 秒及以上时至少等 20 秒）。
    /// 磁盘每 10 秒；GPU 在基础间隔小于 5 秒时每 2 秒，否则每 10 秒，沿用未更新的结果。
    /// on_event 在采样线程同步调用，应及时返回；command.config_text 的落盘与清理由调用方负责。
    pub fn start(
        program: SshProgram,
        command: SshCommand,
        interval: Duration,
        on_event: Arc<dyn Fn(MonitorEvent) + Send + Sync>,
    ) -> Result<Self, AppError> {
        let interval = normalized_interval(interval);
        sample_timeout(interval)?;
        sampling_script(interval)?;
        let control = Arc::new(Control::default());
        control
            .signals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .interval = interval;
        let shared = control.clone();
        let worker = thread::Builder::new()
            .name("资源采样".into())
            .spawn(move || run(program, command, shared, on_event))
            .map_err(|_| {
                io_error(
                    "monitor.create_resource_sampling_thread",
                    "创建资源采样线程",
                )
            })?;
        Ok(Self {
            control,
            worker: Some(worker),
        })
    }

    /// 将基础采样间隔改为 interval，返回参数校验结果；不重建连接或重新认证。
    /// 零使用 15 秒；小于 5 秒时 GPU 每 2 秒，否则每 10 秒，磁盘始终每 10 秒。
    /// 修改在当前采样结束后立即唤醒等待，超时阈值同时随之更新。
    pub fn set_interval(&self, interval: Duration) -> Result<(), AppError> {
        let interval = normalized_interval(interval);
        sample_timeout(interval)?;
        sampling_script(interval)?;
        self.control
            .signals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .interval = interval;
        self.control.changed.notify_all();
        Ok(())
    }

    /// 无参数和返回值；唤醒退避等待，无有效采样的会话立即重建，仍在出帧的会话继续使用。
    /// Failed 或 Unsupported 已终止的监控不会复活，须重新调用 start。
    pub fn network_restored(&self) {
        self.control.signal(false);
    }

    /// 消耗监控句柄，无返回值；停止采样，等待进程回收及 Stopped 回调完成。
    pub fn stop(self) {
        drop(self);
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.control.signal(true);
        if let Some(worker) = self.worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

fn normalized_interval(interval: Duration) -> Duration {
    if interval.is_zero() {
        Duration::from_secs(15)
    } else {
        interval
    }
}
fn sample_timeout(interval: Duration) -> Result<Duration, AppError> {
    interval
        .checked_mul(3)
        .map(|d| {
            if interval >= Duration::from_secs(1) {
                d.max(Duration::from_secs(20))
            } else {
                d
            }
        })
        .filter(|d| Instant::now().checked_add(*d).is_some())
        .ok_or_else(|| AppError::new("monitor.invalid_interval"))
}
fn gpu_period(interval: Duration) -> u64 {
    if interval >= Duration::from_secs(5) {
        10
    } else {
        2
    }
}
fn interval_line(interval: Duration) -> String {
    format!("{:.9} {}\n", interval.as_secs_f64(), gpu_period(interval))
}

fn sampling_script(interval: Duration) -> Result<String, AppError> {
    let seconds = if interval.subsec_nanos() == 0 {
        interval.as_secs().to_string()
    } else {
        format!("{:.9}", interval.as_secs_f64())
    };
    let gpu_period = gpu_period(interval);
    let interval = sh_quote(&seconds)?;
    Ok(include_str!("sample.sh")
        .replacen("interval=15", &format!("interval={interval}"), 1)
        .replacen("gpu_period=2", &format!("gpu_period={gpu_period}"), 1))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

enum End {
    Stop,
    RetryNow,
    Unsupported(String),
    Error(AppError),
}

fn run(
    program: SshProgram,
    command: SshCommand,
    control: Arc<Control>,
    on_event: Arc<dyn Fn(MonitorEvent) + Send + Sync>,
) {
    let mut attempt: u32 = 0;
    loop {
        let (stopped, mut generation) = control.snapshot();
        if stopped {
            break;
        }
        on_event(MonitorEvent::Connecting);
        let interval = control
            .signals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .interval;
        let script = match sampling_script(interval) {
            Ok(script) => script,
            Err(error) => {
                on_event(MonitorEvent::Failed { error });
                break;
            }
        };
        let result = match Session::start(&program, &command, script) {
            Ok(mut session) => observe(
                &mut session,
                interval,
                &control,
                &mut generation,
                &mut attempt,
                &on_event,
            ),
            Err(error) => End::Error(error),
        };
        // observe 返回后先释放整个会话，再回调断开事件或等待重试。
        let error = match result {
            End::Stop => break,
            End::RetryNow => continue,
            End::Unsupported(reason) => {
                on_event(MonitorEvent::Unsupported { reason });
                break;
            }
            End::Error(error) => error,
        };
        if let Some(context) = &command.builtin
            && (error.code == "ssh.password_needed"
                || error.code == "ssh.auth_failed"
                    && error.params.contains_key("password_generation"))
        {
            // 跳板和目标分别保存密码，等待实际缺少凭据的那一级。
            let server_id = error
                .params
                .get("server_id")
                .cloned()
                .unwrap_or_else(|| context.server.id.clone());
            let subscription = context.vault.subscribe(&server_id);
            on_event(MonitorEvent::WaitingPassword { error });
            while !control.snapshot().0 && !context.vault.has(&server_id) {
                subscription.wait(Duration::from_millis(250));
            }
            continue;
        }
        if (error.code == "ssh.password_needed"
            || error.code == "ssh.auth_failed" && error.params.contains_key("password_generation"))
            && let Some(guard) = &command.askpass
        {
            let subscription = guard.subscribe();
            on_event(MonitorEvent::WaitingPassword { error });
            while !control.snapshot().0 && !guard.has_password() {
                subscription.wait(Duration::from_millis(250));
            }
            continue;
        }
        if matches!(
            error.code.as_str(),
            "ssh.auth_failed"
                | "ssh.too_many_auth_attempts"
                | "ssh.host_key_unknown"
                | "ssh.host_key_changed"
                | "ssh.two_factor_required"
        ) {
            on_event(MonitorEvent::Failed { error });
            break;
        }
        attempt = attempt.saturating_add(1);
        let delay = retry_delay(attempt);
        on_event(MonitorEvent::Reconnecting {
            attempt,
            next_retry_at_ms: now_ms().saturating_add(delay.as_millis() as u64),
            error,
        });
        control.wait(delay, generation);
    }
    on_event(MonitorEvent::Stopped);
}

fn observe(
    session: &mut Session,
    mut interval: Duration,
    control: &Control,
    generation: &mut u64,
    attempt: &mut u32,
    on_event: &Arc<dyn Fn(MonitorEvent) + Send + Sync>,
) -> End {
    let mut last_sample = Instant::now();
    let mut sampled = false;
    loop {
        let (stopped, current) = control.snapshot();
        if stopped {
            return End::Stop;
        }
        let next = control
            .signals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .interval;
        if next != interval {
            session.set_interval(next);
            interval = next;
        }
        let timeout = match sample_timeout(interval) {
            Ok(v) => v,
            Err(e) => return End::Error(e),
        };
        match session.frames() {
            Ok(frames) => {
                for frame in frames {
                    match frame {
                        Frame::Sample(sample) => {
                            last_sample = Instant::now();
                            sampled = true;
                            *attempt = 0;
                            on_event(MonitorEvent::Sample(*sample));
                        }
                        Frame::Unsupported => return End::Unsupported("no_proc".into()),
                    }
                }
            }
            Err(error) => return End::Error(error),
        }
        if let Err(error) = session.poll() {
            if error.code == "monitor.no_posix_shell" {
                return End::Unsupported("no_posix_shell".into());
            }
            return End::Error(error);
        }
        if current != *generation {
            *generation = current;
            if !sampled || last_sample.elapsed() >= timeout {
                return End::RetryNow;
            }
        }
        if last_sample.elapsed() >= timeout {
            return End::Error(
                AppError::new("monitor.sample_timeout")
                    .with("timeout_ms", timeout.as_millis())
                    .with_reason(
                        "monitor.no_valid_resource_frame_received_for_three_sampling_intervals",
                        "超过三个采样间隔未收到有效资源帧",
                    ),
            );
        }
        control.wait(Duration::from_millis(20), *generation);
    }
}

#[cfg(test)]
mod tests;
