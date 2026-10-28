//! 页面代数：每个页面（webview 标签）每开始加载一次就加一。
//!
//! 打开终端、指纹探测这类耗时命令开始时记下代数，完成时若代数已变（页面在此期间重载或关闭），
//! 说明结果已经没有页面接收，应当立即释放资源，而不是留下没人管的会话。

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct PageGenerations {
    map: Mutex<HashMap<String, u64>>,
}

impl PageGenerations {
    /// 当前代数（从未加载过的页面为 0）
    pub fn current(&self, label: &str) -> u64 {
        let map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        map.get(label).copied().unwrap_or(0)
    }

    /// 页面开始加载或被销毁时调用，使之前发起的命令结果作废
    pub fn bump(&self, label: &str) {
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        *map.entry(label.to_owned()).or_insert(0) += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 代数按页面独立递增() {
        let pages = PageGenerations::default();
        assert_eq!(pages.current("main"), 0);
        pages.bump("main");
        pages.bump("main");
        pages.bump("other");
        assert_eq!(pages.current("main"), 2);
        assert_eq!(pages.current("other"), 1);
    }
}
