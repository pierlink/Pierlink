// Pierlink 修改：客户端按窗口缓存且分发不等待，通道释放唤醒写入方。
use super::{
    WindowSizeRef,
    receive::{ChannelConsumption, ChannelReceiver},
};
use crate::ChannelMsg;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::sync::mpsc::{self, Sender, UnboundedSender};

#[derive(Debug)]
enum Delivery {
    Bounded(Sender<ChannelMsg>),
    Windowed(UnboundedSender<ChannelMsg>),
}

/// 通道事件投递句柄；客户端的数据字节数由 SSH 接收窗口约束。
#[derive(Debug)]
pub struct ChannelRef {
    sender: Delivery,
    pub(super) window_size: WindowSizeRef,
    pub(crate) consumption: ChannelConsumption,
}
impl ChannelRef {
    /// 以给定有界发送端建立服务端句柄，返回投递句柄。
    pub fn new(sender: Sender<ChannelMsg>) -> Self {
        Self {
            sender: Delivery::Bounded(sender),
            window_size: WindowSizeRef::new(0),
            consumption: ChannelConsumption::default(),
        }
    }
    pub(crate) fn windowed(notify: Arc<Notify>) -> (Self, ChannelReceiver) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Self {
                sender: Delivery::Windowed(sender),
                window_size: WindowSizeRef::new(0),
                consumption: ChannelConsumption::new(notify),
            },
            ChannelReceiver::Windowed(receiver),
        )
    }
    pub(crate) fn window_size(&self) -> &WindowSizeRef {
        &self.window_size
    }
    /// 向应用投递给定消息，客户端立即返回；接收端已释放时返回原消息。
    pub async fn send(&self, msg: ChannelMsg) -> Result<(), mpsc::error::SendError<ChannelMsg>> {
        match &self.sender {
            Delivery::Bounded(tx) => tx.send(msg).await,
            Delivery::Windowed(tx) => tx.send(msg),
        }
    }
    /// 尝试投递给定消息，返回队列已满或接收端已关闭的错误。
    pub fn try_send(&self, msg: ChannelMsg) -> Result<(), mpsc::error::TrySendError<ChannelMsg>> {
        match &self.sender {
            Delivery::Bounded(tx) => tx.try_send(msg),
            Delivery::Windowed(tx) => tx
                .send(msg)
                .map_err(|e| mpsc::error::TrySendError::Closed(e.0)),
        }
    }
}
impl Drop for ChannelRef {
    fn drop(&mut self) {
        self.window_size.close();
    }
}
