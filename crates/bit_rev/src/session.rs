use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::file::{self, TorrentMeta};
use crate::handshake::Handshake;
use crate::peer_connection::{
    try_spawn_peer, PieceWorkState, SpawnPeerParams, TorrentDownloadedState,
};
use crate::peer_state::PeerStates;
use crate::protocol::Protocol;
use crate::storage::Storage;
use crate::torrent::Torrent;
use crate::tracker_peers::TrackerPeers;
use crate::utils;
use dashmap::DashMap;
use flume::Receiver;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadState {
    Init,
    Downloading,
    Paused,
}

#[derive(Debug, Clone, Copy)]
pub struct PieceWork {
    pub index: u32,
    pub length: u32,
    pub hash: [u8; 20],
}

#[derive(Debug, Clone)]
pub struct PieceResult {
    pub index: u32,
    pub length: u32,
}

pub const DEFAULT_LISTEN_PORT: u16 = 6881;
pub const DEFAULT_MAX_PEERS_PER_TORRENT: usize = 55;
pub const DEFAULT_MAX_PEERS_GLOBAL: usize = 200;

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub listen_port: u16,
    pub max_peers_per_torrent: usize,
    pub max_peers_global: usize,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            listen_port: DEFAULT_LISTEN_PORT,
            max_peers_per_torrent: DEFAULT_MAX_PEERS_PER_TORRENT,
            max_peers_global: DEFAULT_MAX_PEERS_GLOBAL,
        }
    }
}

pub struct TorrentSession {
    pub tracker: TrackerPeers,
    pub storage: Arc<Storage>,
    pub downloaded_state: Arc<TorrentDownloadedState>,
    pub peer_states: Arc<PeerStates>,
    pub piece_tx: flume::Sender<crate::peer_connection::FullPiece>,
    pub have_broadcast: Arc<tokio::sync::broadcast::Sender<u32>>,
    pub download_state: Arc<Mutex<DownloadState>>,
    pub uploaded: Arc<AtomicU64>,
    pub torrent: Arc<Torrent>,
    pub torrent_meta: TorrentMeta,
    pub choke_notify: Arc<Notify>,
}

#[derive(Debug, Clone)]
pub struct State {
    pub requested: u32,
    pub downloaded: u32,
    pub buf: Vec<u8>,
}

pub struct Session {
    pub torrents: Arc<DashMap<[u8; 20], Arc<TorrentSession>>>,
    pub download_state: Arc<Mutex<DownloadState>>,
    peer_id: [u8; 20],
    options: SessionOptions,
    listen_addr: Arc<Mutex<Option<SocketAddr>>>,
    global_peers: Arc<AtomicUsize>,
    cancel: CancellationToken,
}

pub struct AddTorrentOptions {
    torrent_meta: TorrentMeta,
    output_dir: Option<PathBuf>,
    seed: bool,
}

impl AddTorrentOptions {
    fn from_meta(torrent_meta: TorrentMeta) -> Self {
        Self {
            torrent_meta,
            output_dir: None,
            seed: false,
        }
    }

    fn from_path(path: &str) -> Self {
        let torrent_meta = file::from_filename(path).unwrap();
        Self::from_meta(torrent_meta)
    }

    pub fn output_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.output_dir = Some(dir.into());
        self
    }

    pub fn seed(mut self, seed: bool) -> Self {
        self.seed = seed;
        self
    }
}

impl From<TorrentMeta> for AddTorrentOptions {
    fn from(torrent_meta: TorrentMeta) -> Self {
        Self::from_meta(torrent_meta)
    }
}

impl From<&str> for AddTorrentOptions {
    fn from(path: &str) -> Self {
        Self::from_path(path)
    }
}

pub struct AddTorrentResult {
    pub torrent: Torrent,
    pub torrent_meta: TorrentMeta,
    pub pr_rx: Receiver<PieceResult>,
}

impl Session {
    pub fn new() -> Self {
        Self::with_options(SessionOptions::default())
    }

    pub fn with_options(options: SessionOptions) -> Self {
        let session = Self {
            torrents: Arc::new(DashMap::new()),
            download_state: Arc::new(Mutex::new(DownloadState::Init)),
            peer_id: utils::generate_peer_id(),
            options,
            listen_addr: Arc::new(Mutex::new(None)),
            global_peers: Arc::new(AtomicUsize::new(0)),
            cancel: CancellationToken::new(),
        };
        session.spawn_listener();
        session
    }

