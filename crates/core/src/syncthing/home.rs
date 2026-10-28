//! 实例目录初始化；首次运行之前原子写入安全配置。
use super::{
    error, io_error,
    process::{SyncthingBin, run_short},
};
use crate::error::AppError;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub enum GuiListen {
    Tcp { port: u16 },
    Unix { path: PathBuf },
}
impl GuiListen {
    pub(crate) fn address(&self) -> Result<String, AppError> {
        match self {
            Self::Tcp { port } if *port > 0 => Ok(format!("127.0.0.1:{port}")),
            Self::Unix { path }
                if path
                    .to_str()
                    .is_some_and(|s| s.starts_with('/') && !s.contains('\0')) =>
            {
                let address = path.to_string_lossy();
                if address.len() >= 100 {
                    return Err(AppError::new("syncthing.socket_path_too_long")
                        .with("bytes", address.len()));
                }
                Ok(address.into_owned())
            }
            _ => Err(error("管理接口地址无效")),
        }
    }

    /// 命令行 `--gui-address` 用的写法。注意与配置文件不同：Unix 套接字在命令行上必须带
    /// `unix://` 前缀，只写路径时 Syncthing 会改为在**所有网卡**的随机 TCP 端口上监听。
    pub(crate) fn cli_address(&self) -> Result<String, AppError> {
        let address = self.address()?;
        Ok(match self {
            Self::Tcp { .. } => address,
            Self::Unix { .. } => format!("unix://{address}"),
        })
    }
}

/// 从 Syncthing 日志里取出管理接口实际监听的地址（`GUI and API listening (address=…)`）。
pub(crate) fn listening_address(log: &str) -> Option<String> {
    const KEY: &str = "GUI and API listening (address=";
    let rest = &log[log.rfind(KEY)? + KEY.len()..];
    if let Some(quoted) = rest.strip_prefix('"') {
        return quoted.split('"').next().map(str::to_owned);
    }
    rest.split([' ', ')', '\n']).next().map(str::to_owned)
}
#[derive(Debug, Clone)]
pub struct HomeSpec {
    pub home: PathBuf,
    pub gui: GuiListen,
    pub gui_user: String,
    pub listen_port: u16,
}
#[derive(Debug, Clone)]
pub struct HomeInfo {
    pub device_id: String,
}

