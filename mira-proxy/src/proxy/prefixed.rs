//! A stream that replays bytes already read off it.

use bytes::Buf;
use bytes::Bytes;
use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;

/// Wraps a stream so reads return `prefix` before anything new.
///
/// A client may send its first tunnel bytes, such as the start of a TLS
/// `ClientHello`, in the same packet as its `CONNECT`. Those bytes land in
/// the request buffer, and whatever reads the tunnel next MUST see them.
pub struct PrefixedStream<S> {
    prefix: Bytes,
    inner: S,
}

impl<S> PrefixedStream<S> {
    /// Replay `prefix`, then read from `inner`.
    pub fn new(prefix: Bytes, inner: S) -> Self {
        Self { prefix, inner }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.prefix.is_empty() {
            return Pin::new(&mut self.inner).poll_read(cx, buf);
        }
        let n = self.prefix.len().min(buf.remaining());
        buf.put_slice(&self.prefix[..n]);
        self.prefix.advance(n);
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::io::duplex;

    #[tokio::test]
    async fn test_prefix_is_read_before_the_stream() {
        let (mut peer, inner) = duplex(64);
        peer.write_all(b" world").await.expect("write");
        drop(peer);

        let mut stream = PrefixedStream::new(Bytes::from_static(b"hello"), inner);
        let mut seen = String::new();
        stream.read_to_string(&mut seen).await.expect("read");
        assert_eq!(seen, "hello world");
    }

    #[tokio::test]
    async fn test_prefix_larger_than_the_read_buffer_is_replayed_in_parts() {
        let (peer, inner) = duplex(64);
        drop(peer);

        let mut stream = PrefixedStream::new(Bytes::from_static(b"abcdef"), inner);
        let mut part = [0u8; 4];
        let n = stream.read(&mut part).await.expect("read");
        assert_eq!(&part[..n], b"abcd");
        let n = stream.read(&mut part).await.expect("read");
        assert_eq!(&part[..n], b"ef");
    }
}
