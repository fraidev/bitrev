use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;

use serde_bencode::value::Value;
use tokio::time::Instant;

use super::handshake::{PeerExtensionInfo, MAX_EXTENSION_PAYLOAD, UT_PEX};
use super::registry::{Extension, ExtensionContext};
use crate::discovery::DiscoverySource;
use crate::file;
use crate::peer::{
    decode_compact_v4, decode_compact_v6, encode_compact_v4, encode_compact_v6, PeerAddr,
};

pub const PEX_INTERVAL: Duration = Duration::from_secs(60);
/// Inbound faster than this is treated as egregious (BEP-0011).
pub const PEX_MIN_INBOUND_INTERVAL: Duration = Duration::from_secs(30);
pub const PEX_MAX_ADDED: usize = 50;
pub const PEX_MAX_DROPPED: usize = 50;

pub const PEX_FLAG_ENCRYPT: u8 = 0x01;
pub const PEX_FLAG_SEED: u8 = 0x02;
pub const PEX_FLAG_UTP: u8 = 0x04;
pub const PEX_FLAG_HOLEPUNCH: u8 = 0x08;
pub const PEX_FLAG_OUTGOING: u8 = 0x10;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PexMessage {
    pub added: Vec<(PeerAddr, u8)>,
    pub dropped: Vec<PeerAddr>,
    pub added6: Vec<(PeerAddr, u8)>,
    pub dropped6: Vec<PeerAddr>,
}

impl PexMessage {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.dropped.is_empty()
            && self.added6.is_empty()
            && self.dropped6.is_empty()
    }

    pub fn encode(&self) -> Vec<u8> {
        encode_message(self)
    }
}

pub struct UtPex {
    ctx: ExtensionContext,
    peer_supports: bool,
    sent: HashSet<PeerAddr>,
    last_sent: Option<Instant>,
    last_recv: Option<Instant>,
    disconnect: bool,
}

impl UtPex {
    pub fn new(ctx: ExtensionContext) -> Self {
        Self {
            ctx,
            peer_supports: false,
            sent: HashSet::new(),
            last_sent: None,
            last_recv: None,
            disconnect: false,
        }
    }

    fn connected_peers(&self) -> HashSet<PeerAddr> {
        self.ctx
            .peer_states
            .states
            .iter()
            .filter(|entry| {
                let addr = *entry.key();
                addr != self.ctx.peer && entry.value().writer_tx.is_some()
            })
            .map(|entry| *entry.key())
            .collect()
    }

    fn flags_for(&self, addr: PeerAddr) -> u8 {
        self.ctx
            .peer_states
            .states
            .get(&addr)
            .map(|state| state.pex_flags(self.ctx.piece_count()))
            .unwrap_or(0)
    }

    fn build_update(&self) -> Option<PexMessage> {
        let current = self.connected_peers();
        let mut added: Vec<PeerAddr> = current
            .iter()
            .copied()
            .filter(|addr| !self.sent.contains(addr))
            .collect();
        let mut dropped: Vec<PeerAddr> = self
            .sent
            .iter()
            .copied()
            .filter(|addr| !current.contains(addr))
            .collect();
        added.sort_by_key(|addr| (addr.is_ipv6(), *addr));
        dropped.sort_by_key(|addr| (addr.is_ipv6(), *addr));
        added.truncate(PEX_MAX_ADDED);
        dropped.truncate(PEX_MAX_DROPPED);

        let mut msg = PexMessage::default();
        for addr in added {
            let flags = self.flags_for(addr);
            match addr {
                SocketAddr::V4(_) => msg.added.push((addr, flags)),
                SocketAddr::V6(_) => msg.added6.push((addr, flags)),
            }
        }
        for addr in dropped {
            match addr {
                SocketAddr::V4(_) => msg.dropped.push(addr),
                SocketAddr::V6(_) => msg.dropped6.push(addr),
            }
        }
        if msg.is_empty() {
            None
        } else {
            Some(msg)
        }
    }

    fn mark_sent(&mut self, msg: &PexMessage) {
        for (addr, _) in msg.added.iter().chain(msg.added6.iter()) {
            self.sent.insert(*addr);
        }
        for addr in msg.dropped.iter().chain(msg.dropped6.iter()) {
            self.sent.remove(addr);
        }
    }
}

impl Extension for UtPex {
    fn name(&self) -> &str {
        UT_PEX
    }

    fn enabled(&self) -> bool {
        self.ctx.allows_pex
    }

    fn on_handshake(&mut self, peer_info: &PeerExtensionInfo) {
        self.peer_supports = peer_info.peer_ext_id(UT_PEX).is_some();
    }

