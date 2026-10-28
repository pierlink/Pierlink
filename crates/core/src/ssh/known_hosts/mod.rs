//! 内置 SSH 的用户 known_hosts 核对；不读取系统文件，不信任数据库中的展示用指纹。
mod patterns;
#[cfg(test)]
mod tests;

use crate::error::AppError;
use pierlink_ssh_engine::HostKey;
use ssh_key::{HashAlg, PublicKey};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

// 限制不可信行与公钥的内存占用；超长行整体跳过，不把剩余部分当成另一条记录。
const MAX_LINE: usize = 64 * 1024;

/// 核对结果；新主机需要显式确认，其他类型用于提示此前已有不同类型的密钥。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    Trusted,
    New { other_key_types: Vec<String> },
}

/// 本次读取的核对结果和跳过的坏行数，不包含原始文件或公钥内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyReport {
    pub status: HostKeyStatus,
    pub invalid_lines: usize,
}

/// 仅首个文件可写；第二个文件只读。测试必须显式指定临时路径。
#[derive(Clone, Debug)]
pub struct KnownHosts {
    primary: PathBuf,
    secondary: Option<PathBuf>,
}

struct Record {
    hosts: patterns::Patterns,
    marker: Marker,
    key: PublicKey,
    line: usize,
    file: PathBuf,
}

#[derive(PartialEq, Eq)]
enum Marker {
    Normal,
    Revoked,
    CertAuthority,
}

#[derive(Default)]
struct Records {
    entries: Vec<Record>,
    invalid_lines: usize,
}

impl KnownHosts {
    /// 用 primary 作为追加文件，secondary 作为可选只读文件；返回不访问磁盘的核对器。
    pub fn new(primary: PathBuf, secondary: Option<PathBuf>) -> Self {
        Self { primary, secondary }
    }

    /// 无参数；返回用户默认两个 known_hosts 路径，缺少用户目录时返回错误，不创建文件。
    pub fn for_user() -> Result<Self, AppError> {
        #[cfg(windows)]
        let variable = "USERPROFILE";
        #[cfg(not(windows))]
        let variable = "HOME";
        let home = std::env::var_os(variable)
            .filter(|home| !home.is_empty())
            .ok_or_else(|| failure("无法确定用户目录"))?;
        let ssh = PathBuf::from(home).join(".ssh");
        Ok(Self::new(
            ssh.join("known_hosts"),
            Some(ssh.join("known_hosts2")),
        ))
    }

    /// 按本级 host、port 或替代二者的 alias 核对 key；名称按 OpenSSH 规则转小写。
    /// 撤销和同类型密钥变化均返回 ssh.host_key_changed；仅其他类型需要首次确认。
    pub fn check(&self, key: &HostKey, alias: Option<&str>) -> Result<HostKeyReport, AppError> {
        let name = record_name(&key.host, key.port, alias)?;
        let public = validate_key(key)?;
        self.read(None)?.check(&name, &public)
    }

    /// 按本级地址或 alias 读取已知公钥类型，返回供引擎 preferred_host_key_algorithms 使用的列表。
    /// 不包含 CA、撤销或不匹配的记录，也不创建目录或文件。
    pub fn preferred_algorithms(
        &self,
        host: &str,
        port: u16,
        alias: Option<&str>,
    ) -> Result<Vec<String>, AppError> {
        let name = record_name(host, port, alias)?;
        let mut algorithms = Vec::new();
        for entry in self.read(None)?.entries {
            if entry.marker == Marker::Normal && entry.hosts.matches(&name) {
                let algorithm = entry.key.algorithm().to_string();
                if !algorithms.contains(&algorithm) {
                    algorithms.push(algorithm);
                }
            }
        }
        Ok(algorithms)
    }

    /// 按 key 和 alias 核对；新主机调用 confirm(key, report)，同意后追加，返回是否放行。
    /// cancelled 置位或界面拒绝返回 false；界面须自行响应取消，等待期间不持文件锁。
    /// 可直接用于 ConnectionCallbacks::check_host_key，每级跳板传入自己的 key 和 alias。
    pub fn verify(
        &self,
        key: &HostKey,
        alias: Option<&str>,
        cancelled: &AtomicBool,
        confirm: impl FnOnce(&HostKey, &HostKeyReport) -> Result<bool, AppError>,
    ) -> Result<bool, AppError> {
        if cancelled.load(Ordering::Acquire) {
            return Ok(false);
        }
        let report = self.check(key, alias)?;
        if report.status == HostKeyStatus::Trusted {
            return Ok(!cancelled.load(Ordering::Acquire));
        }
        if cancelled.load(Ordering::Acquire)
            || !confirm(key, &report)?
            || cancelled.load(Ordering::Acquire)
        {
            return Ok(false);
        }
        self.append_confirmed(key, alias, cancelled)
    }