pub(crate) fn private_dir(path: &Path) -> Result<(), AppError> {
    if path.exists() {
        let meta = fs::symlink_metadata(path).map_err(|_| io_error("读取目录权限"))?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(AppError::new("syncthing.socket_dir_insecure"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if meta.permissions().mode() & 0o777 != 0o700
                || meta.uid() != unsafe { libc::geteuid() }
            {
                return Err(AppError::new("syncthing.socket_dir_insecure"));
            }
        }
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|_| io_error("创建实例目录"))?;
    // 再次核验，避免目录恰好被其他创建者抢先建立。
    private_dir(path)?;

    Ok(())
}
pub(crate) fn socket_dir(path: &Path) -> Result<(), AppError> {
    let existed = path.symlink_metadata().is_ok();
    private_dir(path)?;
    let marker = path.join(".pierlink-socket-dir");
    if existed {
        if fs::read(&marker).ok().as_deref() != Some(b"Pierlink") {
            return Err(AppError::new("syncthing.socket_dir_insecure"));
        }
    } else {
        atomic_write(&marker, b"Pierlink")?;
    }
    Ok(())
}
pub(crate) fn private_file(path: &Path) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| io_error("设置文件权限"))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| error("文件路径没有父目录"))?;
    let temp = Temporary(parent.join(format!(".pierlink-{}.tmp", uuid::Uuid::new_v4())));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp.0)
        .map_err(|_| io_error("创建原子写入文件"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| io_error("写入配置文件"))?;
    drop(file);
    #[cfg(not(windows))]
    fs::rename(&temp.0, path).map_err(|_| io_error("替换配置文件"))?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        let from: Vec<_> = temp.0.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(io_error("替换配置文件"));
        }
    }
    Ok(())
}
#[derive(Debug)]
pub(crate) struct HomeLock(fs::File);
impl Drop for HomeLock {
    fn drop(&mut self) {
        // fork 后 exec 前子进程仍持有同一打开文件描述；必须显式解锁，不能只依靠 close。
        let _ = self.0.unlock();
    }
}
pub(crate) fn lock(home: &Path) -> Result<HomeLock, AppError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(home.join("pierlink.lock"))
        .map_err(|_| io_error("打开实例锁"))?;
    file.try_lock()
        .map_err(|_| AppError::new("syncthing.busy").with("reason", "实例目录锁正在使用"))?;
    Ok(HomeLock(file))
}
/// bin 为可信程序，spec 为实例路径与端口；每次经 stdin 更新 GUI 口令并加固配置，返回设备 ID。
pub fn init_home(
    bin: &SyncthingBin,
    spec: &HomeSpec,
    gui_password: &str,
) -> Result<HomeInfo, AppError> {
    if !spec.home.is_absolute()
        || spec.listen_port == 0
        || spec.gui_user.is_empty()
        || gui_password.is_empty()
        || gui_password.len() > 4096
        || gui_password.contains(['\0', '\n', '\r'])
    {
        return Err(error("实例路径、端口、用户名或口令无效"));
    }
    spec.gui.address()?;
    private_dir(&spec.home)?;
    let _lock = lock(&spec.home)?;
    if super::process::record_is_running(
        bin,
        &crate::fs_local::plain_canonical(&spec.home).map_err(|_| io_error("解析实例目录"))?,
        &spec.gui,
    )? {
        return Err(AppError::new("syncthing.busy").with("reason", "匹配的实例进程仍在运行"));
    }
    if let GuiListen::Unix { path } = &spec.gui {
        socket_dir(path.parent().ok_or_else(|| error("套接字缺少父目录"))?)?;
    }
    let config = spec.home.join("config.xml");
    if config.try_exists().map_err(|_| io_error("检查配置文件"))? {
        super::xml::validate_source(&read_config(&config)?)?;
    }
    {
        let home = spec
            .home
            .to_str()
            .ok_or_else(|| error("实例路径不是有效文本"))?;
        run_short(
            bin,
            vec![
                "generate".into(),
                format!("--home={home}"),
                "--no-port-probing".into(),
                format!("--gui-user={}", spec.gui_user),
                "--gui-password=-".into(),
            ],
            Some(gui_password),
        )?;
    }
    private_file(&config)?;
    let input = read_config(&config)?;
    let output = super::xml::harden(&input, spec)?;
    atomic_write(&config, &output)?;
    let device_id =
        read_device_id(bin, &spec.home)?.ok_or_else(|| error("生成配置后没有设备证书"))?;
    Ok(HomeInfo { device_id })
}
fn read_config(path: &Path) -> Result<Vec<u8>, AppError> {
    let mut input = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(16 * 1024 * 1024 + 1).read_to_end(&mut input))
        .map_err(|_| io_error("读取配置文件"))?;
    if input.len() > 16 * 1024 * 1024 {
        return Err(error("配置文件过大"));
    }
    Ok(input)
}
/// 读取 home 中 cert.pem 的 DER 摘要并编码设备 ID，不启动进程；bin 保留接口兼容，证书缺失返回 None。
pub fn read_device_id(_bin: &SyncthingBin, home: &Path) -> Result<Option<String>, AppError> {
    use sha2::{Digest, Sha256};
    let file = match fs::File::open(home.join("cert.pem")) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(io_error("读取设备证书")),
    };
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| io_error("读取设备证书"))?;
    if bytes.len() > 1024 * 1024 {
        return Err(error("设备证书过大"));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| error("设备证书格式无效"))?;
    let text = text
        .trim()
        .strip_prefix("-----BEGIN CERTIFICATE-----")
        .and_then(|s| s.strip_suffix("-----END CERTIFICATE-----"))
        .ok_or_else(|| error("设备证书格式无效"))?;
    let der = decode_certificate(text)?;
    let hash = Sha256::digest(&der);
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut base32 = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for b in hash {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            base32.push(ALPHABET[((acc >> bits) & 31) as usize]);
        }
    }
    if bits > 0 {
        base32.push(ALPHABET[((acc << (5 - bits)) & 31) as usize]);
    }
    let mut checked = Vec::new();
    for part in base32.chunks(13) {
        checked.extend_from_slice(part);
        let mut sum = 0;
        for (index, b) in part.iter().enumerate() {
            let value = ALPHABET
                .iter()
                .position(|v| v == b)
                .ok_or_else(|| error("设备 ID 编码失败"))?;
            let value = value * (1 + index % 2);
            sum += value / 32 + value % 32;
        }
        checked.push(ALPHABET[(32 - sum % 32) % 32]);
    }
    let id = checked
        .chunks(7)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect::<Vec<_>>()
        .join("-");
    Ok(Some(id))
}
fn decode_certificate(text: &str) -> Result<Vec<u8>, AppError> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<_> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return Err(error("设备证书 Base64 无效"));
    }
    let mut out = Vec::new();
    for (index, group) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let mut values = [0u8; 4];
        let padding = group.iter().rev().take_while(|b| **b == b'=').count();
        if padding > 2 || (padding > 0 && (index + 1) * 4 != bytes.len()) {
            return Err(error("设备证书 Base64 无效"));
        }
        for i in 0..4 - padding {
            values[i] = ALPHABET
                .iter()
                .position(|v| *v == group[i])
                .ok_or_else(|| error("设备证书 Base64 无效"))? as u8;
        }
        if (padding == 2 && values[1] & 15 != 0) || (padding == 1 && values[2] & 3 != 0) {
            return Err(error("设备证书 Base64 无效"));
        }
        out.push(values[0] << 2 | values[1] >> 4);
        if padding < 2 {
            out.push(values[1] << 4 | values[2] >> 2);
        }
        if padding == 0 {
            out.push(values[2] << 6 | values[3]);
        }
    }
    let invalid = || error("设备证书 DER 无效");
    if out.first() != Some(&0x30) {
        return Err(invalid());
    }
    let first_length = *out.get(1).ok_or_else(invalid)?;
    let (header, length) = if first_length < 128 {
        (2, usize::from(first_length))
    } else {
        let count = usize::from(first_length & 127);
        if count == 0 || count > 4 || out.len() < 2 + count {
            return Err(invalid());
        }
        let length = out[2..2 + count]
            .iter()
            .fold(0usize, |n, b| (n << 8) | usize::from(*b));
        (2 + count, length)
    };
    if length == 0 || header + length != out.len() {
        return Err(invalid());
    }
    Ok(out)
}

