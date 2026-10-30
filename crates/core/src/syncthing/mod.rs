//! 单机 Syncthing 实例：启动前加固、受限管理接口与文件夹策略。
pub mod client;
pub mod folder;
pub mod gui_session;
pub mod home;
pub mod process;
#[cfg(test)]
mod tests;
mod xml;

use crate::error::AppError;
fn error(text_key: &'static str, reason: &str) -> AppError {
    AppError::new("syncthing.invalid_config").with_reason(text_key, reason)
}
/// Syncthing 报告的监听错误是不是“端口不能用”（需要换一个端口）。
/// Linux：address already in use；Windows：WSAEADDRINUSE 的“Only one usage of each socket address”，
/// 以及端口落在 Hyper-V / WSL 保留范围里时的 WSAEACCES“forbidden by its access permissions”。
fn port_in_use(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("address already in use")
        || t.contains("only one usage of each socket address")
        || (t.contains("bind") && t.contains("forbidden by its access permissions"))
}
#[cfg(test)]
#[test]
fn 端口占用的三种报法() {
    assert!(port_in_use(
        "listen tcp 127.0.0.1:1: bind: address already in use"
    ));
    assert!(port_in_use(
        "listen tcp 127.0.0.1:65471: bind: Only one usage of each socket address (protocol/network address/port) is normally permitted."
    ));
    assert!(port_in_use(
        "listen tcp 0.0.0.0:22000: bind: An attempt was made to access a socket in a way forbidden by its access permissions."
    ));
    assert!(!port_in_use("open database: permission denied"));
}
fn io_error(text_key: &'static str, operation: &str) -> AppError {
    AppError::new("syncthing.io_failed").with_text("operation", text_key, operation)
}
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
