use std::sync::Arc;

use super::{Connector, DialFuture, PeerDial, PeerDialFuture};

/// Races two dialers. The first successful connection wins and the other
/// future is dropped. Used to try uTP and TCP at the same time.
pub struct RacingConnector {
    first: Arc<dyn Connector>,
    second: Arc<dyn Connector>,
}

impl RacingConnector {
    pub fn new(first: impl Connector + 'static, second: impl Connector + 'static) -> Self {
        Self {
            first: Arc::new(first),
            second: Arc::new(second),
        }
    }

    pub fn from_arcs(first: Arc<dyn Connector>, second: Arc<dyn Connector>) -> Self {
        Self { first, second }
    }
}

impl Connector for RacingConnector {
    fn dial(&self, addr: std::net::SocketAddr) -> DialFuture<'_> {
        let first = self.first.clone();
        let second = self.second.clone();
        Box::pin(async move { race_two(first.dial(addr), second.dial(addr)).await })
    }

    fn dial_peer(&self, req: PeerDial) -> PeerDialFuture<'_> {
        let first = self.first.clone();
        let second = self.second.clone();
        Box::pin(async move { race_two(first.dial_peer(req.clone()), second.dial_peer(req)).await })
    }
}

async fn race_two<T>(
    a: impl std::future::Future<Output = std::io::Result<T>>,
    b: impl std::future::Future<Output = std::io::Result<T>>,
) -> std::io::Result<T> {
    tokio::pin!(a);
    tokio::pin!(b);
    let mut a_err = None;
    let mut b_err = None;
    loop {
        tokio::select! {
            result = &mut a, if a_err.is_none() => match result {
                Ok(value) => return Ok(value),
                Err(err) => a_err = Some(err),
            },
            result = &mut b, if b_err.is_none() => match result {
                Ok(value) => return Ok(value),
                Err(err) => b_err = Some(err),
            },
            else => {
                return Err(a_err.take().or(b_err.take()).unwrap_or_else(|| {
                    std::io::Error::other("both connectors failed")
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{boxed_stream, Connector, DialFuture};
    use std::io;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    struct FailConnector;

    impl Connector for FailConnector {
        fn dial(&self, _addr: SocketAddr) -> DialFuture<'_> {
            Box::pin(async { Err(io::Error::new(io::ErrorKind::ConnectionRefused, "no")) })
        }
    }

    struct SlowOk;

    impl Connector for SlowOk {
        fn dial(&self, _addr: SocketAddr) -> DialFuture<'_> {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                let (half, mut peer) = tokio::io::duplex(16);
                tokio::spawn(async move {
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut peer, b"ok").await;
                });
                Ok(boxed_stream(half))
            })
        }
    }

    #[tokio::test]
    async fn race_falls_back_when_first_fails() {
        let race = RacingConnector::new(FailConnector, SlowOk);
        let addr = "127.0.0.1:1".parse().unwrap();
        let mut stream = race.dial(addr).await.unwrap();
        let mut buf = [0u8; 2];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ok");
    }

    #[tokio::test]
    async fn race_returns_first_success() {
        let race = RacingConnector::new(SlowOk, FailConnector);
        let addr = "127.0.0.1:1".parse().unwrap();
        assert!(race.dial(addr).await.is_ok());
    }
}
