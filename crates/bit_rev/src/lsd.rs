//! Local Peer Discovery (BEP-0014).
//!
//! Docs call this LPD. Code keeps the existing source name `Lsd`
//! (`DiscoverySource::Lsd`, `allows_lsd`). One session task joins
//! `239.192.152.143:6771` and announces non-private torrents. IPv6 group
//! `ff15::efc0:988f` is parsed but not joined until dual-stack lands.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use rand::Rng;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use tracing::debug;

pub const GROUP_V4: Ipv4Addr = Ipv4Addr::new(239, 192, 152, 143);
pub const PORT: u16 = 6771;
pub const HOST_V4: &str = "239.192.152.143:6771";
/// Site-local group from BEP-0014. Not joined until IPv6 dual-stack exists.
pub const HOST_V6: &str = "[ff15::efc0:988f]:6771";
pub const MAX_DATAGRAM: usize = 1400;
pub const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const ANNOUNCE_AFTER_ADD: Duration = Duration::from_secs(1);
/// BEP-0014: at most one announce per minute.
pub const MIN_SEND_GAP: Duration = Duration::from_secs(60);
const RECV_CAP: usize = 2048;
const INBOUND_WINDOW: Duration = Duration::from_secs(10);
const INBOUND_MAX: u32 = 8;
const INBOUND_MAX_SOURCES: usize = 1024;
const POLL: Duration = Duration::from_secs(1);

const REQUEST_LINE: &str = "BT-SEARCH * HTTP/1.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchAnnounce {
    pub host: String,
    pub port: u16,
    pub info_hashes: Vec<[u8; 20]>,
    pub cookie: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Oversize,
    Malformed,
    MissingHeader,
    BadHost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnnounceClock {
    pub first_seen: Instant,
    pub last_sent: Option<Instant>,
}

#[derive(Debug, Default)]
pub struct InboundLimiter {
    hits: HashMap<IpAddr, (Instant, u32)>,
}

impl InboundLimiter {
    pub fn allow(&mut self, ip: IpAddr, now: Instant) -> bool {
        if self.hits.len() >= INBOUND_MAX_SOURCES && !self.hits.contains_key(&ip) {
            self.hits
                .retain(|_, (start, _)| now.saturating_duration_since(*start) < INBOUND_WINDOW);
            if self.hits.len() >= INBOUND_MAX_SOURCES {
                if let Some(drop_key) = self.hits.keys().next().copied() {
                    self.hits.remove(&drop_key);
                }
            }
        }
        match self.hits.get_mut(&ip) {
            Some((start, count)) if now.saturating_duration_since(*start) < INBOUND_WINDOW => {
                if *count >= INBOUND_MAX {
                    return false;
                }
                *count = count.saturating_add(1);
                true
            }
            _ => {
                self.hits.insert(ip, (now, 1));
                true
            }
        }
    }
}

pub fn random_cookie() -> String {
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Non-private, transferring torrents. `allows_lsd` is the private gate.
pub fn announceable(members: &[([u8; 20], bool, bool)]) -> Vec<[u8; 20]> {
    members
        .iter()
        .filter(|(_, allows_lsd, active)| *allows_lsd && *active)
        .map(|(hash, _, _)| *hash)
        .collect()
}

pub fn is_due(clock: &AnnounceClock, now: Instant) -> bool {
    match clock.last_sent {
        None => now.saturating_duration_since(clock.first_seen) >= ANNOUNCE_AFTER_ADD,
        Some(sent) => now.saturating_duration_since(sent) >= ANNOUNCE_INTERVAL,
    }
}

pub fn encode_announce(port: u16, hashes: &[[u8; 20]], cookie: &str) -> Option<Vec<u8>> {
    let (bytes, included) = pack_announce(port, hashes, cookie);
    if included == hashes.len() && included > 0 {
        Some(bytes)
    } else {
        None
    }
}

/// Pack as many leading hashes as fit in [`MAX_DATAGRAM`].
pub fn pack_announce(port: u16, hashes: &[[u8; 20]], cookie: &str) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    push_line(&mut out, REQUEST_LINE);
    push_line(&mut out, &format!("Host: {HOST_V4}"));
    push_line(&mut out, &format!("Port: {port}"));
    let mut included = 0usize;
    for hash in hashes {
        let line = format!("Infohash: {}", hex_hash(hash));
        let trailer = cookie_line(cookie).len() + 4;
        let next = out.len() + line.len() + 2 + trailer;
        if next > MAX_DATAGRAM {
            break;
        }
        push_line(&mut out, &line);
        included += 1;
    }
    if included == 0 {
        return (Vec::new(), 0);
    }
    push_line(&mut out, &cookie_line(cookie));
    push_line(&mut out, "");
    (out, included)
}

