//! 本机同步失败的文件“没有权限”时，用管理员权限修复：Windows 弹出确认框，用户同意后给当前账户授权。
//! 只改同步失败的那几个文件夹（和里面的东西），不进入目录链接，不会改到同步目录以外。
use super::{SyncState, load_task};
use pierlink_core::error::AppError;
use pierlink_core::sync;
use serde::Serialize;
use specta::Type;
use std::path::Path;
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// 修复权限的结果
#[derive(Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FixPermissionsDto {
    /// 修好了几个文件夹（连同里面的文件和子文件夹）
    pub fixed: u32,
    /// 有几个文件夹没修好（里面有改不了的）
    pub failed: u32,
}

/// 用管理员权限修复本机同步失败里“没有权限”的文件夹，让当前账户能读写；修好后让同步程序重新扫描，
/// 失败的文件会自动重试。没有要修的直接返回 0 个。
///
/// :param task_id: 任务 ID
/// :return: 修好和没修好的文件夹个数（现在没有“没有权限”的失败时都是 0）；失败的都在目录链接里时返回 sync.fix_unreachable；
/// 用户在确认框里点了“否”返回 sync.fix_cancelled，非 Windows 返回 sync.fix_unsupported
#[tauri::command]
#[specta::specta]
pub async fn sync_fix_permissions(
    app: AppHandle,
    task_id: String,
) -> Result<FixPermissionsDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (task, _) = load_task(&app, &task_id)?;
        let client = {
            let state = app.state::<SyncState>();
            let mut inner = state.lock();
            let local = super::ensure_local(&app, &mut inner)?;
            local.client().clone()
        };
        let (errors, _) = sync::folder_errors(&client, &task.folder_id, Duration::from_secs(10))?;
        let denied: Vec<String> = errors
            .into_iter()
            .filter(|e| sync::is_permission_denied(&e.reason))
            .map(|e| e.path)
            .collect();
        if denied.is_empty() {
            return Ok(FixPermissionsDto {
                fixed: 0,
                failed: 0,
            });
        }
        let targets = sync::permission_fix_targets(Path::new(&task.local_path), &denied);
        if targets.is_empty() {
            // 失败的都在目录链接里（或路径不合法）：不能顺着链接去改
            return Err(AppError::new("sync.fix_unreachable"));
        }
        eprintln!(
            "用管理员权限修复同步目录的权限: 任务={}, 文件夹数={}",
            task.name,
            targets.len()
        );
        let failed = elevated::grant(Path::new(&task.local_path), &targets)?;
        eprintln!("修复权限结束: 任务={}, 没修好={failed}", task.name);
        // 让同步程序马上重新扫描，失败的文件接着重试；扫描大目录要一会儿，不在这里等
        let folder_id = task.folder_id.clone();
        std::thread::spawn(move || {
            if let Err(e) = client.scan_with_timeout(&folder_id, Duration::from_secs(600)) {
                eprintln!(
                    "修复权限后重新扫描失败: 文件夹={folder_id}, 错误={}",
                    e.code
                );
            }
        });
        let total = u32::try_from(targets.len()).unwrap_or(u32::MAX);
        Ok(FixPermissionsDto {
            fixed: total.saturating_sub(failed),
            failed,
        })
    })
    .await
    .map_err(|e| AppError::new("sync.task_failed").with("reason", e.to_string()))?
}

#[cfg(not(windows))]
mod elevated {
    use pierlink_core::error::AppError;
    use std::path::{Path, PathBuf};

    /// 只有 Windows 上能修复权限
    pub(super) fn grant(_root: &Path, _targets: &[PathBuf]) -> Result<u32, AppError> {
        Err(AppError::new("sync.fix_unsupported"))
    }
}

