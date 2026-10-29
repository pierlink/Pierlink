//! ConPTY 原生启动：创建时即加入作业对象，避免启动与绑定之间派生孤儿进程。
use super::{AppError, PtySize, error};
use std::{
    ffi::OsStr,
    fs::File,
    io::{Read, Write},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{Console::*, JobObjects::*, Pipes::CreatePipe, Threading::*},
};
fn failed() -> AppError {
    error("spawn_failed", "创建 Windows 终端资源")
}
fn wide(value: &OsStr) -> Result<Vec<u16>, AppError> {
    let mut result: Vec<_> = value.encode_wide().collect();
    if result.contains(&0) {
        return Err(error("invalid_argument", "参数含空字符"));
    }
    result.push(0);
    Ok(result)
}
/// 命令行第一个参数用的程序路径：把正斜杠换成反斜杠（Windows 两者等价，但 cmd.exe 会把 “/x” 当开关）。
fn argv0(program: &Path) -> std::ffi::OsString {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let units: Vec<u16> = program
        .as_os_str()
        .encode_wide()
        .map(|u| {
            if u == u16::from(b'/') {
                u16::from(b'\\')
            } else {
                u
            }
        })
        .collect();
    std::ffi::OsString::from_wide(&units)
}
fn pipe() -> Result<(File, File), AppError> {
    let (mut read, mut write) = (std::ptr::null_mut(), std::ptr::null_mut());
    if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
        return Err(failed());
    }
    Ok(unsafe { (File::from_raw_handle(read), File::from_raw_handle(write)) })
}
pub(crate) struct Pair {
    pub master: Box<Master>,
}
pub(crate) struct Master {
    console: HPCON,
    closer: Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
    close_worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    reader: File,
    writer: Mutex<Option<File>>,
}
// ConPTY 句柄由 Master 独占；调用由会话互斥锁串行化。
unsafe impl Send for Master {}
impl Drop for Master {
    fn drop(&mut self) {
        let _ = self.shutdown_and_wait();
    }
}
pub(crate) fn open(size: PtySize) -> Result<Pair, AppError> {
    let (input, writer) = pipe()?;
    let (reader, output) = pipe()?;
    let mut console = 0;
    let coord = dimensions(size)?;
    // 保持 flags=0，不继承外部光标；隐藏指纹探测没有外部终端可提供光标位置。
    // ConPTY 仍可能发送 DA1 查询，隐藏探测由 host_key 回答，交互终端由 xterm.js 回答。
    if unsafe {
        CreatePseudoConsole(
            coord,
            input.as_raw_handle(),
            output.as_raw_handle(),
            0,
            &mut console,
        )
    } < 0
    {
        return Err(failed());
    }
    // ClosePseudoConsole 可能等待管道被排空；独立线程关闭，读取与回调继续运行。
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let close_worker =
        match std::thread::Builder::new()
            .name("关闭ConPTY".into())
            .spawn(move || {
                let _ = rx.recv();
                unsafe {
                    ClosePseudoConsole(console);
                }
            }) {
            Ok(worker) => worker,
            Err(_) => {
                unsafe {
                    ClosePseudoConsole(console);
                }
                return Err(failed());
            }
        };
    Ok(Pair {
        master: Box::new(Master {
            console,
            closer: Mutex::new(Some(tx)),
            close_worker: Mutex::new(Some(close_worker)),
            reader,
            writer: Mutex::new(Some(writer)),
        }),
    })
}
fn dimensions(size: PtySize) -> Result<COORD, AppError> {
    Ok(COORD {
        X: size
            .cols
            .try_into()
            .map_err(|_| error("invalid_size", "列数超过 ConPTY 上限"))?,
        Y: size
            .rows
            .try_into()
            .map_err(|_| error("invalid_size", "行数超过 ConPTY 上限"))?,
    })
}
impl Master {
    pub(crate) fn shutdown(&self) {
        super::lock(&self.closer).take();
    }

