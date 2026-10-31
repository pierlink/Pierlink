// Pierlink 修改：验证部分写入、刷新挂起和错误时的密文字节顺序。
use super::PacketWriter;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[tokio::test]
async fn 部分写入取消后追加报文不会重复或遗漏密文() {
    let mut writer = PacketWriter::clear();
    writer.packet_raw(&vec![0x5a; 256 * 1024]).unwrap();
    let mut expected = writer.buffer().buffer.clone();
    let (mut sender, mut receiver) = tokio::io::duplex(64 * 1024);
    assert!(futures::poll!(Box::pin(writer.flush_into(&mut sender))).is_pending());
    assert!(writer.has_pending_write());
    let previous_len = writer.buffer().buffer.len();
    writer.packet_raw("追加的报文".as_bytes()).unwrap();
    expected.extend_from_slice(&writer.buffer().buffer[previous_len..]);
    let mut actual = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            async {
                writer.flush_into(&mut sender).await.unwrap();
                sender.shutdown().await.unwrap();
            },
            async {
                receiver.read_to_end(&mut actual).await.unwrap();
            }
        );
    })
    .await
    .unwrap();
    assert_eq!(actual, expected);
    assert!(!writer.has_pending_write());
}

#[tokio::test]
async fn 反复取消仅在已发前缀达到一半时回收且保留追加顺序() {
    let mut writer = PacketWriter::clear();
    writer.packet_raw(&vec![0x5a; 256 * 1024]).unwrap();
    let mut expected = writer.buffer().buffer.clone();
    let (mut sender, mut receiver) = tokio::io::duplex(4096);
    assert!(!writer.take_write_progress());
    assert!(futures::poll!(Box::pin(writer.flush_into(&mut sender))).is_pending());
    assert!(writer.take_write_progress());
    assert!(!writer.take_write_progress());
    let offset = writer.write_offset;
    let length = writer.buffer().buffer.len();
    // 没有新的传输进展时反复取消，不应搬移尚未发出的长缓冲。
    for _ in 0..32 {
        assert!(futures::poll!(Box::pin(writer.flush_into(&mut sender))).is_pending());
        assert_eq!(writer.write_offset, offset);
        assert_eq!(writer.buffer().buffer.len(), length);
        assert!(!writer.take_write_progress());
    }
    let mut actual = Vec::new();
    let mut block = [0; 4096];
    while writer.write_offset < writer.buffer().buffer.len() - writer.write_offset {
        receiver.read_exact(&mut block).await.unwrap();
        actual.extend_from_slice(&block);
        assert!(futures::poll!(Box::pin(writer.flush_into(&mut sender))).is_pending());
        assert!(writer.take_write_progress());
    }
    let remaining = writer.buffer().buffer.len() - writer.write_offset;
    assert!(futures::poll!(Box::pin(writer.flush_into(&mut sender))).is_pending());
    assert_eq!(writer.write_offset, 0);
    assert_eq!(writer.buffer().buffer.len(), remaining);
    assert!(!writer.take_write_progress());
    writer.packet_raw("回收后追加".as_bytes()).unwrap();
    expected.extend_from_slice(&writer.buffer().buffer[remaining..]);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            async {
                writer.flush_into(&mut sender).await.unwrap();
                sender.shutdown().await.unwrap();
            },
            async { receiver.read_to_end(&mut actual).await.unwrap() }
        );
    })
    .await
    .unwrap();
    assert_eq!(actual, expected);
    assert!(writer.take_write_progress());
    assert!(!writer.has_pending_write());
    assert!(writer.buffer().buffer.is_empty());
}

#[derive(Default)]
struct TestWriter {
    bytes: Vec<u8>,
    flush_ready: bool,
    failure: Option<io::ErrorKind>,
}

impl AsyncWrite for TestWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Some(kind) = self.failure {
            return Poll::Ready(if kind == io::ErrorKind::WriteZero {
                Ok(0)
            } else {
                Err(io::Error::new(kind, "测试写入失败"))
            });
        }
        self.bytes.extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.flush_ready {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn 刷新挂起时取消仍保留刷新状态并允许追加报文() {
    let mut writer = PacketWriter::clear();
    let mut stream = TestWriter::default();
    assert!(!writer.has_pending_write());
    writer.flush_into(&mut stream).await.unwrap();
    writer.packet_raw(b"first").unwrap();
    let mut expected = writer.buffer().buffer.clone();
    assert!(futures::poll!(Box::pin(writer.flush_into(&mut stream))).is_pending());
    assert!(writer.has_pending_write());
    assert!(writer.buffer().buffer.is_empty());
    writer.packet_raw(b"second").unwrap();
    expected.extend_from_slice(&writer.buffer().buffer);
    stream.flush_ready = true;
    writer.flush_into(&mut stream).await.unwrap();
    assert_eq!(stream.bytes, expected);
    assert!(!writer.has_pending_write());
}

#[tokio::test]
async fn 零写入和传输错误立即返回且不丢弃未发报文() {
    for kind in [io::ErrorKind::WriteZero, io::ErrorKind::BrokenPipe] {
        let mut writer = PacketWriter::clear();
        writer.packet_raw(b"payload").unwrap();
        let expected = writer.buffer().buffer.clone();
        let mut stream = TestWriter {
            failure: Some(kind),
            ..Default::default()
        };
        assert_eq!(
            writer.flush_into(&mut stream).await.unwrap_err().kind(),
            kind
        );
        assert!(writer.has_pending_write());
        stream.failure = None;
        stream.flush_ready = true;
        writer.flush_into(&mut stream).await.unwrap();
        assert_eq!(stream.bytes, expected);
    }
}
