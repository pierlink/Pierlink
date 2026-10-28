//! 显式预览并转换链接型配置；不会在启动或数据库迁移时调用 SSH。
use super::{resolve::capture, validate_alias};
use crate::{
    error::AppError,
    process::SshProgram,
    ssh::config::{ResolvedConfig, parse_ssh_g},
    storage::{AuthMethod, Db, Engine, Server, ServerSource},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use specta::Type;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

/// 从 SSH 有效配置冻结的连接字段，identity_files 保留原顺序。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ConvertedHost {
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub identity_files: Vec<String>,
    pub identities_only: bool,
    pub host_key_alias: Option<String>,
}

/// 一条拒绝原因；code 和 option 均为固定标识，hop 指向目标或跳板别名。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ConversionReason {
    pub code: String,
    pub option: String,
    pub hop: String,
}

/// 跳板按最外层到最内层排列；existing_server_id 为空表示执行时需要新建。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ConversionJump {
    pub fields: ConvertedHost,
    pub existing_server_id: Option<String>,
    /// 此级复用的导入服务器将在执行时一起转换为内置连接。
    pub converts_existing: bool,
}

/// 只读预览；reasons 为空才可执行，摘要用于检测配置或数据库变动。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ConversionPreview {
    pub server_id: String,
    pub target: ConvertedHost,
    pub jumps: Vec<ConversionJump>,
    pub reasons: Vec<ConversionReason>,
    resolution_digest: String,
    records_digest: String,
}

/// 转换后的目标和全部跳板记录，顺序与预览一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ConversionResult {
    pub server: Server,
    pub jumps: Vec<Server>,
}

/// 用 program 解析导入记录 server 及逐级跳板，并读取 db 决定复用或新建。
/// 不修改数据库；解析失败返回 AppError，不支持的配置返回固定原因列表。
/// 与内置引擎一致，目标和跳板合计最多四台。
pub fn preview_conversion(
    program: &SshProgram,
    db: &Db,
    server: &Server,
) -> Result<ConversionPreview, AppError> {
    let records = db.list_servers()?;
    preview_with(server, &records, &mut Resolver::new(program))
}

/// 重新解析 server 并与 preview 比较，在 db 的同一事务中转换目标和跳板。
/// now_ms 是更新时间；任何配置、预览或服务器记录变动返回 ssh.conversion_changed。
pub fn execute_conversion(
    program: &SshProgram,
    db: &mut Db,
    server: &Server,
    preview: &ConversionPreview,
    now_ms: i64,
) -> Result<ConversionResult, AppError> {
    let records = db.list_servers()?;
    let current = preview_with(server, &records, &mut Resolver::new(program))?;
    if &current != preview {
        return Err(AppError::new("ssh.conversion_changed"));
    }
    if !current.reasons.is_empty() {
        return Err(AppError::new("ssh.conversion_unsupported"));
    }
    apply(db, server, &records, &current, now_ms)
}

#[derive(Clone)]
struct JumpSpec {
    alias: String,
    user: Option<String>,
    port: Option<u16>,
}

// 使用现有的有界进程读取、超时与进程树清理，只增加转换所需的诊断和基线解析。
trait Resolve {
    fn get(
        &mut self,
        spec: &JumpSpec,
        defaults: bool,
    ) -> Result<(ResolvedConfig, bool, String), AppError>;
}
struct Resolver<'a> {
    program: &'a SshProgram,
    start: Instant,
}
impl<'a> Resolver<'a> {
    fn new(program: &'a SshProgram) -> Self {
        Self {
            program,
            start: Instant::now(),
        }
    }
}
impl Resolve for Resolver<'_> {
    fn get(
        &mut self,
        spec: &JumpSpec,
        defaults: bool,
    ) -> Result<(ResolvedConfig, bool, String), AppError> {
        validate_alias(&spec.alias)?;
        let remaining = Duration::from_secs(30)
            .checked_sub(self.start.elapsed())
            .ok_or_else(|| AppError::new("ssh.resolve_timeout"))?;
        let mut args = vec!["-G".into()];
        if defaults {
            args.extend(["-F".into(), "none".into()]);
        } else {
            args.push("-vvv".into());
        }
        if let Some(user) = &spec.user {
            args.extend(["-l".into(), user.clone()]);
        }
        if let Some(port) = spec.port {
            args.extend(["-p".into(), port.to_string()]);
        }
        args.extend(["--".into(), spec.alias.clone()]);
        let (output, diagnostics) = capture(
            self.program,
            args,
            remaining.min(Duration::from_secs(5)),
            &AtomicBool::new(false),
        )?;
        let has_exec = match_exec(&diagnostics);
        Ok((parse_ssh_g(&output)?, has_exec, output))
    }
}

