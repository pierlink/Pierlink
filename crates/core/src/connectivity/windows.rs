//! Win10 2004 起的网络提示 API；取消完成前始终保留系统回调上下文。
use super::{Connectivity, Debounce, level};
use crate::error::AppError;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::HANDLE,
    NetworkManagement::IpHelper::{
        CancelMibChangeNotify2, GetNetworkConnectivityHint, NotifyNetworkConnectivityHintChange,
    },
    Networking::WinSock::NL_NETWORK_CONNECTIVITY_HINT,
};

pub(super) fn current() -> Connectivity {
    let mut hint = NL_NETWORK_CONNECTIVITY_HINT::default();
    if unsafe { GetNetworkConnectivityHint(&mut hint) } == 0 {
        level(hint.ConnectivityLevel)
    } else {
        Connectivity::Unknown
    }
}
struct Signal {
    hint: AtomicI32,
    stopped: AtomicBool,
    wake: SyncSender<()>,
}
unsafe extern "system" fn changed(
    context: *const core::ffi::c_void,
    hint: NL_NETWORK_CONNECTIVITY_HINT,
) {
    // Notify 注册到取消之间，Box 的地址和内容都保持有效。
    let signal = unsafe { &*context.cast::<Arc<Signal>>() };
    signal.hint.store(hint.ConnectivityLevel, Ordering::Release);
    let _ = signal.wake.try_send(());
}
pub(super) struct Watch {
    handle: usize,
    // 回调拿到的是 Box 里那个 Arc 的地址：注册期间地址必须固定，取消失败时还要能故意泄漏它，
    // 所以这里的 Box 不是多余的分配
    #[allow(clippy::redundant_allocation)]
    context: Option<Box<Arc<Signal>>>,
    worker: Option<JoinHandle<()>>,
}
impl Watch {
    pub(super) fn stop(&mut self) {
        if let Some(context) = self.context.take() {
            context.stopped.store(true, Ordering::Release);
            let _ = context.wake.try_send(());
            let status = unsafe { CancelMibChangeNotify2(self.handle as HANDLE) };
            if status != 0 {
                // 系统拒绝取消时不能释放仍可能被访问的地址；保留该上下文避免悬空指针。
                let _ = Box::into_raw(context);
            }
        }
        if let Some(worker) = self.worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.stop();
    }
}
pub(super) fn watch(f: impl Fn(Connectivity) + Send + Sync + 'static) -> Result<Watch, AppError> {
    let initial = current();
    let (wake, receiver) = mpsc::sync_channel(1);
    let signal = Arc::new(Signal {
        hint: AtomicI32::new(match initial {
            Connectivity::None => 1,
            Connectivity::LocalOnly => 2,
            Connectivity::Internet => 3,
            Connectivity::Unknown => 0,
        }),
        stopped: AtomicBool::new(false),
        wake,
    });
    let context = Box::new(signal.clone());
    let mut handle = std::ptr::null_mut();
    let status = unsafe {
        NotifyNetworkConnectivityHintChange(
            Some(changed),
            (&*context as *const Arc<Signal>).cast(),
            true,
            &mut handle,
        )
    };
    if status != 0 {
        return Err(AppError::new("network.watch_failed")
            .with("reason", "注册网络变化通知失败")
            .with("os_code", status));
    }
    let mut watch = Watch {
        handle: handle as usize,
        context: Some(context),
        worker: None,
    };
    watch.worker = Some(
        thread::Builder::new()
            .name("pierlink-connectivity".into())
            .spawn(move || {
                let mut debounce = Debounce::new(initial, Instant::now());
                loop {
                    let _ = receiver.recv_timeout(Duration::from_millis(50));
                    if signal.stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let now = Instant::now();
                    debounce.observe(level(signal.hint.load(Ordering::Acquire)), now);
                    if let Some(value) = debounce.poll(now) {
                        f(value);
                    }
                }
            })
            .map_err(|_| {
                AppError::new("network.watch_failed").with("reason", "启动网络通知线程失败")
            })?,
    );
    Ok(watch)
}
