//! 代理设置：存在数据库设置表的 `proxy` 键里（JSON），启动时和保存后交给 core，之后新建的连接按它走。
//! 不支持带用户名密码的代理。还负责系统 OpenSSH 的 ProxyCommand 入口（`Pierlink.exe --proxy-connect 主机 端口`）
//! 和本机终端的代理环境变量。
use crate::data::DbState;
use pierlink_core::detect::shells::{LocalShell, ShellKind};
use pierlink_core::error::AppError;
use pierlink_core::proxy::{self, ProxyKind, ProxyMode, ProxyServer, ProxySettings, SystemProxy};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::net::IpAddr;
use std::path::Path;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

const SETTINGS_KEY: &str = "proxy";
/// 不走代理的名单最多多少条
const MAX_BYPASS: usize = 200;
/// 名单里一条最长多少字符
const MAX_RULE_LEN: usize = 255;
/// 测试代理时经代理连接的目标（检查更新也连它）
const TEST_HOST: &str = "github.com";
const TEST_PORT: u16 = 443;
/// 测试代理最多等多久
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 代理方式
#[derive(Serialize, Deserialize, Type, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProxyModeDto {
    /// 不使用代理（默认）
    #[default]
    Off,
    /// 跟随 Windows 的代理设置（每次连接时现读）
    System,
    /// 自定义
    Custom,
}

/// 代理协议
#[derive(Serialize, Deserialize, Type, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProxyKindDto {
    #[default]
    Http,
    Socks5,
}

impl From<ProxyKindDto> for ProxyKind {
    fn from(value: ProxyKindDto) -> Self {
        match value {
            ProxyKindDto::Http => ProxyKind::Http,
            ProxyKindDto::Socks5 => ProxyKind::Socks5,
        }
    }
}

impl From<ProxyKind> for ProxyKindDto {
    fn from(value: ProxyKind) -> Self {
        match value {
            ProxyKind::Http => ProxyKindDto::Http,
            ProxyKind::Socks5 => ProxyKindDto::Socks5,
        }
    }
}

/// 代理设置
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct ProxySettingsDto {
    pub mode: ProxyModeDto,
    /// 自定义代理的协议、地址、端口（选别的方式时也保留，切回自定义不用重填）
    pub kind: ProxyKindDto,
    pub host: String,
    pub port: Option<u16>,
    /// 自定义代理时不走代理的地址（跟随系统时用 Windows 设置里的例外）
    pub bypass: Vec<String>,
}

impl Default for ProxySettingsDto {
    fn default() -> Self {
        Self {
            mode: ProxyModeDto::Off,
            kind: ProxyKindDto::Http,
            host: String::new(),
            port: None,
            bypass: proxy::DEFAULT_BYPASS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }
}

/// Windows 当前的代理设置（界面显示用）
#[derive(Serialize, Type, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SystemProxyDto {
    /// 没有设置代理
    None,
    /// 手动设置的代理
    Server {
        protocol: ProxyKindDto,
        host: String,
        port: u16,
    },
    /// 用的是 PAC 脚本（不支持，按不使用代理处理）
    Pac { url: String },
}

/// 读保存的设置；没存过或读不出来时返回默认值（不使用代理）
fn load(app: &AppHandle) -> ProxySettingsDto {
    app.state::<DbState>()
        .with(|db| db.get_setting::<ProxySettingsDto>(SETTINGS_KEY))
        .unwrap_or_else(|e| {
            eprintln!("读取代理设置失败，按不使用代理处理: 错误={}", e.code);
            None
        })
        .unwrap_or_default()
}

/// 自定义的代理地址。
///
/// :param settings: 已整理过的设置
/// :return: 代理地址；地址或端口不对时返回 proxy.invalid_host / proxy.invalid_port
fn custom_server(settings: &ProxySettingsDto) -> Result<ProxyServer, AppError> {
    let port = settings
        .port
        .filter(|p| *p != 0)
        .ok_or_else(|| AppError::new("proxy.invalid_port"))?;
    let server = ProxyServer {
        kind: settings.kind.into(),
        host: settings.host.clone(),
        port,
    };
    pierlink_ssh_engine::proxy::validate(&server, TEST_HOST, TEST_PORT)
        .map_err(|_| AppError::new("proxy.invalid_host").with("host", &settings.host))?;
    Ok(server)
}

