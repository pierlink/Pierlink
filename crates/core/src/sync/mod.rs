//! 同步编排；凭据和记录由调用方提供，本模块不写数据库。
mod address;
mod link;
mod paths;
mod reconnect;
mod settings;
mod tasks;
pub use settings::*;
#[cfg(all(test, unix))]
mod builtin_tests;
#[cfg(test)]
mod tests;
use crate::error::AppError;
pub use link::{LocalSync, ServerEndpointInfo, ServerLink};
pub use paths::*;
pub use tasks::*;
fn error(text_key: &'static str, reason: &str) -> AppError {
    AppError::new("sync.invalid_input").with_reason(text_key, reason)
}
fn invalid_response() -> AppError {
    AppError::new("sync.invalid_response")
}

mod conflicts;
mod management;
pub use management::*;
mod estimate;
mod estimate_files;
pub use estimate_files::EstimateFileAccess;
mod events;
mod time;
mod versions;
pub use conflicts::*;
pub use estimate::*;
pub use events::*;
pub use versions::*;

mod markers;

mod trash;
pub use trash::*;
