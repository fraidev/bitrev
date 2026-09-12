use std::{
    collections::{HashMap, HashSet, VecDeque},
    future,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use rand::rngs::StdRng;
use tokio::{
    io::AsyncWriteExt,
    sync::{Notify, Semaphore},
    time::timeout,
};
use tracing::{debug, error, trace};

use crate::{
    allowed_fast::{generate_allowed_fast_for_ip, DEFAULT_ALLOWED_FAST_SET_SIZE},
    bitfield::Bitfield,
    extension::{
        ExtensionContext, ExtensionRegistry, ExtensionSession, MetadataStore, DEFAULT_REQQ,
    },
    message::{
        self, format_reject_request, validate_request, BlockRequest, Message, RequestError,
        RequestStorm, WriterRequest, MAX_UPLOAD_QUEUE,
    },
    peer::PeerAddr,
    peer_state::PeerStates,
    picker::{self, select_piece, Availability, BOOTSTRAP_VERIFIED},
    protocol::{Frame, Protocol},
    session::{DownloadState, PieceWork},
    storage::Storage,
    torrent::Torrent,
    transport::{BoxedPeerStream, Connector, PeerStream},
    utils,
};

pub const SNUB_TIMEOUT: Duration = Duration::from_secs(60);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
pub const ENDGAME_REREQUEST_AFTER: Duration = Duration::from_millis(100);
pub const INITIAL_PIPELINE: usize = 5;
pub const MAX_PIPELINE: usize = 250;

struct BlockAssignment {
    requesters: Vec<PeerAddr>,
    first_at: tokio::time::Instant,
}

pub struct TorrentDownloadedState {
    pub semaphore: Semaphore,
    pub pieces: Vec<PieceWorkState>,
    pub availability: Availability,
    rng: Mutex<StdRng>,
    bootstrap_until: AtomicUsize,
    verified: picker::VerifiedCount,
    block_assignments: Mutex<HashMap<BlockRequest, BlockAssignment>>,
    duplicate_bytes: AtomicU64,
    pub piece_notify: Notify,
}

impl TorrentDownloadedState {
    pub fn new(pieces: Vec<PieceWorkState>) -> Self {
        Self::with_seed(pieces, rand::random())
    }

    pub fn with_seed(pieces: Vec<PieceWorkState>, seed: u64) -> Self {
        let verified = pieces
            .iter()
            .filter(|pw| pw.downloaded.load(Ordering::Relaxed))
            .count();
        let n = pieces.len();
        Self {
            semaphore: Semaphore::new(0),
            pieces,
            availability: Availability::new(n),
            rng: Mutex::new(picker::seeded_rng(seed)),
            bootstrap_until: AtomicUsize::new(BOOTSTRAP_VERIFIED),
            verified: picker::VerifiedCount::new(verified),
            block_assignments: Mutex::new(HashMap::new()),
            duplicate_bytes: AtomicU64::new(0),
            piece_notify: Notify::new(),
        }
    }

    pub fn set_bootstrap_until(&self, n: usize) {
        self.bootstrap_until.store(n, Ordering::Relaxed);
    }

    pub fn duplicate_bytes(&self) -> u64 {
        self.duplicate_bytes.load(Ordering::Relaxed)
    }

    pub fn is_complete(&self) -> bool {
        self.pieces
            .iter()
            .all(|pw| pw.downloaded.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub fn downloaded_bytes(&self) -> u64 {
        self.pieces
            .iter()
            .filter(|pw| pw.downloaded.load(std::sync::atomic::Ordering::Relaxed))
            .map(|pw| u64::from(pw.piece_work.length))
            .sum()
    }

    pub fn left_bytes(&self) -> u64 {
        self.pieces
            .iter()
            .filter(|pw| !pw.downloaded.load(std::sync::atomic::Ordering::Relaxed))
            .map(|pw| u64::from(pw.piece_work.length))
            .sum()
    }

    pub fn has_piece(&self, index: u32) -> bool {
        self.pieces
            .get(index as usize)
            .map(|pw| pw.downloaded.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false)
    }

    pub fn piece_length(&self, index: u32) -> Option<u32> {
        self.pieces
            .get(index as usize)
            .map(|pw| pw.piece_work.length)
    }

    pub fn piece_count(&self) -> usize {
        self.pieces.len()
    }

    pub fn mark_all_downloaded(&self) {
        for pw in &self.pieces {
            if !pw.downloaded.swap(true, Ordering::Relaxed) {
                self.verified.inc();
            }
        }
        self.piece_notify.notify_waiters();
    }

    pub fn mark_downloaded(&self, index: u32) {
        if let Some(pw) = self.pieces.get(index as usize) {
            if !pw.downloaded.swap(true, Ordering::Relaxed) {
                self.verified.inc();
                self.piece_notify.notify_waiters();
            }
        }
    }

    /// Hook for file priorities (#36). Every piece is wanted until then.
    pub fn wanted(&self, index: u32) -> bool {
        (index as usize) < self.pieces.len()
    }

    pub fn in_endgame(&self) -> bool {
        let mut missing = 0usize;
        let mut reserved = 0usize;
        for pw in &self.pieces {
            if pw.downloaded.load(Ordering::Relaxed) {
                continue;
            }
            if !self.wanted(pw.piece_work.index) {
                continue;
            }
            missing += 1;
            if pw.reserved.lock().unwrap().is_some() {
                reserved += 1;
            }
        }
        missing > 0 && missing == reserved
    }

    fn candidates(&self, peer_has: &Bitfield) -> Vec<u32> {
        self.pieces
            .iter()
            .enumerate()
            .filter(|(i, pw)| {
                !pw.downloaded.load(Ordering::Relaxed)
                    && pw.reserved.lock().unwrap().is_none()
                    && peer_has.has_piece(*i)
                    && self.wanted(*i as u32)
            })
            .map(|(i, _)| i as u32)
            .collect()
    }

    pub fn pick(&self, peer: PeerAddr, peer_has: &Bitfield, prefer: &[u32]) -> Option<u32> {
        loop {
            let candidates = self.candidates(peer_has);
            if candidates.is_empty() {
                return None;
            }
            let verified = self.verified.get();
            let bootstrap_until = self.bootstrap_until.load(Ordering::Relaxed);
            let selected = {
                let mut rng = self.rng.lock().unwrap();
                select_piece(
                    &candidates,
                    prefer,
                    &self.availability,
                    verified,
                    bootstrap_until,
                    &mut *rng,
                )
            };
            let index = selected?;
            if self.try_reserve_piece(index, peer).is_some() {
                return Some(index);
            }
        }
    }

    pub fn assign_block(&self, req: BlockRequest, peer: PeerAddr) -> bool {
        let mut map = self.block_assignments.lock().unwrap();
        match map.get_mut(&req) {
            Some(entry) => {
                if entry.requesters.contains(&peer) {
                    return false;
                }
                if entry.requesters.len() >= picker::MAX_BLOCK_REQUESTERS {
                    return false;
                }
                entry.requesters.push(peer);
                true
            }
            None => {
                map.insert(
                    req,
                    BlockAssignment {
                        requesters: vec![peer],
                        first_at: tokio::time::Instant::now(),
                    },
                );
                true
            }
        }
    }

    pub fn endgame_blocks(
        &self,
        peer: PeerAddr,
        peer_has: &Bitfield,
        limit: usize,
    ) -> Vec<BlockRequest> {
        if limit == 0 || !self.in_endgame() {
            return Vec::new();
        }
        let map = self.block_assignments.lock().unwrap();
        let mut found = Vec::new();
        for pw in &self.pieces {
            if found.len() >= limit {
                break;
            }
            if pw.downloaded.load(Ordering::Relaxed) {
                continue;
            }
            if !self.wanted(pw.piece_work.index) {
                continue;
            }
            if pw.reserved.lock().unwrap().is_none() {
                continue;
            }
            if !peer_has.has_piece(pw.piece_work.index as usize) {
                continue;
            }
            let received: HashSet<u32> =
                pw.chuncks.lock().unwrap().iter().map(|c| c.start).collect();
            let mut offset = 0u32;
            while offset < pw.piece_work.length {
                if found.len() >= limit {
                    break;
                }
                let length = utils::calculate_block_size(pw.piece_work.length, offset);
                if !received.contains(&offset) {
                    let req = BlockRequest {
                        index: pw.piece_work.index,
                        begin: offset,
                        length,
                    };
                    match map.get(&req) {
                        None => found.push(req),
                        Some(entry) => {
                            let already = entry.requesters.contains(&peer);
                            let full = entry.requesters.len() >= picker::MAX_BLOCK_REQUESTERS;
                            let aged = entry.first_at.elapsed() >= ENDGAME_REREQUEST_AFTER;
                            if !already && !full && aged {
                                found.push(req);
                            }
                        }
                    }
                }
                offset += length;
            }
        }
        found
    }

    pub fn note_block_received(&self, req: BlockRequest, peer: PeerAddr) -> Vec<PeerAddr> {
        let mut map = self.block_assignments.lock().unwrap();
        let Some(entry) = map.remove(&req) else {
            return Vec::new();
        };
        entry
            .requesters
            .into_iter()
            .filter(|p| *p != peer)
            .collect()
    }

    pub fn unassign_peer_blocks(&self, peer: PeerAddr) {
        let mut map = self.block_assignments.lock().unwrap();
        map.retain(|_, entry| {
            entry.requesters.retain(|p| *p != peer);
            !entry.requesters.is_empty()
        });
    }

    pub fn apply_peer_bitfield(&self, previous: &Bitfield, next: &Bitfield) {
        self.availability.remove_bitfield(previous);
        self.availability.add_bitfield(next);
    }

    pub fn apply_peer_have(&self, bitfield: &mut Bitfield, index: u32) -> bool {
        let i = index as usize;
        if bitfield.has_piece(i) {
            return false;
        }
        bitfield.set_piece(i);
        self.availability.add_have(index);
        true
    }

    pub fn remove_peer_availability(&self, bitfield: &Bitfield) {
        self.availability.remove_bitfield(bitfield);
    }

    pub fn our_bitfield(&self) -> Bitfield {
        let mut bitfield = Bitfield::with_piece_count(self.pieces.len());
        for (i, pw) in self.pieces.iter().enumerate() {
            if pw.downloaded.load(std::sync::atomic::Ordering::Relaxed) {
                bitfield.set_piece(i);
            }
        }
        bitfield
    }

    pub fn missing_pieces(&self) -> Vec<u32> {
        self.pieces
            .iter()
            .enumerate()
            .filter(|(_, pw)| !pw.downloaded.load(std::sync::atomic::Ordering::Relaxed))
            .map(|(i, _)| i as u32)
            .collect()
    }

    pub fn reserved_and_not_downloaded(&self) -> Vec<u32> {
        self.pieces
            .iter()
            .enumerate()
            .filter(|(_, pw)| {
                pw.reserved.lock().unwrap().is_none()
                    && !pw.downloaded.load(std::sync::atomic::Ordering::Relaxed)
            })
            .map(|(i, _)| i as u32)
            .collect()
    }

    pub async fn get_and_reserve_piece(&self, peer: PeerAddr) -> Option<&PieceWorkState> {
        self.get_and_reserve_piece_if(peer, |_| true).await
    }

    pub async fn get_and_reserve_piece_if(
        &self,
        peer: PeerAddr,
        has_piece: impl Fn(u32) -> bool,
    ) -> Option<&PieceWorkState> {
        let mut peer_has = Bitfield::with_piece_count(self.pieces.len());
        for i in 0..self.pieces.len() {
            if has_piece(i as u32) {
                peer_has.set_piece(i);
            }
        }
        let index = self.pick(peer, &peer_has, &[])?;
        self.pieces.get(index as usize)
    }

    pub fn try_reserve_piece(&self, index: u32, peer: PeerAddr) -> Option<&PieceWorkState> {
        let pw = self.pieces.get(index as usize)?;
        if pw.downloaded.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        let mut reserved = pw.reserved.lock().unwrap();
        if reserved.is_some() {
            return None;
        }
        reserved.replace(peer);
        Some(pw)
    }

    pub fn reserve_first_available(
        &self,
        peer: PeerAddr,
        candidates: &[u32],
    ) -> Option<&PieceWorkState> {
        for &index in candidates {
            if let Some(pw) = self.try_reserve_piece(index, peer) {
                return Some(pw);
            }
        }
        None
    }

    pub async fn get_and_reserve_piece_preferring(
        &self,
        peer: PeerAddr,
        preferred: &[u32],
    ) -> Option<&PieceWorkState> {
        let peer_has = Bitfield::filled(self.pieces.len());
        let index = self.pick(peer, &peer_has, preferred)?;
        self.pieces.get(index as usize)
    }

    pub fn remove_downloaded(&self, index: u32) {
        for pw in self.pieces.iter() {
            if pw.piece_work.index == index {
                pw.chuncks.lock().unwrap().clear();
                pw.write_claimed.store(false, Ordering::Relaxed);
                if pw.downloaded.swap(false, Ordering::Relaxed) {
                    self.verified.dec();
                }
            }
        }
    }

    /// First successful hash for this piece wins. Write failure clears via `remove_downloaded`.
    pub fn claim_write(&self, index: u32) -> bool {
        self.pieces
            .get(index as usize)
            .is_some_and(|pw| !pw.write_claimed.swap(true, Ordering::Relaxed))
    }

    pub fn release_piece(&self, index: u32) {
        if let Some(pw) = self.pieces.get(index as usize) {
            *pw.reserved.lock().unwrap() = None;
            if !pw.downloaded.load(std::sync::atomic::Ordering::Relaxed) {
                pw.chuncks.lock().unwrap().clear();
            }
        }
    }

    pub fn remove_reserved(&self, peer: PeerAddr) {
        self.release_peer_reservations(peer, true);
        self.unassign_peer_blocks(peer);
    }

    pub fn snub_release(&self, peer: PeerAddr) {
        self.release_peer_reservations(peer, false);
    }

    fn release_peer_reservations(&self, peer: PeerAddr, clear_chunks: bool) {
        for pw in self.pieces.iter() {
            let mut reserved = pw.reserved.lock().unwrap();
            if let Some(p) = reserved.as_ref() {
                if *p == peer {
                    reserved.take();
                    if clear_chunks && !pw.downloaded.load(Ordering::Relaxed) {
                        pw.chuncks.lock().unwrap().clear();
                    }
                }
            }
        }
    }

    pub fn release_reservation(&self, index: u32, peer: PeerAddr) -> bool {
        let Some(pw) = self.pieces.get(index as usize) else {
            return false;
        };
        let mut reserved = pw.reserved.lock().unwrap();
        if reserved.as_ref() == Some(&peer) {
            reserved.take();
            if !pw.downloaded.load(std::sync::atomic::Ordering::Relaxed) {
                pw.chuncks.lock().unwrap().clear();
            }
            true
        } else {
            false
        }
    }

    pub fn set_chuncks(&self, index: u32, start: u32, buf: Vec<u8>, peer: PeerAddr) -> bool {
        let Some(pw) = self.pieces.iter().find(|pw| pw.piece_work.index == index) else {
            return false;
        };
        let mut chuncks = pw.chuncks.lock().unwrap();
        if chuncks.iter().any(|c| c.start == start) {
            self.duplicate_bytes
                .fetch_add(buf.len() as u64, Ordering::Relaxed);
            return false;
        }
        chuncks.push(Chunk {
            index,
            start,
            length: buf.len() as u32,
            buf,
            peer,
        });
        true
    }

    pub fn piece_contributors(&self, index: u32) -> Vec<PeerAddr> {
        let Some(pw) = self.pieces.get(index as usize) else {
            return Vec::new();
        };
        let mut peers = Vec::new();
        for chunk in pw.chuncks.lock().unwrap().iter() {
            if !peers.contains(&chunk.peer) {
                peers.push(chunk.peer);
            }
        }
        peers
    }

    pub fn set_downloaded_if_all_chunks(&self, index: u32) -> Option<&PieceWorkState> {
        let pw = self.pieces.get(index as usize)?;
        if pw.downloaded.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        if !piece_chunks_cover(pw) {
            return None;
        }
        let already = pw
            .downloaded
            .swap(true, std::sync::atomic::Ordering::Relaxed);
        if already {
            None
        } else {
            self.verified.inc();
            self.piece_notify.notify_waiters();
            Some(pw)
        }
    }
}

pub struct PieceWorkState {
    pub piece_work: PieceWork,
    pub chuncks: Mutex<Vec<Chunk>>,
    pub downloaded: AtomicBool,
    pub reserved: Mutex<Option<PeerAddr>>,
    write_claimed: AtomicBool,
}

impl PieceWorkState {
    pub fn new(piece_work: PieceWork) -> Self {
        Self {
            piece_work,
            chuncks: Mutex::new(vec![]),
            downloaded: AtomicBool::new(false),
            reserved: Mutex::new(None),
            write_claimed: AtomicBool::new(false),
        }
    }
}

fn piece_chunks_cover(pw: &PieceWorkState) -> bool {
    let mut spans: Vec<(u32, u32)> = pw
        .chuncks
        .lock()
        .unwrap()
        .iter()
        .map(|c| (c.start, c.length))
        .collect();
    spans.sort_unstable();
    let mut covered = 0u32;
    for (start, len) in spans {
        if start != covered {
            return false;
        }
        covered = covered.saturating_add(len);
    }
    covered == pw.piece_work.length
}

impl PieceWorkState {
    pub fn chunk_to_buf(&self) -> Vec<u8> {
        let chuncks = self.chuncks.lock().unwrap();
        let mut buf = vec![0u8; self.piece_work.length as usize];
        for chunk in chuncks.iter() {
            let start = chunk.start as usize;
            let end = start.saturating_add(chunk.buf.len());
            if end <= buf.len() {
                buf[start..end].copy_from_slice(&chunk.buf);
            }
        }
        buf
    }
}

pub struct Chunk {
    pub index: u32,
    pub start: u32,
    pub length: u32,
    pub buf: Vec<u8>,
    pub peer: PeerAddr,
}

pub struct FullPiece {
    pub index: u32,
    pub length: u32,
    pub buf: Vec<u8>,
}

impl PieceWorkState {
    pub fn set_downloaded(&self) {
        if self.chuncks.lock().unwrap().len() == self.piece_work.length as usize {
            self.downloaded
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub struct PeerHandlerConfig {
    pub peer: PeerAddr,
    pub piece_tx: flume::Sender<FullPiece>,
    pub peer_writer_tx: flume::Sender<WriterRequest>,
    pub peers_state: Arc<PeerStates>,
    pub torrent_downloaded_state: Arc<TorrentDownloadedState>,
    pub download_state: Arc<Mutex<DownloadState>>,
    pub storage: Arc<Storage>,
    pub uploaded: Arc<AtomicU64>,
    pub torrent: Arc<Torrent>,
    pub choke_notify: Arc<Notify>,
    pub extensions: ExtensionRegistry,
    pub listen_port: u16,
    pub metadata: Arc<MetadataStore>,
    pub info_hash: [u8; 20],
    pub advertise_dht: bool,
    pub dht_port: Option<u16>,
    pub dht: Option<crate::dht::DhtHandle>,
}

pub struct PeerHandler {
    unchoke_notify: Notify,
    on_bitfield_notify: Notify,
    chocked: AtomicBool,
    downloaded: AtomicU32,
    peers_state: Arc<PeerStates>,
    piece_tx: flume::Sender<FullPiece>,
    peer_writer_tx: flume::Sender<WriterRequest>,
    requests_sem: Semaphore,
    peer: PeerAddr,
    torrent_downloaded_state: Arc<TorrentDownloadedState>,
    download_state: Arc<Mutex<DownloadState>>,
    storage: Arc<Storage>,
    uploaded: Arc<AtomicU64>,
    _torrent: Arc<Torrent>,
    choke_notify: Arc<Notify>,
    upload_queue: Mutex<VecDeque<BlockRequest>>,
    fast_extension: AtomicBool,
    peer_allowed_fast: Mutex<HashSet<u32>>,
    our_allowed_fast: Mutex<HashSet<u32>>,
    suggested_pieces: Mutex<Vec<u32>>,
    outstanding_requests: Mutex<HashMap<BlockRequest, tokio::time::Instant>>,
    request_storm: Mutex<RequestStorm>,
    extension_protocol: AtomicBool,
    extensions: Mutex<ExtensionSession>,
    listen_port: u16,
    metadata: Arc<MetadataStore>,
    advertise_dht: bool,
    dht_port: Option<u16>,
    dht: Option<crate::dht::DhtHandle>,
    peer_dht: AtomicBool,
    snubbed: AtomicBool,
    last_block_at: Mutex<Option<tokio::time::Instant>>,
    first_request_at: Mutex<Option<tokio::time::Instant>>,
    interested_since: Mutex<Option<tokio::time::Instant>>,
    pipeline_depth: AtomicUsize,
    peer_reqq: AtomicUsize,
    pipeline_armed: AtomicBool,
    pipeline_debt: AtomicUsize,
    rate_window_start: Mutex<tokio::time::Instant>,
    rate_window_bytes: AtomicU64,
    last_rate: AtomicU64,
    pipeline_notify: Notify,
}

impl PeerHandler {
    pub fn from_config(config: PeerHandlerConfig) -> Self {
        Self {
            unchoke_notify: Notify::new(),
            on_bitfield_notify: Notify::new(),
            downloaded: AtomicU32::new(0),
            chocked: AtomicBool::new(true),
            peers_state: config.peers_state.clone(),
            requests_sem: Semaphore::new(0),
            piece_tx: config.piece_tx,
            peer_writer_tx: config.peer_writer_tx,
            peer: config.peer,
            torrent_downloaded_state: config.torrent_downloaded_state,
            download_state: config.download_state,
            storage: config.storage,
            uploaded: config.uploaded,
            _torrent: config.torrent,
            choke_notify: config.choke_notify,
            upload_queue: Mutex::new(VecDeque::new()),
            fast_extension: AtomicBool::new(false),
            peer_allowed_fast: Mutex::new(HashSet::new()),
            our_allowed_fast: Mutex::new(HashSet::new()),
            suggested_pieces: Mutex::new(Vec::new()),
            outstanding_requests: Mutex::new(HashMap::new()),
            request_storm: Mutex::new(RequestStorm::default()),
            extension_protocol: AtomicBool::new(false),
            extensions: Mutex::new(config.extensions.bind(&ExtensionContext {
                info_hash: config.info_hash,
                peer: config.peer,
                metadata: config.metadata.clone(),
                peer_states: config.peers_state.clone(),
            })),
            listen_port: config.listen_port,
            metadata: config.metadata,
            advertise_dht: config.advertise_dht,
            dht_port: config.dht_port,
            dht: config.dht,
            peer_dht: AtomicBool::new(false),
            snubbed: AtomicBool::new(false),
            last_block_at: Mutex::new(None),
            first_request_at: Mutex::new(None),
            interested_since: Mutex::new(None),
            pipeline_depth: AtomicUsize::new(INITIAL_PIPELINE),
            peer_reqq: AtomicUsize::new(DEFAULT_REQQ as usize),
            pipeline_armed: AtomicBool::new(false),
            pipeline_debt: AtomicUsize::new(0),
            rate_window_start: Mutex::new(tokio::time::Instant::now()),
            rate_window_bytes: AtomicU64::new(0),
            last_rate: AtomicU64::new(0),
            pipeline_notify: Notify::new(),
        }
    }

    fn fast_extension(&self) -> bool {
        self.fast_extension.load(Ordering::Relaxed)
    }

    fn set_fast_extension(&self, enabled: bool) {
        self.fast_extension.store(enabled, Ordering::Relaxed);
        if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
            state.fast_extension = enabled;
        }
    }

    fn extension_protocol(&self) -> bool {
        self.extension_protocol.load(Ordering::Relaxed)
    }

    fn set_extension_protocol(&self, enabled: bool) {
        self.extension_protocol.store(enabled, Ordering::Relaxed);
        if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
            state.extension_protocol = enabled;
        }
    }

    fn require_fast(&self) -> Result<(), anyhow::Error> {
        if !self.fast_extension() {
            anyhow::bail!("fast extension message without negotiation");
        }
        Ok(())
    }

    fn send_reject(&self, req: BlockRequest) {
        if !self.fast_extension() {
            return;
        }
        let _ = self
            .peer_writer_tx
            .send(WriterRequest::Message(format_reject_request(
                req.index, req.begin, req.length,
            )));
    }

    fn reject_upload_queue(&self, keep_allowed_fast: bool) {
        let mut queue = self.upload_queue.lock().unwrap();
        if !self.fast_extension() {
            queue.clear();
            return;
        }
        let allowed = self.our_allowed_fast.lock().unwrap().clone();
        let mut keep = VecDeque::new();
        while let Some(req) = queue.pop_front() {
            if keep_allowed_fast && allowed.contains(&req.index) {
                keep.push_back(req);
            } else {
                let _ = self
                    .peer_writer_tx
                    .send(WriterRequest::Message(format_reject_request(
                        req.index, req.begin, req.length,
                    )));
            }
        }
        *queue = keep;
    }

    fn requeue_outstanding(&self) {
        let outstanding: Vec<BlockRequest> = self
            .outstanding_requests
            .lock()
            .unwrap()
            .drain()
            .map(|(req, _)| req)
            .collect();
        let mut pieces = HashSet::new();
        for req in outstanding {
            pieces.insert(req.index);
        }
        self.disarm_pipeline();
        self.torrent_downloaded_state
            .unassign_peer_blocks(self.peer);
        for index in pieces {
            self.torrent_downloaded_state
                .release_reservation(index, self.peer);
        }
    }

    fn on_reject_request(&self, req: BlockRequest) -> Result<(), anyhow::Error> {
        let known = self.outstanding_requests.lock().unwrap().remove(&req);
        if known.is_none() {
            anyhow::bail!("reject for request that was never sent");
        }
        self.torrent_downloaded_state
            .release_reservation(req.index, self.peer);
        self.refill_pipeline_slot();
        Ok(())
    }

    fn is_allowed_fast_for_peer(&self, index: u32) -> bool {
        self.our_allowed_fast.lock().unwrap().contains(&index)
    }

    fn preferred_piece_indices(&self) -> Vec<u32> {
        let mut preferred = self.suggested_pieces.lock().unwrap().clone();
        preferred.extend(self.peer_allowed_fast.lock().unwrap().iter().copied());
        preferred
    }

    fn is_snubbed(&self) -> bool {
        self.snubbed.load(Ordering::Relaxed)
    }

    fn set_snubbed(&self, snubbed: bool) {
        self.snubbed.store(snubbed, Ordering::Relaxed);
        if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
            state.set_snubbed(snubbed);
        }
    }

    fn max_pipeline(&self) -> usize {
        self.peer_reqq
            .load(Ordering::Relaxed)
            .clamp(1, MAX_PIPELINE)
    }

    fn arm_pipeline(&self) {
        if !self.pipeline_armed.swap(true, Ordering::Relaxed) {
            let depth = self.pipeline_depth.load(Ordering::Relaxed).max(1);
            self.requests_sem.add_permits(depth);
        }
    }

    fn disarm_pipeline(&self) {
        self.pipeline_armed.store(false, Ordering::Relaxed);
        self.pipeline_debt.store(0, Ordering::Relaxed);
        while let Ok(permit) = self.requests_sem.try_acquire() {
            permit.forget();
        }
    }

    fn refill_pipeline_slot(&self) {
        loop {
            let debt = self.pipeline_debt.load(Ordering::Relaxed);
            if debt == 0 {
                self.requests_sem.add_permits(1);
                self.pipeline_notify.notify_waiters();
                return;
            }
            if self
                .pipeline_debt
                .compare_exchange(debt, debt - 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                self.pipeline_notify.notify_waiters();
                return;
            }
        }
    }

    fn set_pipeline_depth(&self, new: usize) {
        let new = new.clamp(1, self.max_pipeline());
        let old = self.pipeline_depth.swap(new, Ordering::Relaxed);
        if new == old {
            return;
        }
        if new > old {
            if self.pipeline_armed.load(Ordering::Relaxed) {
                self.requests_sem.add_permits(new - old);
            }
        } else {
            self.pipeline_debt.fetch_add(old - new, Ordering::Relaxed);
        }
        self.pipeline_notify.notify_waiters();
    }

    fn grow_pipeline(&self) {
        let current = self.pipeline_depth.load(Ordering::Relaxed);
        let max = self.max_pipeline();
        if current < max {
            self.set_pipeline_depth(current + 1);
        }
    }

    fn halve_pipeline(&self) {
        let current = self.pipeline_depth.load(Ordering::Relaxed);
        self.set_pipeline_depth((current / 2).max(1));
    }

    fn on_delivery_progress(&self, bytes: u32) {
        let now = tokio::time::Instant::now();
        *self.last_block_at.lock().unwrap() = Some(now);
        if self.is_snubbed() {
            self.set_snubbed(false);
        }
        self.rate_window_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        let mut start = self.rate_window_start.lock().unwrap();
        let elapsed = now.saturating_duration_since(*start);
        if elapsed >= Duration::from_millis(200) {
            let window_bytes = self.rate_window_bytes.swap(0, Ordering::Relaxed);
            let rate = window_bytes.saturating_mul(1000) / elapsed.as_millis().max(1) as u64;
            let prev = self.last_rate.swap(rate, Ordering::Relaxed);
            if rate > prev && prev > 0 {
                self.grow_pipeline();
            }
            *start = now;
        }
    }

    fn maybe_snub(&self) {
        if self.is_snubbed() {
            return;
        }
        let now = tokio::time::Instant::now();
        let outstanding = !self.outstanding_requests.lock().unwrap().is_empty();
        let last_block = *self.last_block_at.lock().unwrap();
        let first_request = *self.first_request_at.lock().unwrap();
        let interested_since = *self.interested_since.lock().unwrap();
        let start = last_block.or(first_request).or(interested_since);
        let waiting =
            outstanding || (self.chocked.load(Ordering::Relaxed) && interested_since.is_some());
        if !waiting {
            return;
        }
        if let Some(start) = start {
            if now.saturating_duration_since(start) >= SNUB_TIMEOUT {
                debug!("snubbing peer {}", self.peer);
                self.set_snubbed(true);
                self.torrent_downloaded_state.snub_release(self.peer);
            }
        }
    }

    fn expire_stale_requests(&self) {
        let now = tokio::time::Instant::now();
        let stale: Vec<BlockRequest> = self
            .outstanding_requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(req, sent)| {
                if now.saturating_duration_since(*sent) >= REQUEST_TIMEOUT {
                    Some(*req)
                } else {
                    None
                }
            })
            .collect();
        if stale.is_empty() {
            return;
        }
        self.halve_pipeline();
        for req in stale {
            self.outstanding_requests.lock().unwrap().remove(&req);
            let _ = self
                .peer_writer_tx
                .send(WriterRequest::Message(message::format_cancel(
                    req.index, req.begin, req.length,
                )));
            self.torrent_downloaded_state
                .unassign_peer_blocks(self.peer);
            self.torrent_downloaded_state
                .release_reservation(req.index, self.peer);
        }
    }

    fn peer_bitfield(&self) -> Bitfield {
        self.peers_state
            .states
            .get(&self.peer)
            .map(|s| s.bitfield.clone())
            .unwrap_or_else(|| {
                Bitfield::with_piece_count(self.torrent_downloaded_state.piece_count())
            })
    }

    fn refresh_interest(&self) -> anyhow::Result<()> {
        let wanted = self.is_downloading()
            && !self.torrent_downloaded_state.is_complete()
            && (self.peer_has_needed_piece() || !self.needed_allowed_fast_pieces().is_empty());
        let current = self
            .peers_state
            .states
            .get(&self.peer)
            .map(|s| s.am_interested)
            .unwrap_or(false);
        if wanted == current {
            return Ok(());
        }
        self.peer_writer_tx.send(if wanted {
            trace!("sending interested");
            WriterRequest::Message(Message::Interested)
        } else {
            trace!("sending not interested");
            WriterRequest::Message(Message::NotInterested)
        })?;
        if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
            state.set_am_interested(wanted);
        }
        let mut since = self.interested_since.lock().unwrap();
        *since = if wanted {
            Some(tokio::time::Instant::now())
        } else {
            None
        };
        Ok(())
    }

    fn cancel_other_requesters(&self, others: Vec<PeerAddr>, req: BlockRequest) {
        for other in others {
            if let Some(state) = self.peers_state.states.get(&other) {
                state.stats.download_cancels.lock().unwrap().push(req);
                if let Some(tx) = &state.writer_tx {
                    let _ = tx.send(WriterRequest::Message(message::format_cancel(
                        req.index, req.begin, req.length,
                    )));
                }
            }
        }
    }

    fn drain_download_cancels(&self) {
        let cancels = self
            .peers_state
            .states
            .get(&self.peer)
            .map(|s| {
                s.stats
                    .download_cancels
                    .lock()
                    .unwrap()
                    .drain(..)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for req in cancels {
            if self
                .outstanding_requests
                .lock()
                .unwrap()
                .remove(&req)
                .is_some()
            {
                self.refill_pipeline_slot();
            }
        }
    }

    pub fn on_peer_died(&self) {
        if let Some((_, state)) = self.peers_state.states.remove(&self.peer) {
            self.torrent_downloaded_state
                .remove_peer_availability(&state.bitfield);
        }
        self.torrent_downloaded_state.remove_reserved(self.peer);
    }

    pub fn should_transmit_have(&self, id: u32) -> bool {
        if let Some(state) = self.peers_state.states.get(&self.peer) {
            !state.bitfield.has_piece(id as usize)
        } else {
            false
        }
    }

    pub fn get_download_state(&self) -> DownloadState {
        *self.download_state.lock().unwrap()
    }

    pub fn is_downloading(&self) -> bool {
        self.get_download_state() == DownloadState::Downloading
    }

    fn peer_has_needed_piece(&self) -> bool {
        let Some(state) = self.peers_state.states.get(&self.peer) else {
            return false;
        };
        self.torrent_downloaded_state
            .pieces
            .iter()
            .enumerate()
            .any(|(i, pw)| {
                !pw.downloaded.load(Ordering::Relaxed)
                    && self.torrent_downloaded_state.wanted(i as u32)
                    && state.bitfield.has_piece(i)
            })
    }

    fn am_choking(&self) -> bool {
        self.peers_state
            .states
            .get(&self.peer)
            .map(|s| s.stats.am_choking.load(Ordering::Relaxed))
            .unwrap_or(true)
    }

    fn on_incoming_request(&self, payload: Vec<u8>) -> Result<(), anyhow::Error> {
        let req = BlockRequest::from_payload(&payload)
            .ok_or_else(|| anyhow::anyhow!("truncated request from peer"))?;
        let piece_length = self
            .torrent_downloaded_state
            .piece_length(req.index)
            .ok_or_else(|| anyhow::anyhow!("request for unknown piece {}", req.index))?;
        let have_piece = self.torrent_downloaded_state.has_piece(req.index);
        match validate_request(&req, piece_length, have_piece) {
            Ok(()) => {}
            Err(RequestError::MissingPiece) if self.fast_extension() => {
                self.send_reject(req);
                return Ok(());
            }
            Err(e) => {
                return Err(anyhow::anyhow!("invalid request {:?}: {:?}", req, e));
            }
        }

        if !self.is_downloading() {
            debug!("rejecting request while paused {:?}", req);
            self.send_reject(req);
            return Ok(());
        }

        if self.am_choking() && !(self.fast_extension() && self.is_allowed_fast_for_peer(req.index))
        {
            debug!("ignoring request while choking {:?}", req);
            self.send_reject(req);
            self.request_storm
                .lock()
                .unwrap()
                .on_choked_request()
                .map_err(|_| anyhow::anyhow!("request storm while choked"))?;
            return Ok(());
        }
        self.request_storm.lock().unwrap().reset_choked();

        let mut queue = self.upload_queue.lock().unwrap();
        if queue.len() >= MAX_UPLOAD_QUEUE {
            debug!("upload queue full, dropping request {:?}", req);
            self.send_reject(req);
            drop(queue);
            self.request_storm
                .lock()
                .unwrap()
                .on_queue_overflow()
                .map_err(|_| anyhow::anyhow!("upload queue exceeded repeatedly"))?;
            return Ok(());
        }
        queue.push_back(req);
        drop(queue);
        if let Some(state) = self.peers_state.states.get(&self.peer) {
            state.stats.upload_notify.notify_waiters();
        }
        Ok(())
    }

    pub async fn task_peer_uploader(&self) -> Result<(), anyhow::Error> {
        loop {
            if !self.is_downloading() {
                self.reject_upload_queue(false);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }

            let stats = self
                .peers_state
                .states
                .get(&self.peer)
                .map(|s| s.stats.clone());
            if let Some(stats) = stats {
                stats.upload_notify.notified().await;
            } else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            loop {
                if !self.is_downloading() {
                    self.reject_upload_queue(false);
                    break;
                }
                if self.am_choking() {
                    self.reject_upload_queue(true);
                    if self.upload_queue.lock().unwrap().is_empty() {
                        break;
                    }
                }
                let req = self.upload_queue.lock().unwrap().pop_front();
                let Some(req) = req else {
                    break;
                };
                if self.am_choking() && !self.is_allowed_fast_for_peer(req.index) {
                    self.send_reject(req);
                    continue;
                }
                let data = match self
                    .storage
                    .read_block(req.index, req.begin, req.length)
                    .await
                {
                    Ok(data) => data,
                    Err(e) => {
                        debug!("failed to read block {:?}: {:#}", req, e);
                        self.send_reject(req);
                        continue;
                    }
                };
                let length = data.len() as u64;
                if self
                    .peer_writer_tx
                    .send(WriterRequest::Message(message::format_piece(
                        req.index, req.begin, data,
                    )))
                    .is_err()
                {
                    return Ok(());
                }
                self.uploaded.fetch_add(length, Ordering::Relaxed);
                if let Some(state) = self.peers_state.states.get(&self.peer) {
                    state
                        .stats
                        .bytes_uploaded
                        .fetch_add(length, Ordering::Relaxed);
                }
            }
        }
    }

    fn peer_has_piece(&self, index: u32) -> bool {
        self.peers_state
            .states
            .get(&self.peer)
            .map(|state| state.bitfield.has_piece(index as usize))
            .unwrap_or(false)
    }

    fn needed_allowed_fast_pieces(&self) -> Vec<u32> {
        self.peer_allowed_fast
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|&index| {
                !self.torrent_downloaded_state.has_piece(index) && self.peer_has_piece(index)
            })
            .collect()
    }

    async fn acquire_pipeline_slot(&self) -> Result<bool, anyhow::Error> {
        loop {
            self.expire_stale_requests();
            self.maybe_snub();
            match timeout(REQUEST_TIMEOUT, self.requests_sem.acquire()).await {
                Ok(acq) => {
                    acq?.forget();
                    return Ok(true);
                }
                Err(_) => {
                    self.halve_pipeline();
                    self.expire_stale_requests();
                    if !self.is_downloading() {
                        return Ok(false);
                    }
                }
            }
        }
    }

    async fn send_block_request(&self, req: BlockRequest) -> Result<bool, anyhow::Error> {
        if !self.is_downloading() {
            return Ok(false);
        }
        if !self.acquire_pipeline_slot().await? {
            return Ok(false);
        }
        if !self.torrent_downloaded_state.assign_block(req, self.peer) {
            self.refill_pipeline_slot();
            return Ok(true);
        }
        let now = tokio::time::Instant::now();
        {
            let mut first = self.first_request_at.lock().unwrap();
            if first.is_none() {
                *first = Some(now);
            }
        }
        self.outstanding_requests.lock().unwrap().insert(req, now);
        debug!(
            "requesting piece index {} start {} length {}",
            req.index, req.begin, req.length
        );
        if self
            .peer_writer_tx
            .send(WriterRequest::Message(message::format_request(
                req.index, req.begin, req.length,
            )))
            .is_err()
        {
            error!("error sending request to peer");
            return Ok(false);
        }
        Ok(true)
    }

    async fn request_piece_blocks(&self, piece: PieceWork) -> Result<(), anyhow::Error> {
        let mut offset: u32 = 0;
        while offset < piece.length {
            if !self.is_downloading() {
                return Ok(());
            }
            let block_size = utils::calculate_block_size(piece.length, offset);
            let req = BlockRequest {
                index: piece.index,
                begin: offset,
                length: block_size,
            };
            if !self.send_block_request(req).await? {
                return Ok(());
            }
            offset += block_size;
        }
        Ok(())
    }

    async fn request_endgame_blocks(&self) -> Result<(), anyhow::Error> {
        let peer_has = self.peer_bitfield();
        let depth = self.pipeline_depth.load(Ordering::Relaxed);
        let in_flight = self.outstanding_requests.lock().unwrap().len();
        let want = depth.saturating_sub(in_flight);
        let blocks = self
            .torrent_downloaded_state
            .endgame_blocks(self.peer, &peer_has, want);
        if blocks.is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Ok(());
        }
        for req in blocks {
            if !self.send_block_request(req).await? {
                return Ok(());
            }
        }
        Ok(())
    }

    // The job of this is to request chunks and also to keep peer alive.
    // The moment this ends, the peer is disconnected.
    pub async fn task_peer_chunk_requester(&self) -> Result<(), anyhow::Error> {
        loop {
            if !self.is_downloading() {
                self.refresh_interest()?;
                while !self.is_downloading() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }

            self.maybe_snub();
            self.expire_stale_requests();
            self.drain_download_cancels();
            self.refresh_interest()?;

            if self.torrent_downloaded_state.is_complete() {
                self.refresh_interest()?;
                trace!("torrent complete, staying connected to seed");
                future::pending::<()>().await;
            }

            let choked = self.chocked.load(Ordering::Relaxed);
            let can_request_fast = choked && !self.needed_allowed_fast_pieces().is_empty();
            let in_endgame = self.torrent_downloaded_state.in_endgame();
            let snubbed = self.is_snubbed();

            if !self.peer_has_needed_piece() && !can_request_fast && !in_endgame {
                tokio::select! {
                    _ = self.on_bitfield_notify.notified() => {}
                    _ = self.torrent_downloaded_state.piece_notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                }
                continue;
            }

            if choked {
                let allowed = self.needed_allowed_fast_pieces();
                if !allowed.is_empty() && (!snubbed || in_endgame) {
                    let mut allowed_bf =
                        Bitfield::with_piece_count(self.torrent_downloaded_state.piece_count());
                    for index in &allowed {
                        allowed_bf.set_piece(*index as usize);
                    }
                    if let Some(index) =
                        self.torrent_downloaded_state
                            .pick(self.peer, &allowed_bf, &allowed)
                    {
                        if let Some(pw) = self.torrent_downloaded_state.pieces.get(index as usize) {
                            let piece = pw.piece_work;
                            self.request_piece_blocks(piece).await?;
                            continue;
                        }
                    }
                }
                if in_endgame && can_request_fast {
                    self.request_endgame_blocks().await?;
                    continue;
                }
                trace!("waiting for unchoke");
                tokio::select! {
                    _ = self.unchoke_notify.notified() => {}
                    _ = self.on_bitfield_notify.notified() => {}
                    _ = self.torrent_downloaded_state.piece_notify.notified() => {}
                    _ = tokio::time::sleep(SNUB_TIMEOUT) => {
                        self.maybe_snub();
                    }
                }
                continue;
            }

            if in_endgame {
                self.request_endgame_blocks().await?;
                continue;
            }

            if snubbed {
                tokio::select! {
                    _ = self.pipeline_notify.notified() => {}
                    _ = self.torrent_downloaded_state.piece_notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {
                        if self.torrent_downloaded_state.in_endgame() {
                            self.request_endgame_blocks().await?;
                        }
                    }
                }
                continue;
            }

            let peer_has = self.peer_bitfield();
            let preferred: Vec<u32> = self
                .preferred_piece_indices()
                .into_iter()
                .filter(|&index| peer_has.has_piece(index as usize))
                .collect();
            if let Some(index) = self
                .torrent_downloaded_state
                .pick(self.peer, &peer_has, &preferred)
            {
                if let Some(pw) = self.torrent_downloaded_state.pieces.get(index as usize) {
                    let piece = pw.piece_work;
                    self.request_piece_blocks(piece).await?;
                    continue;
                }
            }

            if self.torrent_downloaded_state.in_endgame() {
                self.request_endgame_blocks().await?;
                continue;
            }

            tokio::select! {
                _ = self.on_bitfield_notify.notified() => {}
                _ = self.torrent_downloaded_state.piece_notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
    }

    fn on_received_message(&self, message: crate::message::Message) -> Result<(), anyhow::Error> {
        self.drain_download_cancels();
        match message {
            Message::Choke => {
                debug!("peer choked us");
                self.chocked.store(true, Ordering::Relaxed);
                if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
                    state.set_peer_choking(true);
                }
                if !self.fast_extension() {
                    self.requeue_outstanding();
                }
            }
            Message::Unchoke => {
                debug!("peer unchoked us");
                self.chocked.store(false, Ordering::Relaxed);
                if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
                    state.set_peer_choking(false);
                }
                self.unchoke_notify.notify_waiters();
                self.arm_pipeline();
            }
            Message::Interested => {
                debug!("peer is interested");
                if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
                    state.set_peer_interested(true);
                }
                self.choke_notify.notify_waiters();
            }
            Message::NotInterested => {
                debug!("peer is not interested");
                if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
                    state.set_peer_interested(false);
                }
                self.choke_notify.notify_waiters();
            }
            Message::Have(h) => {
                if let Some(mut p_state) = self.peers_state.states.get_mut(&self.peer) {
                    self.torrent_downloaded_state
                        .apply_peer_have(&mut p_state.bitfield, h);
                }
                self.on_bitfield_notify.notify_waiters();
                self.refresh_interest()?;
            }
            Message::Bitfield(vec) => {
                debug!("peer sent bitfield");
                if let Some(mut ps) = self.peers_state.states.get_mut(&self.peer) {
                    let next = Bitfield::new(vec);
                    self.torrent_downloaded_state
                        .apply_peer_bitfield(&ps.bitfield, &next);
                    ps.bitfield = next;
                }
                self.on_bitfield_notify.notify_waiters();
                self.refresh_interest()?;
            }
            Message::Request(payload) => {
                self.on_incoming_request(payload)?;
            }
            Message::SuggestPiece(index) => {
                self.require_fast()?;
                debug!("peer suggested piece {}", index);
                let mut suggested = self.suggested_pieces.lock().unwrap();
                if !suggested.contains(&index) {
                    suggested.push(index);
                }
                if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
                    if !state.suggested_pieces.contains(&index) {
                        state.suggested_pieces.push(index);
                    }
                }
            }
            Message::HaveAll => {
                self.require_fast()?;
                debug!("peer sent have all");
                let count = self.torrent_downloaded_state.piece_count();
                if let Some(mut ps) = self.peers_state.states.get_mut(&self.peer) {
                    let next = Bitfield::filled(count);
                    self.torrent_downloaded_state
                        .apply_peer_bitfield(&ps.bitfield, &next);
                    ps.bitfield = next;
                }
                self.on_bitfield_notify.notify_waiters();
                self.refresh_interest()?;
            }
            Message::HaveNone => {
                self.require_fast()?;
                debug!("peer sent have none");
                let count = self.torrent_downloaded_state.piece_count();
                if let Some(mut ps) = self.peers_state.states.get_mut(&self.peer) {
                    let next = Bitfield::with_piece_count(count);
                    self.torrent_downloaded_state
                        .apply_peer_bitfield(&ps.bitfield, &next);
                    ps.bitfield = next;
                }
                self.on_bitfield_notify.notify_waiters();
                self.refresh_interest()?;
            }
            Message::RejectRequest {
                index,
                begin,
                length,
            } => {
                self.require_fast()?;
                debug!(
                    "peer rejected request index {} begin {} length {}",
                    index, begin, length
                );
                self.on_reject_request(BlockRequest {
                    index,
                    begin,
                    length,
                })?;
            }
            Message::AllowedFast(index) => {
                self.require_fast()?;
                debug!("peer allowed fast piece {}", index);
                self.peer_allowed_fast.lock().unwrap().insert(index);
                if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
                    state.peer_allowed_fast.insert(index);
                }
                self.on_bitfield_notify.notify_waiters();
                self.unchoke_notify.notify_waiters();
            }
            Message::Piece(piece_chunk) => {
                let req = BlockRequest {
                    index: piece_chunk.index,
                    begin: piece_chunk.start,
                    length: piece_chunk.length,
                };
                self.outstanding_requests.lock().unwrap().remove(&req);
                let others = self
                    .torrent_downloaded_state
                    .note_block_received(req, self.peer);
                self.cancel_other_requesters(others, req);
                self.downloaded
                    .fetch_add(piece_chunk.length, Ordering::Relaxed);
                if let Some(state) = self.peers_state.states.get(&self.peer) {
                    state
                        .stats
                        .bytes_downloaded
                        .fetch_add(piece_chunk.length as u64, Ordering::Relaxed);
                }
                self.on_delivery_progress(piece_chunk.length);
                self.refill_pipeline_slot();
                self.torrent_downloaded_state.set_chuncks(
                    piece_chunk.index,
                    piece_chunk.start,
                    piece_chunk.data,
                    self.peer,
                );
                if let Some(full_piece) = self
                    .torrent_downloaded_state
                    .set_downloaded_if_all_chunks(piece_chunk.index)
                {
                    let buf = full_piece.chunk_to_buf();

                    if utils::check_integrity(full_piece.piece_work.hash.as_ref(), &buf) {
                        trace!("piece index {} is correct", piece_chunk.index);
                        if self.torrent_downloaded_state.claim_write(piece_chunk.index) {
                            let full_piece = FullPiece {
                                index: piece_chunk.index,
                                length: full_piece.piece_work.length,
                                buf,
                            };

                            if self.piece_tx.send(full_piece).is_err() {
                                return Ok(());
                            }
                        }
                    } else {
                        trace!("piece index {} is corrupted", piece_chunk.index);
                        let contributors = self
                            .torrent_downloaded_state
                            .piece_contributors(piece_chunk.index);
                        let sole = contributors.len() == 1;
                        let mut banned_self = false;
                        for peer in contributors {
                            if self.peers_state.record_hash_failure(peer, sole) && peer == self.peer
                            {
                                banned_self = true;
                            }
                        }
                        self.torrent_downloaded_state
                            .remove_downloaded(piece_chunk.index);
                        self.torrent_downloaded_state
                            .release_piece(piece_chunk.index);
                        if banned_self {
                            return Err(anyhow::anyhow!("banned after hash failure"));
                        }
                    }
                }
                self.refresh_interest()?;

                trace!(
                    "peer received piece index {} start {} length {}",
                    piece_chunk.index,
                    piece_chunk.start,
                    piece_chunk.length
                );
            }
            Message::Cancel(payload) => {
                if let Some(req) = BlockRequest::from_payload(&payload) {
                    self.upload_queue
                        .lock()
                        .unwrap()
                        .retain(|queued| *queued != req);
                    debug!("peer canceled request {:?}", req);
                }
            }
            Message::Port(port) => {
                if let Some(dht) = &self.dht {
                    let addr = SocketAddr::new(self.peer.ip(), port);
                    dht.ping_node(addr);
                }
            }
            Message::Extended { ext_id, payload } => {
                if !self.extension_protocol() {
                    debug!("extended message without negotiation, ignoring");
                    return Ok(());
                }
                let outgoing = {
                    let mut session = self.extensions.lock().unwrap();
                    let outgoing = session.handle_extended(ext_id, payload);
                    if ext_id == 0 {
                        if let Some(reqq) = session.peer_info().reqq {
                            if reqq > 0 {
                                let reqq = (reqq as usize).clamp(1, MAX_PIPELINE);
                                self.peer_reqq.store(reqq, Ordering::Relaxed);
                                let depth = self.pipeline_depth.load(Ordering::Relaxed);
                                if depth > reqq {
                                    self.set_pipeline_depth(reqq);
                                }
                            }
                        }
                    }
                    outgoing
                };
                for msg in outgoing {
                    if self
                        .peer_writer_tx
                        .send(WriterRequest::Message(msg))
                        .is_err()
                    {
                        break;
                    }
                }
                if self.extensions.lock().unwrap().should_disconnect() {
                    return Err(anyhow::anyhow!("extension requested disconnect"));
                }
            }
            message => {
                debug!("received unsupported message {:?}, ignoring", message);
            }
        }

        Ok(())
    }
}

