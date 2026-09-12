use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::dht::{DhtHandle, PeerSink};
use crate::discovery::DiscoverySource;
use crate::extension::{Extension, ExtensionContext, ExtensionRegistry, MetadataStore, UtMetadata};
use crate::file::{self, TorrentMeta};
use crate::handshake::Handshake;
use crate::message::{Message, WriterRequest};
use crate::peer::PeerAddr;
use crate::peer_connection::{
    try_spawn_peer, PieceWorkState, SpawnPeerParams, TorrentDownloadedState,
};
use crate::peer_state::PeerStates;
use crate::protocol::Protocol;
use crate::resume::{self, ResumeSnapshot};

pub use crate::dht::{DhtOptions, DhtStats};
pub use crate::resume::ResumeStatus;
use crate::storage::Storage;
use crate::torrent::Torrent;
use crate::tracker_peers::TrackerPeers;
use crate::transport::{
    boxed_stream, BoxedPeerStream, Connector, IncomingKind, IncomingStream, TcpConnector,
};
use crate::utils;
use dashmap::DashMap;
use flume::Receiver;
use tokio::net::TcpListener;
use tokio::sync::Notify;
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
    pub dht: DhtOptions,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            listen_port: DEFAULT_LISTEN_PORT,
            max_peers_per_torrent: DEFAULT_MAX_PEERS_PER_TORRENT,
            max_peers_global: DEFAULT_MAX_PEERS_GLOBAL,
            state_dir: Some(util::paths::state_dir()),
            dht: DhtOptions::default(),
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
    pub metadata: Arc<MetadataStore>,
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
    extensions: ExtensionRegistry,
    connector: Arc<dyn Connector>,
    pending: Arc<DashMap<[u8; 20], Arc<PendingTorrent>>>,
    dht: Arc<Mutex<Option<DhtHandle>>>,
}

pub(crate) struct PendingTorrent {
    metadata: Arc<MetadataStore>,
    peer_states: Arc<PeerStates>,
    download_state: Arc<Mutex<DownloadState>>,
    uploaded: Arc<AtomicU64>,
    choke_notify: Arc<Notify>,
    have_broadcast: Arc<tokio::sync::broadcast::Sender<u32>>,
    downloaded_state: Arc<TorrentDownloadedState>,
    storage: Arc<Storage>,
    torrent: Arc<Torrent>,
    piece_tx: flume::Sender<crate::peer_connection::FullPiece>,
}

#[derive(Debug, Clone, Default)]
pub struct AddInfoHashOptions {
    output_dir: Option<PathBuf>,
}

impl AddInfoHashOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn output_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.output_dir = Some(dir.into());
        self
    }
}

#[derive(Clone)]
pub struct MetadataHandle {
    pub info_hash: [u8; 20],
    store: Arc<MetadataStore>,
    peer_states: Arc<PeerStates>,
}

impl MetadataHandle {
    pub fn store(&self) -> &Arc<MetadataStore> {
        &self.store
    }

    pub fn peer_states(&self) -> &Arc<PeerStates> {
        &self.peer_states
    }

    pub async fn wait_meta(&self) -> anyhow::Result<TorrentMeta> {
        let bytes = self.store.wait().await;
        TorrentMeta::from_info_bytes(bytes)
    }
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

    pub fn from_path(path: &str) -> anyhow::Result<Self> {
        let torrent_meta = file::from_filename(path)?;
        Ok(Self::from_meta(torrent_meta))
    }

