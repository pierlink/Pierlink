//! 一键接入：把 Pierlink 的 MCP 加进本机的 Claude Code / Codex / Cursor 配置，
//! 或在服务器上把服务器入口加进那里的 Claude Code。
//! 本机用各客户端自己的命令行（Cursor 没有命令行，合并它的 mcp.json）；
//! 服务器上用后台连接执行脚本，令牌由 Pierlink 直接给，不依赖服务器上的地址文件。
use pierlink_core::error::AppError;
use serde::Serialize;
use specta::Type;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// 客户端命令最多等多久（第一次运行可能要初始化）
const CLIENT_TIMEOUT: Duration = Duration::from_secs(60);
/// 在 Claude Code、Codex、Cursor 里的名字
const NAME: &str = "pierlink";
/// 在服务器上加过 Claude Code 的服务器入口（换令牌后自动重加）
const INSTALLED_FILE: &str = "mcp-installed.json";

/// 一键接入的结果
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpInstallDto {
    /// claude / codex / cursor
    pub client: String,
    /// 改了哪个配置文件（Cursor）或用了哪个程序（其他），给用户看
    pub detail: String,
}

/// 客户端的显示名（错误提示里用）
fn display(client: &str) -> &str {
    match client {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "gemini" => "Gemini CLI",
        "cursor" => "Cursor",
        other => other,
    }
}

fn failed(client: &str, reason: impl Into<String>) -> AppError {
    AppError::new("mcp.install_failed")
        .with("client", display(client))
        .with("reason", reason.into())
}

/// 固定原因的安装失败：界面按 errors.detail.<key> 显示当前语言，text 是中文原文。
fn failed_text(client: &str, key: &'static str, text: &str) -> AppError {
    AppError::new("mcp.install_failed")
        .with("client", display(client))
        .with_reason(key, text)
}

/// 在目录列表里找第一个存在的程序
fn find_in(dirs: &[PathBuf], names: &[&str]) -> Option<PathBuf> {
    dirs.iter()
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

/// 找本机的客户端程序：先 PATH，再几个常见的安装位置（应用从开始菜单启动时 PATH 可能是旧的）
fn find_client(client: &str) -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        match client {
            "claude" => &["claude.exe", "claude.cmd"],
            _ => &["codex.exe", "codex.cmd"],
        }
    } else {
        match client {
            "claude" => &["claude"],
            _ => &["codex"],
        }
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = home_dir() {
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(".claude").join("local"));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(appdata).join("npm"));
    }
    find_in(&dirs, names)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// 运行客户端命令，等它结束；返回 (是否成功, 输出末尾)
fn run_client(program: &Path, args: &[&str]) -> Result<(bool, String), AppError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW：不闪控制台窗口
    }
    let child = command
        .spawn()
        .map_err(|e| AppError::new("mcp.install_failed").with("reason", e.to_string()))?;
    let id = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = match rx.recv_timeout(CLIENT_TIMEOUT) {
        Ok(result) => {
            result.map_err(|e| AppError::new("mcp.install_failed").with("reason", e.to_string()))?
        }
        Err(_) => {
            kill(id);
            return Err(AppError::new("mcp.install_timeout"));
        }
    };
    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    if text.trim().is_empty() {
        text = String::from_utf8_lossy(&output.stdout).into_owned();
    }
    Ok((output.status.success(), tail(&text, 400)))
}

/// 结束超时的客户端进程
fn kill(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x08000000)
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
}

/// 最后 max 个字符（去掉首尾空白）
fn tail(text: &str, max: usize) -> String {
    let t = text.trim();
    let n = t.chars().count();
    t.chars().skip(n.saturating_sub(max)).collect()
}

/// 用客户端自己的命令行加：先删掉同名的（可能是旧路径），再加用户级的
fn install_cli(client: &str, exe: &str) -> Result<McpInstallDto, AppError> {
    let program = find_client(client)
        .ok_or_else(|| AppError::new("mcp.client_not_found").with("client", display(client)))?;
    let (remove, add): (Vec<&str>, Vec<&str>) = match client {
        "claude" => (
            vec!["mcp", "remove", NAME, "--scope", "user"],
            vec!["mcp", "add", "--scope", "user", NAME, "--", exe, "--mcp"],
        ),
        _ => (
            vec!["mcp", "remove", NAME],
            vec!["mcp", "add", NAME, "--", exe, "--mcp"],
        ),
    };
    // 没加过时删除会失败，属于正常情况
    let _ = run_client(&program, &remove);
    let (ok, out) = run_client(&program, &add)?;
    if !ok {
        return Err(failed(client, out));
    }
    eprintln!(
        "已把 Pierlink 加进 {client}: 程序={}",
        program.to_string_lossy()
    );
    Ok(McpInstallDto {
        client: client.into(),
        detail: program.to_string_lossy().into_owned(),
    })
}