    /// 无参数；排空剩余输出并等待 ConPTY 关闭，成功时返回空值，重复调用安全。
    pub(crate) fn shutdown_and_wait(&self) -> std::io::Result<()> {
        self.shutdown();
        if let Some(worker) = super::lock(&self.close_worker).take() {
            // 指纹读取可能因输出超限提前停止；关闭时必须继续排空，避免关闭线程卡在写管道。
            let _ = std::io::copy(&mut &self.reader, &mut std::io::sink());
            worker
                .join()
                .map_err(|_| std::io::Error::other("等待 ConPTY 关闭线程失败"))?;
        }
        Ok(())
    }

    pub(crate) fn resize(&self, size: PtySize) -> Result<(), AppError> {
        let closer = super::lock(&self.closer);
        if closer.is_none() {
            return Err(error("not_found", "终端已结束"));
        }
        if unsafe { ResizePseudoConsole(self.console, dimensions(size)?) } < 0 {
            return Err(error("resize_failed", "调整 ConPTY 尺寸"));
        }
        Ok(())
    }
    pub(crate) fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, AppError> {
        Ok(Box::new(self.reader.try_clone().map_err(|_| failed())?))
    }
    pub(crate) fn take_writer(&self) -> Result<Box<dyn Write + Send>, AppError> {
        Ok(Box::new(
            super::lock(&self.writer).take().ok_or_else(failed)?,
        ))
    }
    pub(crate) fn spawn(
        &self,
        program: &Path,
        args: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
    ) -> Result<Child, AppError> {
        self.spawn_block(program, args, cwd, environment(env)?)
    }
    /// 只用给定的环境变量启动（不继承父进程环境），供指纹探测等需要最小环境的场合使用。
    pub(crate) fn spawn_clean(
        &self,
        program: &Path,
        args: &[String],
        env: &[(std::ffi::OsString, std::ffi::OsString)],
    ) -> Result<Child, AppError> {
        let mut block = Vec::new();
        for (key, value) in env {
            let key_text = key.to_string_lossy();
            if key_text.is_empty() || key_text.contains(['=', '\0']) {
                return Err(error("invalid_argument", "环境变量名称无效"));
            }
            let mut entry = key.clone();
            entry.push("=");
            entry.push(value);
            block.extend(wide(&entry)?);
        }
        block.push(0);
        if block.len() == 1 {
            block.push(0);
        }
        self.spawn_block(program, args, None, block)
    }
    fn spawn_block(
        &self,
        program: &Path,
        args: &[String],
        cwd: Option<&Path>,
        env: Vec<u16>,
    ) -> Result<Child, AppError> {
        let exe = wide(program.as_os_str())?;
        // 命令行里的程序路径统一用反斜杠：cmd.exe 会从整条命令行里找开关，
        // “C:\Windows\System32/cmd.exe” 中的 “/cmd.exe” 会被当成 /c 开关。
        let mut line = quote(&argv0(program))?;
        for arg in args {
            line.push(32);
            line.extend(quote(OsStr::new(arg))?);
        }
        line.push(0);
        let cwd = cwd.map(|p| wide(p.as_os_str())).transpose()?;
        let job = create_job()?;
        let mut attributes = Attributes::new()?;
        let job_handle = job.as_raw_handle();
        unsafe {
            attributes.set(
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                self.console as *const _,
                std::mem::size_of::<HPCON>(),
            )?;
            attributes.set(
                PROC_THREAD_ATTRIBUTE_JOB_LIST,
                (&job_handle as *const HANDLE).cast(),
                std::mem::size_of::<HANDLE>(),
            )?;
            let mut startup: STARTUPINFOEXW = std::mem::zeroed();
            startup.StartupInfo.cb = std::mem::size_of_val(&startup) as u32;
            // 必须显式声明“不使用父进程的标准句柄”：父进程的标准输入输出被重定向时
            // （如在 CI 或被其他程序启动），子进程否则会继承它们，从那里读到 EOF 后立即退出，
            // 而不是使用伪终端。
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
            startup.lpAttributeList = attributes.ptr();
            let mut info: PROCESS_INFORMATION = std::mem::zeroed();
            if CreateProcessW(
                exe.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                env.as_ptr().cast(),
                cwd.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                &startup.StartupInfo,
                &mut info,
            ) == 0
            {
                return Err(failed());
            }
            let _thread = OwnedHandle::from_raw_handle(info.hThread);
            Ok(Child {
                process: OwnedHandle::from_raw_handle(info.hProcess),
                job,
            })
        }
    }
}
fn create_job() -> Result<OwnedHandle, AppError> {
    unsafe {
        let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if handle.is_null() {
            return Err(failed());
        }
        let job = OwnedHandle::from_raw_handle(handle);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&info) as u32,
        ) == 0
        {
            return Err(failed());
        }
        Ok(job)
    }
}
struct Attributes(Vec<usize>, bool);
impl Attributes {
    fn new() -> Result<Self, AppError> {
        let mut bytes = 0;
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut bytes);
        }
        let mut result = Self(vec![0; bytes.div_ceil(std::mem::size_of::<usize>())], false);
        if unsafe { InitializeProcThreadAttributeList(result.ptr(), 2, 0, &mut bytes) } == 0 {
            return Err(failed());
        }
        result.1 = true;
        Ok(result)
    }
    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }
    unsafe fn set(
        &mut self,
        key: u32,
        value: *const std::ffi::c_void,
        size: usize,
    ) -> Result<(), AppError> {
        if unsafe {
            UpdateProcThreadAttribute(
                self.ptr(),
                0,
                key as usize,
                value,
                size,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(failed());
        }
        Ok(())
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            if self.1 {
                DeleteProcThreadAttributeList(self.ptr());
            }
        }
    }
}
pub(crate) struct Child {
    process: OwnedHandle,
    job: OwnedHandle,
}
impl Child {
    pub(crate) fn process_id(&self) -> Option<u32> {
        let pid = unsafe {
            windows_sys::Win32::System::Threading::GetProcessId(self.process.as_raw_handle())
        };
        (pid != 0).then_some(pid)
    }
    /// 无参数；终止并等待进程树，成功返回空值，失败或超时返回错误。
    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        self.terminate_and_wait()
    }
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        let Some(status) = self.try_wait_preserving_job()? else {
            return Ok(None);
        };
        // 自然退出后解除关闭作业时杀树的限制，保留用户启动的 GUI 程序。
        let info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe {
            SetInformationJobObject(
                self.job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Some(status))
    }
    /// 无参数；查询直接子进程状态并保留作业对象的杀树限制，未退出时返回 None。
    pub(crate) fn try_wait_preserving_job(
        &mut self,
    ) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(Some(portable_pty::ExitStatus::with_exit_code(code)))
            }
            _ => Err(std::io::Error::last_os_error()),
        }
    }
    /// 无参数；终止整个作业并等待直接子进程和全部后代退出，失败或五秒内未退出返回错误。
    pub(crate) fn terminate_and_wait(&self) -> std::io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        // 必须在终止前保留进程句柄：终止是异步的，作业计数不能代替进程对象有信号。
        let processes = job_process_handles(self.job.as_raw_handle(), deadline);
        // 即使枚举失败也发出终止请求，随后仍返回枚举错误，不能报告清理成功。
        if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        for process in processes? {
            wait_for_process(process.as_raw_handle(), deadline)?;
        }
        // 直接子进程可能自然退出后已不在作业列表中，仍用原始句柄确认退出。
        wait_for_process(self.process.as_raw_handle(), deadline)?;
        loop {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe {
                QueryInformationJobObject(
                    self.job.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if info.ActiveProcesses == 0 {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(tree_timeout());
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }
    /// 等进程结束并取退出码（现在只有测试用：旧的密钥生成流程已改成进程内生成）
    #[cfg(test)]
    pub(crate) fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        if unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut code = 0;
        if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(portable_pty::ExitStatus::with_exit_code(code))
    }
}
fn tree_timeout() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, "等待终端进程树退出超时")
}
fn wait_for_process(process: HANDLE, deadline: Instant) -> std::io::Result<()> {
    // 所有进程共用一个截止时间，不能每个进程各等五秒。
    let millis = deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .try_into()
        .unwrap_or(INFINITE - 1);
    match unsafe { WaitForSingleObject(process, millis) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err(tree_timeout()),
        _ => Err(std::io::Error::last_os_error()),
    }
}
fn job_process_handles(job: HANDLE, deadline: Instant) -> std::io::Result<Vec<OwnedHandle>> {
    // usize 缓冲区保证可变长 ProcessIdList 的对齐，前两个 DWORD 为列表头。
    let header = std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList)
        / std::mem::size_of::<usize>();
    let mut capacity = 1;
    loop {
        if Instant::now() >= deadline {
            return Err(tree_timeout());
        }
        let mut buffer = vec![0usize; header + capacity];
        let bytes = u32::try_from(std::mem::size_of_val(buffer.as_slice()))
            .map_err(|_| std::io::Error::other("终端作业进程列表过大"))?;
        let result = unsafe {
            QueryInformationJobObject(
                job,
                JobObjectBasicProcessIdList,
                buffer.as_mut_ptr().cast(),
                bytes,
                std::ptr::null_mut(),
            )
        };
        let failure = (result == 0).then(std::io::Error::last_os_error);
        let info = unsafe { &*buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
        if let Some(failure) = failure
            && failure.raw_os_error() != Some(ERROR_MORE_DATA as i32)
        {
            return Err(failure);
        }
        if result == 0 || info.NumberOfAssignedProcesses > info.NumberOfProcessIdsInList {
            // 查询期间仍可能派生进程；按新数量扩容并重试，受同一截止时间限制。
            capacity = (info.NumberOfAssignedProcesses as usize).max(capacity * 2);
            continue;
        }
        let mut processes = Vec::new();
        for &pid in &buffer[header..header + info.NumberOfProcessIdsInList as usize] {
            if Instant::now() >= deadline {
                return Err(tree_timeout());
            }
            if let Some(process) = job_process_handle(job, pid as u32)? {
                processes.push(process);
            }
        }
        return Ok(processes);
    }
}
fn job_process_handle(job: HANDLE, pid: u32) -> std::io::Result<Option<OwnedHandle>> {
    let handle = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if handle.is_null() {
        let failure = std::io::Error::last_os_error();
        // 列表快照后已退出并销毁的进程不再有可等待的对象；其他失败不能忽略。
        if failure.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(None);
        }
        return Err(failure);
    }
    let process = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut in_job = 0;
    if unsafe { IsProcessInJob(handle, job, &mut in_job) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // 快照中的 PID 可能已被复用；只等待仍属于本作业的进程对象。
    Ok((in_job != 0).then_some(process))
}
// Windows CRT 参数引用规则；不经过 cmd.exe 或 PowerShell。
fn quote(value: &OsStr) -> Result<Vec<u16>, AppError> {
    let value = wide(value)?;
    if value.len() > 1
        && !value[..value.len() - 1]
            .iter()
            .any(|u| matches!(u, 32 | 9 | 34))
    {
        return Ok(value[..value.len() - 1].to_vec());
    }
    let mut result = vec![34];
    let mut slashes = 0;
    for &unit in &value[..value.len() - 1] {
        if unit == 92 {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            92,
            if unit == 34 { slashes * 2 + 1 } else { slashes },
        ));
        slashes = 0;
        result.push(unit);
    }
    result.extend(std::iter::repeat_n(92, slashes * 2));
    result.push(34);
    Ok(result)
}
fn environment(overrides: &[(String, String)]) -> Result<Vec<u16>, AppError> {
    let mut entries = std::collections::BTreeMap::new();
    for (key, value) in std::env::vars_os() {
        entries.insert(key.to_string_lossy().to_uppercase(), (key, value));
    }
    for (key, value) in overrides {
        if key.is_empty() || key.contains(['=', '\0']) {
            return Err(error("invalid_argument", "环境变量名称无效"));
        }
        entries.insert(key.to_uppercase(), (key.into(), value.into()));
    }
    let mut result = Vec::new();
    for (_, (key, value)) in entries {
        let mut entry = key;
        entry.push("=");
        entry.push(value);
        result.extend(wide(&entry)?);
    }
    result.push(0);
    if result.len() == 1 {
        result.push(0);
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 作业句柄过滤无关及无效进程() {
        let job = create_job().unwrap();
        // 用快照后被复用到当前测试进程的 PID，验证不会等待作业外的存活进程。
        let pid = unsafe { GetCurrentProcessId() };
        assert!(
            job_process_handle(job.as_raw_handle(), pid)
                .unwrap()
                .is_none()
        );
        assert!(
            job_process_handle(job.as_raw_handle(), 0)
                .unwrap()
                .is_none()
        );
        assert!(job_process_handle(INVALID_HANDLE_VALUE, pid).is_err());
    }

    #[test]
    fn 终止失败后查询存活进程不阻塞() {
        const JOB_OBJECT_QUERY: u32 = 0x0004;
        // 空作业只保留查询权限，稳定触发终止失败；直接进程使用当前进程的查询句柄。
        let job = create_job().unwrap();
        let mut restricted = std::ptr::null_mut();
        let current = unsafe { GetCurrentProcess() };
        assert_ne!(
            unsafe {
                DuplicateHandle(
                    current,
                    job.as_raw_handle(),
                    current,
                    &mut restricted,
                    JOB_OBJECT_QUERY,
                    0,
                    0,
                )
            },
            0
        );
        let restricted = unsafe { OwnedHandle::from_raw_handle(restricted) };
        let process = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                GetCurrentProcessId(),
            )
        };
        assert!(!process.is_null());
        let mut child = Child {
            process: unsafe { OwnedHandle::from_raw_handle(process) },
            job: restricted,
        };
        let started = Instant::now();
        assert_eq!(
            child.kill().unwrap_err().raw_os_error(),
            Some(ERROR_ACCESS_DENIED as i32)
        );
        assert!(child.try_wait_preserving_job().unwrap().is_none());
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut process = super::super::child::Process::from_test_child(child);
            process.terminate();
            let status = process.poll();
            process.terminate();
            let _ = tx.send(status);
        });
        // 使用通道时限兜底，避免无限等待的回归直接挂住整个测试进程。
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), Some(None));
        worker.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    // 后代由外层测试的作业对象终止和等待，此辅助进程必须先于后代退出。
    #[allow(clippy::zombie_processes)]
    fn 进程树辅助进程() {
        let Some(mode) = std::env::var_os("PIERLINK_TEST_PROBE_MODE") else {
            return;
        };
        if mode != "child" {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "terminal::windows::tests::进程树辅助进程",
                    "--nocapture",
                ])
                .env("PIERLINK_TEST_PROBE_MODE", "child")
                .spawn()
                .unwrap();
            std::fs::write(
                std::env::var_os("PIERLINK_TEST_PROBE_PID_FILE").unwrap(),
                child.id().to_string(),
            )
            .unwrap();
            if mode == "exited" {
                // 直接进程先退出，后代继续运行，模拟跳板或代理进程。
                return;
            }
            if mode == "overflow" {
                println!("{}", "x".repeat(128 * 1024));
            }
            // 由测试持有的作业对象统一终止和等待；此处不能等待后代，避免掩盖残留。
            drop(child);
        }
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    #[test]
    fn 自然退出和取消均清理后代并等待关闭满输出管道() {
        use std::time::{Duration, Instant};
        let root = crate::test_support::tempdir();
        for mode in ["exited", "waiting", "overflow"] {
            let pair = open(PtySize::default()).unwrap();
            let mut reader =
                crate::process::Reader::start(pair.master.try_clone_reader().unwrap(), 1024)
                    .unwrap();
            let pid_file = root.path().join(format!("{mode}.pid"));
            let mut child = pair
                .master
                .spawn(
                    &std::env::current_exe().unwrap(),
                    &[
                        "--exact".into(),
                        "terminal::windows::tests::进程树辅助进程".into(),
                        "--nocapture".into(),
                    ],
                    None,
                    &[
                        ("PIERLINK_TEST_PROBE_MODE".into(), mode.into()),
                        (
                            "PIERLINK_TEST_PROBE_PID_FILE".into(),
                            pid_file.to_str().unwrap().into(),
                        ),
                    ],
                )
                .unwrap();
            let started = Instant::now();
            let pid: u32 = loop {
                let pid = std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|text| text.parse().ok());
                if let Some(pid) = pid {
                    let ready = match mode {
                        "exited" => child.try_wait_preserving_job().unwrap().is_some(),
                        "overflow" => super::super::lock(&reader.state).exceeded,
                        _ => true,
                    };
                    if ready {
                        break pid;
                    }
                }
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "mode={mode} 启动超时，后代 PID={pid:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            let handle = unsafe {
                OpenProcess(
                    PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    pid,
                )
            };
            assert!(!handle.is_null(), "mode={mode} 无法打开后代 PID={pid}");
            let descendant = unsafe { OwnedHandle::from_raw_handle(handle) };
            let mut in_job = 0;
            assert_ne!(
                unsafe { IsProcessInJob(handle, child.job.as_raw_handle(), &mut in_job) },
                0,
                "mode={mode} 查询后代 PID={pid} 的作业归属失败"
            );
            assert_ne!(in_job, 0, "mode={mode} 后代 PID={pid} 未加入终端作业");
            let handles = job_process_handles(
                child.job.as_raw_handle(),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap_or_else(|e| panic!("mode={mode} 后代 PID={pid} 枚举失败：{e}"));
            assert!(
                handles.len() >= if mode == "exited" { 1 } else { 2 },
                "mode={mode} 后代 PID={pid} 作业列表漏进程"
            );
            drop(handles);
            assert_eq!(
                unsafe { WaitForSingleObject(handle, 0) },
                WAIT_TIMEOUT,
                "mode={mode} 后代 PID={pid} 在终止前已退出"
            );
            child
                .terminate_and_wait()
                .unwrap_or_else(|e| panic!("mode={mode} 后代 PID={pid} 清理失败：{e}"));
            assert_eq!(
                unsafe { WaitForSingleObject(handle, 0) },
                WAIT_OBJECT_0,
                "mode={mode} 后代 PID={pid} 在清理返回后尚未退出"
            );
            assert!(
                child.try_wait_preserving_job().unwrap().is_some(),
                "mode={mode} 后代 PID={pid} 的直接父进程尚未退出"
            );
            child
                .terminate_and_wait()
                .unwrap_or_else(|e| panic!("mode={mode} 后代 PID={pid} 重复清理失败：{e}"));
            pair.master.shutdown_and_wait().unwrap();
            pair.master.shutdown_and_wait().unwrap();
            assert!(
                super::super::lock(&pair.master.close_worker).is_none(),
                "mode={mode} 后代 PID={pid} 的 ConPTY 关闭线程未结束"
            );
            reader.join();
            drop(descendant);
        }
    }

    #[test]
    fn 空作业重复枚举及等待超时和句柄错误() {
        let job = create_job().unwrap();
        for _ in 0..2 {
            assert!(
                job_process_handles(job.as_raw_handle(), Instant::now() + Duration::from_secs(1))
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(
            job_process_handles(job.as_raw_handle(), Instant::now())
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::TimedOut
        );
        assert!(
            job_process_handles(
                INVALID_HANDLE_VALUE,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        let started = Instant::now();
        let deadline = started + Duration::from_millis(20);
        for _ in 0..2 {
            assert_eq!(
                wait_for_process(unsafe { GetCurrentProcess() }, deadline)
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::TimedOut
            );
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(wait_for_process(std::ptr::null_mut(), deadline).is_err());
    }

    #[test]
    fn 交互终端关闭同步等待后代() {
        use super::super::{TerminalEvent, TerminalKind, TerminalManager, TerminalSpec};
        use std::sync::{Arc, mpsc};

        let root = crate::test_support::tempdir();
        for mode in ["waiting", "overflow"] {
            let manager = TerminalManager::new();
            let pid_file = root.path().join(format!("terminal-{mode}.pid"));
            let (tx, rx) = mpsc::channel();
            let id = manager
                .open(
                    TerminalSpec {
                        kind: TerminalKind::Local {
                            program: std::env::current_exe().unwrap(),
                            args: vec![
                                "--exact".into(),
                                "terminal::windows::tests::进程树辅助进程".into(),
                                "--nocapture".into(),
                            ],
                            cwd: None,
                            env: vec![
                                ("PIERLINK_TEST_PROBE_MODE".into(), mode.into()),
                                (
                                    "PIERLINK_TEST_PROBE_PID_FILE".into(),
                                    pid_file.to_str().unwrap().into(),
                                ),
                            ],
                        },
                        cols: 80,
                        rows: 24,
                    },
                    Arc::new(move |event| {
                        let _ = tx.send(event);
                    }),
                )
                .unwrap();
            let started = Instant::now();
            let pid: u32 = loop {
                let pid = std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|text| text.parse().ok());
                if let Some(pid) = pid {
                    break pid;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "mode={mode} 启动超时，后代 PID={pid:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
            assert!(!handle.is_null(), "mode={mode} 无法打开后代 PID={pid}");
            let _descendant = unsafe { OwnedHandle::from_raw_handle(handle) };
            manager.close(&id).unwrap();
            assert_eq!(
                unsafe { WaitForSingleObject(handle, 0) },
                WAIT_OBJECT_0,
                "mode={mode} 后代 PID={pid} 在交互终端关闭返回后尚未退出"
            );
            manager.close(&id).unwrap();
            loop {
                match rx
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap_or_else(|e| panic!("mode={mode} 后代 PID={pid} 缺少退出事件：{e}"))
                {
                    TerminalEvent::Exited { closed_by_user, .. } => {
                        assert!(closed_by_user, "mode={mode} 后代 PID={pid} 退出原因错误");
                        break;
                    }
                    TerminalEvent::Output(_) => (),
                }
            }
        }
    }

    #[test]
    fn 启动失败也等待伪终端关闭() {
        let root = crate::test_support::tempdir();
        let pair = open(PtySize::default()).unwrap();
        assert!(
            pair.master
                .spawn_clean(&root.path().join("不存在的程序.exe"), &[], &[])
                .is_err()
        );
        pair.master.shutdown_and_wait().unwrap();
        assert!(super::super::lock(&pair.master.close_worker).is_none());
    }

    #[test]
    fn 参数引用和环境校验() {
        assert_eq!(
            String::from_utf16(&quote(OsStr::new("a b\\")).unwrap()).unwrap(),
            "\"a b\\\\\""
        );
        assert_eq!(
            String::from_utf16(&quote(OsStr::new("a\"b")).unwrap()).unwrap(),
            "\"a\\\"b\""
        );
        for (arg, expected) in [
            ("/Q", "/Q"),
            ("/C", "/C"),
            ("", "\"\""),
            ("a\tb", "\"a\tb\""),
            ("C:\\bin\\", "C:\\bin\\"),
        ] {
            assert_eq!(
                String::from_utf16(&quote(OsStr::new(arg)).unwrap()).unwrap(),
                expected
            );
        }
        assert!(quote(OsStr::new("\0")).is_err());
        assert!(environment(&[("坏=名称".into(), "值".into())]).is_err());
    }
}
