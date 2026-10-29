//! Tauri 外壳：注册命令、生成前端类型、创建窗口。
//! 业务逻辑放在 `pierlink-core`，这里只做薄薄的一层转发。

mod app_shell;
mod builtin;
mod convert;
mod data;
mod deploy;
mod desktop;
mod files;
mod forwards;
mod host_key;
mod keys;
mod mcp;
mod mobile;
mod monitor;
mod network;
mod pages;
mod passwords;
mod profile;
mod servers;
mod snippets;
mod ssh_import;
mod sync;
mod terminal;
mod tunnels;
mod updates;

use pierlink_core::info::{self, AppInfo};
use tauri::Manager;
use tauri_specta::{Builder, collect_commands};

/// 读取应用版本与运行平台。版本号用打包配置里的（和自动更新比较的是同一个）。
#[tauri::command]
#[specta::specta]
fn app_info(app: tauri::AppHandle) -> AppInfo {
    let mut info = info::app_info();
    info.version = app.package_info().version.to_string();
    info
}

/// 构建命令注册表。命令列表只在这里维护一份，前端类型由它自动生成。
fn specta_builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new().commands(collect_commands![
        app_info,
        app_shell::app_shell_config,
        app_shell::app_tray_available,
        mcp::mcp_settings_get,
        mcp::mcp_settings_set,
        mcp::mcp_status,
        mcp::mcp_connect_info,
        mcp::mcp_token_rotate,
        mcp::mcp_activity,
        mcp::mcp_stop_all,
        mcp::mcp_confirm_answer,
        mcp::mcp_confirm_pending,
        mcp::mcp_terminal_opened,
        mcp::mcp_open_log_folder,
        mcp::terminal_set_user_control,
        mcp::mcp_handover_entries,
        mcp::mcp_hand_over,
        mcp::mcp_take_back,
        mcp::install::mcp_install_local,
        mcp::install::mcp_install_server,
        mcp::install::mcp_detect_clients,
        mcp::install::mcp_detect_server_clients,
        app_shell::app_exit_ack,
        app_shell::app_hide_to_tray,
        app_shell::app_exit_summary,
        app_shell::app_quit,
        data::app_db_restored,
        data::app_data_problem,
        terminal::list_local_shells,
        terminal::terminal_open_local,
        terminal::terminal_open_quick,
        terminal::terminal_write,
        terminal::terminal_report_cwd,
        terminal::terminal_resize,
        terminal::terminal_ack,
        terminal::terminal_close,
        terminal::terminal_open_server,
        terminal::clipboard_read_text,
        terminal::clipboard_write_text,
        terminal::open_in_browser,
        files::files_home,
        files::files_list,
        files::files_terminal_cwd,
        files::files_rename,
        files::files_create_dir,
        files::files_create_file,
        files::files_delete,
        files::files_read_text,
        files::files_write_text,
        files::files_mode,
        files::files_chmod,
        files::files_open_default,
        files::files_reveal,
        files::pick_folder,
        files::terminal_busy,
        network::app_connectivity,
        sync::sync_update_settings,
        sync::sync_revert_local,
        files::files_plan,
        files::files_transfer,
        files::files_cancel,
        host_key::host_key_check,
        host_key::host_key_answer,
        host_key::host_key_cancel,
        host_key::host_key_check_server,
        passwords::password_answer,
        passwords::password_enter,
        passwords::password_pending,
        passwords::password_known,
        servers::server_list,
        servers::server_save,
        servers::server_delete,
        monitor::monitor_watch,
        monitor::monitor_stop,
        monitor::monitor_visible,
        updates::update_check,
        updates::update_install,
        forwards::forward_list,
        forwards::forward_save,
        forwards::forward_set_enabled,
        forwards::forward_delete,
        mobile::mobile_status,
        mobile::mobile_configure,
        mobile::mobile_invite,
        mobile::mobile_devices,
        mobile::mobile_set_permissions,
        mobile::mobile_rename,
        mobile::mobile_cancel_invite,
        mobile::mobile_revoke,
        mobile::mobile_pair_answer,
        desktop::desktop_detect,
        desktop::desktop_connect,
        desktop::desktop_send,
        desktop::desktop_close,
        desktop::desktop_create,
        desktop::desktop_create_cancel,
        desktop::desktop_pick_package,
        desktop::desktop_stop,
        desktop::desktop_password,
        desktop::desktop_save_password,
        desktop::desktop_forget_password,
        desktop::desktop_open_window,
        profile::profile_export,
        profile::profile_import,
        profile::servers_export,
        profile::servers_import,
        snippets::snippet_list,
        snippets::snippet_save,
        snippets::snippet_delete,
        servers::server_set_favorite,
        servers::server_update_link,
        convert::server_convert_preview,
        convert::server_convert,
        servers::server_set_engine,
        builtin::server_login,
        servers::server_resolve,
        keys::key_status,
        keys::key_install,
        keys::key_remove,
        ssh_import::ssh_import_preview,
        ssh_import::ssh_import_cancel,
        ssh_import::ssh_import,
        deploy::deploy_inspect,
        deploy::deploy_run,
        deploy::deploy_cancel,
        sync::sync_deployed_servers,
        sync::sync_tasks,
        sync::sync_task_status,
        sync::sync_check_paths,
        sync::sync_create_task,
        sync::sync_set_paused,
        sync::sync_rescan,
        sync::sync_remove_task,
        sync::sync_events,
        sync::sync_conflicts,
        sync::sync_resolve_conflict,
        sync::sync_estimate,
        sync::sync_estimate_cancel,
        sync::sync_versions,
        sync::sync_trash,
        sync::sync_clear_trash,
        sync::endpoints::sync_add_endpoint,
        sync::endpoints::sync_remove_endpoint,
        sync::extras::sync_editors,
        sync::extras::sync_open_in_editor,
        sync::extras::sync_conflict_diff,
        sync::syncthing_ui::sync_open_syncthing_ui,
        sync::syncthing_ui::sync_config_report,
        sync::syncthing_ui::sync_config_resolve,
        sync::syncthing_ui::sync_config_dismiss,
        sync::sync_restore_versions,
    ])
}