pub fn decode(buf: &[u8]) -> Result<SearchAnnounce, DecodeError> {
    if buf.len() > MAX_DATAGRAM {
        return Err(DecodeError::Oversize);
    }
    let text = std::str::from_utf8(buf).map_err(|_| DecodeError::Malformed)?;
    let mut lines = text.split("\r\n");
    let request = lines.next().ok_or(DecodeError::Malformed)?;
    if request.trim() != REQUEST_LINE {
        return Err(DecodeError::Malformed);
    }

    let mut host = None;
    let mut port = None;
    let mut info_hashes = Vec::new();
    let mut cookie = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(DecodeError::Malformed);
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("Host") {
            if host.is_none() {
                host = Some(value.to_string());
            }
        } else if name.eq_ignore_ascii_case("Port") {
            if port.is_none() {
                let parsed: u16 = value.parse().map_err(|_| DecodeError::Malformed)?;
                if parsed == 0 {
                    return Err(DecodeError::Malformed);
                }
                port = Some(parsed);
            }
        } else if name.eq_ignore_ascii_case("Infohash") {
            let hash = parse_info_hash(value).ok_or(DecodeError::Malformed)?;
            if !info_hashes.contains(&hash) {
                info_hashes.push(hash);
            }
        } else if name.eq_ignore_ascii_case("Cookie") && cookie.is_none() && !value.is_empty() {
            cookie = Some(value.to_string());
        }
    }

    let host = host.ok_or(DecodeError::MissingHeader)?;
    if host != HOST_V4 && host != HOST_V6 {
        return Err(DecodeError::BadHost);
    }
    let port = port.ok_or(DecodeError::MissingHeader)?;
    if info_hashes.is_empty() {
        return Err(DecodeError::MissingHeader);
    }
    Ok(SearchAnnounce {
        host,
        port,
        info_hashes,
        cookie,
    })
}

/// Parse one datagram and report matching peers. Returns how many `add` calls
/// were made. Drops oversize input, our own cookie, and per-IP floods.
pub fn ingest(
    datagram: &[u8],
    src: SocketAddr,
    our_cookie: &str,
    limiter: &mut InboundLimiter,
    now: Instant,
    mut add: impl FnMut(&[u8; 20], SocketAddr),
) -> usize {
    if datagram.len() > MAX_DATAGRAM {
        return 0;
    }
    if !limiter.allow(src.ip(), now) {
        return 0;
    }
    let announce = match decode(datagram) {
        Ok(announce) => announce,
        Err(_) => return 0,
    };
    if announce.cookie.as_deref() == Some(our_cookie) {
        return 0;
    }
    let mut added = 0;
    for hash in &announce.info_hashes {
        let addr = SocketAddr::new(src.ip(), announce.port);
        add(hash, addr);
        added += 1;
    }
    added
}

pub fn bind_multicast() -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    // macOS delivers a multicast datagram to every member only when each
    // socket sets SO_REUSEPORT before bind.
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    let bind_addr = socket2::SockAddr::from(SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT)));
    socket.bind(&bind_addr)?;
    socket.join_multicast_v4(&GROUP_V4, &Ipv4Addr::UNSPECIFIED)?;
    socket.set_multicast_loop_v4(true)?;
    // TTL 1 stays on the local subnet. No cross-subnet relay.
    socket.set_multicast_ttl_v4(1)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

pub async fn run<H, P, D>(
    socket: UdpSocket,
    cancel: CancellationToken,
    cookie: String,
    mut hashes: H,
    mut listen_port: P,
    mut on_packet: D,
) where
    H: FnMut() -> Vec<[u8; 20]>,
    P: FnMut() -> u16,
    D: FnMut(SocketAddr, &[u8]),
{
    let dest = SocketAddr::from((GROUP_V4, PORT));
    let mut seen: HashMap<[u8; 20], AnnounceClock> = HashMap::new();
    let mut last_packet: Option<Instant> = None;
    let mut buf = vec![0u8; RECV_CAP];

    loop {
        let now = Instant::now();
        let current = hashes();
        reconcile(&mut seen, &current, now);
        let port = listen_port();
        if port != 0 {
            let due = due_hashes(&seen, now);
            let gap_open = last_packet
                .map(|sent| now.saturating_duration_since(sent) >= MIN_SEND_GAP)
                .unwrap_or(true);
            if gap_open && !due.is_empty() {
                let (packet, included) = pack_announce(port, &due, &cookie);
                if included > 0 {
                    match socket.send_to(&packet, dest).await {
                        Ok(_) => {
                            let sent_at = Instant::now();
                            for hash in due.iter().take(included) {
                                if let Some(clock) = seen.get_mut(hash) {
                                    clock.last_sent = Some(sent_at);
                                }
                            }
                            last_packet = Some(sent_at);
                        }
                        Err(err) => {
                            debug!(error = %err, "LPD announce send failed");
                        }
                    }
                }
            }
        }

        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(POLL) => {}
            received = socket.recv_from(&mut buf) => {
                match received {
                    Ok((n, src)) => on_packet(src, &buf[..n]),
                    Err(err) => {
                        debug!(error = %err, "LPD recv failed");
                    }
                }
            }
        }
    }
}