    fn append_confirmed(
        &self,
        key: &HostKey,
        alias: Option<&str>,
        cancelled: &AtomicBool,
    ) -> Result<bool, AppError> {
        let name = record_name(&key.host, key.port, alias)?;
        let public = validate_key(key)?;
        self.create_parent()?;
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&self.primary)
            .map_err(|e| io_failure("打开追加文件失败", e))?;
        // 系统文件锁随 File 释放；同进程和跨进程的本模块写入都不会交叉。
        // 用非阻塞锁轮询，使连接取消不被另一个正在写入的进程阻塞。
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Ok(false);
            }
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(io_failure("锁定追加文件失败", e));
                }
            }
        }
        // 使用同一文件句柄读取主文件，兼容 Windows 文件锁；确认等待期间新增的撤销也生效。
        if self.read(Some(&mut file))?.check(&name, &public)?.status == HostKeyStatus::Trusted {
            return Ok(!cancelled.load(Ordering::Acquire));
        }
        let length = file
            .metadata()
            .map_err(|e| io_failure("读取文件长度失败", e))?
            .len();
        let mut line = String::new();
        if length > 0 {
            file.seek(SeekFrom::End(-1))
                .map_err(|e| io_failure("读取文件末尾失败", e))?;
            let mut last = [0];
            file.read_exact(&mut last)
                .map_err(|e| io_failure("读取文件末尾失败", e))?;
            if last != *b"\n" {
                line.push('\n');
            }
        }
        line.push_str(&format!("{name} {} {}\n", key.algorithm, key.public_key));
        if cancelled.load(Ordering::Acquire) {
            return Ok(false);
        }
        file.write_all(line.as_bytes())
            .map_err(|e| io_failure("追加主机公钥失败", e))?;
        file.sync_data()
            .map_err(|e| io_failure("保存主机公钥失败", e))?;
        Ok(true)
    }

    fn create_parent(&self) -> Result<(), AppError> {
        if let Some(parent) = self.primary.parent().filter(|p| !p.as_os_str().is_empty()) {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(parent)
                .map_err(|e| io_failure("创建主机记录目录失败", e))?;
        }
        Ok(())
    }

    fn read(&self, primary: Option<&mut File>) -> Result<Records, AppError> {
        let mut records = Records::default();
        if let Some(file) = primary {
            file.seek(SeekFrom::Start(0))
                .map_err(|e| io_failure("读取主机记录失败", e))?;
            records.read(BufReader::new(file), &self.primary)?;
        } else {
            records.read_path(&self.primary)?;
        }
        if let Some(path) = &self.secondary {
            records.read_path(path)?;
        }
        Ok(records)
    }
}

impl Records {
    fn read_path(&mut self, path: &Path) -> Result<(), AppError> {
        match File::open(path) {
            Ok(file) => self.read(BufReader::new(file), path),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_failure("读取主机记录失败", e)),
        }
    }

    fn read(&mut self, mut reader: impl BufRead, path: &Path) -> Result<(), AppError> {
        let mut bytes = Vec::new();
        let mut line = 0;
        loop {
            bytes.clear();
            let count = reader
                .by_ref()
                .take((MAX_LINE + 1) as u64)
                .read_until(b'\n', &mut bytes)
                .map_err(|e| io_failure("读取主机记录失败", e))?;
            if count == 0 {
                return Ok(());
            }
            line += 1;
            if bytes.len() > MAX_LINE {
                if bytes.last() != Some(&b'\n') {
                    reader
                        .skip_until(b'\n')
                        .map_err(|e| io_failure("跳过超长主机记录失败", e))?;
                }
                self.invalid_lines += 1;
                continue;
            }
            match parse_line(&bytes, path, line) {
                Ok(Some(record)) => self.entries.push(record),
                Ok(None) => (),
                Err(()) => self.invalid_lines += 1,
            }
        }
    }

    fn check(&self, name: &str, key: &PublicKey) -> Result<HostKeyReport, AppError> {
        let mut trusted = false;
        let mut changed = None;
        let mut other_key_types = Vec::new();
        for entry in &self.entries {
            if entry.marker == Marker::CertAuthority || !entry.hosts.matches(name) {
                continue;
            }
            if entry.marker == Marker::Revoked {
                if entry.key.key_data() == key.key_data() {
                    return Err(key_changed(
                        name,
                        key,
                        entry,
                        "主机公钥已被撤销",
                        self.invalid_lines,
                    ));
                }
                continue;
            }
            if entry.key.key_data() == key.key_data() {
                trusted = true;
            } else if entry.key.algorithm() == key.algorithm() {
                changed = Some(entry);
            } else {
                let algorithm = entry.key.algorithm().to_string();
                if !other_key_types.contains(&algorithm) {
                    other_key_types.push(algorithm);
                }
            }
        }
        // 和 OpenSSH 一样，同类型存在多行时，只要有相同的有效密钥就认得；撤销始终优先。
        if !trusted && let Some(entry) = changed {
            return Err(key_changed(
                name,
                key,
                entry,
                "同类型主机公钥发生变化",
                self.invalid_lines,
            ));
        }
        Ok(HostKeyReport {
            status: if trusted {
                HostKeyStatus::Trusted
            } else {
                HostKeyStatus::New { other_key_types }
            },
            invalid_lines: self.invalid_lines,
        })
    }
}

