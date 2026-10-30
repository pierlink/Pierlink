//! Windows 作业对象；句柄不继承，关闭时杀死全部成员。
use super::io_error;
use crate::error::AppError;
use std::os::windows::io::RawHandle;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::JobObjects::*,
};
pub(crate) struct Job(HANDLE);
// 作业句柄由 Job 独占；内核允许跨线程使用。
unsafe impl Send for Job {}
impl Job {
    pub fn attach(process: RawHandle) -> Result<Self, AppError> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(io_error("process.create_job_object", "创建作业对象"));
            }
            let job = Self(handle);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as _,
                std::mem::size_of_val(&info) as u32,
            ) == 0
                || AssignProcessToJobObject(handle, process) == 0
            {
                return Err(io_error("process.join_job_object", "加入作业对象"));
            }
            Ok(job)
        }
    }
    pub fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.0, 1);
        }
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtResumeProcess(process: HANDLE) -> i32;
}
pub(crate) fn resume(process: RawHandle) -> Result<(), AppError> {
    if unsafe { NtResumeProcess(process) } < 0 {
        Err(io_error("process.resume_process", "恢复进程"))
    } else {
        Ok(())
    }
}
