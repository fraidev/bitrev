use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use thiserror::Error;
use tokio::net::{lookup_host, UdpSocket};
use tokio::time::{timeout, Instant};
use tracing::debug;

use crate::file::{AnnounceEvent, AnnounceParams, TorrentMeta};

const PROTOCOL_ID: u64 = 0x0417_2710_1980;
const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;

const CONNECTION_TTL: Duration = Duration::from_secs(60);
const TIMEOUT_BASE_SECS: u64 = 15;
const DEFAULT_MAX_CONNECT_N: u32 = 3;
const DEFAULT_MAX_ANNOUNCE_N: u32 = 3;
const DEFAULT_MAX_TOTAL_WAIT: Duration = Duration::from_secs(120);
const ANNOUNCE_REQUEST_LEN: usize = 98;
const CONNECT_REQUEST_LEN: usize = 16;
const CONNECT_RESPONSE_LEN: usize = 16;
const ANNOUNCE_RESPONSE_HEADER_LEN: usize = 20;
const ERROR_HEADER_LEN: usize = 8;
const IPV4_PEER_LEN: usize = 6;
const IPV6_PEER_LEN: usize = 18;

#[derive(Debug, Error)]
pub enum UdpTrackerError {
    #[error("tracker error: {0}")]
    Message(String),
    #[error("UDP tracker timed out")]
    Timeout,
    #[error("invalid UDP tracker URL: {0}")]
    InvalidUrl(String),
    #[error("could not resolve UDP tracker address: {0}")]
    Resolve(String),
    #[error("UDP tracker I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy)]
pub struct AnnounceLimits {
    pub max_connect_n: u32,
    pub max_announce_n: u32,
    pub max_total_wait: Duration,
}

impl AnnounceLimits {
    pub const DEFAULT: Self = Self {
        max_connect_n: DEFAULT_MAX_CONNECT_N,
        max_announce_n: DEFAULT_MAX_ANNOUNCE_N,
        max_total_wait: DEFAULT_MAX_TOTAL_WAIT,
    };

    pub const STOPPED: Self = Self {
        max_connect_n: 0,
        max_announce_n: 0,
        max_total_wait: Duration::from_secs(5),
    };
}

pub struct UdpTracker {
    url: String,
    socket: Option<UdpSocket>,
    connection: Option<CachedConnection>,
    resolved: Option<SocketAddr>,
}

struct CachedConnection {
    id: u64,
    obtained_at: Instant,
    addr: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpAnnounceResponse {
    pub interval: u32,
    pub leechers: u32,
    pub seeders: u32,
    pub peers: Vec<SocketAddr>,
}

#[derive(Debug)]
enum Packet<T> {
    Valid(T),
    Error(String),
    Ignore,
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

pub fn is_udp_url(url: &str) -> bool {
    url.get(..6)
        .is_some_and(|s| s.eq_ignore_ascii_case("udp://"))
}

impl UdpTracker {
    pub fn new(url: String) -> Self {
        Self {
            url,
            socket: None,
            connection: None,
            resolved: None,
        }
    }

    pub async fn announce(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        params: &AnnounceParams,
    ) -> Result<UdpAnnounceResponse, UdpTrackerError> {
        self.announce_with_limits(info_hash, peer_id, params, AnnounceLimits::DEFAULT)
            .await
    }

