//! 生成供调用方直接传给 SSH 子进程的参数和环境变量。
use super::{
    askpass::normalize_password_target,
    config::ResolvedConfig,
    quote::sh_quote,
    target::{ExplicitTarget, SshTarget},
};
use crate::error::AppError;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    Terminal {
        login_shell: Option<String>,
        cwd_marker: Option<String>,
        /// 是否注入提示符与命令记录标记；关闭时保留原连接命令。
        integration: bool,
    },
    Background,
    /// 主机密钥确认；隔离配置中每一级均允许交互确认，但禁止密码认证。
    HostKeyProbe,
    Tunnel {
        forwards: Vec<Forward>,
        readiness: Readiness,
    },
    Sftp,
}

/// 所有 TCP 监听地址固定为回环地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forward {
    Local {
        local_port: u16,
        remote: ForwardTarget,
    },
    Remote {
        remote_port: u16,
        local_port: u16,
    },
    Dynamic {
        local_port: u16,
    },
}

/// 同步隧道探测监听端口；用户转发只观察认证标记，避免连接用户服务。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    ProbeLocalPorts,
    AuthMarker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardTarget {
    Tcp { port: u16 },
    Unix { path: String },
    Host { host: String, port: u16 },
}

/// 校验 remote 并返回 OpenSSH 转发参数中的目标段，非法值返回不含原文的错误。
pub(crate) fn forward_target(remote: &ForwardTarget) -> Result<String, AppError> {
    match remote {
        ForwardTarget::Tcp { port: p } => {
            port(*p)?;
            Ok(format!("127.0.0.1:{p}"))
        }
        ForwardTarget::Host { host, port: p } => {
            name(host, "forward.host", true)?;
            port(*p)?;
            let address = if host.contains(':') || host.contains(['[', ']']) {
                let literal = host
                    .strip_prefix('[')
                    .and_then(|h| h.strip_suffix(']'))
                    .unwrap_or(host);
                if literal.parse::<std::net::Ipv6Addr>().is_err() {
                    return Err(invalid("forward.host"));
                }
                format!("[{literal}]")
            } else {
                if !host
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
                {
                    return Err(invalid("forward.host"));
                }
                host.clone()
            };
            Ok(format!("{address}:{p}"))
        }
        ForwardTarget::Unix { path: p } => {
            text(p, "forward.path")?;
            // SSH 转发语法以冒号和方括号分段；不接受会被重新解释的路径。
            if !p.starts_with('/') || p.contains([':', '[', ']', '\\', '\n', '\r']) {
                return Err(invalid("forward.path"));
            }
            Ok(p.clone())
        }
    }
}

/// 密码连接通过主进程登记的 IPC 取密码；不携带服务器凭据引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskPass {
    pub program: PathBuf,
    pub registration: std::sync::Arc<super::askpass_pipe::Registration>,
}

/// 专用密钥配置；只携带凭据引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedicatedKey {
    pub private_path: PathBuf,
    pub askpass_program: PathBuf,
    pub credential_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommonOpts {
    pub dedicated_key: Option<DedicatedKey>,
    /// 手动跳板隔离配置的写入路径；调用方负责创建及连接结束后的删除。
    pub config_file: Option<PathBuf>,
    pub known_hosts: Option<PathBuf>,
    pub askpass: Option<AskPass>,
    pub connect_timeout: Option<u16>,
    /// 别名目标的 ssh -G 结果；未知时保守跳过集成，避免覆盖 RemoteCommand。
    pub has_remote_command: Option<bool>,
}

/// 调用方用 args 和 env 设置子进程，不修改当前进程环境。
#[derive(Clone)]
pub struct SshCommand {
    /// 内置后台执行上下文；由 remote::command::background 构造，不启动本地进程。
    pub builtin: Option<std::sync::Arc<crate::remote::command::BuiltinCommand>>,
    /// 本次终端命令是否包含提示符钩子；后台用途为 false。
    pub hooked: bool,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// 启动前须写入 CommonOpts.config_file 的配置文本；结束或失败后由调用方删除。
    pub config_text: Option<String>,
    /// 命令登记模板；每次真正启动派生独立 nonce，进程持有实际守卫。比较命令时忽略此字段。
    pub askpass: Option<std::sync::Arc<super::askpass_pipe::Registration>>,
}

impl PartialEq for SshCommand {
    fn eq(&self, other: &Self) -> bool {
        self.hooked == other.hooked
            && self.args == other.args
            && self.env == other.env
            && self.config_text == other.config_text
            && match (&self.builtin, &other.builtin) {
                (None, None) => true,
                (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}
impl Eq for SshCommand {}
impl SshCommand {
    /// 无参数；为真正启动派生新登记并覆盖环境，后台缺密码时直接返回错误，不启动 SSH。
    pub fn prepare(&self) -> Result<Self, AppError> {
        let mut command = self.clone();
        if let Some(template) = &self.askpass {
            let guard = template.prepare()?;
            command.env.retain(|(key, _)| {
                key != "PIERLINK_ASKPASS_NONCE" && key != "PIERLINK_ASKPASS_PIPE"
            });
            command
                .env
                .push(("PIERLINK_ASKPASS_PIPE".into(), guard.pipe().into()));
            command
                .env
                .push(("PIERLINK_ASKPASS_NONCE".into(), guard.nonce().into()));
            command.askpass = Some(guard);
        }
        Ok(command)
    }
}

fn invalid(field: &str) -> AppError {
    AppError::new("ssh.invalid_argument").with("field", field)
}

fn text(value: &str, field: &str) -> Result<(), AppError> {
    if value.is_empty() || value.contains('\0') {
        return Err(invalid(field));
    }
    Ok(())
}

pub(crate) fn name(value: &str, field: &str, host: bool) -> Result<(), AppError> {
    text(value, field)?;
    if value.starts_with('-')
        || value.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || "'\"$\\;&<>|(){}`%".contains(c)
                || (host && "@,".contains(c))
        })
    {
        return Err(invalid(field));
    }
    Ok(())
}

fn path(value: &Path, field: &str) -> Result<String, AppError> {
    let value = value.to_str().ok_or_else(|| invalid(field))?;
    text(value, field)?;
    Ok(value.to_owned())
}

fn port(value: u16) -> Result<(), AppError> {
    if value == 0 {
        return Err(invalid("port"));
    }
    Ok(())
}

fn option(args: &mut Vec<String>, value: impl Into<String>) {
    args.push("-o".into());
    args.push(value.into());
}

// SSH 配置词法会消耗反斜杠，路径须先双写再用双引号包裹。
fn config_word(value: &str, field: &str) -> Result<String, AppError> {
    text(value, field)?;
    if value.contains(['"', '\n', '\r']) {
        return Err(invalid(field));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('%', "%%")
    ))
}

