//! 导出 / 导入配置（换电脑时迁移）：服务器、端口转发、片段、应用设置、MCP 设置。
//! 不含任何密码和令牌，也不含同步任务（两台电脑的目录不同，迁过去会出问题）。
//! 另有“只导出服务器”：给别人用，格式相同，只带选中的服务器和它们用到的跳板机。
use crate::data::DbState;
use crate::mcp::settings::{self as mcp_settings, McpSettingsDto};
use pierlink_core::error::AppError;
use pierlink_core::profile::{self, ImportReason, ProfileExtras};
use pierlink_core::storage::{AuthMethod, ServerSource};
use serde::Serialize;
use serde_json::{Value, json};
use specta::Type;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

/// 导入文件最大多少（和 core 的限制一致）
const MAX_FILE: u64 = 32 * 1024 * 1024;

fn failed(e: impl ToString) -> AppError {
    AppError::new("profile.io_failed").with("reason", e.to_string())
}

/// 导入时跳过或改动的一项
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ImportItemDto {
    pub name: String,
    /// duplicate / missing_jump_server / jump_cycle / missing_server / local_port_conflict / missing_key
    pub reason: String,
    /// 私钥路径（missing_key 时）
    pub path: Option<String>,
}

/// 导入结果
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ImportResultDto {
    pub servers_added: u32,
    pub forwards_added: u32,
    pub snippets_added: u32,
    /// 跳过或需要注意的项（重复、引用不到、端口冲突停用、私钥在这台电脑上不存在）
    pub notes: Vec<ImportItemDto>,
    /// 新增的服务器入口（MCP，默认关闭）
    pub mcp_entries_added: u32,
    /// 导出时的应用设置（JSON 文字）；界面决定怎么套用，没有时为 null
    pub app_settings: Option<String>,
}

fn reason_text(r: &ImportReason) -> &'static str {
    match r {
        ImportReason::Duplicate => "duplicate",
        ImportReason::MissingJumpServer => "missing_jump_server",
        ImportReason::JumpCycle => "jump_cycle",
        ImportReason::MissingServer => "missing_server",
        ImportReason::LocalPortConflict => "local_port_conflict",
    }
}

fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 只用来起默认文件名：按 UTC 算日期就够了
    let days = secs / 86_400;
    let (y, m, d) = civil(days as i64);
    format!("{y:04}{m:02}{d:02}")
}

/// 1970-01-01 起的天数 → 年月日
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// 导出到文件：弹出“另存为”，写入 JSON
///
/// :param app_settings: 界面的设置（JSON 文字）
/// :return: 写入的文件路径；用户取消时为 null
#[tauri::command]
#[specta::specta]
pub async fn profile_export(
    app: AppHandle,
    app_settings: String,
) -> Result<Option<String>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let app_value: Value = serde_json::from_str(&app_settings).unwrap_or(Value::Null);
        let mcp = serde_json::to_value(mcp_settings::load(&app)).map_err(failed)?;
        let profile = app.state::<DbState>().with(|db| {
            profile::export_profile(
                db,
                ProfileExtras {
                    app_version: env!("CARGO_PKG_VERSION").to_owned(),
                    settings: json!({ "app": app_value, "mcp": mcp }),
                },
            )
        })?;
        let Some(target) = app
            .dialog()
            .file()
            .set_file_name(format!("pierlink-{}.json", today()))
            .add_filter("Pierlink", &["json"])
            .blocking_save_file()
        else {
            return Ok(None);
        };
        let path = target.into_path().map_err(failed)?;
        let text = serde_json::to_string_pretty(&profile).map_err(failed)?;
        std::fs::write(&path, text).map_err(failed)?;
        eprintln!(
            "导出配置: 文件={}, 服务器={}, 转发={}, 片段={}",
            path.display(),
            profile.servers.len(),
            profile.port_forwards.len(),
            profile.snippets.len()
        );
        Ok(Some(path.to_string_lossy().into_owned()))
    })
    .await
    .map_err(failed)?
}

