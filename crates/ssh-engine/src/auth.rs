use crate::{Error, Result, connection::Client};
use russh::{
    client,
    keys::{
        self, HashAlg, PrivateKey, PrivateKeyWithHashAlg,
        agent::{AgentIdentity, client::AgentClient},
    },
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// 每次登录独立保存的服务器断开原因；原文只用于分类，不进入公开错误。
#[derive(Default)]
pub(crate) struct DisconnectInfo(Mutex<Option<(u32, String)>>);

impl DisconnectInfo {
    /// 保存 info 的原因码及至多 200 个无控制字符；无返回值，不记录服务器原文。
    pub(crate) fn record(&self, info: client::RemoteDisconnectInfo) {
        let message = info
            .message
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect();
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) =
            Some((info.reason_code as u32, message));
    }

    fn authentication_error(&self, rejected_keys: usize) -> Error {
        let info = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let exhausted = match info.as_ref() {
            Some((code, message)) => {
                let message = message.to_lowercase();
                // OpenSSH、Dropbear 等达到上限时的说法
                let limit = [
                    "too many authentication failures",
                    "max auth tries",
                    "maximum authentication attempts",
                ]
                .iter()
                .any(|text| message.contains(text));
                // 登录时限到了（输验证码太久）是另一回事，按断线处理
                let timeout = ["timeout", "time out", "timed out", "grace", "超时", "时限"]
                    .iter()
                    .any(|text| message.contains(text));
                *code == russh::Disconnect::NoMoreAuthMethodsAvailable as u32
                    || limit
                    || rejected_keys >= 5 && !timeout
            }
            None => rejected_keys >= 5,
        };
        if exhausted {
            Error::TooManyAuthAttempts
        } else {
            Error::Disconnected
        }
    }
}

/// 一轮 keyboard-interactive 提示；提示来自服务器，调用方应当按不可信文本展示。
#[derive(Clone, Debug)]
pub struct Prompt {
    pub text: String,
    pub echo: bool,
}

/// 服务器发起的一轮认证，不记录回调返回的回答。
#[derive(Clone, Debug)]
pub struct Challenge {
    pub name: String,
    pub instructions: String,
    pub prompts: Vec<Prompt>,
}

/// 私钥口令回调；返回 None 表示取消，返回值不进入日志或错误。
pub type Passphrase = Arc<dyn Fn() -> Option<String> + Send + Sync>;
/// 每轮交互回调；返回与提示一一对应的回答，None 表示取消。
pub type Answers = Arc<dyn Fn(Challenge) -> Option<Vec<String>> + Send + Sync>;
/// 按需取得服务器密码；None 表示缺少密码或用户取消，秘密不进入错误。
pub type PasswordProvider = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// 一把私钥及其可选口令回调；不实现 Debug。
#[derive(Clone)]
pub struct PrivateKeyFile {
    pub path: PathBuf,
    /// 未提供回调时，有口令的默认私钥会被跳过。
    pub passphrase: Option<Passphrase>,
}

/// 由 core 选择的登录策略；只在同一连接中执行指定的密钥与密码流程。
#[derive(Clone)]
pub struct ConfiguredAuthentication {
    pub agent: bool,
    /// 为真时，agent 只可使用 private_keys 指定身份的公钥（包括同名 .pub 文件）。
    pub agent_identities_only: bool,
    pub private_keys: Vec<PrivateKeyFile>,
    /// 默认密钥允许跳过不存在或损坏的文件；指定密钥失败立即结束。
    pub skip_unusable_keys: bool,
    /// 无此回调时绝不尝试密码，包括指定密钥和专用密钥登录。
    pub password: Option<PasswordProvider>,
    pub answers: Answers,
}

/// 确定性选择一种认证方式，不自动降级到其他方式；故意不实现 Debug。
#[derive(Clone)]
pub enum Authentication {
    PrivateKey {
        path: PathBuf,
        passphrase: Option<Passphrase>,
    },
    /// 只使用指定公钥；不遍历 agent 的其他密钥。
    Agent {
        public_key: keys::PublicKey,
    },
    Password(String),
    KeyboardInteractive(Answers),
    Configured(ConfiguredAuthentication),
    /// 仅测试功能可用：用内存流替代平台 agent，仍执行真实列举与签名协议。
    #[cfg(any(test, feature = "test-support"))]
    MemoryAgent(
        ConfiguredAuthentication,
        Arc<std::sync::Mutex<Option<tokio::io::DuplexStream>>>,
    ),
}

