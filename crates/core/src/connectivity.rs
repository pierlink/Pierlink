//! 系统网络等级通知；用户回调在独立线程执行，系统回调仅投递最新等级。
use crate::error::AppError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, specta::Type)]
pub enum Connectivity {
    None,
    LocalOnly,
    Internet,
    Unknown,
}

/// 无参数；返回当前系统网络等级，查询失败及非 Windows 平台返回 Unknown。
pub fn current_connectivity() -> Connectivity {
    #[cfg(windows)]
    return windows::current();
    #[cfg(not(windows))]
    Connectivity::Unknown
}

/// 网络订阅句柄；释放时取消系统通知并停止后台线程。
pub struct ConnectivityWatch {
    #[cfg(windows)]
    inner: windows::Watch,
}

/// 订阅等级变化，稳定一秒后在后台线程调用 f；返回释放即取消的句柄，非 Windows 返回空句柄。
pub fn watch_connectivity(
    f: impl Fn(Connectivity) + Send + Sync + 'static,
) -> Result<ConnectivityWatch, AppError> {
    #[cfg(windows)]
    return windows::watch(f).map(|inner| ConnectivityWatch { inner });
    #[cfg(not(windows))]
    {
        let _ = f;
        Ok(ConnectivityWatch {})
    }
}

impl Drop for ConnectivityWatch {
    fn drop(&mut self) {
        #[cfg(windows)]
        self.inner.stop();
    }
}

#[cfg(any(windows, test))]
fn level(hint: i32) -> Connectivity {
    match hint {
        1 => Connectivity::None,
        // 受限互联网（例如认证门户）还不能用来恢复服务器连接。
        2 | 4 => Connectivity::LocalOnly,
        3 => Connectivity::Internet,
        _ => Connectivity::Unknown,
    }
}

#[cfg(any(windows, test))]
struct Debounce {
    reported: Connectivity,
    pending: Connectivity,
    since: std::time::Instant,
}
#[cfg(any(windows, test))]
impl Debounce {
    fn new(initial: Connectivity, now: std::time::Instant) -> Self {
        Self {
            reported: initial,
            pending: initial,
            since: now,
        }
    }
    fn observe(&mut self, value: Connectivity, now: std::time::Instant) {
        if self.pending != value {
            self.pending = value;
            self.since = now;
        }
    }
    fn poll(&mut self, now: std::time::Instant) -> Option<Connectivity> {
        if self.pending != self.reported
            && now.duration_since(self.since) >= std::time::Duration::from_secs(1)
        {
            self.reported = self.pending;
            Some(self.reported)
        } else {
            None
        }
    }
}

#[cfg(windows)]
#[path = "connectivity/windows.rs"]
mod windows;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    #[test]
    fn n1_n5_等级映射包括受限网络和未知值() {
        for (input, expected) in [
            (0, Connectivity::Unknown),
            (1, Connectivity::None),
            (2, Connectivity::LocalOnly),
            (3, Connectivity::Internet),
            (4, Connectivity::LocalOnly),
            (5, Connectivity::Unknown),
            (99, Connectivity::Unknown),
        ] {
            assert_eq!(level(input), expected);
        }
    }
    #[test]
    fn n2_抖动合并重复等级不延迟且恢复原等级不通知() {
        let start = Instant::now();
        let mut d = Debounce::new(Connectivity::Internet, start);
        d.observe(Connectivity::None, start);
        d.observe(Connectivity::LocalOnly, start + Duration::from_millis(300));
        assert_eq!(d.poll(start + Duration::from_secs(1)), None);
        d.observe(Connectivity::LocalOnly, start + Duration::from_millis(1200));
        assert_eq!(
            d.poll(start + Duration::from_millis(1300)),
            Some(Connectivity::LocalOnly)
        );
        assert_eq!(d.poll(start + Duration::from_secs(3)), None);
        d.observe(Connectivity::None, start + Duration::from_secs(4));
        d.observe(Connectivity::LocalOnly, start + Duration::from_millis(4500));
        assert_eq!(d.poll(start + Duration::from_secs(6)), None);
    }
    #[test]
    fn 查询注册与释放() {
        #[cfg(not(windows))]
        assert_eq!(current_connectivity(), Connectivity::Unknown);
        #[cfg(windows)]
        assert!(matches!(
            current_connectivity(),
            Connectivity::Internet | Connectivity::LocalOnly
        ));
        for _ in 0..3 {
            drop(watch_connectivity(|_| {}).unwrap());
        }
    }
}