/// 把 pierlink 合并进 Cursor 的 mcp.json；文件读不懂时不改，返回错误
///
/// :param path: mcp.json 路径
/// :param exe: Pierlink 程序路径
/// :return: 合并后的文本
fn merge_cursor(existing: Option<&str>, exe: &str) -> Result<String, AppError> {
    let mut root: serde_json::Value = match existing.map(str::trim) {
        None | Some("") => serde_json::json!({}),
        Some(text) => {
            serde_json::from_str(text).map_err(|_| AppError::new("mcp.cursor_config_invalid"))?
        }
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| AppError::new("mcp.cursor_config_invalid"))?;
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| AppError::new("mcp.cursor_config_invalid"))?;
    servers.insert(
        NAME.into(),
        serde_json::json!({ "command": exe, "args": ["--mcp"] }),
    );
    serde_json::to_string_pretty(&root).map_err(|e| failed("cursor", e.to_string()))
}

fn install_cursor(exe: &str) -> Result<McpInstallDto, AppError> {
    let home = home_dir()
        .ok_or_else(|| failed_text("cursor", "mcp.home_folder_not_found", "找不到用户目录"))?;
    let dir = home.join(".cursor");
    let path = dir.join("mcp.json");
    let existing = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(failed("cursor", e.to_string())),
    };
    let text = merge_cursor(existing.as_deref(), exe)
        .map_err(|e| e.with("path", path.to_string_lossy()))?;
    std::fs::create_dir_all(&dir).map_err(|e| failed("cursor", e.to_string()))?;
    // 先写临时文件再替换，写到一半出错不会弄坏原来的配置
    let part = dir.join("mcp.json.pierlink-part");
    std::fs::write(&part, text).map_err(|e| failed("cursor", e.to_string()))?;
    std::fs::rename(&part, &path).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        failed("cursor", e.to_string())
    })?;
    eprintln!("已把 Pierlink 加进 Cursor: 文件={}", path.to_string_lossy());
    Ok(McpInstallDto {
        client: "cursor".into(),
        detail: path.to_string_lossy().into_owned(),
    })
}

/// 把本机入口加进 Claude Code、Codex 或 Cursor
///
/// :param client: claude / codex / cursor
/// :return: 结果；没装这个客户端返回 mcp.client_not_found
#[tauri::command]
#[specta::specta]
pub async fn mcp_install_local(client: String) -> Result<McpInstallDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let exe = std::env::current_exe()
            .map_err(|e| failed(&client, e.to_string()))?
            .to_string_lossy()
            .into_owned();
        match client.as_str() {
            "claude" | "codex" => install_cli(&client, &exe),
            "cursor" => install_cursor(&exe),
            _ => Err(AppError::new("mcp.client_not_found").with("client", &client)),
        }
    })
    .await
    .map_err(|e| AppError::new("mcp.task_failed").with("reason", e.to_string()))?
}

