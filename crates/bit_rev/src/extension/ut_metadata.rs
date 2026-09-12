use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_bencode::value::Value;
use tokio::sync::watch;
use tokio::time::Instant;

use super::handshake::{PeerExtensionInfo, MAX_EXTENSION_PAYLOAD, MAX_METADATA_SIZE, UT_METADATA};
use super::registry::{Extension, ExtensionContext};
use crate::file::{self, skip_bencode};
use crate::peer::PeerAddr;

pub const METADATA_PIECE_LEN: usize = 16 * 1024;
pub const METADATA_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
pub const METADATA_PIPELINE: usize = 2;
pub const MAX_SERVE_PER_WINDOW: usize = 4;

const MSG_REQUEST: i64 = 0;
const MSG_DATA: i64 = 1;
const MSG_REJECT: i64 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UtMetadataMessage {
    Request {
        piece: u32,
    },
    Data {
        piece: u32,
        total_size: Option<i64>,
        data: Vec<u8>,
    },
    Reject {
        piece: u32,
    },
}

#[derive(Debug)]
struct Inflight {
    peer: PeerAddr,
    at: Instant,
}

#[derive(Debug)]
struct FetchState {
    size: usize,
    pieces: Vec<Option<Vec<u8>>>,
    inflight: HashMap<u32, Inflight>,
    contributors: HashSet<PeerAddr>,
}

impl FetchState {
    fn new(size: usize) -> Self {
        let n = size.div_ceil(METADATA_PIECE_LEN);
        Self {
            size,
            pieces: vec![None; n],
            inflight: HashMap::new(),
            contributors: HashSet::new(),
        }
    }

    fn piece_len(&self, piece: u32) -> Option<usize> {
        let idx = piece as usize;
        if idx >= self.pieces.len() {
            return None;
        }
        if idx + 1 == self.pieces.len() {
            let rem = self.size % METADATA_PIECE_LEN;
            Some(if rem == 0 { METADATA_PIECE_LEN } else { rem })
        } else {
            Some(METADATA_PIECE_LEN)
        }
    }
}

struct StoreInner {
    info_bytes: Option<Arc<[u8]>>,
    fetch: Option<FetchState>,
}

pub struct MetadataStore {
    info_hash: [u8; 20],
    inner: Mutex<StoreInner>,
    completed: watch::Sender<Option<Arc<[u8]>>>,
}

impl MetadataStore {
    pub fn new(info_hash: [u8; 20]) -> Arc<Self> {
        let (completed, _) = watch::channel(None);
        Arc::new(Self {
            info_hash,
            inner: Mutex::new(StoreInner {
                info_bytes: None,
                fetch: None,
            }),
            completed,
        })
    }

    pub fn with_bytes(info_hash: [u8; 20], bytes: Arc<[u8]>) -> Arc<Self> {
        let (completed, _) = watch::channel(Some(bytes.clone()));
        Arc::new(Self {
            info_hash,
            inner: Mutex::new(StoreInner {
                info_bytes: Some(bytes),
                fetch: None,
            }),
            completed,
        })
    }

    pub fn info_hash(&self) -> [u8; 20] {
        self.info_hash
    }

    pub fn info_bytes(&self) -> Option<Arc<[u8]>> {
        self.inner.lock().unwrap().info_bytes.clone()
    }