    pub fn listen_port(&self) -> u16 {
        self.listen_addr
            .lock()
            .unwrap()
            .map(|addr| addr.port())
            .unwrap_or(self.options.listen_port)
    }

    pub async fn wait_listening(&self) -> SocketAddr {
        loop {
            if let Some(addr) = *self.listen_addr.lock().unwrap() {
                return addr;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    pub fn uploaded(&self) -> u64 {
        self.torrents
            .iter()
            .map(|entry| entry.value().uploaded.load(Ordering::Relaxed))
            .sum()
    }

    pub fn torrent_uploaded(&self, info_hash: &[u8; 20]) -> Option<u64> {
        self.torrents
            .get(info_hash)
            .map(|entry| entry.uploaded.load(Ordering::Relaxed))
    }

    pub fn options(&self) -> &SessionOptions {
        &self.options
    }

    pub fn peer_id(&self) -> [u8; 20] {
        self.peer_id
    }

    fn spawn_listener(&self) {
        let port = self.options.listen_port;
        let torrents = self.torrents.clone();
        let peer_id = self.peer_id;
        let listen_addr = self.listen_addr.clone();
        let global_peers = self.global_peers.clone();
        let cancel = self.cancel.clone();
        let max_peers_per_torrent = self.options.max_peers_per_torrent;
        let max_peers_global = self.options.max_peers_global;

        tokio::spawn(async move {
            let bind_addr = SocketAddr::from(([0, 0, 0, 0], port));
            let listener = match TcpListener::bind(bind_addr).await {
                Ok(listener) => listener,
                Err(e) => {
                    warn!(port, error = %e, "failed to bind listen port");
                    return;
                }
            };
            match listener.local_addr() {
                Ok(addr) => {
                    info!(%addr, "listening for incoming peers");
                    *listen_addr.lock().unwrap() = Some(addr);
                }
                Err(e) => {
                    warn!(error = %e, "failed to read listen address");
                    return;
                }
            }

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    accepted = listener.accept() => {
                        let (stream, addr) = match accepted {
                            Ok(pair) => pair,
                            Err(e) => {
                                debug!(error = %e, "accept failed");
                                continue;
                            }
                        };
                        let torrents = torrents.clone();
                        let global_peers = global_peers.clone();
                        tokio::spawn(async move {
                            handle_incoming(
                                stream,
                                addr,
                                peer_id,
                                torrents,
                                global_peers,
                                max_peers_per_torrent,
                                max_peers_global,
                            )
                            .await;
                        });
                    }
                }
            }
        });
    }

    pub fn start_downloading(&self) {
        {
            let mut state = self.download_state.lock().unwrap();
            *state = DownloadState::Downloading;
        }
        for entry in self.torrents.iter() {
            entry
                .value()
                .tracker
                .set_download_state(DownloadState::Downloading);
        }
    }

    pub fn pause(&self) {
        {
            let mut state = self.download_state.lock().unwrap();
            *state = DownloadState::Paused;
        }
        for entry in self.torrents.iter() {
            entry
                .value()
                .tracker
                .set_download_state(DownloadState::Paused);
        }
    }

    pub fn resume(&self) {
        {
            let mut state = self.download_state.lock().unwrap();
            *state = DownloadState::Downloading;
        }
        for entry in self.torrents.iter() {
            entry
                .value()
                .tracker
                .set_download_state(DownloadState::Downloading);
        }
    }

    pub fn get_download_state(&self) -> DownloadState {
        *self.download_state.lock().unwrap()
    }

    pub fn is_paused(&self) -> bool {
        self.get_download_state() == DownloadState::Paused
    }

    pub fn is_downloading(&self) -> bool {
        self.get_download_state() == DownloadState::Downloading
    }

    pub fn is_init(&self) -> bool {
        self.get_download_state() == DownloadState::Init
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
        for entry in self.torrents.iter() {
            entry.value().tracker.shutdown();
        }
    }

    pub fn remove_torrent(&self, info_hash: &[u8; 20]) {
        if let Some((_, torrent)) = self.torrents.remove(info_hash) {
            torrent.tracker.shutdown();
        }
    }

    pub async fn add_torrent(
        &self,
        add_torrent: AddTorrentOptions,
    ) -> anyhow::Result<AddTorrentResult> {
        let torrent = Torrent::new(&add_torrent.torrent_meta.clone())?;
        if torrent.is_private() {
            let disabled: Vec<&str> = torrent
                .disabled_discovery_sources()
                .iter()
                .map(|source| source.as_str())
                .collect();
            info!(
                name = %torrent.name,
                disabled = %disabled.join(", "),
                "private torrent: non-tracker peer sources are disabled"
            );
        }
        let torrent = Arc::new(torrent);
        let torrent_meta = add_torrent.torrent_meta.clone();
        let output_dir = add_torrent
            .output_dir
            .unwrap_or_else(|| PathBuf::from(&torrent.name));
        let storage = Storage::open(&torrent, &output_dir).await?;

        let (pr_tx, pr_rx) = flume::bounded::<PieceResult>(torrent.piece_hashes.len().max(1));
        let have_broadcast = Arc::new(tokio::sync::broadcast::channel(128).0);
        let peer_states = Arc::new(PeerStates::default());
        let uploaded = Arc::new(AtomicU64::new(0));
        let choke_notify = Arc::new(Notify::new());

        let pieces_of_work = (0..torrent.piece_hashes.len())
            .map(|index| {
                let length = utils::calculate_piece_size(&torrent, index);
                PieceWork {
                    index: index as u32,
                    length: length as u32,
                    hash: torrent.piece_hashes[index],
                }
            })
            .collect::<Vec<PieceWork>>();

        let downloaded_state = Arc::new(TorrentDownloadedState {
            semaphore: Semaphore::new(1),
            pieces: pieces_of_work
                .into_iter()
                .map(|pw| PieceWorkState {
                    piece_work: pw,
                    chuncks: Mutex::new(vec![]),
                    downloaded: std::sync::atomic::AtomicBool::new(false),
                    reserved: Mutex::new(None),
                })
                .collect(),
        });
        if add_torrent.seed {
            downloaded_state.mark_all_downloaded();
        }

        let tracker_stream = TrackerPeers::new(
            torrent_meta.clone(),
            15,
            self.peer_id,
            peer_states.clone(),
            have_broadcast.clone(),
            pr_rx.clone(),
            self.download_state.clone(),
        );

        let listen_port =
            tokio::time::timeout(std::time::Duration::from_millis(250), self.wait_listening())
                .await
                .map(|addr| addr.port())
                .unwrap_or_else(|_| self.listen_port());
        tracker_stream
            .connect(crate::tracker_peers::PeerSpawnRuntime {
                info_hash: torrent.info_hash,
                peer_id: self.peer_id,
                storage: storage.clone(),
                downloaded_state: downloaded_state.clone(),
                uploaded: uploaded.clone(),
                torrent: torrent.clone(),
                choke_notify: choke_notify.clone(),
                global_peers: self.global_peers.clone(),
                max_peers_per_torrent: self.options.max_peers_per_torrent,
                max_peers_global: self.options.max_peers_global,
                listen_port,
            })
            .await;

        spawn_choke_loop(
            peer_states.clone(),
            downloaded_state.clone(),
            choke_notify.clone(),
            tracker_stream.cancel_token(),
        );

        let have_broadcast_writer = have_broadcast.clone();
        let piece_rx = tracker_stream.piece_rx.clone();
        let storage_writer = storage.clone();
        let downloaded_writer = downloaded_state.clone();
        tokio::spawn(async move {
            loop {
                let piece = match piece_rx.recv_async().await {
                    Ok(piece) => piece,
                    Err(_) => break,
                };
                if let Err(e) = storage_writer.write_piece(piece.index, &piece.buf).await {
                    debug!(index = piece.index, error = %e, "failed to write piece");
                    downloaded_writer.remove_downloaded(piece.index);
                    continue;
                }
                let _ = have_broadcast_writer.send(piece.index);
                if pr_tx
                    .send_async(PieceResult {
                        index: piece.index,
                        length: piece.length,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let piece_tx = tracker_stream.piece_tx.clone();
        self.torrents.insert(
            torrent.info_hash,
            Arc::new(TorrentSession {
                tracker: tracker_stream,
                storage,
                downloaded_state,
                peer_states,
                piece_tx,
                have_broadcast,
                download_state: self.download_state.clone(),
                uploaded,
                torrent: torrent.clone(),
                torrent_meta: torrent_meta.clone(),
                choke_notify,
            }),
        );

        self.start_downloading();

        Ok(AddTorrentResult {
            torrent: (*torrent).clone(),
            torrent_meta,
            pr_rx,
        })
    }
}

async fn handle_incoming(
    mut stream: tokio::net::TcpStream,
    addr: SocketAddr,
    peer_id: [u8; 20],
    torrents: Arc<DashMap<[u8; 20], Arc<TorrentSession>>>,
    global_peers: Arc<AtomicUsize>,
    max_peers_per_torrent: usize,
    max_peers_global: usize,
) {
    let handshake = match Protocol::read_handshake(&mut stream).await {
        Ok(handshake) => handshake,
        Err(e) => {
            debug!(%addr, error = %e, "incoming handshake failed");
            return;
        }
    };
    let Some(torrent) = torrents
        .get(&handshake.info_hash)
        .map(|entry| entry.clone())
    else {
        debug!(%addr, "incoming peer for unknown info hash");
        return;
    };
    let reply = Handshake::new(handshake.info_hash, peer_id);
    if let Err(e) = Protocol::write_handshake(&mut stream, &reply).await {
        debug!(%addr, error = %e, "failed to write handshake reply");
        return;
    }

    try_spawn_peer(SpawnPeerParams {
        peer: addr,
        info_hash: handshake.info_hash,
        peer_id,
        piece_tx: torrent.piece_tx.clone(),
        have_broadcast: torrent.have_broadcast.clone(),
        torrent_downloaded_state: torrent.downloaded_state.clone(),
        peer_states: torrent.peer_states.clone(),
        download_state: torrent.download_state.clone(),
        storage: torrent.storage.clone(),
        uploaded: torrent.uploaded.clone(),
        torrent: torrent.torrent.clone(),
        choke_notify: torrent.choke_notify.clone(),
        incoming: Some(stream),
        global_peers,
        max_peers_per_torrent,
        max_peers_global,
    });
}

fn spawn_choke_loop(
    peer_states: Arc<PeerStates>,
    downloaded_state: Arc<TorrentDownloadedState>,
    choke_notify: Arc<Notify>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(crate::choke::CHOKE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_optimistic = std::time::Instant::now() - crate::choke::OPTIMISTIC_INTERVAL;
        loop {
            let reset_rates = tokio::select! {
                _ = cancel.cancelled() => break,
                _ = choke_notify.notified() => false,
                _ = interval.tick() => true,
            };
            apply_choke(
                &peer_states,
                downloaded_state.is_complete(),
                &mut last_optimistic,
                reset_rates,
            );
        }
    });
}

fn apply_choke(
    peer_states: &PeerStates,
    seeding: bool,
    last_optimistic: &mut std::time::Instant,
    reset_rates: bool,
) {
    use crate::choke::{
        choke_transitions, select_unchoked, ChokeAction, ChokePeer, OPTIMISTIC_INTERVAL,
    };
    use crate::message::{Message, WriterRequest};

    let now = std::time::Instant::now();
    let pick_optimistic = now.duration_since(*last_optimistic) >= OPTIMISTIC_INTERVAL;
    let snapshots: Vec<ChokePeer> = peer_states
        .states
        .iter()
        .map(|entry| {
            let state = entry.value();
            ChokePeer {
                addr: *entry.key(),
                peer_interested: state.stats.peer_interested.load(Ordering::Relaxed),
                currently_unchoked: !state.stats.am_choking.load(Ordering::Relaxed),
                download_bytes: if reset_rates {
                    state.stats.bytes_downloaded.swap(0, Ordering::Relaxed)
                } else {
                    state.stats.bytes_downloaded.load(Ordering::Relaxed)
                },
                last_unchoked: state.last_unchoked,
                connected_at: state.connected_at,
                is_optimistic: state.is_optimistic,
            }
        })
        .collect();

    let previous: std::collections::HashSet<_> = snapshots
        .iter()
        .filter(|peer| peer.currently_unchoked)
        .map(|peer| peer.addr)
        .collect();
    let next = select_unchoked(
        &snapshots,
        seeding,
        pick_optimistic,
        now,
        &mut rand::thread_rng(),
    );
    if pick_optimistic {
        *last_optimistic = now;
    }

    let regular = select_unchoked(&snapshots, seeding, false, now, &mut rand::thread_rng());

    for (addr, action) in choke_transitions(&previous, &next) {
        let Some(mut state) = peer_states.states.get_mut(&addr) else {
            continue;
        };
        match action {
            ChokeAction::Choke => {
                state.set_am_choking(true);
                state.is_optimistic = false;
                if let Some(tx) = &state.writer_tx {
                    let _ = tx.send(WriterRequest::Message(Message::Choke));
                }
            }
            ChokeAction::Unchoke => {
                state.set_am_choking(false);
                state.last_unchoked = Some(now);
                state.is_optimistic = !regular.contains(&addr);
                if let Some(tx) = &state.writer_tx {
                    let _ = tx.send(WriterRequest::Message(Message::Unchoke));
                }
                state.stats.upload_notify.notify_waiters();
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