pub struct PeerConnection {
    pub handler: Arc<PeerHandler>,
    pub bitfield: Bitfield,
    pub peer: PeerAddr,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    connector: Arc<dyn Connector>,
}

impl PeerConnection {
    pub fn new(
        peer: PeerAddr,
        info_hash: [u8; 20],
        peer_id: [u8; 20],
        handler: Arc<PeerHandler>,
        connector: Arc<dyn Connector>,
    ) -> Self {
        Self {
            handler,
            bitfield: Bitfield::new(vec![]),
            peer,
            info_hash,
            peer_id,
            connector,
        }
    }

    pub async fn manage_peer_incoming(
        &self,
        peer_writer_rx: flume::Receiver<WriterRequest>,
        have_broadcast: tokio::sync::broadcast::Receiver<u32>,
    ) -> anyhow::Result<()> {
        let mut stream = self.connector.dial(self.peer).await?;

        let protocol = Arc::new(
            Protocol::connect(self.peer, self.info_hash, self.peer_id)
                .await?
                .with_piece_count(self.handler.torrent_downloaded_state.piece_count())
                .with_dht(self.handler.advertise_dht),
        );
        let handshake = protocol.complete_handshake(&mut stream).await?;
        self.handler
            .set_fast_extension(handshake.supports_fast_extension());
        self.handler
            .set_extension_protocol(handshake.supports_extension_protocol());
        if handshake.supports_dht() {
            self.handler.peer_dht.store(true, Ordering::Relaxed);
        }
        self.send_dht_port(&protocol, &mut stream, handshake.supports_dht())
            .await?;
        self.send_extension_handshake(&protocol, &mut stream)
            .await?;
        self.send_initial_bitfield(&protocol, &mut stream).await?;
        self.send_allowed_fast(&protocol, &mut stream).await?;
        self.manage_established(stream, protocol, peer_writer_rx, have_broadcast)
            .await
    }