fn match_exec(diagnostics: &str) -> bool {
    diagnostics.lines().any(|line| {
        line.starts_with("debug2: checking match for '")
            && super::words(line.trim_start_matches("debug2: checking match for '"))
                .iter()
                .any(|word| matches!(word.trim_matches('\''), "exec" | "!exec"))
    })
}

fn reason(reasons: &mut Vec<ConversionReason>, hop: &str, code: &str, option: &str) {
    let item = ConversionReason {
        code: code.into(),
        option: option.into(),
        hop: hop.into(),
    };
    if !reasons.contains(&item) {
        reasons.push(item);
    }
}

fn blacklist(
    cfg: &ResolvedConfig,
    defaults: &ResolvedConfig,
    has_exec: bool,
    alias: &str,
    reasons: &mut Vec<ConversionReason>,
) {
    let options = &cfg.conversion_options;
    for (key, label) in [
        ("proxycommand", "ProxyCommand"),
        ("localforward", "LocalForward"),
        ("remoteforward", "RemoteForward"),
        ("dynamicforward", "DynamicForward"),
        ("certificatefile", "CertificateFile"),
        ("pkcs11provider", "PKCS11Provider"),
        ("knownhostscommand", "KnownHostsCommand"),
        ("bindaddress", "BindAddress"),
        ("remotecommand", "RemoteCommand"),
    ] {
        if options
            .get(key)
            .is_some_and(|values| values.iter().any(|v| !v.eq_ignore_ascii_case("none")))
        {
            reason(reasons, alias, "ssh.conversion_unsupported_option", label);
        }
    }
    // SendEnv 不拦：它只是请服务器接收本机的环境变量（Debian/Ubuntu 的系统配置默认就有
    // `SendEnv LANG LC_*`），内置 SSH 不发也不影响登录和使用。SetEnv 是写明要设的值，转过去会丢，要拦
    if options.contains_key("setenv") {
        reason(
            reasons,
            alias,
            "ssh.conversion_unsupported_option",
            "SetEnv",
        );
    }
    for (key, label, allowed) in [
        ("requesttty", "RequestTTY", &["auto"][..]),
        ("forwardagent", "ForwardAgent", &["no", "false", "none"][..]),
        ("forwardx11", "ForwardX11", &["no", "false"][..]),
    ] {
        if options.get(key).is_some_and(|values| {
            values
                .iter()
                .any(|v| !allowed.iter().any(|a| v.eq_ignore_ascii_case(a)))
        }) {
            reason(reasons, alias, "ssh.conversion_unsupported_option", label);
        }
    }
    if options
        .get("permitlocalcommand")
        .is_some_and(|v| v.iter().any(|v| v.eq_ignore_ascii_case("yes")))
        && options
            .get("localcommand")
            .is_some_and(|v| v.iter().any(|v| !v.eq_ignore_ascii_case("none")))
    {
        reason(
            reasons,
            alias,
            "ssh.conversion_unsupported_option",
            "LocalCommand",
        );
    }
    for (key, label) in [
        ("securitykeyprovider", "SecurityKeyProvider"),
        ("identityagent", "IdentityAgent"),
        ("userknownhostsfile", "UserKnownHostsFile"),
        ("ciphers", "Ciphers"),
        ("kexalgorithms", "KexAlgorithms"),
        ("hostkeyalgorithms", "HostKeyAlgorithms"),
        ("macs", "MACs"),
        ("pubkeyacceptedalgorithms", "PubkeyAcceptedAlgorithms"),
    ] {
        if options.get(key) != defaults.conversion_options.get(key) {
            reason(reasons, alias, "ssh.conversion_nondefault_option", label);
        }
    }
    if options
        .get("gssapiauthentication")
        .is_some_and(|v| v.iter().any(|v| v.eq_ignore_ascii_case("yes")))
    {
        reason(
            reasons,
            alias,
            "ssh.conversion_unsupported_option",
            "GSSAPIAuthentication",
        );
    }
    if has_exec
        || options.get("match").is_some_and(|v| {
            v.iter().any(|v| {
                super::words(v)
                    .iter()
                    .any(|w| matches!(w.as_str(), "exec" | "!exec"))
            })
        })
    {
        reason(
            reasons,
            alias,
            "ssh.conversion_unsupported_option",
            "Match exec",
        );
    }
}

