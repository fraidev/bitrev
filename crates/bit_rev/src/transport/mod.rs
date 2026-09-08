use std::future::Future;
use std::io::{self, Cursor};
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

mod tcp;

pub use tcp::TcpConnector;

/// Budget applied by [`TcpConnector`]. MSE wrappers and other connectors
/// should honor the same limit unless they define their own.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);

/// A bidirectional peer byte stream. TCP today; MSE and uTP later.
pub trait PeerStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

impl<T> PeerStream for T where T: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

pub type BoxedPeerStream = Box<dyn PeerStream>;

pub fn boxed_stream<S: PeerStream>(stream: S) -> BoxedPeerStream {
    Box::new(stream)
}

pub type DialFuture<'a> = Pin<Box<dyn Future<Output = io::Result<BoxedPeerStream>> + Send + 'a>>;

/// Outgoing dialer. MSE wraps one; uTP adds one.
pub trait Connector: Send + Sync {
    fn dial(&self, addr: SocketAddr) -> DialFuture<'_>;
}

/// Classification of inbound traffic before the BitTorrent handshake.
/// MSE peeks the first byte: `0x13` is plaintext, anything else is encrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncomingKind {
    Plaintext,
    MaybeEncrypted,
}

/// pstrlen of a standard BitTorrent handshake (`"BitTorrent protocol"`).
pub const BT_HANDSHAKE_PSTRLEN: u8 = 19;

pub fn classify_incoming(first_byte: u8) -> IncomingKind {
    if first_byte == BT_HANDSHAKE_PSTRLEN {
        IncomingKind::Plaintext
    } else {
        IncomingKind::MaybeEncrypted
    }
}

/// Replay bytes already read (MSE peek) before the inner stream.
pub struct PrefixedStream<S> {
    prefix: Cursor<Vec<u8>>,
    inner: S,
}

impl<S> PrefixedStream<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix: Cursor::new(prefix),
            inner,
        }
    }

    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let pos = this.prefix.position() as usize;
        let data = this.prefix.get_ref();
        if pos < data.len() {
            let remaining = &data[pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.prefix.set_position((pos + n) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Inbound stream plus optional peek classification for the listener seam.
pub struct IncomingStream {
    pub stream: BoxedPeerStream,
    pub addr: SocketAddr,
    pub kind: IncomingKind,
}

impl IncomingStream {
    pub fn new(stream: BoxedPeerStream, addr: SocketAddr) -> Self {
        Self {
            stream,
            addr,
            kind: IncomingKind::Plaintext,
        }
    }

    /// Wrap a stream that already had `prefix` read. MSE uses this after peeking.
    pub fn with_peek(prefix: Vec<u8>, stream: BoxedPeerStream, addr: SocketAddr) -> Self {
        let kind = prefix
            .first()
            .copied()
            .map(classify_incoming)
            .unwrap_or(IncomingKind::Plaintext);
        let stream = if prefix.is_empty() {
            stream
        } else {
            boxed_stream(PrefixedStream::new(prefix, stream))
        };
        Self { stream, addr, kind }
    }
}

/// Apply the shared connect budget to an arbitrary dial future.
pub async fn with_connect_timeout<F, T>(fut: F) -> io::Result<T>
where
    F: Future<Output = io::Result<T>>,
{
    with_connect_timeout_for(CONNECT_TIMEOUT, fut).await
}

pub async fn with_connect_timeout_for<F, T>(timeout: Duration, fut: F) -> io::Result<T>
where
    F: Future<Output = io::Result<T>>,
{
    match tokio::time::timeout(timeout, fut).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "peer connect timed out",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn classify_plaintext_vs_encrypted() {
        assert_eq!(classify_incoming(19), IncomingKind::Plaintext);
        assert_eq!(classify_incoming(0), IncomingKind::MaybeEncrypted);
        assert_eq!(classify_incoming(0x13), IncomingKind::Plaintext);
        assert_eq!(classify_incoming(0x20), IncomingKind::MaybeEncrypted);
    }

    #[tokio::test]
    async fn prefixed_stream_replays_then_inner() {
        let (client, mut server) = tokio::io::duplex(64);
        tokio::spawn(async move {
            server.write_all(b"def").await.unwrap();
        });
        let mut stream = PrefixedStream::new(b"abc".to_vec(), client);
        let mut buf = [0u8; 6];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"abcdef");
    }

    struct DelayedDuplex {
        delay: Duration,
    }

    impl Connector for DelayedDuplex {
        fn dial(&self, _addr: SocketAddr) -> DialFuture<'_> {
            let delay = self.delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                let (half, _peer) = tokio::io::duplex(32);
                Ok(boxed_stream(half))
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn mock_connector_duplex_times_out_under_pause() {
        let connector = DelayedDuplex {
            delay: Duration::from_secs(10),
        };
        let addr = "127.0.0.1:1".parse().unwrap();
        let fut = with_connect_timeout(connector.dial(addr));
        tokio::pin!(fut);

        tokio::select! {
            _ = &mut fut => panic!("connect should still be pending"),
            _ = tokio::time::sleep(Duration::from_millis(1)) => {}
        }

        tokio::time::advance(CONNECT_TIMEOUT).await;
        match fut.await {
            Err(err) => assert_eq!(err.kind(), io::ErrorKind::TimedOut),
            Ok(_) => panic!("connect budget exceeded"),
        }
    }

    struct ReadyDuplex {
        half: Mutex<Option<BoxedPeerStream>>,
    }

    impl Connector for ReadyDuplex {
        fn dial(&self, _addr: SocketAddr) -> DialFuture<'_> {
            let half = self.half.lock().unwrap().take();
            Box::pin(
                async move { half.ok_or_else(|| io::Error::other("duplex half already taken")) },
            )
        }
    }

    #[tokio::test]
    async fn mock_connector_returns_duplex_half() {
        let (half, mut peer) = tokio::io::duplex(32);
        let connector = ReadyDuplex {
            half: Mutex::new(Some(boxed_stream(half))),
        };
        let addr = "127.0.0.1:6881".parse().unwrap();
        let mut stream = connector.dial(addr).await.unwrap();
        tokio::spawn(async move {
            peer.write_all(b"hi").await.unwrap();
        });
        let mut buf = [0u8; 2];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");
    }

    #[tokio::test]
    async fn incoming_stream_with_peek_classifies_and_replays() {
        let (client, mut server) = tokio::io::duplex(64);
        tokio::spawn(async move {
            server.write_all(b"xyz").await.unwrap();
        });
        let incoming = IncomingStream::with_peek(
            b"\x13Bit".to_vec(),
            boxed_stream(client),
            "127.0.0.1:1".parse().unwrap(),
        );
        assert_eq!(incoming.kind, IncomingKind::Plaintext);
        let mut stream = incoming.stream;
        let mut buf = [0u8; 7];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"\x13Bitxyz");
    }
}