    pub async fn announce_with_limits(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        params: &AnnounceParams,
        limits: AnnounceLimits,
    ) -> Result<UdpAnnounceResponse, UdpTrackerError> {
        let deadline = Instant::now() + limits.max_total_wait;
        let addr = resolve_udp_url(&self.url, self.resolved).await?;
        self.ensure_transport(addr).await?;
        let ipv6 = addr.is_ipv6();

        let mut n = 0u32;
        loop {
            if remaining_until(deadline).is_zero() {
                return Err(UdpTrackerError::Timeout);
            }

            let connection_id = self
                .obtain_connection(deadline, limits.max_connect_n)
                .await?;
            let wait = timeout_for(n).min(remaining_until(deadline));
            if wait.is_zero() {
                return Err(UdpTrackerError::Timeout);
            }

            let transaction_id = new_transaction_id();
            let request = build_announce_request(&AnnounceRequest {
                connection_id,
                transaction_id,
                info_hash: *info_hash,
                peer_id: *peer_id,
                downloaded: params.downloaded,
                left: params.left,
                uploaded: params.uploaded,
                event: udp_event(params.event),
                ip: 0,
                key: params.key,
                num_want: udp_num_want(params.numwant),
                port: params.port,
            });

            debug!(tracker = %self.url, addr = %addr, "sending UDP announce");
            self.socket().send(&request).await?;

            match recv_packet(self.socket(), wait, |data| {
                parse_announce_response(data, transaction_id, ipv6)
            })
            .await
            {
                Ok(response) => return Ok(response),
                Err(UdpTrackerError::Timeout) => {
                    if n >= limits.max_announce_n {
                        return Err(UdpTrackerError::Timeout);
                    }
                    n += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn obtain_connection(
        &mut self,
        deadline: Instant,
        max_n: u32,
    ) -> Result<u64, UdpTrackerError> {
        if let Some(id) = self.cached_connection_id() {
            return Ok(id);
        }

        let mut n = 0u32;
        loop {
            let wait = timeout_for(n).min(remaining_until(deadline));
            if wait.is_zero() {
                return Err(UdpTrackerError::Timeout);
            }

            let transaction_id = new_transaction_id();
            let request = build_connect_request(transaction_id);
            debug!(tracker = %self.url, "sending UDP connect");
            self.socket().send(&request).await?;

            match recv_packet(self.socket(), wait, |data| {
                parse_connect_response(data, transaction_id)
            })
            .await
            {
                Ok(id) => {
                    let addr = self.resolved.expect("resolved after ensure_transport");
                    self.connection = Some(CachedConnection {
                        id,
                        obtained_at: Instant::now(),
                        addr,
                    });
                    debug!(tracker = %self.url, connection_id = id, "UDP tracker connected");
                    return Ok(id);
                }
                Err(UdpTrackerError::Timeout) => {
                    if n >= max_n {
                        return Err(UdpTrackerError::Timeout);
                    }
                    n += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn cached_connection_id(&self) -> Option<u64> {
        let cached = self.connection.as_ref()?;
        let addr = self.resolved?;
        if cached.addr == addr && cached.obtained_at.elapsed() < CONNECTION_TTL {
            Some(cached.id)
        } else {
            None
        }
    }

    async fn ensure_transport(&mut self, addr: SocketAddr) -> Result<(), UdpTrackerError> {
        let family_changed = self
            .resolved
            .is_some_and(|prev| prev.is_ipv6() != addr.is_ipv6());
        if self.socket.is_none() || family_changed {
            let bind_addr = unspecified_bind_addr(addr);
            let socket = UdpSocket::bind(bind_addr).await?;
            socket.connect(addr).await?;
            self.socket = Some(socket);
            self.connection = None;
            self.resolved = Some(addr);
            return Ok(());
        }

        if self.resolved != Some(addr) {
            self.socket().connect(addr).await?;
            self.connection = None;
            self.resolved = Some(addr);
        }
        Ok(())
    }

    fn socket(&self) -> &UdpSocket {
        self.socket
            .as_ref()
            .expect("UDP socket exists after ensure_transport")
    }
}

pub async fn request_udp_peers(
    tracker_url: &str,
    torrent_meta: &TorrentMeta,
    peer_id: &[u8; 20],
    port: u16,
) -> Result<UdpAnnounceResponse, UdpTrackerError> {
    let mut tracker = UdpTracker::new(tracker_url.to_string());
    let params = AnnounceParams {
        uploaded: 0,
        downloaded: 0,
        left: torrent_left_bytes(torrent_meta),
        port,
        event: Some(AnnounceEvent::Started),
        numwant: AnnounceParams::DEFAULT_NUMWANT,
        key: rand::Rng::gen(&mut rand::thread_rng()),
        tracker_id: None,
    };
    tracker
        .announce(&torrent_meta.info_hash, peer_id, &params)
        .await
}

fn torrent_left_bytes(torrent_meta: &TorrentMeta) -> u64 {
    if let Some(length) = torrent_meta.torrent_file.info.length {
        return length.max(0) as u64;
    }
    torrent_meta
        .torrent_file
        .info
        .files
        .as_ref()
        .map(|files| files.iter().map(|file| file.length.max(0) as u64).sum())
        .unwrap_or(0)
}

async fn resolve_udp_url(
    url: &str,
    preferred: Option<SocketAddr>,
) -> Result<SocketAddr, UdpTrackerError> {
    let host_port = parse_udp_host_port(url)?;
    let addrs: Vec<SocketAddr> = lookup_host(host_port)
        .await
        .map_err(|e| UdpTrackerError::Resolve(format!("{host_port}: {e}")))?
        .collect();
    if let Some(prev) = preferred {
        if addrs.contains(&prev) {
            return Ok(prev);
        }
    }
    addrs
        .into_iter()
        .min_by_key(|addr| addr.is_ipv6())
        .ok_or_else(|| UdpTrackerError::Resolve(host_port.to_string()))
}

fn parse_udp_host_port(url: &str) -> Result<&str, UdpTrackerError> {
    let rest = url
        .get(6..)
        .filter(|_| is_udp_url(url))
        .ok_or_else(|| UdpTrackerError::InvalidUrl(url.to_string()))?;
    let host_port = rest.split('/').next().unwrap_or_default();
    if host_port.is_empty() {
        return Err(UdpTrackerError::InvalidUrl(url.to_string()));
    }
    Ok(host_port)
}

fn unspecified_bind_addr(peer: SocketAddr) -> SocketAddr {
    if peer.is_ipv6() {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    }
}

fn new_transaction_id() -> u32 {
    rand::Rng::gen(&mut rand::thread_rng())
}

fn udp_event(event: Option<AnnounceEvent>) -> u32 {
    event.map(AnnounceEvent::as_udp).unwrap_or(0)
}

fn udp_num_want(numwant: u32) -> i32 {
    i32::try_from(numwant).unwrap_or(i32::MAX)
}

fn timeout_for(n: u32) -> Duration {
    let shift = n.min(8);
    Duration::from_secs(TIMEOUT_BASE_SECS.saturating_mul(1u64 << shift))
}

fn remaining_until(deadline: Instant) -> Duration {
    deadline
        .checked_duration_since(Instant::now())
        .unwrap_or_default()
}

async fn recv_packet<T>(
    socket: &UdpSocket,
    wait: Duration,
    parse: impl Fn(&[u8]) -> Packet<T>,
) -> Result<T, UdpTrackerError> {
    let deadline = Instant::now() + wait;
    let mut buf = vec![0u8; 65535];
    loop {
        let remaining = remaining_until(deadline);
        if remaining.is_zero() {
            return Err(UdpTrackerError::Timeout);
        }
        match timeout(remaining, socket.recv(&mut buf)).await {
            Err(_) => return Err(UdpTrackerError::Timeout),
            Ok(Err(err)) => return Err(err.into()),
            Ok(Ok(len)) => match parse(&buf[..len]) {
                Packet::Valid(value) => return Ok(value),
                Packet::Error(message) => return Err(UdpTrackerError::Message(message)),
                Packet::Ignore => {}
            },
        }
    }
}

fn build_connect_request(transaction_id: u32) -> [u8; CONNECT_REQUEST_LEN] {
    let mut request = [0u8; CONNECT_REQUEST_LEN];
    request[0..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    request[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    request[12..16].copy_from_slice(&transaction_id.to_be_bytes());
    request
}

fn build_announce_request(req: &AnnounceRequest) -> [u8; ANNOUNCE_REQUEST_LEN] {
    let mut request = [0u8; ANNOUNCE_REQUEST_LEN];
    request[0..8].copy_from_slice(&req.connection_id.to_be_bytes());
    request[8..12].copy_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    request[12..16].copy_from_slice(&req.transaction_id.to_be_bytes());
    request[16..36].copy_from_slice(&req.info_hash);
    request[36..56].copy_from_slice(&req.peer_id);
    request[56..64].copy_from_slice(&req.downloaded.to_be_bytes());
    request[64..72].copy_from_slice(&req.left.to_be_bytes());
    request[72..80].copy_from_slice(&req.uploaded.to_be_bytes());
    request[80..84].copy_from_slice(&req.event.to_be_bytes());
    request[84..88].copy_from_slice(&req.ip.to_be_bytes());
    request[88..92].copy_from_slice(&req.key.to_be_bytes());
    request[92..96].copy_from_slice(&req.num_want.to_be_bytes());
    request[96..98].copy_from_slice(&req.port.to_be_bytes());
    request
}

fn parse_connect_response(data: &[u8], expected_transaction_id: u32) -> Packet<u64> {
    match classify_header(
        data,
        expected_transaction_id,
        ACTION_CONNECT,
        CONNECT_RESPONSE_LEN,
    ) {
        Header::Ignore => Packet::Ignore,
        Header::Error(message) => Packet::Error(message),
        Header::Ok => match read_u64(data, 8) {
            Some(connection_id) => Packet::Valid(connection_id),
            None => Packet::Ignore,
        },
    }
}

fn parse_announce_response(
    data: &[u8],
    expected_transaction_id: u32,
    ipv6: bool,
) -> Packet<UdpAnnounceResponse> {
    match classify_header(
        data,
        expected_transaction_id,
        ACTION_ANNOUNCE,
        ANNOUNCE_RESPONSE_HEADER_LEN,
    ) {
        Header::Ignore => Packet::Ignore,
        Header::Error(message) => Packet::Error(message),
        Header::Ok => parse_announce_body(data, ipv6),
    }
}

fn parse_announce_body(data: &[u8], ipv6: bool) -> Packet<UdpAnnounceResponse> {
    let Some(interval) = read_u32(data, 8) else {
        return Packet::Ignore;
    };
    let Some(leechers) = read_u32(data, 12) else {
        return Packet::Ignore;
    };
    let Some(seeders) = read_u32(data, 16) else {
        return Packet::Ignore;
    };
    let Some(peers) = parse_peers(&data[ANNOUNCE_RESPONSE_HEADER_LEN..], ipv6) else {
        return Packet::Ignore;
    };
    Packet::Valid(UdpAnnounceResponse {
        interval,
        leechers,
        seeders,
        peers,
    })
}

fn parse_peers(rest: &[u8], ipv6: bool) -> Option<Vec<SocketAddr>> {
    let entry_len = if ipv6 { IPV6_PEER_LEN } else { IPV4_PEER_LEN };
    if !rest.len().is_multiple_of(entry_len) {
        return None;
    }
    let mut peers = Vec::with_capacity(rest.len() / entry_len);
    for chunk in rest.chunks_exact(entry_len) {
        peers.push(if ipv6 {
            parse_ipv6_peer(chunk)?
        } else {
            parse_ipv4_peer(chunk)?
        });
    }
    Some(peers)
}

fn parse_ipv4_peer(chunk: &[u8]) -> Option<SocketAddr> {
    if chunk.len() != IPV4_PEER_LEN {
        return None;
    }
    let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
    let port = u16::from_be_bytes([chunk[4], chunk[5]]);
    Some(SocketAddr::from((ip, port)))
}

fn parse_ipv6_peer(chunk: &[u8]) -> Option<SocketAddr> {
    if chunk.len() != IPV6_PEER_LEN {
        return None;
    }
    let mut octets = [0u8; 16];
    octets.copy_from_slice(&chunk[..16]);
    let port = u16::from_be_bytes([chunk[16], chunk[17]]);
    Some(SocketAddr::from((Ipv6Addr::from(octets), port)))
}

enum Header {
    Ok,
    Error(String),
    Ignore,
}

fn classify_header(
    data: &[u8],
    expected_transaction_id: u32,
    expected_action: u32,
    min_ok_len: usize,
) -> Header {
    if data.len() < ERROR_HEADER_LEN {
        return Header::Ignore;
    }
    let Some(action) = read_u32(data, 0) else {
        return Header::Ignore;
    };
    let Some(transaction_id) = read_u32(data, 4) else {
        return Header::Ignore;
    };
    if transaction_id != expected_transaction_id {
        return Header::Ignore;
    }
    if action == ACTION_ERROR {
        let message = String::from_utf8_lossy(&data[ERROR_HEADER_LEN..]).into_owned();
        return Header::Error(message);
    }
    if action != expected_action || data.len() < min_ok_len {
        return Header::Ignore;
    }
    Header::Ok
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    data.get(offset..offset + 4)?
        .try_into()
        .ok()
        .map(u32::from_be_bytes)
}

fn read_u64(data: &[u8], offset: usize) -> Option<u64> {
    data.get(offset..offset + 8)?
        .try_into()
        .ok()
        .map(u64::from_be_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, SocketAddrV6};
    use tokio::sync::mpsc;

    const INFO_HASH: [u8; 20] = [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
    ];
    const PEER_ID: [u8; 20] = [
        20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1,
    ];
    const ANNOUNCE_KEY: u32 = 0xDEAD_BEEF;
    const FIXED_CONNECTION_ID: u64 = 0x1111_1111_1111_1111;
    const FIXED_TRANSACTION_ID: u32 = 0xAABB_CCDD;

    fn sample_params(event: Option<AnnounceEvent>) -> AnnounceParams {
        AnnounceParams {
            uploaded: 3,
            downloaded: 1,
            left: 2,
            port: 6881,
            event,
            numwant: AnnounceParams::DEFAULT_NUMWANT,
            key: ANNOUNCE_KEY,
            tracker_id: None,
        }
    }

    fn fixture_announce_request(num_want: i32) -> AnnounceRequest {
        AnnounceRequest {
            connection_id: FIXED_CONNECTION_ID,
            transaction_id: FIXED_TRANSACTION_ID,
            info_hash: INFO_HASH,
            peer_id: PEER_ID,
            downloaded: 1,
            left: 2,
            uploaded: 3,
            event: 2,
            ip: 0,
            key: ANNOUNCE_KEY,
            num_want,
            port: 6881,
        }
    }

    fn connect_response_bytes(transaction_id: u32, connection_id: u64) -> [u8; 16] {
        let mut data = [0u8; 16];
        data[0..4].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
        data[4..8].copy_from_slice(&transaction_id.to_be_bytes());
        data[8..16].copy_from_slice(&connection_id.to_be_bytes());
        data
    }

    fn announce_response_v4(
        transaction_id: u32,
        interval: u32,
        leechers: u32,
        seeders: u32,
        peers: &[(Ipv4Addr, u16)],
    ) -> Vec<u8> {
        let mut data =
            Vec::with_capacity(ANNOUNCE_RESPONSE_HEADER_LEN + peers.len() * IPV4_PEER_LEN);
        data.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
        data.extend_from_slice(&transaction_id.to_be_bytes());
        data.extend_from_slice(&interval.to_be_bytes());
        data.extend_from_slice(&leechers.to_be_bytes());
        data.extend_from_slice(&seeders.to_be_bytes());
        for (ip, port) in peers {
            data.extend_from_slice(&ip.octets());
            data.extend_from_slice(&port.to_be_bytes());
        }
        data
    }

    fn announce_response_v6(
        transaction_id: u32,
        interval: u32,
        leechers: u32,
        seeders: u32,
        peers: &[(Ipv6Addr, u16)],
    ) -> Vec<u8> {
        let mut data =
            Vec::with_capacity(ANNOUNCE_RESPONSE_HEADER_LEN + peers.len() * IPV6_PEER_LEN);
        data.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
        data.extend_from_slice(&transaction_id.to_be_bytes());
        data.extend_from_slice(&interval.to_be_bytes());
        data.extend_from_slice(&leechers.to_be_bytes());
        data.extend_from_slice(&seeders.to_be_bytes());
        for (ip, port) in peers {
            data.extend_from_slice(&ip.octets());
            data.extend_from_slice(&port.to_be_bytes());
        }
        data
    }

    fn error_response_bytes(transaction_id: u32, message: &str) -> Vec<u8> {
        let mut data = Vec::with_capacity(ERROR_HEADER_LEN + message.len());
        data.extend_from_slice(&ACTION_ERROR.to_be_bytes());
        data.extend_from_slice(&transaction_id.to_be_bytes());
        data.extend_from_slice(message.as_bytes());
        data
    }

    fn read_u32_at(data: &[u8], offset: usize) -> u32 {
        u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
    }

    fn read_u64_at(data: &[u8], offset: usize) -> u64 {
        u64::from_be_bytes(data[offset..offset + 8].try_into().unwrap())
    }

    fn assert_connect_request(data: &[u8]) -> u32 {
        assert_eq!(data.len(), CONNECT_REQUEST_LEN);
        assert_eq!(&data[0..8], &PROTOCOL_ID.to_be_bytes());
        assert_eq!(read_u32_at(data, 8), ACTION_CONNECT);
        read_u32_at(data, 12)
    }

    fn assert_announce_request(data: &[u8], connection_id: u64, params: &AnnounceParams) -> u32 {
        assert_eq!(data.len(), ANNOUNCE_REQUEST_LEN);
        assert_eq!(read_u64_at(data, 0), connection_id);
        assert_eq!(read_u32_at(data, 8), ACTION_ANNOUNCE);
        assert_eq!(&data[16..36], &INFO_HASH);
        assert_eq!(&data[36..56], &PEER_ID);
        assert_eq!(&data[56..64], &params.downloaded.to_be_bytes());
        assert_eq!(&data[64..72], &params.left.to_be_bytes());
        assert_eq!(&data[72..80], &params.uploaded.to_be_bytes());
        assert_eq!(&data[80..84], &udp_event(params.event).to_be_bytes());
        assert_eq!(&data[84..88], &0u32.to_be_bytes());
        assert_eq!(&data[88..92], &params.key.to_be_bytes());
        assert_eq!(&data[92..96], &udp_num_want(params.numwant).to_be_bytes());
        assert_eq!(&data[96..98], &params.port.to_be_bytes());
        read_u32_at(data, 12)
    }

    async fn bind_mock(addr: SocketAddr) -> UdpSocket {
        UdpSocket::bind(addr).await.expect("bind mock tracker")
    }

    fn tracker_url(addr: SocketAddr) -> String {
        match addr.ip() {
            IpAddr::V4(ip) => format!("udp://{ip}:{}/announce", addr.port()),
            IpAddr::V6(ip) => format!("udp://[{ip}]:{}/announce", addr.port()),
        }
    }

    async fn recv_from_mock(socket: &UdpSocket) -> (Vec<u8>, SocketAddr) {
        let mut buf = vec![0u8; 512];
        let (len, from) = socket.recv_from(&mut buf).await.expect("mock recv");
        (buf[..len].to_vec(), from)
    }

    #[test]
    fn test_build_connect_request() {
        let request = build_connect_request(0x0102_0304);
        let expected = [
            0x00, 0x00, 0x04, 0x17, 0x27, 0x10, 0x19, 0x80, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02,
            0x03, 0x04,
        ];
        assert_eq!(request, expected);
    }

    #[test]
    fn test_build_announce_request() {
        let request = build_announce_request(&fixture_announce_request(-1));
        let expected = [
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x00, 0x00, 0x00, 0x01, 0xaa, 0xbb,
            0xcc, 0xdd, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 20,
            19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
            0x00, 0xde, 0xad, 0xbe, 0xef, 0xff, 0xff, 0xff, 0xff, 0x1a, 0xe1,
        ];
        assert_eq!(request.len(), ANNOUNCE_REQUEST_LEN);
        assert_eq!(request, expected);
        assert_eq!(&request[96..], &[0x1a, 0xe1]);
    }

    #[test]
    fn test_build_announce_request_http_lifecycle_numwant() {
        let request = build_announce_request(&fixture_announce_request(50));
        assert_eq!(&request[92..96], &50i32.to_be_bytes());
        assert_eq!(request.len(), ANNOUNCE_REQUEST_LEN);
    }

    #[test]
    fn test_parse_connect_response() {
        let data = connect_response_bytes(0x0102_0304, 0xFEDC_BA98_7654_3210);
        match parse_connect_response(&data, 0x0102_0304) {
            Packet::Valid(connection_id) => assert_eq!(connection_id, 0xFEDC_BA98_7654_3210),
            other => panic!("expected valid connect response, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_announce_response_ipv4() {
        let data = announce_response_v4(
            FIXED_TRANSACTION_ID,
            1800,
            3,
            7,
            &[(Ipv4Addr::new(127, 0, 0, 1), 6881)],
        );
        match parse_announce_response(&data, FIXED_TRANSACTION_ID, false) {
            Packet::Valid(response) => {
                assert_eq!(response.interval, 1800);
                assert_eq!(response.leechers, 3);
                assert_eq!(response.seeders, 7);
                assert_eq!(
                    response.peers,
                    vec![SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 6881))]
                );
            }
            other => panic!("expected valid announce response, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_announce_response_ipv6() {
        let ip = Ipv6Addr::LOCALHOST;
        let data = announce_response_v6(FIXED_TRANSACTION_ID, 900, 1, 2, &[(ip, 51413)]);
        match parse_announce_response(&data, FIXED_TRANSACTION_ID, true) {
            Packet::Valid(response) => {
                assert_eq!(response.interval, 900);
                assert_eq!(response.leechers, 1);
                assert_eq!(response.seeders, 2);
                assert_eq!(response.peers, vec![SocketAddr::from((ip, 51413))]);
            }
            other => panic!("expected valid IPv6 announce response, got {other:?}"),
        }
    }

    #[test]
    fn test_short_packets_are_ignored() {
        assert!(matches!(
            parse_connect_response(&[0u8; 15], 1),
            Packet::Ignore
        ));
        assert!(matches!(
            parse_announce_response(&[0u8; 19], 1, false),
            Packet::Ignore
        ));
        assert!(matches!(
            parse_announce_response(&[0u8; 7], 1, false),
            Packet::Ignore
        ));
    }

    #[test]
    fn test_wrong_transaction_id_is_ignored() {
        let connect = connect_response_bytes(0x0102_0304, 1);
        assert!(matches!(
            parse_connect_response(&connect, 0xDEAD_BEEF),
            Packet::Ignore
        ));

        let announce = announce_response_v4(FIXED_TRANSACTION_ID, 1, 0, 0, &[]);
        assert!(matches!(
            parse_announce_response(&announce, 0x0102_0304, false),
            Packet::Ignore
        ));
    }

    #[test]
    fn test_wrong_action_is_ignored() {
        let connect = connect_response_bytes(1, 1);
        assert!(matches!(
            parse_announce_response(&connect, 1, false),
            Packet::Ignore
        ));
    }

    #[test]
    fn test_misaligned_peers_are_ignored() {
        let mut data = announce_response_v4(1, 1800, 0, 0, &[(Ipv4Addr::LOCALHOST, 1)]);
        data.pop();
        assert!(matches!(
            parse_announce_response(&data, 1, false),
            Packet::Ignore
        ));

        let mut v6 = announce_response_v6(1, 1800, 0, 0, &[(Ipv6Addr::LOCALHOST, 1)]);
        v6.push(0);
        assert!(matches!(
            parse_announce_response(&v6, 1, true),
            Packet::Ignore
        ));
    }

    #[test]
    fn test_error_action() {
        let data = error_response_bytes(0x0102_0304, "banned by tracker");
        match parse_connect_response(&data, 0x0102_0304) {
            Packet::Error(message) => assert_eq!(message, "banned by tracker"),
            other => panic!("expected connect error, got {other:?}"),
        }
        match parse_announce_response(&data, 0x0102_0304, false) {
            Packet::Error(message) => assert_eq!(message, "banned by tracker"),
            other => panic!("expected announce error, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_udp_host_port() {
        assert_eq!(
            parse_udp_host_port("udp://tracker.example:1337/announce").unwrap(),
            "tracker.example:1337"
        );
        assert_eq!(
            parse_udp_host_port("UDP://[::1]:1337/announce").unwrap(),
            "[::1]:1337"
        );
        assert!(parse_udp_host_port("http://tracker.example:80/announce").is_err());
        assert!(parse_udp_host_port("udp://").is_err());
        assert!(is_udp_url("udp://127.0.0.1:1/announce"));
        assert!(is_udp_url("UDP://127.0.0.1:1/announce"));
        assert!(!is_udp_url("http://127.0.0.1:1/announce"));
    }

    #[test]
    fn test_timeout_schedule() {
        assert_eq!(timeout_for(0), Duration::from_secs(15));
        assert_eq!(timeout_for(1), Duration::from_secs(30));
        assert_eq!(timeout_for(2), Duration::from_secs(60));
        assert_eq!(timeout_for(3), Duration::from_secs(120));
    }

    #[test]
    fn test_udp_event_and_num_want() {
        assert_eq!(udp_event(None), 0);
        assert_eq!(udp_event(Some(AnnounceEvent::Completed)), 1);
        assert_eq!(udp_event(Some(AnnounceEvent::Started)), 2);
        assert_eq!(udp_event(Some(AnnounceEvent::Stopped)), 3);
        assert_eq!(udp_num_want(50), 50);
        assert_eq!(udp_num_want(0), 0);
    }

    #[tokio::test]
    async fn announce_round_trip_peers_and_interval() {
        let mock = bind_mock(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await;
        let url = tracker_url(mock.local_addr().unwrap());
        let params = sample_params(Some(AnnounceEvent::Started));
        let client = tokio::spawn(async move {
            let mut tracker = UdpTracker::new(url);
            tracker.announce(&INFO_HASH, &PEER_ID, &params).await
        });

        let (connect, from) = recv_from_mock(&mock).await;
        let connect_tid = assert_connect_request(&connect);
        mock.send_to(
            &connect_response_bytes(connect_tid, FIXED_CONNECTION_ID),
            from,
        )
        .await
        .unwrap();

        let (announce, from) = recv_from_mock(&mock).await;
        let announce_tid = assert_announce_request(
            &announce,
            FIXED_CONNECTION_ID,
            &sample_params(Some(AnnounceEvent::Started)),
        );
        let response = announce_response_v4(
            announce_tid,
            1800,
            3,
            7,
            &[(Ipv4Addr::new(127, 0, 0, 1), 6881)],
        );
        mock.send_to(&response, from).await.unwrap();

        let got = client.await.unwrap().expect("announce succeeded");
        assert_eq!(got.interval, 1800);
        assert_eq!(got.leechers, 3);
        assert_eq!(got.seeders, 7);
        assert_eq!(
            got.peers,
            vec![SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 6881))]
        );
    }

    #[tokio::test]
    async fn transaction_mismatch_is_ignored_until_matching_packet() {
        let mock = bind_mock(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await;
        let url = tracker_url(mock.local_addr().unwrap());
        let params = sample_params(None);
        let client_params = params.clone();
        let client = tokio::spawn(async move {
            let mut tracker = UdpTracker::new(url);
            tracker.announce(&INFO_HASH, &PEER_ID, &client_params).await
        });

        let (connect, from) = recv_from_mock(&mock).await;
        let connect_tid = assert_connect_request(&connect);
        mock.send_to(
            &connect_response_bytes(connect_tid.wrapping_add(1), FIXED_CONNECTION_ID),
            from,
        )
        .await
        .unwrap();
        mock.send_to(
            &connect_response_bytes(connect_tid, FIXED_CONNECTION_ID),
            from,
        )
        .await
        .unwrap();

        let (announce, from) = recv_from_mock(&mock).await;
        let announce_tid = assert_announce_request(&announce, FIXED_CONNECTION_ID, &params);
        mock.send_to(
            &announce_response_v4(announce_tid.wrapping_add(1), 60, 0, 0, &[]),
            from,
        )
        .await
        .unwrap();
        mock.send_to(
            &announce_response_v4(
                announce_tid,
                60,
                0,
                1,
                &[(Ipv4Addr::new(10, 0, 0, 2), 51413)],
            ),
            from,
        )
        .await
        .unwrap();

        let got = client.await.unwrap().expect("announce succeeded");
        assert_eq!(got.interval, 60);
        assert_eq!(got.seeders, 1);
        assert_eq!(
            got.peers,
            vec![SocketAddr::from((Ipv4Addr::new(10, 0, 0, 2), 51413))]
        );
    }

    #[tokio::test]
    async fn error_packet_is_typed_error() {
        let mock = bind_mock(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await;
        let url = tracker_url(mock.local_addr().unwrap());
        let params = sample_params(Some(AnnounceEvent::Started));
        let client_params = params.clone();
        let client = tokio::spawn(async move {
            let mut tracker = UdpTracker::new(url);
            tracker.announce(&INFO_HASH, &PEER_ID, &client_params).await
        });

        let (connect, from) = recv_from_mock(&mock).await;
        let connect_tid = assert_connect_request(&connect);
        mock.send_to(
            &connect_response_bytes(connect_tid, FIXED_CONNECTION_ID),
            from,
        )
        .await
        .unwrap();

        let (announce, from) = recv_from_mock(&mock).await;
        let announce_tid = assert_announce_request(&announce, FIXED_CONNECTION_ID, &params);
        mock.send_to(
            &error_response_bytes(announce_tid, "banned by tracker"),
            from,
        )
        .await
        .unwrap();

        match client.await.unwrap() {
            Err(UdpTrackerError::Message(message)) => {
                assert_eq!(message, "banned by tracker");
            }
            other => panic!("expected tracker message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn expired_connection_id_reconnects() {
        tokio::time::pause();
        let mock = bind_mock(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await;
        let url = tracker_url(mock.local_addr().unwrap());
        let params = sample_params(None);
        let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(UdpTracker::new(url)));

        let first = {
            let tracker = tracker.clone();
            let params = params.clone();
            tokio::spawn(async move {
                tracker
                    .lock()
                    .await
                    .announce(&INFO_HASH, &PEER_ID, &params)
                    .await
            })
        };

        let (connect, from) = recv_from_mock(&mock).await;
        let connect_tid = assert_connect_request(&connect);
        mock.send_to(&connect_response_bytes(connect_tid, 1), from)
            .await
            .unwrap();
        let (announce, from) = recv_from_mock(&mock).await;
        let announce_tid = assert_announce_request(&announce, 1, &params);
        mock.send_to(&announce_response_v4(announce_tid, 1800, 0, 0, &[]), from)
            .await
            .unwrap();
        first.await.unwrap().expect("first announce");

        tokio::time::advance(Duration::from_secs(61)).await;

        let second = {
            let tracker = tracker.clone();
            let params = params.clone();
            tokio::spawn(async move {
                tracker
                    .lock()
                    .await
                    .announce(&INFO_HASH, &PEER_ID, &params)
                    .await
            })
        };

        let (connect, from) = recv_from_mock(&mock).await;
        let connect_tid = assert_connect_request(&connect);
        mock.send_to(&connect_response_bytes(connect_tid, 2), from)
            .await
            .unwrap();

        let (stale, from) = recv_from_mock(&mock).await;
        let stale_cid = read_u64_at(&stale, 0);
        assert_eq!(
            stale_cid, 2,
            "client must announce with the fresh connection id"
        );
        let announce_tid = assert_announce_request(&stale, 2, &params);
        mock.send_to(&announce_response_v4(announce_tid, 60, 0, 0, &[]), from)
            .await
            .unwrap();
        second.await.unwrap().expect("second announce");
    }

    #[tokio::test]
    async fn server_rejects_old_connection_id_then_client_reconnects() {
        tokio::time::pause();
        let mock = bind_mock(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await;
        let url = tracker_url(mock.local_addr().unwrap());
        let params = sample_params(None);
        let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(UdpTracker::new(url)));

        let first = {
            let tracker = tracker.clone();
            let params = params.clone();
            tokio::spawn(async move {
                tracker
                    .lock()
                    .await
                    .announce(&INFO_HASH, &PEER_ID, &params)
                    .await
            })
        };

        let (connect, from) = recv_from_mock(&mock).await;
        let connect_tid = assert_connect_request(&connect);
        mock.send_to(&connect_response_bytes(connect_tid, 11), from)
            .await
            .unwrap();
        let (announce, from) = recv_from_mock(&mock).await;
        let announce_tid = assert_announce_request(&announce, 11, &params);
        mock.send_to(&announce_response_v4(announce_tid, 1800, 0, 0, &[]), from)
            .await
            .unwrap();
        first.await.unwrap().expect("first announce");

        tokio::time::advance(Duration::from_secs(61)).await;

        let second = {
            let tracker = tracker.clone();
            let params = params.clone();
            tokio::spawn(async move {
                tracker
                    .lock()
                    .await
                    .announce(&INFO_HASH, &PEER_ID, &params)
                    .await
            })
        };

        let (connect, from) = recv_from_mock(&mock).await;
        let connect_tid = assert_connect_request(&connect);
        mock.send_to(&connect_response_bytes(connect_tid, 22), from)
            .await
            .unwrap();
        let (announce, from) = recv_from_mock(&mock).await;
        assert_ne!(
            read_u64_at(&announce, 0),
            11,
            "server would reject the expired connection id"
        );
        let announce_tid = assert_announce_request(&announce, 22, &params);
        mock.send_to(&announce_response_v4(announce_tid, 90, 1, 1, &[]), from)
            .await
            .unwrap();
        let got = second.await.unwrap().expect("reconnected announce");
        assert_eq!(got.interval, 90);
    }

    #[tokio::test]
    async fn announce_over_ipv6_parses_18_byte_peers() {
        let mock = match UdpSocket::bind(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::LOCALHOST,
            0,
            0,
            0,
        )))
        .await
        {
            Ok(socket) => socket,
            Err(_) => return,
        };
        let url = tracker_url(mock.local_addr().unwrap());
        let params = sample_params(Some(AnnounceEvent::Started));
        let client_params = params.clone();
        let client = tokio::spawn(async move {
            let mut tracker = UdpTracker::new(url);
            tracker.announce(&INFO_HASH, &PEER_ID, &client_params).await
        });

        let (connect, from) = recv_from_mock(&mock).await;
        let connect_tid = assert_connect_request(&connect);
        mock.send_to(
            &connect_response_bytes(connect_tid, FIXED_CONNECTION_ID),
            from,
        )
        .await
        .unwrap();

        let (announce, from) = recv_from_mock(&mock).await;
        let announce_tid = assert_announce_request(&announce, FIXED_CONNECTION_ID, &params);
        let peer = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        mock.send_to(
            &announce_response_v6(announce_tid, 120, 4, 5, &[(peer, 6881)]),
            from,
        )
        .await
        .unwrap();

        let got = client.await.unwrap().expect("IPv6 announce succeeded");
        assert_eq!(got.interval, 120);
        assert_eq!(got.leechers, 4);
        assert_eq!(got.seeders, 5);
        assert_eq!(got.peers, vec![SocketAddr::from((peer, 6881))]);
    }

    #[tokio::test]
    async fn retransmit_on_dropped_announce() {
        tokio::time::pause();
        let mock = bind_mock(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await;
        let url = tracker_url(mock.local_addr().unwrap());
        let params = sample_params(Some(AnnounceEvent::Started));
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        let client_params = params.clone();
        let client = tokio::spawn(async move {
            let mut tracker = UdpTracker::new(url);
            let result = tracker.announce(&INFO_HASH, &PEER_ID, &client_params).await;
            let _ = done_tx.send(());
            result
        });

        let (connect, from) = recv_from_mock(&mock).await;
        let connect_tid = assert_connect_request(&connect);
        mock.send_to(
            &connect_response_bytes(connect_tid, FIXED_CONNECTION_ID),
            from,
        )
        .await
        .unwrap();

        let (first, _from) = recv_from_mock(&mock).await;
        let first_tid = assert_announce_request(&first, FIXED_CONNECTION_ID, &params);

        tokio::time::advance(Duration::from_secs(15)).await;

        let (second, from) = recv_from_mock(&mock).await;
        let second_tid = assert_announce_request(&second, FIXED_CONNECTION_ID, &params);
        assert_ne!(
            first_tid, second_tid,
            "each request uses a fresh transaction id"
        );
        mock.send_to(
            &announce_response_v4(
                second_tid,
                1800,
                0,
                0,
                &[(Ipv4Addr::new(192, 168, 1, 10), 51413)],
            ),
            from,
        )
        .await
        .unwrap();

        let got = client.await.unwrap().expect("announce after retransmit");
        assert!(done_rx.recv().await.is_some());
        assert_eq!(got.interval, 1800);
        assert_eq!(
            got.peers,
            vec![SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 51413))]
        );
    }

    #[tokio::test]
    async fn malformed_packets_never_panic() {
        let cases: &[&[u8]] = &[
            &[],
            &[0],
            &[0; 7],
            &[0; 8],
            &[0; 15],
            &[0; 19],
            &[0; 21],
            &[0xff; 3],
        ];
        for packet in cases {
            let _ = parse_connect_response(packet, 1);
            let _ = parse_announce_response(packet, 1, false);
            let _ = parse_announce_response(packet, 1, true);
        }
    }
}
