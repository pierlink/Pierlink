//! MCP 服务：让 AI agent 通过 Pierlink 操作本机 Windows 和各台服务器的命令、终端、文件和同步。
//! 协议用官方 SDK rmcp（同时支持旧式 `initialize` 握手和 2026-07-28 的无握手协议）；
//! 两个入口：Windows 本机（命名管道上的字节流）、服务器（远程转发到本机端口的 Streamable HTTP）。
pub mod audit;
mod host;
pub mod http;
pub mod pipe;
pub mod safety;
mod service;
mod session;
pub mod text;
pub mod token;
pub mod tools;

pub use host::*;
pub use http::HttpEntry;
pub use service::McpService;
pub use session::SessionHandle;

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests;
