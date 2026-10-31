//! 查询 Windows IPv4 监听端口的占用进程；无权限时仍返回端口错误。
use crate::error::AppError;
use windows_sys::Win32::{
    Foundation::CloseHandle,
    NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
    },
    System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    },
};
pub(super) fn enrich(error: AppError, port: u16) -> AppError {
    match lookup(port) {
        Some((pid, name)) => error
            .with("pid", pid)
            .with("process", crate::security::redact(&name, &[])),
        None => error,
    }
}
fn lookup(port: u16) -> Option<(u32, String)> {
    unsafe {
        let mut size = 0;
        GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            2,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        );
        // 使用 u32 数组保证与系统 TCP 表相同的对齐，并限制分配上限。
        if !(4..=16 * 1024 * 1024).contains(&size) {
            return None;
        }
        let mut table = vec![0u32; (size as usize).div_ceil(4)];
        if GetExtendedTcpTable(
            table.as_mut_ptr().cast(),
            &mut size,
            0,
            2,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        ) != 0
        {
            return None;
        }
        let count = table[0] as usize;
        let rows = (size as usize).saturating_sub(4) / std::mem::size_of::<MIB_TCPROW_OWNER_PID>();
        if count > rows {
            return None;
        }
        let ptr = table.as_ptr().add(1).cast::<MIB_TCPROW_OWNER_PID>();
        for index in 0..count {
            let row = &*ptr.add(index);
            if u16::from_be(row.dwLocalPort as u16) != port {
                continue;
            }
            let pid = row.dwOwningPid;
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return Some((pid, String::new()));
            }
            let mut name = vec![0u16; 32768];
            let mut len = name.len() as u32;
            let ok = QueryFullProcessImageNameW(process, 0, name.as_mut_ptr(), &mut len);
            CloseHandle(process);
            let name = if ok != 0 {
                String::from_utf16_lossy(&name[..len as usize])
            } else {
                String::new()
            };
            let name = name.rsplit('\\').next().unwrap_or("").to_owned();
            return Some((pid, name));
        }
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn p2_查询当前进程监听端口() {
        let socket = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let (pid, name) = lookup(socket.local_addr().unwrap().port()).unwrap();
        assert_eq!(pid, std::process::id());
        assert!(!name.is_empty());
    }
}