    fn on_message(&mut self, payload: &[u8]) -> Vec<Vec<u8>> {
        if self.disconnect || !self.ctx.allows_pex {
            return Vec::new();
        }
        let now = Instant::now();
        if let Some(last) = self.last_recv {
            if now.saturating_duration_since(last) < PEX_MIN_INBOUND_INTERVAL {
                self.disconnect = true;
                return Vec::new();
            }
        }
        self.last_recv = Some(now);
        let Some(msg) = parse_message(payload) else {
            return Vec::new();
        };
        let mut addrs: Vec<PeerAddr> = msg
            .added
            .into_iter()
            .chain(msg.added6)
            .map(|(addr, _flags)| addr)
            .filter(|addr| *addr != self.ctx.peer)
            .collect();
        addrs.sort();
        addrs.dedup();
        if !addrs.is_empty() {
            (self.ctx.add_peers)(&self.ctx.info_hash, DiscoverySource::Pex, addrs);
        }
        Vec::new()
    }

    fn on_tick(&mut self) -> Vec<Vec<u8>> {
        if self.disconnect || !self.peer_supports || !self.ctx.allows_pex {
            return Vec::new();
        }
        let now = Instant::now();
        if let Some(last) = self.last_sent {
            if now.saturating_duration_since(last) < PEX_INTERVAL {
                return Vec::new();
            }
        }
        let Some(msg) = self.build_update() else {
            return Vec::new();
        };
        self.mark_sent(&msg);
        self.last_sent = Some(now);
        vec![msg.encode()]
    }

    fn should_disconnect(&self) -> bool {
        self.disconnect
    }
}

pub fn parse_message(payload: &[u8]) -> Option<PexMessage> {
    if payload.is_empty() || payload.len() > MAX_EXTENSION_PAYLOAD {
        return None;
    }
    if file::check_bencode_depth(payload).is_err() {
        return None;
    }
    let Value::Dict(dict) = serde_bencode::from_bytes::<Value>(payload).ok()? else {
        return None;
    };

    let added = compact_list(&dict, b"added", false)?;
    let added6 = compact_list(&dict, b"added6", true)?;

    let dropped = match dict_bytes(&dict, b"dropped") {
        Some(buf) => decode_compact_v4(buf).unwrap_or_default(),
        None => Vec::new(),
    };
    let dropped6 = match dict_bytes(&dict, b"dropped6") {
        Some(buf) => decode_compact_v6(buf).unwrap_or_default(),
        None => Vec::new(),
    };

    Some(PexMessage {
        added,
        dropped,
        added6,
        dropped6,
    })
}

/// Flag length mismatch rejects the message (`None`). Odd compact length is ignored.
fn compact_list(
    dict: &std::collections::HashMap<Vec<u8>, Value>,
    key: &[u8],
    v6: bool,
) -> Option<Vec<(PeerAddr, u8)>> {
    let flag_key = if v6 {
        &b"added6.f"[..]
    } else {
        &b"added.f"[..]
    };
    let peers = match dict_bytes(dict, key) {
        Some(buf) => {
            let decoded = if v6 {
                decode_compact_v6(buf)
            } else {
                decode_compact_v4(buf)
            };
            decoded.unwrap_or_default()
        }
        None => Vec::new(),
    };
    match dict_bytes(dict, flag_key) {
        Some(flags) if flags.len() != peers.len() => None,
        Some(flags) => Some(peers.into_iter().zip(flags.iter().copied()).collect()),
        None => Some(peers.into_iter().map(|addr| (addr, 0)).collect()),
    }
}

fn dict_bytes<'a>(
    dict: &'a std::collections::HashMap<Vec<u8>, Value>,
    key: &[u8],
) -> Option<&'a [u8]> {
    match dict.get(key) {
        Some(Value::Bytes(bytes)) => Some(bytes.as_slice()),
        _ => None,
    }
}

fn encode_message(msg: &PexMessage) -> Vec<u8> {
    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    if !msg.added.is_empty() {
        let addrs: Vec<PeerAddr> = msg.added.iter().map(|(addr, _)| *addr).collect();
        let flags: Vec<u8> = msg.added.iter().map(|(_, f)| *f).collect();
        pairs.push((b"added".to_vec(), encode_bytes(&encode_compact_v4(&addrs))));
        pairs.push((b"added.f".to_vec(), encode_bytes(&flags)));
    }
    if !msg.added6.is_empty() {
        let addrs: Vec<PeerAddr> = msg.added6.iter().map(|(addr, _)| *addr).collect();
        let flags: Vec<u8> = msg.added6.iter().map(|(_, f)| *f).collect();
        pairs.push((b"added6".to_vec(), encode_bytes(&encode_compact_v6(&addrs))));
        pairs.push((b"added6.f".to_vec(), encode_bytes(&flags)));
    }
    if !msg.dropped.is_empty() {
        pairs.push((
            b"dropped".to_vec(),
            encode_bytes(&encode_compact_v4(&msg.dropped)),
        ));
    }
    if !msg.dropped6.is_empty() {
        pairs.push((
            b"dropped6".to_vec(),
            encode_bytes(&encode_compact_v6(&msg.dropped6)),
        ));
    }
    encode_dict(&pairs)
}

