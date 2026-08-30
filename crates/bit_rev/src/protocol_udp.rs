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
    pub key: u32,
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

        let request = build_announce_request(AnnounceRequest {
            connection_id,
            transaction_id,
            info_hash: torrent_meta.info_hash,
            peer_id: *peer_id,
            downloaded,
            left,
            uploaded,
            event,
            ip: 0,
            key: announce_options.key,
            num_want: -1,
            port,
        })?;

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

        let request = build_connect_request(transaction_id)?;

        debug!("Sending UDP connect request to {}", addr);
        socket.send_to(&request, addr).await?;

        // Receive response with timeout
        let mut buf = [0u8; 16];
        let (len, _) = timeout(Duration::from_secs(15), socket.recv_from(&mut buf)).await??;

        self.connection_id = Some(parse_connect_response(&buf[..len], transaction_id)?);
        self.last_connect = Some(Instant::now());

        debug!(
            "UDP tracker connected with connection_id: {:?}",
            self.connection_id
        );
        Ok(())
    }

    fn parse_udp_url(&self) -> Result<SocketAddr> {
        let host_port = parse_udp_host_port(&self.url)?;

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

struct AnnounceRequest {
    connection_id: u64,
    transaction_id: u32,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    downloaded: u64,
    left: u64,
    uploaded: u64,
    event: u32,
    ip: u32,
    key: u32,
    num_want: i32,
    port: u16,
}

fn build_connect_request(transaction_id: u32) -> Result<Vec<u8>> {
    let mut request = Vec::new();
    request.write_u64::<BigEndian>(PROTOCOL_ID)?;
    request.write_u32::<BigEndian>(ACTION_CONNECT)?;
    request.write_u32::<BigEndian>(transaction_id)?;
    Ok(request)
}

fn build_announce_request(req: AnnounceRequest) -> Result<Vec<u8>> {
    let mut request = Vec::new();
    request.write_u64::<BigEndian>(req.connection_id)?;
    request.write_u32::<BigEndian>(ACTION_ANNOUNCE)?;
    request.write_u32::<BigEndian>(req.transaction_id)?;
    request.write_all(&req.info_hash)?;
    request.write_all(&req.peer_id)?;
    request.write_u64::<BigEndian>(req.downloaded)?;
    request.write_u64::<BigEndian>(req.left)?;
    request.write_u64::<BigEndian>(req.uploaded)?;
    request.write_u32::<BigEndian>(req.event)?;
    request.write_u32::<BigEndian>(req.ip)?;
    request.write_u32::<BigEndian>(req.key)?;
    request.write_i32::<BigEndian>(req.num_want)?;
    request.write_u16::<BigEndian>(req.port)?;
    Ok(request)
}

fn parse_udp_host_port(url: &str) -> Result<&str> {
    if !url.starts_with("udp://") {
        return Err(anyhow!("Invalid UDP tracker URL: {}", url));
    }

    let url_without_scheme = &url[6..];
    let host_port = url_without_scheme
        .split('/')
        .next()
        .ok_or_else(|| anyhow!("Invalid UDP tracker URL format: {}", url))?;

    if host_port.is_empty() {
        return Err(anyhow!("Invalid UDP tracker URL format: {}", url));
    }

    Ok(host_port)
}

fn parse_connect_response(data: &[u8], expected_transaction_id: u32) -> Result<u64> {
    if data.len() < 16 {
        return Err(anyhow!("Connect response too short: {} bytes", data.len()));
    }

    let mut cursor = Cursor::new(data);
    let action = cursor.read_u32::<BigEndian>()?;
    let response_transaction_id = cursor.read_u32::<BigEndian>()?;

    if action == ACTION_ERROR {
        let error_msg = String::from_utf8_lossy(&data[8..]);
        return Err(anyhow!("Tracker error: {}", error_msg));
    }

    if action != ACTION_CONNECT {
        return Err(anyhow!("Invalid action in connect response: {}", action));
    }

    if response_transaction_id != expected_transaction_id {
        return Err(anyhow!("Transaction ID mismatch in connect response"));
    }

    Ok(cursor.read_u64::<BigEndian>()?)
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
        key: rand::thread_rng().gen(),
    };

    tracker.announce(&announce_options).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_udp_url() {
        let tracker = UdpTracker::new("udp://tracker.opentrackr.org:1337/announce".to_string());
        let result = tracker.parse_udp_url();
        assert!(result.is_ok());
    }

    #[test]
    fn test_invalid_udp_url() {
        let tracker = UdpTracker::new("http://tracker.example.com:8080/announce".to_string());
        let result = tracker.parse_udp_url();
        assert!(result.is_err());
    }

    #[test]
    fn test_build_connect_request() {
        let transaction_id = 0x01020304;
        let request = build_connect_request(transaction_id).unwrap();
        let expected = [
            0x00, 0x00, 0x04, 0x17, 0x27, 0x10, 0x19, 0x80, // protocol id 0x41727101980
            0x00, 0x00, 0x00, 0x00, // action connect
            0x01, 0x02, 0x03, 0x04, // transaction_id
        ];
        assert_eq!(request, expected);
    }

    #[test]
    fn test_build_announce_request() {
        let info_hash: [u8; 20] = [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ];
        let peer_id: [u8; 20] = [
            20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1,
        ];
        let request = build_announce_request(AnnounceRequest {
            connection_id: 0x1111_1111_1111_1111,
            transaction_id: 0xaabbccdd,
            info_hash,
            peer_id,
            downloaded: 1,
            left: 2,
            uploaded: 3,
            event: 2,
            ip: 0,
            key: 0xdeadbeef,
            num_want: -1,
            port: 6881,
        })
        .unwrap();

        let expected = [
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, // connection_id
            0x00, 0x00, 0x00, 0x01, // action announce
            0xaa, 0xbb, 0xcc, 0xdd, // transaction_id
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
            20, // info_hash
            20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, // peer_id
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, // downloaded
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, // left
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, // uploaded
            0x00, 0x00, 0x00, 0x02, // event started
            0x00, 0x00, 0x00, 0x00, // ip
            0xde, 0xad, 0xbe, 0xef, // key
            0xff, 0xff, 0xff, 0xff, // num_want -1
            0x1a, 0xe1, // port 6881
        ];
        assert_eq!(request.len(), 98);
        assert_eq!(request, expected);
        assert_eq!(&request[96..], &[0x1a, 0xe1]);
    }

    #[test]
    fn test_parse_connect_response() {
        let transaction_id = 0x01020304;
        let data = [
            0x00, 0x00, 0x00, 0x00, // action connect
            0x01, 0x02, 0x03, 0x04, // transaction_id
            0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10, // connection_id
        ];
        let connection_id = parse_connect_response(&data, transaction_id).unwrap();
        assert_eq!(connection_id, 0xfedcba9876543210);
    }

    #[test]
    fn test_parse_announce_response() {
        let tracker = UdpTracker::new("udp://127.0.0.1:1".into());
        let transaction_id = 0xaabbccdd;
        let data = [
            0x00, 0x00, 0x00, 0x01, // action announce
            0xaa, 0xbb, 0xcc, 0xdd, // transaction_id
            0x00, 0x00, 0x07, 0x08, // interval 1800
            0x00, 0x00, 0x00, 0x03, // leechers
            0x00, 0x00, 0x00, 0x07, // seeders
            127, 0, 0, 1, // 127.0.0.1
            0x1a, 0xe1, // port 6881
        ];
        let response = tracker
            .parse_announce_response(&data, transaction_id)
            .unwrap();
        assert_eq!(response.action, ACTION_ANNOUNCE);
        assert_eq!(response.transaction_id, transaction_id);
        assert_eq!(response.interval, 1800);
        assert_eq!(response.leechers, 3);
        assert_eq!(response.seeders, 7);
        assert_eq!(response.peers.len(), 1);
        assert_eq!(response.peers[0].ip, [127, 0, 0, 1]);
        assert_eq!(response.peers[0].port, 6881);
    }

    #[test]
    fn test_short_packets() {
        let tracker = UdpTracker::new("udp://127.0.0.1:1".into());
        let short_connect = [0u8; 15];
        let short_announce = [0u8; 19];
        assert!(parse_connect_response(&short_connect, 1).is_err());
        assert!(tracker.parse_announce_response(&short_announce, 1).is_err());
    }

    #[test]
    fn test_wrong_transaction_id() {
        let tracker = UdpTracker::new("udp://127.0.0.1:1".into());
        let connect_data = [
            0x00, 0x00, 0x00, 0x00, // action connect
            0x01, 0x02, 0x03, 0x04, // transaction_id
            0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10,
        ];
        assert!(parse_connect_response(&connect_data, 0xdeadbeef).is_err());

        let announce_data = [
            0x00, 0x00, 0x00, 0x01, // action announce
            0xaa, 0xbb, 0xcc, 0xdd, // transaction_id
            0x00, 0x00, 0x07, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert!(tracker
            .parse_announce_response(&announce_data, 0x01020304)
            .is_err());
    }

    #[test]
    fn test_error_action() {
        let tracker = UdpTracker::new("udp://127.0.0.1:1".into());
        let transaction_id = 0x01020304;
        let mut data = vec![
            0x00, 0x00, 0x00, 0x03, // action error
            0x01, 0x02, 0x03, 0x04, // transaction_id
        ];
        data.extend_from_slice(b"banned by tracker");

        let connect_err = parse_connect_response(&data, transaction_id).unwrap_err();
        assert!(connect_err.to_string().contains("banned by tracker"));

        let announce_err = tracker
            .parse_announce_response(&data, transaction_id)
            .unwrap_err();
        assert!(announce_err.to_string().contains("banned by tracker"));
    }

    #[test]
    fn test_parse_udp_host_port() {
        assert_eq!(
            parse_udp_host_port("udp://tracker.example:1337/announce").unwrap(),
            "tracker.example:1337"
        );
        assert!(parse_udp_host_port("http://tracker.example:80/announce").is_err());
        assert!(parse_udp_host_port("udp://").is_err());
    }
}
