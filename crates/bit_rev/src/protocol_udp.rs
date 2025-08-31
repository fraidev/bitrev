use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use rand::Rng;
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::{timeout, Instant};
use tracing::debug;

use crate::file::TorrentMeta;

const PROTOCOL_ID: u64 = 0x41727101980;
const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
#[allow(dead_code)]
const ACTION_SCRAPE: u32 = 2;
const ACTION_ERROR: u32 = 3;

#[derive(Debug, Clone)]
pub struct UdpTracker {
    pub url: String,
    pub connection_id: Option<u64>,
    pub last_connect: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct UdpPeer {
    pub ip: [u8; 4],
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct UdpAnnounceResponse {
    pub action: u32,
    pub transaction_id: u32,
    pub interval: u32,
    pub leechers: u32,
    pub seeders: u32,
    pub peers: Vec<UdpPeer>,
}

pub struct AnnounceOptions {
    pub torrent_meta: TorrentMeta,
    pub peer_id: [u8; 20],
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: u32,
}

impl UdpTracker {
    pub fn new(url: String) -> Self {
        Self {
            url,
            connection_id: None,
            last_connect: None,
        }
    }

    pub async fn announce(
        &mut self,
        announce_options: &AnnounceOptions,
    ) -> Result<UdpAnnounceResponse> {
        // Check if we need to connect/reconnect
        if self.connection_id.is_none()
            || self
                .last_connect
                .map_or_else(|| true, |t| t.elapsed() > Duration::from_secs(60))
        {
            self.connect().await?;
        }

        let connection_id = self
            .connection_id
            .ok_or_else(|| anyhow!("No connection ID"))?;

        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let addr = self.parse_udp_url()?;
        // info!("Using UDP tracker at {}", addr);

        let transaction_id: u32 = rand::thread_rng().gen();

        let torrent_meta = &announce_options.torrent_meta;
        let peer_id = &announce_options.peer_id;
        let port = announce_options.port;
        let uploaded = announce_options.uploaded;
        let downloaded = announce_options.downloaded;
        let left = announce_options.left;
        let event = announce_options.event;

        // Build announce request
        let mut request = Vec::new();
        request.write_u64::<BigEndian>(connection_id)?;
        request.write_u32::<BigEndian>(ACTION_ANNOUNCE)?;
        request.write_u32::<BigEndian>(transaction_id)?;
        request.write_all(&torrent_meta.info_hash)?;
        request.write_all(peer_id)?;
        request.write_u64::<BigEndian>(downloaded)?;
        request.write_u64::<BigEndian>(left)?;
        request.write_u64::<BigEndian>(uploaded)?;
        request.write_u32::<BigEndian>(event)?; // 0: none, 1: completed, 2: started, 3: stopped
        request.write_u32::<BigEndian>(0)?; // IP address (0 = default)
        request.write_u32::<BigEndian>(rand::thread_rng().gen())?; // key
        request.write_i32::<BigEndian>(-1)?; // num_want (-1 = default)
        request.write_u16::<BigEndian>(port)?;

        debug!("Sending UDP announce request to {}", addr);
        socket.send_to(&request, addr).await?;

        // Receive response with timeout
        let mut buf = [0u8; 1024];
        let (len, _) = timeout(Duration::from_secs(15), socket.recv_from(&mut buf)).await??;

        self.parse_announce_response(&buf[..len], transaction_id)
    }

    async fn connect(&mut self) -> Result<()> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let addr = self.parse_udp_url()?;

        let transaction_id: u32 = rand::thread_rng().gen();

        // Build connect request
        let mut request = Vec::new();
        request.write_u64::<BigEndian>(PROTOCOL_ID)?;
        request.write_u32::<BigEndian>(ACTION_CONNECT)?;
        request.write_u32::<BigEndian>(transaction_id)?;

        debug!("Sending UDP connect request to {}", addr);
        socket.send_to(&request, addr).await?;

        // Receive response with timeout
        let mut buf = [0u8; 16];
        let (len, _) = timeout(Duration::from_secs(15), socket.recv_from(&mut buf)).await??;

        if len < 16 {
            return Err(anyhow!("Connect response too short: {} bytes", len));
        }

        let mut cursor = Cursor::new(&buf[..len]);
        let action = cursor.read_u32::<BigEndian>()?;
        let response_transaction_id = cursor.read_u32::<BigEndian>()?;

        if action == ACTION_ERROR {
            let error_msg = String::from_utf8_lossy(&buf[8..len]);
            return Err(anyhow!("Tracker error: {}", error_msg));
        }

        if action != ACTION_CONNECT {
            return Err(anyhow!("Invalid action in connect response: {}", action));
        }

        if response_transaction_id != transaction_id {
            return Err(anyhow!("Transaction ID mismatch in connect response"));
        }

        self.connection_id = Some(cursor.read_u64::<BigEndian>()?);
        self.last_connect = Some(Instant::now());

        debug!(
            "UDP tracker connected with connection_id: {:?}",
            self.connection_id
        );
        Ok(())
    }

    fn parse_udp_url(&self) -> Result<SocketAddr> {
        if !self.url.starts_with("udp://") {
            return Err(anyhow!("Invalid UDP tracker URL: {}", self.url));
        }

        let url_without_scheme = &self.url[6..]; // Remove "udp://"

        // Split at the first '/' to separate hostname:port from path
        let host_port = url_without_scheme
            .split('/')
            .next()
            .ok_or_else(|| anyhow!("Invalid UDP tracker URL format: {}", self.url))?;

        let addr = host_port
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow!("Could not resolve UDP tracker address: {}", host_port))?;

        Ok(addr)
    }

