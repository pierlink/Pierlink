//! 在进程内生成、加密和检查专用密钥；口令仅保存到凭据管理器。
use super::public::parse;
use crate::{
    error::AppError,
    security::{CredentialStore, random_secret},
};
use rand::RngExt;
use ssh_key::{Algorithm, Cipher, Kdf, LineEnding, PrivateKey};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

pub const CREDENTIAL_ID: &str = "pierlink/local/dedicated-key-passphrase";
#[derive(Debug, Clone)]
pub struct LocalKey {
    pub private_path: PathBuf,
    pub public_line: String,
    pub fingerprint: String,
    pub regenerated: bool,
}
fn failed(reason: &str) -> AppError {
    AppError::new("keys.local_failed").with("reason", reason)
}
fn io<T>(result: std::io::Result<T>) -> Result<T, AppError> {
    result.map_err(|_| failed("本机密钥文件操作失败"))
}
fn path_exists(path: &Path) -> Result<bool, AppError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(failed("无法检查密钥文件").with("path", path.display())),
    }
}

fn permissions(path: &Path, directory: bool) -> Result<(), AppError> {
    if io(fs::symlink_metadata(path))?.file_type().is_symlink() {
        return Err(failed("密钥路径不能是符号链接"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = io(fs::metadata(path))?;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(failed("密钥路径不属于当前用户"));
        }
        let mode = if directory { 0o700 } else { 0o600 };
        if metadata.permissions().mode() & 0o7777 != mode {
            io(fs::set_permissions(path, fs::Permissions::from_mode(mode)))?;
        }
    }
    #[cfg(windows)]
    super::windows::permissions(path, directory)?;
    Ok(())
}
struct Lock(PathBuf);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}
fn lock(dir: &Path) -> Result<Lock, AppError> {
    lock_with_timeout(dir, Duration::from_secs(10))
}
fn lock_with_timeout(dir: &Path, timeout: Duration) -> Result<Lock, AppError> {
    let path = dir.join(".pierlink-lock");
    let start = Instant::now();
    loop {
        match fs::create_dir(&path) {
            Ok(()) => return Ok(Lock(path)),
            // Windows 上别的线程正在删除锁目录时，建目录和读属性会短暂报“拒绝访问”：当作锁还被占着
            Err(e) if delete_pending(&e) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match fs::symlink_metadata(&path) {
                    Ok(metadata) => {
                        if metadata.is_dir()
                            && !metadata.file_type().is_symlink()
                            && io(metadata.modified())?
                                .elapsed()
                                .is_ok_and(|age| age > Duration::from_secs(600))
                        {
                            // 先移走再清理，避免清理旧锁时误删刚取得的新锁。
                            let stale =
                                Temporary(dir.join(format!(".stale-lock-{}", random_secret(8))));
                            match fs::rename(&path, &stale.0) {
                                Ok(()) => continue,
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                                Err(_) => {
                                    return Err(
                                        failed("无法清理陈旧密钥锁").with("path", path.display())
                                    );
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) if delete_pending(&e) => {}
                    Err(_) => return Err(failed("无法读取密钥锁").with("path", path.display())),
                }
            }
            Err(_) => {
                return Err(
                    failed("无法取得本机密钥锁；请检查未完成操作").with("path", path.display())
                );
            }
        }
        if start.elapsed() >= timeout {
            return Err(failed("无法取得本机密钥锁；请检查未完成操作").with("path", path.display()));
        }
        thread::sleep(Duration::from_millis(50));
    }
}
/// Windows 上目录处于“正在删除”状态时，访问它会返回拒绝访问；其他系统没有这种情况
fn delete_pending(e: &std::io::Error) -> bool {
    cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
enum Inspection {
    Missing,
    Damaged(&'static str),
    Ready(LocalKey),
}
fn inspect(dir: &Path, store: &dyn CredentialStore) -> Result<Inspection, AppError> {
    let private_path = dir.join("pierlink_ed25519");
    if !private_path
        .try_exists()
        .map_err(|_| failed("无法检查私钥"))?
    {
        return Ok(Inspection::Missing);
    }
    permissions(&private_path, false)?;
    let public_path = dir.join("pierlink_ed25519.pub");
    if !io(public_path.try_exists())? {
        return Ok(Inspection::Damaged("公钥文件缺失"));
    }
    permissions(&public_path, false)?;
    let Some(secret) = store
        .get(CREDENTIAL_ID)
        .map_err(|_| failed("无法读取密钥凭据"))?
    else {
        return Ok(Inspection::Damaged("密钥口令缺失"));
    };
    let secret = Zeroizing::new(secret);
    if secret.len() != 64 || !secret.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(failed("密钥口令格式无效"));
    }
    let public_line = io(fs::read_to_string(&public_path))?.trim().to_owned();
    let Some(public) = parse(&public_line) else {
        return Ok(Inspection::Damaged("公钥文件无效"));
    };
    let encoded = Zeroizing::new(io(fs::read_to_string(&private_path))?);
    let Ok(encrypted) = PrivateKey::from_openssh(encoded.as_bytes()) else {
        return Ok(Inspection::Damaged("私钥格式无效"));
    };
    if !encrypted.is_encrypted() {
        return Ok(Inspection::Damaged("私钥未加密"));
    }
    let Ok(private) = encrypted.decrypt(secret.as_bytes()) else {
        return Ok(Inspection::Damaged("私钥无法用口令解开"));
    };
    if private.algorithm() != Algorithm::Ed25519 {
        return Ok(Inspection::Damaged("专用密钥不是 ed25519"));
    }
    let derived_line = private
        .public_key()
        .to_openssh()
        .map_err(|_| failed("无法从私钥读取公钥"))?;
    let derived = parse(&derived_line).ok_or_else(|| failed("无法从私钥读取公钥"))?;
    if public.kind != derived.kind || public.data != derived.data {
        return Ok(Inspection::Damaged("公钥与私钥不一致"));
    }
    Ok(Inspection::Ready(LocalKey {
        private_path,
        fingerprint: public.fingerprint,
        public_line,
        regenerated: false,
    }))
}
/// 从 dir 检查并读取密钥；不存在返回 None，损坏返回错误，权限放宽时修复。
pub fn load_local_key(
    dir: &Path,
    store: &dyn CredentialStore,
) -> Result<Option<LocalKey>, AppError> {
    if !dir.try_exists().map_err(|_| failed("无法检查密钥目录"))? {
        return Ok(None);
    }
    permissions(dir, true)?;
    let _lock = lock(dir)?;
    match inspect(dir, store)? {
        Inspection::Missing => Ok(None),
        Inspection::Damaged(reason) => Err(failed(reason)),
        Inspection::Ready(key) => Ok(Some(key)),
    }
}
/// 清理 computer_name 为最长 32 位 ASCII 名称，并用 short_id 返回可识别的密钥注释。
pub fn key_comment(computer_name: &str, short_id: &str) -> Result<String, AppError> {
    if short_id.len() != 8
        || !short_id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(failed("电脑短 ID 无效"));
    }
    let name: String = computer_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(32)
        .collect();
    Ok(format!(
        "pierlink-{}-{short_id}",
        if name.is_empty() { "computer" } else { &name }
    ))
}
/// 保证 dir 中存在一把加密专用密钥；comment 为 key_comment 的结果，store 保存随机口令。
/// 损坏文件移入 old-时间目录，最多保留最近 3 组；regenerated 表示本次替换了旧文件。
/// 凭据读取、生成或权限检查失败直接返回错误，不归档现有密钥。
pub fn ensure_local_key(
    dir: &Path,
    comment: &str,
    store: &dyn CredentialStore,
) -> Result<LocalKey, AppError> {
    if !comment.starts_with("pierlink-")
        || comment.len() > 50
        || !comment
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        return Err(failed("密钥注释无效"));
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    io(builder.create(dir))?;
    permissions(dir, true)?;
    let _lock = lock(dir)?;
    if let Inspection::Ready(key) = inspect(dir, store)? {
        return Ok(key);
    }
    let regenerated = path_exists(&dir.join("pierlink_ed25519"))?
        || path_exists(&dir.join("pierlink_ed25519.pub"))?;
    let temporary = Temporary(dir.join(format!(".generate-{}", random_secret(8))));
    io(fs::create_dir(&temporary.0))?;
    permissions(&temporary.0, true)?;
    let path = temporary.0.join("pierlink_ed25519");
    let secret = Zeroizing::new(random_secret(32));
    let mut rng = rand::rng();
    let mut private =
        PrivateKey::random(&mut rng, Algorithm::Ed25519).map_err(|_| failed("密钥生成失败"))?;
    private.set_comment(comment);
    let public = private
        .public_key()
        .to_openssh()
        .map_err(|_| failed("公钥编码失败"))?;
    let encrypted = private
        .encrypt_with(
            Cipher::Aes256Ctr,
            Kdf::Bcrypt {
                salt: rng.random::<[u8; 16]>().to_vec(),
                rounds: 64,
            },
            rng.random(),
            secret.as_bytes(),
        )
        .map_err(|_| failed("私钥加密失败"))?;
    let encoded = encrypted
        .to_openssh(LineEnding::LF)
        .map_err(|_| failed("私钥编码失败"))?;
    write_key_file(&path, encoded.as_bytes())?;
    write_key_file(
        &path.with_extension("pub"),
        format!("{public}\n").as_bytes(),
    )?;
    permissions(&path, false)?;
    permissions(&path.with_extension("pub"), false)?;
    // 新密钥生成、权限检查和凭据写入完成后才归档现有文件。
    store
        .set(CREDENTIAL_ID, &secret)
        .map_err(|_| failed("无法保存密钥口令"))?;
    if regenerated {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| failed("系统时间无效"))?
            .as_millis();
        let archive = dir.join(format!("old-{time}-{}", random_secret(4)));
        io(fs::create_dir(&archive))?;
        permissions(&archive, true)?;
        for name in ["pierlink_ed25519", "pierlink_ed25519.pub"] {
            if path_exists(&dir.join(name))? {
                io(fs::rename(dir.join(name), archive.join(name)))?;
            }
        }
        let mut archives = io(fs::read_dir(dir))?
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name().to_str().is_some_and(|s| {
                    let parts: Vec<_> = s.split('-').collect();
                    parts.len() == 3
                        && parts[0] == "old"
                        && parts[1].len() >= 13
                        && parts[1].bytes().all(|b| b.is_ascii_digit())
                        && parts[2].len() == 8
                        && parts[2].bytes().all(|b| b.is_ascii_hexdigit())
                }) && e.file_type().is_ok_and(|t| t.is_dir())
            })
            .map(|e| e.path())
            .collect::<Vec<_>>();
        archives.sort();
        for old in archives.iter().take(archives.len().saturating_sub(3)) {
            io(fs::remove_dir_all(old))?;
        }
    }
    io(fs::rename(&path, dir.join("pierlink_ed25519")))?;
    io(fs::rename(
        path.with_extension("pub"),
        dir.join("pierlink_ed25519.pub"),
    ))?;
    let mut key = match inspect(dir, store)? {
        Inspection::Ready(key) => key,
        Inspection::Missing => return Err(failed("生成后私钥缺失")),
        Inspection::Damaged(reason) => return Err(failed(reason)),
    };
    key.regenerated = regenerated;
    Ok(key)
}

fn write_key_file(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = io(options.open(path))?;
    permissions(path, false)?;
    io(file.write_all(bytes))?;
    io(file.sync_all())
}

#[cfg(test)]
mod legacy;
#[cfg(test)]
mod tests;
