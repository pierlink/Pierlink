//! 一次拖出的请求、进度和结果：在后台线程上准备好，交给主线程上的系统拖放（`win.rs`）。和平台无关。
// 有些字段和方法只有 Windows 的拖放用到
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// 进度框里的文字（随界面语言）
pub struct Labels {
    /// 标题栏
    pub title: String,
    /// 第一行，如“正在下载 report.pdf”
    pub line: String,
    /// 点了取消之后显示的文字
    pub cancelling: String,
}

/// 下载进度：工作线程写，主线程读出来更新进度框
pub struct Progress {
    cancel: Arc<AtomicBool>,
    done: AtomicU64,
    total: AtomicU64,
    current: Mutex<String>,
}

impl Progress {
    /// :param cancel: 取消标记（和传输列表里的“取消”共用一个）
    pub fn new(cancel: Arc<AtomicBool>) -> Self {
        Self {
            cancel,
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            current: Mutex::new(String::new()),
        }
    }

    /// 记下进度（工作线程调用）。
    ///
    /// :param done: 已下载的字节数
    /// :param total: 总字节数（还在统计时是 0）
    /// :param current: 正在下载的文件（相对路径）
    pub fn set(&self, done: u64, total: u64, current: &str) {
        self.done.store(done, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
        let mut shown = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if *shown != current {
            shown.clear();
            shown.push_str(current);
        }
    }

    /// 取消标记，交给下载函数检查
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }

    /// 取消下载（进度框里点了取消、应用要退出）
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// 是否已经取消（包括从传输列表取消的）
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 当前进度：(已下载字节, 总字节, 正在下载的文件)
    pub fn snapshot(&self) -> (u64, u64, String) {
        let current = self
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
            current,
        )
    }
}

/// 拖放的结果
#[derive(Debug)]
pub enum Outcome {
    /// 已下载并交给了放下的程序
    Dropped,
    /// 没有放下：按了 Esc、放在不接收文件的地方或本程序的其他窗口上
    Cancelled,
    /// 拖回了开始拖动的窗口：松手处相对它客户区的物理像素
    Returned { x: i32, y: i32 },
    /// 下载没成功：出错或在进度框里取消了（原因由下载函数自己记下）
    FetchFailed,
    /// 拖放本身出错；downloaded 表示出错前是否已经下载好（这时对方可能正在读临时文件，不要马上删）
    Failed { reason: Failure, downloaded: bool },
}

/// 拖放本身出错的原因（界面按当前语言显示：errors.detail 下的键，见 drag_out.rs 的 failed）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// 设置开始拖放的定时器失败
    Timer,
    /// 已有一个拖出正在进行
    Busy,
    /// 拖放代码里出了没料到的错误
    Crashed,
    /// OLE 初始化失败（附系统给的错误）
    Ole(String),
    /// 下载好了，放下的程序最后没有接收
    Rejected,
    /// 系统拖放返回了意外的结果（附返回值）
    Unexpected(String),
    /// 只支持 Windows（只在其他平台上用到）
    #[cfg_attr(windows, allow(dead_code))]
    Unsupported,
    /// 拖放没有完成（比如程序正在退出）
    Unfinished,
}

impl Failure {
    /// 附带的系统信息（错误、返回值）；没有时为 None
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Ole(s) | Self::Unexpected(s) => Some(s),
            _ => None,
        }
    }
}

/// 松手放到别的程序上时在工作线程里调用：下载，返回实际存在、可以交给对方的路径；失败或取消时返回 None
pub type Fetch = Box<dyn FnOnce(&Progress) -> Option<Vec<PathBuf>> + Send>;
/// 拖放结束后调用（在主线程上）
pub type Done = Box<dyn FnOnce(Outcome) + Send>;

/// 一次拖出
pub struct Request {
    /// CF_HDROP 里的路径：这时还不存在，松手下载后才有
    pub paths: Vec<PathBuf>,
    pub labels: Labels,
    pub progress: Arc<Progress>,
    pub fetch: Fetch,
    pub done: Done,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 进度记下最新的字节数和当前文件() {
        let progress = Progress::new(Arc::new(AtomicBool::new(false)));
        assert_eq!(progress.snapshot(), (0, 0, String::new()));
        progress.set(10, 100, "logs/a.log");
        progress.set(40, 100, "logs/b.log");
        assert_eq!(progress.snapshot(), (40, 100, "logs/b.log".to_owned()));
    }

    #[test]
    fn 取消和传输列表共用一个标记() {
        let flag = Arc::new(AtomicBool::new(false));
        let progress = Progress::new(Arc::clone(&flag));
        assert!(!progress.cancelled());
        // 从传输列表取消
        flag.store(true, Ordering::Relaxed);
        assert!(progress.cancelled());
        assert!(progress.cancel_flag().load(Ordering::Relaxed));
        // 从进度框取消
        let flag = Arc::new(AtomicBool::new(false));
        let progress = Progress::new(Arc::clone(&flag));
        progress.cancel();
        assert!(flag.load(Ordering::Relaxed));
    }
}