fn jump_spec(text: &str) -> Result<JumpSpec, AppError> {
    let invalid = || AppError::new("ssh.conversion_invalid_jump").with("option", "ProxyJump");
    let (user, host) = match text.rsplit_once('@') {
        Some((user, host)) if !user.is_empty() && !user.contains('@') => {
            (Some(user.to_owned()), host)
        }
        Some(_) => return Err(invalid()),
        None => (None, text),
    };
    let (alias, port) = if let Some(host) = host.strip_prefix('[') {
        let (alias, suffix) = host.split_once(']').ok_or_else(invalid)?;
        (
            alias,
            if suffix.is_empty() {
                None
            } else {
                Some(suffix.strip_prefix(':').ok_or_else(invalid)?)
            },
        )
    } else if host.matches(':').count() <= 1 {
        match host.split_once(':') {
            Some((alias, port)) => (alias, Some(port)),
            None => (host, None),
        }
    } else {
        return Err(invalid());
    };
    validate_alias(alias).map_err(|_| invalid())?;
    if user
        .as_ref()
        .is_some_and(|s| s.chars().any(|c| c.is_control() || c.is_whitespace()))
    {
        return Err(invalid());
    }
    let port = port
        .map(|s| s.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(invalid))
        .transpose()?;
    Ok(JumpSpec {
        alias: alias.into(),
        user,
        port,
    })
}

fn identity_path(
    value: &str,
    cfg: &ResolvedConfig,
    alias: &str,
    local_user: &str,
) -> Option<String> {
    if value.eq_ignore_ascii_case("none") {
        return Some("none".into());
    }
    let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).ok();
    let mut expanded = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next()? {
                '%' => expanded.push('%'),
                'd' => expanded.push_str(home.as_deref()?),
                'h' => expanded.push_str(&cfg.hostname),
                'n' => expanded.push_str(alias),
                'p' => expanded.push_str(&cfg.port.to_string()),
                'r' => expanded.push_str(&cfg.user),
                'u' => expanded.push_str(local_user),
                _ => return None,
            }
        } else if c == '$' && chars.peek() == Some(&'{') {
            chars.next();
            let mut name = String::new();
            loop {
                let next = chars.next()?;
                if next == '}' {
                    break;
                }
                name.push(next);
            }
            expanded.push_str(&std::env::var(name).ok()?);
        } else {
            expanded.push(c);
        }
    }
    let path = if expanded == "~" {
        PathBuf::from(home?)
    } else if let Some(rest) = expanded
        .strip_prefix("~/")
        .or_else(|| expanded.strip_prefix("~\\"))
    {
        PathBuf::from(home?).join(rest)
    } else if expanded.starts_with('~') {
        return None;
    } else {
        PathBuf::from(expanded)
    };
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    path.to_str()
        .filter(|p| !p.chars().any(char::is_control))
        .map(str::to_owned)
}