/// 整理界面提交的设置：地址去掉空白和 IPv6 的方括号；名单按逗号、分号、空白拆开，去掉空的和重复的。
///
/// :param settings: 界面提交的设置
/// :return: 整理后的设置；选了自定义但地址或端口不对、名单太长时返回错误
fn normalize(mut settings: ProxySettingsDto) -> Result<ProxySettingsDto, AppError> {
    let host = settings.host.trim();
    settings.host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .to_owned();
    let mut bypass: Vec<String> = Vec::new();
    for rule in settings
        .bypass
        .iter()
        .flat_map(|r| r.split([',', ';', ' ', '\t', '\r', '\n']))
    {
        if rule.is_empty() || bypass.iter().any(|b| b.eq_ignore_ascii_case(rule)) {
            continue;
        }
        if rule.chars().count() > MAX_RULE_LEN || rule.chars().any(char::is_control) {
            return Err(AppError::new("proxy.invalid_bypass")
                .with("rule", rule.chars().take(60).collect::<String>()));
        }
        bypass.push(rule.to_owned());
    }
    if bypass.len() > MAX_BYPASS {
        return Err(AppError::new("proxy.bypass_too_many").with("max", MAX_BYPASS.to_string()));
    }
    settings.bypass = bypass;
    if settings.mode == ProxyModeDto::Custom {
        custom_server(&settings)?;
    }
    Ok(settings)
}

/// 换成 core 用的设置
fn to_core(settings: &ProxySettingsDto) -> ProxySettings {
    let mode = match settings.mode {
        ProxyModeDto::Off => ProxyMode::Off,
        ProxyModeDto::System => ProxyMode::System,
        ProxyModeDto::Custom => match custom_server(settings) {
            Ok(server) => ProxyMode::Custom(server),
            // 保存时检查过，正常到不了这里
            Err(e) => {
                eprintln!("自定义代理地址无效，按不使用代理处理: 错误={}", e.code);
                ProxyMode::Off
            }
        },
    };
    ProxySettings {
        mode,
        bypass: settings.bypass.clone(),
    }
}

/// 设置的简短描述（日志用）
fn describe(settings: &ProxySettingsDto) -> String {
    match settings.mode {
        ProxyModeDto::Off => "不使用".into(),
        ProxyModeDto::System => "跟随系统".into(),
        ProxyModeDto::Custom => format!(
            "自定义 {:?} {}",
            settings.kind,
            pierlink_ssh_engine::proxy::authority(&settings.host, settings.port.unwrap_or(0))
        ),
    }
}

/// 启动时调用：把保存的代理设置交给 core。要在任何连接之前。
pub fn apply_saved(app: &AppHandle) {
    let settings = load(app);
    eprintln!(
        "代理设置: 方式={}, 名单条数={}",
        describe(&settings),
        settings.bypass.len()
    );
    proxy::set_settings(to_core(&settings));
}

/// 读代理设置
#[tauri::command]
#[specta::specta]
pub async fn proxy_get(app: AppHandle) -> ProxySettingsDto {
    load(&app)
}

/// 保存代理设置并马上生效（已经连上的不受影响，之后新建的连接按新设置走）
///
/// :param settings: 界面提交的设置
/// :return: 保存后的设置（整理过的地址和名单）
#[tauri::command]
#[specta::specta]
pub async fn proxy_set(
    app: AppHandle,
    settings: ProxySettingsDto,
) -> Result<ProxySettingsDto, AppError> {
    let settings = normalize(settings)?;
    app.state::<DbState>()
        .with(|db| db.set_setting(SETTINGS_KEY, &settings))?;
    proxy::set_settings(to_core(&settings));
    eprintln!(
        "保存代理设置: 方式={}, 名单条数={}",
        describe(&settings),
        settings.bypass.len()
    );
    Ok(settings)
}