    pub fn metadata_size(&self) -> Option<i64> {
        self.info_bytes()
            .map(|bytes| i64::try_from(bytes.len()).unwrap_or(i64::MAX))
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<[u8]>>> {
        self.completed.subscribe()
    }

    pub async fn wait(&self) -> Arc<[u8]> {
        if let Some(bytes) = self.info_bytes() {
            return bytes;
        }
        let mut rx = self.completed.subscribe();
        loop {
            if let Some(bytes) = rx.borrow().clone() {
                return bytes;
            }
            if rx.changed().await.is_err() {
                if let Some(bytes) = self.info_bytes() {
                    return bytes;
                }
                std::future::pending::<()>().await;
            }
        }
    }

    pub fn adopt_size(&self, size: i64) -> Option<usize> {
        if size <= 0 || size > MAX_METADATA_SIZE {
            return None;
        }
        let size = size as usize;
        let mut inner = self.inner.lock().unwrap();
        if inner.info_bytes.is_some() {
            return inner.info_bytes.as_ref().map(|b| b.len());
        }
        match inner.fetch.as_ref() {
            Some(fetch) if fetch.size == size => Some(size),
            Some(_) => None,
            None => {
                inner.fetch = Some(FetchState::new(size));
                Some(size)
            }
        }
    }

    pub fn expected_size(&self) -> Option<usize> {
        let inner = self.inner.lock().unwrap();
        if let Some(bytes) = &inner.info_bytes {
            return Some(bytes.len());
        }
        inner.fetch.as_ref().map(|f| f.size)
    }

    pub fn claim_requests(&self, peer: PeerAddr, max: usize) -> Vec<u32> {
        if max == 0 {
            return Vec::new();
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.info_bytes.is_some() {
            return Vec::new();
        }
        let Some(fetch) = inner.fetch.as_mut() else {
            return Vec::new();
        };
        let now = Instant::now();
        let mut out = Vec::new();
        for (idx, slot) in fetch.pieces.iter().enumerate() {
            if out.len() >= max {
                break;
            }
            if slot.is_some() {
                continue;
            }
            let piece = idx as u32;
            if fetch.inflight.contains_key(&piece) {
                continue;
            }
            fetch.inflight.insert(piece, Inflight { peer, at: now });
            out.push(piece);
        }
        out
    }

    pub fn on_data(
        &self,
        peer: PeerAddr,
        piece: u32,
        data: &[u8],
        total_size: Option<i64>,
        peer_states: &crate::peer_state::PeerStates,
    ) -> DataOutcome {
        let mut inner = self.inner.lock().unwrap();
        if inner.info_bytes.is_some() {
            return DataOutcome::AlreadyComplete;
        }
        let Some(fetch) = inner.fetch.as_mut() else {
            return DataOutcome::Unrequested;
        };
        if let Some(size) = total_size {
            if size <= 0 || size as usize != fetch.size {
                return DataOutcome::Invalid;
            }
        }
        let Some(expected_len) = fetch.piece_len(piece) else {
            return DataOutcome::Invalid;
        };
        if data.len() != expected_len {
            return DataOutcome::Invalid;
        }
        let Some(inflight) = fetch.inflight.get(&piece) else {
            if fetch
                .pieces
                .get(piece as usize)
                .is_some_and(|slot| slot.is_some())
            {
                return DataOutcome::Duplicate;
            }
            return DataOutcome::Unrequested;
        };
        if inflight.peer != peer {
            return DataOutcome::Unrequested;
        }
        fetch.inflight.remove(&piece);
        let slot = &mut fetch.pieces[piece as usize];
        if slot.is_some() {
            return DataOutcome::Duplicate;
        }
        *slot = Some(data.to_vec());
        fetch.contributors.insert(peer);

        if fetch.pieces.iter().any(|p| p.is_none()) {
            return DataOutcome::Accepted;
        }

        let mut assembled = Vec::with_capacity(fetch.size);
        for part in &fetch.pieces {
            assembled.extend_from_slice(part.as_ref().expect("all pieces present"));
        }
        assembled.truncate(fetch.size);

        let mut hasher = sha1_smol::Sha1::new();
        hasher.update(&assembled);
        if hasher.digest().bytes() != self.info_hash {
            let contributors: Vec<PeerAddr> = fetch.contributors.iter().copied().collect();
            let sole = contributors.len() == 1;
            inner.fetch = None;
            drop(inner);
            for contributor in contributors {
                peer_states.record_hash_failure(contributor, sole);
            }
            return DataOutcome::HashMismatch;
        }

        let bytes: Arc<[u8]> = assembled.into();
        inner.info_bytes = Some(bytes.clone());
        inner.fetch = None;
        let _ = self.completed.send(Some(bytes));
        DataOutcome::Complete
    }

    pub fn on_reject(&self, peer: PeerAddr, piece: u32) {
        let mut inner = self.inner.lock().unwrap();
        let Some(fetch) = inner.fetch.as_mut() else {
            return;
        };
        if fetch
            .inflight
            .get(&piece)
            .is_some_and(|inf| inf.peer == peer)
        {
            fetch.inflight.remove(&piece);
        }
    }

    pub fn on_peer_gone(&self, peer: PeerAddr) {
        let mut inner = self.inner.lock().unwrap();
        let Some(fetch) = inner.fetch.as_mut() else {
            return;
        };
        fetch.inflight.retain(|_, inf| inf.peer != peer);
    }

    pub fn release_timeouts(&self, now: Instant) -> Vec<u32> {
        let mut inner = self.inner.lock().unwrap();
        let Some(fetch) = inner.fetch.as_mut() else {
            return Vec::new();
        };
        let mut released = Vec::new();
        fetch.inflight.retain(|piece, inf| {
            if now.saturating_duration_since(inf.at) >= METADATA_REQUEST_TIMEOUT {
                released.push(*piece);
                false
            } else {
                true
            }
        });
        released
    }

    pub fn serve_piece(&self, piece: u32) -> Option<(Vec<u8>, i64)> {
        let inner = self.inner.lock().unwrap();
        let bytes = inner.info_bytes.as_ref()?;
        let start = piece as usize * METADATA_PIECE_LEN;
        if start >= bytes.len() {
            return None;
        }
        let end = (start + METADATA_PIECE_LEN).min(bytes.len());
        Some((
            bytes[start..end].to_vec(),
            i64::try_from(bytes.len()).unwrap_or(i64::MAX),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataOutcome {
    Accepted,
    Complete,
    AlreadyComplete,
    Duplicate,
    Unrequested,
    Invalid,
    HashMismatch,
}

pub struct UtMetadata {
    ctx: ExtensionContext,
    peer_supports: bool,
    peer_size: Option<i64>,
    inflight: HashMap<u32, Instant>,
    served_this_window: usize,
    disconnect: bool,
}

impl UtMetadata {
    pub fn new(ctx: ExtensionContext) -> Self {
        Self {
            ctx,
            peer_supports: false,
            peer_size: None,
            inflight: HashMap::new(),
            served_this_window: 0,
            disconnect: false,
        }
    }

    fn request_more(&mut self) -> Vec<Vec<u8>> {
        if !self.peer_supports || self.ctx.metadata.info_bytes().is_some() {
            return Vec::new();
        }
        if self.peer_size.is_none() {
            return Vec::new();
        }
        let want = METADATA_PIPELINE.saturating_sub(self.inflight.len());
        let pieces = self.ctx.metadata.claim_requests(self.ctx.peer, want);
        let now = Instant::now();
        let mut out = Vec::with_capacity(pieces.len());
        for piece in pieces {
            self.inflight.insert(piece, now);
            out.push(encode_request(piece));
        }
        out
    }

    fn expire_local(&mut self, now: Instant) {
        self.ctx.metadata.release_timeouts(now);
        self.inflight
            .retain(|_, at| now.saturating_duration_since(*at) < METADATA_REQUEST_TIMEOUT);
    }
}

impl Drop for UtMetadata {
    fn drop(&mut self) {
        self.ctx.metadata.on_peer_gone(self.ctx.peer);
    }
}

impl Extension for UtMetadata {
    fn name(&self) -> &str {
        UT_METADATA
    }

    fn on_handshake(&mut self, peer_info: &PeerExtensionInfo) {
        self.peer_supports = peer_info.peer_ext_id(UT_METADATA).is_some();
        if !self.peer_supports {
            self.peer_size = None;
            return;
        }
        if let Some(size) = peer_info.metadata_size {
            if self.ctx.metadata.info_bytes().is_none() {
                self.peer_size = self.ctx.metadata.adopt_size(size).map(|s| s as i64);
            } else {
                self.peer_size = Some(size);
            }
        }
    }

    fn on_message(&mut self, payload: &[u8]) -> Vec<Vec<u8>> {
        if self.disconnect {
            return Vec::new();
        }
        let Some(msg) = parse_message(payload) else {
            self.disconnect = true;
            return Vec::new();
        };
        match msg {
            UtMetadataMessage::Request { piece } => {
                if self.served_this_window >= MAX_SERVE_PER_WINDOW {
                    return vec![encode_reject(piece)];
                }
                self.served_this_window += 1;
                match self.ctx.metadata.serve_piece(piece) {
                    Some((data, total_size)) => vec![encode_data(piece, total_size, &data)],
                    None => vec![encode_reject(piece)],
                }
            }
            UtMetadataMessage::Reject { piece } => {
                self.inflight.remove(&piece);
                self.ctx.metadata.on_reject(self.ctx.peer, piece);
                self.request_more()
            }
            UtMetadataMessage::Data {
                piece,
                total_size,
                data,
            } => {
                if !self.inflight.contains_key(&piece) {
                    self.disconnect = true;
                    return Vec::new();
                }
                self.inflight.remove(&piece);
                let outcome = self.ctx.metadata.on_data(
                    self.ctx.peer,
                    piece,
                    &data,
                    total_size,
                    &self.ctx.peer_states,
                );
                match outcome {
                    DataOutcome::Invalid | DataOutcome::Unrequested => {
                        self.disconnect = true;
                        Vec::new()
                    }
                    DataOutcome::HashMismatch
                    | DataOutcome::Complete
                    | DataOutcome::AlreadyComplete => Vec::new(),
                    DataOutcome::Accepted | DataOutcome::Duplicate => self.request_more(),
                }
            }
        }
    }

    fn on_tick(&mut self) -> Vec<Vec<u8>> {
        self.served_this_window = 0;
        if self.disconnect || !self.peer_supports {
            return Vec::new();
        }
        let now = Instant::now();
        self.expire_local(now);
        self.request_more()
    }

    fn should_disconnect(&self) -> bool {
        self.disconnect
    }
}

pub fn parse_message(payload: &[u8]) -> Option<UtMetadataMessage> {
    if payload.is_empty() || payload.len() > MAX_EXTENSION_PAYLOAD {
        return None;
    }
    if file::check_bencode_depth(payload).is_err() {
        return None;
    }
    let end = skip_bencode(payload, 0).ok()?;
    if end == 0 || payload[0] != b'd' {
        return None;
    }
    let dict_bytes = &payload[..end];
    let rest = &payload[end..];
    let Value::Dict(dict) = serde_bencode::from_bytes::<Value>(dict_bytes).ok()? else {
        return None;
    };
    let msg_type = match dict.get(&b"msg_type"[..]) {
        Some(Value::Int(t)) => *t,
        _ => return None,
    };
    let piece = match dict.get(&b"piece"[..]) {
        Some(Value::Int(n)) if *n >= 0 => u32::try_from(*n).ok()?,
        _ => return None,
    };
    match msg_type {
        MSG_REQUEST => {
            if !rest.is_empty() {
                return None;
            }
            Some(UtMetadataMessage::Request { piece })
        }
        MSG_REJECT => {
            if !rest.is_empty() {
                return None;
            }
            Some(UtMetadataMessage::Reject { piece })
        }
        MSG_DATA => {
            let total_size = match dict.get(&b"total_size"[..]) {
                Some(Value::Int(n)) => Some(*n),
                None => None,
                _ => return None,
            };
            if rest.len() > METADATA_PIECE_LEN {
                return None;
            }
            Some(UtMetadataMessage::Data {
                piece,
                total_size,
                data: rest.to_vec(),
            })
        }
        _ => None,
    }
}

pub fn encode_request(piece: u32) -> Vec<u8> {
    encode_dict(&[("msg_type", MSG_REQUEST), ("piece", i64::from(piece))])
}

pub fn encode_reject(piece: u32) -> Vec<u8> {
    encode_dict(&[("msg_type", MSG_REJECT), ("piece", i64::from(piece))])
}

pub fn encode_data(piece: u32, total_size: i64, data: &[u8]) -> Vec<u8> {
    let mut out = encode_dict(&[
        ("msg_type", MSG_DATA),
        ("piece", i64::from(piece)),
        ("total_size", total_size),
    ]);
    out.extend_from_slice(data);
    out
}

fn encode_dict(pairs: &[(&str, i64)]) -> Vec<u8> {
    let mut pairs: Vec<(&[u8], i64)> = pairs.iter().map(|(k, v)| (k.as_bytes(), *v)).collect();
    pairs.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut out = vec![b'd'];
    for (key, value) in pairs {
        out.extend(format!("{}:", key.len()).as_bytes());
        out.extend_from_slice(key);
        out.extend(format!("i{value}e").as_bytes());
    }
    out.push(b'e');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::{Info, TorrentFile, TorrentMeta};
    use crate::peer_state::PeerStates;
    use crate::torrent::Torrent;
    use serde_bytes::ByteBuf;
    use std::net::SocketAddr;
    use std::time::Duration;

    fn peer(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn ctx_with(store: Arc<MetadataStore>, addr: SocketAddr) -> ExtensionContext {
        ExtensionContext {
            info_hash: store.info_hash(),
            peer: addr,
            metadata: store,
            peer_states: Arc::new(PeerStates::default()),
        }
    }

    fn sample_meta() -> TorrentMeta {
        let data = b"hello metadata payload!!";
        let mut pieces = Vec::new();
        pieces.extend_from_slice(&sha1(data));
        TorrentMeta::new(TorrentFile {
            info: Info {
                name: "tiny.bin".into(),
                pieces: ByteBuf::from(pieces),
                piece_length: data.len() as i64,
                md5sum: None,
                length: Some(data.len() as i64),
                files: None,
                private: None,
                path: None,
                root_hash: None,
            },
            announce: None,
            nodes: None,
            encoding: None,
            httpseeds: None,
            announce_list: None,
            creation_date: None,
            comment: None,
            created_by: None,
        })
        .expect("meta")
    }

    fn sha1(data: &[u8]) -> [u8; 20] {
        let mut hasher = sha1_smol::Sha1::new();
        hasher.update(data);
        hasher.digest().bytes()
    }

    fn peer_info(size: Option<i64>) -> PeerExtensionInfo {
        let mut info = PeerExtensionInfo::default();
        info.m.insert(UT_METADATA.into(), 3);
        info.metadata_size = size;
        info
    }

    #[test]
    fn decode_request_fixture() {
        let bytes = b"d8:msg_typei0e5:piecei3ee";
        assert_eq!(
            parse_message(bytes),
            Some(UtMetadataMessage::Request { piece: 3 })
        );
    }

    #[test]
    fn decode_reject_fixture() {
        let bytes = b"d8:msg_typei2e5:piecei1ee";
        assert_eq!(
            parse_message(bytes),
            Some(UtMetadataMessage::Reject { piece: 1 })
        );
    }

    #[test]
    fn decode_data_appended_bytes() {
        let dict = b"d8:msg_typei1e5:piecei0e10:total_sizei4ee";
        let mut msg = dict.to_vec();
        msg.extend_from_slice(b"abcd");
        assert_eq!(
            parse_message(&msg),
            Some(UtMetadataMessage::Data {
                piece: 0,
                total_size: Some(4),
                data: b"abcd".to_vec(),
            })
        );
    }

    #[test]
    fn encode_decode_round_trip_request_reject_data() {
        assert_eq!(
            parse_message(&encode_request(7)),
            Some(UtMetadataMessage::Request { piece: 7 })
        );
        assert_eq!(
            parse_message(&encode_reject(2)),
            Some(UtMetadataMessage::Reject { piece: 2 })
        );
        let encoded = encode_data(0, 4, b"abcd");
        assert_eq!(
            parse_message(&encoded),
            Some(UtMetadataMessage::Data {
                piece: 0,
                total_size: Some(4),
                data: b"abcd".to_vec(),
            })
        );
        assert!(encoded.windows(4).any(|w| w == b"abcd"));
    }

    #[test]
    fn parse_rejects_garbage_and_oversized() {
        assert!(parse_message(b"").is_none());
        assert!(parse_message(b"not bencode").is_none());
        assert!(parse_message(b"i4e").is_none());
        let huge = vec![0u8; MAX_EXTENSION_PAYLOAD + 1];
        assert!(parse_message(&huge).is_none());
        let mut oversized_data = encode_data(0, 1, &[]);
        oversized_data.extend(vec![0u8; METADATA_PIECE_LEN + 1]);
        assert!(parse_message(&oversized_data).is_none());
    }

    #[test]
    fn serve_rejects_out_of_range_and_missing_metadata() {
        let empty = MetadataStore::new([1; 20]);
        let mut ext = UtMetadata::new(ctx_with(empty, peer(1)));
        assert_eq!(ext.on_message(&encode_request(0)), vec![encode_reject(0)]);

        let meta = sample_meta();
        let store = MetadataStore::with_bytes(meta.info_hash, meta.info_bytes.clone());
        let mut ext = UtMetadata::new(ctx_with(store, peer(2)));
        assert_eq!(ext.on_message(&encode_request(99)), vec![encode_reject(99)]);
    }

    #[test]
    fn serve_returns_data_from_info_bytes() {
        let meta = sample_meta();
        let store = MetadataStore::with_bytes(meta.info_hash, meta.info_bytes.clone());
        let mut ext = UtMetadata::new(ctx_with(store, peer(3)));
        let replies = ext.on_message(&encode_request(0));
        assert_eq!(replies.len(), 1);
        match parse_message(&replies[0]) {
            Some(UtMetadataMessage::Data { piece, data, .. }) => {
                assert_eq!(piece, 0);
                assert_eq!(data, meta.info_bytes.as_ref());
            }
            other => panic!("expected data, got {other:?}"),
        }
    }

    #[test]
    fn fetch_verifies_hash_and_builds_matching_torrent() {
        let meta = sample_meta();
        let expected = Torrent::new(&meta).unwrap();
        let store = MetadataStore::new(meta.info_hash);
        store.adopt_size(meta.info_bytes.len() as i64);
        let states = Arc::new(PeerStates::default());
        let addr = peer(4);
        let outcome = store.on_data(
            addr,
            0,
            &meta.info_bytes,
            Some(meta.info_bytes.len() as i64),
            &states,
        );
        // piece 0 was never claimed, so Unrequested
        assert_eq!(outcome, DataOutcome::Unrequested);

        let claimed = store.claim_requests(addr, 1);
        assert_eq!(claimed, vec![0]);
        let outcome = store.on_data(
            addr,
            0,
            &meta.info_bytes,
            Some(meta.info_bytes.len() as i64),
            &states,
        );
        assert_eq!(outcome, DataOutcome::Complete);
        let got = TorrentMeta::from_info_bytes(store.info_bytes().unwrap()).unwrap();
        let torrent = Torrent::new(&got).unwrap();
        assert_eq!(torrent.info_hash, expected.info_hash);
        assert_eq!(torrent.piece_hashes, expected.piece_hashes);
        assert_eq!(torrent.files, expected.files);
    }

    #[test]
    fn corrupt_metadata_penalizes_contributors_and_resets() {
        let meta = sample_meta();
        let store = MetadataStore::new(meta.info_hash);
        store.adopt_size(meta.info_bytes.len() as i64);
        let states = Arc::new(PeerStates::default());
        let addr = peer(5);
        assert_eq!(store.claim_requests(addr, 1), vec![0]);
        let mut bad = meta.info_bytes.to_vec();
        bad[0] ^= 0xff;
        let outcome = store.on_data(addr, 0, &bad, Some(bad.len() as i64), &states);
        assert_eq!(outcome, DataOutcome::HashMismatch);
        assert!(store.info_bytes().is_none());
        assert!(states.is_banned(addr));
        assert!(store.adopt_size(meta.info_bytes.len() as i64).is_some());
        assert_eq!(store.claim_requests(peer(6), 1), vec![0]);
    }

    #[test]
    fn unrequested_or_oversized_data_is_invalid() {
        let meta = sample_meta();
        let store = MetadataStore::new(meta.info_hash);
        store.adopt_size(meta.info_bytes.len() as i64);
        let states = Arc::new(PeerStates::default());
        let addr = peer(7);
        assert_eq!(
            store.on_data(addr, 0, &meta.info_bytes, None, &states),
            DataOutcome::Unrequested
        );
        store.claim_requests(addr, 1);
        assert_eq!(
            store.on_data(
                addr,
                0,
                &[0u8; 32],
                Some(meta.info_bytes.len() as i64),
                &states
            ),
            DataOutcome::Invalid
        );
    }

    #[tokio::test]
    async fn on_tick_rerequests_after_timeout() {
        tokio::time::pause();
        let meta = sample_meta();
        let store = MetadataStore::new(meta.info_hash);
        let mut ext = UtMetadata::new(ctx_with(store, peer(8)));
        ext.on_handshake(&peer_info(Some(meta.info_bytes.len() as i64)));
        let first = ext.on_tick();
        assert_eq!(first, vec![encode_request(0)]);
        assert!(ext.inflight.contains_key(&0));

        tokio::time::advance(Duration::from_secs(11)).await;
        let retry = ext.on_tick();
        assert_eq!(retry, vec![encode_request(0)]);
    }

    #[test]
    fn drop_releases_inflight_for_other_peers() {
        let meta = sample_meta();
        let store = MetadataStore::new(meta.info_hash);
        store.adopt_size(meta.info_bytes.len() as i64);
        {
            let mut ext = UtMetadata::new(ctx_with(store.clone(), peer(9)));
            ext.on_handshake(&peer_info(Some(meta.info_bytes.len() as i64)));
            assert_eq!(ext.on_tick(), vec![encode_request(0)]);
        }
        assert_eq!(store.claim_requests(peer(10), 1), vec![0]);
    }
}
