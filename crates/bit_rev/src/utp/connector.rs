use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::transport::{
    boxed_stream, with_connect_timeout_for, Connector, DialFuture, PeerConnected, PeerDial,
    PeerDialFuture,
};

use super::socket::UtpSocket;

/// Result of a deferred uTP bind. `None` means the listener is still starting.
pub type UtpBindState = Option<Result<Arc<UtpSocket>, ()>>;

/// Outgoing uTP dialer. Shares the session's multiplexed [`UtpSocket`].
#[derive(Clone)]
pub struct UtpConnector {
    source: UtpSocketSource,
    timeout: Duration,
}

#[derive(Clone)]
enum UtpSocketSource {
    Ready(Arc<UtpSocket>),
    Watch(watch::Receiver<UtpBindState>),
}

impl UtpConnector {
    pub fn new(socket: Arc<UtpSocket>) -> Self {
        Self {
            source: UtpSocketSource::Ready(socket),
            timeout: crate::transport::CONNECT_TIMEOUT,
        }
    }

    pub fn with_timeout(socket: Arc<UtpSocket>, timeout: Duration) -> Self {
        Self {
            source: UtpSocketSource::Ready(socket),
            timeout,
        }
    }

    /// Wait until [`Session`] finishes binding the uTP socket.
    pub fn deferred(rx: watch::Receiver<UtpBindState>) -> Self {
        Self {
            source: UtpSocketSource::Watch(rx),
            timeout: crate::transport::CONNECT_TIMEOUT,
        }
    }

    async fn socket(&self) -> io::Result<Arc<UtpSocket>> {
        match &self.source {
            UtpSocketSource::Ready(socket) => Ok(socket.clone()),
            UtpSocketSource::Watch(rx) => {
                let mut rx = rx.clone();
                loop {
                    if let Some(result) = rx.borrow().clone() {
                        return result.map_err(|()| {
                            io::Error::new(
                                io::ErrorKind::AddrNotAvailable,
                                "uTP socket failed to bind",
                            )
                        });
                    }
                    rx.changed().await.map_err(|_| {
                        io::Error::new(io::ErrorKind::NotConnected, "uTP bind cancelled")
                    })?;
                }
            }
        }
    }
}

impl Connector for UtpConnector {
    fn dial(&self, addr: std::net::SocketAddr) -> DialFuture<'_> {
        let timeout = self.timeout;
        Box::pin(async move {
            let socket = self.socket().await?;
            let stream = with_connect_timeout_for(timeout, socket.connect(addr)).await?;
            Ok(boxed_stream(stream))
        })
    }

    fn dial_peer(&self, req: PeerDial) -> PeerDialFuture<'_> {
        let timeout = self.timeout;
        Box::pin(async move {
            let socket = self.socket().await?;
            let stream = with_connect_timeout_for(timeout, socket.connect(req.addr)).await?;
            Ok(PeerConnected {
                stream: boxed_stream(stream),
                encrypted: false,
                sent_initial_payload: false,
                utp: true,
            })
        })
    }
}