/// 远程配置加固参数；API 密钥仅用于服务器端持久化，不实现 Debug。
pub struct HardenSpec<'a> {
    pub home: &'a HomeSpec,
    pub api_key: &'a str,
}
/// 将 xml 按 spec 加固为独立配置文本；不读写文件，失败不回显原文或凭据。
pub fn harden_config_xml(xml: &str, spec: &HardenSpec<'_>) -> Result<String, AppError> {
    if xml.len() > 16 * 1024 * 1024
        || spec.home.listen_port == 0
        || spec.home.gui_user.is_empty()
        || spec.api_key.len() > 4096
        || spec.api_key.contains(['\0', '\n', '\r'])
    {
        return Err(error("远程配置参数无效"));
    }
    String::from_utf8(super::xml::harden_with_key(
        xml.as_bytes(),
        spec.home,
        spec.api_key,
    )?)
    .map_err(|_| error("配置编码无效"))
}

#[cfg(test)]
mod listen_tests {
    use super::*;

    #[test]
    fn 命令行地址与日志解析() {
        let unix = GuiListen::Unix {
            path: "/run/u/pl/g".into(),
        };
        assert_eq!(unix.cli_address().unwrap(), "unix:///run/u/pl/g");
        assert_eq!(unix.address().unwrap(), "/run/u/pl/g");
        let tcp = GuiListen::Tcp { port: 38411 };
        assert_eq!(tcp.cli_address().unwrap(), "127.0.0.1:38411");
        let log = "x INF GUI and API listening (address=\"[::]:44471\" log.pkg=api)\n";
        assert_eq!(listening_address(log).as_deref(), Some("[::]:44471"));
        let log = "x INF GUI and API listening (address=/run/u/pl/g log.pkg=api)\n";
        assert_eq!(listening_address(log).as_deref(), Some("/run/u/pl/g"));
        let log = "x INF GUI and API listening (address=127.0.0.1:38411 log.pkg=api)\n";
        assert_eq!(listening_address(log).as_deref(), Some("127.0.0.1:38411"));
        assert_eq!(listening_address("没有这一行"), None);
    }
}

#[cfg(all(test, unix))]
mod lock_tests {
    use super::*;
    #[test]
    fn 复制描述符存活时仍显式释放锁() {
        let dir = crate::test_support::tempdir();
        let guard = lock(dir.path()).unwrap();
        // dup 和 fork 都共享同一打开文件描述；保留副本精确模拟 exec 前的继承窗口。
        let inherited = guard.0.try_clone().unwrap();
        assert_eq!(lock(dir.path()).unwrap_err().code, "syncthing.busy");
        drop(guard);
        let next = lock(dir.path()).unwrap();
        drop(inherited);
        assert_eq!(lock(dir.path()).unwrap_err().code, "syncthing.busy");
        drop(next);
        assert!(lock(dir.path()).is_ok());
    }
}