    pub async fn manage_incoming_stream<S: PeerStream>(
        &self,
        mut stream: S,
        peer_writer_rx: flume::Receiver<WriterRequest>,
        have_broadcast: tokio::sync::broadcast::Receiver<u32>,
    ) -> anyhow::Result<()> {
        let protocol = Arc::new(
            Protocol::connect(self.peer, self.info_hash, self.peer_id)
                .await?
                .with_piece_count(self.handler.torrent_downloaded_state.piece_count())
                .with_dht(self.handler.advertise_dht),
        );
        if self.handler.peer_dht.load(Ordering::Relaxed) {
            self.send_dht_port(&protocol, &mut stream, true).await?;
        }
        self.send_extension_handshake(&protocol, &mut stream)
            .await?;
        self.send_initial_bitfield(&protocol, &mut stream).await?;
        self.send_allowed_fast(&protocol, &mut stream).await?;
        self.manage_established(stream, protocol, peer_writer_rx, have_broadcast)
            .await
    }

    async fn send_dht_port<S: PeerStream>(
        &self,
        protocol: &Protocol,
        stream: &mut S,
        peer_supports_dht: bool,
    ) -> anyhow::Result<()> {
        if !peer_supports_dht {
            return Ok(());
        }
        let Some(port) = self.handler.dht_port else {
            return Ok(());
        };
        if port == 0 {
            return Ok(());
        }
        protocol.send_message(stream, Message::Port(port)).await?;
        Ok(())
    }