async fn rsa_hash(
    handle: &client::Handle<Client>,
    rsa: bool,
    legacy: bool,
) -> Result<Option<HashAlg>> {
    if !rsa {
        return Ok(None);
    }
    match handle
        .best_supported_rsa_hash()
        .await
        .map_err(Error::from)?
    {
        Some(Some(hash)) => Ok(Some(hash)),
        None if legacy => Ok(None),
        None => Ok(Some(HashAlg::Sha256)),
        Some(None) if legacy => Ok(None),
        Some(None) => Err(Error::Algorithm),
    }
}

pub(crate) async fn load_key(path: PathBuf, passphrase: Option<Passphrase>) -> Result<PrivateKey> {
    tokio::task::spawn_blocking(move || match keys::load_secret_key(&path, None) {
        Ok(key) => Ok(key),
        Err(keys::Error::KeyIsEncrypted) => {
            let phrase = passphrase
                .and_then(|callback| callback())
                .ok_or(Error::Callback)?;
            keys::load_secret_key(&path, Some(&phrase)).map_err(|_| Error::PrivateKey)
        }
        Err(_) => Err(Error::PrivateKey),
    })
    .await
    .map_err(|_| Error::Runtime)?
}

pub(crate) async fn agent()
-> Result<AgentClient<Box<dyn keys::agent::client::AgentStream + Send + Unpin>>> {
    #[cfg(unix)]
    let agent = AgentClient::connect_env().await.map_err(|_| Error::Agent)?;
    #[cfg(windows)]
    let agent = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
        .await
        .map_err(|_| Error::Agent)?;
    #[cfg(not(any(unix, windows)))]
    return Err(Error::Agent);
    #[cfg(any(unix, windows))]
    Ok(agent.dynamic())
}

/// 按 auth 登录。认证途中连接断了（服务器的登录时限到了、网络断了）返回 Disconnected，
/// 不报 Authentication：那不是密码或密钥不对，上层不能因此作废记住的密码。
pub(crate) async fn authenticate(
    handle: &mut client::Handle<Client>,
    user: &str,
    auth: Authentication,
    legacy: bool,
    disconnect: &DisconnectInfo,
) -> Result<()> {
    let mut rejected_keys = 0;
    let result = match auth {
        Authentication::Configured(config) => {
            configured(handle, user, config, legacy, None, &mut rejected_keys).await
        }
        #[cfg(any(test, feature = "test-support"))]
        Authentication::MemoryAgent(config, stream) => {
            let stream = stream
                .lock()
                .map_err(|_| Error::Runtime)?
                .take()
                .ok_or(Error::Agent)?;
            configured(
                handle,
                user,
                config,
                legacy,
                Some(AgentClient::connect(stream).dynamic()),
                &mut rejected_keys,
            )
            .await
        }
        auth => authenticate_single(handle, user, auth, legacy).await,
    };
    if result == Err(Error::Disconnected) {
        // SendError 可早于 disconnected 回调；等连接任务结束后再读取原因，仍受认证总时限约束。
        let _ = handle.await;
        return Err(disconnect.authentication_error(rejected_keys));
    }
    result
}

async fn authenticate_single(
    handle: &mut client::Handle<Client>,
    user: &str,
    auth: Authentication,
    legacy: bool,
) -> Result<()> {
    let success = match auth {
        Authentication::Password(password) => handle
            .authenticate_password(user, password)
            .await?
            .success(),
        Authentication::PrivateKey { path, passphrase } => {
            let key = load_key(path, passphrase).await?;
            let hash = rsa_hash(handle, key.algorithm().is_rsa(), legacy).await?;
            handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await?
                .success()
        }
        Authentication::Agent { public_key } => {
            let mut agent = agent().await?;
            let found = agent.request_identities().await.map_err(|_| Error::Agent)?.iter().any(|identity| {
                matches!(identity, AgentIdentity::PublicKey { key, .. } if key.key_data() == public_key.key_data())
            });
            if !found {
                return Err(Error::Agent);
            }
            let hash = rsa_hash(handle, public_key.algorithm().is_rsa(), legacy).await?;
            handle
                .authenticate_publickey_with(user, public_key, hash, &mut agent)
                .await
                .map_err(|error| match error {
                    russh::AgentAuthError::Send(_) => Error::Disconnected,
                    _ => Error::Agent,
                })?
                .success()
        }
        Authentication::KeyboardInteractive(callback) => {
            keyboard_interactive(handle, user, callback, None).await?
        }
        Authentication::Configured(_) => return Err(Error::InvalidInput),
        #[cfg(any(test, feature = "test-support"))]
        Authentication::MemoryAgent(..) => return Err(Error::InvalidInput),
    };
    if success {
        Ok(())
    } else {
        Err(Error::Authentication)
    }
}

