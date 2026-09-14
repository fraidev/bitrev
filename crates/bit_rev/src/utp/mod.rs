//! µTP (BEP-0029) over a dedicated UDP socket.
//!
//! Outgoing dials race uTP against TCP via [`crate::transport::RacingConnector`].
//! The first successful handshake wins and the loser is dropped. That prefers a
//! working UDP path without waiting for a uTP timeout when only TCP is reachable.

mod conn;
mod connector;
mod delay;
pub mod header;
pub mod relay;
mod socket;

pub use conn::{UtpStats, UtpStream};
pub use connector::{UtpBindState, UtpConnector};
pub use header::{DecodeError, Packet, PacketType, HEADER_LEN};
pub use relay::{LossyRelay, RelayConfig};
pub use socket::UtpSocket;

/// Session knob for the uTP listener and outgoing race.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UtpOptions {
    pub enabled: bool,
    /// 0 means "same port as the TCP listen socket".
    pub port: u16,
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    async fn pair() -> (UtpSocket, UtpSocket) {
        let server = UtpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let client = UtpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        (client, server)
    }

    #[tokio::test]
    async fn loopback_connect_transfer_close() {
        let (client_sock, server_sock) = pair().await;
        let server_addr = server_sock.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            let (mut stream, _) = server_sock.accept().await.unwrap();
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).await.unwrap();
            stream.write_all(b"world").await.unwrap();
            stream.shutdown().await.unwrap();
            let mut rest = Vec::new();
            stream.read_to_end(&mut rest).await.unwrap();
            (buf, rest)
        });

        let mut client = client_sock.connect(server_addr).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        client.flush().await.unwrap();
        let mut buf = vec![0u8; 5];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");
        client.shutdown().await.unwrap();
        let mut eof = Vec::new();
        client.read_to_end(&mut eof).await.unwrap();
        assert!(eof.is_empty());

        let (got, rest) = accept.await.unwrap();
        assert_eq!(&got, b"hello");
        assert!(rest.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn retransmit_after_dropped_data() {
        let relay = LossyRelay::bind(RelayConfig {
            drop_first_data: 1,
            ..RelayConfig::default()
        })
        .await
        .unwrap();

        let server = UtpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let client = UtpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        relay.set_backend(server.local_addr().unwrap());
        let relay_addr = relay.local_addr();

        let accept = tokio::spawn(async move {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut buf = vec![0u8; 16];
            stream.read_exact(&mut buf).await.unwrap();
            stream.shutdown().await.unwrap();
            (buf, stream.stats())
        });

        let mut stream = client.connect(relay_addr).await.unwrap();
        stream.write_all(b"0123456789abcdef").await.unwrap();
        stream.flush().await.unwrap();

        // First DATA was dropped. Advance past the RTO so it is resent.
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;

        let (got, _server_stats) = tokio::time::timeout(Duration::from_secs(5), accept)
            .await
            .expect("accept join")
            .unwrap();
        assert_eq!(&got, b"0123456789abcdef");
        assert!(
            stream.stats().retransmits > 0 || relay.data_dropped() > 0,
            "retransmit must run after the dropped DATA packet"
        );
        assert!(
            stream.stats().retransmits > 0,
            "client should have retransmitted, stats={:?}",
            stream.stats()
        );
        stream.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn lossy_relay_preserves_bytes() {
        let relay = LossyRelay::bind(RelayConfig {
            drop_rate: 0.10,
            dup_rate: 0.10,
            reorder_rate: 0.15,
            seed: 42,
            drop_first_data: 0,
        })
        .await
        .unwrap();

        let server = UtpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let client = UtpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        relay.set_backend(server.local_addr().unwrap());
        let payload: Vec<u8> = (0..8000).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();

        let accept = tokio::spawn(async move {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut buf = vec![0u8; expected.len()];
            stream.read_exact(&mut buf).await.unwrap();
            stream.shutdown().await.unwrap();
            buf
        });

        let mut stream = client.connect(relay.local_addr()).await.unwrap();
        stream.write_all(&payload).await.unwrap();
        stream.flush().await.unwrap();
        stream.shutdown().await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(15), accept)
            .await
            .expect("lossy transfer")
            .unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn unknown_connection_gets_reset() {
        let server = UtpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = server.local_addr().unwrap();
        let udp = tokio::net::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let mut pkt = Packet::new(PacketType::Data, 99, 1, 0);
        pkt.payload = b"x".to_vec();
        udp.send_to(&pkt.encode(), addr).await.unwrap();
        let mut buf = [0u8; 64];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), udp.recv_from(&mut buf))
            .await
            .expect("reset reply")
            .unwrap();
        let reply = Packet::decode(&buf[..n]).unwrap();
        assert_eq!(reply.ty, PacketType::Reset);
        assert_eq!(reply.connection_id, 99);
    }
}
