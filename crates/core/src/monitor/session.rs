//! SSH 会话与有界逐行解析；不回传服务器原文，避免错误输出泄露秘密。
use super::{NetUsage, ResourceSample};
use crate::{
    error::AppError,
    process::{Reader, SshProcess, SshProgram, io_error, join_bounded},
    remote::run::classify_error,
    ssh::args::SshCommand,
};
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(super) const MAX_LINE: usize = 64 * 1024;
const MAX_PENDING: usize = 4;

#[derive(Debug)]
pub(super) enum Frame {
    Sample(Box<ResourceSample>),
    Unsupported,
}

#[derive(Default)]
struct NetworkRates {
    previous: Option<(u64, u64, u64)>,
    interfaces: HashMap<String, (u64, u64, u64)>,
}
impl NetworkRates {
    fn apply(&mut self, sample: &mut ResourceSample, rx: Option<u64>, tx: Option<u64>, at: u64) {
        let current = rx.zip(tx).map(|(rx, tx)| (at, rx, tx));
        (sample.net_rx_bps, sample.net_tx_bps) = rates(self.previous, current);
        self.previous = current;
    }
    fn interfaces(&mut self, nets: Vec<WireNet>, at: u64) -> Vec<NetUsage> {
        let mut current = HashMap::new();
        let mut result = Vec::new();
        for net in nets {
            let name: String = net
                .name
                .chars()
                .filter(|c| !c.is_control())
                .take(32)
                .collect();
            if name.is_empty() || name == "lo" || current.contains_key(&name) {
                continue;
            }
            let counters = net.rx_bytes.zip(net.tx_bytes).map(|(rx, tx)| (at, rx, tx));
            let (rx_bps, tx_bps) = rates(self.interfaces.get(&name).copied(), counters);
            // 无效计数也占一个显示位置，但不能用于下一帧差值。
            current.insert(name.clone(), counters);
            result.push(NetUsage {
                name,
                rx_bps,
                tx_bps,
            });
            if result.len() == 16 {
                break;
            }
        }
        self.interfaces = current
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect();
        result
    }
}
fn rates(
    previous: Option<(u64, u64, u64)>,
    current: Option<(u64, u64, u64)>,
) -> (Option<f64>, Option<f64>) {
    if let (Some((last_at, last_rx, last_tx)), Some((at, rx, tx))) = (previous, current)
        && let (Some(elapsed), Some(rx), Some(tx)) = (
            at.checked_sub(last_at),
            rx.checked_sub(last_rx),
            tx.checked_sub(last_tx),
        )
        && elapsed > 0
    {
        (
            Some(rx as f64 * 1000.0 / elapsed as f64),
            Some(tx as f64 * 1000.0 / elapsed as f64),
        )
    } else {
        (None, None)
    }
}
#[derive(serde::Deserialize)]
struct WireNet {
    name: String,
    rx_bytes: Option<u64>,
    tx_bytes: Option<u64>,
}

#[derive(serde::Deserialize)]
struct WireSample {
    #[serde(flatten)]
    sample: ResourceSample,
    net_rx_bytes: Option<u64>,
    net_tx_bytes: Option<u64>,
    /// /proc/uptime 的毫秒时间不受远端系统校时影响；旧帧回退 at_ms。
    net_at_ms: Option<u64>,
    #[serde(default)]
    net_interfaces: Vec<WireNet>,
}
fn parse_line(line: &[u8], network: &mut NetworkRates) -> Option<Frame> {
    if line.len() > MAX_LINE {
        return None;
    }
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let json = line.strip_prefix(b"PLMON1 ")?.strip_suffix(b" PLMON1")?;
    if let Ok(mut wire) = serde_json::from_slice::<WireSample>(json) {
        let at = wire.net_at_ms.unwrap_or(wire.sample.at_ms);
        network.apply(&mut wire.sample, wire.net_rx_bytes, wire.net_tx_bytes, at);
        wire.sample.nets = network.interfaces(wire.net_interfaces, at);
        return Some(Frame::Sample(Box::new(wire.sample)));
    }
    let unsupported: serde_json::Value = serde_json::from_slice(json).ok()?;
    (unsupported.get("unsupported")?.as_str()? == "no_proc").then_some(Frame::Unsupported)
}