/// 私钥路径在用户目录下时改成 ~ 开头，别人导入时换成他自己的用户目录
fn to_portable(path: &str, home: &Path) -> String {
    let home = home.to_string_lossy().replace('\\', "/");
    let home = home.trim_end_matches('/');
    let path_norm = path.replace('\\', "/");
    if home.is_empty() || path_norm.len() <= home.len() || !path_norm.is_char_boundary(home.len()) {
        return path.to_owned();
    }
    let (head, rest) = path_norm.split_at(home.len());
    if head.eq_ignore_ascii_case(home) && rest.starts_with('/') {
        format!("~{rest}")
    } else {
        path.to_owned()
    }
}

/// ~ 开头的私钥路径换成本机用户目录
fn from_portable(path: &str, home: &Path) -> String {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => {
            let rest = if cfg!(windows) {
                rest.replace('/', "\\")
            } else {
                rest.to_owned()
            };
            home.join(rest).to_string_lossy().into_owned()
        }
        None => path.to_owned(),
    }
}

/// 选中的服务器加上它们（一层层）用到的跳板机
fn with_jumps(servers: &[pierlink_core::storage::Server], ids: &[String]) -> HashSet<String> {
    let mut keep: HashSet<String> = ids.iter().cloned().collect();
    loop {
        let more: Vec<String> = servers
            .iter()
            .filter(|s| keep.contains(&s.id))
            .filter_map(|s| s.jump_server_id.clone())
            .filter(|j| !keep.contains(j))
            .collect();
        if more.is_empty() {
            return keep;
        }
        keep.extend(more);
    }
}

/// 只导出服务器（给别人导入）：选中的服务器和它们的跳板机（不含从 ssh config 导入的）；
/// 不含转发、片段、设置、密码、备注；收藏去掉；专用密钥登录改成默认密钥；用户目录下的私钥路径改成 ~ 开头
///
/// :param ids: 要导出的服务器 ID
/// :return: 写入的文件路径；用户取消时为 null
#[tauri::command]
#[specta::specta]
pub async fn servers_export(app: AppHandle, ids: Vec<String>) -> Result<Option<String>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut profile = app.state::<DbState>().with(|db| {
            profile::export_profile(
                db,
                ProfileExtras {
                    app_version: env!("CARGO_PKG_VERSION").to_owned(),
                    settings: json!({}),
                },
            )
        })?;
        let keep = with_jumps(&profile.servers, &ids);
        let home = app.path().home_dir().unwrap_or_default();
        profile
            .servers
            .retain(|s| keep.contains(&s.id) && s.source == ServerSource::Manual);
        profile.port_forwards.clear();
        profile.snippets.clear();
        for s in &mut profile.servers {
            s.favorite = false;
            s.notes.clear();
            s.engine_note = None;
            // 专用密钥只在导出的这台电脑上有：对方先用默认密钥，不认时会问密码
            if s.auth_method == AuthMethod::DedicatedKey {
                s.auth_method = AuthMethod::Agent;
            }
            s.identity_file = s.identity_file.as_deref().map(|p| to_portable(p, &home));
            for p in &mut s.identity_files {
                *p = to_portable(p, &home);
            }
        }
        if profile.servers.is_empty() {
            return Err(AppError::new("profile.nothing_to_export"));
        }
        let name = match profile.servers.as_slice() {
            [one] => format!("pierlink-{}.json", safe_name(&one.name)),
            _ => format!("pierlink-servers-{}.json", today()),
        };
        let Some(target) = app
            .dialog()
            .file()
            .set_file_name(name)
            .add_filter("Pierlink", &["json"])
            .blocking_save_file()
        else {
            return Ok(None);
        };
        let path = target.into_path().map_err(failed)?;
        let text = serde_json::to_string_pretty(&profile).map_err(failed)?;
        std::fs::write(&path, text).map_err(failed)?;
        eprintln!(
            "导出服务器: 文件={}, 服务器={}",
            path.display(),
            profile.servers.len()
        );
        Ok(Some(path.to_string_lossy().into_owned()))
    })
    .await
    .map_err(failed)?
}