    pub fn name(&self) -> &str {
        &self.torrent_meta.torrent_file.info.name
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

impl TryFrom<&str> for AddTorrentOptions {
    type Error = anyhow::Error;

    fn try_from(path: &str) -> Result<Self, Self::Error> {
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
        Self::with_connector(options, Arc::new(TcpConnector::new()))
    }

    pub fn with_connector(options: SessionOptions, connector: Arc<dyn Connector>) -> Self {
        let session = Self {
            torrents: Arc::new(DashMap::new()),
            download_state: Arc::new(Mutex::new(DownloadState::Init)),
            peer_id: utils::generate_peer_id(),
            options,
            listen_addr: Arc::new(Mutex::new(None)),
            global_peers: Arc::new(AtomicUsize::new(0)),
            cancel: CancellationToken::new(),
            extensions: ExtensionRegistry::new(),
            connector,
            pending: Arc::new(DashMap::new()),
            dht: Arc::new(Mutex::new(None)),
        };
        session
            .extensions
            .register(|ctx| Box::new(UtMetadata::new(ctx.clone())));
        session.spawn_listener();
        session.start_dht();
        session
    }

    fn start_dht(&self) {
        if !self.options.dht.enabled {
            return;
        }
        let inlet = self.peer_inlet();
        let sink: PeerSink = Arc::new(move |info_hash, addrs| {
            inlet.add_peers(&info_hash, DiscoverySource::Dht, addrs);
        });
        match DhtHandle::spawn(
            self.options.dht.clone(),
            self.options.state_dir.clone(),
            sink,
            self.cancel.clone(),
        ) {
            Ok(handle) => {
                *self.dht.lock().unwrap() = Some(handle);
            }
            Err(e) => {
                warn!(error = %e, "failed to start DHT");
            }
        }
    }

    fn peer_inlet(&self) -> PeerInlet {
        PeerInlet {
            torrents: self.torrents.clone(),
            pending: self.pending.clone(),
            peer_id: self.peer_id,
            extensions: self.extensions.clone(),
            listen_addr: self.listen_addr.clone(),
            listen_port: self.options.listen_port,
            global_peers: self.global_peers.clone(),
            max_peers_per_torrent: self.options.max_peers_per_torrent,
            max_peers_global: self.options.max_peers_global,
            connector: self.connector.clone(),
            dht: self.dht.clone(),
            download_state: self.download_state.clone(),
        }
    }

    pub fn dht(&self) -> Option<DhtHandle> {
        self.dht.lock().unwrap().clone()
    }

    pub fn dht_stats(&self) -> Option<DhtStats> {
        self.dht().map(|d| d.stats())
    }

    pub fn add_peers(
        &self,
        info_hash: &[u8; 20],
        source: DiscoverySource,
        addrs: Vec<PeerAddr>,
    ) -> usize {
        self.peer_inlet().add_peers(info_hash, source, addrs)
    }

    pub fn connector(&self) -> Arc<dyn Connector> {
        self.connector.clone()
    }

    pub fn global_peer_count(&self) -> usize {
        self.global_peers.load(Ordering::Relaxed)
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

    pub fn extensions(&self) -> &ExtensionRegistry {
        &self.extensions
    }

    pub fn register_extension<F>(&self, factory: F) -> u8
    where
        F: Fn(&ExtensionContext) -> Box<dyn Extension> + Send + Sync + 'static,
    {
        self.extensions.register(factory)
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
        let extensions = self.extensions.clone();
        let connector = self.connector.clone();
        let pending = self.pending.clone();
        let dht = self.dht.clone();

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
                        let extensions = extensions.clone();
                        let connector = connector.clone();
                        let pending = pending.clone();
                        let dht = dht.lock().unwrap().clone();
                        let listen_port = listen_addr
                            .lock()
                            .unwrap()
                            .map(|bound| bound.port())
                            .unwrap_or(port);
                        tokio::spawn(async move {
                            accept_incoming(
                                boxed_stream(stream),
                                addr,
                                IncomingPeerContext {
                                    peer_id,
                                    torrents,
                                    global_peers,
                                    max_peers_per_torrent,
                                    max_peers_global,
                                    extensions,
                                    listen_port,
                                    connector,
                                    pending,
                                    dht,
                                },
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
        if let Some(dht) = self.dht() {
            dht.set_all_active(true);
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
        if let Some(dht) = self.dht() {
            dht.set_all_active(false);
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
        if let Some(dht) = self.dht() {
            dht.set_all_active(true);
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
        if let Some(dht) = self.dht() {
            dht.shutdown();
        }
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

    fn dht_peer_fields(&self) -> (bool, Option<u16>, Option<DhtHandle>) {
        match self.dht() {
            Some(dht) => (true, Some(dht.udp_port()), Some(dht)),
            None => (false, None, None),
        }
    }

    pub fn connect_peer(&self, info_hash: &[u8; 20], addr: SocketAddr) -> bool {
        if let Some(torrent) = self.torrents.get(info_hash).map(|entry| entry.clone()) {
            return try_spawn_peer(SpawnPeerParams {
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
                incoming_extension_protocol: None,
                incoming_dht: None,
                extensions: self.extensions.clone(),
                listen_port: self.listen_port(),
                metadata: torrent.metadata.clone(),
                advertise_dht: self.dht_peer_fields().0,
                dht_port: self.dht_peer_fields().1,
                dht: self.dht_peer_fields().2,
                global_peers: self.global_peers.clone(),
                max_peers_per_torrent: self.options.max_peers_per_torrent,
                max_peers_global: self.options.max_peers_global,
                connector: self.connector.clone(),
            });
        }
        let Some(pending) = self.pending.get(info_hash).map(|entry| entry.clone()) else {
            return false;
        };
        try_spawn_peer(SpawnPeerParams {
            peer: addr,
            info_hash: *info_hash,
            peer_id: self.peer_id,
            piece_tx: pending.piece_tx.clone(),
            have_broadcast: pending.have_broadcast.clone(),
            torrent_downloaded_state: pending.downloaded_state.clone(),
            peer_states: pending.peer_states.clone(),
            download_state: pending.download_state.clone(),
            storage: pending.storage.clone(),
            uploaded: pending.uploaded.clone(),
            torrent: pending.torrent.clone(),
            choke_notify: pending.choke_notify.clone(),
            incoming: None,
            incoming_fast_extension: None,
            incoming_extension_protocol: None,
            incoming_dht: None,
            extensions: self.extensions.clone(),
            listen_port: self.listen_port(),
            metadata: pending.metadata.clone(),
            advertise_dht: self.dht_peer_fields().0,
            dht_port: self.dht_peer_fields().1,
            dht: self.dht_peer_fields().2,
            global_peers: self.global_peers.clone(),
            max_peers_per_torrent: self.options.max_peers_per_torrent,
            max_peers_global: self.options.max_peers_global,
            connector: self.connector.clone(),
        })
    }

    pub fn torrent_session(&self, info_hash: &[u8; 20]) -> Option<Arc<TorrentSession>> {
        self.torrents.get(info_hash).map(|entry| entry.clone())
    }

    pub fn remove_torrent(&self, info_hash: &[u8; 20]) {
        if let Some((_, torrent)) = self.torrents.remove(info_hash) {
            torrent.tracker.shutdown();
        }
        self.pending.remove(info_hash);
        if let Some(dht) = self.dht() {
            dht.remove_torrent(*info_hash);
        }
    }

    pub async fn add_torrent_by_info_hash(
        &self,
        info_hash: [u8; 20],
        opts: AddInfoHashOptions,
    ) -> anyhow::Result<MetadataHandle> {
        if let Some(torrent) = self.torrents.get(&info_hash) {
            return Ok(MetadataHandle {
                info_hash,
                store: torrent.metadata.clone(),
                peer_states: torrent.peer_states.clone(),
            });
        }
        if let Some(pending) = self.pending.get(&info_hash) {
            return Ok(MetadataHandle {
                info_hash,
                store: pending.metadata.clone(),
                peer_states: pending.peer_states.clone(),
            });
        }

        let metadata = MetadataStore::new(info_hash);
        let torrent = Arc::new(Torrent {
            info_hash,
            piece_hashes: Vec::new(),
            piece_length: 16 * 1024,
            length: 0,
            files: Vec::new(),
            name: resume::info_hash_hex(&info_hash),
            private: false,
        });
        let output_dir = opts
            .output_dir
            .unwrap_or_else(|| std::env::temp_dir().join("bitrev-metadata"));
        let storage = Storage::open(&torrent, &output_dir).await?;
        let (piece_tx, _piece_rx) = flume::unbounded();
        let pending = Arc::new(PendingTorrent {
            metadata: metadata.clone(),
            peer_states: Arc::new(PeerStates::default()),
            download_state: self.download_state.clone(),
            uploaded: Arc::new(AtomicU64::new(0)),
            choke_notify: Arc::new(Notify::new()),
            have_broadcast: Arc::new(tokio::sync::broadcast::channel(8).0),
            downloaded_state: Arc::new(TorrentDownloadedState::new(Vec::new())),
            storage,
            torrent,
            piece_tx,
        });
        self.pending.insert(info_hash, pending.clone());
        Ok(MetadataHandle {
            info_hash,
            store: metadata,
            peer_states: pending.peer_states.clone(),
        })
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
            resume::cache_torrent_file(
                state_dir,
                &torrent.info_hash,
                &torrent_meta.torrent_file,
                &torrent_meta.info_bytes,
            )
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

        let (pr_tx, pr_rx) = flume::bounded::<PieceResult>(torrent.piece_hashes.len().max(1) * 2);
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

        let downloaded_state = Arc::new(TorrentDownloadedState::new(
            pieces_of_work
                .into_iter()
                .map(PieceWorkState::new)
                .collect(),
        ));

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

        let metadata =
            MetadataStore::with_bytes(torrent.info_hash, torrent_meta.info_bytes.clone());

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
                extensions: self.extensions.clone(),
                connector: self.connector.clone(),
                metadata: metadata.clone(),
                advertise_dht: self.dht_peer_fields().0,
                dht_port: self.dht_peer_fields().1,
                dht: self.dht_peer_fields().2,
            })
            .await;

        if let Some(dht) = self.dht() {
            match tracker_stream.register_source(DiscoverySource::Dht) {
                Ok(()) => {
                    let active = !start_paused
                        && *self.download_state.lock().unwrap() != DownloadState::Paused;
                    dht.add_torrent(torrent.info_hash, listen_port, active);
                }
                Err(denied) => {
                    debug!(error = %denied, "dht disabled for torrent");
                }
            }
        }

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
            metadata,
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

#[derive(Clone)]
struct PeerInlet {
    torrents: Arc<DashMap<[u8; 20], Arc<TorrentSession>>>,
    pending: Arc<DashMap<[u8; 20], Arc<PendingTorrent>>>,
    peer_id: [u8; 20],
    extensions: ExtensionRegistry,
    listen_addr: Arc<Mutex<Option<SocketAddr>>>,
    listen_port: u16,
    global_peers: Arc<AtomicUsize>,
    max_peers_per_torrent: usize,
    max_peers_global: usize,
    connector: Arc<dyn Connector>,
    dht: Arc<Mutex<Option<DhtHandle>>>,
    download_state: Arc<Mutex<DownloadState>>,
}

impl PeerInlet {
    fn listen_port(&self) -> u16 {
        self.listen_addr
            .lock()
            .unwrap()
            .map(|addr| addr.port())
            .unwrap_or(self.listen_port)
    }

    fn dht_fields(&self) -> (bool, Option<u16>, Option<DhtHandle>) {
        match self.dht.lock().unwrap().clone() {
            Some(dht) => (true, Some(dht.udp_port()), Some(dht)),
            None => (false, None, None),
        }
    }

    fn add_peers(
        &self,
        info_hash: &[u8; 20],
        source: DiscoverySource,
        addrs: Vec<PeerAddr>,
    ) -> usize {
        let allowed = if let Some(torrent) = self.torrents.get(info_hash) {
            torrent.tracker.allows_source(source)
        } else if let Some(pending) = self.pending.get(info_hash) {
            pending.torrent.allows_source(source)
        } else {
            return 0;
        };
        if !allowed {
            return 0;
        }
        if *self.download_state.lock().unwrap() != DownloadState::Downloading {
            return 0;
        }

        let (peer_states, max_per) = if let Some(torrent) = self.torrents.get(info_hash) {
            (torrent.peer_states.clone(), self.max_peers_per_torrent)
        } else if let Some(pending) = self.pending.get(info_hash) {
            (pending.peer_states.clone(), self.max_peers_per_torrent)
        } else {
            return 0;
        };

        let mut added = 0;
        for addr in addrs {
            if self.global_peers.load(Ordering::Relaxed) >= self.max_peers_global {
                break;
            }
            if peer_states.len() >= max_per {
                break;
            }
            if !peer_states.add_if_not_seen(addr) {
                continue;
            }
            if self.connect_peer(info_hash, addr) {
                added += 1;
            }
        }
        added
    }

    fn connect_peer(&self, info_hash: &[u8; 20], addr: SocketAddr) -> bool {
        let (advertise_dht, dht_port, dht) = self.dht_fields();
        if let Some(torrent) = self.torrents.get(info_hash).map(|entry| entry.clone()) {
            return try_spawn_peer(SpawnPeerParams {
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
                incoming_extension_protocol: None,
                incoming_dht: None,
                extensions: self.extensions.clone(),
                listen_port: self.listen_port(),
                metadata: torrent.metadata.clone(),
                advertise_dht,
                dht_port,
                dht,
                global_peers: self.global_peers.clone(),
                max_peers_per_torrent: self.max_peers_per_torrent,
                max_peers_global: self.max_peers_global,
                connector: self.connector.clone(),
            });
        }
        let Some(pending) = self.pending.get(info_hash).map(|entry| entry.clone()) else {
            return false;
        };
        try_spawn_peer(SpawnPeerParams {
            peer: addr,
            info_hash: *info_hash,
            peer_id: self.peer_id,
            piece_tx: pending.piece_tx.clone(),
            have_broadcast: pending.have_broadcast.clone(),
            torrent_downloaded_state: pending.downloaded_state.clone(),
            peer_states: pending.peer_states.clone(),
            download_state: pending.download_state.clone(),
            storage: pending.storage.clone(),
            uploaded: pending.uploaded.clone(),
            torrent: pending.torrent.clone(),
            choke_notify: pending.choke_notify.clone(),
            incoming: None,
            incoming_fast_extension: None,
            incoming_extension_protocol: None,
            incoming_dht: None,
            extensions: self.extensions.clone(),
            listen_port: self.listen_port(),
            metadata: pending.metadata.clone(),
            advertise_dht,
            dht_port,
            dht,
            global_peers: self.global_peers.clone(),
            max_peers_per_torrent: self.max_peers_per_torrent,
            max_peers_global: self.max_peers_global,
            connector: self.connector.clone(),
        })
    }
}

pub struct IncomingPeerContext {
    pub peer_id: [u8; 20],
    pub torrents: Arc<DashMap<[u8; 20], Arc<TorrentSession>>>,
    pub global_peers: Arc<AtomicUsize>,
    pub max_peers_per_torrent: usize,
    pub max_peers_global: usize,
    pub extensions: ExtensionRegistry,
    pub listen_port: u16,
    pub connector: Arc<dyn Connector>,
    pub(crate) pending: Arc<DashMap<[u8; 20], Arc<PendingTorrent>>>,
    pub dht: Option<DhtHandle>,
}

impl Session {
    pub fn incoming_context(&self) -> IncomingPeerContext {
        IncomingPeerContext {
            peer_id: self.peer_id,
            torrents: self.torrents.clone(),
            global_peers: self.global_peers.clone(),
            max_peers_per_torrent: self.options.max_peers_per_torrent,
            max_peers_global: self.options.max_peers_global,
            extensions: self.extensions.clone(),
            listen_port: self.listen_port(),
            connector: self.connector.clone(),
            pending: self.pending.clone(),
            dht: self.dht(),
        }
    }

    /// Accept an inbound peer stream (TCP listener, later uTP). Handshake
    /// lookup is transport-agnostic.
    pub async fn accept_incoming(&self, stream: BoxedPeerStream, addr: SocketAddr) {
        accept_incoming(stream, addr, self.incoming_context()).await;
    }

    pub async fn accept_incoming_stream(&self, incoming: IncomingStream) {
        accept_incoming_stream(incoming, self.incoming_context()).await;
    }
}

/// Handshake lookup shared by the TCP listener and later uTP accepts.
pub async fn accept_incoming(stream: BoxedPeerStream, addr: SocketAddr, ctx: IncomingPeerContext) {
    accept_incoming_stream(IncomingStream::new(stream, addr), ctx).await;
}

pub async fn accept_incoming_stream(incoming: IncomingStream, ctx: IncomingPeerContext) {
    let IncomingStream {
        mut stream,
        addr,
        kind,
    } = incoming;
    debug!(%addr, ?kind, "accepting incoming peer");
    if kind == IncomingKind::MaybeEncrypted {
        // MSE (#7) will take this branch after peeking the first bytes.
        debug!(%addr, "encrypted inbound not implemented, trying plaintext handshake");
    }
    let handshake = match Protocol::read_handshake(&mut stream).await {
        Ok(handshake) => handshake,
        Err(e) => {
            debug!(%addr, error = %e, "incoming handshake failed");
            return;
        }
    };
    let known = ctx
        .torrents
        .get(&handshake.info_hash)
        .map(|entry| IncomingTarget {
            piece_tx: entry.piece_tx.clone(),
            have_broadcast: entry.have_broadcast.clone(),
            downloaded_state: entry.downloaded_state.clone(),
            peer_states: entry.peer_states.clone(),
            download_state: entry.download_state.clone(),
            storage: entry.storage.clone(),
            uploaded: entry.uploaded.clone(),
            torrent: entry.torrent.clone(),
            choke_notify: entry.choke_notify.clone(),
            metadata: entry.metadata.clone(),
        })
        .or_else(|| {
            ctx.pending
                .get(&handshake.info_hash)
                .map(|entry| IncomingTarget {
                    piece_tx: entry.piece_tx.clone(),
                    have_broadcast: entry.have_broadcast.clone(),
                    downloaded_state: entry.downloaded_state.clone(),
                    peer_states: entry.peer_states.clone(),
                    download_state: entry.download_state.clone(),
                    storage: entry.storage.clone(),
                    uploaded: entry.uploaded.clone(),
                    torrent: entry.torrent.clone(),
                    choke_notify: entry.choke_notify.clone(),
                    metadata: entry.metadata.clone(),
                })
        });
    let Some(target) = known else {
        debug!(%addr, "incoming peer for unknown info hash");
        return;
    };
    if target.peer_states.is_banned(addr) {
        debug!(%addr, "refusing banned incoming peer");
        return;
    }
    let mut reply = Handshake::outgoing(handshake.info_hash, ctx.peer_id);
    if ctx.dht.is_some() {
        reply.enable_dht();
    }
    if let Err(e) = Protocol::write_handshake(&mut stream, &reply).await {
        debug!(%addr, error = %e, "failed to write handshake reply");
        return;
    }

    try_spawn_peer(SpawnPeerParams {
        peer: addr,
        info_hash: handshake.info_hash,
        peer_id: ctx.peer_id,
        incoming_fast_extension: Some(handshake.supports_fast_extension()),
        incoming_extension_protocol: Some(handshake.supports_extension_protocol()),
        incoming_dht: Some(handshake.supports_dht()),
        extensions: ctx.extensions,
        listen_port: ctx.listen_port,
        metadata: target.metadata,
        advertise_dht: ctx.dht.is_some(),
        dht_port: ctx.dht.as_ref().map(|d| d.udp_port()),
        dht: ctx.dht,
        piece_tx: target.piece_tx,
        have_broadcast: target.have_broadcast,
        torrent_downloaded_state: target.downloaded_state,
        peer_states: target.peer_states,
        download_state: target.download_state,
        storage: target.storage,
        uploaded: target.uploaded,
        torrent: target.torrent,
        choke_notify: target.choke_notify,
        incoming: Some(stream),
        global_peers: ctx.global_peers,
        max_peers_per_torrent: ctx.max_peers_per_torrent,
        max_peers_global: ctx.max_peers_global,
        connector: ctx.connector,
    });
}

struct IncomingTarget {
    piece_tx: flume::Sender<crate::peer_connection::FullPiece>,
    have_broadcast: Arc<tokio::sync::broadcast::Sender<u32>>,
    downloaded_state: Arc<TorrentDownloadedState>,
    peer_states: Arc<PeerStates>,
    download_state: Arc<Mutex<DownloadState>>,
    storage: Arc<Storage>,
    uploaded: Arc<AtomicU64>,
    torrent: Arc<Torrent>,
    choke_notify: Arc<Notify>,
    metadata: Arc<MetadataStore>,
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

#[cfg(test)]
mod add_options_tests {
    use super::AddTorrentOptions;

    #[test]
    fn from_path_is_fallible() {
        let err = match AddTorrentOptions::from_path("/does/not/exist.torrent") {
            Ok(_) => panic!("expected a parse error"),
            Err(err) => err,
        };
        assert!(!err.to_string().is_empty());
    }
}

#[cfg(test)]
mod incoming_tests {
    use super::*;
    use crate::file::{Info, TorrentFile};
    use crate::handshake::Handshake;
    use crate::transport::boxed_stream;
    use serde_bytes::ByteBuf;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn sha1(data: &[u8]) -> [u8; 20] {
        let mut hasher = sha1_smol::Sha1::new();
        hasher.update(data);
        hasher.digest().bytes()
    }

    fn tiny_meta() -> TorrentMeta {
        let data = [0u8; 16];
        TorrentMeta::new(TorrentFile {
            info: Info {
                name: "tiny.bin".into(),
                pieces: ByteBuf::from(sha1(&data).to_vec()),
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

    #[tokio::test]
    async fn accept_incoming_joins_torrent_by_info_hash() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::with_options(SessionOptions {
            listen_port: 0,
            state_dir: None,
            ..SessionOptions::default()
        });
        let _ = tokio::time::timeout(Duration::from_secs(2), session.wait_listening()).await;

        let meta = tiny_meta();
        let path = dir.path().join("tiny.bin");
        session
            .add_torrent(AddTorrentOptions::from(meta.clone()).output_dir(path))
            .await
            .expect("add torrent");

        let addr: SocketAddr = "127.0.0.1:51413".parse().unwrap();
        let (mut remote, server) = tokio::io::duplex(256);
        let handshake = Handshake::outgoing(meta.info_hash, *b"-LC0001-0123456789ab");

        let remote_task = tokio::spawn(async move {
            remote.write_all(&handshake.serialize()).await.unwrap();
            let mut reply = [0u8; 68];
            remote.read_exact(&mut reply).await.unwrap();
            remote
        });

        session.accept_incoming(boxed_stream(server), addr).await;

        let torrent = session
            .torrent_session(&meta.info_hash)
            .expect("torrent registered");
        assert!(
            torrent.peer_states.states.contains_key(&addr),
            "incoming duplex should join by info hash"
        );

        drop(remote_task);
        session.shutdown();
    }
}