/// 根据已解析配置、Host 名和隔离配置路径返回显式目标；非法值返回脱敏错误。
/// 调用方须用 isolated::render_config 生成完整跳板链配置，写入应用数据目录，
/// 并在连接结束（包括失败）后删除文件。known_hosts 必须另由 CommonOpts 传入。
pub fn explicit_target(
    cfg: &ResolvedConfig,
    host: &str,
    config_file: &Path,
) -> Result<ExplicitTarget, AppError> {
    name(host, "alias", true)?;
    if host.contains(['*', '?', '!']) {
        return Err(invalid("alias"));
    }
    name(&cfg.hostname, "hostname", true)?;
    name(&cfg.user, "user", false)?;
    path(config_file, "config_file")?;
    Ok(ExplicitTarget {
        host: host.into(),
        config_file: config_file.into(),
        hostname: cfg.hostname.clone(),
        user: cfg.user.clone(),
        has_remote_command: cfg.has_remote_command,
    })
}

impl std::fmt::Debug for SshCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let args: Vec<_> = self
            .args
            .iter()
            .map(|a| crate::security::redact(a, &[]))
            .collect();
        let env: Vec<_> = self.env.iter().map(|(k, _)| (k, "***")).collect();
        f.debug_struct("SshCommand")
            .field("args", &args)
            .field("env", &env)
            .finish()
    }
}

pub(crate) fn terminal_command(
    shell: &Option<String>,
    integration: bool,
) -> Result<Option<String>, AppError> {
    let original = shell
        .as_ref()
        .map(|s| {
            text(s, "login_shell")?;
            if s.starts_with('-')
                || !s
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c))
            {
                return Err(invalid("login_shell"));
            }
            Ok(format!("exec {} -l", sh_quote(s)?))
        })
        .transpose()?;
    if integration {
        crate::shell_integration::command(shell.as_deref()).map(Some)
    } else {
        Ok(original)
    }
}