    async fn send_extension_handshake<S: PeerStream>(
        &self,
        protocol: &Protocol,
        stream: &mut S,
    ) -> anyhow::Result<()> {
        if !self.handler.extension_protocol() {
            return Ok(());
        }
        let listen_port = (self.handler.listen_port != 0).then_some(self.handler.listen_port);
        let msg = self
            .handler
            .extensions
            .lock()
            .unwrap()
            .outgoing_handshake(listen_port, self.handler.metadata.metadata_size());
        protocol.send_message(stream, msg).await?;
        Ok(())
    }

    async fn send_initial_bitfield<S: PeerStream>(
        &self,
        protocol: &Protocol,
        stream: &mut S,
    ) -> anyhow::Result<()> {
        if self.handler.torrent_downloaded_state.piece_count() == 0 {
            if self.handler.fast_extension() {
                protocol.send_message(stream, Message::HaveNone).await?;
            }
            return Ok(());
        }
        let bitfield = self.handler.torrent_downloaded_state.our_bitfield();
        if self.handler.fast_extension() {
            let msg = if self.handler.torrent_downloaded_state.is_complete() {
                Message::HaveAll
            } else if bitfield.is_empty() {
                Message::HaveNone
            } else {
                Message::Bitfield(bitfield.as_bytes().to_vec())
            };
            protocol.send_message(stream, msg).await?;
        } else if !bitfield.is_empty() {
            protocol.send_bitfield(stream, bitfield.as_bytes()).await?;
        }
        Ok(())
    }