async fn keyboard_interactive(
    handle: &mut client::Handle<Client>,
    user: &str,
    callback: Answers,
    password_fallback: Option<PasswordProvider>,
) -> Result<bool> {
    let mut response = handle
        .authenticate_keyboard_interactive_start(user, None)
        .await?;
    // 防止恶意服务器无限要求提示；正常多轮认证远少于此上限。
    let mut rounds = 0;
    let success = loop {
        match response {
            client::KeyboardInteractiveAuthResponse::Success => break true,
            client::KeyboardInteractiveAuthResponse::Failure {
                remaining_methods,
                partial_success,
            } => {
                if rounds == 0
                    && !partial_success
                    && remaining_methods.contains(&russh::MethodKind::Password)
                    && let Some(password) = password_fallback
                {
                    return password_auth(handle, user, password).await;
                }
                break false;
            }
            client::KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                rounds += 1;
                if rounds > 32 {
                    return Err(Error::Callback);
                }
                let challenge = Challenge {
                    name,
                    instructions,
                    prompts: prompts
                        .into_iter()
                        .map(|p| Prompt {
                            text: p.prompt,
                            echo: p.echo,
                        })
                        .collect(),
                };
                let count = challenge.prompts.len();
                let callback = callback.clone();
                let answers = tokio::task::spawn_blocking(move || callback(challenge))
                    .await
                    .map_err(|_| Error::Runtime)?
                    .ok_or(Error::Callback)?;
                if answers.len() != count {
                    return Err(Error::Callback);
                }
                response = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await?;
            }
        }
    };
    Ok(success)
}

async fn password_auth(
    handle: &mut client::Handle<Client>,
    user: &str,
    password: PasswordProvider,
) -> Result<bool> {
    let password = tokio::task::spawn_blocking(move || password())
        .await
        .map_err(|_| Error::Runtime)?
        .ok_or(Error::Callback)?;
    Ok(handle
        .authenticate_password(user, password)
        .await?
        .success())
}

// 读取加密私钥的公开头部不需要口令；公钥文件也可单独提供给 agent 选择身份。
fn identity_public_keys(paths: &[PathBuf]) -> Vec<keys::PublicKey> {
    paths
        .iter()
        .filter_map(|path| {
            let mut public_path = path.as_os_str().to_owned();
            public_path.push(".pub");
            keys::load_public_key(path)
                .ok()
                .or_else(|| keys::load_public_key(std::path::Path::new(&public_path)).ok())
                .or_else(|| {
                    PrivateKey::read_openssh_file(path)
                        .ok()
                        .map(|key| key.public_key().clone())
                })
        })
        .collect()
}