/// 按目标、用途及公共设置生成 SSH 参数和子进程环境；非法值返回不含原文的错误。
/// 手动跳板必须提供 config_file 和 known_hosts；调用方启动前写入返回的 config_text，
/// 文件放在应用数据目录，连接结束或失败后删除。探测时必须使用 HostKeyProbe。
pub fn build_args(
    target: &SshTarget,
    purpose: &Purpose,
    opts: &CommonOpts,
) -> Result<SshCommand, AppError> {
    if opts.askpass.is_some() && opts.dedicated_key.is_some() {
        return Err(invalid("dedicated_key"));
    }
    if let Purpose::Terminal {
        cwd_marker: Some(marker),
        ..
    } = purpose
    {
        crate::cwd::validate_marker(marker)?;
    }
    let mut config_text = None;
    let mut args = Vec::new();
    let mut env = Vec::new();
    if matches!(purpose, Purpose::HostKeyProbe) {
        for setting in [
            "StrictHostKeyChecking=ask",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "NumberOfPasswordPrompts=0",
            "FingerprintHash=sha256",
        ] {
            option(&mut args, setting);
        }
    }
    if let Purpose::Terminal {
        cwd_marker: Some(marker),
        ..
    } = purpose
    {
        // 用 SendEnv 而不是 SetEnv：命令行的 SetEnv 会让用户 ssh config 里的 SetEnv 整体失效（取第一个值），
        // SendEnv 是累加的，不影响用户配置。变量放在 ssh 进程自己的环境里。
        option(&mut args, "SendEnv=LC_PIERLINK_TERM");
        env.push(("LC_PIERLINK_TERM".into(), marker.clone()));
    }
    option(&mut args, "ServerAliveInterval=15");
    option(&mut args, "ServerAliveCountMax=3");
    if !matches!(purpose, Purpose::Terminal { .. }) {
        for setting in [
            "RemoteCommand=none",
            "RequestTTY=no",
            "PermitLocalCommand=no",
            "ForwardAgent=no",
            "ForwardX11=no",
            "NoHostAuthenticationForLocalhost=no",
            "ControlMaster=no",
            "ControlPath=none",
            "UpdateHostKeys=no",
            "ForkAfterAuthentication=no",
            "StdinNull=no",
        ] {
            option(&mut args, setting);
        }
        // 隧道需要 INFO 的转发禁止提示；解析后台输出时用 ERROR 屏蔽服务器 Banner。
        option(
            &mut args,
            if matches!(
                purpose,
                Purpose::Tunnel {
                    readiness: Readiness::AuthMarker,
                    ..
                }
            ) {
                "LogLevel=VERBOSE"
            } else if matches!(purpose, Purpose::Tunnel { .. }) {
                "LogLevel=INFO"
            } else {
                "LogLevel=ERROR"
            },
        );
        if !matches!(purpose, Purpose::HostKeyProbe) {
            option(&mut args, "StrictHostKeyChecking=yes");
        }
        if opts.askpass.is_none()
            && opts.dedicated_key.is_none()
            && !matches!(purpose, Purpose::HostKeyProbe)
        {
            option(&mut args, "BatchMode=yes");
        }
    }
    if matches!(
        purpose,
        Purpose::Background | Purpose::HostKeyProbe | Purpose::Sftp
    ) {
        option(&mut args, "ClearAllForwardings=yes");
    }
    if matches!(purpose, Purpose::Terminal { login_shell, .. } if login_shell.is_some()) {
        option(&mut args, "RemoteCommand=none");
    }
    if let Some(timeout) = opts.connect_timeout {
        option(&mut args, format!("ConnectTimeout={timeout}"));
    }
    if let Some(ask) = &opts.askpass
        && !matches!(purpose, Purpose::HostKeyProbe)
    {
        let expected = match target {
            SshTarget::Manual {
                host,
                user: Some(user),
                ..
            } => format!("{user}@{}", host.to_ascii_lowercase()),
            SshTarget::Explicit(t) => format!("{}@{}", t.user, t.hostname.to_ascii_lowercase()),
            _ => return Err(AppError::new("ssh.askpass_needs_resolved_target")),
        };
        if normalize_password_target(ask.registration.expected_prompt()) != expected {
            return Err(invalid("askpass.expected_prompt"));
        }
        option(&mut args, "StrictHostKeyChecking=yes");
        env.push(("PIERLINK_EXPECTED_PROMPT".into(), expected));
        option(&mut args, "BatchMode=no");
        option(&mut args, "NumberOfPasswordPrompts=1");
        option(&mut args, "ControlMaster=no");
        option(&mut args, "ControlPath=none");
        env.push(("SSH_ASKPASS".into(), path(&ask.program, "askpass.program")?));
        env.push(("SSH_ASKPASS_REQUIRE".into(), "force".into()));
        env.push((
            "PIERLINK_ASKPASS_PIPE".into(),
            ask.registration.pipe().into(),
        ));
        env.push((
            "PIERLINK_ASKPASS_NONCE".into(),
            ask.registration.nonce().into(),
        ));
    }
    if let Some(key) = &opts.dedicated_key {
        let private = path(&key.private_path, "dedicated_key.private_path")?;
        text(&key.credential_id, "dedicated_key.credential_id")?;
        for setting in [
            "IdentitiesOnly=yes",
            "IdentityAgent=none",
            "PreferredAuthentications=publickey",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "BatchMode=no",
            "NumberOfPasswordPrompts=1",
            "ControlMaster=no",
            "ControlPath=none",
        ] {
            option(&mut args, setting);
        }
        option(
            &mut args,
            format!(
                "IdentityFile={}",
                config_word(&private, "dedicated_key.private_path")?
            ),
        );
        env.extend([
            (
                "SSH_ASKPASS".into(),
                path(&key.askpass_program, "dedicated_key.askpass_program")?,
            ),
            ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
            ("PIERLINK_CREDENTIAL_ID".into(), key.credential_id.clone()),
            ("PIERLINK_KEY_PATH".into(), private),
        ]);
    }
    if let Some(p) = &opts.known_hosts {
        let p = path(p, "known_hosts")?;
        option(
            &mut args,
            format!("UserKnownHostsFile={}", config_word(&p, "known_hosts")?),
        );
    }
    let destination = match target {
        SshTarget::Manual { jump: Some(_), .. } => {
            let file = opts
                .config_file
                .as_ref()
                .ok_or_else(|| invalid("config_file"))?;
            let (rendered, destination) =
                super::isolated::render_manual_config(target, purpose, opts)?;
            args.extend(["-F".into(), path(file, "config_file")?]);
            config_text = Some(rendered);
            destination
        }

        SshTarget::Explicit(target) => {
            if opts.known_hosts.is_none() {
                return Err(AppError::new("ssh.known_hosts_required"));
            }
            args.extend(["-F".into(), path(&target.config_file, "config_file")?]);
            if opts.dedicated_key.is_some() {
                let source = std::fs::read_to_string(&target.config_file)
                    .map_err(|_| invalid("dedicated_key.config_file"))?;
                config_text = Some(super::isolated::dedicated_config(&source, &target.host)?);
            }
            target.host.clone()
        }
        SshTarget::Alias { alias } => {
            if opts.dedicated_key.is_some() {
                return Err(AppError::new("ssh.dedicated_key_needs_resolved_target"));
            }
            name(alias, "alias", true)?;
            alias.clone()
        }
        SshTarget::Manual {
            host,
            port: p,
            user,
            identity_file,
            jump: _,
        } => {
            name(host, "host", true)?;
            port(*p)?;
            args.extend(["-F".into(), "none".into(), "-p".into(), p.to_string()]);
            if let Some(u) = user {
                name(u, "user", false)?;
                args.extend(["-l".into(), u.clone()]);
            }
            if let Some(p) = identity_file
                .as_ref()
                .filter(|_| opts.dedicated_key.is_none())
            {
                // 指定了私钥就只用它（agent 里同一把也算），不先把 agent 里的密钥挨个试一遍：
                // 密钥多时会超过服务器的登录尝试次数（MaxAuthTries）；和内置 SSH、跳板的写法一致
                args.extend([
                    "-i".into(),
                    path(p, "identity_file")?,
                    "-o".into(),
                    "IdentitiesOnly=yes".into(),
                ]);
            }
            host.clone()
        }
    };
    match purpose {
        Purpose::Terminal { .. } => args.push("-t".into()),
        Purpose::Background | Purpose::HostKeyProbe => args.push("-T".into()),
        Purpose::Sftp => args.extend(["-T".into(), "-s".into()]),
        Purpose::Tunnel { forwards, .. } => {
            args.push("-N".into());
            option(&mut args, "ExitOnForwardFailure=yes");
            for forward in forwards {
                let (flag, value) = match forward {
                    Forward::Local { local_port, remote } => {
                        port(*local_port)?;
                        (
                            "-L",
                            format!("127.0.0.1:{local_port}:{}", forward_target(remote)?),
                        )
                    }
                    Forward::Remote {
                        remote_port,
                        local_port,
                    } => {
                        port(*remote_port)?;
                        port(*local_port)?;
                        (
                            "-R",
                            format!("127.0.0.1:{remote_port}:127.0.0.1:{local_port}"),
                        )
                    }
                    Forward::Dynamic { local_port } => {
                        port(*local_port)?;
                        ("-D", format!("127.0.0.1:{local_port}"))
                    }
                };
                args.extend([flag.into(), value]);
            }
        }
    }
    args.push(destination);
    let mut hooked = false;
    match purpose {
        Purpose::Background => args.push("/bin/sh -s".into()),
        Purpose::HostKeyProbe => args.push("exit".into()),
        Purpose::Sftp => args.push("sftp".into()),
        Purpose::Terminal {
            login_shell,
            integration,
            ..
        } => {
            let has_remote_command = opts.has_remote_command == Some(true)
                || match target {
                    SshTarget::Alias { .. } => opts.has_remote_command.unwrap_or(true),
                    SshTarget::Explicit(target) => target.has_remote_command,
                    SshTarget::Manual { .. } => false,
                };
            hooked = *integration && !has_remote_command;
            if let Some(command) = terminal_command(login_shell, hooked)? {
                args.push(command);
            }
        }
        Purpose::Tunnel { .. } => {}
    }
    Ok(SshCommand {
        hooked,
        args,
        env,
        config_text,
        builtin: None,
        askpass: opts
            .askpass
            .as_ref()
            .filter(|_| !matches!(purpose, Purpose::HostKeyProbe))
            .map(|ask| ask.registration.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 配置远程命令时跳过集成且保留登录shell覆盖行为() {
        let mut cfg =
            super::super::config::parse_ssh_g(include_str!("../../tests/data/ssh-g.txt")).unwrap();
        for has_remote_command in [false, true] {
            cfg.has_remote_command = has_remote_command;
            let explicit = SshTarget::Explicit(Box::new(
                explicit_target(&cfg, "dev", Path::new("config")).unwrap(),
            ));
            for target in [&alias(), &explicit] {
                let opts = CommonOpts {
                    known_hosts: Some("hosts".into()),
                    has_remote_command: Some(has_remote_command),
                    ..Default::default()
                };
                for shell in [None, Some("/bin/bash".into())] {
                    let purpose = Purpose::Terminal {
                        login_shell: shell.clone(),
                        cwd_marker: None,
                        integration: true,
                    };
                    let command = build_args(target, &purpose, &opts).unwrap();
                    assert_eq!(command.hooked, !has_remote_command);
                    assert_eq!(
                        command
                            .args
                            .last()
                            .unwrap()
                            .starts_with("exec /bin/sh -c '"),
                        !has_remote_command
                    );
                    assert_eq!(
                        has(&command.args, ["-o", "RemoteCommand=none"]),
                        shell.is_some()
                    );
                    if has_remote_command {
                        let old = build_args(
                            target,
                            &Purpose::Terminal {
                                login_shell: shell,
                                cwd_marker: None,
                                integration: false,
                            },
                            &opts,
                        )
                        .unwrap();
                        assert_eq!(command, old);
                    }
                }
            }
        }
        let purpose = Purpose::Terminal {
            login_shell: None,
            cwd_marker: None,
            integration: true,
        };
        assert_eq!(build(&alias(), purpose).args.last().unwrap(), "example");
    }

    #[test]
    fn 终端目录标记严格校验及setenv() {
        let marker = "Abc_123-xyz";
        let command = build(
            &alias(),
            Purpose::Terminal {
                login_shell: None,
                cwd_marker: Some(marker.into()),
                integration: false,
            },
        );
        assert!(has(&command.args, ["-o", "SendEnv=LC_PIERLINK_TERM"]));
        assert!(
            command
                .env
                .contains(&("LC_PIERLINK_TERM".into(), "Abc_123-xyz".into()))
        );
        let plain = build(
            &alias(),
            Purpose::Terminal {
                login_shell: None,
                cwd_marker: None,
                integration: false,
            },
        );
        assert!(!plain.args.iter().any(|a| a.contains("LC_PIERLINK_TERM")));
        assert!(!plain.env.iter().any(|(k, _)| k == "LC_PIERLINK_TERM"));
        for marker in ["", "x y", "$(secret)", "x\ny", &"a".repeat(65)] {
            let error = build_args(
                &alias(),
                &Purpose::Terminal {
                    login_shell: None,
                    cwd_marker: Some(marker.into()),
                    integration: false,
                },
                &CommonOpts::default(),
            )
            .unwrap_err();
            assert_eq!(error.code, "ssh.invalid_argument");
            assert!(!format!("{error:?}").contains("secret"));
        }
    }
    fn alias() -> SshTarget {
        SshTarget::Alias {
            alias: "example".into(),
        }
    }
    fn manual() -> SshTarget {
        SshTarget::Manual {
            host: "example.com".into(),
            port: 2222,
            user: Some("用户".into()),
            identity_file: Some("/中文 路径/a'$(x)\nkey".into()),
            jump: None,
        }
    }
    fn build(target: &SshTarget, purpose: Purpose) -> SshCommand {
        build_args(target, &purpose, &CommonOpts::default()).unwrap()
    }
    fn has(args: &[String], pair: [&str; 2]) -> bool {
        args.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1])
    }
    #[test]
    fn s21_s22_各用途隔离用户配置() {
        for purpose in [
            Purpose::Background,
            Purpose::Sftp,
            Purpose::Tunnel {
                forwards: vec![],
                readiness: Readiness::ProbeLocalPorts,
            },
            Purpose::Terminal {
                login_shell: None,
                cwd_marker: None,
                integration: false,
            },
            Purpose::Terminal {
                login_shell: Some("/bin/sh".into()),
                cwd_marker: None,
                integration: false,
            },
        ] {
            let args = build(&alias(), purpose.clone()).args;
            let terminal = matches!(purpose, Purpose::Terminal { .. });
            for value in [
                "RequestTTY=no",
                "PermitLocalCommand=no",
                "ForwardAgent=no",
                "ForwardX11=no",
            ] {
                assert_eq!(has(&args, ["-o", value]), !terminal);
            }
            assert_eq!(
                has(&args, ["-o", "ClearAllForwardings=yes"]),
                matches!(
                    purpose,
                    Purpose::Background | Purpose::HostKeyProbe | Purpose::Sftp
                )
            );
            assert_eq!(
                has(&args, ["-o", "RemoteCommand=none"]),
                !matches!(
                    purpose,
                    Purpose::Terminal {
                        login_shell: None,
                        cwd_marker: None,
                        ..
                    }
                )
            );
        }
    }

    #[test]
    fn 显式目标校验错误不泄漏() {
        let mut cfg =
            super::super::config::parse_ssh_g(include_str!("../../tests/data/ssh-g.txt")).unwrap();
        let target = explicit_target(&cfg, "target", Path::new("/app/config")).unwrap();
        let cmd = build_args(
            &SshTarget::Explicit(Box::new(target)),
            &Purpose::Sftp,
            &CommonOpts {
                known_hosts: Some("/app/hosts".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(has(&cmd.args, ["-F", "/app/config"]));
        let target = SshTarget::Explicit(Box::new(
            explicit_target(&cfg, "target", Path::new("/app/config")).unwrap(),
        ));
        assert_eq!(
            build_args(&target, &Purpose::Background, &CommonOpts::default())
                .unwrap_err()
                .code,
            "ssh.known_hosts_required"
        );
        let cmd = build_args(
            &target,
            &Purpose::Background,
            &CommonOpts {
                known_hosts: Some("/app/hosts".into()),
                askpass: Some(AskPass {
                    program: "/app/askpass".into(),
                    registration: crate::ssh::askpass_pipe::test_registration(
                        "pierlink@example.com",
                    ),
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(cmd.env.contains(&(
            "PIERLINK_EXPECTED_PROMPT".into(),
            "pierlink@example.com".into()
        )));
        cfg.hostname = "-secret".into();
        assert!(
            !format!(
                "{:?}",
                explicit_target(&cfg, "target", Path::new("/app/config")).unwrap_err()
            )
            .contains("secret")
        );
    }

    #[test]
    #[cfg(unix)]
    fn s21_s22_真实配置的转发和命令被隔离() {
        // 本次测试的所有文件共用一个数据盘临时根目录，退出时自动删除。
        let root = crate::test_support::tempdir();
        let config = root.path().join("config");
        std::fs::write(&config, "Host example\n HostName example.com\n User alice\n Port 2222\n RemoteCommand echo config\n RequestTTY force\n PermitLocalCommand yes\n LocalCommand echo local\n ForwardAgent yes\n ForwardX11 yes\n LocalForward 19001 localhost:19002\n RemoteForward 19003 localhost:19004\n DynamicForward 19005\n NoHostAuthenticationForLocalhost yes\n ControlMaster auto\n ControlPath /bad/socket\n UpdateHostKeys yes\n ForkAfterAuthentication yes\n StdinNull yes\n LogLevel QUIET\n").unwrap();
        let resolve = |args: &[String], user_config: bool| {
            let mut cmd = std::process::Command::new("ssh");
            cmd.arg("-G");
            if user_config {
                cmd.arg("-F").arg(&config);
            }
            let out = cmd.args(args).output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };
        let chain = root.path().join("chain");
        std::fs::write(&chain, "Host target\n HostName target.internal\n User alice\n ProxyJump middle\n LocalForward 19001 localhost:19002\nHost middle\n HostName middle.internal\n User bob\n ProxyJump outer\nHost outer\n HostName outer.internal\n User carol\n").unwrap();
        let raws: Vec<_> = ["target", "middle", "outer"]
            .iter()
            .map(|host| {
                let out = std::process::Command::new("ssh")
                    .arg("-G")
                    .arg("-F")
                    .arg(&chain)
                    .arg(host)
                    .output()
                    .unwrap();
                assert!(out.status.success());
                String::from_utf8(out.stdout).unwrap()
            })
            .collect();
        let blocks: Vec<_> = ["target", "middle", "outer"]
            .iter()
            .zip(&raws)
            .map(|(h, r)| (h.to_string(), r.as_str()))
            .collect();
        let generated = super::super::isolated::render_config(
            &blocks,
            &[],
            &[],
            std::path::Path::new("/app/hosts"),
        )
        .unwrap();
        std::fs::write(&chain, generated).unwrap();
        for (host, user, proxy) in [
            ("target", "alice", Some("middle")),
            ("middle", "bob", Some("outer")),
            ("outer", "carol", None),
        ] {
            let out = std::process::Command::new("ssh")
                .arg("-G")
                .arg("-F")
                .arg(&chain)
                .arg(host)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let out = String::from_utf8(out.stdout).unwrap();
            assert!(out.contains(&format!("user {user}")));
            assert!(out.contains(&format!("hostname {host}.internal")));
            if let Some(proxy) = proxy {
                assert!(out.contains(&format!("proxyjump {proxy}")));
            }
            assert!(!out.contains("19001"));
        }
        for path in [
            r"C:\Users\John Smith\key",
            r"C:\用户\100%\key",
            r"\\server\share\key",
        ] {
            let raw = format!(
                "hostname example.com\nuser alice\nidentityfile {path}\ncertificatefile {path}\nidentityagent {path}\nuserknownhostsfile broken path"
            );
            std::fs::write(
                &chain,
                super::super::isolated::render_config(
                    &[("target".into(), &raw)],
                    &[path.into()],
                    &[],
                    std::path::Path::new("/app/hosts"),
                )
                .unwrap(),
            )
            .unwrap();
            let cfg = super::super::config::parse_ssh_g(include_str!("../../tests/data/ssh-g.txt"))
                .unwrap();
            let target =
                SshTarget::Explicit(Box::new(explicit_target(&cfg, "target", &chain).unwrap()));
            let command = build_args(
                &target,
                &Purpose::Background,
                &CommonOpts {
                    known_hosts: Some(path.into()),
                    ..Default::default()
                },
            )
            .unwrap();
            let out = resolve(&command.args, false);
            for key in [
                "identityfile",
                "certificatefile",
                "identityagent",
                "userknownhostsfile",
            ] {
                assert!(
                    out.lines().any(|l| l
                        == format!(
                            "{key} {}",
                            if ["identityfile", "certificatefile"].contains(&key) {
                                path.replace('%', "%%")
                            } else {
                                path.to_owned()
                            }
                        )),
                    "{key}: {out}"
                );
            }
        }
        let original = resolve(&["example".into()], true);
        let cfg = super::super::config::parse_ssh_g(&original).unwrap();
        assert!(cfg.has_forwards);
        for purpose in [Purpose::Background, Purpose::Sftp] {
            let out = resolve(&build(&alias(), purpose).args, true);
            for setting in [
                "requesttty false",
                "permitlocalcommand no",
                "forwardagent no",
                "forwardx11 no",
                "clearallforwardings yes",
                "nohostauthenticationforlocalhost no",
                "controlmaster false",
                "updatehostkeys false",
                "forkafterauthentication no",
                "stdinnull no",
                "loglevel ERROR",
            ] {
                assert!(out.lines().any(|l| l == setting), "{setting}: {out}");
            }
            assert!(!out.contains("19001"));
            assert!(!out.contains("19003"));
            assert!(!out.contains("19005"));
            assert!(!out.contains("remotecommand echo config"));
        }
        let isolated = root.path().join("isolated");
        std::fs::write(
            &isolated,
            super::super::isolated::render_config(
                &[("example".into(), original.as_str())],
                &[],
                &[],
                std::path::Path::new("/app/hosts"),
            )
            .unwrap(),
        )
        .unwrap();
        let target = SshTarget::Explicit(Box::new(
            explicit_target(&cfg, "example", &isolated).unwrap(),
        ));
        let out = resolve(
            &build_args(
                &target,
                &Purpose::Tunnel {
                    readiness: Readiness::ProbeLocalPorts,
                    forwards: vec![Forward::Local {
                        local_port: 12000,
                        remote: ForwardTarget::Tcp { port: 22000 },
                    }],
                },
                &CommonOpts {
                    known_hosts: Some(root.path().join("hosts")),
                    ..Default::default()
                },
            )
            .unwrap()
            .args,
            false,
        );
        assert!(out.contains("localforward [127.0.0.1]:12000 [127.0.0.1]:22000"));
        assert!(!out.contains("19001"));
        assert!(!out.contains("19003"));
        assert!(!out.contains("19005"));
        assert!(out.contains("hostname example.com"));
        assert!(out.contains("user alice"));
        assert!(out.contains("port 2222"));
        let out = resolve(
            &build(
                &alias(),
                Purpose::Terminal {
                    login_shell: Some("/bin/sh".into()),
                    cwd_marker: None,
                    integration: false,
                },
            )
            .args,
            true,
        );
        assert!(!out.contains("remotecommand echo config"));
        assert!(out.contains("19001"));
    }

    #[test]
    fn s23_s24_s25_认证隔离和后台安全选项() {
        let opts = CommonOpts {
            has_remote_command: None,
            dedicated_key: None,
            config_file: Some("/app/config".into()),
            known_hosts: Some("/app/hosts".into()),
            askpass: Some(AskPass {
                program: "/app/askpass".into(),
                registration: crate::ssh::askpass_pipe::test_registration("用户@example.com"),
            }),
            connect_timeout: Some(9),
        };
        assert_eq!(
            build_args(&alias(), &Purpose::Background, &opts)
                .unwrap_err()
                .code,
            "ssh.askpass_needs_resolved_target"
        );
        let mut target = manual();
        for purpose in [
            Purpose::Background,
            Purpose::Sftp,
            Purpose::Tunnel {
                forwards: vec![],
                readiness: Readiness::ProbeLocalPorts,
            },
        ] {
            let cmd = build_args(&target, &purpose, &opts).unwrap();
            for setting in [
                "NoHostAuthenticationForLocalhost=no",
                "ControlMaster=no",
                "ControlPath=none",
                "UpdateHostKeys=no",
                "ForkAfterAuthentication=no",
                "StdinNull=no",
                "ConnectTimeout=9",
            ] {
                assert!(has(&cmd.args, ["-o", setting]));
            }
            assert!(has(
                &cmd.args,
                [
                    "-o",
                    if matches!(purpose, Purpose::Tunnel { .. }) {
                        "LogLevel=INFO"
                    } else {
                        "LogLevel=ERROR"
                    }
                ]
            ));
            assert!(has(&cmd.args, ["-F", "none"]));
            assert!(!format!("{cmd:?}").contains("private-reference"));
            assert!(!format!("{cmd:?}").contains("用户@example.com"));
        }
        let cmd = build_args(
            &target,
            &Purpose::Terminal {
                login_shell: None,
                cwd_marker: None,
                integration: false,
            },
            &opts,
        )
        .unwrap();
        assert!(has(&cmd.args, ["-o", "StrictHostKeyChecking=yes"]));
        let mut wrong = opts.clone();
        wrong.askpass.as_mut().unwrap().registration =
            crate::ssh::askpass_pipe::test_registration("jump@evil");
        assert!(build_args(&target, &Purpose::Background, &wrong).is_err());
        if let SshTarget::Manual { jump, .. } = &mut target {
            *jump = Some(Box::new(alias()));
        }
        assert_eq!(
            build_args(&target, &Purpose::Background, &opts)
                .unwrap_err()
                .code,
            "ssh.jump_alias_needs_resolved"
        );
        if let SshTarget::Manual { jump, user, .. } = &mut target {
            *jump = None;
            *user = None;
        }
        assert_eq!(
            build_args(&target, &Purpose::Background, &opts)
                .unwrap_err()
                .code,
            "ssh.askpass_needs_resolved_target"
        );
    }

    #[test]
    fn 名称黑名单和终端白名单() {
        for good in ["user@corp.local", "用户", "a+b", "a=b"] {
            assert!(name(good, "user", false).is_ok());
        }
        for bad in [
            "-bad", "a%h", "a b", "a\tb", "a\u{7f}", "a$", "a`", "a\\", "a;", "a|", "a{", "a}",
            "a(", "a)", "a<", "a>", "a&", "a\"", "a'",
        ] {
            assert!(name(bad, "user", false).is_err(), "{bad:?}");
        }
        for bad in ["/bin/a b", "fish;id", "$(id)", "中文", "a'", "a\n"] {
            assert!(terminal_command(&Some(bad.into()), true).is_err());
        }
        assert_eq!(
            config_word(r"C:\100%\hosts", "path").unwrap(),
            r#""C:\\100%%\\hosts""#
        );
        let cmd = SshCommand {
            config_text: None,
            hooked: false,
            builtin: None,
            askpass: None,
            args: vec!["ProxyCommand=helper --credential sensitive-value".into()],
            env: vec![("SSH_ASKPASS".into(), "hidden-program".into())],
        };
        let debug = format!("{cmd:?}");
        assert!(!debug.contains("sensitive-value"));
        assert!(!debug.contains("hidden-program"));
    }

    #[test]
    fn 所有用途的参数和认证策略() {
        for purpose in [
            Purpose::Background,
            Purpose::Sftp,
            Purpose::Tunnel {
                forwards: vec![],
                readiness: Readiness::ProbeLocalPorts,
            },
            Purpose::Terminal {
                login_shell: None,
                cwd_marker: None,
                integration: false,
            },
        ] {
            let cmd = build(&alias(), purpose.clone());
            assert!(has(&cmd.args, ["-o", "ServerAliveInterval=15"]));
            assert!(has(&cmd.args, ["-o", "ServerAliveCountMax=3"]));
            let terminal = matches!(purpose, Purpose::Terminal { .. });
            assert_eq!(has(&cmd.args, ["-o", "BatchMode=yes"]), !terminal);
            assert_eq!(
                has(&cmd.args, ["-o", "StrictHostKeyChecking=yes"]),
                !terminal
            );
            assert!(cmd.env.is_empty());
            match purpose {
                Purpose::HostKeyProbe => unreachable!(),
                Purpose::Background => assert!(has(&cmd.args, ["example", "/bin/sh -s"])),
                Purpose::Sftp => {
                    assert!(cmd.args.contains(&"-s".into()));
                    assert!(has(&cmd.args, ["example", "sftp"]));
                }
                Purpose::Terminal { .. } => {
                    assert!(cmd.args.contains(&"-t".into()));
                    assert_eq!(cmd.args.last().unwrap(), "example");
                }
                Purpose::Tunnel { .. } => assert!(cmd.args.contains(&"-N".into())),
            }
        }
    }
    #[test]
    fn p1_转发只监听回环且失败退出() {
        let cmd = build(
            &alias(),
            Purpose::Tunnel {
                readiness: Readiness::ProbeLocalPorts,
                forwards: vec![
                    Forward::Local {
                        local_port: 12000,
                        remote: ForwardTarget::Tcp { port: 22000 },
                    },
                    Forward::Local {
                        local_port: 12001,
                        remote: ForwardTarget::Unix {
                            path: "/run/中文 空格/'$(x).sock".into(),
                        },
                    },
                ],
            },
        );
        assert!(has(&cmd.args, ["-o", "ExitOnForwardFailure=yes"]));
        assert!(has(&cmd.args, ["-L", "127.0.0.1:12000:127.0.0.1:22000"]));
        assert!(has(
            &cmd.args,
            ["-L", "127.0.0.1:12001:/run/中文 空格/'$(x).sock"]
        ));
    }
    #[test]
    fn askpass_只传引用且覆盖批处理配置() {
        let opts = CommonOpts {
            has_remote_command: None,
            dedicated_key: None,
            known_hosts: Some("C:\\中文 空格\\known_hosts".into()),
            askpass: Some(AskPass {
                program: "C:\\程序 空格\\askpass.exe".into(),
                registration: crate::ssh::askpass_pipe::test_registration("用户@example.com"),
            }),
            connect_timeout: None,
            config_file: None,
        };
        for purpose in [
            Purpose::Background,
            Purpose::Sftp,
            Purpose::Tunnel {
                forwards: vec![],
                readiness: Readiness::ProbeLocalPorts,
            },
            Purpose::Terminal {
                login_shell: None,
                cwd_marker: None,
                integration: false,
            },
        ] {
            let cmd = build_args(&manual(), &purpose, &opts).unwrap();
            assert!(!has(&cmd.args, ["-o", "BatchMode=yes"]));
            assert!(has(&cmd.args, ["-o", "BatchMode=no"]));
            assert!(has(&cmd.args, ["-o", "NumberOfPasswordPrompts=1"]));
            assert!(has(
                &cmd.args,
                [
                    "-o",
                    "UserKnownHostsFile=\"C:\\\\中文 空格\\\\known_hosts\""
                ]
            ));
            assert_eq!(
                cmd.env,
                vec![
                    ("PIERLINK_EXPECTED_PROMPT".into(), "用户@example.com".into()),
                    ("SSH_ASKPASS".into(), "C:\\程序 空格\\askpass.exe".into()),
                    ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
                    ("PIERLINK_ASKPASS_PIPE".into(), "test-pipe".into()),
                    (
                        "PIERLINK_ASKPASS_NONCE".into(),
                        opts.askpass.as_ref().unwrap().registration.nonce().into()
                    )
                ]
            );
            assert!(!cmd.args.iter().any(|s| s.contains("pierlink/server")));
        }
    }
    #[test]
    fn s23_手动与显式目标主机名小写且保留用户名() {
        for host in ["LOCALHOST", "MiXeD.Example.COM", "127.0.0.1", "2001:DB8::A"] {
            let targets = [
                SshTarget::Manual {
                    host: host.into(),
                    port: 22,
                    user: Some("Alice".into()),
                    identity_file: None,
                    jump: None,
                },
                SshTarget::Explicit(Box::new(ExplicitTarget {
                    host: "alias".into(),
                    hostname: host.into(),
                    user: "Alice".into(),
                    config_file: "config".into(),
                    has_remote_command: false,
                })),
            ];
            for target in targets {
                for spelling in [host.to_owned(), host.to_ascii_lowercase()] {
                    let mut opts = CommonOpts {
                        askpass: Some(AskPass {
                            program: "askpass".into(),
                            registration: crate::ssh::askpass_pipe::test_registration(
                                &(format!("Alice@{spelling}")),
                            ),
                        }),
                        known_hosts: Some("hosts".into()),
                        ..Default::default()
                    };
                    let command = build_args(&target, &Purpose::Background, &opts).unwrap();
                    assert!(command.env.contains(&(
                        "PIERLINK_EXPECTED_PROMPT".into(),
                        format!("Alice@{}", host.to_ascii_lowercase())
                    )));
                    opts.askpass.as_mut().unwrap().registration =
                        crate::ssh::askpass_pipe::test_registration(&format!("alice@{spelling}"));
                    assert!(build_args(&target, &Purpose::Background, &opts).is_err());
                }
            }
        }
    }
    #[test]
    fn 无跳板手动目标保持参数不变() {
        let target = manual();
        let cmd = build(&target, Purpose::Background);
        assert!(has(&cmd.args, ["-p", "2222"]));
        assert!(has(&cmd.args, ["-l", "用户"]));
        assert!(has(&cmd.args, ["-i", "/中文 路径/a'$(x)\nkey"]));
        assert!(has(&cmd.args, ["-o", "IdentitiesOnly=yes"]));
        assert!(cmd.config_text.is_none());
    }
    #[test]
    fn s9_拒绝选项和无效目标() {
        for bad in [
            "-V", "", "a\0b", "a\nb", "a b", "$(id)", "a'b", "x,y", "u@host",
        ] {
            for target in [
                SshTarget::Alias { alias: bad.into() },
                SshTarget::Manual {
                    host: bad.into(),
                    port: 22,
                    user: None,
                    identity_file: None,
                    jump: None,
                },
            ] {
                assert!(build_args(&target, &Purpose::Background, &CommonOpts::default()).is_err());
            }
        }
        let mut target = manual();
        if let SshTarget::Manual { jump, .. } = &mut target {
            *jump = Some(Box::new(manual()));
        }
        assert!(build_args(&target, &Purpose::Background, &CommonOpts::default()).is_err());
    }
    #[test]
    fn s9_终端命令统一转义() {
        let shell = "/bin/bash";
        let command = build(
            &alias(),
            Purpose::Terminal {
                login_shell: Some(shell.into()),
                cwd_marker: None,
                integration: false,
            },
        );
        assert_eq!(
            command.args.last().unwrap(),
            &format!("exec {} -l", sh_quote(shell).unwrap())
        );
    }
    #[test]
    fn 拒绝无效端口和路径() {
        for remote in [
            ForwardTarget::Tcp { port: 0 },
            ForwardTarget::Unix {
                path: "/a:b".into(),
            },
            ForwardTarget::Unix {
                path: "relative".into(),
            },
            ForwardTarget::Unix {
                path: "/a\0b".into(),
            },
        ] {
            assert!(
                build_args(
                    &alias(),
                    &Purpose::Tunnel {
                        readiness: Readiness::ProbeLocalPorts,
                        forwards: vec![Forward::Local {
                            local_port: 1,
                            remote
                        }]
                    },
                    &CommonOpts::default()
                )
                .is_err()
            );
        }
        assert!(
            build_args(
                &alias(),
                &Purpose::Tunnel {
                    readiness: Readiness::ProbeLocalPorts,
                    forwards: vec![Forward::Local {
                        local_port: 0,
                        remote: ForwardTarget::Tcp { port: 1 }
                    }]
                },
                &CommonOpts::default()
            )
            .is_err()
        );
        assert!(
            build_args(
                &alias(),
                &Purpose::Background,
                &CommonOpts {
                    has_remote_command: None,
                    dedicated_key: None,
                    known_hosts: Some("a\"\nProxyCommand secret".into()),
                    askpass: None,
                    connect_timeout: None,
                    config_file: None,
                }
            )
            .is_err()
        );
        assert!(
            build_args(
                &alias(),
                &Purpose::Terminal {
                    login_shell: Some("x\0secret".into()),
                    cwd_marker: None,
                    integration: false,
                },
                &CommonOpts::default()
            )
            .is_err()
        );
    }
    #[test]
    #[cfg(unix)]
    fn 参数被真实_openssh_接受() {
        let opts = CommonOpts {
            has_remote_command: None,
            dedicated_key: None,
            known_hosts: Some("/中文 空格/a'hosts".into()),
            askpass: None,
            connect_timeout: None,
            config_file: None,
        };
        let purpose = Purpose::Tunnel {
            readiness: Readiness::ProbeLocalPorts,
            forwards: vec![
                Forward::Local {
                    local_port: 12000,
                    remote: ForwardTarget::Tcp { port: 22000 },
                },
                Forward::Local {
                    local_port: 12001,
                    remote: ForwardTarget::Unix {
                        path: "/run/中文 空格/a'$(id).sock".into(),
                    },
                },
            ],
        };
        let command = build_args(&alias(), &purpose, &opts).unwrap();
        let output = std::process::Command::new("ssh")
            .args(["-G", "-F", "/dev/null"])
            .args(&command.args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(output.contains("localforward [127.0.0.1]:12000 [127.0.0.1]:22000"));
        assert!(output.contains("/run/中文 空格/a'$(id).sock"));
        assert!(output.contains("userknownhostsfile /中文 空格/a'hosts"));
        assert!(output.contains("exitonforwardfailure yes"));
        assert!(
            output.contains("stricthostkeychecking true")
                || output.contains("stricthostkeychecking yes")
        );
    }
}

#[cfg(test)]
mod dedicated_tests {
    use super::*;
    #[test]
    fn s13_s18_所有用途仅用专用密钥且密码互斥() {
        let target = SshTarget::Manual {
            host: "host".into(),
            user: Some("user".into()),
            port: 22,
            identity_file: Some("old-key".into()),
            jump: None,
        };
        let mut opts = CommonOpts {
            dedicated_key: Some(DedicatedKey {
                private_path: "/a b/key".into(),
                askpass_program: "/app".into(),
                credential_id: "pierlink/local/key".into(),
            }),
            ..Default::default()
        };
        for purpose in [
            Purpose::Background,
            Purpose::Terminal {
                login_shell: None,
                cwd_marker: None,
                integration: false,
            },
            Purpose::Sftp,
            Purpose::Tunnel {
                readiness: Readiness::ProbeLocalPorts,
                forwards: vec![Forward::Local {
                    local_port: 4000,
                    remote: ForwardTarget::Tcp { port: 22 },
                }],
            },
        ] {
            let cmd = build_args(&target, &purpose, &opts).unwrap();
            for required in [
                "IdentitiesOnly=yes",
                "IdentityAgent=none",
                "BatchMode=no",
                "NumberOfPasswordPrompts=1",
                "PreferredAuthentications=publickey",
                "PasswordAuthentication=no",
                "KbdInteractiveAuthentication=no",
            ] {
                assert!(cmd.args.iter().any(|a| a == required));
            }
            assert!(
                !cmd.args
                    .iter()
                    .any(|a| a == "old-key" || a == "BatchMode=yes")
            );
            assert!(
                cmd.env
                    .contains(&("SSH_ASKPASS_REQUIRE".into(), "force".into()))
            );
        }
        opts.askpass = Some(AskPass {
            program: "/app".into(),
            registration: crate::ssh::askpass_pipe::test_registration("user@host"),
        });
        assert_eq!(
            build_args(&target, &Purpose::Background, &opts)
                .unwrap_err()
                .code,
            "ssh.invalid_argument"
        );
    }
    #[test]
    fn 专用密钥从解析配置移除目标旧身份但保留跳板() {
        let root = crate::test_support::tempdir();
        let config = root.path().join("config");
        std::fs::write(&config, "Host target\n HostName host\n User user\n IdentityFile old\n CertificateFile old-cert\nHost jump\n HostName jump\n IdentityFile jump-key\n").unwrap();
        let target = SshTarget::Explicit(Box::new(ExplicitTarget {
            host: "target".into(),
            hostname: "host".into(),
            user: "user".into(),
            config_file: config,
            has_remote_command: false,
        }));
        let opts = CommonOpts {
            dedicated_key: Some(DedicatedKey {
                private_path: "/key".into(),
                askpass_program: "/app".into(),
                credential_id: "ref".into(),
            }),
            known_hosts: Some("hosts".into()),
            ..Default::default()
        };
        let result = build_args(&target, &Purpose::Background, &opts).unwrap();
        let config = result.config_text.unwrap();
        assert!(!config.contains("old"));
        assert!(config.contains("jump-key"));
        assert!(
            build_args(
                &SshTarget::Alias {
                    alias: "target".into()
                },
                &Purpose::Background,
                &opts
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod forward_tests {
    use super::*;

    fn command(forwards: Vec<Forward>) -> SshCommand {
        build_args(
            &SshTarget::Alias {
                alias: "destination".into(),
            },
            &Purpose::Tunnel {
                forwards,
                readiness: Readiness::AuthMarker,
            },
            &CommonOpts::default(),
        )
        .unwrap()
    }

    #[test]
    fn 三种转发参数快照与日志级别() {
        for (forward, flag, value) in [
            (
                Forward::Local {
                    local_port: 18080,
                    remote: ForwardTarget::Host {
                        host: "db.internal".into(),
                        port: 5432,
                    },
                },
                "-L",
                "127.0.0.1:18080:db.internal:5432",
            ),
            (
                Forward::Remote {
                    remote_port: 28080,
                    local_port: 8080,
                },
                "-R",
                "127.0.0.1:28080:127.0.0.1:8080",
            ),
            (
                Forward::Dynamic { local_port: 1080 },
                "-D",
                "127.0.0.1:1080",
            ),
        ] {
            let cmd = command(vec![forward]);
            assert_eq!(
                &cmd.args[cmd.args.len() - 6..],
                [
                    "-N",
                    "-o",
                    "ExitOnForwardFailure=yes",
                    flag,
                    value,
                    "destination"
                ]
            );
            assert!(cmd.args.windows(2).any(|a| a == ["-o", "LogLevel=VERBOSE"]));
            assert!(!cmd.args.iter().any(|a| a.contains("ClearAllForwardings")));
        }
    }

    #[test]
    fn 非法目标和零端口拒绝_ipv6只加一层方括号() {
        for host in [
            "",
            "-x",
            "db host",
            "user@db",
            "db'host",
            "db\"host",
            "host:80",
            "[::1",
            "::1]",
            "db\nsecret",
            "db\0secret",
            "a/b",
            "/run/db.sock",
            "db\\host",
            "db?host",
            "db*host",
            "db#host",
            "db!host",
            "db=host",
            "数据库",
            "[db]",
            "fe80::1%eth0",
        ] {
            let error = forward_target(&ForwardTarget::Host {
                host: host.into(),
                port: 80,
            })
            .unwrap_err();
            assert_eq!(error.code, "ssh.invalid_argument", "{host:?}");
            assert_eq!(error.params["field"], "forward.host");
            assert_eq!(error.params.len(), 1);
        }
        for host in [
            "localhost",
            "db-01.internal",
            "db_01.internal.",
            "127.0.0.1",
        ] {
            assert_eq!(
                forward_target(&ForwardTarget::Host {
                    host: host.into(),
                    port: 80,
                })
                .unwrap(),
                format!("{host}:80")
            );
        }
        for host in ["::1", "[::1]", "2001:db8::1", "[2001:db8::1]"] {
            let cmd = command(vec![Forward::Local {
                local_port: 18080,
                remote: ForwardTarget::Host {
                    host: host.into(),
                    port: 80,
                },
            }]);
            let literal = host.trim_matches(['[', ']']);
            assert!(
                cmd.args
                    .contains(&format!("127.0.0.1:18080:[{literal}]:80"))
            );
        }
        for forward in [
            Forward::Remote {
                remote_port: 0,
                local_port: 80,
            },
            Forward::Remote {
                remote_port: 80,
                local_port: 0,
            },
            Forward::Dynamic { local_port: 0 },
            Forward::Local {
                local_port: 80,
                remote: ForwardTarget::Host {
                    host: "db".into(),
                    port: 0,
                },
            },
        ] {
            assert!(
                build_args(
                    &SshTarget::Alias {
                        alias: "host".into()
                    },
                    &Purpose::Tunnel {
                        forwards: vec![forward],
                        readiness: Readiness::AuthMarker
                    },
                    &CommonOpts::default()
                )
                .is_err()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn 三种转发和ipv6被真实openssh接受() {
        let cmd = command(vec![
            Forward::Local {
                local_port: 18080,
                remote: ForwardTarget::Host {
                    host: "::1".into(),
                    port: 80,
                },
            },
            Forward::Remote {
                remote_port: 28080,
                local_port: 8080,
            },
            Forward::Dynamic { local_port: 1080 },
        ]);
        let output = std::process::Command::new("/usr/bin/ssh")
            .args(["-G", "-F", "none"])
            .args(cmd.args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let config = String::from_utf8(output.stdout).unwrap();
        assert!(config.contains("localforward [127.0.0.1]:18080 [::1]:80"));
        assert!(config.contains("remoteforward [127.0.0.1]:28080 [127.0.0.1]:8080"));
        assert!(config.contains("dynamicforward [127.0.0.1]:1080"));
    }
}