/// 文件名里不能用的字符换成 -
fn safe_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(60)
        .collect();
    if s.is_empty() { "server".into() } else { s }
}

/// 把导入的 MCP 设置并进现有设置：服务器 ID 按映射改写；只新增现在没有的服务器入口，
/// 新增的入口默认关闭；本机入口、总开关和已有入口都不动
///
/// :return: 新增的服务器入口数
fn merge_mcp(
    app: &AppHandle,
    imported: &Value,
    map: &BTreeMap<String, String>,
) -> Result<u32, AppError> {
    let Ok(imported) = serde_json::from_value::<McpSettingsDto>(imported.clone()) else {
        return Ok(0);
    };
    let remap = |ids: &[String]| -> Vec<String> {
        ids.iter().filter_map(|id| map.get(id).cloned()).collect()
    };
    let mut current = mcp_settings::load(app);
    // 本机入口允许的服务器不从文件里并进来：本机入口可能正开着，导入文件不能悄悄扩大 AI 能操作的范围
    let mut added = 0;
    for entry in imported.servers {
        let Some(server_id) = map.get(&entry.server_id).cloned() else {
            continue;
        };
        if current.servers.iter().any(|s| s.server_id == server_id) {
            continue;
        }
        let mut e = entry.clone();
        e.server_id = server_id;
        e.entry.enabled = false;
        e.entry.remembered = false;
        e.entry.servers = remap(&entry.entry.servers);
        current.servers.push(e);
        added += 1;
    }
    mcp_settings::save(app, current)?;
    Ok(added)
}

/// 从文件导入：弹出“打开”，读入后合并进现有配置（整体事务，失败时什么都不改）
///
/// :return: 导入结果；用户取消时为 null
#[tauri::command]
#[specta::specta]
pub async fn profile_import(app: AppHandle) -> Result<Option<ImportResultDto>, AppError> {
    tauri::async_runtime::spawn_blocking(move || import_file(&app, false))
        .await
        .map_err(failed)?
}

/// 从文件只导入服务器（别人导出的服务器文件，或完整备份里的服务器部分）
///
/// :return: 导入结果；用户取消时为 null
#[tauri::command]
#[specta::specta]
pub async fn servers_import(app: AppHandle) -> Result<Option<ImportResultDto>, AppError> {
    tauri::async_runtime::spawn_blocking(move || import_file(&app, true))
        .await
        .map_err(failed)?
}