#[cfg(windows)]
mod elevated {
    use base64::Engine;
    use pierlink_core::error::AppError;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_CANCELLED, GetLastError, HANDLE, LocalFree, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    /// 系统调用失败：带上是哪一步和系统给的原因
    fn failed(step: &str) -> AppError {
        AppError::new("sync.fix_failed").with(
            "reason",
            format!("{step}: {}", std::io::Error::last_os_error()),
        )
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// 当前账户（没有提权的 Pierlink 进程）的 SID 文字，例如 S-1-5-21-…。
    /// 提权时用户可能输入的是另一个管理员账户，所以要在这里取，不能在提权后的进程里取。
    fn current_user_sid() -> Result<String, AppError> {
        // SAFETY：只查询本进程的令牌；缓冲区按系统给的长度分配，用完关闭句柄、释放系统分配的字符串
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(failed("OpenProcessToken"));
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            // TOKEN_USER 里有指针：按 usize 分配，保证对齐
            let mut buf = vec![0usize; (len as usize).div_ceil(std::mem::size_of::<usize>())];
            let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
            if ok == 0 {
                let e = failed("GetTokenInformation");
                CloseHandle(token);
                return Err(e);
            }
            CloseHandle(token);
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut text: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
                return Err(failed("ConvertSidToStringSidW"));
            }
            let mut n = 0;
            while *text.add(n) != 0 {
                n += 1;
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
            LocalFree(text.cast());
            Ok(sid)
        }
    }

    /// 脚本自己出错（不是哪个文件夹没修好）时的退出码；没修好的个数最多 50，不会撞上
    const SCRIPT_ERROR: u32 = 255;
    /// 每个要修的文件夹最多检查多少个文件和文件夹：同步目录里有很大的目录（例如没同步的 node_modules）时不至于一直跑
    const MAX_ITEMS: u32 = 200_000;

