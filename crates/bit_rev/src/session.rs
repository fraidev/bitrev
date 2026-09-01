use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::file::{self, TorrentMeta};
use crate::handshake::Handshake;
use crate::message::{Message, WriterRequest};
use crate::peer_connection::{
    try_spawn_peer, PieceWorkState, SpawnPeerParams, TorrentDownloadedState,
};
use crate::peer_state::PeerStates;
use crate::protocol::Protocol;
use crate::resume::{self, ResumeSnapshot};

pub use crate::resume::ResumeStatus;
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
pub const RESUME_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub listen_port: u16,
    pub max_peers_per_torrent: usize,
    pub max_peers_global: usize,
    /// Directory for resume data and cached torrents. `None` disables persistence.
    pub state_dir: Option<PathBuf>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            listen_port: DEFAULT_LISTEN_PORT,
            max_peers_per_torrent: DEFAULT_MAX_PEERS_PER_TORRENT,
            max_peers_global: DEFAULT_MAX_PEERS_GLOBAL,
            state_dir: Some(util::paths::state_dir()),
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
    pub output_dir: PathBuf,
    pub added_at: i64,
    pub completed_at: Arc<Mutex<Option<i64>>>,
    pub torrent_cache_path: PathBuf,
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
    verify: bool,
}