/// 在服务器上找客户端程序的脚本片段：PATH、常见安装位置、登录 shell 和交互 shell 的 PATH（nvm 等）
/// 找到时 c 是绝对路径，找不到时以 3 退出
fn find_on_server(program: &str) -> String {
    format!(
        r#"c=$(command -v {program} 2>/dev/null || true)
for p in "$HOME/.local/bin/{program}" "$HOME/.claude/local/{program}" "$HOME/.npm-global/bin/{program}" /usr/local/bin/{program}; do
  [ -n "$c" ] && break
  [ -x "$p" ] && c=$p
done
if [ -z "$c" ]; then
  c=$("${{SHELL:-/bin/sh}}" -lc 'command -v {program}' 2>/dev/null | tail -n 1)
fi
if [ -z "$c" ]; then
  c=$("${{SHELL:-/bin/sh}}" -ic 'command -v {program}' 2>/dev/null </dev/null | tail -n 1)
fi
case "$c" in /*) ;; *) exit 3;; esac
"#
    )
}

/// 在服务器上把服务器入口加进 Claude Code 的脚本：找 claude（含 nvm、npm 全局、官方安装位置），
/// 先删同名的再加用户级的，成功时输出 {"ok":true}。找不到 claude 以 3 退出，加失败以 4 退出。
/// claude 的输出可能带令牌，一律丢掉。
pub(crate) fn server_install_script(url: &str, token: &str) -> Result<String, AppError> {
    let url = pierlink_core::ssh::quote::sh_quote(url)?;
    let header = pierlink_core::ssh::quote::sh_quote(&format!("Authorization: Bearer {token}"))?;
    Ok(format!(
        r#"{find}"$c" mcp remove {name} --scope user >/dev/null 2>&1 || true
"$c" mcp add --transport http --scope user {name} {url} --header {header} >/dev/null 2>&1 || exit 4
printf '%s\n' '{{"ok":true}}'
"#,
        find = find_on_server("claude"),
        name = NAME
    ))
}

/// 在服务器上把服务器入口加进 Gemini CLI（用户级配置 ~/.gemini/settings.json）：先删同名的再加。
/// 在家目录里运行：不认 --scope 的旧版本，加到“当前目录”的配置也就是用户级配置。
/// gemini 的输出可能带令牌，一律丢掉。找不到 gemini 以 3 退出，加失败以 4 退出。
pub(crate) fn server_gemini_script(url: &str, token: &str) -> Result<String, AppError> {
    let url = pierlink_core::ssh::quote::sh_quote(url)?;
    let header = pierlink_core::ssh::quote::sh_quote(&format!("Authorization: Bearer {token}"))?;
    Ok(format!(
        r#"{find}cd "$HOME" || exit 4
"$c" mcp remove --scope user {name} >/dev/null 2>&1 || "$c" mcp remove {name} >/dev/null 2>&1 || true
"$c" mcp add --scope user --transport http --header {header} {name} {url} >/dev/null 2>&1 \
  || "$c" mcp add --transport http --header {header} {name} {url} >/dev/null 2>&1 || exit 4
# 配置里有令牌：只给自己读（Gemini CLI 按默认 umask 写文件，可能是 644）
chmod 700 "$HOME/.gemini" 2>/dev/null
chmod 600 "$HOME/.gemini/settings.json" 2>/dev/null
printf '%s\n' '{{"ok":true}}'
"#,
        find = find_on_server("gemini"),
        name = NAME
    ))
}

/// 在服务器上把服务器入口写进 Codex 的 ~/.codex/config.toml：
/// 去掉原来的 [mcp_servers.pierlink] 段再追加新的（url + Authorization 头），其他内容不动。
/// 文件含令牌，权限 600；先写临时文件再替换。找不到 codex 以 3 退出，写失败以 4 退出。
pub(crate) fn server_codex_script(url: &str, token: &str) -> Result<String, AppError> {
    // 令牌只含字母数字和 -_，地址是 http://127.0.0.1:端口/mcp：放进 TOML 字符串不需要转义，这里再确认一次
    let safe = |v: &str| {
        v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:/".contains(c))
    };
    if !safe(url) || !safe(token) {
        return Err(failed_text(
            "codex",
            "mcp.address_or_token_has_unsupported_characters",
            "地址或令牌含有不支持的字符",
        ));
    }
    let url = pierlink_core::ssh::quote::sh_quote(url)?;
    let token = pierlink_core::ssh::quote::sh_quote(token)?;
    Ok(format!(
        r#"{find}url={url}
token={token}
dir="$HOME/.codex"
f="$dir/config.toml"
umask 077
mkdir -p "$dir" || exit 4
tmp=$(mktemp "$dir/.config.toml.XXXXXX") || exit 4
trap 'rm -f "$tmp"' EXIT
if [ -f "$f" ]; then
  awk '/^[[:space:]]*\[/ {{ skip = ($0 ~ /^[[:space:]]*\[mcp_servers\.{name}(\]|\.)/) }} !skip' "$f" > "$tmp" || exit 4
fi
printf '\n[mcp_servers.{name}]\nurl = "%s"\nhttp_headers = {{ "Authorization" = "Bearer %s" }}\n' "$url" "$token" >> "$tmp" || exit 4
# 配置文件是符号链接（dotfiles 管理的）时写到链接指向的文件里，不把链接换成普通文件
if [ -L "$f" ]; then
  cat "$tmp" > "$f" || exit 4
  # 文件里有令牌：链接指向的文件也只给自己读
  chmod 600 "$f" || exit 4
  rm -f "$tmp"
else
  mv -f "$tmp" "$f" || exit 4
fi
trap - EXIT
printf '%s\n' '{{"ok":true}}'
"#,
        find = find_on_server("codex"),
        name = NAME
    ))
}

fn installed_path(app: &AppHandle) -> Result<PathBuf, AppError> {
    Ok(crate::data::data_dir(app)?.join(INSTALLED_FILE))
}

/// 记录里的一项：Claude Code 只写服务器 ID（旧记录也是这样），其他客户端写“服务器 ID:客户端”
fn installed_key(server_id: &str, client: &str) -> String {
    if client == "claude" {
        server_id.to_owned()
    } else {
        format!("{server_id}:{client}")
    }
}

/// 在服务器上一键添加过的（换令牌后要重加）
fn installed(app: &AppHandle) -> Vec<String> {
    installed_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn set_installed(app: &AppHandle, key: &str) {
    let mut list = installed(app);
    if list.iter().any(|x| x == key) {
        return;
    }
    list.push(key.to_owned());
    let result = installed_path(app).and_then(|p| {
        std::fs::write(p, serde_json::to_string(&list).unwrap_or_default())
            .map_err(|e| AppError::new("mcp.install_failed").with("reason", e.to_string()))
    });
    if let Err(e) = result {
        eprintln!("记录服务器一键接入失败: 项={key}, 错误={}", e.code);
    }
}

/// 在服务器上执行接入脚本
///
/// :param client: claude / codex
fn install_on_server(
    app: &AppHandle,
    server_id: &str,
    client: &str,
) -> Result<McpInstallDto, AppError> {
    let settings = super::settings::load(app);
    let port = settings
        .servers
        .iter()
        .find(|s| s.server_id == server_id)
        .map(|s| s.port)
        .ok_or_else(|| {
            AppError::new("mcp.entry_disabled")
                .with_reason("mcp.server_has_no_entry", "这台服务器没有入口")
        })?;
    let token = super::entries::token(server_id)?;
    let url = super::entries::url(port);
    let script = match client {
        "claude" => server_install_script(&url, &token)?,
        "codex" => server_codex_script(&url, &token)?,
        "gemini" => server_gemini_script(&url, &token)?,
        _ => return Err(AppError::new("mcp.client_not_found").with("client", client)),
    };
    let (program, cmd, name) = crate::deploy::background_command(
        app,
        server_id,
        crate::keys::Access::Background("AI 接入"),
    )?;
    let out = pierlink_core::remote::run::run_script(
        &program,
        &cmd,
        &script,
        pierlink_core::remote::run::RunLimits {
            timeout: CLIENT_TIMEOUT,
            stdout_bytes: 16 * 1024,
            stderr_bytes: 4096,
        },
    );
    match out {
        Ok(_) => {
            set_installed(app, &installed_key(server_id, client));
            eprintln!(
                "已在服务器上把 Pierlink 加进 {}: 服务器={name}",
                display(client)
            );
            Ok(McpInstallDto {
                client: client.into(),
                detail: name,
            })
        }
        Err(e) if e.code == "remote.script_failed" => {
            match e.params.get("exit_code").map(String::as_str) {
                Some("3") => Err(AppError::new("mcp.server_client_not_found")
                    .with("server", &name)
                    .with("client", display(client))),
                Some("4") => Err(failed_text(
                    client,
                    "mcp.couldn_t_write_the_config_on_the_server",
                    "在服务器上写配置没有成功",
                )),
                // 脚本正文里有令牌：错误尾部不给界面，免得万一带出来
                _ => {
                    let mut e = e;
                    e.params.remove("stderr_tail");
                    Err(e)
                }
            }
        }
        Err(mut e) => {
            e.params.remove("stderr_tail");
            Err(e)
        }
    }
}

/// 在服务器上把这台服务器的入口加进 Claude Code 或 Codex（之后换令牌会自动重加）
///
/// :param server_id: 服务器 ID
/// :param client: claude / codex
/// :return: 结果；服务器上没装这个客户端返回 mcp.server_client_not_found
#[tauri::command]
#[specta::specta]
pub async fn mcp_install_server(
    app: AppHandle,
    server_id: String,
    client: String,
) -> Result<McpInstallDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || install_on_server(&app, &server_id, &client))
        .await
        .map_err(|e| AppError::new("mcp.task_failed").with("reason", e.to_string()))?
}

/// 这台电脑上装了哪些客户端
#[derive(Serialize, Type, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct McpClientsDto {
    pub claude: bool,
    pub codex: bool,
    pub cursor: bool,
    /// 只在服务器上检测（本机固定为 false）
    pub gemini: bool,
}

/// Cursor 装没装：开始菜单装的位置、PATH 里的 cursor，或用过它留下的 ~/.cursor
fn cursor_installed() -> bool {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(|d| {
            PathBuf::from(d)
                .join("Programs")
                .join("cursor")
                .join("Cursor.exe")
        })
        .is_some_and(|p| p.is_file());
    let in_path = std::env::var_os("PATH").is_some_and(|p| {
        let dirs: Vec<PathBuf> = std::env::split_paths(&p).collect();
        find_in(&dirs, &["cursor.cmd", "cursor.exe", "cursor"]).is_some()
    });
    let used = home_dir().is_some_and(|h| h.join(".cursor").is_dir());
    local || in_path || used
}

/// 检测这台电脑上装了 Claude Code、Codex、Cursor 中的哪些
#[tauri::command]
#[specta::specta]
pub async fn mcp_detect_clients() -> Result<McpClientsDto, AppError> {
    tauri::async_runtime::spawn_blocking(|| McpClientsDto {
        claude: find_client("claude").is_some(),
        codex: find_client("codex").is_some(),
        cursor: cursor_installed(),
        gemini: false,
    })
    .await
    .map_err(|e| AppError::new("mcp.task_failed").with("reason", e.to_string()))
}

/// 查服务器上装了哪些客户端的脚本：和添加时找程序的办法一样（PATH、常见位置、登录和交互 shell）
const SERVER_DETECT_SCRIPT: &str = r#"has() {
  command -v "$1" >/dev/null 2>&1 && return 0
  for d in "$HOME/.local/bin" "$HOME/.claude/local" "$HOME/.npm-global/bin" /usr/local/bin; do
    [ -x "$d/$1" ] && return 0
  done
  "${SHELL:-/bin/sh}" -lc "command -v $1" >/dev/null 2>&1 && return 0
  "${SHELL:-/bin/sh}" -ic "command -v $1" </dev/null >/dev/null 2>&1 && return 0
  return 1
}
claude=false; has claude && claude=true
codex=false; has codex && codex=true
gemini=false; has gemini && gemini=true
printf '{"claude":%s,"codex":%s,"gemini":%s}\n' "$claude" "$codex" "$gemini"
"#;

/// 检测服务器上装了 Claude Code、Codex 中的哪些（Cursor 只在本机，固定为 false）
///
/// :param server_id: 服务器 ID
/// :return: 检测结果；连不上服务器时返回错误（界面就不限制按钮）
#[tauri::command]
#[specta::specta]
pub async fn mcp_detect_server_clients(
    app: AppHandle,
    server_id: String,
) -> Result<McpClientsDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (program, cmd, _) = crate::deploy::background_command(
            &app,
            &server_id,
            crate::keys::Access::Background("AI 接入"),
        )?;
        let v = pierlink_core::remote::run::run_script(
            &program,
            &cmd,
            SERVER_DETECT_SCRIPT,
            pierlink_core::remote::run::RunLimits {
                timeout: std::time::Duration::from_secs(30),
                stdout_bytes: 4096,
                stderr_bytes: 4096,
            },
        )?;
        Ok(McpClientsDto {
            claude: v["claude"].as_bool().unwrap_or(false),
            codex: v["codex"].as_bool().unwrap_or(false),
            cursor: false,
            gemini: v["gemini"].as_bool().unwrap_or(false),
        })
    })
    .await
    .map_err(|e| AppError::new("mcp.task_failed").with("reason", e.to_string()))?
}

/// 换令牌后：这台服务器上一键添加过的客户端，在后台用新令牌重加
pub(crate) fn reinstall_after_rotate(app: &AppHandle, server_id: &str) {
    let list = installed(app);
    let clients: Vec<&str> = ["claude", "codex"]
        .into_iter()
        .filter(|c| list.iter().any(|x| *x == installed_key(server_id, c)))
        .collect();
    if clients.is_empty() {
        return;
    }
    let (app, id) = (app.clone(), server_id.to_owned());
    let clients: Vec<String> = clients.into_iter().map(str::to_owned).collect();
    std::thread::spawn(move || {
        if app.try_state::<super::McpState>().is_none() {
            return;
        }
        for client in &clients {
            match install_on_server(&app, &id, client) {
                Ok(_) => eprintln!("换令牌后已在服务器上重加 {}: 服务器={id}", display(client)),
                Err(e) => eprintln!(
                    "换令牌后在服务器上重加 {} 失败: 服务器={id}, 错误={}",
                    display(client),
                    e.code
                ),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor配置合并时保留其他服务器() {
        let old = r#"{"mcpServers":{"other":{"command":"x"}},"theme":"dark"}"#;
        let text = merge_cursor(Some(old), r"C:\Pierlink\Pierlink.exe").unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["theme"], "dark");
        assert_eq!(
            v["mcpServers"]["pierlink"]["command"],
            r"C:\Pierlink\Pierlink.exe"
        );
        assert_eq!(v["mcpServers"]["pierlink"]["args"][0], "--mcp");
    }

    #[test]
    fn cursor配置为空或不存在时新建() {
        for old in [None, Some(""), Some("  \n")] {
            let v: serde_json::Value =
                serde_json::from_str(&merge_cursor(old, "p").unwrap()).unwrap();
            assert_eq!(v["mcpServers"]["pierlink"]["command"], "p");
        }
    }

    #[test]
    fn cursor配置读不懂时不改() {
        for old in ["{", "[]", r#"{"mcpServers":[]}"#] {
            assert_eq!(
                merge_cursor(Some(old), "p").unwrap_err().code,
                "mcp.cursor_config_invalid"
            );
        }
    }

    #[test]
    fn 服务器接入脚本引用了地址和令牌() {
        let s = server_install_script("http://127.0.0.1:47821/mcp", "abc_-123").unwrap();
        assert!(s.contains("'http://127.0.0.1:47821/mcp'"));
        assert!(s.contains("'Authorization: Bearer abc_-123'"));
        assert!(s.contains("--scope user pierlink"));
    }

    #[cfg(unix)]
    #[test]
    fn 检测服务器上的客户端() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("codex");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(SERVER_DETECT_SCRIPT)
            .env("HOME", dir.path())
            .env("PATH", "/nonexistent")
            .env("SHELL", "/bin/sh")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            r#"{"claude":false,"codex":true,"gemini":false}"#
        );
    }

    #[cfg(unix)]
    #[test]
    fn 服务器上找不到claude时报告() {
        let dir = tempfile::tempdir().unwrap();
        let script = server_install_script("http://127.0.0.1:1/mcp", "t").unwrap();
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .env("HOME", dir.path())
            .env("PATH", "/nonexistent")
            .env("SHELL", "/bin/sh")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(3));
    }

    #[cfg(unix)]
    #[test]
    fn 服务器上写codex配置时替换旧段并保留其他内容() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("codex");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let codex = dir.path().join(".codex");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            codex.join("config.toml"),
            "model = \"o4\"\n\n[mcp_servers.pierlink]\nurl = \"http://127.0.0.1:1/mcp\"\n\n[mcp_servers.pierlink.env]\nA = \"1\"\n\n[mcp_servers.other]\ncommand = \"x\"\n",
        )
        .unwrap();
        let script = server_codex_script("http://127.0.0.1:47821/mcp", "new-tok_1").unwrap();
        for _ in 0..2 {
            let out = Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .env("HOME", dir.path())
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let text = std::fs::read_to_string(codex.join("config.toml")).unwrap();
        assert!(text.contains("model = \"o4\""));
        assert!(text.contains("[mcp_servers.other]\ncommand = \"x\""));
        assert_eq!(text.matches("[mcp_servers.pierlink]").count(), 1);
        assert!(!text.contains("[mcp_servers.pierlink.env]"));
        assert!(text.contains("url = \"http://127.0.0.1:47821/mcp\""));
        assert!(text.contains("\"Authorization\" = \"Bearer new-tok_1\""));
        let mode = std::fs::metadata(codex.join("config.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[cfg(unix)]
    #[test]
    fn 服务器上找到claude时先删再加() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = dir.path().join("log");
        let fake = bin.join("claude");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = server_install_script("http://127.0.0.1:1/mcp", "tok").unwrap();
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .env("HOME", dir.path())
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            r#"{"ok":true}"#
        );
        let calls = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = calls.lines().collect();
        assert_eq!(lines[0], "mcp remove pierlink --scope user");
        assert_eq!(
            lines[1],
            "mcp add --transport http --scope user pierlink http://127.0.0.1:1/mcp --header Authorization: Bearer tok"
        );
    }
}