fn reconcile(seen: &mut HashMap<[u8; 20], AnnounceClock>, current: &[[u8; 20]], now: Instant) {
    seen.retain(|hash, _| current.contains(hash));
    for hash in current {
        seen.entry(*hash).or_insert(AnnounceClock {
            first_seen: now,
            last_sent: None,
        });
    }
}

fn due_hashes(seen: &HashMap<[u8; 20], AnnounceClock>, now: Instant) -> Vec<[u8; 20]> {
    let mut due: Vec<[u8; 20]> = seen
        .iter()
        .filter(|(_, clock)| is_due(clock, now))
        .map(|(hash, _)| *hash)
        .collect();
    due.sort();
    due
}

fn cookie_line(cookie: &str) -> String {
    format!("Cookie: {cookie}")
}

fn push_line(out: &mut Vec<u8>, line: &str) {
    out.extend_from_slice(line.as_bytes());
    out.extend_from_slice(b"\r\n");
}

fn hex_hash(hash: &[u8; 20]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_info_hash(hex: &str) -> Option<[u8; 20]> {
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddrV4;

    fn sample_hash(byte: u8) -> [u8; 20] {
        [byte; 20]
    }

    fn sample_packet(port: u16, hashes: &[[u8; 20]], cookie: &str) -> Vec<u8> {
        encode_announce(port, hashes, cookie).expect("packet fits")
    }

    #[test]
    fn encode_matches_bep_14_shape() {
        let hash = sample_hash(0xab);
        let bytes = sample_packet(6881, &[hash], "cafebabe01234567");
        let text = std::str::from_utf8(&bytes).unwrap();
        let hex = hex_hash(&hash);
        assert_eq!(hex.len(), 40);
        assert_eq!(
            text,
            format!(
                "\
BT-SEARCH * HTTP/1.1\r\n\
Host: {HOST_V4}\r\n\
Port: 6881\r\n\
Infohash: {hex}\r\n\
Cookie: cafebabe01234567\r\n\
\r\n"
            )
        );
    }

    #[test]
    fn parse_round_trip_and_several_infohashes() {
        let hashes = [sample_hash(1), sample_hash(2), sample_hash(3)];
        let bytes = sample_packet(51413, &hashes, "cookie");
        let parsed = decode(&bytes).unwrap();
        assert_eq!(parsed.host, HOST_V4);
        assert_eq!(parsed.port, 51413);
        assert_eq!(parsed.info_hashes, hashes);
        assert_eq!(parsed.cookie.as_deref(), Some("cookie"));
    }

    #[test]
    fn parse_accepts_header_case_and_ipv6_host() {
        let raw = "\
BT-SEARCH * HTTP/1.1\r\n\
host: [ff15::efc0:988f]:6771\r\n\
port: 6881\r\n\
infohash: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\n\
INFOHASH: BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\r\n\
cookie: mine\r\n\
Extra: ignored\r\n\
\r\n";
        let parsed = decode(raw.as_bytes()).unwrap();
        assert_eq!(parsed.host, HOST_V6);
        assert_eq!(parsed.info_hashes.len(), 2);
        assert_eq!(parsed.info_hashes[0], [0xaa; 20]);
        assert_eq!(parsed.info_hashes[1], [0xbb; 20]);
        assert_eq!(parsed.cookie.as_deref(), Some("mine"));
    }

    #[test]
    fn missing_headers_and_garbage_do_not_panic() {
        assert_eq!(decode(b""), Err(DecodeError::Malformed));
        assert_eq!(decode(b"not a search"), Err(DecodeError::Malformed));
        assert_eq!(
            decode(b"BT-SEARCH * HTTP/1.1\r\nPort: 6881\r\n\r\n"),
            Err(DecodeError::MissingHeader)
        );
        assert_eq!(
            decode(
                b"BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nInfohash: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n\r\n"
            ),
            Err(DecodeError::MissingHeader)
        );
        assert_eq!(
            decode(b"BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\n\r\n"),
            Err(DecodeError::MissingHeader)
        );
        assert_eq!(
            decode(
                b"BT-SEARCH * HTTP/1.1\r\nHost: 10.0.0.1:6771\r\nPort: 1\r\nInfohash: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n\r\n"
            ),
            Err(DecodeError::BadHost)
        );
        assert!(decode(&[0xff; 20]).is_err());
        assert_eq!(
            decode(&vec![b'x'; MAX_DATAGRAM + 1]),
            Err(DecodeError::Oversize)
        );
    }

    #[test]
    fn oversize_pack_stops_before_1400() {
        let hashes: Vec<[u8; 20]> = (0..40).map(sample_hash).collect();
        let (bytes, included) = pack_announce(6881, &hashes, "cookie");
        assert!(bytes.len() <= MAX_DATAGRAM);
        assert!(included < hashes.len());
        assert!(included > 1);
        assert!(decode(&bytes).is_ok());
    }

    #[test]
    fn private_and_inactive_are_not_announced() {
        let public = sample_hash(1);
        let private = sample_hash(2);
        let paused = sample_hash(3);
        let got = announceable(&[
            (public, true, true),
            (private, false, true),
            (paused, true, false),
        ]);
        assert_eq!(got, vec![public]);
    }

    #[test]
    fn due_shortly_after_add_then_every_five_minutes() {
        let start = Instant::now();
        let fresh = AnnounceClock {
            first_seen: start,
            last_sent: None,
        };
        assert!(!is_due(&fresh, start));
        assert!(is_due(&fresh, start + ANNOUNCE_AFTER_ADD));
        let sent = AnnounceClock {
            first_seen: start,
            last_sent: Some(start + ANNOUNCE_AFTER_ADD),
        };
        assert!(!is_due(
            &sent,
            start + ANNOUNCE_AFTER_ADD + Duration::from_secs(60)
        ));
        assert!(is_due(
            &sent,
            start + ANNOUNCE_AFTER_ADD + ANNOUNCE_INTERVAL
        ));
    }

    #[test]
    fn ingest_calls_add_for_each_hash_and_skips_self() {
        let hashes = [sample_hash(4), sample_hash(5)];
        let packet = sample_packet(7000, &hashes, "remote");
        let src = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 9), 9999));
        let mut limiter = InboundLimiter::default();
        let mut seen = Vec::new();
        let n = ingest(
            &packet,
            src,
            "local-cookie",
            &mut limiter,
            Instant::now(),
            |hash, addr| seen.push((*hash, addr)),
        );
        assert_eq!(n, 2);
        assert_eq!(
            seen[0].1,
            SocketAddr::from((Ipv4Addr::new(192, 168, 1, 9), 7000))
        );
        assert_eq!(seen[1].0, hashes[1]);

        let own = sample_packet(7000, &hashes, "local-cookie");
        let n = ingest(
            &own,
            src,
            "local-cookie",
            &mut limiter,
            Instant::now(),
            |_, _| {
                panic!("own cookie must be ignored");
            },
        );
        assert_eq!(n, 0);
    }

    #[test]
    fn ingest_drops_oversize_and_rate_limits_source() {
        let src = SocketAddr::from((Ipv4Addr::new(10, 0, 0, 8), 1));
        let mut limiter = InboundLimiter::default();
        let now = Instant::now();
        let big = vec![0u8; MAX_DATAGRAM + 50];
        assert_eq!(
            ingest(&big, src, "c", &mut limiter, now, |_, _| panic!("oversize")),
            0
        );

        let packet = sample_packet(9, &[sample_hash(7)], "other");
        let mut accepted = 0;
        for _ in 0..INBOUND_MAX + 5 {
            accepted += ingest(&packet, src, "c", &mut limiter, now, |_, _| {});
        }
        assert_eq!(accepted, INBOUND_MAX as usize);
    }

    #[tokio::test]
    #[ignore = "joins multicast group 239.192.152.143:6771; skipped when the group is unavailable in CI"]
    async fn two_socket_multicast_loopback() {
        let left = bind_multicast().expect("join multicast on first socket");
        let right = bind_multicast().expect("join multicast on second socket");
        let left = UdpSocket::from_std(left).unwrap();
        let right = UdpSocket::from_std(right).unwrap();
        let hash = sample_hash(0x11);
        let packet = sample_packet(6881, &[hash], "loop-cookie");
        let dest = SocketAddr::from((GROUP_V4, PORT));
        left.send_to(&packet, dest).await.expect("send announce");

        let mut buf = [0u8; RECV_CAP];
        let (n, src) = tokio::time::timeout(Duration::from_secs(2), right.recv_from(&mut buf))
            .await
            .expect("timeout waiting for multicast")
            .expect("recv");
        let parsed = decode(&buf[..n]).expect("parse looped announce");
        assert_eq!(parsed.info_hashes, vec![hash]);
        assert_eq!(parsed.port, 6881);
        assert!(src.port() > 0);
    }
}