impl AddTorrentOptions {
    fn from_meta(torrent_meta: TorrentMeta) -> Self {
        Self {
            torrent_meta,
            output_dir: None,
            seed: false,
            verify: false,
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

    pub fn verify(mut self, verify: bool) -> Self {
        self.verify = verify;
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
    pub resume_status: ResumeStatus,
    pub already_have: Vec<PieceResult>,
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
            let torrent = entry.value();
            torrent.tracker.set_download_state(DownloadState::Paused);
            choke_all_peers(&torrent.peer_states);
        }
        self.spawn_flush_resume();
    }

    pub fn resume(&self) {
        {
            let mut state = self.download_state.lock().unwrap();
            *state = DownloadState::Downloading;
        }
        for entry in self.torrents.iter() {
            let torrent = entry.value();
            torrent
                .tracker
                .set_download_state(DownloadState::Downloading);
            torrent.choke_notify.notify_waiters();
        }
        self.spawn_flush_resume();
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

    pub async fn flush_resume(&self) {
        let Some(state_dir) = self.options.state_dir.as_ref() else {
            return;
        };
        let torrents: Vec<Arc<TorrentSession>> = self
            .torrents
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        for torrent in torrents {
            if let Err(e) = persist_torrent(state_dir, &torrent) {
                warn!(
                    name = %torrent.torrent.name,
                    error = %e,
                    "failed to flush resume data"
                );
            }
        }
    }

    pub async fn shutdown_graceful(&self) {
        self.flush_resume().await;
        self.shutdown();
    }

    fn spawn_flush_resume(&self) {
        let Some(state_dir) = self.options.state_dir.clone() else {
            return;
        };
        let torrents: Vec<Arc<TorrentSession>> = self
            .torrents
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        tokio::spawn(async move {
            for torrent in torrents {
                if let Err(e) = persist_torrent(&state_dir, &torrent) {
                    warn!(
                        name = %torrent.torrent.name,
                        error = %e,
                        "failed to persist resume data"
                    );
                }
            }
        });
    }

    pub fn connect_peer(&self, info_hash: &[u8; 20], addr: SocketAddr) -> bool {
        let Some(torrent) = self.torrents.get(info_hash).map(|entry| entry.clone()) else {
            return false;
        };
        try_spawn_peer(SpawnPeerParams {
            peer: addr,
            info_hash: *info_hash,
            peer_id: self.peer_id,
            piece_tx: torrent.piece_tx.clone(),
            have_broadcast: torrent.have_broadcast.clone(),
            torrent_downloaded_state: torrent.downloaded_state.clone(),
            peer_states: torrent.peer_states.clone(),
            download_state: torrent.download_state.clone(),
            storage: torrent.storage.clone(),
            uploaded: torrent.uploaded.clone(),
            torrent: torrent.torrent.clone(),
            choke_notify: torrent.choke_notify.clone(),
            incoming: None,
            incoming_fast_extension: None,
            global_peers: self.global_peers.clone(),
            max_peers_per_torrent: self.options.max_peers_per_torrent,
            max_peers_global: self.options.max_peers_global,
        })
    }

    pub fn torrent_session(&self, info_hash: &[u8; 20]) -> Option<Arc<TorrentSession>> {
        self.torrents.get(info_hash).map(|entry| entry.clone())
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

        let torrent_cache_path = if let Some(state_dir) = self.options.state_dir.as_ref() {
            resume::cache_torrent_file(state_dir, &torrent.info_hash, &torrent_meta.torrent_file)
        } else {
            PathBuf::new()
        };

        let resume_file = self
            .options
            .state_dir
            .as_ref()
            .map(|dir| resume::resume_path(dir, &torrent.info_hash));
        let loaded_resume = match resume_file.as_ref() {
            Some(path) => resume::load_optional(path)?,
            None => None,
        };
        let loaded_resume = loaded_resume.filter(|data| match data.info_hash() {
            Some(hash) if hash == torrent.info_hash => true,
            _ => {
                warn!("resume file info hash does not match torrent, starting fresh");
                false
            }
        });
        let resume_existed = resume_file.as_ref().is_some_and(|path| path.exists());
        let resume_unreadable = resume_existed && loaded_resume.is_none();

        let fast_path = match &loaded_resume {
            Some(data) if !add_torrent.verify => {
                resume::files_match(&data.files, &torrent, &output_dir)
            }
            _ => false,
        };

        let storage = Storage::open(&torrent, &output_dir).await?;

        let (pr_tx, pr_rx) = flume::bounded::<PieceResult>(torrent.piece_hashes.len().max(1));
        let have_broadcast = Arc::new(tokio::sync::broadcast::channel(128).0);
        let peer_states = Arc::new(PeerStates::default());
        let uploaded = Arc::new(AtomicU64::new(
            loaded_resume
                .as_ref()
                .map(|data| data.uploaded.max(0) as u64)
                .unwrap_or(0),
        ));
        let choke_notify = Arc::new(Notify::new());
        let added_at = loaded_resume
            .as_ref()
            .map(|data| data.added_at)
            .unwrap_or_else(resume::now_unix);
        let completed_at = Arc::new(Mutex::new(
            loaded_resume.as_ref().and_then(|data| data.completed_at()),
        ));

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

        let resume_status = if add_torrent.seed {
            downloaded_state.mark_all_downloaded();
            ResumeStatus::Fresh
        } else if let Some(data) = &loaded_resume {
            if fast_path {
                resume::apply_bitfield(&downloaded_state, &data.bitfield());
                ResumeStatus::FastPath
            } else {
                resume::verify_existing_pieces(&storage, &downloaded_state).await;
                ResumeStatus::SlowPath
            }
        } else if resume_unreadable {
            ResumeStatus::Corrupt
        } else {
            ResumeStatus::Fresh
        };

        let already_have: Vec<PieceResult> = downloaded_state
            .pieces
            .iter()
            .filter(|pw| pw.downloaded.load(Ordering::Relaxed))
            .map(|pw| PieceResult {
                index: pw.piece_work.index,
                length: pw.piece_work.length,
            })
            .collect();

        if downloaded_state.is_complete() {
            let mut done = completed_at.lock().unwrap();
            if done.is_none() {
                *done = Some(resume::now_unix());
            }
        }

        let start_paused = loaded_resume.as_ref().is_some_and(|data| data.is_paused());

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
        let persist_state_dir = self.options.state_dir.clone();
        let persist_output_dir = output_dir.clone();
        let persist_torrent = torrent.clone();
        let persist_uploaded = uploaded.clone();
        let persist_download_state = self.download_state.clone();
        let persist_added_at = added_at;
        let persist_completed_at = completed_at.clone();
        let persist_cache_path = torrent_cache_path.clone();
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
                if downloaded_writer.is_complete() {
                    let mut done = persist_completed_at.lock().unwrap();
                    if done.is_none() {
                        *done = Some(resume::now_unix());
                    }
                }
                if let Some(state_dir) = persist_state_dir.as_ref() {
                    if let Err(e) = persist_from_parts(
                        state_dir,
                        &persist_torrent.info_hash,
                        &persist_output_dir,
                        &persist_torrent,
                        &downloaded_writer,
                        persist_uploaded.load(Ordering::Relaxed),
                        *persist_download_state.lock().unwrap() == DownloadState::Paused,
                        &persist_cache_path,
                        persist_added_at,
                        *persist_completed_at.lock().unwrap(),
                    ) {
                        debug!(error = %e, "failed to persist resume after piece write");
                    }
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
        let torrent_session = Arc::new(TorrentSession {
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
            output_dir,
            added_at,
            completed_at,
            torrent_cache_path,
        });
        self.torrents
            .insert(torrent.info_hash, torrent_session.clone());

        if let Some(state_dir) = self.options.state_dir.clone() {
            spawn_resume_timer(torrent_session, state_dir, self.cancel.clone());
        }

        if start_paused {
            self.pause();
        } else {
            self.start_downloading();
        }

        Ok(AddTorrentResult {
            torrent: (*torrent).clone(),
            torrent_meta,
            pr_rx,
            resume_status,
            already_have,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn persist_from_parts(
    state_dir: &std::path::Path,
    info_hash: &[u8; 20],
    output_dir: &std::path::Path,
    torrent: &Torrent,
    downloaded_state: &TorrentDownloadedState,
    uploaded: u64,
    paused: bool,
    torrent_path: &std::path::Path,
    added_at: i64,
    completed_at: Option<i64>,
) -> Result<PathBuf, resume::ResumeError> {
    resume::persist(
        state_dir,
        ResumeSnapshot {
            info_hash,
            output_dir,
            torrent,
            downloaded_state,
            uploaded,
            paused,
            torrent_path,
            added_at,
            completed_at,
        },
    )
}

fn persist_torrent(
    state_dir: &std::path::Path,
    torrent: &TorrentSession,
) -> Result<PathBuf, resume::ResumeError> {
    persist_from_parts(
        state_dir,
        &torrent.torrent.info_hash,
        &torrent.output_dir,
        &torrent.torrent,
        &torrent.downloaded_state,
        torrent.uploaded.load(Ordering::Relaxed),
        *torrent.download_state.lock().unwrap() == DownloadState::Paused,
        &torrent.torrent_cache_path,
        torrent.added_at,
        *torrent.completed_at.lock().unwrap(),
    )
}

fn spawn_resume_timer(torrent: Arc<TorrentSession>, state_dir: PathBuf, cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RESUME_FLUSH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        let torrent_cancel = torrent.tracker.cancel_token();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = torrent_cancel.cancelled() => break,
                _ = interval.tick() => {
                    let downloading =
                        *torrent.download_state.lock().unwrap() == DownloadState::Downloading;
                    if downloading {
                        if let Err(e) = persist_torrent(&state_dir, &torrent) {
                            debug!(error = %e, "periodic resume persist failed");
                        }
                    }
                }
            }
        }
    });
}

fn choke_all_peers(peer_states: &PeerStates) {
    for mut state in peer_states.states.iter_mut() {
        if !state.stats.am_choking.load(Ordering::Relaxed) {
            state.set_am_choking(true);
            state.is_optimistic = false;
            if let Some(tx) = &state.writer_tx {
                let _ = tx.send(WriterRequest::Message(Message::Choke));
            }
            state.stats.upload_notify.notify_waiters();
        }
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
    let reply = Handshake::outgoing(handshake.info_hash, peer_id);
    if let Err(e) = Protocol::write_handshake(&mut stream, &reply).await {
        debug!(%addr, error = %e, "failed to write handshake reply");
        return;
    }

    try_spawn_peer(SpawnPeerParams {
        peer: addr,
        info_hash: handshake.info_hash,
        peer_id,
        incoming_fast_extension: Some(handshake.supports_fast_extension()),
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
                state.stats.upload_notify.notify_waiters();
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
