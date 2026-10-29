//! Windows 命名管道：服务端（Pierlink 主程序）和桥接端（`Pierlink.exe --mcp`）。
//! 权限做法和 askpass 管道相同：DACL 只给 SYSTEM 和当前用户、拒绝远程客户端、核对对方进程的程序路径。
use super::{UNAVAILABLE, answer_unavailable, relay};
use crate::error::AppError;
use sha2::{Digest, Sha256};
use std::{
    ffi::c_void,
    mem::size_of,
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::{Path, PathBuf},
    ptr::null_mut,
    time::{Duration, Instant},
};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::{
        Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
        Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
    },
};

/// 同时连着的客户端上限（每个 agent 会话一个）
// 每个入口最多 8 个会话；先建好下一个实例再把当前客户端交出去，所以要多一个
const MAX_INSTANCES: usize = 9;
/// 桥接程序等 Pierlink 的管道出现最多这么久（agent 启动 MCP 服务器有超时，不能等太久）
const CONNECT_WAIT: Duration = Duration::from_secs(10);

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY：句柄由本结构独占，只关一次
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY：指针来自 LocalAlloc 系列函数的输出，只释放一次
        unsafe {
            LocalFree(self.0);
        }
    }
}

/// 管道的安全描述符（只给 SYSTEM 和当前用户）；建好后只读
struct Security(LocalAllocation);
// SAFETY：描述符建好后只读，不同线程同时读取它是安全的
unsafe impl Send for Security {}
unsafe impl Sync for Security {}

fn failed(reason: &str) -> AppError {
    AppError::new("mcp.pipe_failed").with("reason", reason)
}

fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

/// 进程所属用户的 SID 字符串
fn process_sid(process: HANDLE) -> Result<String, AppError> {
    // SAFETY：所有指针都指向本函数里有效的缓冲区；句柄由 OwnedHandle 负责关闭
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return Err(failed("读取进程用户失败"));
        }
        let token = OwnedHandle(token);
        let mut length = 0;
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut length);
        if length < size_of::<TOKEN_USER>() as u32 {
            return Err(failed("用户标识长度无效"));
        }
        // usize 向量保证 TOKEN_USER 需要的指针对齐
        let mut buffer = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
        ) == 0
        {
            return Err(failed("读取用户标识失败"));
        }
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut sid = null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
            return Err(failed("转换用户标识失败"));
        }
        let _allocation = LocalAllocation(sid.cast());
        let mut count = 0;
        while *sid.add(count) != 0 {
            count += 1;
        }
        String::from_utf16(std::slice::from_raw_parts(sid, count))
            .map_err(|_| failed("用户标识编码无效"))
    }
}

fn current_user_sid() -> Result<String, AppError> {
    // SAFETY：GetCurrentProcess 返回伪句柄，不需要关闭
    process_sid(unsafe { GetCurrentProcess() })
}

/// 进程的程序路径（规范化后）；拿不到返回 None
fn process_image(process: HANDLE) -> Option<PathBuf> {
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    // SAFETY：缓冲区长度和 length 一致
    if unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut length) } == 0 {
        return None;
    }
    let path = String::from_utf16(&path[..length as usize]).ok()?;
    std::fs::canonicalize(path).ok()
}

fn open_process(pid: u32) -> Option<OwnedHandle> {
    // SAFETY：只申请查询权限；返回空表示失败
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    (!handle.is_null()).then_some(OwnedHandle(handle))
}

fn own_executable() -> Result<PathBuf, AppError> {
    std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|_| failed("读取程序路径失败"))
}

/// 当前用户的 MCP 管道名：`\\.\pipe\pierlink-mcp-<SID 的 SHA-256 前 16 位十六进制>`。
/// 按用户区分，同一台电脑上不同用户的 Pierlink 互不干扰。
///
/// :return: 管道名；读不到当前用户时返回 mcp.pipe_failed
pub fn pipe_name() -> Result<String, AppError> {
    let digest = Sha256::digest(current_user_sid()?.as_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(r"\\.\pipe\pierlink-mcp-{hex}"))
}

fn security() -> Result<Security, AppError> {
    let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{})", current_user_sid()?));
    let mut descriptor = null_mut();
    // SAFETY：sddl 以 0 结尾；输出的描述符由 LocalAllocation 释放
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(failed("建立管道权限失败"));
    }
    Ok(Security(LocalAllocation(descriptor)))
}

fn create(name: &str, security: &Security, first: bool) -> std::io::Result<NamedPipeServer> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0.0,
        bInheritHandle: 0,
    };
    // SAFETY：attributes 和其中的描述符在调用期间有效
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .max_instances(MAX_INSTANCES)
            .create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    }
}