/// 选文件、校验、导入；servers_only 时丢掉文件里的转发、片段和设置
fn import_file(app: &AppHandle, servers_only: bool) -> Result<Option<ImportResultDto>, AppError> {
    {
        let Some(source) = app
            .dialog()
            .file()
            .add_filter("Pierlink", &["json"])
            .blocking_pick_file()
        else {
            return Ok(None);
        };
        let path = source.into_path().map_err(failed)?;
        let size = std::fs::metadata(&path).map_err(failed)?.len();
        if size > MAX_FILE {
            return Err(AppError::new("profile.limit_exceeded")
                .with_reason("profile.file_too_large", "文件太大"));
        }
        let bytes = std::fs::read(&path).map_err(failed)?;
        let mut parsed = profile::parse_profile(&bytes)?;
        if servers_only {
            parsed.port_forwards.clear();
            parsed.snippets.clear();
            parsed.extras = json!({});
        }
        let home = app.path().home_dir().unwrap_or_default();
        for s in &mut parsed.servers {
            s.identity_file = s.identity_file.as_deref().map(|p| from_portable(p, &home));
            for p in &mut s.identity_files {
                *p = from_portable(p, &home);
            }
        }
        let before: Vec<String> = app
            .state::<DbState>()
            .with(|db| db.list_port_forwards(None))?
            .into_iter()
            .map(|f| f.id)
            .collect();
        let report = app
            .state::<DbState>()
            .with(|db| profile::import_profile(db, &parsed))?;
        let mut notes: Vec<ImportItemDto> = report
            .servers_skipped
            .iter()
            .chain(&report.forwards_skipped)
            .chain(&report.forwards_disabled)
            .chain(&report.snippets_skipped)
            .map(|i| ImportItemDto {
                name: i.name.clone(),
                reason: reason_text(&i.reason).into(),
                path: None,
            })
            .collect();
        notes.extend(report.missing_identity_files.iter().map(|m| ImportItemDto {
            name: m.name.clone(),
            reason: "missing_key".into(),
            path: Some(m.path.clone()),
        }));
        let mcp_entries_added = match parsed.extras.get("mcp") {
            Some(mcp) => merge_mcp(app, mcp, &report.server_id_map).unwrap_or_else(|e| {
                eprintln!("导入 MCP 设置失败: 错误={}", e.code);
                0
            }),
            None => 0,
        };
        // 新导入且启用的端口转发：现在就开起来（不用等下次启动）
        if let Ok(after) = app
            .state::<DbState>()
            .with(|db| db.list_port_forwards(None))
        {
            for f in after
                .iter()
                .filter(|f| f.enabled && !before.contains(&f.id))
            {
                if let Err(e) = crate::forwards::start(app, f) {
                    eprintln!("开启导入的端口转发失败: id={}, 错误={}", f.id, e.code);
                }
            }
        }
        let app_settings = parsed
            .extras
            .get("app")
            .filter(|v| !v.is_null())
            .map(Value::to_string);
        eprintln!(
            "导入配置: 文件={}, 新增服务器={}, 新增转发={}, 新增片段={}, 新增入口={mcp_entries_added}, 备注={}",
            path.display(),
            report.servers_added,
            report.forwards_added,
            report.snippets_added,
            notes.len()
        );
        Ok(Some(ImportResultDto {
            servers_added: report.servers_added as u32,
            forwards_added: report.forwards_added as u32,
            snippets_added: report.snippets_added as u32,
            notes,
            mcp_entries_added,
            app_settings,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{civil, from_portable, to_portable, with_jumps};
    use std::path::Path;

    #[test]
    fn 私钥路径在用户目录下时换成波浪号() {
        let home = Path::new("C:\\Users\\alice");
        assert_eq!(
            to_portable("C:\\Users\\Alice\\.ssh\\id_ed25519", home),
            "~/.ssh/id_ed25519"
        );
        assert_eq!(to_portable("D:\\keys\\id", home), "D:\\keys\\id");
        assert_eq!(
            to_portable("C:\\Users\\alice2\\id", home),
            "C:\\Users\\alice2\\id"
        );
        assert_eq!(
            to_portable("C:/Users/alice/.ssh/id_rsa", home),
            "~/.ssh/id_rsa"
        );
        let bob = Path::new("/home/bob");
        assert_eq!(
            from_portable("~/.ssh/id_ed25519", bob),
            "/home/bob/.ssh/id_ed25519"
        );
        assert_eq!(from_portable("/etc/key", bob), "/etc/key");
    }

    #[test]
    fn 导出时带上跳板机() {
        let mk = |id: &str, jump: Option<&str>| {
            let mut s: pierlink_core::storage::Server = serde_json::from_value(serde_json::json!({
                "id": id, "name": id, "group_name": null, "favorite": false, "sort_order": 0,
                "source": "manual", "connection_engine": "builtin", "engine_note": null, "ssh_alias": null,
                "host": "h", "port": 22, "username": "u", "auth_method": "agent", "identity_file": null,
                "jump_server_id": null, "login_shell": null, "use_tmux": false, "remember_password": false,
                "password_cred": null, "host_key_sha256": null, "vnc_autostart": false, "notes": "",
                "created_at": 0, "updated_at": 0
            }))
            .unwrap();
            s.jump_server_id = jump.map(str::to_owned);
            s
        };
        let all = vec![
            mk("a", Some("b")),
            mk("b", Some("c")),
            mk("c", None),
            mk("d", None),
        ];
        let keep = with_jumps(&all, &["a".into()]);
        assert_eq!(keep.len(), 3);
        assert!(!keep.contains("d"));
    }

    #[test]
    fn 天数换算成日期() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_752), (2026, 10, 26));
        assert_eq!(civil(11_016), (2000, 2, 29));
    }
}