    /// 提权运行的 PowerShell 脚本。路径和 SID 用 base64 传进去，脚本里不出现原文（路径里有引号等字符也不会被当成脚本）。
    /// 每个文件夹先核对：在同步目录里，并且到同步目录之间没有目录链接。然后去掉这个账户的拒绝项、加上完全控制
    /// （带继承，新权限会自动继承到下层）；再往下逐层检查，不继承上层权限的、或单独拒绝了这个账户的，也照样改。
    /// 不进入、不改目录链接。退出码是没修好的文件夹个数，脚本自己出错时是 SCRIPT_ERROR。
    fn script(sid: &str, root: &Path, targets: &[PathBuf]) -> String {
        let paths = targets
            .iter()
            .map(|p| format!("'{}'", b64(p.to_string_lossy().as_bytes())))
            .collect::<Vec<_>>()
            .join(",");
        let sid = b64(sid.as_bytes());
        let root = b64(root.to_string_lossy().as_bytes());
        format!(
            r#"$ErrorActionPreference = 'Stop'
try {{
function D($s) {{ [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($s)) }}
$id = New-Object Security.Principal.SecurityIdentifier (D '{sid}')
$base = (New-Object IO.DirectoryInfo (D '{root}')).FullName.TrimEnd('\')
$all = [Security.AccessControl.FileSystemRights]::FullControl
$allow = [Security.AccessControl.AccessControlType]::Allow
$deny = [Security.AccessControl.AccessControlType]::Deny
$inherit = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
$single = [Security.AccessControl.InheritanceFlags]::None
$noprop = [Security.AccessControl.PropagationFlags]::None
$access = [Security.AccessControl.AccessControlSections]::Access
$link = [IO.FileAttributes]::ReparsePoint
function Fix($item, $acl) {{
  $flags = if ($item -is [IO.DirectoryInfo]) {{ $inherit }} else {{ $single }}
  [void]$acl.RemoveAccessRuleAll((New-Object Security.AccessControl.FileSystemAccessRule($id, $all, $flags, $noprop, $deny)))
  $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($id, $all, $flags, $noprop, $allow)))
  $item.SetAccessControl($acl)
}}
function Inside($dir) {{
  $d = $dir
  while ($d -ne $null) {{
    if ($d.FullName.TrimEnd('\') -eq $base) {{ return $true }}
    if ($d.Attributes -band $link) {{ return $false }}
    $d = $d.Parent
  }}
  return $false
}}
function NeedsFix($acl) {{
  if ($acl.AreAccessRulesProtected) {{ return $true }}
  foreach ($r in $acl.GetAccessRules($true, $false, [Security.Principal.SecurityIdentifier])) {{
    if ($r.IdentityReference -eq $id -and $r.AccessControlType -eq $deny) {{ return $true }}
  }}
  return $false
}}
$failed = 0
foreach ($p in @({paths})) {{
  $ok = $true
  $seen = 0
  try {{
    $top = New-Object IO.DirectoryInfo (D $p)
    if (-not $top.Exists -or -not (Inside $top)) {{ $failed++; continue }}
    Fix $top ($top.GetAccessControl($access))
    $stack = New-Object Collections.Stack
    $stack.Push($top)
    while ($stack.Count -gt 0 -and $seen -lt {max_items}) {{
      $dir = $stack.Pop()
      try {{ $items = $dir.GetFileSystemInfos() }} catch {{ $ok = $false; continue }}
      foreach ($item in $items) {{
        $seen++
        if ($item.Attributes -band $link) {{ continue }}
        try {{
          $acl = $item.GetAccessControl($access)
          if (NeedsFix $acl) {{ Fix $item $acl }}
        }} catch {{ $ok = $false }}
        if ($item -is [IO.DirectoryInfo]) {{ $stack.Push($item) }}
      }}
    }}
    if ($stack.Count -gt 0) {{ $ok = $false }}
  }} catch {{ $ok = $false }}
  if (-not $ok) {{ $failed++ }}
}}
exit $failed
}} catch {{ exit {script_error} }}
"#,
            max_items = MAX_ITEMS,
            script_error = SCRIPT_ERROR,
        )
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// 弹出管理员确认框，同意后运行修复脚本，等它结束。
    ///
    /// :param targets: 要修复的文件夹（已经限制在同步目录里）
    /// :return: 没修好的文件夹个数；用户点了“否”返回 sync.fix_cancelled
    pub(super) fn grant(root: &Path, targets: &[PathBuf]) -> Result<u32, AppError> {
        let sid = current_user_sid()?;
        if !sid.strip_prefix("S-1-").is_some_and(|rest| {
            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'-')
        }) {
            return Err(AppError::new("sync.fix_failed")
                .with_reason("sync.account_sid_is_malformed", "账户 SID 格式不对"));
        }
        let utf16: Vec<u8> = script(&sid, root, targets)
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let system = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let exe = wide(
            system
                .join(r"System32\WindowsPowerShell\v1.0\powershell.exe")
                .as_os_str(),
        );
        let verb = wide("runas".as_ref());
        let params = format!(
            "-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -EncodedCommand {}",
            b64(&utf16)
        );
        // 命令行最长 32767 个字符（要修的路径已按字节数限制过，这里兜底）
        if exe.len() + params.len() > 32_000 {
            return Err(AppError::new("sync.fix_failed")
                .with_reason("sync.paths_to_fix_are_too_long", "要修复的路径太长"));
        }
        let params = wide(params.as_ref());
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: verb.as_ptr(),
            lpFile: exe.as_ptr(),
            lpParameters: params.as_ptr(),
            nShow: SW_HIDE,
            ..Default::default()
        };
        // SAFETY：结构体引用的字符串在调用期间都还活着；拿到的进程句柄等它结束后关闭
        unsafe {
            if ShellExecuteExW(&mut info) == 0 {
                return Err(if GetLastError() == ERROR_CANCELLED {
                    AppError::new("sync.fix_cancelled")
                } else {
                    failed("ShellExecuteExW")
                });
            }
            if info.hProcess.is_null() {
                return Err(AppError::new("sync.fix_failed")
                    .with_reason("sync.fix_process_handle_missing", "没有拿到修复程序的进程"));
            }
            let waited = WaitForSingleObject(info.hProcess, INFINITE);
            let mut code = 0u32;
            let got = GetExitCodeProcess(info.hProcess, &mut code);
            let result = if waited != WAIT_OBJECT_0 || got == 0 {
                Err(failed("WaitForSingleObject"))
            } else if code == SCRIPT_ERROR {
                Err(AppError::new("sync.fix_failed")
                    .with_reason("sync.fix_program_failed", "修复程序出错"))
            } else {
                Ok(code.min(u32::try_from(targets.len()).unwrap_or(u32::MAX)))
            };
            CloseHandle(info.hProcess);
            result
        }
    }
}