#[derive(Default)]
struct Lines {
    pending: VecDeque<Frame>,
    saw_marker: bool,
    done: bool,
    failed: bool,
}

#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    network: NetworkRates,
    oversized: bool,
    marker_prefix: usize,
}
impl Decoder {
    fn feed(&mut self, bytes: &[u8], state: &mut Lines) {
        let marker = b"PLMON1";
        for byte in bytes {
            // 坏帧、超长行及跨块标记也说明脚本曾经启动。
            if self.marker_prefix < marker.len() {
                self.marker_prefix = if *byte == marker[self.marker_prefix] {
                    self.marker_prefix + 1
                } else {
                    usize::from(*byte == marker[0])
                };
                state.saw_marker |= self.marker_prefix == marker.len();
            }
            if *byte == b'\n' {
                if !self.oversized
                    && let Some(frame) = parse_line(&self.line, &mut self.network)
                {
                    if state.pending.len() == MAX_PENDING {
                        state.pending.pop_front();
                    }
                    state.pending.push_back(frame);
                }
                self.line.clear();
                self.oversized = false;
            } else if self.line.len() < MAX_LINE {
                self.line.push(*byte);
            } else {
                self.oversized = true;
            }
        }
    }
}

pub(super) enum Session {
    OpenSsh(OpenSshSession),
    Builtin(builtin::BuiltinSession),
}
impl Session {
    pub(super) fn set_interval(&self, interval: Duration) {
        match self {
            Self::OpenSsh(session) => {
                if let Some(writer) = &session.writer {
                    writer.updates.set(super::interval_line(interval));
                }
            }
            Self::Builtin(session) => session.set_interval(interval),
        }
    }
    pub(super) fn start(
        program: &SshProgram,
        command: &SshCommand,
        script: String,
    ) -> Result<Self, AppError> {
        match &command.builtin {
            Some(context) => {
                builtin::BuiltinSession::start(context.as_ref().clone(), script).map(Self::Builtin)
            }
            None => OpenSshSession::start(program, command, script).map(Self::OpenSsh),
        }
    }
    pub(super) fn frames(&self) -> Result<Vec<Frame>, AppError> {
        match self {
            Self::OpenSsh(session) => session.frames(),
            Self::Builtin(session) => session.frames(),
        }
    }
    pub(super) fn poll(&mut self) -> Result<(), AppError> {
        match self {
            Self::OpenSsh(session) => session.poll(),
            Self::Builtin(session) => session.poll(),
        }
    }
}
#[path = "builtin_session.rs"]
mod builtin;

struct LineReader {
    state: Arc<Mutex<Lines>>,
    worker: Option<JoinHandle<()>>,
}
impl LineReader {
    fn start(input: impl Read + Send + 'static) -> Result<Self, AppError> {
        let state = Arc::new(Mutex::new(Lines::default()));
        Self::start_with_state(input, state)
    }
    fn start_with_state(
        mut input: impl Read + Send + 'static,
        state: Arc<Mutex<Lines>>,
    ) -> Result<Self, AppError> {
        let shared = state.clone();
        let worker = thread::Builder::new()
            .name("资源帧读取".into())
            .spawn(move || {
                let mut decoder = Decoder::default();
                let mut chunk = [0; 8192];
                loop {
                    let count = match input.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(count) => count,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            shared.lock().unwrap_or_else(|e| e.into_inner()).failed = true;
                            break;
                        }
                    };
                    decoder.feed(
                        &chunk[..count],
                        &mut shared.lock().unwrap_or_else(|e| e.into_inner()),
                    );
                }
                shared.lock().unwrap_or_else(|e| e.into_inner()).done = true;
            })
            .map_err(|_| {
                io_error(
                    "monitor.create_resource_frame_reader_thread",
                    "创建资源帧读取线程",
                )
            })?;
        Ok(Self {
            state,
            worker: Some(worker),
        })
    }
    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            join_bounded(worker, Duration::from_secs(2));
        }
    }
}