/// Windows 当前的代理设置（每次现读）
#[tauri::command]
#[specta::specta]
pub async fn proxy_system() -> SystemProxyDto {
    match proxy::system_proxy() {
        SystemProxy::None => SystemProxyDto::None,
        SystemProxy::Server { server, .. } => SystemProxyDto::Server {
            protocol: server.kind.into(),
            host: server.host,
            port: server.port,
        },
        SystemProxy::Pac { url } => SystemProxyDto::Pac { url },
    }
}

/// 测试代理：经代理连一下 github.com:443（不保存设置）
///
/// :param settings: 界面上当前填的设置
/// :return: 用了多少毫秒；连不上时返回 proxy.* 错误（如 proxy.unreachable、proxy.refused、proxy.timeout）
#[tauri::command]
#[specta::specta]
pub async fn proxy_test(settings: ProxySettingsDto) -> Result<u32, AppError> {
    let settings = normalize(settings)?;
    let server = match settings.mode {
        ProxyModeDto::Off => return Err(AppError::new("proxy.not_enabled")),
        ProxyModeDto::Custom => custom_server(&settings)?,
        ProxyModeDto::System => match proxy::system_proxy() {
            SystemProxy::Server { server, .. } => server,
            SystemProxy::Pac { .. } => return Err(AppError::new("proxy.pac_unsupported")),
            SystemProxy::None => return Err(AppError::new("proxy.system_none")),
        },
    };
    tauri::async_runtime::spawn_blocking(move || {
        let start = Instant::now();
        let stream = proxy::connect(&server, TEST_HOST, TEST_PORT, TEST_TIMEOUT)?;
        drop(stream);
        let ms = u32::try_from(start.elapsed().as_millis()).unwrap_or(u32::MAX);
        eprintln!(
            "测试代理成功: 代理={}, 目标={TEST_HOST}:{TEST_PORT}, 毫秒={ms}",
            pierlink_ssh_engine::proxy::authority(&server.host, server.port)
        );
        Ok(ms)
    })
    .await
    .map_err(|e| AppError::new("proxy.io_failed").with("reason", e.to_string()))?
}