/// 把命令和类型导出为前端的 TypeScript 文件（`src/bindings/commands.ts`）。
///
/// :return: 导出失败时返回错误说明
pub fn export_bindings() -> Result<(), String> {
    export_with(&specta_builder())
}

fn export_with(builder: &Builder<tauri::Wry>) -> Result<(), String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../src/bindings/commands.ts");
    builder
        .export(
            specta_typescript::Typescript::default()
                .header("// 本文件由 Rust 自动生成，请勿手改。\n// @ts-nocheck"),
            path,
        )
        .map_err(|e| format!("导出前端类型失败: 路径={path}, 原因={e}"))
}

/// 应用入口。
/// 自动化测试（Windows CI 端到端）用：设置了这个环境变量时打开 WebView2 调试端口（只监听本机）。
/// 普通使用不会设置；能给 Pierlink 设环境变量的程序本来就以同一用户身份运行，不会因此多出权限。
const E2E_DEVTOOLS_ENV: &str = "PIERLINK_E2E_DEVTOOLS_PORT";
/// Tauri（wry）在没有指定时使用的 WebView2 浏览器参数，指定时要自己带上
const DEFAULT_BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

/// 测试开关里的端口（1024–65535），没设置或无效时为 None
fn e2e_devtools_port() -> Option<u16> {
    std::env::var(E2E_DEVTOOLS_ENV)
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|p| *p >= 1024)
}

/// 按配置创建主窗口（配置里 create=false，由这里创建，才能按需加测试用的浏览器参数）
fn create_main_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    let Some(config) = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == "main")
        .cloned()
    else {
        return Err(tauri::Error::WindowNotFound);
    };
    let mut builder = tauri::WebviewWindowBuilder::from_config(app, &config)?;
    if let Some(args) = e2e_browser_args() {
        eprintln!("自动化测试：打开 WebView2 调试端口: 参数={args}");
        builder = builder.additional_browser_args(&args);
    }
    builder.build()?;
    Ok(())
}

/// 自动化测试时所有窗口都要带的浏览器参数（同一个数据目录下的 WebView2 窗口，参数必须完全一样，
/// 否则后开的窗口会创建失败）；平时为 None，用 wry 的默认参数
pub(crate) fn e2e_browser_args() -> Option<String> {
    e2e_devtools_port().map(|port| format!("{DEFAULT_BROWSER_ARGS} --remote-debugging-port={port}"))
}

