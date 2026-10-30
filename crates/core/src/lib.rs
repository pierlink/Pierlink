//! Pierlink 核心逻辑。
//!
//! 这里只放与平台无关、可在 Linux 上编译和测试的代码。
//! 各模块（ssh、tunnel、remote、sftp、syncthing、detect、storage、security）逐步加入，

pub mod app_data;
pub mod builtin;
pub mod clipboard;
pub mod connectivity;
pub mod cwd;
pub mod deploy;
pub mod detect;
pub mod error;
pub mod fs_local;
pub mod info;
pub mod keys;
pub mod links;
pub mod mcp;
pub mod mobile;
pub mod monitor;
pub mod process;
pub mod profile;
pub mod remote;
pub mod security;
pub mod server_input;
pub mod sftp;
pub mod ssh;
pub mod storage;
pub mod sync;
pub mod syncthing;
pub mod terminal;
pub mod tunnel;
pub mod vnc;

#[cfg(test)]
mod error_text_tests;

#[cfg(test)]
mod test_support;

#[cfg(all(test, unix))]
#[path = "test_support/builtin.rs"]
mod builtin_test_support;