pub(super) struct OpenSshSession {
    process: SshProcess,
    stdout: Option<LineReader>,
    stderr: Option<Reader>,
    writer: Option<InputWriter>,
    exited: Option<Instant>,
}
impl OpenSshSession {
    pub(super) fn start(
        program: &SshProgram,
        command: &SshCommand,
        script: String,
    ) -> Result<Self, AppError> {
        let mut session = Self {
            process: program.spawn(command)?,
            stdout: None,
            stderr: None,
            writer: None,
            exited: None,
        };
        session.stdout = Some(LineReader::start(
            session
                .process
                .child
                .stdout
                .take()
                .ok_or_else(|| io_error("monitor.get_resource_output", "获取资源输出"))?,
        )?);
        session.stderr =
            Some(Reader::tail(
                session.process.child.stderr.take().ok_or_else(|| {
                    io_error("monitor.get_resource_error_output", "获取资源错误输出")
                })?,
                16 * 1024,
            )?);
        let stdin = session
            .process
            .child
            .stdin
            .take()
            .ok_or_else(|| io_error("monitor.get_sampling_script_input", "获取采样脚本输入"))?;
        session.writer = Some(InputWriter::start(
            stdin,
            lifetime_script(&script)?,
            Arc::default(),
        )?);
        Ok(session)
    }
    pub(super) fn frames(&self) -> Result<Vec<Frame>, AppError> {
        let mut data = self
            .stdout
            .as_ref()
            .ok_or_else(|| io_error("monitor.get_resource_reader", "获取资源读取器"))?
            .state
            .lock()
            .map_err(|_| io_error("monitor.read_resource_frame", "读取资源帧"))?;
        if data.failed {
            return Err(io_error("monitor.read_resource_output", "读取资源输出"));
        }
        Ok(data.pending.drain(..).collect())
    }
    pub(super) fn poll(&mut self) -> Result<(), AppError> {
        let stderr = self
            .stderr
            .as_ref()
            .ok_or_else(|| io_error("monitor.get_error_reader", "获取错误读取器"))?
            .state
            .lock()
            .map_err(|_| io_error("monitor.read_sampling_error_output", "读取采样错误输出"))?;
        let text = String::from_utf8_lossy(&stderr.bytes);
        let mut error = classify_error(None, &text, &[]);
        // 不保留任何远端原文；即便服务器原样打印秘密，也不会进入事件或日志。
        error.params.remove("stderr_tail");
        if matches!(
            error.code.as_str(),
            "ssh.auth_failed"
                | "ssh.too_many_auth_attempts"
                | "ssh.host_key_unknown"
                | "ssh.host_key_changed"
        ) {
            return Err(self.process.finish_error(error));
        }
        if stderr.failed {
            return Err(io_error(
                "monitor.read_sampling_error_output",
                "读取采样错误输出",
            ));
        }
        if let Some(status) = self.process.try_wait()? {
            let out = self
                .stdout
                .as_ref()
                .ok_or_else(|| io_error("monitor.get_resource_reader", "获取资源读取器"))?
                .state
                .lock()
                .map_err(|_| io_error("monitor.read_resource_status", "读取资源状态"))?;
            // 让读取线程先消化进程退出前的最后一帧，尤其是 Unsupported。
            if (out.done && stderr.done && out.pending.is_empty())
                || self.exited.get_or_insert_with(Instant::now).elapsed()
                    >= Duration::from_millis(500)
            {
                let authentication = self.process.finish_error(error.clone());
                if matches!(
                    authentication.code.as_str(),
                    "ssh.password_needed" | "ssh.two_factor_required"
                ) {
                    return Err(authentication);
                }
                // Windows sshd 可能使用 cmd 或 PowerShell；连接类错误仍重试，
                // 其余非零退出且从未输出定界符时，视为无法启动 POSIX 采样脚本。
                if error.code != "ssh.unreachable"
                    && (text.contains("is not recognized as an internal or external command")
                        || text.contains("The system cannot find the path specified")
                        || (text.contains("/bin/sh")
                            && (text.contains("not found")
                                || text.contains("No such file or directory")))
                        || (!out.saw_marker && status.code().is_some_and(|code| code != 0)))
                {
                    return Err(AppError::new("monitor.no_posix_shell"));
                }
                if let Some(code) = status.code() {
                    error = error.with("exit_code", code);
                }
                return Err(self.process.finish_error(error));
            }
        }
        Ok(())
    }
}
impl Drop for OpenSshSession {
    fn drop(&mut self) {
        let _ = self.process.terminate();
        if let Some(stdout) = self.stdout.as_mut() {
            stdout.join();
        }
        if let Some(stderr) = self.stderr.as_mut() {
            stderr.join();
        }
        if let Some(writer) = self.writer.take() {
            drop(writer);
        }
    }
}