struct Builder<'a, R> {
    resolver: &'a mut R,
    defaults: ResolvedConfig,
    hosts: Vec<ConvertedHost>,
    reasons: Vec<ConversionReason>,
    stack: HashSet<String>,
    digest: Sha256,
}
impl<R: Resolve> Builder<'_, R> {
    fn collect(&mut self, spec: JumpSpec, prefix: Option<Vec<JumpSpec>>) -> Result<(), AppError> {
        if self.stack.len() + self.hosts.len() >= 4 || !self.stack.insert(spec.alias.clone()) {
            reason(
                &mut self.reasons,
                &spec.alias,
                "ssh.conversion_jump_cycle_or_depth",
                "ProxyJump",
            );
            return Ok(());
        }
        let (cfg, has_exec, raw) = self.resolver.get(&spec, false)?;
        self.digest.update((raw.len() as u64).to_be_bytes());
        self.digest.update(raw.as_bytes());
        self.digest.update([u8::from(has_exec)]);
        blacklist(
            &cfg,
            &self.defaults,
            has_exec,
            &spec.alias,
            &mut self.reasons,
        );
        let jumps = if let Some(prefix) = prefix {
            prefix
        } else {
            cfg.proxy_jump
                .as_deref()
                .map(|s| s.split(',').map(jump_spec).collect::<Result<Vec<_>, _>>())
                .transpose()?
                .unwrap_or_default()
        };
        let mut jumps = jumps;
        if let Some(last) = jumps.pop() {
            // 显式逗号链覆盖后续跳板自带的 ProxyJump，第一跳仍可使用自己的配置。
            let prefix = (!jumps.is_empty()).then_some(jumps);
            self.collect(last, prefix)?;
        }
        let mut identity_files = Vec::new();
        for file in &cfg.identity_files {
            if let Some(path) = identity_path(file, &cfg, &spec.alias, &self.defaults.user) {
                identity_files.push(path);
            } else {
                reason(
                    &mut self.reasons,
                    &spec.alias,
                    "ssh.conversion_unresolved_path",
                    "IdentityFile",
                );
            }
        }
        if cfg.hostname.starts_with('-')
            || cfg
                .hostname
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
            || cfg
                .user
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
        {
            reason(
                &mut self.reasons,
                &spec.alias,
                "ssh.conversion_invalid_endpoint",
                "HostName",
            );
        }
        if cfg.host_key_alias.as_ref().is_some_and(|s| {
            s.is_empty()
                || s.len() > 1024
                || s.chars()
                    .any(|c| c.is_control() || c.is_whitespace() || ",!*?|#@".contains(c))
        }) {
            reason(
                &mut self.reasons,
                &spec.alias,
                "ssh.conversion_unsupported_option",
                "HostKeyAlias",
            );
        }
        self.hosts.push(ConvertedHost {
            alias: spec.alias.clone(),
            host: cfg.hostname,
            port: cfg.port,
            username: cfg.user,
            identity_files,
            identities_only: cfg.identities_only,
            host_key_alias: cfg.host_key_alias,
        });
        self.stack.remove(&spec.alias);
        Ok(())
    }
}

