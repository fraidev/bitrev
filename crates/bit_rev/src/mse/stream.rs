use std::io::{self, Cursor};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::rc4::Rc4;

/// Bidirectional MSE stream. RC4 ciphers are `None` when plaintext was selected.
pub struct EncryptedStream<S> {
    inner: S,
    enc: Option<Rc4>,
    dec: Option<Rc4>,
    prefix: Cursor<Vec<u8>>,
    write_pending: Vec<u8>,
    write_src_len: usize,
}

impl<S> EncryptedStream<S> {
    pub fn new(inner: S, enc: Option<Rc4>, dec: Option<Rc4>, prefix: Vec<u8>) -> Self {
        Self {
            inner,
            enc,
            dec,
            prefix: Cursor::new(prefix),
            write_pending: Vec::new(),
            write_src_len: 0,
        }
    }

    pub fn rc4(inner: S, enc: Rc4, dec: Rc4, prefix: Vec<u8>) -> Self {
        Self::new(inner, Some(enc), Some(dec), prefix)
    }

    pub fn plaintext(inner: S, prefix: Vec<u8>) -> Self {
        Self::new(inner, None, None, prefix)
    }

    pub fn is_encrypted(&self) -> bool {
        self.enc.is_some()
    }

    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for EncryptedStream<S> {
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

        let filled_before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if let Some(dec) = this.dec.as_mut() {
                    let filled = buf.filled_mut();
                    if filled.len() > filled_before {
                        dec.apply(&mut filled[filled_before..]);
                    }
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<S: AsyncWrite + Unpin> EncryptedStream<S> {
    fn poll_flush_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.write_pending.is_empty() {
            match Pin::new(&mut self.inner).poll_write(cx, &self.write_pending) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "mse write wrote zero bytes",
                    )));
                }
                Poll::Ready(Ok(n)) => {
                    self.write_pending.drain(..n);
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for EncryptedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.poll_flush_pending(cx) {
            Poll::Ready(Ok(())) => {
                if this.write_src_len > 0 {
                    let written = this.write_src_len;
                    this.write_src_len = 0;
                    return Poll::Ready(Ok(written));
                }
            }
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        }

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if let Some(enc) = this.enc.as_mut() {
            this.write_pending.extend_from_slice(buf);
            enc.apply(&mut this.write_pending);
            this.write_src_len = buf.len();
            match this.poll_flush_pending(cx) {
                Poll::Ready(Ok(())) => {
                    this.write_src_len = 0;
                    Poll::Ready(Ok(buf.len()))
                }
                Poll::Ready(Err(e)) => {
                    this.write_pending.clear();
                    this.write_src_len = 0;
                    Poll::Ready(Err(e))
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Pin::new(&mut this.inner).poll_write(cx, buf)
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.poll_flush_pending(cx) {
            Poll::Ready(Ok(())) => {
                this.write_src_len = 0;
                Pin::new(&mut this.inner).poll_flush(cx)
            }
            other => other,
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.as_mut().poll_flush(cx) {
            Poll::Ready(Ok(())) => {}
            other => return other,
        }
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::EncryptedStream;
    use crate::mse::rc4::Rc4;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn rc4_round_trip_over_duplex() {
        let (a, b) = tokio::io::duplex(64);
        let key_ab = b"key-a-to-b-20-bytes!";
        let key_ba = b"key-b-to-a-20-bytes!";
        let mut client = EncryptedStream::rc4(a, Rc4::new(key_ab), Rc4::new(key_ba), Vec::new());
        let mut server = EncryptedStream::rc4(b, Rc4::new(key_ba), Rc4::new(key_ab), Vec::new());

        client.write_all(b"hello").await.unwrap();
        client.flush().await.unwrap();
        let mut buf = [0u8; 5];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");

        server.write_all(b"world").await.unwrap();
        server.flush().await.unwrap();
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");
    }

    #[tokio::test]
    async fn prefix_is_not_decrypted() {
        let (a, mut b) = tokio::io::duplex(32);
        tokio::spawn(async move {
            b.write_all(b"xy").await.unwrap();
        });
        let mut stream = EncryptedStream::plaintext(a, b"ab".to_vec());
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"abxy");
    }
}