    async fn send_allowed_fast<S: PeerStream>(
        &self,
        protocol: &Protocol,
        stream: &mut S,
    ) -> anyhow::Result<()> {
        if !self.handler.fast_extension() {
            return Ok(());
        }
        let piece_count = self.handler.torrent_downloaded_state.piece_count() as u32;
        let set = generate_allowed_fast_for_ip(
            self.peer.ip(),
            &self.info_hash,
            piece_count,
            DEFAULT_ALLOWED_FAST_SET_SIZE,
        );
        {
            let mut ours = self.handler.our_allowed_fast.lock().unwrap();
            ours.extend(set.iter().copied());
        }
        if let Some(mut state) = self.handler.peers_state.states.get_mut(&self.peer) {
            state.our_allowed_fast.extend(set.iter().copied());
        }
        for index in set {
            protocol
                .send_message(&mut *stream, Message::AllowedFast(index))
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn manage_established<S: PeerStream>(
        &self,
        stream: S,
        protocol: Arc<Protocol>,
        peer_writer_rx: flume::Receiver<WriterRequest>,
        mut have_broadcast: tokio::sync::broadcast::Receiver<u32>,
    ) -> anyhow::Result<()> {
        let (mut read, mut write) = tokio::io::split(stream);
        let timeouts = protocol.timeouts;

        let writer = {
            async move {
                let mut broadcast_closed = false;
                loop {
                    let req = loop {
                        break tokio::select! {
                            r = have_broadcast.recv(), if !broadcast_closed => match r {
                                Ok(id) => {
                                    if self.handler.should_transmit_have(id) {
                                         WriterRequest::Message(Message::Have(id))
                                    } else {
                                        continue
                                    }
                                },
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                    broadcast_closed = true;
                                    debug!("broadcast channel closed, will not poll it anymore");
                                    continue
                                },
                                _ => continue
                            },
                            r = timeout(timeouts.keep_alive, peer_writer_rx.recv_async()) => match r {
                                Ok(Ok(msg)) =>{
                                    msg
                                },
                                Ok(Err(_)) => {
                                    error!("closing writer, channel closed");
                                    anyhow::bail!("closing writer, channel closed");
                                }
                                Err(_) => {
                                    debug!("timeout reading, let's keep alive");
                                    WriterRequest::Message(Message::KeepAlive)
                                },
                            }
                        };
                    };

                    let buf = match req {
                        WriterRequest::Disconnect => {
                            debug!("writer received disconnect");
                            break;
                        }
                        WriterRequest::Message(msg)
                            if msg.is_extended() && !self.handler.extension_protocol() =>
                        {
                            continue;
                        }
                        WriterRequest::Message(msg) => message::serialize(Some(msg)),
                    };

                    match timeout(timeouts.read_step, write.write_all(&buf)).await {
                        Ok(Ok(_)) => {
                            //debug!("sent message");
                        }
                        Ok(Err(e)) => {
                            debug!("error writing to peer: {:?}", e);
                            break;
                        }
                        Err(e) => {
                            debug!("timeout writing to peer: {:?}", e);
                            break;
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }
        };

        let reader = async move {
            let handshake_at = tokio::time::Instant::now();
            let mut last_inbound = handshake_at;
            let mut seen_first = false;
            loop {
                let frame = match protocol
                    .read_with_idle(&mut read, last_inbound, seen_first, handshake_at)
                    .await
                {
                    Ok(frame) => frame,
                    Err(e) => {
                        debug!("reader stop: {:?}", e);
                        break;
                    }
                };
                match frame {
                    Frame::Eof => {
                        debug!("peer disconnected");
                        break;
                    }
                    Frame::KeepAlive => {
                        last_inbound = tokio::time::Instant::now();
                        seen_first = true;
                    }
                    Frame::Unknown { id } => {
                        debug!("skipping unknown message id {id}");
                        last_inbound = tokio::time::Instant::now();
                        seen_first = true;
                    }
                    Frame::Message(msg) => {
                        last_inbound = tokio::time::Instant::now();
                        seen_first = true;
                        if let Err(e) = self.handler.on_received_message(msg) {
                            debug!("error processing message: {:?}", e);
                            break;
                        }
                    }
                }
            }

            Ok::<_, anyhow::Error>(())
        };

        let ticker = async {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let (outgoing, close) = {
                    let mut session = self.handler.extensions.lock().unwrap();
                    let outgoing = session.on_tick();
                    (outgoing, session.should_disconnect())
                };
                for msg in outgoing {
                    if self
                        .handler
                        .peer_writer_tx
                        .send(WriterRequest::Message(msg))
                        .is_err()
                    {
                        anyhow::bail!("extension tick writer closed");
                    }
                }
                if close {
                    let _ = self.handler.peer_writer_tx.send(WriterRequest::Disconnect);
                    anyhow::bail!("extension requested disconnect");
                }
            }
        };

        tokio::select! {
            r = reader => {
                trace!(result=?r, "reader is done, exiting");
                r
            }
            r = writer => {
                trace!(result=?r, "writer is done, exiting");
                r
            }
            r = ticker => {
                trace!(result=?r, "extension ticker is done, exiting");
                r
            }
        }
    }
}

pub struct SpawnPeerParams {
    pub peer: PeerAddr,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub piece_tx: flume::Sender<FullPiece>,
    pub have_broadcast: Arc<tokio::sync::broadcast::Sender<u32>>,
    pub torrent_downloaded_state: Arc<TorrentDownloadedState>,
    pub peer_states: Arc<PeerStates>,
    pub download_state: Arc<Mutex<DownloadState>>,
    pub storage: Arc<Storage>,
    pub uploaded: Arc<AtomicU64>,
    pub torrent: Arc<Torrent>,
    pub choke_notify: Arc<Notify>,
    pub incoming: Option<BoxedPeerStream>,
    pub connector: Arc<dyn Connector>,
    pub incoming_fast_extension: Option<bool>,
    pub incoming_extension_protocol: Option<bool>,
    pub incoming_dht: Option<bool>,
    pub extensions: ExtensionRegistry,
    pub listen_port: u16,
    pub metadata: Arc<MetadataStore>,
    pub advertise_dht: bool,
    pub dht_port: Option<u16>,
    pub dht: Option<crate::dht::DhtHandle>,
    pub global_peers: Arc<std::sync::atomic::AtomicUsize>,
    pub max_peers_per_torrent: usize,
    pub max_peers_global: usize,
}

struct PeerSlotGuard {
    global_peers: Arc<std::sync::atomic::AtomicUsize>,
    peer_states: Arc<PeerStates>,
    downloaded_state: Arc<TorrentDownloadedState>,
    peer: PeerAddr,
}

impl Drop for PeerSlotGuard {
    fn drop(&mut self) {
        if let Some((_, state)) = self.peer_states.states.remove(&self.peer) {
            self.downloaded_state
                .remove_peer_availability(&state.bitfield);
        }
        self.downloaded_state.remove_reserved(self.peer);
        self.global_peers.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn try_spawn_peer(params: SpawnPeerParams) -> bool {
    let global = params.global_peers.load(Ordering::Relaxed);
    if global >= params.max_peers_global {
        debug!(peer = %params.peer, "global connection cap reached");
        return false;
    }
    let already_seen = params.peer_states.states.contains_key(&params.peer);
    if !already_seen && params.peer_states.len() >= params.max_peers_per_torrent {
        debug!(peer = %params.peer, "per-torrent connection cap reached");
        return false;
    }
    if params.peer_states.is_banned(params.peer) {
        debug!(peer = %params.peer, "refusing banned peer");
        return false;
    }

    let (peer_writer_tx, peer_writer_rx) = flume::unbounded();
    if !params
        .peer_states
        .insert_live(params.peer, peer_writer_tx.clone())
    {
        return false;
    }
    params.global_peers.fetch_add(1, Ordering::Relaxed);
    let slot = PeerSlotGuard {
        global_peers: params.global_peers.clone(),
        peer_states: params.peer_states.clone(),
        downloaded_state: params.torrent_downloaded_state.clone(),
        peer: params.peer,
    };

    tokio::spawn(async move {
        let _slot = slot;
        let handler = Arc::new(PeerHandler::from_config(PeerHandlerConfig {
            peer: params.peer,
            piece_tx: params.piece_tx,
            peer_writer_tx,
            peers_state: params.peer_states.clone(),
            torrent_downloaded_state: params.torrent_downloaded_state,
            download_state: params.download_state,
            storage: params.storage,
            uploaded: params.uploaded,
            torrent: params.torrent,
            choke_notify: params.choke_notify,
            extensions: params.extensions,
            listen_port: params.listen_port,
            metadata: params.metadata,
            info_hash: params.info_hash,
            advertise_dht: params.advertise_dht,
            dht_port: params.dht_port,
            dht: params.dht,
        }));
        if let Some(fast) = params.incoming_fast_extension {
            handler.set_fast_extension(fast);
        }
        if let Some(extended) = params.incoming_extension_protocol {
            handler.set_extension_protocol(extended);
        }
        if params.incoming_dht == Some(true) {
            handler.peer_dht.store(true, Ordering::Relaxed);
        }
        let connection = PeerConnection::new(
            params.peer,
            params.info_hash,
            params.peer_id,
            handler.clone(),
            params.connector,
        );
        let requester = handler.task_peer_chunk_requester();
        let uploader = handler.task_peer_uploader();
        let have_rx = params.have_broadcast.subscribe();
        let result = match params.incoming {
            Some(stream) => {
                tokio::select! {
                    r = connection.manage_incoming_stream(stream, peer_writer_rx, have_rx) => r,
                    r = requester => r,
                    r = uploader => r,
                }
            }
            None => {
                tokio::select! {
                    r = connection.manage_peer_incoming(peer_writer_rx, have_rx) => r,
                    r = requester => r,
                    r = uploader => r,
                }
            }
        };
        if let Err(e) = result {
            debug!("error managing peer: {:#}", e);
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncWriteExt;

    fn piece(index: u32, length: u32) -> PieceWorkState {
        PieceWorkState::new(PieceWork {
            index,
            length,
            hash: [0; 20],
        })
    }

    fn state(n: u32, piece_len: u32) -> TorrentDownloadedState {
        let s = TorrentDownloadedState::with_seed((0..n).map(|i| piece(i, piece_len)).collect(), 1);
        s.set_bootstrap_until(0);
        s
    }

    fn peer(port: u16) -> PeerAddr {
        (std::net::Ipv4Addr::LOCALHOST, port).into()
    }

    #[tokio::test]
    async fn get_and_reserve_piece_assigns_distinct_peers_then_enters_endgame() {
        let s = state(3, 16);
        let p1 = peer(6881);
        let p2 = peer(6882);
        let p3 = peer(6883);
        let p4 = peer(6884);

        let a = s.get_and_reserve_piece(p1).await.unwrap();
        let i1 = a.piece_work.index;
        assert_eq!(*a.reserved.lock().unwrap(), Some(p1));

        let b = s.get_and_reserve_piece(p2).await.unwrap();
        let i2 = b.piece_work.index;
        assert_eq!(*b.reserved.lock().unwrap(), Some(p2));
        assert_ne!(i1, i2);

        let c = s.get_and_reserve_piece(p3).await.unwrap();
        let i3 = c.piece_work.index;
        assert_eq!(*c.reserved.lock().unwrap(), Some(p3));
        assert_ne!(i1, i3);
        assert_ne!(i2, i3);

        assert!(s.get_and_reserve_piece(p4).await.is_none());
        assert!(s.in_endgame());
        assert_eq!(*s.pieces[i1 as usize].reserved.lock().unwrap(), Some(p1));
        assert_eq!(*s.pieces[i2 as usize].reserved.lock().unwrap(), Some(p2));
        assert_eq!(*s.pieces[i3 as usize].reserved.lock().unwrap(), Some(p3));
    }

    #[tokio::test]
    async fn remove_reserved_clears_only_that_peer() {
        let s = state(3, 16);
        let p1 = peer(6881);
        let p2 = peer(6882);
        let p3 = peer(6883);

        let i1 = s.get_and_reserve_piece(p1).await.unwrap().piece_work.index;
        let i2 = s.get_and_reserve_piece(p2).await.unwrap().piece_work.index;
        let i3 = s.get_and_reserve_piece(p3).await.unwrap().piece_work.index;

        s.remove_reserved(p2);

        assert_eq!(*s.pieces[i1 as usize].reserved.lock().unwrap(), Some(p1));
        assert!(s.pieces[i2 as usize].reserved.lock().unwrap().is_none());
        assert_eq!(*s.pieces[i3 as usize].reserved.lock().unwrap(), Some(p3));
    }

    #[tokio::test]
    async fn rejected_request_releases_reservation_for_other_peers() {
        let s = state(2, 16);
        let p1 = peer(6881);
        let p2 = peer(6882);

        let first = s.get_and_reserve_piece(p1).await.unwrap().piece_work.index;
        assert_eq!(*s.pieces[first as usize].reserved.lock().unwrap(), Some(p1));
        assert!(s.release_reservation(first, p1));
        assert!(s.pieces[first as usize].reserved.lock().unwrap().is_none());

        let next = s.try_reserve_piece(first, p2).unwrap();
        assert_eq!(next.piece_work.index, first);
        assert_eq!(*next.reserved.lock().unwrap(), Some(p2));
        assert!(!s.release_reservation(first, p1));
        assert_eq!(*s.pieces[first as usize].reserved.lock().unwrap(), Some(p2));
    }

    #[tokio::test]
    async fn preferring_reserves_suggested_piece_first() {
        let s = state(3, 16);
        let p1 = peer(6881);
        let preferred = [2u32, 1];
        let pw = s
            .get_and_reserve_piece_preferring(p1, &preferred)
            .await
            .unwrap();
        assert_eq!(pw.piece_work.index, 2);
        assert_eq!(*pw.reserved.lock().unwrap(), Some(p1));
    }

    #[tokio::test]
    async fn get_and_reserve_piece_if_skips_pieces_the_peer_lacks() {
        let s = state(3, 16);
        let p1 = peer(6881);
        let taken = s
            .get_and_reserve_piece_if(p1, |index| index == 2)
            .await
            .unwrap();
        assert_eq!(taken.piece_work.index, 2);
        assert_eq!(*s.pieces[2].reserved.lock().unwrap(), Some(p1));
        assert!(s.pieces[0].reserved.lock().unwrap().is_none());
        assert!(s.pieces[1].reserved.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn release_piece_clears_reservation_and_incomplete_chunks() {
        let s = state(1, 16);
        let p1 = peer(6881);
        s.get_and_reserve_piece(p1).await.unwrap();
        s.set_chuncks(0, 0, vec![1u8; 8], p1);

        s.release_piece(0);

        assert!(s.pieces[0].reserved.lock().unwrap().is_none());
        assert!(s.pieces[0].chuncks.lock().unwrap().is_empty());
        assert!(!s.pieces[0].downloaded.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn remove_reserved_clears_incomplete_chunks() {
        let s = state(1, 16);
        let p1 = peer(6881);
        s.get_and_reserve_piece(p1).await.unwrap();
        s.set_chuncks(0, 0, vec![1u8; 8], p1);

        s.remove_reserved(p1);

        assert!(s.pieces[0].reserved.lock().unwrap().is_none());
        assert!(s.pieces[0].chuncks.lock().unwrap().is_empty());
    }

    #[test]
    fn set_chuncks_then_downloaded_when_lengths_sum() {
        let s = state(2, 16);

        s.set_chuncks(0, 0, vec![0u8; 8], peer(1));
        assert!(s.set_downloaded_if_all_chunks(0).is_none());
        assert!(!s.pieces[0].downloaded.load(Ordering::Relaxed));
        assert!(!s.is_complete());

        s.set_chuncks(0, 8, vec![1u8; 8], peer(1));
        let done = s.set_downloaded_if_all_chunks(0);
        assert!(done.is_some());
        assert_eq!(done.unwrap().piece_work.index, 0);
        assert!(s.pieces[0].downloaded.load(Ordering::Relaxed));
        assert!(!s.is_complete());

        s.set_chuncks(1, 0, vec![2u8; 16], peer(2));
        assert!(s.set_downloaded_if_all_chunks(1).is_some());
        assert!(s.set_downloaded_if_all_chunks(1).is_none());
        assert!(s.pieces[1].downloaded.load(Ordering::Relaxed));
        assert!(s.is_complete());
    }

    #[test]
    fn has_piece_and_bitfield_follow_downloaded_flags() {
        let s = state(3, 16);
        assert!(!s.has_piece(0));
        assert_eq!(s.piece_length(1), Some(16));
        assert_eq!(s.piece_count(), 3);
        s.mark_downloaded(1);
        assert!(s.has_piece(1));
        let bf = s.our_bitfield();
        assert!(!bf.has_piece(0));
        assert!(bf.has_piece(1));
        s.mark_all_downloaded();
        assert!(s.is_complete());
        assert!(s.has_piece(0) && s.has_piece(2));
    }

    #[test]
    fn remove_downloaded_clears_chunks_and_flag() {
        let s = state(1, 16);
        s.set_chuncks(0, 0, vec![7u8; 16], peer(1));
        assert!(s.set_downloaded_if_all_chunks(0).is_some());
        assert!(s.pieces[0].downloaded.load(Ordering::Relaxed));
        assert_eq!(s.pieces[0].chuncks.lock().unwrap().len(), 1);

        s.remove_downloaded(0);
        assert!(!s.pieces[0].downloaded.load(Ordering::Relaxed));
        assert!(s.pieces[0].chuncks.lock().unwrap().is_empty());
        assert!(!s.is_complete());
    }

    #[test]
    fn claim_write_once_until_removed() {
        let s = state(1, 16);
        assert!(s.claim_write(0));
        assert!(!s.claim_write(0));
        s.remove_downloaded(0);
        assert!(s.claim_write(0));
    }

    #[tokio::test]
    async fn concurrent_reservation_completes_each_piece_once() {
        const N: u16 = 16;
        const M: u32 = 8;
        const PIECE_LEN: u32 = 16;

        let s = Arc::new(state(M, PIECE_LEN));
        let mut set = tokio::task::JoinSet::new();

        for i in 0..N {
            let s = Arc::clone(&s);
            let peer = peer(6881 + i);
            set.spawn(async move {
                let mut completed = 0u32;
                while let Some(pw) = s.get_and_reserve_piece(peer).await {
                    let index = pw.piece_work.index;
                    s.set_chuncks(index, 0, vec![0u8; PIECE_LEN as usize], peer);
                    s.set_downloaded_if_all_chunks(index);
                    completed += 1;
                }
                completed
            });
        }

        let mut total_completed = 0u32;
        while let Some(res) = set.join_next().await {
            total_completed += res.unwrap();
        }

        assert_eq!(total_completed, M);
        assert!(s.is_complete());
        for pw in s.pieces.iter() {
            assert!(pw.downloaded.load(Ordering::Relaxed));
            let reserved = pw.reserved.lock().unwrap();
            assert!(reserved.is_some());
        }
    }

    #[test]
    fn set_chuncks_first_write_wins_attribution() {
        let s = state(1, 16);
        let p1 = peer(6881);
        let p2 = peer(6882);
        assert!(s.set_chuncks(0, 0, vec![1u8; 8], p1));
        assert!(!s.set_chuncks(0, 0, vec![2u8; 8], p2));
        assert!(s.set_chuncks(0, 8, vec![3u8; 8], p2));
        let contributors = s.piece_contributors(0);
        assert_eq!(contributors, vec![p1, p2]);
        assert_eq!(s.pieces[0].chuncks.lock().unwrap()[0].buf[0], 1);
        assert_eq!(s.duplicate_bytes(), 8);
    }

    #[test]
    fn pick_chooses_rarest_piece_first() {
        let s = state(4, 16);
        let p1 = peer(6881);
        for index in [0u32, 1, 3] {
            s.availability.add_have(index);
            s.availability.add_have(index);
        }
        s.availability.add_have(2);
        let all = Bitfield::filled(4);
        let picked = s.pick(p1, &all, &[]).unwrap();
        assert_eq!(picked, 2);
        assert_eq!(*s.pieces[2].reserved.lock().unwrap(), Some(p1));
    }

    #[test]
    fn pick_prefer_beats_rarity() {
        let s = state(3, 16);
        s.availability.add_have(0);
        s.availability.add_have_all();
        s.availability.add_have_all();
        let p1 = peer(1);
        let picked = s.pick(p1, &Bitfield::filled(3), &[1]).unwrap();
        assert_eq!(picked, 1);
    }

    #[test]
    fn endgame_two_requesters_then_cancel_list() {
        let s = state(1, 16);
        let p1 = peer(6881);
        let p2 = peer(6882);
        let p3 = peer(6883);
        assert!(s.try_reserve_piece(0, p1).is_some());
        assert!(s.in_endgame());
        let req = BlockRequest {
            index: 0,
            begin: 0,
            length: 16,
        };
        assert!(s.assign_block(req, p1));
        assert!(s.assign_block(req, p2));
        assert!(!s.assign_block(req, p3));
        let others = s.note_block_received(req, p1);
        assert_eq!(others, vec![p2]);
        assert!(s.set_chuncks(0, 0, vec![0u8; 16], p1));
        let has = Bitfield::filled(1);
        assert!(s.endgame_blocks(p3, &has, 4).is_empty());
    }

    #[test]
    fn endgame_requests_unassigned_blocks_on_reserved_piece() {
        let s = state(1, 16);
        let p1 = peer(6881);
        let p2 = peer(6882);
        assert!(s.try_reserve_piece(0, p1).is_some());
        let has = Bitfield::filled(1);
        let blocks = s.endgame_blocks(p2, &has, 4);
        assert_eq!(
            blocks,
            vec![BlockRequest {
                index: 0,
                begin: 0,
                length: 16,
            }]
        );
    }

    #[test]
    fn availability_have_bitfield_disconnect_and_floor() {
        let s = state(3, 16);
        let mut empty = Bitfield::with_piece_count(3);
        let mut bf = Bitfield::with_piece_count(3);
        bf.set_piece(0);
        bf.set_piece(2);
        s.apply_peer_bitfield(&empty, &bf);
        assert_eq!(s.availability.count(0), 1);
        assert_eq!(s.availability.count(1), 0);
        assert!(s.apply_peer_have(&mut empty, 1));
        assert_eq!(s.availability.count(1), 1);
        assert!(!s.apply_peer_have(&mut empty, 1));
        assert_eq!(s.availability.count(1), 1);
        let all = Bitfield::filled(3);
        s.apply_peer_bitfield(&bf, &all);
        assert_eq!(s.availability.count(0), 1);
        assert_eq!(s.availability.count(1), 2);
        s.remove_peer_availability(&all);
        assert_eq!(s.availability.count(0), 0);
        s.remove_peer_availability(&all);
        assert_eq!(s.availability.count(0), 0);
    }

    #[test]
    fn snub_release_keeps_chunks() {
        let s = state(1, 16);
        let p1 = peer(6881);
        s.try_reserve_piece(0, p1);
        s.set_chuncks(0, 0, vec![1u8; 8], p1);
        s.snub_release(p1);
        assert!(s.pieces[0].reserved.lock().unwrap().is_none());
        assert_eq!(s.pieces[0].chuncks.lock().unwrap().len(), 1);
    }

    fn sha1(data: &[u8]) -> [u8; 20] {
        let mut hasher = sha1_smol::Sha1::new();
        hasher.update(data);
        hasher.digest().bytes()
    }

    fn tiny_meta() -> crate::file::TorrentMeta {
        let data = [0u8; 16];
        crate::file::TorrentMeta::new(crate::file::TorrentFile {
            info: crate::file::Info {
                name: "tiny.bin".into(),
                pieces: serde_bytes::ByteBuf::from(sha1(&data).to_vec()),
                piece_length: 16,
                md5sum: None,
                length: Some(16),
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
        .expect("tiny torrent")
    }

    async fn connection_fixture(
        addr: PeerAddr,
    ) -> (
        PeerConnection,
        Arc<PeerStates>,
        flume::Receiver<WriterRequest>,
        tokio::sync::broadcast::Receiver<u32>,
        tempfile::TempDir,
    ) {
        let meta = tiny_meta();
        let torrent = Arc::new(crate::torrent::Torrent::new(&meta).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.bin");
        std::fs::write(&path, [0u8; 16]).unwrap();
        let storage = Storage::open(&torrent, &path).await.unwrap();
        let peer_states = Arc::new(PeerStates::default());
        let (piece_tx, _piece_rx) = flume::unbounded();
        let (writer_tx, writer_rx) = flume::unbounded();
        assert!(peer_states.insert_live(addr, writer_tx.clone()));
        let downloaded = Arc::new(state(1, 16));
        let handler = Arc::new(PeerHandler::from_config(PeerHandlerConfig {
            peer: addr,
            piece_tx,
            peer_writer_tx: writer_tx,
            peers_state: peer_states.clone(),
            torrent_downloaded_state: downloaded,
            download_state: Arc::new(Mutex::new(DownloadState::Downloading)),
            storage,
            uploaded: Arc::new(AtomicU64::new(0)),
            torrent,
            choke_notify: Arc::new(Notify::new()),
            extensions: ExtensionRegistry::new(),
            listen_port: 0,
            metadata: MetadataStore::new(meta.info_hash),
            info_hash: meta.info_hash,
            advertise_dht: false,
            dht_port: None,
            dht: None,
        }));
        let connector: Arc<dyn Connector> = Arc::new(crate::transport::TcpConnector::new());
        let connection = PeerConnection::new(
            addr,
            meta.info_hash,
            *b"-BR0100-testdriver01",
            handler,
            connector,
        );
        let have_rx = tokio::sync::broadcast::channel(8).0.subscribe();
        (connection, peer_states, writer_rx, have_rx, dir)
    }

    #[tokio::test]
    async fn manage_established_runs_over_duplex_without_a_socket() {
        let addr = peer(51413);
        let (connection, peer_states, writer_rx, have_rx, _dir) = connection_fixture(addr).await;
        let protocol = Arc::new(
            Protocol::connect(addr, connection.info_hash, connection.peer_id)
                .await
                .unwrap()
                .with_piece_count(1),
        );

        let (driver, mut remote) = tokio::io::duplex(256);
        let mut bitfield = Bitfield::with_piece_count(1);
        bitfield.set_piece(0);
        let payload = message::serialize(Some(Message::Bitfield(bitfield.as_bytes().to_vec())));

        let drive = tokio::spawn(async move {
            connection
                .manage_established(driver, protocol, writer_rx, have_rx)
                .await
        });

        remote.write_all(&payload).await.unwrap();
        drop(remote);

        drive.await.unwrap().unwrap();
        let state = peer_states.states.get(&addr).expect("peer still tracked");
        assert!(state.bitfield.has_piece(0));
    }

    #[tokio::test(start_paused = true)]
    async fn outstanding_request_without_block_snubs_after_timeout() {
        let addr = peer(51414);
        let (connection, peer_states, _writer_rx, _have_rx, _dir) = connection_fixture(addr).await;
        let handler = connection.handler;
        handler.torrent_downloaded_state.try_reserve_piece(0, addr);
        let req = BlockRequest {
            index: 0,
            begin: 0,
            length: 16,
        };
        let now = tokio::time::Instant::now();
        handler
            .outstanding_requests
            .lock()
            .unwrap()
            .insert(req, now);
        *handler.first_request_at.lock().unwrap() = Some(now);
        handler.maybe_snub();
        assert!(!handler.is_snubbed());

        tokio::time::advance(SNUB_TIMEOUT + Duration::from_millis(1)).await;
        handler.maybe_snub();
        assert!(handler.is_snubbed());
        assert!(handler.torrent_downloaded_state.pieces[0]
            .reserved
            .lock()
            .unwrap()
            .is_none());
        assert!(peer_states.states.get(&addr).unwrap().snubbed);

        *handler.last_block_at.lock().unwrap() = Some(tokio::time::Instant::now());
        handler.on_delivery_progress(16);
        assert!(!handler.is_snubbed());
        assert!(!peer_states.states.get(&addr).unwrap().snubbed);
    }
}
