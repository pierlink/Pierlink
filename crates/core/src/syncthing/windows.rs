//! Windows 接管：快照枚举、完整程序路径及命令行核验，清理时逐个终止整组进程。
use super::io_error;
use crate::error::AppError;
use std::path::Path;
use windows_sys::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_TIMEOUT},
    System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    },
};
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
pub(super) fn identity(pid: u32, bin: &Path, home: &Path, gui: &str) -> Option<String> {
    unsafe {
        let process = Handle(OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | 0x00100000,
            0,
            pid,
        ));
        if process.0.is_null() || WaitForSingleObject(process.0, 0) != WAIT_TIMEOUT {
            return None;
        }
        let mut name = vec![0u16; 32768];
        let mut len = name.len() as u32;
        if QueryFullProcessImageNameW(process.0, 0, name.as_mut_ptr(), &mut len) == 0 {
            return None;
        }
        let name = std::fs::canonicalize(String::from_utf16_lossy(&name[..len as usize])).ok()?;
        let bin = std::fs::canonicalize(bin).ok()?;
        if name.to_string_lossy().to_lowercase() != bin.to_string_lossy().to_lowercase() {
            return None;
        }
        let args = command_line(process.0)?;
        if ![
            "serve".to_owned(),
            "--no-restart".to_owned(),
            format!("--home={}", home.to_str()?),
            format!("--gui-address={gui}"),
        ]
        .iter()
        .all(|arg| args.contains(arg))
        {
            return None;
        }
        let mut created: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        if GetProcessTimes(process.0, &mut created, &mut exit, &mut kernel, &mut user) == 0 {
            return None;
        }
        Some(format!(
            "{}:{}",
            created.dwHighDateTime, created.dwLowDateTime
        ))
    }
}

// ProcessCommandLineInformation 返回本进程缓冲区中的 UNICODE_STRING，无需读取远程 PEB。
#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationProcess(
        process: HANDLE,
        class: u32,
        buffer: *mut core::ffi::c_void,
        size: u32,
        returned: *mut u32,
    ) -> i32;
}
fn command_line(process: HANDLE) -> Option<Vec<String>> {
    unsafe {
        let mut size = 0;
        NtQueryInformationProcess(process, 60, std::ptr::null_mut(), 0, &mut size);
        if size < std::mem::size_of::<UnicodeString>() as u32 || size > 128 * 1024 {
            return None;
        }
        let mut storage = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        if NtQueryInformationProcess(process, 60, storage.as_mut_ptr().cast(), size, &mut size) < 0
        {
            return None;
        }
        let value = &*storage.as_ptr().cast::<UnicodeString>();
        let base = storage.as_ptr() as usize;
        let end = base + storage.len() * std::mem::size_of::<usize>();
        let start = value.buffer as usize;
        if value.length % 2 != 0 || start < base || start.checked_add(value.length as usize)? > end
        {
            return None;
        }
        let mut text = std::slice::from_raw_parts(value.buffer, value.length as usize / 2).to_vec();
        text.push(0);
        let mut count = 0;
        let argv = windows_sys::Win32::UI::Shell::CommandLineToArgvW(text.as_ptr(), &mut count);
        if argv.is_null() {
            return None;
        }
        let mut out = Vec::new();
        for ptr in std::slice::from_raw_parts(argv, count as usize) {
            let mut length = 0;
            while *ptr.add(length) != 0 {
                length += 1;
            }
            out.push(String::from_utf16_lossy(std::slice::from_raw_parts(
                *ptr, length,
            )));
        }
        windows_sys::Win32::Foundation::LocalFree(argv.cast());
        Some(out)
    }
}
pub(super) fn pids() -> Result<Vec<u32>, AppError> {
    use windows_sys::Win32::{Foundation::INVALID_HANDLE_VALUE, System::Diagnostics::ToolHelp::*};
    unsafe {
        let snapshot = Handle(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0));
        if snapshot.0 == INVALID_HANDLE_VALUE {
            return Err(io_error("syncthing.enumerate_processes", "枚举进程"));
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        let mut out = Vec::new();
        if Process32FirstW(snapshot.0, &mut entry) == 0 {
            return Err(io_error("syncthing.read_process_snapshot", "读取进程快照"));
        }
        loop {
            out.push(entry.th32ProcessID);
            if Process32NextW(snapshot.0, &mut entry) == 0 {
                break;
            }
        }
        Ok(out)
    }
}
pub(super) fn terminate(pid: u32) -> Result<(), AppError> {
    unsafe {
        let process = Handle(OpenProcess(PROCESS_TERMINATE | 0x00100000, 0, pid));
        if process.0.is_null() {
            return Err(io_error("syncthing.open_adopted_process", "打开接管进程"));
        }
        if TerminateProcess(process.0, 1) == 0 && WaitForSingleObject(process.0, 0) == WAIT_TIMEOUT
        {
            return Err(io_error(
                "syncthing.terminate_adopted_process",
                "终止接管进程",
            ));
        }
        if WaitForSingleObject(process.0, 5000) == WAIT_TIMEOUT {
            return Err(io_error(
                "syncthing.wait_for_adopted_process_to_exit",
                "等待接管进程退出",
            ));
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 当前进程命令行可读且不误识别() {
        let pid = std::process::id();
        assert!(pids().unwrap().contains(&pid));
        let process = Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) });
        assert!(!command_line(process.0).unwrap().is_empty());
        assert!(
            identity(
                pid,
                &std::env::current_exe().unwrap(),
                Path::new(r"C:\test"),
                "127.0.0.1:1"
            )
            .is_none()
        );
    }
}