/// 管道另一头（客户端）是不是同一个程序
fn client_is_ours(pipe: &NamedPipeServer, expected: &Path) -> bool {
    let mut pid = 0;
    // SAFETY：句柄来自仍然有效的管道对象
    if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle() as HANDLE, &mut pid) } == 0 {
        return false;
    }
    open_process(pid)
        .and_then(|process| process_image(process.0))
        .is_some_and(|path| path == expected)
}

/// 管道另一头（服务端）是不是同一个程序、同一个用户
fn server_is_ours(pipe: &NamedPipeClient, expected: &Path, user: &str) -> bool {
    let mut pid = 0;
    // SAFETY：句柄来自仍然有效的管道对象
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle() as HANDLE, &mut pid) } == 0 {
        return false;
    }
    let Some(process) = open_process(pid) else {
        return false;
    };
    process_image(process.0).is_some_and(|path| path == expected)
        && process_sid(process.0).is_ok_and(|sid| sid == user)
}

/// 在管道上一直接受连接；每个核对通过（同一个程序）的客户端交给 on_client，其余直接断开。
/// 管道名已被别的进程占用（可能是别的用户抢先建的）时返回错误，不在别人的管道上服务。
/// 一般作为后台任务运行，停止时直接结束这个任务。
///
/// :param name: 管道名（见 pipe_name）
/// :param on_client: 收到一个核对通过的客户端时调用
/// :return: 只在出错时返回（管道建不起来）
pub async fn serve<F>(name: String, on_client: F) -> Result<(), AppError>
where
    F: Fn(NamedPipeServer) + Send + Sync + 'static,
{
    let security = security()?;
    let expected = own_executable()?;
    let mut server = create(&name, &security, true).map_err(|e| {
        eprintln!("建立 MCP 管道失败: 管道={name}, 错误={e}");
        failed("管道名已被占用或无法建立")
    })?;
    loop {
        if let Err(e) = server.connect().await {
            eprintln!("等待 MCP 客户端失败: 错误={e}");
        }
        // 先建下一个实例再处理当前客户端，避免两个客户端之间出现没有实例可连的空档
        let next = loop {
            match create(&name, &security, false) {
                Ok(next) => break next,
                // 实例数到上限：等有客户端断开
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => {
                    eprintln!("建立 MCP 管道实例失败: 错误={e}");
                    return Err(failed("无法建立新的管道实例"));
                }
            }
        };
        let connected = std::mem::replace(&mut server, next);
        if client_is_ours(&connected, &expected) {
            on_client(connected);
        } else {
            eprintln!("拒绝 MCP 管道连接：对方不是 Pierlink 程序");
        }
    }
}

async fn connect(name: &str) -> Result<NamedPipeClient, AppError> {
    let deadline = Instant::now() + CONNECT_WAIT;
    loop {
        match ClientOptions::new().open(name) {
            Ok(pipe) => return Ok(pipe),
            Err(e)
                if Instant::now() < deadline
                    && matches!(
                        e.raw_os_error(),
                        Some(code) if code == ERROR_PIPE_BUSY as i32 || code == ERROR_FILE_NOT_FOUND as i32
                    ) =>
            {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => {
                eprintln!("连接 Pierlink 的 MCP 管道失败: 错误={e}");
                return Err(failed("连不上 Pierlink"));
            }
        }
    }
}

async fn bridge() -> i32 {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let pipe = match (pipe_name(), own_executable(), current_user_sid()) {
        (Ok(name), Ok(expected), Ok(user)) => match connect(&name).await {
            Ok(pipe) if server_is_ours(&pipe, &expected, &user) => Some(pipe),
            Ok(_) => {
                eprintln!("MCP 管道的另一头不是本用户的 Pierlink，拒绝连接");
                None
            }
            Err(_) => None,
        },
        _ => None,
    };
    let Some(pipe) = pipe else {
        return match answer_unavailable(stdin, stdout, UNAVAILABLE).await {
            Ok(()) => 1,
            Err(e) => {
                eprintln!("回答 agent 失败: 错误={e}");
                1
            }
        };
    };
    match relay(stdin, stdout, pipe).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("MCP 转发中断: 错误={e}");
            1
        }
    }
}

/// `Pierlink.exe --mcp`：把 agent 的标准输入输出接到运行中的 Pierlink。Pierlink 没在运行时不去启动它
/// （agent 退出时会结束整个进程树，会把 Pierlink 一起结束），只回错误说明。
///
/// :return: 进程退出码：正常结束 0，连不上或出错 1
pub fn run_bridge() -> i32 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("启动 MCP 桥接失败: 错误={e}");
            return 1;
        }
    };
    let code = runtime.block_on(bridge());
    // 读标准输入的阻塞线程可能还停在读取上，不等它
    runtime.shutdown_background();
    code
}