/// `Pierlink.exe --proxy-connect 主机 端口`：系统 OpenSSH 的 ProxyCommand。经环境变量 PIERLINK_PROXY 给的代理
/// 连到服务器，在标准输入输出和这条连接之间转发；出错时原因写到标准错误（ssh 会显示在终端里）。
///
/// :param args: --proxy-connect 后面的参数
/// :return: 进程退出码：0 正常结束，1 连接或转发失败，2 参数不对
pub fn run_proxy_connect(args: &[std::ffi::OsString]) -> i32 {
    let [host, port] = args else {
        eprintln!("参数不对，用法: --proxy-connect 主机 端口");
        return 2;
    };
    let (Some(host), Some(port)) = (
        host.to_str(),
        port.to_str().and_then(|p| p.parse::<u16>().ok()),
    ) else {
        eprintln!("参数不对: 主机或端口无效");
        return 2;
    };
    match proxy::pipe_stdio_from_env(host, port) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// 本机终端的代理环境变量（设置了代理就加上，不另设开关）。
///
/// WSL 只能经 WSLENV 拿到 Windows 这边的环境变量：镜像网络模式下直接传；NAT 模式下 WSL 里的 127.0.0.1
/// 不是这台 Windows，代理在本机地址时不传（传了 WSL 里反而上不了网），在局域网地址时照传。
///
/// :param shell: 要打开的 shell
/// :return: 要加到终端里的环境变量；没开代理时为空
pub fn terminal_env(shell: &LocalShell) -> Vec<(String, String)> {
    let mut env = proxy::terminal_env();
    if env.is_empty() || !matches!(shell.kind, ShellKind::Wsl { .. }) {
        return env;
    }
    let loopback = active_server().is_some_and(|s| is_loopback(&s.host));
    if loopback && !wsl_mirrored() {
        eprintln!("WSL 是 NAT 网络、代理在本机地址，WSL 终端不加代理环境变量");
        return Vec::new();
    }
    let names: Vec<String> = env.iter().map(|(key, _)| format!("{key}/u")).collect();
    env.push((
        "WSLENV".into(),
        merge_wslenv(&std::env::var("WSLENV").unwrap_or_default(), &names),
    ));
    env
}

/// 现在生效的代理（跟随系统时现读系统设置）
fn active_server() -> Option<ProxyServer> {
    match proxy::settings().mode {
        ProxyMode::Off => None,
        ProxyMode::Custom(server) => Some(server),
        ProxyMode::System => match proxy::system_proxy() {
            SystemProxy::Server { server, .. } => Some(server),
            SystemProxy::None | SystemProxy::Pac { .. } => None,
        },
    }
}

/// 是不是本机地址（localhost、127.x、::1）
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// 把要传给 WSL 的变量名加到原有的 WSLENV 后面（去重，保留原有的）
fn merge_wslenv(existing: &str, names: &[String]) -> String {
    let mut parts: Vec<String> = existing
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    for name in names {
        if !parts.iter().any(|p| p == name) {
            parts.push(name.clone());
        }
    }
    parts.join(":")
}

/// WSL 是不是镜像网络模式（%USERPROFILE%\.wslconfig 的 [wsl2] 里 networkingMode=mirrored）
fn wsl_mirrored() -> bool {
    std::env::var_os("USERPROFILE")
        .and_then(|home| std::fs::read_to_string(Path::new(&home).join(".wslconfig")).ok())
        .is_some_and(|text| mirrored_in(&text))
}

/// 解析 .wslconfig 的内容：[wsl2] 段里最后一个 networkingMode 是不是 mirrored
fn mirrored_in(text: &str) -> bool {
    let mut section_wsl2 = false;
    let mut mirrored = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section_wsl2 = line.eq_ignore_ascii_case("[wsl2]");
        } else if section_wsl2
            && let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("networkingMode")
        {
            let value = value
                .split(['#', ';'])
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"');
            mirrored = value.eq_ignore_ascii_case("mirrored");
        }
    }
    mirrored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(host: &str, port: Option<u16>) -> ProxySettingsDto {
        ProxySettingsDto {
            mode: ProxyModeDto::Custom,
            host: host.into(),
            port,
            ..Default::default()
        }
    }

    #[test]
    fn 整理地址和名单() {
        let mut settings = custom("  [::1] ", Some(7890));
        settings.bypass = vec![
            " a.example, b.example;A.EXAMPLE ".into(),
            "10.0.0.0/8\n*.local".into(),
            "".into(),
        ];
        let saved = normalize(settings).unwrap();
        assert_eq!(saved.host, "::1");
        assert_eq!(
            saved.bypass,
            ["a.example", "b.example", "10.0.0.0/8", "*.local"]
        );
    }

    #[test]
    fn 自定义代理要有效地址和端口() {
        assert_eq!(
            normalize(custom("", Some(7890))).unwrap_err().code,
            "proxy.invalid_host"
        );
        assert_eq!(
            normalize(custom("http://127.0.0.1", Some(7890)))
                .unwrap_err()
                .code,
            "proxy.invalid_host"
        );
        assert_eq!(
            normalize(custom("user@host", Some(7890))).unwrap_err().code,
            "proxy.invalid_host"
        );
        assert_eq!(
            normalize(custom("127.0.0.1", None)).unwrap_err().code,
            "proxy.invalid_port"
        );
        assert_eq!(
            normalize(custom("127.0.0.1", Some(0))).unwrap_err().code,
            "proxy.invalid_port"
        );
        assert!(normalize(custom("proxy.lan", Some(1080))).is_ok());
        // 不选自定义时，没填完的地址也能保存
        let mut off = custom("", None);
        off.mode = ProxyModeDto::Off;
        assert!(normalize(off).is_ok());
    }

    #[test]
    fn 名单太长或有控制字符时报错() {
        let mut settings = custom("127.0.0.1", Some(7890));
        settings.bypass = (0..=MAX_BYPASS).map(|i| format!("h{i}.example")).collect();
        assert_eq!(
            normalize(settings.clone()).unwrap_err().code,
            "proxy.bypass_too_many"
        );
        settings.bypass = vec!["a\u{7}b".into()];
        assert_eq!(
            normalize(settings.clone()).unwrap_err().code,
            "proxy.invalid_bypass"
        );
        settings.bypass = vec!["x".repeat(MAX_RULE_LEN + 1)];
        assert_eq!(
            normalize(settings).unwrap_err().code,
            "proxy.invalid_bypass"
        );
    }

    #[test]
    fn 换成core的设置() {
        let mut settings = custom("127.0.0.1", Some(1080));
        settings.kind = ProxyKindDto::Socks5;
        let core = to_core(&settings);
        let ProxyMode::Custom(server) = core.mode else {
            panic!("应为自定义")
        };
        assert_eq!(
            (server.kind, server.host.as_str(), server.port),
            (ProxyKind::Socks5, "127.0.0.1", 1080)
        );
        settings.mode = ProxyModeDto::System;
        assert!(matches!(to_core(&settings).mode, ProxyMode::System));
        // 存进去的地址坏了：按不使用代理
        let broken = custom("bad host", Some(1));
        assert!(matches!(to_core(&broken).mode, ProxyMode::Off));
    }

    #[test]
    fn 旧设置缺字段时用默认值() {
        let parsed: ProxySettingsDto = serde_json::from_str(r#"{"mode":"system"}"#).unwrap();
        assert_eq!(parsed.mode, ProxyModeDto::System);
        assert_eq!(parsed.bypass.len(), proxy::DEFAULT_BYPASS.len());
    }

    #[test]
    fn wslenv合并去重() {
        let names = vec!["http_proxy/u".to_owned(), "NO_PROXY/u".to_owned()];
        assert_eq!(merge_wslenv("", &names), "http_proxy/u:NO_PROXY/u");
        assert_eq!(
            merge_wslenv("USERPROFILE/p:http_proxy/u:", &names),
            "USERPROFILE/p:http_proxy/u:NO_PROXY/u"
        );
    }

    #[test]
    fn 识别wsl镜像网络() {
        assert!(mirrored_in("[wsl2]\nnetworkingMode=mirrored\n"));
        assert!(mirrored_in(
            "[WSL2]\r\n  NetworkingMode = \"Mirrored\" # 注释\r\n"
        ));
        assert!(!mirrored_in("[wsl2]\nnetworkingMode=NAT\n"));
        // 别的段里的不算；后写的覆盖前面的
        assert!(!mirrored_in("[experimental]\nnetworkingMode=mirrored\n"));
        assert!(!mirrored_in(
            "[wsl2]\nnetworkingMode=mirrored\nnetworkingMode=nat\n"
        ));
        assert!(!mirrored_in(""));
    }

    #[test]
    fn 本机地址() {
        for host in ["localhost", "LOCALHOST", "127.0.0.1", "127.1.2.3", "::1"] {
            assert!(is_loopback(host), "{host}");
        }
        for host in ["192.168.1.2", "proxy.lan", "10.0.0.1"] {
            assert!(!is_loopback(host), "{host}");
        }
    }

    #[test]
    fn 代理入口参数不对时退出码为2() {
        assert_eq!(run_proxy_connect(&[]), 2);
        assert_eq!(run_proxy_connect(&["host".into()]), 2);
        assert_eq!(run_proxy_connect(&["host".into(), "notaport".into()]), 2);
        assert_eq!(run_proxy_connect(&["host".into(), "70000".into()]), 2);
    }
}
