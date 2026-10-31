//! 桌面发现、专用桌面生命周期、服务端程序获取及 SSH 字节流。
mod acquire;
mod desktop;
mod detection;
mod model;
mod parse;
mod relay;
mod rfb;
mod stream;
mod transport;

pub use acquire::{acquire_light_desktop, acquire_xvnc};
pub use desktop::{cleanup_stale, create_desktop, stop_desktop};
pub use detection::{detect, install_command};
pub use model::*;
pub use rfb::probe_rfb;
pub use stream::{VncCloser, VncReader, VncStream, VncWriter};
pub use transport::open_stream;

#[cfg(all(test, unix))]
mod apt_tests;
#[cfg(all(test, unix))]
mod desktop_tests;
#[cfg(all(test, unix))]
mod real_tests;
#[cfg(test)]
mod tests;
#[cfg(all(test, unix))]
mod xkb_tests;
