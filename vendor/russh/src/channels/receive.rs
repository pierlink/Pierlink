// Pierlink 修改：按窗口约束缓存，消费累计达到半窗或最大包长才唤醒补窗，并累计越窗字节。
use super::ChannelMsg;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll};
use tokio::sync::{Notify, mpsc};

#[derive(Debug)]
pub(crate) enum ChannelReceiver {
    Bounded(mpsc::Receiver<ChannelMsg>),
    Windowed(mpsc::UnboundedReceiver<ChannelMsg>),
}

impl From<mpsc::Receiver<ChannelMsg>> for ChannelReceiver {
    fn from(receiver: mpsc::Receiver<ChannelMsg>) -> Self {
        Self::Bounded(receiver)
    }
}

impl ChannelReceiver {
    pub(crate) fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<ChannelMsg>> {
        match self {
            Self::Bounded(rx) => rx.poll_recv(cx),
            Self::Windowed(rx) => rx.poll_recv(cx),
        }
    }
    pub(crate) async fn recv(&mut self) -> Option<ChannelMsg> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }
    pub(crate) fn try_recv(&mut self) -> Result<ChannelMsg, mpsc::error::TryRecvError> {
        match self {
            Self::Bounded(rx) => rx.try_recv(),
            Self::Windowed(rx) => rx.try_recv(),
        }
    }
    pub(crate) fn close(&mut self) {
        match self {
            Self::Bounded(rx) => rx.close(),
            Self::Windowed(rx) => rx.close(),
        }
    }
}

#[derive(Debug)]
struct Consumption {
    buffered: AtomicUsize,
    consumed: AtomicUsize,
    threshold: AtomicUsize,
    exceeded: AtomicUsize,
    notify: Arc<Notify>,
}

/// 延迟消费凭据；把数据交给额外缓冲的调用方应在最终读取后归还额度。
#[derive(Clone, Debug, Default)]
pub struct ChannelConsumption(Option<Arc<Consumption>>);

impl ChannelConsumption {
    pub(crate) fn new(notify: Arc<Notify>) -> Self {
        Self(Some(Arc::new(Consumption {
            buffered: AtomicUsize::new(0),
            consumed: AtomicUsize::new(0),
            threshold: AtomicUsize::new(usize::MAX),
            exceeded: AtomicUsize::new(0),
            notify,
        })))
    }
    /// 按本端初始窗口和最大包长设置消费阈值；返回值为空，须在投递数据前调用。
    pub(crate) fn configure(&self, window: u32, max_packet: u32) {
        if let Some(inner) = &self.0 {
            inner
                .threshold
                .store((window / 2).max(max_packet) as usize, Ordering::Release);
        }
    }
    /// 累加本包超过剩余窗口的字节数，返回本通道累计越窗字节；服务端默认凭据返回零。
    pub(crate) fn record_exceeded(&self, bytes: usize) -> usize {
        self.0.as_ref().map_or(0, |inner| {
            inner
                .exceeded
                .fetch_add(bytes, Ordering::AcqRel)
                .saturating_add(bytes)
        })
    }
    pub(crate) fn receive(&self, bytes: usize) {
        if let Some(inner) = &self.0 {
            inner.buffered.fetch_add(bytes, Ordering::AcqRel);
        }
    }
    pub(crate) fn take_consumed(&self) -> usize {
        self.0.as_ref().map_or(0, |inner| {
            let threshold = inner.threshold.load(Ordering::Acquire);
            let mut consumed = inner.consumed.load(Ordering::Acquire);
            while consumed >= threshold {
                match inner.consumed.compare_exchange_weak(
                    consumed,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return consumed,
                    Err(current) => consumed = current,
                }
            }
            0
        })
    }
    /// 归还实际读走的字节数；返回值为空，调用方不得重复确认同一段数据。
    pub fn consume(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        if let Some(inner) = &self.0 {
            let mut buffered = inner.buffered.load(Ordering::Acquire);
            let consumed = loop {
                debug_assert!(bytes <= buffered, "消费字节不能超过尚未确认的缓冲");
                let count = bytes.min(buffered);
                match inner.buffered.compare_exchange_weak(
                    buffered,
                    buffered - count,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break count,
                    Err(current) => buffered = current,
                }
            };
            let previous = inner.consumed.fetch_add(consumed, Ordering::AcqRel);
            let threshold = inner.threshold.load(Ordering::Acquire);
            if previous < threshold && previous + consumed >= threshold {
                inner.notify.notify_one();
            }
        }
    }
    /// 无参数；返回已收到但尚未最终消费的字节数，包含调用方延迟确认的缓冲。
    pub fn buffered_bytes(&self) -> usize {
        self.0
            .as_ref()
            .map_or(0, |inner| inner.buffered.load(Ordering::Acquire))
    }
}

pub(crate) fn data_len(msg: &ChannelMsg) -> usize {
    match msg {
        ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => data.len(),
        _ => 0,
    }
}