    fn parse_announce_response(
        &self,
        data: &[u8],
        expected_transaction_id: u32,
    ) -> Result<UdpAnnounceResponse> {
        if data.len() < 20 {
            return Err(anyhow!("Announce response too short: {} bytes", data.len()));
        }

        let mut cursor = Cursor::new(data);
        let action = cursor.read_u32::<BigEndian>()?;
        let transaction_id = cursor.read_u32::<BigEndian>()?;

        if action == ACTION_ERROR {
            let error_msg = String::from_utf8_lossy(&data[8..]);
            return Err(anyhow!("Tracker error: {}", error_msg));
        }

        if action != ACTION_ANNOUNCE {
            return Err(anyhow!("Invalid action in announce response: {}", action));
        }

        if transaction_id != expected_transaction_id {
            return Err(anyhow!("Transaction ID mismatch in announce response"));
        }

        let interval = cursor.read_u32::<BigEndian>()?;
        let leechers = cursor.read_u32::<BigEndian>()?;
        let seeders = cursor.read_u32::<BigEndian>()?;

        let mut peers = Vec::new();
        let remaining_bytes = data.len() - 20;
        let peer_count = remaining_bytes / 6; // Each peer is 6 bytes (4 IP + 2 port)

        for _ in 0..peer_count {
            let mut ip = [0u8; 4];
            cursor.read_exact(&mut ip)?;
            let port = cursor.read_u16::<BigEndian>()?;

            peers.push(UdpPeer { ip, port });
        }

        debug!(
            "UDP announce response: {} seeders, {} leechers, {} peers",
            seeders,
            leechers,
            peers.len()
        );

        Ok(UdpAnnounceResponse {
            action,
            transaction_id,
            interval,
            leechers,
            seeders,
            peers,
        })
    }
}

impl UdpPeer {
    pub fn to_socket_addr(&self) -> SocketAddr {
        SocketAddr::from((self.ip, self.port))
    }
}

pub async fn request_udp_peers(
    tracker_url: &str,
    torrent_meta: &TorrentMeta,
    peer_id: &[u8; 20],
    port: u16,
) -> Result<UdpAnnounceResponse> {
    let mut tracker = UdpTracker::new(tracker_url.to_string());

    let uploaded = 0;
    let downloaded = 0;
    let left = torrent_meta.torrent_file.info.length.unwrap_or(0) as u64;
    let event = 2; // started event

    let announce_options = AnnounceOptions {
        torrent_meta: torrent_meta.clone(),
        peer_id: *peer_id,
        port,
        uploaded,
        downloaded,
        left,
        event,
    };

    tracker.announce(&announce_options).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_udp_url() {
        let tracker = UdpTracker::new("udp://tracker.example.com:8080/announce".to_string());
        let result = tracker.parse_udp_url();
        assert!(result.is_ok());
    }

    #[test]
    fn test_invalid_udp_url() {
        let tracker = UdpTracker::new("http://tracker.example.com:8080/announce".to_string());
        let result = tracker.parse_udp_url();
        assert!(result.is_err());
    }
}