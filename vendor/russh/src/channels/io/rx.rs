// Pierlink 修改：按实际读取或丢弃的字节归还额度，部分读取器释放时归还余量，保留上游导入顺序。
use std::borrow::BorrowMut;
use std::io;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use tokio::io::AsyncRead;

use super::{ChannelMsg, ChannelReadHalf};

#[derive(Debug)]
pub struct ChannelRx<R> {
    channel: R,
    buffer: Option<(ChannelMsg, usize)>,

    ext: Option<u32>,
    consumption: super::super::ChannelConsumption,
}

impl<R: BorrowMut<ChannelReadHalf>> ChannelRx<R> {
    pub fn new(mut channel: R, ext: Option<u32>) -> Self {
        let consumption = channel.borrow_mut().consumption();
        Self {
            consumption,
            channel,
            buffer: None,
            ext,
        }
    }
}

impl<R> AsyncRead for ChannelRx<R>
where
    R: BorrowMut<ChannelReadHalf> + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let (msg, mut idx) = match self.buffer.take() {
            Some(msg) => msg,
            None => match ready!(self.channel.borrow_mut().receiver.poll_recv(cx)) {
                Some(msg) => (msg, 0),
                None => return Poll::Ready(Ok(())),
            },
        };

        match (&msg, self.ext) {
            (ChannelMsg::Data { data }, None) => {
                let readable = buf.remaining().min(data.len() - idx);

                // Clamped to maximum `buf.remaining()` and `data.len() - idx` with `.min`
                #[allow(clippy::indexing_slicing)]
                buf.put_slice(&data[idx..idx + readable]);
                idx += readable;
                self.channel.borrow_mut().consumption.consume(readable);

                if idx != data.len() {
                    self.buffer = Some((msg, idx));
                }

                Poll::Ready(Ok(()))
            }
            (ChannelMsg::ExtendedData { data, ext }, Some(target)) if *ext == target => {
                let readable = buf.remaining().min(data.len() - idx);

                // Clamped to maximum `buf.remaining()` and `data.len() - idx` with `.min`
                #[allow(clippy::indexing_slicing)]
                buf.put_slice(&data[idx..idx + readable]);
                idx += readable;
                self.channel.borrow_mut().consumption.consume(readable);

                if idx != data.len() {
                    self.buffer = Some((msg, idx));
                }

                Poll::Ready(Ok(()))
            }
            (ChannelMsg::Eof | ChannelMsg::Close, _) => {
                self.channel.borrow_mut().receiver.close();

                Poll::Ready(Ok(()))
            }
            _ => {
                self.channel
                    .borrow_mut()
                    .consumption
                    .consume(super::super::receive::data_len(&msg));
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

impl<R> Drop for ChannelRx<R> {
    fn drop(&mut self) {
        if let Some((msg, offset)) = self.buffer.take() {
            self.consumption
                .consume(super::super::receive::data_len(&msg).saturating_sub(offset));
        }
    }
}
