//! 导入的私钥：导入时选一次私钥文件，统一转成 OpenSSH 格式、加密后存进
//! Pierlink 的数据目录（目录和文件都只让当前用户读写），服务器的“指定私钥”从列表里选。
//!
//! 原文件没有口令的，用随机口令加密，口令放进凭据管理器（和专用密钥一样，光拷走文件打不开，连接时自动填）；
//! 原文件有口令的，照旧用这个口令，用户可以选择把口令存进凭据管理器，不存就每次运行第一次连接时问。
//! 每把私钥两个文件：`<编号>.key`（加密的私钥，ssh -i 直接能用）和 `<编号>.json`（名字、算法、指纹）。
use super::local::permissions;
use crate::{
    error::AppError,
    security::{CredentialStore, random_secret},
};
use pierlink_ssh_engine::{KeyImportError, decode_private_key};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use ssh_key::{Cipher, HashAlg, Kdf, LineEnding, PrivateKey};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

/// 存导入私钥的子目录（在专用密钥的目录下）
pub const DIR_NAME: &str = "imported";
/// 凭据管理器里口令条目的前缀，后面接私钥编号
const CREDENTIAL_PREFIX: &str = "pierlink/imported-key/";
/// 私钥文件最大多大（再大就不是私钥了）
const MAX_SOURCE_BYTES: u64 = 64 * 1024;
/// 名字最长多少个字
const MAX_NAME_CHARS: usize = 60;
/// 加密时 bcrypt 的轮数（和 ssh-keygen 默认一样；每次连接都要解一次，不宜太慢）
const KDF_ROUNDS: u32 = 16;

/// 存在 `<编号>.json` 里的信息
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    name: String,
    algorithm: String,
    fingerprint: String,
    comment: String,
    /// 原文件有口令（用户自己的口令）；没有的话口令是随机生成的、一定存着
    user_passphrase: bool,
    created_ms: i64,
}

/// 一把导入的私钥
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedKey {
    pub id: String,
    pub name: String,
    /// 算法，如 ssh-ed25519、ssh-rsa
    pub algorithm: String,
    /// 公钥指纹（SHA256:…）
    pub fingerprint: String,
    /// 私钥里带的注释（可能为空）
    pub comment: String,
    /// 原文件有口令（用户自己的口令）
    pub user_passphrase: bool,
    /// 口令存在凭据管理器里，连接时自动填
    pub passphrase_saved: bool,
    pub created_ms: i64,
    /// 加密私钥的完整路径（用这把私钥的服务器，identity_file 填它）
    pub path: PathBuf,
}

fn failed(text_key: &'static str, reason: &str) -> AppError {
    AppError::new("keys.imported_failed").with_reason(text_key, reason)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn valid_id(id: &str) -> bool {
    (8..=40).contains(&id.len()) && id.bytes().all(|c| c.is_ascii_alphanumeric())
}

/// 凭据管理器里这把私钥口令的条目名。
///
/// :param id: 私钥编号
/// :return: 条目名
pub fn credential_id(id: &str) -> String {
    format!("{CREDENTIAL_PREFIX}{id}")
}

/// 是不是导入私钥的口令条目（ssh 自动填口令时只放行专用密钥和这些，服务器密码等一律不给）。
///
/// :param name: 凭据条目名
/// :return: 是导入私钥的口令条目
pub fn is_credential_id(name: &str) -> bool {
    name.strip_prefix(CREDENTIAL_PREFIX).is_some_and(valid_id)
}

fn key_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.key"))
}

fn meta_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// 这个路径是不是某把导入的私钥。
///
/// :param dir: 存导入私钥的目录
/// :param path: 服务器 identity_file 里的路径
/// :return: 是的话返回私钥编号
pub fn id_of_path(dir: &Path, path: &Path) -> Option<String> {
    let id = path.file_name()?.to_str()?.strip_suffix(".key")?;
    if !valid_id(id) {
        return None;
    }
    let parent = path.parent()?;
    // 能规范化就比规范化后的路径（大小写、斜杠、\\?\ 前缀都对得上），不能就按原样比
    let same = match (fs::canonicalize(parent), fs::canonicalize(dir)) {
        (Ok(a), Ok(b)) => a == b,
        _ => parent == dir,
    };
    same.then(|| id.to_owned())
}

