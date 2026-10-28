//! 有界启动与停止；PID 仅在可执行文件、实例路径和进程身份匹配后才用于接管。
use super::{
    client::Client,
    error,
    home::{self, GuiListen},
    io_error,
};
use crate::{
    error::AppError,
    process::{Reader, SshProcess, SshProgram},
    security::redact,
    ssh::args::SshCommand,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct SyncthingBin {
    pub path: PathBuf,
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    pid: u32,
    identity: String,
}
pub struct RunningInstance {
    pid: u32,
    bin: SyncthingBin,
    home: PathBuf,
    identity: String,
    gui: String,
    group: Vec<Record>,
    client: Client,
    child: Option<SshProcess>,
    readers: Vec<Reader>,
    stopped: bool,
}
fn spawn(bin: &SyncthingBin, args: Vec<String>, key: Option<&str>) -> Result<SshProcess, AppError> {
    let mut env = vec![("STNOUPGRADE".into(), "1".into())];
    if let Some(key) = key {
        env.push(("STGUIAPIKEY".into(), key.into()));
    }
    let cmd = SshCommand {
        args,
        env,
        config_text: None,
        builtin: None,
        askpass: None,
    };
    let mut environment: Vec<_> = [
        "PATH",
        "HOME",
        "USERPROFILE",
        "SystemRoot",
        "ProgramData",
        "SystemDrive",
        "LOCALAPPDATA",
        "TEMP",
        "TMP",
    ]
    .iter()
    .filter_map(|key| std::env::var_os(key).map(|value| ((*key).into(), value)))
    .collect();
    environment.extend(
        cmd.env
            .iter()
            .map(|(key, value)| (key.into(), value.into())),
    );
    SshProgram {
        path: bin.path.clone(),
    }
    .spawn_with_environment(&cmd, environment)
    .map_err(|_| io_error("启动 Syncthing 进程"))
}
fn readers(child: &mut SshProcess) -> Result<Vec<Reader>, AppError> {
    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| io_error("获取进程输出"))?;
    let stderr = child
        .child
        .stderr
        .take()
        .ok_or_else(|| io_error("获取进程错误输出"))?;
    Ok(vec![
        Reader::tail(stdout, 64 * 1024).map_err(|_| io_error("读取进程输出"))?,
        Reader::tail(stderr, 64 * 1024).map_err(|_| io_error("读取进程错误输出"))?,
    ])
}
fn tail(readers: &[Reader]) -> String {
    readers
        .iter()
        .filter_map(|r| {
            r.state
                .lock()
                .ok()
                .map(|s| String::from_utf8_lossy(&s.bytes).into_owned())
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn start_error(text: &str, secrets: &[&str], reason: &str) -> AppError {
    let code = if super::port_in_use(text) {
        "syncthing.listen_port_in_use"
    } else if text.contains("Failed to acquire lock") {
        "syncthing.home_locked"
    } else {
        "syncthing.start_failed"
    };
    let safe = redact(text, secrets);
    let tail = safe
        .chars()
        .rev()
        .take(2048)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    let mut e = AppError::new(code)
        .with("reason", reason)
        .with("log_tail", tail);
    if code == "syncthing.home_locked" {
        let pid = regex::Regex::new(r"(?i)pid[= :]+([0-9]+)")
            .ok()
            .and_then(|re| {
                re.captures(text)
                    .and_then(|c| c.get(1).map(|v| v.as_str().to_owned()))
            })
            .unwrap_or_else(|| "未知".into());
        e = e.with("pid", pid);
    }
    e
}
pub(crate) fn run_short(
    bin: &SyncthingBin,
    args: Vec<String>,
    password: Option<&str>,
) -> Result<String, AppError> {
    let mut child = spawn(bin, args, None)?;
    let mut readers = readers(&mut child)?;
    let result = (|| {
        if let Some(password) = password {
            // 输入严格限制为 4096 字节；使用独立线程避免异常程序不读 stdin 时堵住调用方。
            let mut stdin = child
                .child
                .stdin
                .take()
                .ok_or_else(|| io_error("获取标准输入"))?;
            let bytes = format!("{password}\n").into_bytes();
            let writer = thread::Builder::new()
                .name("Syncthing口令写入".into())
                .spawn(move || stdin.write_all(&bytes))
                .map_err(|_| io_error("创建输入线程"))?;
            let deadline = Instant::now() + Duration::from_secs(30);
            while !writer.is_finished() {
                if Instant::now() >= deadline {
                    return Err(start_error("", &[], "写入口令超时"));
                }
                thread::sleep(Duration::from_millis(10));
            }
            writer
                .join()
                .map_err(|_| io_error("输入线程失败"))?
                .map_err(|_| io_error("写入口令"))?;
        }
        drop(child.child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait().map_err(|_| io_error("查询子进程"))? {
                for reader in &mut readers {
                    reader.join();
                }
                if !status.success() {
                    return Err(start_error(
                        &tail(&readers),
                        &[password.unwrap_or("")],
                        "子命令失败",
                    ));
                }
                let out = readers[0]
                    .state
                    .lock()
                    .map_err(|_| io_error("读取子命令结果"))?;
                return String::from_utf8(out.bytes.clone())
                    .map_err(|_| AppError::new("syncthing.invalid_response"));
            }
            if Instant::now() >= deadline {
                return Err(start_error(
                    &tail(&readers),
                    &[password.unwrap_or("")],
                    "子命令超时",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    let _ = child.terminate();
    for reader in &mut readers {
        reader.join();
    }
    result
}
fn record(home: &Path) -> Result<Option<Record>, AppError> {
    match fs::read(home.join("pierlink.pid")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok().or_else(|| {
            std::str::from_utf8(&bytes)
                .ok()?
                .trim()
                .parse()
                .ok()
                .map(|pid| Record {
                    pid,
                    identity: String::new(),
                })
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(io_error("读取 PID 文件")),
    }
}
#[cfg(target_os = "linux")]
fn identity(pid: u32, bin: &Path, home: &Path, gui: &str) -> Option<String> {
    if pid <= 1 {
        return None;
    }
    let root = PathBuf::from(format!("/proc/{pid}"));
    if fs::read_link(root.join("exe")).ok()? != fs::canonicalize(bin).ok()? {
        return None;
    }
    let bytes = fs::read(root.join("cmdline")).ok()?;
    let args: Vec<_> = bytes.split(|b| *b == 0).collect();
    let expected = format!("--home={}", home.to_str()?);
    let expected_gui = format!("--gui-address={gui}");
    if !args.contains(&b"serve".as_slice())
        || !args.contains(&b"--no-restart".as_slice())
        || !args.contains(&expected.as_bytes())
        || !args.contains(&expected_gui.as_bytes())
    {
        return None;
    }
    let stat = fs::read_to_string(root.join("stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    if fields.first() == Some(&"Z") {
        return None;
    }
    Some(fields.get(19)?.to_string())
}
#[cfg(windows)]
fn identity(pid: u32, bin: &Path, home: &Path, gui: &str) -> Option<String> {
    super::windows::identity(pid, bin, home, gui)
}
#[cfg(not(any(target_os = "linux", windows)))]
fn identity(_: u32, _: &Path, _: &Path, _: &str) -> Option<String> {
    None
}
fn matching(bin: &Path, home: &Path, gui: &str) -> Result<Vec<Record>, AppError> {
    #[cfg(target_os = "linux")]
    let pids: Vec<u32> = fs::read_dir("/proc")
        .map_err(|_| io_error("枚举进程"))?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    #[cfg(windows)]
    let pids = super::windows::pids()?;
    #[cfg(not(any(target_os = "linux", windows)))]
    let pids: Vec<u32> = return Err(io_error("此平台不支持进程身份查询"));
    Ok(pids
        .into_iter()
        .filter_map(|pid| identity(pid, bin, home, gui).map(|identity| Record { pid, identity }))
        .collect())
}
fn terminate_group(bin: &Path, home: &Path, gui: &str, group: &[Record]) -> Result<(), AppError> {
    let mut failure = None;
    for r in group {
        if identity(r.pid, bin, home, gui).as_ref() != Some(&r.identity) {
            continue;
        }
        #[cfg(unix)]
        if unsafe { libc::kill(r.pid as i32, libc::SIGKILL) } != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        {
            failure = Some(io_error("终止匹配进程"));
        }
        #[cfg(windows)]
        if let Err(e) = super::windows::terminate(r.pid) {
            failure = Some(e);
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while group
        .iter()
        .any(|r| identity(r.pid, bin, home, gui).as_ref() == Some(&r.identity))
    {
        if Instant::now() >= deadline {
            return Err(io_error("等待匹配进程退出"));
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}
pub(crate) fn record_is_running(
    bin: &SyncthingBin,
    home: &Path,
    gui: &GuiListen,
) -> Result<bool, AppError> {
    Ok(!matching(&bin.path, home, &gui.cli_address()?)?.is_empty())
}
/// 根据 home 内记录的程序与管理地址核验并强制结束整组进程；无匹配进程也成功。
pub fn force_stop(home: &Path) -> Result<(), AppError> {
    let home = crate::fs_local::plain_canonical(home).map_err(|_| io_error("解析实例目录"))?;
    let _lock = home::lock(&home)?;
    let bytes =
        fs::read(home.join("pierlink-process.json")).map_err(|_| io_error("读取实例进程信息"))?;
    let (bin, gui): (PathBuf, String) =
        serde_json::from_slice(&bytes).map_err(|_| error("实例进程信息无效"))?;
    let group = matching(&bin, &home, &gui)?;
    terminate_group(&bin, &home, &gui, &group)?;
    remove_pid(&home)
}

// 迁移时进程记录可能已经搬到新目录，命令行中的 home 仍是旧目录。
// 返回目录锁并由迁移持有，避免停止后又被本应用的启动入口拉起。
pub(crate) fn stop_for_migration(
    storage: &Path,
    process_home: &Path,
) -> Result<home::HomeLock, AppError> {
    let lock = home::lock(storage)?;
    let process_home = crate::fs_local::plain_canonical(process_home)
        .map_err(|_| io_error("解析迁移前实例目录"))?;
    match fs::read(storage.join("pierlink-process.json")) {
        Ok(bytes) => {
            let (bin, gui): (PathBuf, String) =
                serde_json::from_slice(&bytes).map_err(|_| error("实例进程信息无效"))?;
            let group = matching(&bin, &process_home, &gui)?;
            terminate_group(&bin, &process_home, &gui, &group)?;
            remove_pid(storage)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if storage
                .join("pierlink.pid")
                .try_exists()
                .map_err(|_| io_error("检查迁移前进程记录"))?
            {
                return Err(AppError::new("syncthing.busy").with(
                    "reason",
                    "存在 PID 记录但缺少可核验的程序信息，无法安全迁移",
                ));
            }
        }
        Err(_) => return Err(io_error("读取迁移前进程信息")),
    }
    Ok(lock)
}
/// 用 bin、home、gui 和仅经环境传递的 api_key 启动或接管实例；30 秒内确认认证接口可用。
pub fn start(
    bin: &SyncthingBin,
    home: &Path,
    gui: &GuiListen,
    api_key: &str,
) -> Result<RunningInstance, AppError> {
    for attempt in 0..2 {
        match start_once(bin, home, gui, api_key, Duration::from_secs(30)) {
            Err(e) if e.code == "syncthing.home_locked" && attempt == 0 => {
                let canonical =
                    crate::fs_local::plain_canonical(home).map_err(|_| io_error("解析实例目录"))?;
                let group = matching(&bin.path, &canonical, &gui.cli_address()?)?;
                if group.is_empty()
                    || e.params
                        .get("pid")
                        .and_then(|p| p.parse::<u32>().ok())
                        .is_some_and(|pid| !group.iter().any(|r| r.pid == pid))
                {
                    return Err(e);
                }
                force_stop(home)?;
            }
            result => return result,
        }
    }
    Err(AppError::new("syncthing.home_locked"))
}
fn start_once(
    bin: &SyncthingBin,
    home: &Path,
    gui: &GuiListen,
    api_key: &str,
    timeout: Duration,
) -> Result<RunningInstance, AppError> {
    if api_key.is_empty() {
        return Err(error("启动实例必须提供 API 密钥"));
    }
    let address = gui.address()?;
    // 日志里报告的实际监听地址应与之完全一致
    let address_plain = address.clone();
    let cli_address = gui.cli_address()?;
    let home = crate::fs_local::plain_canonical(home).map_err(|_| io_error("解析实例目录"))?;
    if !home.join("config.xml").is_file() {
        return Err(error("必须先初始化实例配置"));
    }
    let _lock = home::lock(&home)?;
    if let GuiListen::Unix { path } = gui {
        home::socket_dir(path.parent().ok_or_else(|| error("套接字缺少父目录"))?)?;
    }
    let client = match gui {
        GuiListen::Tcp { .. } => Client::new(&format!("http://{address}"), api_key)?,
        #[cfg(unix)]
        GuiListen::Unix { path } => Client::unix(path, api_key)?,
        #[cfg(not(unix))]
        GuiListen::Unix { .. } => return Err(error("此平台不支持 Unix 套接字")),
    };
    let ownership = serde_json::to_vec(&(
        fs::canonicalize(&bin.path).map_err(|_| io_error("解析程序路径"))?,
        &cli_address,
    ))
    .map_err(|_| io_error("编码实例进程信息"))?;
    // 地址或程序变更时保留仍在运行的旧实例信息，让 force_stop 仍能找到它。
    if let Ok(bytes) = fs::read(home.join("pierlink-process.json"))
        && let Ok((old_bin, old_gui)) = serde_json::from_slice::<(PathBuf, String)>(&bytes)
        && (old_gui != cli_address || fs::canonicalize(&bin.path).ok().as_ref() != Some(&old_bin))
        && let Some(owner) = matching(&old_bin, &home, &old_gui)?.first()
    {
        return Err(AppError::new("syncthing.home_locked").with("pid", owner.pid));
    }
    home::atomic_write(&home.join("pierlink-process.json"), &ownership)?;
    let mut group = matching(&bin.path, &home, &cli_address)?;
    group.sort_by_key(|r| r.pid);
    // 接管已在运行的实例：主进程优先用上次启动时记下的那个（PID 文件）。
    // Windows 的 PID 不按启动先后递增，取最小的可能取到子进程。
    let recorded = record(&home)?.map(|r| r.pid);
    let primary = recorded
        .and_then(|pid| group.iter().find(|r| r.pid == pid))
        .or(group.first())
        .cloned();
    if let Some(r) = primary {
        let deadline = Instant::now() + timeout;
        loop {
            match client.ping_with_timeout(Duration::from_millis(500)) {
                Ok(()) => break,
                Err(e) => {
                    if e.code == "syncthing.unauthorized" || Instant::now() >= deadline {
                        return Err(AppError::new("syncthing.adopt_failed")
                            .with("pid", r.pid)
                            .with("reason", e.code));
                    }
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        group = matching(&bin.path, &home, &cli_address)?;
        loop {
            let log = read_log(&home)?;
            match verify_gui(&log, &address_plain) {
                Err(e) => {
                    terminate_group(&bin.path, &home, &cli_address, &group)?;
                    return Err(e);
                }
                Ok(true) => break,
                Ok(false) if Instant::now() >= deadline => {
                    terminate_group(&bin.path, &home, &cli_address, &group)?;
                    return Err(start_error("", &[], "未见监听地址"));
                }
                _ => thread::sleep(Duration::from_millis(50)),
            }
        }
        match client.listeners_ready(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(1)),
        ) {
            Ok(true) => {}
            Err(e) if e.code == "syncthing.listen_address_mismatch" => {
                terminate_group(&bin.path, &home, &cli_address, &group)?;
                return Err(e);
            }
            result => {
                return Err(AppError::new("syncthing.adopt_failed")
                    .with("pid", r.pid)
                    .with(
                        "reason",
                        result
                            .err()
                            .map(|e| e.code)
                            .unwrap_or_else(|| "同步监听未就绪".into()),
                    ));
            }
        }
        let group = matching(&bin.path, &home, &cli_address)?;
        write_record(&home, &r, &group)?;
        return Ok(RunningInstance {
            pid: r.pid,
            bin: bin.clone(),
            home,
            identity: r.identity.clone(),
            gui: cli_address,
            group,
            client,
            child: None,
            readers: vec![],
            stopped: false,
        });
    }
    remove_pid(&home)?;
    let args = vec![
        "serve".into(),
        format!(
            "--home={}",
            home.to_str().ok_or_else(|| error("实例路径不是有效文本"))?
        ),
        "--no-browser".into(),
        "--no-restart".into(),
        "--no-upgrade".into(),
        format!("--gui-address={cli_address}"),
        format!(
            "--log-file={}",
            home.join("syncthing.log")
                .to_str()
                .ok_or_else(|| error("日志路径不是有效文本"))?
        ),
        "--log-max-size=10485760".into(),
        "--log-max-old-files=1".into(),
    ];
    let mut child =
        spawn(bin, args, Some(api_key)).map_err(|_| start_error("", &[], "无法启动程序"))?;
    drop(child.child.stdin.take());
    let readers = readers(&mut child)?;
    let pid = child.child.id();
    let deadline = Instant::now() + timeout;
    let mut instance = RunningInstance {
        pid,
        bin: bin.clone(),
        home,
        identity: String::new(),
        gui: cli_address,
        group: vec![],
        client,
        child: Some(child),
        readers,
        stopped: false,
    };
    loop {
        #[cfg(unix)]
        {
            instance.group = matching(&bin.path, &instance.home, &instance.gui)?;
        }
        if instance
            .child
            .as_mut()
            .ok_or_else(|| io_error("丢失进程句柄"))?
            .try_wait()
            .map_err(|_| io_error("查询进程"))?
            .is_some()
        {
            for reader in &mut instance.readers {
                reader.join();
            }
            return Err(launch_error(
                &instance.home,
                &tail(&instance.readers),
                api_key,
                "进程提前退出",
            ));
        }
        let text = tail(&instance.readers);
        // 安全检查：管理接口实际监听的地址必须与要求的一致（否则可能暴露在外网），不一致立即停止
        if let Some(actual) = super::home::listening_address(&text)
            && actual != address_plain
        {
            return Err(AppError::new("syncthing.gui_address_mismatch")
                .with("expected", &address_plain)
                .with("actual", actual));
        }
        if text.to_ascii_lowercase().contains("address already in use") {
            return Err(start_error(&text, &[api_key], "监听端口被占用"));
        }
        if text.contains("Failed to acquire lock") {
            return Err(launch_error(
                &instance.home,
                &text,
                api_key,
                "实例目录已被占用",
            ));
        }
        if Instant::now() >= deadline {
            return Err(start_error(
                &text,
                &[api_key],
                if home::listening_address(&text).is_none() {
                    "未见监听地址"
                } else {
                    "管理接口启动超时"
                },
            ));
        }
        if home::listening_address(&text).is_none() {
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        if instance
            .client
            .ping_with_timeout(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(500)),
            )
            .is_ok()
        {
            match instance.client.listeners_ready(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(500)),
            ) {
                Ok(true) => {}
                Err(e)
                    if e.code == "syncthing.listen_port_in_use"
                        || e.code == "syncthing.start_failed"
                        || e.code == "syncthing.listen_address_mismatch" =>
                {
                    return Err(e);
                }
                _ => {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
            }
            instance.identity = identity(pid, &bin.path, &instance.home, &instance.gui)
                .ok_or_else(|| start_error("", &[], "无法确认进程身份"))?;
            instance.group = matching(&bin.path, &instance.home, &instance.gui)?;
            // 同步监听失败可能不退出进程，须等连接子系统报告后再确认启动成功。
            if tail(&instance.readers)
                .to_ascii_lowercase()
                .contains("address already in use")
            {
                return Err(start_error(
                    &tail(&instance.readers),
                    &[api_key],
                    "监听端口被占用",
                ));
            }
            write_record(
                &instance.home,
                &Record {
                    pid,
                    identity: instance.identity.clone(),
                },
                &instance.group,
            )?;
            return Ok(instance);
        }
        thread::sleep(Duration::from_millis(50));
    }
}
fn write_record(home: &Path, primary: &Record, group: &[Record]) -> Result<(), AppError> {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "pid": primary.pid, "identity": primary.identity, "group": group
    }))
    .map_err(|_| io_error("编码 PID 文件"))?;
    home::atomic_write(&home.join("pierlink.pid"), &bytes)
}
fn launch_error(home: &Path, text: &str, key: &str, reason: &str) -> AppError {
    let mut e = start_error(text, &[key], reason);
    if e.code == "syncthing.home_locked"
        && let Some(pid) = lock_owner(home)
    {
        e = e.with("pid", pid);
    }
    e
}
#[cfg(target_os = "linux")]
fn lock_owner(home: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(home.join("syncthing.lock")).ok()?;
    let locks = fs::read_to_string("/proc/locks").ok()?;
    for line in locks.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 6 || fields[1] == "->" {
            continue;
        }
        let device: Vec<_> = fields[5].split(':').collect();
        if device.len() == 3
            && u64::from_str_radix(device[0], 16).ok()? == u64::from(libc::major(meta.dev()))
            && u64::from_str_radix(device[1], 16).ok()? == u64::from(libc::minor(meta.dev()))
            && device[2].parse::<u64>().ok()? == meta.ino()
        {
            return fields[4].parse().ok();
        }
    }
    None
}
#[cfg(not(target_os = "linux"))]
fn lock_owner(_: &Path) -> Option<u32> {
    None
}
fn verify_gui(text: &str, expected: &str) -> Result<bool, AppError> {
    match home::listening_address(text) {
        Some(actual) if actual != expected => Err(AppError::new("syncthing.gui_address_mismatch")
            .with("expected", expected)
            .with("actual", actual)),
        Some(_) => Ok(true),
        None => Ok(false),
    }
}
fn read_log(home: &Path) -> Result<String, AppError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match fs::File::open(home.join("syncthing.log")) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(_) => return Err(io_error("读取启动日志")),
    };
    let length = file.metadata().map_err(|_| io_error("读取日志大小"))?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(65536)))
        .map_err(|_| io_error("读取日志尾部"))?;
    let mut bytes = Vec::new();
    file.take(65536)
        .read_to_end(&mut bytes)
        .map_err(|_| io_error("读取启动日志"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
fn remove_pid(home: &Path) -> Result<(), AppError> {
    match fs::remove_file(home.join("pierlink.pid")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(io_error("清理 PID 文件")),
    }
}
impl RunningInstance {
    /// 无参数；返回该实例的管理接口客户端引用。
    pub fn client(&self) -> &Client {
        &self.client
    }
    /// 无参数；返回已确认身份的进程 PID，便于监测。
    pub fn pid(&self) -> u32 {
        self.pid
    }
    fn alive(&self) -> bool {
        self.group.iter().any(|r| {
            identity(r.pid, &self.bin.path, &self.home, &self.gui).as_ref() == Some(&r.identity)
        })
    }
    /// 消费实例句柄；先请求退出，最多等 10 秒后终止进程树；进程已退出也成功。
    pub fn stop(mut self) -> Result<(), AppError> {
        let deadline = Instant::now() + Duration::from_secs(10);
        if self.alive() {
            let _ = self.client.shutdown_with_timeout(Duration::from_secs(2));
        }
        while self.alive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        self.cleanup()
    }
    fn cleanup(&mut self) -> Result<(), AppError> {
        if self.stopped {
            return Ok(());
        }
        let child_result = if let Some(mut child) = self.child.take() {
            child.terminate().map_err(|_| io_error("终止进程树"))
        } else {
            Ok(())
        };
        // 监控进程退出前后，内层可能刚刚启动且自成进程组；重新按完整身份枚举。
        let group = matching(&self.bin.path, &self.home, &self.gui)?;
        let group_result = terminate_group(&self.bin.path, &self.home, &self.gui, &group);
        child_result?;
        group_result?;
        self.stopped = true;
        for reader in &mut self.readers {
            reader.join();
        }
        if let Some(r) = record(&self.home)?
            && r.pid == self.pid
            && r.identity == self.identity
        {
            remove_pid(&self.home)?;
        }
        Ok(())
    }
}
impl Drop for RunningInstance {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[cfg(target_os = "linux")]
    #[test]
    fn 接管不可达保留进程并允许强制停止() {
        let dir = crate::test_support::tempdir();
        let home = fs::canonicalize(dir.path()).unwrap();
        fs::write(home.join("config.xml"), "").unwrap();
        let bin = SyncthingBin {
            path: "/usr/bin/python3".into(),
        };
        let args = vec![
            "-c".into(),
            "import time; time.sleep(60)".into(),
            "serve".into(),
            "--no-restart".into(),
            format!("--home={}", home.display()),
            "--gui-address=127.0.0.1:1".into(),
        ];
        let mut child = spawn(&bin, args, None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while matching(&bin.path, &home, "127.0.0.1:1")
            .unwrap()
            .is_empty()
        {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        let result = start_once(
            &bin,
            &home,
            &GuiListen::Tcp { port: 1 },
            "key",
            Duration::from_millis(50),
        );
        let Err(e) = result else {
            panic!("不可达进程不应接管成功");
        };
        assert_eq!(e.code, "syncthing.adopt_failed");
        assert_eq!(e.params["pid"], child.child.id().to_string());
        assert!(child.try_wait().unwrap().is_none());
        let other = start(&bin, &home, &GuiListen::Tcp { port: 2 }, "key");
        let Err(e) = other else {
            panic!("已有实例占用 home");
        };
        assert_eq!(e.code, "syncthing.home_locked");
        force_stop(&home).unwrap();
        child.child.wait().unwrap();
    }
    // 用 python 脚本冒充程序，只在 Linux 上跑
    #[cfg(target_os = "linux")]
    #[test]
    fn 启动缺少监听日志或地址错误不写pid() {
        use std::os::unix::fs::PermissionsExt;
        for message in ["", "GUI and API listening (address=0.0.0.0:1)"] {
            let dir = crate::test_support::tempdir();
            let path = dir.path().join("fake");
            fs::write(&path, format!("#!/usr/bin/python3\nimport time\nprint({message:?}, flush=True)\ntime.sleep(60)\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(dir.path().join("config.xml"), "").unwrap();
            let result = start_once(
                &SyncthingBin { path },
                dir.path(),
                &GuiListen::Tcp { port: 1 },
                "key",
                Duration::from_millis(200),
            );
            let Err(e) = result else {
                panic!("监听证据无效却启动成功");
            };
            if message.is_empty() {
                assert_eq!(e.code, "syncthing.start_failed");
                assert_eq!(e.params["reason"], "未见监听地址");
            } else {
                assert_eq!(e.code, "syncthing.gui_address_mismatch");
            }
            assert!(!dir.path().join("pierlink.pid").exists());
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn 锁错误附真实占用pid() {
        use std::os::fd::AsRawFd;
        let dir = crate::test_support::tempdir();
        let file = fs::File::create(dir.path().join("syncthing.lock")).unwrap();
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
        assert_eq!(lock_owner(dir.path()), Some(std::process::id()));
        let e = launch_error(
            dir.path(),
            "Failed to acquire lock",
            "key",
            "实例目录已被占用",
        );
        assert_eq!(e.params["pid"], std::process::id().to_string());
    }
    #[test]
    fn 监听证据必须存在且完全匹配() {
        assert!(!verify_gui("尚未监听", "127.0.0.1:1").unwrap());
        assert!(verify_gui("GUI and API listening (address=127.0.0.1:1)", "127.0.0.1:1").unwrap());
        assert_eq!(
            verify_gui("GUI and API listening (address=0.0.0.0:1)", "127.0.0.1:1")
                .unwrap_err()
                .code,
            "syncthing.gui_address_mismatch"
        );
        let error = start_error("Failed to acquire lock (pid=123)", &[], "实例目录已被占用");
        assert_eq!(error.code, "syncthing.home_locked");
        assert_eq!(error.params["pid"], "123");
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn 无端口整组识别与孤儿清理() {
        let dir = crate::test_support::tempdir();
        let home = fs::canonicalize(dir.path()).unwrap();
        let bin = SyncthingBin {
            path: "/usr/bin/python3".into(),
        };
        let gui = "127.0.0.1:1";
        let script = "import os,sys,time; p=os.fork(); time.sleep(60)";
        let args = vec![
            "-c".into(),
            script.into(),
            "serve".into(),
            "--no-restart".into(),
            format!("--home={}", home.display()),
            format!("--gui-address={gui}"),
        ];
        let mut child = spawn(&bin, args, None).unwrap();
        let pid = child.child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        let group = loop {
            let group = matching(&bin.path, &home, gui).unwrap();
            if group.len() == 2 {
                break group;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        };
        assert!(
            matching(&bin.path, &home, "127.0.0.1:2")
                .unwrap()
                .is_empty()
        );
        for r in &group {
            assert_eq!(unsafe { libc::getpgid(r.pid as i32) }, pid as i32);
        }
        home::atomic_write(
            &home.join("pierlink-process.json"),
            &serde_json::to_vec(&(&bin.path, gui)).unwrap(),
        )
        .unwrap();
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
        child.child.wait().unwrap();
        assert_eq!(matching(&bin.path, &home, gui).unwrap().len(), 1);
        force_stop(&home).unwrap();
        assert!(matching(&bin.path, &home, gui).unwrap().is_empty());
        force_stop(&home).unwrap();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn 端口失败清理独立进程组内层_避免再次初始化busy() {
        let dir = crate::test_support::tempdir();
        let home = fs::canonicalize(dir.path()).unwrap();
        fs::write(home.join("config.xml"), "").unwrap();
        let script = dir.path().join("fake-syncthing");
        fs::write(&script, "#!/usr/bin/python3\nimport os,time\np=os.fork()\nif p == 0: os.setsid()\nelse: print('address already in use',flush=True)\ntime.sleep(60)\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        // 脚本解释器的 /proc/exe 是 Python，故显式用 Python 启动并保持 serve 参数。
        let bin = SyncthingBin {
            path: "/usr/bin/python3".into(),
        };
        let mut child = spawn(
            &bin,
            vec![
                script.display().to_string(),
                "serve".into(),
                "--no-restart".into(),
                format!("--home={}", home.display()),
                "--gui-address=127.0.0.1:1".into(),
            ],
            None,
        )
        .unwrap();
        let pid = child.child.id();
        let readers = readers(&mut child).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let group = matching(&bin.path, &home, "127.0.0.1:1").unwrap();
            if group.len() == 2
                && group
                    .iter()
                    .any(|r| unsafe { libc::getpgid(r.pid as i32) } != pid as i32)
            {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(record_is_running(&bin, &home, &GuiListen::Tcp { port: 1 }).unwrap());
        // 模拟启动失败尚未记录完整 group 的窗口；旧实现会遗留独立内层，导致 init_home 返回 busy。
        let instance = RunningInstance {
            pid,
            bin: bin.clone(),
            home: home.clone(),
            identity: String::new(),
            gui: "127.0.0.1:1".into(),
            group: vec![],
            client: Client::new("http://127.0.0.1:1", "key").unwrap(),
            child: Some(child),
            readers,
            stopped: false,
        };
        drop(instance);
        assert!(!record_is_running(&bin, &home, &GuiListen::Tcp { port: 1 }).unwrap());
        assert!(home::lock(&home).is_ok());
    }
    #[test]
    fn p6_启动日志分类脱敏和截断() {
        let e = start_error(
            "address already in use\nX-API-Key: key\npassword=口令",
            &["key", "口令"],
            "监听失败",
        );
        assert_eq!(e.code, "syncthing.listen_port_in_use");
        assert!(!format!("{e:?}").contains("口令"));
        let e = start_error(
            &format!("{}apikey=secret", "中".repeat(3000)),
            &["secret"],
            "进程失败",
        );
        assert_eq!(e.code, "syncthing.start_failed");
        assert_eq!(e.params["log_tail"].chars().count(), 2048);
        assert!(!e.params["log_tail"].contains("secret"));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn l1_无端口验证身份环境与释放() {
        let dir = crate::test_support::tempdir();
        let bin = SyncthingBin {
            path: "/usr/bin/python3".into(),
        };
        let home = fs::canonicalize(dir.path()).unwrap();
        let args = vec![
            "-c".into(),
            "import os,time; print(os.environ.get('STGUIAPIKEY'), flush=True); time.sleep(30)"
                .into(),
            "serve".into(),
            "--no-restart".into(),
            format!("--home={}", home.display()),
            "--gui-address=127.0.0.1:1".into(),
        ];
        let mut child = spawn(&bin, args, Some("test-api-secret")).unwrap();
        let pid = child.child.id();
        let readers = readers(&mut child).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let id = loop {
            if let Some(id) = identity(pid, &bin.path, &home, "127.0.0.1:1") {
                break id;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        };
        assert!(identity(pid, &bin.path, &home.join("other"), "127.0.0.1:1").is_none());
        assert!(identity(pid, Path::new("/bin/sleep"), &home, "127.0.0.1:1").is_none());
        assert!(identity(std::process::id(), &bin.path, &home, "127.0.0.1:1").is_none());
        let env = fs::read(format!("/proc/{pid}/environ")).unwrap();
        let env = String::from_utf8_lossy(&env);
        assert!(env.contains("STGUIAPIKEY=test-api-secret"));
        assert!(env.contains("STNOUPGRADE=1"));
        for key in [
            "SSH_AUTH_SOCK=",
            "SSH_AGENT_PID=",
            "KRB5CCNAME=",
            "HTTP_PROXY=",
            "STTRACE=",
        ] {
            assert!(!env.contains(key));
        }
        let args = fs::read(format!("/proc/{pid}/cmdline")).unwrap();
        assert!(!String::from_utf8_lossy(&args).contains("test-api-secret"));
        home::atomic_write(
            &home.join("pierlink.pid"),
            &serde_json::to_vec(&Record {
                pid,
                identity: id.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(record_is_running(&bin, &home, &GuiListen::Tcp { port: 1 }).unwrap());
        home::atomic_write(
            &home.join("pierlink.pid"),
            &serde_json::to_vec(&Record {
                pid,
                identity: "旧进程".into(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(record_is_running(&bin, &home, &GuiListen::Tcp { port: 1 }).unwrap());
        home::atomic_write(
            &home.join("pierlink.pid"),
            &serde_json::to_vec(&Record {
                pid,
                identity: id.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        let instance = RunningInstance {
            pid,
            bin,
            home: home.clone(),
            identity: id.clone(),
            gui: "127.0.0.1:1".into(),
            group: vec![Record { pid, identity: id }],
            client: Client::new("http://127.0.0.1:1", "key").unwrap(),
            child: Some(child),
            readers,
            stopped: false,
        };
        assert_eq!(instance.pid(), pid);
        drop(instance);
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        assert!(!home.join("pierlink.pid").exists());
    }
}
