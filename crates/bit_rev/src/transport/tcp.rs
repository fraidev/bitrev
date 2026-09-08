use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;

use super::{boxed_stream, with_connect_timeout_for, Connector, DialFuture};

/// Outgoing TCP dialer. The 6s connect budget lives here.
#[derive(Debug, Clone)]
pub struct TcpConnector {
    timeout: Duration,
}

impl TcpConnector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Default for TcpConnector {
    fn default() -> Self {
        Self {
            timeout: super::CONNECT_TIMEOUT,
        }
    }
}

impl Connector for TcpConnector {
    fn dial(&self, addr: SocketAddr) -> DialFuture<'_> {
        let timeout = self.timeout;
        Box::pin(async move {
            let stream = with_connect_timeout_for(timeout, TcpStream::connect(addr)).await?;
            Ok(boxed_stream(stream))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::CONNECT_TIMEOUT;
    use std::io::ErrorKind;

    #[tokio::test(start_paused = true)]
    async fn tcp_connector_times_out_under_pause() {
        let connector = TcpConnector::new();
        // TEST-NET-1 is reserved and should not answer SYNs.
        let addr: SocketAddr = "192.0.2.1:1".parse().unwrap();
        let fut = connector.dial(addr);
        tokio::pin!(fut);

        tokio::select! {
            result = &mut fut => {
                // Some environments fail immediately (network unreachable).
                // That is still a failed dial, not a hang.
                match result {
                    Err(err) => assert_ne!(err.kind(), ErrorKind::NotFound),
                    Ok(_) => panic!("blackhole dial must not succeed"),
                }
                return;
            }
            _ = tokio::time::sleep(Duration::from_millis(1)) => {}
        }

        tokio::time::advance(CONNECT_TIMEOUT).await;
        match fut.await {
            Err(err) => assert_eq!(err.kind(), ErrorKind::TimedOut),
            Ok(_) => panic!("connect budget exceeded"),
        }
    }
}