fn read_meta(dir: &Path, id: &str) -> Result<Meta, AppError> {
    let bytes = fs::read(meta_path(dir, id))
        .map_err(|_| failed("keys.imported_read_failed", "读取私钥信息失败"))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| failed("keys.imported_read_failed", "读取私钥信息失败"))
}

/// 建好存私钥的目录，只让当前用户读写
fn ensure_dir(dir: &Path) -> Result<(), AppError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|_| failed("keys.imported_write_failed", "保存私钥失败"))?;
    permissions(dir, true)
}

/// 写一个只让当前用户读写的文件：先写同目录的临时文件、收紧权限，再改名，写到一半不会留下坏文件
fn write_private(path: &Path, data: &[u8]) -> Result<(), AppError> {
    let write_failed = || failed("keys.imported_write_failed", "保存私钥失败");
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(write_failed)?;
    let temporary = path.with_file_name(format!(".{name}.{}", random_secret(6)));
    let result = (|| {
        fs::write(&temporary, data).map_err(|_| write_failed())?;
        permissions(&temporary, false)?;
        fs::rename(&temporary, path).map_err(|_| write_failed())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn to_imported(dir: &Path, id: &str, meta: Meta, passphrase_saved: bool) -> ImportedKey {
    ImportedKey {
        id: id.to_owned(),
        name: meta.name,
        algorithm: meta.algorithm,
        fingerprint: meta.fingerprint,
        comment: meta.comment,
        user_passphrase: meta.user_passphrase,
        passphrase_saved,
        created_ms: meta.created_ms,
        path: key_path(dir, id),
    }
}

/// 列出导入的私钥（按导入时间）。
///
/// :param dir: 存导入私钥的目录（还没有时返回空列表）
/// :param store: 凭据管理器（看口令存没存着）
/// :return: 私钥列表
pub fn list(dir: &Path, store: &dyn CredentialStore) -> Result<Vec<ImportedKey>, AppError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(_) => return Err(failed("keys.imported_read_failed", "读取私钥信息失败")),
    };
    let mut keys = vec![];
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(id) = file_name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        if !valid_id(id) || !key_path(dir, id).is_file() {
            continue;
        }
        // 某一把的信息坏了不影响列出别的
        let Ok(meta) = read_meta(dir, id) else {
            continue;
        };
        let saved = store.get(&credential_id(id))?.is_some();
        keys.push(to_imported(dir, id, meta, saved));
    }
    keys.sort_by(|a, b| {
        a.created_ms
            .cmp(&b.created_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(keys)
}

/// 读出私钥文件，挡掉明显不是私钥的（太大、不是文字、公钥）
fn read_source(source: &Path) -> Result<Zeroizing<String>, AppError> {
    let unreadable = || AppError::new("keys.imported_unreadable").with("path", source.display());
    let metadata = fs::metadata(source).map_err(|_| unreadable())?;
    if !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES {
        return Err(AppError::new("keys.imported_unsupported"));
    }
    let bytes = Zeroizing::new(fs::read(source).map_err(|_| unreadable())?);
    let text =
        std::str::from_utf8(&bytes).map_err(|_| AppError::new("keys.imported_unsupported"))?;
    let head = text.trim_start();
    if head.starts_with("ssh-") || head.starts_with("ecdsa-") || text.contains("PUBLIC KEY") {
        return Err(AppError::new("keys.imported_public_key"));
    }
    Ok(Zeroizing::new(text.to_owned()))
}

fn decode_error(e: KeyImportError) -> AppError {
    match e {
        KeyImportError::NeedsPassphrase => AppError::new("keys.imported_needs_passphrase"),
        KeyImportError::WrongPassphrase => AppError::new("keys.imported_wrong_passphrase"),
        KeyImportError::OldFormat => AppError::new("keys.imported_old_format"),
        KeyImportError::Unsupported => AppError::new("keys.imported_unsupported"),
    }
}

/// 名字：去掉首尾空白，不能有控制字符，不超过 MAX_NAME_CHARS 个字
fn clean_name(name: &str) -> Option<String> {
    let name = name.trim();
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.chars().any(char::is_control);
    ok.then(|| name.to_owned())
}

/// 用口令加密成 OpenSSH 格式
fn encrypt(key: &PrivateKey, secret: &str) -> Result<Zeroizing<String>, AppError> {
    let mut rng = rand::rng();
    let encrypted = key
        .encrypt_with(
            Cipher::Aes256Ctr,
            Kdf::Bcrypt {
                salt: rng.random::<[u8; 16]>().to_vec(),
                rounds: KDF_ROUNDS,
            },
            rng.random(),
            secret.as_bytes(),
        )
        .map_err(|_| failed("keys.imported_encrypt_failed", "私钥加密失败"))?;
    encrypted
        .to_openssh(LineEnding::LF)
        .map_err(|_| failed("keys.imported_encrypt_failed", "私钥加密失败"))
}

/// 导入一把私钥：读出来（有口令要给口令），转成加密的 OpenSSH 格式存好。同一把私钥（指纹相同）导入过的，直接返回原来那一把。
///
/// :param dir: 存导入私钥的目录
/// :param source: 私钥文件
/// :param name: 显示的名字（空时用文件名）
/// :param passphrase: 原文件的口令（没有口令时不用）
/// :param save_passphrase: 原文件有口令时，要不要把口令存进凭据管理器
/// :param store: 凭据管理器
/// :return: 导入好的私钥
pub fn import(
    dir: &Path,
    source: &Path,
    name: &str,
    passphrase: Option<&str>,
    save_passphrase: bool,
    store: &dyn CredentialStore,
) -> Result<ImportedKey, AppError> {
    let text = read_source(source)?;
    let (key, user_passphrase) = decode_private_key(&text, passphrase).map_err(decode_error)?;
    let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
    if let Some(same) = list(dir, store)?
        .into_iter()
        .find(|k| k.fingerprint == fingerprint)
    {
        // 原来的随机口令丢了（凭据管理器被清过）：用这次读到的私钥重新加密、存新口令，原来那一把就又能用了，
        // 用它的服务器不用改
        if !same.user_passphrase && !same.passphrase_saved {
            let secret = Zeroizing::new(random_secret(32));
            write_private(&key_path(dir, &same.id), encrypt(&key, &secret)?.as_bytes())?;
            store.set(&credential_id(&same.id), &secret)?;
            return Ok(ImportedKey {
                passphrase_saved: true,
                ..same
            });
        }
        // 同一把私钥已经导入过：直接用原来那一把（服务器设置里点导入时就选上它），不重复存
        return Ok(same);
    }
    let file_name = source
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let name = clean_name(name)
        .or_else(|| clean_name(file_name))
        .ok_or_else(|| AppError::new("keys.imported_name_invalid"))?;
    ensure_dir(dir)?;
    // 原文件有口令：照旧用这个口令；没有：随机口令，一定存进凭据管理器
    let secret = if user_passphrase {
        Zeroizing::new(passphrase.unwrap_or_default().to_owned())
    } else {
        Zeroizing::new(random_secret(32))
    };
    let encoded = encrypt(&key, &secret)?;
    let meta = Meta {
        name,
        algorithm: key.algorithm().to_string(),
        fingerprint,
        comment: key.comment().as_str_lossy().to_owned(),
        user_passphrase,
        created_ms: now_ms(),
    };
    let id = random_secret(8);
    let meta_bytes = serde_json::to_vec_pretty(&meta)
        .map_err(|_| failed("keys.imported_write_failed", "保存私钥失败"))?;
    let save = !user_passphrase || save_passphrase;
    let written = write_private(&key_path(dir, &id), encoded.as_bytes())
        .and_then(|()| write_private(&meta_path(dir, &id), &meta_bytes))
        .and_then(|()| {
            if save {
                store.set(&credential_id(&id), &secret)
            } else {
                Ok(())
            }
        });
    if let Err(e) = written {
        // 没存完整：已经写下的都删掉，免得留下打不开的私钥
        let _ = fs::remove_file(key_path(dir, &id));
        let _ = fs::remove_file(meta_path(dir, &id));
        return Err(e);
    }
    Ok(to_imported(dir, &id, meta, save))
}

fn existing(dir: &Path, id: &str) -> Result<Meta, AppError> {
    if !valid_id(id) || !key_path(dir, id).is_file() {
        return Err(AppError::new("keys.imported_not_found"));
    }
    read_meta(dir, id)
}

/// 改名。
///
/// :param dir: 存导入私钥的目录
/// :param id: 私钥编号
/// :param name: 新名字
/// :param store: 凭据管理器（返回口令存没存着）
/// :return: 改好的私钥
pub fn rename(
    dir: &Path,
    id: &str,
    name: &str,
    store: &dyn CredentialStore,
) -> Result<ImportedKey, AppError> {
    let mut meta = existing(dir, id)?;
    meta.name = clean_name(name).ok_or_else(|| AppError::new("keys.imported_name_invalid"))?;
    let bytes = serde_json::to_vec_pretty(&meta)
        .map_err(|_| failed("keys.imported_write_failed", "保存私钥失败"))?;
    write_private(&meta_path(dir, id), &bytes)?;
    let saved = store.get(&credential_id(id))?.is_some();
    Ok(to_imported(dir, id, meta, saved))
}

/// 删掉一把私钥和它存着的口令（还有没有服务器在用由调用方先查）。
///
/// :param dir: 存导入私钥的目录
/// :param id: 私钥编号
/// :param store: 凭据管理器
pub fn remove(dir: &Path, id: &str, store: &dyn CredentialStore) -> Result<(), AppError> {
    if !valid_id(id) {
        return Err(AppError::new("keys.imported_not_found"));
    }
    for path in [key_path(dir, id), meta_path(dir, id)] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(failed("keys.imported_remove_failed", "删除私钥文件失败")),
        }
    }
    store.delete(&credential_id(id))
}

/// 保存或忘掉用户自己设的口令（随机口令不能改、不能忘）。保存前先核对口令能解开私钥。
///
/// :param dir: 存导入私钥的目录
/// :param id: 私钥编号
/// :param passphrase: 要保存的口令；None 表示忘掉（以后连接时问）
/// :param store: 凭据管理器
pub fn set_passphrase(
    dir: &Path,
    id: &str,
    passphrase: Option<&str>,
    store: &dyn CredentialStore,
) -> Result<(), AppError> {
    let meta = existing(dir, id)?;
    if !meta.user_passphrase {
        return Err(AppError::new("keys.imported_random_passphrase"));
    }
    let Some(phrase) = passphrase else {
        return store.delete(&credential_id(id));
    };
    let text = Zeroizing::new(
        fs::read_to_string(key_path(dir, id))
            .map_err(|_| failed("keys.imported_read_failed", "读取私钥信息失败"))?,
    );
    let key = PrivateKey::from_openssh(text.as_bytes())
        .map_err(|_| failed("keys.imported_read_failed", "读取私钥信息失败"))?;
    key.decrypt(phrase.as_bytes())
        .map_err(|_| AppError::new("keys.imported_wrong_passphrase"))?;
    store.set(&credential_id(id), phrase)
}

/// 连接时用：这个路径是导入的私钥、口令又存着时，取出口令（自动填，不问用户）。
///
/// :param dir: 存导入私钥的目录
/// :param path: 服务器 identity_file 里的路径
/// :param store: 凭据管理器
/// :return: 口令；不是导入的私钥、口令没存或读不出来时为 None
pub fn saved_passphrase(
    dir: &Path,
    path: &Path,
    store: &dyn CredentialStore,
) -> Option<Zeroizing<String>> {
    let id = id_of_path(dir, path)?;
    store
        .get(&credential_id(&id))
        .ok()
        .flatten()
        .map(Zeroizing::new)
}

#[cfg(test)]
mod tests;