#[derive(Default)]
struct InputUpdates {
    // 只保留最新值，快速显示/隐藏面板不会积累控制行。
    state: Mutex<(bool, Option<String>)>,
    changed: std::sync::Condvar,
}
impl InputUpdates {
    fn set(&self, line: String) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).1 = Some(line);
        self.changed.notify_all();
    }
}
struct InputWriter {
    updates: Arc<InputUpdates>,
    worker: Option<JoinHandle<()>>,
}
impl InputWriter {
    fn start(
        mut stdin: impl Write + Send + 'static,
        script: String,
        updates: Arc<InputUpdates>,
    ) -> Result<Self, AppError> {
        let shared = updates.clone();
        let worker = thread::Builder::new()
            .name("采样控制输入".into())
            .spawn(move || {
                if stdin.write_all(script.as_bytes()).is_err() {
                    return;
                }
                loop {
                    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    while !state.0 && state.1.is_none() {
                        state = shared
                            .changed
                            .wait(state)
                            .unwrap_or_else(|e| e.into_inner());
                    }
                    if state.0 {
                        break;
                    }
                    let line = state.1.take();
                    drop(state);
                    if let Some(line) = line
                        && stdin.write_all(line.as_bytes()).is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|_| io_error("monitor.create_sampling_control_thread", "创建采样控制线程"))?;
        Ok(Self {
            updates,
            worker: Some(worker),
        })
    }
}
impl Drop for InputWriter {
    fn drop(&mut self) {
        self.updates
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0 = true;
        self.updates.changed.notify_all();
        if let Some(worker) = self.worker.take() {
            join_bounded(worker, Duration::from_secs(2));
        }
    }
}
fn lifetime_script(script: &str) -> Result<String, AppError> {
    Ok(include_str!("lifetime.sh")
        .replace("PLMON_SCRIPT_QUOTED", &crate::ssh::quote::sh_quote(script)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn s11_只认完整定界符_坏行及超长行之后继续解析_缓存有界() {
        let valid = b"PLMON1 {\"unsupported\":\"no_proc\"} PLMON1\n";
        for bad in [
            &b"{\"unsupported\":\"no_proc\"}"[..],
            b"motd PLMON1 {\"unsupported\":\"no_proc\"} PLMON1",
            b"PLMON1 {bad} PLMON1",
            b"PLMON1 {\"unsupported\":\"no_proc\"}",
            b"PLMON1 {\"unsupported\":\"other\"} PLMON1",
            b"PLMON1 \xff PLMON1",
        ] {
            assert!(parse_line(bad, &mut NetworkRates::default()).is_none());
        }
        let mut bytes = valid[..valid.len() - 1].to_vec();
        bytes.splice(bytes.len() - 7..bytes.len() - 7, vec![b' '; MAX_LINE]);
        assert!(parse_line(&bytes, &mut NetworkRates::default()).is_none());
        bytes.push(b'\n');
        bytes.extend_from_slice(b"PLMON1 {broken}\n");
        for _ in 0..100 {
            bytes.extend_from_slice(valid);
        }
        let mut reader = LineReader::start(std::io::Cursor::new(bytes)).unwrap();
        reader.join();
        let data = reader.state.lock().unwrap();
        assert!(data.done && !data.failed);
        assert_eq!(data.pending.len(), MAX_PENDING);
        assert!(data.pending.iter().all(|f| matches!(f, Frame::Unsupported)));
    }

    #[test]
    fn f35_记住跨块及超长坏行中的定界符_不依赖有效帧或换行() {
        for prefix in [8190, MAX_LINE + 1] {
            let mut bytes = vec![b'x'; prefix];
            bytes.extend_from_slice(b"PPLMON1 {broken}");
            let mut reader = LineReader::start(std::io::Cursor::new(bytes)).unwrap();
            reader.join();
            let data = reader.state.lock().unwrap();
            assert!(data.done && data.saw_marker);
            assert!(data.pending.is_empty());
        }
        for bytes in [b"PLMON".as_slice(), b"PLMPLMON", "启动噪声".as_bytes()] {
            let mut reader = LineReader::start(std::io::Cursor::new(bytes)).unwrap();
            reader.join();
            let data = reader.state.lock().unwrap();
            assert!(data.done && !data.saw_marker);
        }
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;
    fn raw(at: u64, counters: &str) -> Vec<u8> {
        format!("PLMON1 {{\"at_ms\":{at},\"uptime_s\":1,\"load\":[0,0,0],\"cpu_percent\":null,\"cpu_cores\":1,\"mem_total\":1,\"mem_available\":1,\"disks\":[],\"gpus\":[],\"gpu_error\":null,\"os_name\":null{counters}}} PLMON1").into_bytes()
    }
    fn sample(at: u64, counters: &str, state: &mut NetworkRates) -> ResourceSample {
        match parse_line(&raw(at, counters), state).unwrap() {
            Frame::Sample(sample) => *sample,
            _ => panic!("应是采样帧"),
        }
    }
    #[test]
    fn 首帧_缺失_计数回绕_零间隔_时钟回拨_旧帧兼容() {
        let mut state = NetworkRates::default();
        let counters = ",\"net_rx_bytes\":100,\"net_tx_bytes\":200";
        assert!(sample(1000, counters, &mut state).net_rx_bps.is_none());
        let next = sample(
            3000,
            ",\"net_rx_bytes\":300,\"net_tx_bytes\":600",
            &mut state,
        );
        assert_eq!(
            (next.net_rx_bps, next.net_tx_bps),
            (Some(100.0), Some(200.0))
        );
        assert!(sample(4000, counters, &mut state).net_rx_bps.is_none());
        assert!(sample(4000, counters, &mut state).net_rx_bps.is_none());
        assert!(sample(3000, counters, &mut state).net_rx_bps.is_none());
        assert!(sample(5000, "", &mut state).net_rx_bps.is_none());
        assert!(sample(6000, counters, &mut state).net_rx_bps.is_none());
        assert!(
            sample(7000, ",\"net_rx_bytes\":400", &mut state)
                .net_tx_bps
                .is_none()
        );
        let disk: super::super::DiskUsage =
            serde_json::from_str(r#"{"mount":"/","fs_type":"ext4","total":100,"free":90}"#)
                .unwrap();
        assert_eq!(disk.used, 0);
    }
}

#[cfg(test)]
mod interface_tests {
    use super::*;
    fn net(name: &str, rx: Option<u64>) -> WireNet {
        WireNet {
            name: name.into(),
            rx_bytes: rx,
            tx_bytes: rx.map(|v| v * 2),
        }
    }
    #[test]
    fn 分网卡首帧增减回绕缺失及时间异常() {
        let mut rates = NetworkRates::default();
        assert!(
            rates.interfaces(vec![net("eth0", Some(100))], 1000)[0]
                .rx_bps
                .is_none()
        );
        let next = rates.interfaces(vec![net("eth0", Some(300)), net("eth1", Some(999))], 3000);
        assert_eq!((next[0].rx_bps, next[0].tx_bps), (Some(100.0), Some(200.0)));
        assert!(next[1].rx_bps.is_none());
        for (at, rx) in [
            (4000, Some(1)),
            (4000, Some(2)),
            (3000, Some(3)),
            (5000, None),
            (6000, Some(4)),
        ] {
            assert!(
                rates.interfaces(vec![net("eth0", rx)], at)[0]
                    .rx_bps
                    .is_none()
            );
        }
        assert!(rates.interfaces(vec![], 7000).is_empty());
        assert!(
            rates.interfaces(vec![net("eth0", Some(5))], 8000)[0]
                .rx_bps
                .is_none()
        );
    }
    #[test]
    fn 网卡名称过滤截断去重及数量有界() {
        let mut rates = NetworkRates::default();
        let mut nets = vec![
            net("lo", Some(1)),
            net("\n\t", Some(1)),
            net("网\u{7f}卡", Some(1)),
            net("网卡", Some(1)),
            net(&"长".repeat(40), Some(1)),
        ];
        nets.extend((0..30).map(|i| net(&format!("eth{i}"), Some(1))));
        let result = rates.interfaces(nets, 1000);
        assert_eq!(result.len(), 16);
        assert_eq!(result[0].name, "网卡");
        assert_eq!(result[1].name.chars().count(), 32);
        assert_eq!(rates.interfaces.len(), 16);
    }
}
