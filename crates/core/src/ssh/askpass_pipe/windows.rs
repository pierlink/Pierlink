//! 仅当前用户和 SYSTEM 可连接的本地命名管道；轮询模式避免退出时遗留阻塞的 Win32 调用。
use super::*;
use std::{
    ffi::c_void,
    mem::size_of,
    os::windows::ffi::OsStrExt,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING,
        ERROR_PIPE_NOT_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile,
        SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
    },
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_NOWAIT,
            PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
            SetNamedPipeHandleState, WaitNamedPipeW,
        },
        Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
    },
};
struct Handle(HANDLE);
// 句柄独占所有权，每次只有所属线程操作，Drop 负责关闭。
unsafe impl Send for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(Some(0))
        .collect()
}
fn user_sid() -> Result<String, AppError> {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(failed("读取当前用户失败"));
        }
        let token = Handle(token);
        let mut length = 0;
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut length);
        if length < size_of::<TOKEN_USER>() as u32 {
            return Err(failed("用户标识长度无效"));
        }
        // usize 向量确保 TOKEN_USER 所需的指针对齐。
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
pub(super) struct Listener {
    endpoint: String,
    pending: Handle,
    security: LocalAllocation,
    executable: std::path::PathBuf,
}
// 描述符从建立监听起只读，独占移交给监听线程。
unsafe impl Send for Listener {}
impl Listener {
    pub(super) fn bind(_: &Path, client: Option<&Path>) -> Result<Self, AppError> {
        let endpoint = format!(r"\\.\pipe\pierlink-askpass-{}", random_secret(16));
        let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{})", user_sid()?));
        let mut descriptor = null_mut();
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
        let security = LocalAllocation(descriptor);
        let pending = create(&endpoint, &security, true)?;
        // 只接受这个程序来要密码：默认就是本程序自己（ssh 把本程序当 askpass 调用）
        let executable = client
            .map(Path::to_path_buf)
            .map_or_else(std::env::current_exe, Ok)
            .and_then(std::fs::canonicalize)
            .map_err(|_| failed("读取程序路径失败"))?;
        Ok(Self {
            endpoint,
            pending,
            security,
            executable,
        })
    }
    pub(super) fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub(super) fn accept(&mut self, stopped: &Arc<AtomicBool>) -> Result<Option<Stream>, AppError> {
        let connected = unsafe { ConnectNamedPipe(self.pending.0, null_mut()) };
        let error = if connected == 0 {
            unsafe { GetLastError() }
        } else {
            ERROR_PIPE_CONNECTED
        };
        if error == ERROR_PIPE_LISTENING {
            return Ok(None);
        }
        if error == ERROR_NO_DATA || error == ERROR_PIPE_NOT_CONNECTED {
            self.pending = create(&self.endpoint, &self.security, false)?;
            return Ok(None);
        }
        if error != ERROR_PIPE_CONNECTED {
            return Err(failed("接受管道连接失败"));
        }
        // 非阻塞模式首次调用也可能仅表示实例已进入监听；没有客户端时继续保留该实例。
        let mut client_pid = 0;
        if connected != 0
            && unsafe { GetNamedPipeClientProcessId(self.pending.0, &mut client_pid) } == 0
        {
            return Ok(None);
        }
        // 在释放已连接实例前先创建下一实例，整个服务周期都占有随机管道名。
        let next = create(&self.endpoint, &self.security, false)?;
        let current = std::mem::replace(&mut self.pending, next);
        if !same_executable(current.0, &self.executable) {
            return Ok(None);
        }
        Ok(Some(Stream {
            handle: current,
            deadline: Instant::now() + TIMEOUT,
            stopped: Some(stopped.clone()),
        }))
    }
}
fn create(endpoint: &str, security: &LocalAllocation, first: bool) -> Result<Handle, AppError> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0,
        bInheritHandle: 0,
    };
    let name = wide(endpoint);
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX
                | if first {
                    FILE_FLAG_FIRST_PIPE_INSTANCE
                } else {
                    0
                },
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
            64,
            MAX_MESSAGE as u32,
            MAX_MESSAGE as u32,
            0,
            &attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(failed("建立管道实例失败"))
    } else {
        Ok(Handle(handle))
    }
}
fn same_executable(pipe: HANDLE, expected: &Path) -> bool {
    unsafe {
        let mut pid = 0;
        if GetNamedPipeClientProcessId(pipe, &mut pid) == 0 {
            return false;
        }
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let process = Handle(handle);
        let mut path = vec![0u16; 32768];
        let mut length = path.len() as u32;
        if QueryFullProcessImageNameW(process.0, 0, path.as_mut_ptr(), &mut length) == 0 {
            return false;
        }
        let Ok(path) = String::from_utf16(&path[..length as usize]) else {
            return false;
        };
        std::fs::canonicalize(path).is_ok_and(|path| path == expected)
    }
}
pub(super) struct Stream {
    handle: Handle,
    deadline: Instant,
    stopped: Option<Arc<AtomicBool>>,
}
impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let mut count = 0;
            let result = unsafe {
                ReadFile(
                    self.handle.0,
                    bytes.as_mut_ptr(),
                    bytes.len().min(u32::MAX as usize) as u32,
                    &mut count,
                    null_mut(),
                )
            };
            if result != 0 {
                return Ok(count as usize);
            }
            let error = unsafe { GetLastError() };
            if error != ERROR_NO_DATA {
                return Err(std::io::Error::from_raw_os_error(error as i32));
            }
            if Instant::now() >= self.deadline
                || self
                    .stopped
                    .as_ref()
                    .is_some_and(|s| s.load(Ordering::Acquire))
            {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}
impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        loop {
            let mut count = 0;
            let result = unsafe {
                WriteFile(
                    self.handle.0,
                    bytes.as_ptr(),
                    bytes.len().min(u32::MAX as usize) as u32,
                    &mut count,
                    null_mut(),
                )
            };
            if result != 0 && count != 0 {
                return Ok(count as usize);
            }
            if result == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if Instant::now() >= self.deadline
                || self
                    .stopped
                    .as_ref()
                    .is_some_and(|s| s.load(Ordering::Acquire))
            {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(super) fn connect(endpoint: &str) -> Result<Stream, AppError> {
    if !endpoint.starts_with(r"\\.\pipe\pierlink-askpass-")
        || endpoint.len() != r"\\.\pipe\pierlink-askpass-".len() + 32
    {
        return Err(failed("管道名称无效"));
    }
    let name = wide(endpoint);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                0x80000000 | 0x40000000,
                0,
                null(),
                OPEN_EXISTING,
                // 只允许服务端识别身份、不允许模拟，防止管道名被冒用时借用本进程身份
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            let handle = Handle(handle);
            let mode = PIPE_READMODE_BYTE | PIPE_NOWAIT;
            if unsafe { SetNamedPipeHandleState(handle.0, &mode, null(), null()) } == 0 {
                return Err(failed("设置管道时限失败"));
            }
            return Ok(Stream {
                handle,
                deadline: Instant::now() + TIMEOUT,
                stopped: None,
            });
        }
        if unsafe { GetLastError() } != ERROR_PIPE_BUSY || Instant::now() >= deadline {
            return Err(failed("连接管道失败"));
        }
        unsafe {
            WaitNamedPipeW(name.as_ptr(), 50);
        }
    }
}