fn parse_line(bytes: &[u8], path: &Path, line: usize) -> Result<Option<Record>, ()> {
    let text = std::str::from_utf8(bytes).map_err(|_| ())?.trim();
    if text.is_empty() || text.starts_with('#') {
        return Ok(None);
    }
    let mut fields = text.split_ascii_whitespace();
    let first = fields.next().ok_or(())?;
    let (marker, hosts) = match first {
        "@revoked" => (Marker::Revoked, fields.next().ok_or(())?),
        "@cert-authority" => (Marker::CertAuthority, fields.next().ok_or(())?),
        value if value.starts_with('@') => return Err(()),
        value => (Marker::Normal, value),
    };
    let algorithm = fields.next().ok_or(())?;
    let base64 = fields.next().ok_or(())?;
    let key = PublicKey::from_openssh(&format!("{algorithm} {base64}")).map_err(|_| ())?;
    Ok(Some(Record {
        hosts: patterns::Patterns::parse(hosts)?,
        marker,
        key,
        line,
        file: path.to_path_buf(),
    }))
}

fn record_name(host: &str, port: u16, alias: Option<&str>) -> Result<String, AppError> {
    let value = alias.unwrap_or(host);
    if value.is_empty()
        || value.len() > 1024
        || port == 0
        || value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || ",!*?|#@".contains(c))
    {
        return Err(
            AppError::new("ssh.invalid_host").with("reason", "主机名或别名不能写入主机记录")
        );
    }
    // OpenSSH 会将 HostKeyAlias 转小写并代替完整查找名称，不能再加端口。
    Ok(if alias.is_some() || port == 22 {
        value.to_owned()
    } else {
        format!("[{host}]:{port}")
    }
    .to_ascii_lowercase())
}

fn validate_key(key: &HostKey) -> Result<PublicKey, AppError> {
    let invalid =
        || AppError::new("ssh.invalid_host_key").with("reason", "服务器公钥格式或指纹无效");
    if key.public_key.len() > MAX_LINE / 2
        || key.algorithm.len() > 128
        || key.public_key.bytes().any(|b| b.is_ascii_whitespace())
        || key.algorithm.bytes().any(|b| b.is_ascii_whitespace())
    {
        return Err(invalid());
    }
    let public = PublicKey::from_openssh(&format!("{} {}", key.algorithm, key.public_key))
        .map_err(|_| invalid())?;
    if public.algorithm().to_string() != key.algorithm
        || public.fingerprint(HashAlg::Sha256).to_string() != key.fingerprint
    {
        return Err(invalid());
    }
    Ok(public)
}

fn key_changed(
    name: &str,
    key: &PublicKey,
    record: &Record,
    reason: &str,
    invalid_lines: usize,
) -> AppError {
    AppError::new("ssh.host_key_changed")
        .with("host", name)
        .with("reason", reason)
        .with("fingerprint", key.fingerprint(HashAlg::Sha256))
        .with("known_fingerprint", record.key.fingerprint(HashAlg::Sha256))
        .with("file", record.file.display())
        .with("line", record.line)
        .with("invalid_lines", invalid_lines)
}

fn failure(reason: &str) -> AppError {
    AppError::new("ssh.known_hosts_failed").with("reason", reason)
}

fn io_failure(reason: &str, error: io::Error) -> AppError {
    // 不回传原始底层错误文本，避免不可信内容进入界面或日志。
    failure(reason).with("io_kind", format!("{:?}", error.kind()))
}