/// 恢复本进程对 Ctrl+C 的正常处理（这个设置会被子进程继承）。
/// 如果 Pierlink 是被“新进程组”方式启动的（某些启动器、计划任务、脚本），Windows 会把它设成忽略 Ctrl+C，
/// 并一路传给它打开的 PowerShell、cmd 和里面运行的程序，结果终端里按 Ctrl+C 中断不了程序。
#[cfg(windows)]
fn restore_ctrl_c_for_children() {
    // SAFETY：传 NULL 处理函数和 FALSE 只是恢复默认行为，不注册回调
    let ok = unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 0) };
    if ok == 0 {
        eprintln!("恢复 Ctrl+C 处理失败：终端里 Ctrl+C 可能中断不了程序");
    }
}

pub fn run() {
    // `Pierlink.exe --mcp`：AI agent 启动的桥接程序，只在标准输入输出和运行中的 Pierlink 之间转发，
    // 不打开界面。要在单实例插件之前（否则参数被转给已运行的实例、本进程直接退出）和 askpass 判断之前
    #[cfg(windows)]
    if std::env::args_os()
        .skip(1)
        .eq([std::ffi::OsString::from("--mcp")])
    {
        std::process::exit(pierlink_core::mcp::pipe::run_bridge());
    }
    #[cfg(windows)]
    restore_ctrl_c_for_children();
    // ssh 把本程序当作 SSH_ASKPASS 调用时，只回答口令提示后立即退出
    if let Some(code) = keys::run_askpass_if_requested() {
        std::process::exit(code);
    }
    let builder = specta_builder();
    #[cfg(debug_assertions)]
    if let Err(e) = export_with(&builder) {
        eprintln!("{e}");
    }
    tauri::Builder::default()
        // 单实例必须最先注册：再次启动应用时只激活已有窗口，新进程随即退出（L2）
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            app_shell::show_main(app);
        }))
        // 自动更新：从公开仓库的 Releases 检查、下载并校验签名（公钥在 tauri.conf.json）
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .manage(app_shell::ShellState::default())
        .manage(terminal::TerminalState::new())
        .manage(sync::syncthing_ui::SyncthingUiState::default())
        .manage(files::FilesState::default())
        .manage(ssh_import::ImportState::default())
        .manage(host_key::HostKeyState::default())
        .manage(pages::PageGenerations::default())
        .manage(deploy::DeployState::default())
        .manage(sync::SyncState::default())
        .manage(tunnels::TunnelsState::default())
        .manage(forwards::ForwardsState::default())
        .manage(monitor::MonitorState::default())
        .manage(mobile::MobileState::default())
        .manage(desktop::DesktopState::default())
        .manage(convert::ConversionState::default())
        .invoke_handler(builder.invoke_handler())
        // 关闭窗口：缩到托盘，或交给界面做退出确认
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                app_shell::on_close_requested(window, api);
            }
        })
        // 页面开始加载（含重载、跳转）时，旧页面打开的终端已无人接收输出，统一关闭
        .on_page_load(|webview, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Started {
                // 先让旧页面发起、尚未完成的命令作废，再关闭旧页面已有的会话
                webview
                    .state::<pages::PageGenerations>()
                    .bump(webview.label());
                let state = webview.state::<terminal::TerminalState>();
                let closed = state.close_owned_by(webview.label());
                webview
                    .state::<host_key::HostKeyState>()
                    .cancel_owned_by(webview.label());
                webview.state::<sync::SyncState>().cancel_estimate();
                webview.state::<ssh_import::ImportState>().cancel();
                if closed > 0 {
                    eprintln!(
                        "页面重新加载，关闭旧终端: 页面={}, 数量={closed}",
                        webview.label()
                    );
                }
            }
        })
        .setup(move |app| {
            builder.mount_events(app);
            // 最先确定数据目录：旧版本的数据在安装目录里，先搬到新位置
            let prepared = data::prepare_data_dir(app.handle());
            create_main_window(app.handle())?;
            // 打开数据库（失败时应用仍可使用终端等功能，相关命令返回打开失败的原因）
            let db = match prepared {
                Ok(()) => data::DbState::open(app.handle()),
                Err(e) => data::DbState::failed(e),
            };
            // 部署模块用本电脑的短 ID 区分不同电脑（写在服务器端的锁和清单里）
            match db.with(|d| d.device_short_id()) {
                Ok(id) => {
                    if let Err(e) = pierlink_core::deploy::set_computer_short_id(&id) {
                        eprintln!("设置电脑短 ID 失败: 错误={e:?}");
                    }
                }
                Err(e) => eprintln!("读取电脑短 ID 失败: 错误={e:?}"),
            }
            app.manage(db);
            // 密码服务器的内存密码库和 askpass 管道：要在任何连接（同步、转发自动开启）之前建立
            let passwords = passwords::PasswordState::start(app.handle());
            app.manage(passwords);
            // 内置 SSH 的连接管理器和界面回调（按需连接）
            app.manage(builtin::BuiltinState::start(app.handle()));
            // AI 接入（MCP）：要在终端状态和内置连接之后；建不起来时只是不能用 AI 接入
            match mcp::McpState::start(app.handle()) {
                Ok(state) => {
                    app.manage(state);
                    mcp::apply_entries(app.handle());
                }
                Err(e) => eprintln!("启动 AI 接入失败: 错误={}", e.code),
            }
            // 托盘要读数据库里的同步任务，放在数据库之后
            app_shell::setup_tray(app.handle());
            data::start_daily_backup(app.handle());
            // 有同步任务时在后台启动本机同步服务并连接相关服务器
            sync::resume_on_startup(app.handle());
            // 已启用的端口转发在后台自动开启
            forwards::resume_enabled(app.handle());
            mobile::start_on_launch(app.handle());
            sync::cleanup_conflict_archive(app.handle());
            // 清理上次异常退出时残留的远程终端临时配置。必须在任何远程会话打开之前完成，
            // 所以在启动时同步执行（只读一次目录，耗时可忽略）。
            if let Ok(dir) = terminal::ssh_config_dir(app.handle()) {
                match pierlink_core::terminal::cleanup_stale_configs(&dir) {
                    Ok(n) if n > 0 => {
                        eprintln!("清理残留终端配置: 目录={}, 数量={n}", dir.display())
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("清理残留终端配置失败: 目录={}, 错误={e:?}", dir.display())
                    }
                }
            }
            // 两台服务器之间中转传输的临时文件：上次被强制结束时可能残留，启动时整体删除
            files::cleanup_staging(app.handle());
            sync::prune_events(app.handle());
            // 网络断开/恢复：通知界面，恢复时立即重连同步隧道
            network::start(app.handle());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("启动 Pierlink 失败")
        .run(|app, event| match event {
            // 窗口销毁时关闭它打开的终端（主窗口的 webview 标签与窗口标签相同）
            tauri::RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::Destroyed,
                ..
            } => {
                app.state::<pages::PageGenerations>().bump(&label);
                app.state::<terminal::TerminalState>()
                    .close_owned_by(&label);
                app.state::<host_key::HostKeyState>()
                    .cancel_owned_by(&label);
            }
            // 退出应用时结束所有终端会话（进程树）
            tauri::RunEvent::Exit => shutdown_all(app),
            _ => {}
        });
}

/// 退出前的清理：结束所有终端、停止本机同步、关闭隧道和资源概览、清空内存里的密码。
/// 正常退出和自动更新运行安装程序前都调用它。
pub(crate) fn shutdown_all(app: &tauri::AppHandle) {
    // 先断开 AI agent、放弃等待中的确认框，免得它们在关闭过程中继续操作
    mcp::shutdown(app);
    app.state::<terminal::TerminalState>().manager.close_all();
    // 退出应用 = 本机同步停止（设计决策）：关闭隧道、停止本机 Syncthing；服务器端照常运行
    app.state::<sync::SyncState>().shutdown();
    // 最后放开共享的隧道管理器（端口转发等其余隧道随之关闭）
    app.state::<monitor::MonitorState>().shutdown();
    mobile::shutdown(app);
    desktop::shutdown(app);
    app.state::<tunnels::TunnelsState>().shutdown();
    // 关掉内置 SSH 的连接（取消等待中的登录）
    if let Some(state) = app.try_state::<builtin::BuiltinState>() {
        state.shutdown();
    }
    // 连接都关了再停密码管道、清空内存里的密码
    if let Some(state) = app.try_state::<passwords::PasswordState>() {
        state.shutdown(app);
    }
}