fn encode_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = format!("{}:", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out
}

fn encode_dict(pairs: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut pairs = pairs.to_vec();
    pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut out = vec![b'd'];
    for (key, value) in pairs {
        out.extend(encode_bytes(&key));
        out.extend(value);
    }
    out.push(b'e');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::registry::noop_add_peers;
    use crate::extension::ut_metadata::MetadataStore;
    use crate::peer_state::{PeerState, PeerStates};
    use std::sync::{Arc, Mutex};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn ctx_with(
        states: Arc<PeerStates>,
        peer: SocketAddr,
        add_peers: AddPeersFnForTest,
        allows_pex: bool,
    ) -> ExtensionContext {
        ExtensionContext {
            info_hash: [7; 20],
            peer,
            metadata: MetadataStore::new([7; 20]),
            peer_states: states,
            add_peers,
            allows_pex,
            piece_count: Arc::new(|| 1),
        }
    }

    type AddPeersFnForTest = crate::extension::AddPeersFn;

    fn live(states: &PeerStates, peer: SocketAddr) {
        let (tx, _rx) = flume::unbounded();
        assert!(states.insert_live(peer, tx));
    }

    fn peer_info_pex() -> PeerExtensionInfo {
        let mut info = PeerExtensionInfo::default();
        info.m.insert(UT_PEX.into(), 1);
        info
    }

    #[test]
    fn encode_decode_added_dropped_fixture() {
        let added = addr(6881);
        let dropped = addr(6889);
        let msg = PexMessage {
            added: vec![(added, PEX_FLAG_SEED)],
            dropped: vec![dropped],
            added6: Vec::new(),
            dropped6: Vec::new(),
        };
        let encoded = msg.encode();
        let expected = {
            let mut body = b"d5:added6:".to_vec();
            body.extend_from_slice(&encode_compact_v4(&[added]));
            body.extend_from_slice(b"7:added.f1:");
            body.push(PEX_FLAG_SEED);
            body.extend_from_slice(b"7:dropped6:");
            body.extend_from_slice(&encode_compact_v4(&[dropped]));
            body.push(b'e');
            body
        };
        assert_eq!(encoded, expected);
        assert_eq!(parse_message(&encoded), Some(msg));
    }

    #[test]
    fn encode_decode_ipv6_fixture() {
        let added6: SocketAddr = "[::1]:6881".parse().unwrap();
        let dropped6: SocketAddr = "[::1]:6889".parse().unwrap();
        let msg = PexMessage {
            added: Vec::new(),
            dropped: Vec::new(),
            added6: vec![(added6, PEX_FLAG_UTP)],
            dropped6: vec![dropped6],
        };
        let decoded = parse_message(&msg.encode()).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let added = addr(51413);
        let compact = encode_compact_v4(&[added]);
        let mut body = b"d5:added6:".to_vec();
        body.extend_from_slice(&compact);
        body.extend_from_slice(b"1:xi1ee");
        let decoded = parse_message(&body).unwrap();
        assert_eq!(decoded.added, vec![(added, 0)]);
        assert!(decoded.dropped.is_empty());
    }

    #[test]
    fn added_f_length_mismatch_is_rejected() {
        let added = addr(6881);
        let compact = encode_compact_v4(&[added]);
        let mut body = b"d5:added6:".to_vec();
        body.extend_from_slice(&compact);
        body.extend_from_slice(b"7:added.f2:");
        body.extend_from_slice(&[PEX_FLAG_SEED, 0]);
        body.push(b'e');
        assert!(parse_message(&body).is_none());
    }

    #[test]
    fn malformed_compact_added_is_ignored() {
        let body = b"d5:added5:\x01\x02\x03\x04\x05e";
        let decoded = parse_message(body).unwrap();
        assert!(decoded.added.is_empty());
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_message(b"").is_none());
        assert!(parse_message(b"not bencode").is_none());
        assert!(parse_message(b"i4e").is_none());
        let huge = vec![0u8; MAX_EXTENSION_PAYLOAD + 1];
        assert!(parse_message(&huge).is_none());
    }

    #[test]
    fn on_tick_sends_connected_peer_once() {
        let states = Arc::new(PeerStates::default());
        let self_peer = addr(1);
        let other = addr(2);
        live(&states, self_peer);
        live(&states, other);
        let mut ext = UtPex::new(ctx_with(states, self_peer, noop_add_peers(), true));
        ext.on_handshake(&peer_info_pex());
        let first = ext.on_tick();
        assert_eq!(first.len(), 1);
        let msg = parse_message(&first[0]).unwrap();
        assert_eq!(msg.added, vec![(other, 0)]);
        assert!(ext.on_tick().is_empty(), "second tick must wait 60s");
    }

    #[test]
    fn on_tick_caps_added_at_50() {
        let states = Arc::new(PeerStates::default());
        let self_peer = addr(1);
        live(&states, self_peer);
        for port in 2..62 {
            live(&states, addr(port));
        }
        let mut ext = UtPex::new(ctx_with(states, self_peer, noop_add_peers(), true));
        ext.on_handshake(&peer_info_pex());
        let first = ext.on_tick();
        let msg = parse_message(&first[0]).unwrap();
        assert_eq!(msg.added.len(), PEX_MAX_ADDED);
    }

    #[test]
    fn private_context_is_disabled() {
        let mut ext = UtPex::new(ctx_with(
            Arc::new(PeerStates::default()),
            addr(1),
            noop_add_peers(),
            false,
        ));
        assert!(!ext.enabled());
        ext.on_handshake(&peer_info_pex());
        assert!(ext.on_tick().is_empty());
        assert!(ext.on_message(&PexMessage::default().encode()).is_empty());
    }

    #[tokio::test]
    async fn on_tick_rate_limit_under_pause() {
        tokio::time::pause();
        let states = Arc::new(PeerStates::default());
        let self_peer = addr(1);
        live(&states, self_peer);
        live(&states, addr(2));
        let mut ext = UtPex::new(ctx_with(states.clone(), self_peer, noop_add_peers(), true));
        ext.on_handshake(&peer_info_pex());
        assert_eq!(ext.on_tick().len(), 1);
        assert!(ext.on_tick().is_empty());
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(ext.on_tick().is_empty());
        live(&states, addr(3));
        tokio::time::advance(Duration::from_secs(1)).await;
        let second = ext.on_tick();
        assert_eq!(second.len(), 1);
        let msg = parse_message(&second[0]).unwrap();
        assert_eq!(msg.added, vec![(addr(3), 0)]);
    }

    #[tokio::test]
    async fn inbound_faster_than_half_minute_disconnects() {
        tokio::time::pause();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        let add_peers: AddPeersFnForTest = Arc::new(move |_, _, addrs| {
            seen_clone.lock().unwrap().extend(addrs);
            0
        });
        let mut ext = UtPex::new(ctx_with(
            Arc::new(PeerStates::default()),
            addr(1),
            add_peers,
            true,
        ));
        let payload = PexMessage {
            added: vec![(addr(9), 0)],
            ..PexMessage::default()
        }
        .encode();
        assert!(ext.on_message(&payload).is_empty());
        assert!(!ext.should_disconnect());
        assert_eq!(*seen.lock().unwrap(), vec![addr(9)]);
        assert!(ext.on_message(&payload).is_empty());
        assert!(ext.should_disconnect());

        tokio::time::advance(PEX_MIN_INBOUND_INTERVAL).await;
        let mut later = UtPex::new(ctx_with(
            Arc::new(PeerStates::default()),
            addr(1),
            noop_add_peers(),
            true,
        ));
        assert!(later.on_message(&payload).is_empty());
        tokio::time::advance(PEX_MIN_INBOUND_INTERVAL).await;
        assert!(later.on_message(&payload).is_empty());
        assert!(!later.should_disconnect());
    }

    #[test]
    fn incoming_added_feeds_add_peers() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        let add_peers: AddPeersFnForTest = Arc::new(move |hash, source, addrs| {
            assert_eq!(hash, &[7; 20]);
            assert_eq!(source, DiscoverySource::Pex);
            seen_clone.lock().unwrap().extend(addrs);
            1
        });
        let mut ext = UtPex::new(ctx_with(
            Arc::new(PeerStates::default()),
            addr(1),
            add_peers,
            true,
        ));
        let payload = PexMessage {
            added: vec![(addr(9), PEX_FLAG_SEED)],
            added6: vec![("[::1]:7000".parse().unwrap(), 0)],
            ..PexMessage::default()
        }
        .encode();
        ext.on_message(&payload);
        let got = seen.lock().unwrap().clone();
        assert!(got.contains(&addr(9)));
        assert!(got.contains(&"[::1]:7000".parse().unwrap()));
    }

    #[test]
    fn flags_record_known_bits() {
        let state = PeerState {
            encrypted: true,
            utp: true,
            outgoing: true,
            bitfield: crate::bitfield::Bitfield::filled(1),
            ..PeerState::default()
        };
        assert_eq!(
            state.pex_flags(1),
            PEX_FLAG_ENCRYPT | PEX_FLAG_SEED | PEX_FLAG_UTP | PEX_FLAG_OUTGOING
        );
        assert_eq!(state.pex_flags(1) & PEX_FLAG_HOLEPUNCH, 0);
    }
}
