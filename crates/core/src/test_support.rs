//! 测试临时目录集中在数据盘同一个根目录，正常退出时整体自动清理。
use std::sync::{Mutex, OnceLock};
use tempfile::TempDir;

static ROOT: OnceLock<Mutex<Option<TempDir>>> = OnceLock::new();
unsafe extern "C" {
    fn atexit(callback: extern "C" fn()) -> i32;
}
extern "C" fn cleanup() {
    if let Some(root) = ROOT.get()
        && let Ok(mut root) = root.lock()
    {
        drop(root.take());
    }
}

/// 无参数；返回共用根目录下自动删除的子目录，根目录在测试进程退出时删除。
pub fn tempdir() -> TempDir {
    let root = ROOT.get_or_init(|| {
        // 设了 PIERLINK_TEST_TMP（如放到数据盘）就用它；否则用工作区根目录，避免 crates/core 使回退套接字超长。
        let configured = std::env::var_os("PIERLINK_TEST_TMP").map(std::path::PathBuf::from);
        let data_root = match configured.as_deref().filter(|p| p.is_dir()) {
            Some(dir) => dir,
            None => std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap(),
        };
        let root = tempfile::Builder::new()
            .prefix("pl-")
            .tempdir_in(data_root)
            .unwrap();
        assert_eq!(unsafe { atexit(cleanup) }, 0);
        Mutex::new(Some(root))
    });
    let root = root.lock().unwrap();
    tempfile::Builder::new()
        .prefix("t")
        .tempdir_in(root.as_ref().unwrap().path())
        .unwrap()
}