async fn configured(
    handle: &mut client::Handle<Client>,
    user: &str,
    config: ConfiguredAuthentication,
    legacy: bool,
    agent_override: Option<AgentClient<Box<dyn keys::agent::client::AgentStream + Send + Unpin>>>,
    rejected_keys: &mut usize,
) -> Result<()> {
    use russh::{MethodKind, client::AuthResult};
    let methods = match handle.authenticate_none(user).await? {
        AuthResult::Success => return Ok(()),
        AuthResult::Failure {
            remaining_methods, ..
        } => remaining_methods,
    };
    let mut rejected_agent_keys = Vec::new();
    if methods.contains(&MethodKind::PublicKey) {
        let available_agent = if config.agent {
            match agent_override {
                Some(agent) => Ok(agent),
                None => agent().await,
            }
        } else {
            Err(Error::Agent)
        };
        if let Ok(mut agent) = available_agent {
            let paths: Vec<_> = config
                .private_keys
                .iter()
                .map(|file| file.path.clone())
                .collect();
            let preferred = tokio::task::spawn_blocking(move || identity_public_keys(&paths))
                .await
                .map_err(|_| Error::PrivateKey)?;
            let mut identities: Vec<_> = agent
                .request_identities()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter_map(|identity| match identity {
                    AgentIdentity::PublicKey { key, .. } => Some(key),
                    _ => None,
                })
                .collect();
            let position = |key: &keys::PublicKey| {
                preferred
                    .iter()
                    .position(|allowed| allowed.key_data() == key.key_data())
            };
            if config.agent_identities_only {
                identities.retain(|key| position(key).is_some());
            }
            // 稳定排序：指定身份按文件顺序优先，其余保持 agent 原始顺序。
            identities.sort_by_key(|key| position(key).unwrap_or(usize::MAX));
            for key in identities {
                let hash = match rsa_hash(handle, key.algorithm().is_rsa(), legacy).await {
                    Ok(hash) => hash,
                    Err(Error::Algorithm) => continue,
                    Err(error) => return Err(error),
                };
                match handle
                    .authenticate_publickey_with(user, key.clone(), hash, &mut agent)
                    .await
                {
                    Ok(result) if result.success() => return Ok(()),
                    // 连接断了：后面的身份和密码都发不出去，直接报断开
                    Err(russh::AgentAuthError::Send(_)) => return Err(Error::Disconnected),
                    Ok(AuthResult::Failure {
                        partial_success: false,
                        ..
                    }) => {
                        *rejected_keys += 1;
                        rejected_agent_keys.push(key);
                    }
                    Ok(_) | Err(_) => continue,
                }
            }
        }
        for file in config.private_keys {
            if !rejected_agent_keys.is_empty() {
                let path = file.path.clone();
                let public = tokio::task::spawn_blocking(move || identity_public_keys(&[path]))
                    .await
                    .map_err(|_| Error::PrivateKey)?;
                if public.iter().any(|key| {
                    rejected_agent_keys
                        .iter()
                        .any(|rejected| rejected.key_data() == key.key_data())
                }) {
                    continue;
                }
            }
            let can_prompt = file.passphrase.is_some();
            let key = match load_key(file.path, file.passphrase).await {
                Ok(key) => key,
                Err(Error::PrivateKey) if config.skip_unusable_keys => continue,
                Err(Error::Callback) if config.skip_unusable_keys && !can_prompt => continue,
                Err(error) => return Err(error),
            };
            if rejected_agent_keys
                .iter()
                .any(|rejected| rejected.key_data() == key.public_key().key_data())
            {
                continue;
            }
            let hash = match rsa_hash(handle, key.algorithm().is_rsa(), legacy).await {
                Ok(hash) => hash,
                Err(Error::Algorithm) if config.skip_unusable_keys => continue,
                Err(error) => return Err(error),
            };
            match handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await?
            {
                AuthResult::Success => return Ok(()),
                AuthResult::Failure {
                    partial_success: false,
                    ..
                } => *rejected_keys += 1,
                _ => {}
            }
        }
    }
    let Some(password) = config.password else {
        return Err(Error::Authentication);
    };
    // KI 尚未提出问题就被拒绝时可以回退；回答被拒后绝不再发送同一个密码。
    let success = if methods.contains(&MethodKind::KeyboardInteractive) {
        keyboard_interactive(handle, user, config.answers, Some(password)).await?
    } else if methods.contains(&MethodKind::Password) {
        password_auth(handle, user, password).await?
    } else {
        false
    };
    if success {
        Ok(())
    } else {
        Err(Error::Authentication)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Engine;

    #[cfg(windows)]
    #[test]
    fn windows_agent_named_pipe_list_and_sign_including_rsa_sha2() {
        use keys::signature::Verifier;
        let Ok(text) = std::env::var("PIERLINK_ENGINE_AGENT_PUBKEY") else {
            eprintln!("跳过：未设置 PIERLINK_ENGINE_AGENT_PUBKEY");
            return;
        };
        let public = keys::PublicKey::from_openssh(&text).unwrap();
        let engine = Engine::new().unwrap();
        assert!(
            engine
                .agent_keys()
                .unwrap()
                .iter()
                .any(|key| key.key_data() == public.key_data())
        );
        engine
            .executor
            .run(std::time::Duration::from_secs(10), "agent 签名", async {
                let mut agent = agent().await?;
                let hashes = if public.algorithm().is_rsa() {
                    vec![Some(HashAlg::Sha256), Some(HashAlg::Sha512)]
                } else {
                    vec![None]
                };
                for hash in hashes {
                    let signature = agent
                        .sign_request_signature(&public, hash, b"pierlink-agent-test")
                        .await
                        .map_err(|_| Error::Agent)?;
                    Verifier::verify(&public, b"pierlink-agent-test", &signature)
                        .map_err(|_| Error::Agent)?;
                    if let Some(hash) = hash {
                        assert_eq!(
                            signature.algorithm(),
                            keys::Algorithm::Rsa { hash: Some(hash) }
                        );
                    }
                }
                Ok(())
            })
            .unwrap();
        if !public.algorithm().is_rsa() {
            eprintln!(
                "跳过：PIERLINK_ENGINE_AGENT_PUBKEY 不是 RSA；需用 RSA 公钥再运行以验证 rsa-sha2"
            );
        }
    }

    #[test]
    fn 断开文字去控制字符并按字符截断且不进入错误() {
        let info = DisconnectInfo::default();
        info.record(client::RemoteDisconnectInfo {
            reason_code: russh::Disconnect::ProtocolError,
            message: format!("\0Too many authentication failures\r\n{}", "密".repeat(250)),
            lang_tag: "".into(),
        });
        let stored = info.0.lock().unwrap();
        let (code, message) = stored.as_ref().unwrap();
        assert_eq!(*code, 2);
        assert_eq!(message.chars().count(), 200);
        assert!(!message.chars().any(char::is_control));
        drop(stored);
        let error = info.authentication_error(0);
        assert_eq!(error, Error::TooManyAuthAttempts);
        assert!(!format!("{error:?} {error}").contains('密'));
        assert_eq!(
            crate::error::io_error(error).kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn missing_key_does_not_expose_path_or_passphrase() {
        let engine = Engine::new().unwrap();
        let error = engine
            .executor
            .run(
                std::time::Duration::from_secs(2),
                "加载私钥",
                load_key(
                    PathBuf::from("不存在的秘密路径"),
                    Some(Arc::new(|| Some("秘密口令".into()))),
                ),
            )
            .unwrap_err();
        assert_eq!(error, Error::PrivateKey);
        assert!(!format!("{error:?}").contains("秘密"));
    }

    #[cfg(unix)]
    #[test]
    fn 私钥数字开头是零的_ecdsa_密钥也能读() {
        // OpenSSH 保存时省掉开头的 0，约每 256 个 P-256 密钥有一个只剩 31 字节；反复生成直到碰上一个
        use russh::keys::ssh_key::private::EcdsaKeypair;
        let root = crate::test_files::tempdir();
        let engine = Engine::new().unwrap();
        let mut found = false;
        for i in 0..4000 {
            let path = root.path().join(format!("k{i}"));
            assert!(
                std::process::Command::new("/usr/bin/ssh-keygen")
                    .args(["-q", "-t", "ecdsa", "-N", "", "-f"])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            let key = engine
                .executor
                .run(
                    std::time::Duration::from_secs(10),
                    "加载密钥",
                    load_key(path.clone(), None),
                )
                .unwrap_or_else(|e| panic!("第 {i} 个密钥读不出来：{e:?}"));
            if matches!(key.key_data().ecdsa(), Some(EcdsaKeypair::NistP256 { private, .. }) if private.as_slice()[0] == 0)
            {
                found = true;
                break;
            }
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(path.with_extension("pub"));
        }
        assert!(found, "4000 个密钥里没碰到开头是 0 的");
    }

    #[cfg(unix)]
    #[test]
    fn openssh_private_key_algorithms_and_encrypted_callbacks() {
        let root = crate::test_files::tempdir();
        let engine = Engine::new().unwrap();
        for algorithm in ["ed25519", "ecdsa", "rsa"] {
            for encrypted in [false, true] {
                let path = root.path().join(format!("{algorithm}-{encrypted}"));
                let phrase = if encrypted { "测试口令" } else { "" };
                assert!(
                    std::process::Command::new("/usr/bin/ssh-keygen")
                        .args(["-q", "-t", algorithm, "-N", phrase, "-f"])
                        .arg(&path)
                        .status()
                        .unwrap()
                        .success()
                );
                let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let count = calls.clone();
                let callback: Passphrase = Arc::new(move || {
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Some("测试口令".into())
                });
                let key = engine
                    .executor
                    .run(
                        // 加密私钥要跑 bcrypt 派生，机器忙时较慢；这里只防卡死
                        std::time::Duration::from_secs(60),
                        "加载密钥",
                        load_key(path.clone(), Some(callback)),
                    )
                    .unwrap();
                assert_eq!(
                    key.algorithm().as_str(),
                    match algorithm {
                        "ed25519" => "ssh-ed25519",
                        "ecdsa" => "ecdsa-sha2-nistp256",
                        _ => "ssh-rsa",
                    }
                );
                assert_eq!(
                    calls.load(std::sync::atomic::Ordering::Relaxed),
                    usize::from(encrypted)
                );
                if encrypted {
                    assert!(matches!(
                        engine.executor.run(
                            std::time::Duration::from_secs(10),
                            "取消口令",
                            load_key(path.clone(), None)
                        ),
                        Err(Error::Callback)
                    ));
                    assert!(matches!(
                        engine.executor.run(
                            std::time::Duration::from_secs(10),
                            "错误口令",
                            load_key(path, Some(Arc::new(|| Some("错误口令".into()))))
                        ),
                        Err(Error::PrivateKey)
                    ));
                }
            }
        }
    }
}