fn records_digest(records: &[Server]) -> Result<String, AppError> {
    let bytes = serde_json::to_vec(records).map_err(|_| AppError::new("storage.json"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn preview_with(
    server: &Server,
    records: &[Server],
    resolver: &mut impl Resolve,
) -> Result<ConversionPreview, AppError> {
    if server.source != ServerSource::SshConfig {
        return Err(AppError::new("ssh.conversion_not_imported"));
    }
    if !records.iter().any(|s| s == server) {
        return Err(AppError::new("ssh.conversion_changed"));
    }
    let alias = server
        .ssh_alias
        .as_ref()
        .ok_or_else(|| AppError::new("ssh.conversion_not_imported"))?;
    validate_alias(alias)?;
    let spec = JumpSpec {
        alias: alias.clone(),
        user: None,
        port: None,
    };
    let (defaults, _, raw) = resolver.get(&spec, true)?;
    let mut builder = Builder {
        resolver,
        defaults,
        hosts: Vec::new(),
        reasons: Vec::new(),
        stack: HashSet::new(),
        digest: Sha256::new(),
    };
    builder.digest.update(raw.as_bytes());
    builder.collect(spec, None)?;
    let target = builder
        .hosts
        .pop()
        .ok_or_else(|| AppError::new("ssh.conversion_invalid_jump"))?;
    let mut jumps = Vec::new();
    let mut previous: Option<String> = None;
    for fields in builder.hosts {
        let existing = records
            .iter()
            .find(|s| {
                s.id != server.id
                    && s.source == ServerSource::SshConfig
                    && s.ssh_alias.as_deref() == Some(&fields.alias)
            })
            .or_else(|| {
                records.iter().find(|s| {
                    s.id != server.id
                        && (jumps.is_empty() || previous.is_some())
                        && same_host(s, &fields)
                        && s.jump_server_id == previous
                })
            });
        previous = existing.map(|s| s.id.clone());
        jumps.push(ConversionJump {
            fields,
            existing_server_id: previous.clone(),
            converts_existing: existing.is_some_and(|s| s.source == ServerSource::SshConfig),
        });
    }
    let mut reasons = builder.reasons;
    // 跳板后面的服务器在连接前没法按跳板确认指纹，系统 OpenSSH 兜底也只会直连；暂不转换，继续用系统 OpenSSH
    if !jumps.is_empty() {
        reason(
            &mut reasons,
            &target.alias,
            "ssh.conversion_unsupported_option",
            "ProxyJump",
        );
    }
    Ok(ConversionPreview {
        server_id: server.id.clone(),
        target,
        jumps,
        reasons,
        resolution_digest: format!("{:x}", builder.digest.finalize()),
        records_digest: records_digest(records)?,
    })
}

fn same_host(server: &Server, fields: &ConvertedHost) -> bool {
    server.source == ServerSource::Manual
        && server.ssh_alias.is_none()
        && server.connection_engine == Engine::Builtin
        && server.host.as_ref() == Some(&fields.host)
        && server.port == Some(fields.port)
        && server.username.as_ref() == Some(&fields.username)
        && server.identity_files == fields.identity_files
        && server.identities_only == fields.identities_only
        && server.host_key_alias == fields.host_key_alias
        && server.auth_method == AuthMethod::Agent
}
fn copy_fields(server: &mut Server, fields: &ConvertedHost, jump: Option<String>, now_ms: i64) {
    server.source = ServerSource::Manual;
    server.ssh_alias = None;
    server.connection_engine = Engine::Builtin;
    server.engine_note = None;
    server.host = Some(fields.host.clone());
    server.port = Some(fields.port);
    server.username = Some(fields.username.clone());
    server.identity_file = fields
        .identity_files
        .iter()
        .find(|p| p.as_str() != "none")
        .cloned();
    server.identity_files = fields.identity_files.clone();
    server.identities_only = fields.identities_only;
    server.host_key_alias = fields.host_key_alias.clone();
    server.auth_method = AuthMethod::Agent;
    server.jump_server_id = jump;
    server.updated_at = now_ms;
}
fn apply(
    db: &mut Db,
    server: &Server,
    records: &[Server],
    preview: &ConversionPreview,
    now_ms: i64,
) -> Result<ConversionResult, AppError> {
    let mut additions = Vec::new();
    let mut updates = Vec::new();
    let mut jumps = Vec::<Server>::new();
    for jump in &preview.jumps {
        let mut record = if let Some(id) = &jump.existing_server_id {
            records
                .iter()
                .find(|s| &s.id == id)
                .cloned()
                .ok_or_else(|| AppError::new("ssh.conversion_changed"))?
        } else {
            crate::server_input::build_alias_server(
                &jump.fields.alias,
                None,
                &uuid::Uuid::new_v4().to_string(),
                now_ms,
            )?
        };
        if record.source == ServerSource::SshConfig {
            copy_fields(
                &mut record,
                &jump.fields,
                jumps.last().map(|s| s.id.clone()),
                now_ms,
            );
            if jump.existing_server_id.is_some() {
                updates.push(record.clone());
            } else {
                additions.push(record.clone());
            }
        }
        jumps.push(record);
    }
    let mut target = server.clone();
    copy_fields(
        &mut target,
        &preview.target,
        jumps.last().map(|s| s.id.clone()),
        now_ms,
    );
    updates.push(target.clone());
    db.apply_conversion(records, &updates, &additions)?;
    Ok(ConversionResult {
        server: target,
        jumps,
    })
}

#[cfg(test)]
mod tests;
