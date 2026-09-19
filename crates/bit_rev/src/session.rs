use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::dht::{DhtHandle, PeerSink};
use crate::discovery::DiscoverySource;
use crate::extension::{Extension, ExtensionContext, ExtensionRegistry, MetadataStore, UtMetadata};
use crate::file::{self, TorrentMeta};
use crate::handshake::Handshake;
use crate::magnet::Magnet;
use crate::message::{Message, WriterRequest};
use crate::peer::PeerAddr;
use crate::peer_connection::{
    try_spawn_peer, PieceWorkState, Slot, SpawnPeerParams, TorrentDownloadedState,
};
use crate::peer_state::PeerStates;
use crate::protocol::Protocol;
use crate::resume::{self, ResumeSnapshot};

pub use crate::dht::{DhtOptions, DhtStats};
use crate::hash::PieceHasher;
use crate::mse::{self, EncryptionPolicy, MseConnector};
pub use crate::resume::ResumeStatus;
pub use crate::storage::Preallocate;
use crate::storage::{Storage, StorageOptions};
use crate::torrent::Torrent;
use crate::tracker_peers::TrackerPeers;
use crate::transport::{
    boxed_stream, BoxedPeerStream, Connector, IncomingKind, IncomingStream, RacingConnector,
    TcpConnector, BT_HANDSHAKE_HEAD,
};
use crate::utils;
pub use crate::utp::UtpOptions;
use crate::utp::{UtpBindState, UtpConnector, UtpSocket};
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

/// Info hash used as a stable torrent handle. Display is 40 lowercase hex chars.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TorrentId(pub [u8; 20]);

impl TorrentId {
    pub fn new(info_hash: [u8; 20]) -> Self {
        Self(info_hash)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_bytes(self) -> [u8; 20] {
        self.0
    }
}

impl From<[u8; 20]> for TorrentId {
    fn from(info_hash: [u8; 20]) -> Self {
        Self(info_hash)
    }
}

impl fmt::Display for TorrentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for TorrentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TorrentId({self})")
    }
}

/// Per-torrent lifecycle. `Queued` and `Moving` exist for later issues and are
/// never entered here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TorrentState {
    Checking,
    Metadata,
    Downloading,
    Seeding,
    Paused,
    Queued,
    Moving,
    Error(String),
}

impl TorrentState {
    fn transfer_enabled(&self) -> bool {
        matches!(
            self,
            TorrentState::Downloading | TorrentState::Seeding | TorrentState::Metadata
        )
    }

    fn error_message(&self) -> Option<String> {
        match self {
            TorrentState::Error(message) => Some(message.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TorrentSnapshot {
    pub id: TorrentId,
    pub name: String,
    pub info_hash: [u8; 20],
    pub state: TorrentState,
    pub error: Option<String>,
    pub progress: f64,
    pub downloaded: u64,
    pub uploaded: u64,
    pub left: u64,
    pub size: u64,
    pub download_rate: u64,
    pub upload_rate: u64,
    pub peers: u32,
    pub seeds: u32,
    pub save_path: PathBuf,
    pub torrent_path: PathBuf,
    pub category: String,
    pub tags: Vec<String>,
    pub sequential: bool,
    pub added_at: i64,
    pub completed_at: Option<i64>,
    pub piece_count: u32,
    pub pieces_have: u32,
    pub ratio: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    Added {
        id: TorrentId,
    },
    Removed {
        id: TorrentId,
    },
    StateChanged {
        id: TorrentId,
        state: TorrentState,
    },
    Progress {
        id: TorrentId,
        snapshot: Box<TorrentSnapshot>,
    },
    Completed {
        id: TorrentId,
    },
    Error {
        id: TorrentId,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransferStats {
    pub downloaded: u64,
    pub uploaded: u64,
    pub download_rate: u64,
    pub upload_rate: u64,
    pub torrents: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("torrent {0} not found")]
    NotFound(TorrentId),
    #[error("torrent {0} has no metadata yet")]
    NoMetadata(TorrentId),
}

const EVENT_CHANNEL_CAPACITY: usize = 256;
const RATE_TICK: Duration = Duration::from_secs(1);
const RATE_EMA_ALPHA: f64 = 0.2;

#[derive(Debug, Default)]
struct RateSample {
    last_downloaded: u64,
    last_uploaded: u64,
    download_rate: f64,
    upload_rate: f64,
    initialized: bool,
}

impl RateSample {
    fn tick(&mut self, downloaded: u64, uploaded: u64, dt: f64) {
        if !self.initialized {
            self.last_downloaded = downloaded;
            self.last_uploaded = uploaded;
            self.initialized = true;
            return;
        }
        let dt = if dt > 0.0 { dt } else { 1.0 };
        let inst_dl = downloaded.saturating_sub(self.last_downloaded) as f64 / dt;
        let inst_ul = uploaded.saturating_sub(self.last_uploaded) as f64 / dt;
        self.download_rate = RATE_EMA_ALPHA * inst_dl + (1.0 - RATE_EMA_ALPHA) * self.download_rate;
        self.upload_rate = RATE_EMA_ALPHA * inst_ul + (1.0 - RATE_EMA_ALPHA) * self.upload_rate;
        self.last_downloaded = downloaded;
        self.last_uploaded = uploaded;
    }
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
    pub encryption: EncryptionPolicy,
    pub utp: UtpOptions,
    pub preallocate: Preallocate,
    /// Piece LRU for the seeding read path. 0 disables it (the default).
    pub piece_cache_pieces: usize,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            listen_port: DEFAULT_LISTEN_PORT,
            max_peers_per_torrent: DEFAULT_MAX_PEERS_PER_TORRENT,
            max_peers_global: DEFAULT_MAX_PEERS_GLOBAL,
            state_dir: Some(util::paths::state_dir()),
            dht: DhtOptions::default(),
            encryption: EncryptionPolicy::default(),
            utp: UtpOptions::default(),
            preallocate: Preallocate::default(),
            piece_cache_pieces: 0,
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
    pub torrent_state: Arc<Mutex<TorrentState>>,
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
    pub category: String,
    pub tags: Vec<String>,
    pub sequential: bool,
    pub file_priorities: Vec<i64>,
    pub pr_rx: Receiver<PieceResult>,
    rates: Arc<Mutex<RateSample>>,
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
    utp: Arc<Mutex<Option<Arc<UtpSocket>>>>,
    utp_bind: tokio::sync::watch::Sender<UtpBindState>,
    owns_lifecycle: bool,
    hasher: Arc<PieceHasher>,
    event_tx: tokio::sync::broadcast::Sender<SessionEvent>,
}

pub(crate) struct PendingTorrent {
    metadata: Arc<MetadataStore>,
    peer_states: Arc<PeerStates>,
    torrent_state: Arc<Mutex<TorrentState>>,
    download_state: Arc<Mutex<DownloadState>>,
    uploaded: Arc<AtomicU64>,
    choke_notify: Arc<Notify>,
    have_broadcast: Arc<tokio::sync::broadcast::Sender<u32>>,
    downloaded_state: Arc<Slot<Arc<TorrentDownloadedState>>>,
    storage: Arc<Slot<Arc<Storage>>>,
    torrent: Arc<Slot<Arc<Torrent>>>,
    piece_tx: Arc<Slot<flume::Sender<crate::peer_connection::FullPiece>>>,
    promote_notify: Arc<Notify>,
    trackers: Vec<String>,
    output_dir: PathBuf,
    pr_tx: flume::Sender<PieceResult>,
    pr_rx: Receiver<PieceResult>,
    added_at: i64,
    start_paused: bool,
    verify: bool,
    skip_checking: bool,
    category: String,
    tags: Vec<String>,
    sequential: bool,
    file_priorities: Vec<i64>,
    rates: Arc<Mutex<RateSample>>,
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

enum AddSource {
    Meta(Box<TorrentMeta>),
    Magnet(Magnet),
}

pub struct AddTorrentOptions {
    source: AddSource,
    name: String,
    output_dir: Option<PathBuf>,
    seed: bool,
    verify: bool,
    paused: Option<bool>,
    category: String,
    tags: Vec<String>,
    sequential: bool,
    skip_checking: bool,
    file_priorities: Vec<i64>,
}

impl AddTorrentOptions {
    fn from_meta(torrent_meta: TorrentMeta) -> Self {
        let name = torrent_meta.torrent_file.info.name.clone();
        Self {
            source: AddSource::Meta(Box::new(torrent_meta)),
            name,
            output_dir: None,
            seed: false,
            verify: false,
            paused: None,
            category: String::new(),
            tags: Vec::new(),
            sequential: false,
            skip_checking: false,
            file_priorities: Vec::new(),
        }
    }

    pub fn from_path(path: &str) -> anyhow::Result<Self> {
        let torrent_meta = file::from_filename(path)?;
        Ok(Self::from_meta(torrent_meta))
    }

    pub fn from_magnet(magnet: &Magnet) -> Self {
        Self {
            name: magnet.name_or_hash(),
            source: AddSource::Magnet(magnet.clone()),
            output_dir: None,
            seed: false,
            verify: false,
            paused: None,
            category: String::new(),
            tags: Vec::new(),
            sequential: false,
            skip_checking: false,
            file_priorities: Vec::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_magnet(&self) -> bool {
        matches!(self.source, AddSource::Magnet(_))
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

    pub fn save_path(self, dir: impl Into<PathBuf>) -> Self {
        self.output_dir(dir)
    }

    pub fn paused(mut self, paused: bool) -> Self {
        self.paused = Some(paused);
        self
    }

    pub fn category(mut self, category: impl Into<String>) -> Self {
        self.category = category.into();
        self
    }

    pub fn tags(mut self, tags: impl Into<Vec<String>>) -> Self {
        self.tags = tags.into();
        self
    }

    pub fn sequential(mut self, sequential: bool) -> Self {
        self.sequential = sequential;
        self
    }

    pub fn skip_checking(mut self, skip: bool) -> Self {
        self.skip_checking = skip;
        self
    }

    pub fn file_priorities(mut self, priorities: impl Into<Vec<i64>>) -> Self {
        self.file_priorities = priorities.into();
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

    fn try_from(input: &str) -> Result<Self, Self::Error> {
        if input.starts_with("magnet:") {
            let magnet = Magnet::parse(input)?;
            Ok(Self::from_magnet(&magnet))
        } else {
            Self::from_path(input)
        }
    }
}

pub struct AddTorrentResult {
    pub id: TorrentId,
    pub torrent: Torrent,
    pub torrent_meta: Option<TorrentMeta>,
    pub pr_rx: Receiver<PieceResult>,
    pub resume_status: ResumeStatus,
    pub already_have: Vec<PieceResult>,
    fetching_metadata: bool,
    metadata: Arc<MetadataStore>,
    peer_states: Arc<PeerStates>,
    ready: tokio::sync::watch::Receiver<Option<Arc<Torrent>>>,
}

impl AddTorrentResult {
    pub fn info_hash(&self) -> [u8; 20] {
        self.id.0
    }

    pub fn is_fetching_metadata(&self) -> bool {
        self.fetching_metadata && self.metadata.info_bytes().is_none()
    }

    pub fn peer_count(&self) -> usize {
        self.peer_states.len()
    }

    pub async fn metadata(&self) -> anyhow::Result<Arc<Torrent>> {
        if let Some(torrent) = self.ready.borrow().clone() {
            return Ok(torrent);
        }
        let bytes = self.metadata.wait().await;
        let meta = TorrentMeta::from_info_bytes(bytes)?;
        let torrent = Arc::new(Torrent::new(&meta)?);
        let mut rx = self.ready.clone();
        if rx.borrow().is_some() {
            return Ok(rx.borrow().clone().expect("ready"));
        }
        loop {
            if let Some(ready) = rx.borrow().clone() {
                return Ok(ready);
            }
            if rx.changed().await.is_err() {
                return Ok(torrent);
            }
        }
    }
}

fn bind_tcp_listener(port: u16) -> std::io::Result<TcpListener> {
    let std_listener = std::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port)))?;
    std_listener.set_nonblocking(true)?;
    TcpListener::from_std(std_listener)
}

impl Session {
    pub fn new() -> Self {
        Self::with_options(SessionOptions::default())
    }

    pub fn with_options(options: SessionOptions) -> Self {
        let (utp_tx, utp_rx) = tokio::sync::watch::channel(None);
        let connector: Arc<dyn Connector> = if options.utp.enabled {
            Arc::new(RacingConnector::new(
                UtpConnector::deferred(utp_rx),
                TcpConnector::new(),
            ))
        } else {
            Arc::new(TcpConnector::new())
        };
        Self::with_connector_and_utp(options, connector, utp_tx)
    }

    pub fn with_connector(options: SessionOptions, connector: Arc<dyn Connector>) -> Self {
        let (utp_tx, _utp_rx) = tokio::sync::watch::channel(None);
        Self::with_connector_and_utp(options, connector, utp_tx)
    }

    fn with_connector_and_utp(
        options: SessionOptions,
        connector: Arc<dyn Connector>,
        utp_bind: tokio::sync::watch::Sender<UtpBindState>,
    ) -> Self {
        let connector: Arc<dyn Connector> =
            Arc::new(MseConnector::new(connector, options.encryption));
        let (event_tx, _) = tokio::sync::broadcast::channel(EVENT_CHANNEL_CAPACITY);
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
            utp: Arc::new(Mutex::new(None)),
            utp_bind,
            owns_lifecycle: true,
            hasher: Arc::new(PieceHasher::new()),
            event_tx,
        };
        session
            .extensions
            .register(|ctx| Box::new(UtMetadata::new(ctx.clone())));
        session.spawn_listener();
        session.start_dht();
        session.spawn_tick();
        session
    }

    fn share(&self) -> Self {
        Self {
            torrents: self.torrents.clone(),
            download_state: self.download_state.clone(),
            peer_id: self.peer_id,
            options: self.options.clone(),
            listen_addr: self.listen_addr.clone(),
            global_peers: self.global_peers.clone(),
            cancel: self.cancel.clone(),
            extensions: self.extensions.clone(),
            connector: self.connector.clone(),
            pending: self.pending.clone(),
            dht: self.dht.clone(),
            utp: self.utp.clone(),
            utp_bind: self.utp_bind.clone(),
            owns_lifecycle: false,
            hasher: self.hasher.clone(),
            event_tx: self.event_tx.clone(),
        }
    }

    /// Reload torrents persisted under `options.state_dir`. One-shot callers
    /// should keep using `new` / `with_options`.
    pub async fn open(options: SessionOptions) -> anyhow::Result<Self> {
        let session = Self::with_options(options);
        session.load_persisted().await;
        Ok(session)
    }

    async fn load_persisted(&self) {
        let Some(state_dir) = self.options.state_dir.clone() else {
            return;
        };
        for path in resume::list_resume_files(&state_dir) {
            match resume::load_optional(&path) {
                Ok(Some(data)) => {
                    if let Err(e) = self.restore_resume(&state_dir, data).await {
                        warn!(
                            path = %path.display(),
                            error = %e,
                            "skipping resume entry"
                        );
                    }
                }
                Ok(None) => {
                    warn!(path = %path.display(), "skipping unreadable resume file");
                }
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping resume entry"
                    );
                }
            }
        }
    }

    async fn restore_resume(
        &self,
        state_dir: &std::path::Path,
        data: resume::ResumeData,
    ) -> anyhow::Result<()> {
        let Some(info_hash) = data.info_hash() else {
            anyhow::bail!("resume file info hash is not 20 bytes");
        };
        let cached = resume::torrent_cache_path(state_dir, &info_hash);
        let torrent_path =
            if !data.torrent_path.is_empty() && std::path::Path::new(&data.torrent_path).exists() {
                PathBuf::from(&data.torrent_path)
            } else {
                cached
            };
        if !torrent_path.exists() {
            anyhow::bail!("cached torrent missing at {}", torrent_path.display());
        }
        let Some(path_str) = torrent_path.to_str() else {
            anyhow::bail!("cached torrent path is not utf-8");
        };
        let meta = file::from_filename(path_str)?;
        self.add_torrent(
            AddTorrentOptions::from(meta)
                .output_dir(PathBuf::from(&data.output_dir))
                .paused(data.is_paused())
                .category(data.category.clone())
                .tags(data.tags.clone())
                .sequential(data.is_sequential())
                .file_priorities(data.file_priorities.clone()),
        )
        .await?;
        Ok(())
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
            encryption: self.options.encryption,
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

    /// Loopback-facing address of the dedicated uTP UDP socket.
    pub fn utp_local_addr(&self) -> Option<SocketAddr> {
        let socket = self.utp.lock().unwrap().clone()?;
        let addr = socket.local_addr().ok()?;
        if addr.ip().is_unspecified() {
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                addr.port(),
            ))
        } else {
            Some(addr)
        }
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
        let encryption = self.options.encryption;

        let listener = match bind_tcp_listener(port) {
            Ok(listener) => listener,
            Err(e) => {
                warn!(port, error = %e, "failed to bind listen port");
                if self.options.utp.enabled {
                    let _ = self.utp_bind.send(Some(Err(())));
                }
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
                if self.options.utp.enabled {
                    let _ = self.utp_bind.send(Some(Err(())));
                }
                return;
            }
        }
        if self.options.utp.enabled {
            self.bind_utp();
        }

        tokio::spawn(async move {
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
                                    encryption,
                                },
                            )
                            .await;
                        });
                    }
                }
            }
        });
        if let Some(socket) = self.utp.lock().unwrap().clone() {
            self.spawn_utp_accept(socket);
        }
    }

    fn bind_utp(&self) {
        let port = if self.options.utp.port != 0 {
            self.options.utp.port
        } else {
            self.listen_port()
        };
        let bind_addr = SocketAddr::from(([0, 0, 0, 0], port));
        let socket = match UtpSocket::bind_std(bind_addr) {
            Ok(socket) => socket,
            Err(e) => {
                warn!(port, error = %e, "uTP bind failed, trying an ephemeral port");
                match UtpSocket::bind_std(SocketAddr::from(([0, 0, 0, 0], 0))) {
                    Ok(socket) => socket,
                    Err(e) => {
                        warn!(error = %e, "failed to bind uTP socket");
                        let _ = self.utp_bind.send(Some(Err(())));
                        return;
                    }
                }
            }
        };
        if let Ok(addr) = socket.local_addr() {
            info!(%addr, "listening for incoming uTP peers");
        }
        let socket = Arc::new(socket);
        *self.utp.lock().unwrap() = Some(socket.clone());
        let _ = self.utp_bind.send(Some(Ok(socket)));
    }

    fn spawn_utp_accept(&self, socket: Arc<UtpSocket>) {
        let cancel = self.cancel.clone();
        let ctx = self.incoming_context();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    accepted = socket.accept() => {
                        let (stream, addr) = match accepted {
                            Ok(pair) => pair,
                            Err(e) => {
                                debug!(error = %e, "uTP accept failed");
                                break;
                            }
                        };
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            accept_incoming_utp(boxed_stream(stream), addr, ctx).await;
                        });
                    }
                }
            }
        });
    }

    pub fn start_downloading(&self) {
        self.resume_all();
    }

    pub fn pause_all(&self) {
        {
            let mut state = self.download_state.lock().unwrap();
            *state = DownloadState::Paused;
        }
        for entry in self.torrents.iter() {
            self.apply_pause(entry.value(), TorrentId::new(*entry.key()));
        }
        for entry in self.pending.iter() {
            self.apply_pending_pause(entry.value(), TorrentId::new(*entry.key()));
        }
        if let Some(dht) = self.dht() {
            dht.set_all_active(false);
        }
        self.spawn_flush_resume();
    }

    pub fn resume_all(&self) {
        {
            let mut state = self.download_state.lock().unwrap();
            *state = DownloadState::Downloading;
        }
        for entry in self.torrents.iter() {
            self.apply_resume(entry.value(), TorrentId::new(*entry.key()));
        }
        for entry in self.pending.iter() {
            self.apply_pending_resume(entry.value(), TorrentId::new(*entry.key()));
        }
        if let Some(dht) = self.dht() {
            dht.set_all_active(true);
        }
        self.spawn_flush_resume();
    }

    pub fn pause(&self, id: TorrentId) -> Result<(), ControlError> {
        if let Some(torrent) = self.torrents.get(&id.0) {
            self.apply_pause(torrent.value(), id);
            if let Some(dht) = self.dht() {
                dht.set_active(id.0, false);
            }
            self.spawn_flush_one(torrent.value().clone());
            return Ok(());
        }
        if let Some(pending) = self.pending.get(&id.0) {
            self.apply_pending_pause(pending.value(), id);
            if let Some(dht) = self.dht() {
                dht.set_active(id.0, false);
            }
            return Ok(());
        }
        Err(ControlError::NotFound(id))
    }

    pub fn resume(&self, id: TorrentId) -> Result<(), ControlError> {
        if let Some(torrent) = self.torrents.get(&id.0) {
            self.apply_resume(torrent.value(), id);
            {
                let mut state = self.download_state.lock().unwrap();
                *state = DownloadState::Downloading;
            }
            if let Some(dht) = self.dht() {
                dht.set_active(id.0, true);
            }
            self.spawn_flush_one(torrent.value().clone());
            return Ok(());
        }
        if let Some(pending) = self.pending.get(&id.0) {
            self.apply_pending_resume(pending.value(), id);
            {
                let mut state = self.download_state.lock().unwrap();
                *state = DownloadState::Downloading;
            }
            if let Some(dht) = self.dht() {
                dht.set_active(id.0, true);
            }
            return Ok(());
        }
        Err(ControlError::NotFound(id))
    }

    pub fn list(&self) -> Vec<TorrentSnapshot> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for entry in self.torrents.iter() {
            seen.insert(*entry.key());
            out.push(torrent_snapshot(entry.value()));
        }
        for entry in self.pending.iter() {
            if seen.contains(entry.key()) {
                continue;
            }
            out.push(pending_snapshot(*entry.key(), entry.value()));
        }
        out
    }

    pub fn snapshot(&self, id: TorrentId) -> Option<TorrentSnapshot> {
        if let Some(torrent) = self.torrents.get(&id.0) {
            return Some(torrent_snapshot(torrent.value()));
        }
        self.pending
            .get(&id.0)
            .map(|pending| pending_snapshot(id.0, pending.value()))
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<SessionEvent> {
        self.event_tx.subscribe()
    }

    pub fn transfer_stats(&self) -> TransferStats {
        let mut stats = TransferStats::default();
        for entry in self.torrents.iter() {
            let torrent = entry.value();
            stats.downloaded += torrent.downloaded_state.downloaded_bytes();
            stats.uploaded += torrent.uploaded.load(Ordering::Relaxed);
            let rates = torrent.rates.lock().unwrap();
            stats.download_rate += rates.download_rate.max(0.0).round() as u64;
            stats.upload_rate += rates.upload_rate.max(0.0).round() as u64;
            stats.torrents += 1;
        }
        for entry in self.pending.iter() {
            if self.torrents.contains_key(entry.key()) {
                continue;
            }
            let pending = entry.value();
            stats.downloaded += pending.downloaded_state.get().downloaded_bytes();
            stats.uploaded += pending.uploaded.load(Ordering::Relaxed);
            let rates = pending.rates.lock().unwrap();
            stats.download_rate += rates.download_rate.max(0.0).round() as u64;
            stats.upload_rate += rates.upload_rate.max(0.0).round() as u64;
            stats.torrents += 1;
        }
        stats
    }

    pub async fn recheck(&self, id: TorrentId) -> Result<(), ControlError> {
        if self.pending.contains_key(&id.0) && !self.torrents.contains_key(&id.0) {
            return Err(ControlError::NoMetadata(id));
        }
        let torrent = self
            .torrents
            .get(&id.0)
            .map(|entry| entry.clone())
            .ok_or(ControlError::NotFound(id))?;
        let previous = torrent.torrent_state.lock().unwrap().clone();
        let stay_paused = previous == TorrentState::Paused;
        self.set_torrent_state(&torrent, id, TorrentState::Checking);
        torrent.downloaded_state.clear_all_downloaded();
        resume::verify_existing_pieces(&torrent.storage, &torrent.downloaded_state).await;
        if torrent.downloaded_state.is_complete() {
            let mut done = torrent.completed_at.lock().unwrap();
            if done.is_none() {
                *done = Some(resume::now_unix());
            }
        }
        let next = if stay_paused {
            TorrentState::Paused
        } else {
            active_state(&torrent.downloaded_state)
        };
        self.set_torrent_state(&torrent, id, next);
        self.spawn_flush_one(torrent);
        Ok(())
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

    fn spawn_flush_one(&self, torrent: Arc<TorrentSession>) {
        let Some(state_dir) = self.options.state_dir.clone() else {
            return;
        };
        tokio::spawn(async move {
            if let Err(e) = persist_torrent(&state_dir, &torrent) {
                warn!(
                    name = %torrent.torrent.name,
                    error = %e,
                    "failed to persist resume data"
                );
            }
        });
    }

    fn emit(&self, event: SessionEvent) {
        let _ = self.event_tx.send(event);
    }

    fn set_torrent_state(&self, torrent: &TorrentSession, id: TorrentId, state: TorrentState) {
        let changed = {
            let mut current = torrent.torrent_state.lock().unwrap();
            if *current == state {
                false
            } else {
                *current = state.clone();
                true
            }
        };
        sync_download_state(&torrent.download_state, &state);
        if matches!(
            state,
            TorrentState::Paused | TorrentState::Checking | TorrentState::Error(_)
        ) {
            choke_all_peers(&torrent.peer_states);
        } else if state.transfer_enabled() {
            torrent.choke_notify.notify_waiters();
        }
        if changed {
            self.emit(SessionEvent::StateChanged { id, state });
        }
    }

    fn apply_pause(&self, torrent: &TorrentSession, id: TorrentId) {
        self.set_torrent_state(torrent, id, TorrentState::Paused);
    }

    fn apply_resume(&self, torrent: &TorrentSession, id: TorrentId) {
        let next = active_state(&torrent.downloaded_state);
        self.set_torrent_state(torrent, id, next);
    }

    fn apply_pending_pause(&self, pending: &PendingTorrent, id: TorrentId) {
        let changed = {
            let mut current = pending.torrent_state.lock().unwrap();
            if *current == TorrentState::Paused {
                false
            } else {
                *current = TorrentState::Paused;
                true
            }
        };
        sync_download_state(&pending.download_state, &TorrentState::Paused);
        choke_all_peers(&pending.peer_states);
        if changed {
            self.emit(SessionEvent::StateChanged {
                id,
                state: TorrentState::Paused,
            });
        }
    }

    fn apply_pending_resume(&self, pending: &PendingTorrent, id: TorrentId) {
        let changed = {
            let mut current = pending.torrent_state.lock().unwrap();
            if *current == TorrentState::Metadata {
                false
            } else {
                *current = TorrentState::Metadata;
                true
            }
        };
        sync_download_state(&pending.download_state, &TorrentState::Metadata);
        pending.choke_notify.notify_waiters();
        if changed {
            self.emit(SessionEvent::StateChanged {
                id,
                state: TorrentState::Metadata,
            });
        }
    }

    fn spawn_tick(&self) {
        let torrents = self.torrents.clone();
        let pending = self.pending.clone();
        let event_tx = self.event_tx.clone();
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(RATE_TICK);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = interval.tick() => {
                        tick_session(&torrents, &pending, &event_tx);
                    }
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
        let (advertise_dht, dht_port, dht) = self.dht_peer_fields();
        if let Some(torrent) = self.torrents.get(info_hash).map(|entry| entry.clone()) {
            return try_spawn_peer(spawn_from_session(
                addr,
                *info_hash,
                self.peer_id,
                &torrent,
                None,
                self.extensions.clone(),
                self.listen_port(),
                advertise_dht,
                dht_port,
                dht,
                self.global_peers.clone(),
                self.options.max_peers_per_torrent,
                self.options.max_peers_global,
                self.connector.clone(),
                self.options.encryption,
            ));
        }
        let Some(pending) = self.pending.get(info_hash).map(|entry| entry.clone()) else {
            return false;
        };
        try_spawn_peer(spawn_from_pending(
            addr,
            *info_hash,
            self.peer_id,
            &pending,
            None,
            self.extensions.clone(),
            self.listen_port(),
            advertise_dht,
            dht_port,
            dht,
            self.global_peers.clone(),
            self.options.max_peers_per_torrent,
            self.options.max_peers_global,
            self.connector.clone(),
            self.options.encryption,
        ))
    }

    pub fn torrent_session(&self, info_hash: &[u8; 20]) -> Option<Arc<TorrentSession>> {
        self.torrents.get(info_hash).map(|entry| entry.clone())
    }

    pub fn remove_torrent(&self, id: TorrentId, delete_files: bool) -> Result<(), ControlError> {
        let torrent = self.torrents.remove(&id.0).map(|(_, t)| t);
        let pending = self.pending.remove(&id.0).map(|(_, p)| p);
        if torrent.is_none() && pending.is_none() {
            return Err(ControlError::NotFound(id));
        }
        if let Some(dht) = self.dht() {
            dht.remove_torrent(id.0);
        }
        if let Some(torrent) = torrent {
            disconnect_all_peers(&torrent.peer_states);
            torrent.tracker.shutdown();
            if let Some(state_dir) = self.options.state_dir.as_ref() {
                let resume_file = resume::resume_path(state_dir, &id.0);
                if let Err(e) = std::fs::remove_file(&resume_file) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        debug!(path = %resume_file.display(), error = %e, "failed to delete resume file");
                    }
                }
                if !torrent.torrent_cache_path.as_os_str().is_empty() {
                    if let Err(e) = std::fs::remove_file(&torrent.torrent_cache_path) {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            debug!(
                                path = %torrent.torrent_cache_path.display(),
                                error = %e,
                                "failed to delete cached torrent"
                            );
                        }
                    }
                }
            }
            if delete_files {
                delete_torrent_files(&torrent.torrent, &torrent.output_dir);
            }
        } else if let Some(pending) = pending {
            disconnect_all_peers(&pending.peer_states);
            if delete_files {
                delete_torrent_files(&pending.torrent.get(), &pending.output_dir);
            }
        }
        self.emit(SessionEvent::Removed { id });
        Ok(())
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

        let dummy = AddTorrentOptions {
            source: AddSource::Magnet(crate::magnet::Magnet {
                info_hash,
                display_name: None,
                trackers: Vec::new(),
                peers: Vec::new(),
                webseeds: Vec::new(),
            }),
            name: resume::info_hash_hex(&info_hash),
            output_dir: opts.output_dir.clone(),
            seed: false,
            verify: false,
            paused: None,
            category: String::new(),
            tags: Vec::new(),
            sequential: false,
            skip_checking: false,
            file_priorities: Vec::new(),
        };
        let pending = self
            .register_pending(
                info_hash,
                resume::info_hash_hex(&info_hash),
                Vec::new(),
                opts.output_dir
                    .unwrap_or_else(|| std::env::temp_dir().join("bitrev-metadata")),
                &dummy,
            )
            .await?;
        Ok(MetadataHandle {
            info_hash,
            store: pending.metadata.clone(),
            peer_states: pending.peer_states.clone(),
        })
    }

    async fn register_pending(
        &self,
        info_hash: [u8; 20],
        name: String,
        trackers: Vec<String>,
        output_dir: PathBuf,
        opts: &AddTorrentOptions,
    ) -> anyhow::Result<Arc<PendingTorrent>> {
        if let Some(pending) = self.pending.get(&info_hash) {
            return Ok(pending.clone());
        }
        let metadata = MetadataStore::new(info_hash);
        let torrent = Arc::new(Torrent {
            info_hash,
            piece_hashes: Vec::new(),
            piece_length: 16 * 1024,
            length: 0,
            files: Vec::new(),
            name,
            private: false,
        });
        let storage = Storage::open_with(
            &torrent,
            &output_dir,
            StorageOptions {
                preallocate: Preallocate::Off,
                piece_cache_pieces: 0,
                hasher: self.hasher.clone(),
            },
        )
        .await?;
        let (piece_tx, _piece_rx) = flume::unbounded();
        let (pr_tx, pr_rx) = flume::unbounded();
        let start_paused = opts.paused.unwrap_or(false);
        let torrent_state = Arc::new(Mutex::new(TorrentState::Metadata));
        let download_state = Arc::new(Mutex::new(DownloadState::Init));
        sync_download_state(&download_state, &TorrentState::Metadata);
        let pending = Arc::new(PendingTorrent {
            metadata,
            peer_states: Arc::new(PeerStates::default()),
            torrent_state,
            download_state,
            uploaded: Arc::new(AtomicU64::new(0)),
            choke_notify: Arc::new(Notify::new()),
            have_broadcast: Arc::new(tokio::sync::broadcast::channel(8).0),
            downloaded_state: Slot::new(Arc::new(TorrentDownloadedState::new(Vec::new()))),
            storage: Slot::new(storage),
            torrent: Slot::new(torrent),
            piece_tx: Slot::new(piece_tx),
            promote_notify: Arc::new(Notify::new()),
            trackers,
            output_dir,
            pr_tx,
            pr_rx,
            added_at: resume::now_unix(),
            start_paused,
            verify: opts.verify,
            skip_checking: opts.skip_checking,
            category: opts.category.clone(),
            tags: opts.tags.clone(),
            sequential: opts.sequential,
            file_priorities: opts.file_priorities.clone(),
            rates: Arc::new(Mutex::new(RateSample::default())),
        });
        self.pending.insert(info_hash, pending.clone());
        Ok(pending)
    }

    pub async fn add_torrent(
        &self,
        add_torrent: AddTorrentOptions,
    ) -> anyhow::Result<AddTorrentResult> {
        match &add_torrent.source {
            AddSource::Magnet(magnet) => self.add_magnet(magnet.clone(), add_torrent).await,
            AddSource::Meta(torrent_meta) => {
                self.add_torrent_meta((**torrent_meta).clone(), add_torrent, None)
                    .await
            }
        }
    }

    fn result_from_existing(&self, existing: &TorrentSession) -> AddTorrentResult {
        let torrent = existing.torrent.clone();
        let (ready_tx, ready_rx) = tokio::sync::watch::channel(Some(torrent.clone()));
        let _ = ready_tx;
        AddTorrentResult {
            id: TorrentId::new(torrent.info_hash),
            torrent: (*torrent).clone(),
            torrent_meta: Some(existing.torrent_meta.clone()),
            pr_rx: existing.pr_rx.clone(),
            resume_status: ResumeStatus::FastPath,
            already_have: already_have_from(&existing.downloaded_state),
            fetching_metadata: false,
            metadata: existing.metadata.clone(),
            peer_states: existing.peer_states.clone(),
            ready: ready_rx,
        }
    }

    async fn add_torrent_meta(
        &self,
        torrent_meta: TorrentMeta,
        add_torrent: AddTorrentOptions,
        reuse: Option<Arc<PendingTorrent>>,
    ) -> anyhow::Result<AddTorrentResult> {
        if reuse.is_none() {
            if let Some(existing) = self.torrents.get(&torrent_meta.info_hash) {
                return Ok(self.result_from_existing(existing.value()));
            }
            if let Some(pending) = self.pending.get(&torrent_meta.info_hash) {
                return Ok(result_from_pending(pending.value(), torrent_meta.info_hash));
            }
        }
        let torrent = Torrent::new(&torrent_meta)?;
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
        let output_dir = add_torrent
            .output_dir
            .clone()
            .or_else(|| reuse.as_ref().map(|p| p.output_dir.clone()))
            .unwrap_or_else(|| PathBuf::from(&torrent.name));
        let seed = add_torrent.seed;
        let skip_checking =
            add_torrent.skip_checking || reuse.as_ref().is_some_and(|p| p.skip_checking);
        let verify =
            (add_torrent.verify || reuse.as_ref().is_some_and(|p| p.verify)) && !skip_checking;
        let category = if add_torrent.category.is_empty() {
            reuse
                .as_ref()
                .map(|p| p.category.clone())
                .unwrap_or_default()
        } else {
            add_torrent.category.clone()
        };
        let tags = if add_torrent.tags.is_empty() {
            reuse.as_ref().map(|p| p.tags.clone()).unwrap_or_default()
        } else {
            add_torrent.tags.clone()
        };
        let sequential = add_torrent.sequential || reuse.as_ref().is_some_and(|p| p.sequential);
        let file_priorities = if add_torrent.file_priorities.is_empty() {
            reuse
                .as_ref()
                .map(|p| p.file_priorities.clone())
                .unwrap_or_default()
        } else {
            add_torrent.file_priorities.clone()
        };

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
            Some(_) if skip_checking => true,
            Some(data) if !verify => resume::files_match(&data.files, &torrent, &output_dir),
            _ => false,
        };

        let storage = Storage::open_with(
            &torrent,
            &output_dir,
            StorageOptions {
                preallocate: if seed {
                    Preallocate::Off
                } else {
                    self.options.preallocate
                },
                piece_cache_pieces: self.options.piece_cache_pieces,
                hasher: self.hasher.clone(),
            },
        )
        .await?;

        let (pr_tx, pr_rx) = if let Some(pending) = reuse.as_ref() {
            (pending.pr_tx.clone(), pending.pr_rx.clone())
        } else {
            flume::bounded::<PieceResult>(torrent.piece_hashes.len().max(1) * 2)
        };
        let have_broadcast = reuse
            .as_ref()
            .map(|p| p.have_broadcast.clone())
            .unwrap_or_else(|| Arc::new(tokio::sync::broadcast::channel(128).0));
        let peer_states = reuse
            .as_ref()
            .map(|p| p.peer_states.clone())
            .unwrap_or_else(|| Arc::new(PeerStates::default()));
        let uploaded = reuse
            .as_ref()
            .map(|p| p.uploaded.clone())
            .unwrap_or_else(|| {
                Arc::new(AtomicU64::new(
                    loaded_resume
                        .as_ref()
                        .map(|data| data.uploaded.max(0) as u64)
                        .unwrap_or(0),
                ))
            });
        let choke_notify = reuse
            .as_ref()
            .map(|p| p.choke_notify.clone())
            .unwrap_or_else(|| Arc::new(Notify::new()));
        let promote_notify = reuse
            .as_ref()
            .map(|p| p.promote_notify.clone())
            .unwrap_or_else(|| Arc::new(Notify::new()));
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

        let resume_status = if seed {
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

        let start_paused = match add_torrent.paused {
            Some(paused) => paused,
            None => {
                reuse.as_ref().is_some_and(|p| p.start_paused)
                    || loaded_resume.as_ref().is_some_and(|data| data.is_paused())
            }
        };
        let initial_state = if start_paused {
            TorrentState::Paused
        } else {
            active_state(&downloaded_state)
        };
        let download_state = reuse
            .as_ref()
            .map(|p| p.download_state.clone())
            .unwrap_or_else(|| Arc::new(Mutex::new(DownloadState::Init)));
        let torrent_state = reuse
            .as_ref()
            .map(|p| p.torrent_state.clone())
            .unwrap_or_else(|| Arc::new(Mutex::new(initial_state.clone())));
        *torrent_state.lock().unwrap() = initial_state.clone();
        sync_download_state(&download_state, &initial_state);
        let rates = reuse
            .as_ref()
            .map(|p| p.rates.clone())
            .unwrap_or_else(|| Arc::new(Mutex::new(RateSample::default())));

        let metadata = reuse
            .as_ref()
            .map(|p| p.metadata.clone())
            .unwrap_or_else(|| {
                MetadataStore::with_bytes(torrent.info_hash, torrent_meta.info_bytes.clone())
            });
        let torrent_slot = reuse
            .as_ref()
            .map(|p| p.torrent.clone())
            .unwrap_or_else(|| Slot::new(torrent.clone()));
        let storage_slot = reuse
            .as_ref()
            .map(|p| p.storage.clone())
            .unwrap_or_else(|| Slot::new(storage.clone()));
        let downloaded_slot = reuse
            .as_ref()
            .map(|p| p.downloaded_state.clone())
            .unwrap_or_else(|| Slot::new(downloaded_state.clone()));
        let piece_tx_slot = reuse
            .as_ref()
            .map(|p| p.piece_tx.clone())
            .unwrap_or_else(|| Slot::new(flume::unbounded().0));
        torrent_slot.set(torrent.clone());
        storage_slot.set(storage.clone());
        downloaded_slot.set(downloaded_state.clone());

        let tracker_stream = TrackerPeers::new(
            torrent_meta.clone(),
            15,
            self.peer_id,
            peer_states.clone(),
            have_broadcast.clone(),
            pr_rx.clone(),
            download_state.clone(),
        );
        piece_tx_slot.set(tracker_stream.piece_tx.clone());

        let listen_port =
            tokio::time::timeout(std::time::Duration::from_millis(250), self.wait_listening())
                .await
                .map(|addr| addr.port())
                .unwrap_or_else(|_| self.listen_port());
        tracker_stream
            .connect(crate::tracker_peers::PeerSpawnRuntime {
                info_hash: torrent.info_hash,
                peer_id: self.peer_id,
                storage: storage_slot.clone(),
                downloaded_state: downloaded_slot.clone(),
                uploaded: uploaded.clone(),
                torrent: torrent_slot.clone(),
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
                piece_tx: piece_tx_slot.clone(),
                promote_notify: promote_notify.clone(),
                encryption: self.options.encryption,
            })
            .await;

        if let Some(dht) = self.dht() {
            match tracker_stream.register_source(DiscoverySource::Dht) {
                Ok(()) => {
                    let active = initial_state.transfer_enabled();
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
            download_state.clone(),
            choke_notify.clone(),
            tracker_stream.cancel_token(),
        );

        let have_broadcast_writer = have_broadcast.clone();
        let piece_rx = tracker_stream.piece_rx.clone();
        let storage_writer = storage.clone();
        let downloaded_writer = downloaded_state.clone();
        let persist_state_dir = self.options.state_dir.clone();
        let persist_output_dir = output_dir.clone();
        let persist_meta_torrent = torrent.clone();
        let persist_uploaded = uploaded.clone();
        let persist_torrent_state = torrent_state.clone();
        let persist_added_at = added_at;
        let persist_completed_at = completed_at.clone();
        let persist_cache_path = torrent_cache_path.clone();
        let persist_category = category.clone();
        let persist_tags = tags.clone();
        let persist_sequential = sequential;
        let persist_file_priorities = file_priorities.clone();
        let event_tx = self.event_tx.clone();
        let completed_id = TorrentId::new(torrent.info_hash);
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
                    let first_complete = {
                        let mut done = persist_completed_at.lock().unwrap();
                        if done.is_none() {
                            *done = Some(resume::now_unix());
                            true
                        } else {
                            false
                        }
                    };
                    if first_complete {
                        if let Err(e) = storage_writer.sync_all().await {
                            debug!(error = %e, "failed to sync files on completion");
                        }
                        let _ = event_tx.send(SessionEvent::Completed { id: completed_id });
                        let was_downloading = {
                            let mut state = persist_torrent_state.lock().unwrap();
                            if *state == TorrentState::Downloading {
                                *state = TorrentState::Seeding;
                                true
                            } else {
                                false
                            }
                        };
                        if was_downloading {
                            let _ = event_tx.send(SessionEvent::StateChanged {
                                id: completed_id,
                                state: TorrentState::Seeding,
                            });
                        }
                    }
                }
                if let Some(state_dir) = persist_state_dir.as_ref() {
                    let paused = *persist_torrent_state.lock().unwrap() == TorrentState::Paused;
                    if let Err(e) = persist_from_parts(
                        state_dir,
                        &persist_meta_torrent.info_hash,
                        &persist_output_dir,
                        &persist_meta_torrent,
                        &downloaded_writer,
                        persist_uploaded.load(Ordering::Relaxed),
                        paused,
                        &persist_cache_path,
                        persist_added_at,
                        *persist_completed_at.lock().unwrap(),
                        &persist_category,
                        &persist_tags,
                        persist_sequential,
                        &persist_file_priorities,
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
        let (ready_tx, ready_rx) = tokio::sync::watch::channel(Some(torrent.clone()));
        let _ = ready_tx;
        let torrent_session = Arc::new(TorrentSession {
            tracker: tracker_stream,
            storage,
            downloaded_state,
            peer_states: peer_states.clone(),
            piece_tx,
            have_broadcast,
            torrent_state,
            download_state,
            uploaded,
            torrent: torrent.clone(),
            torrent_meta: torrent_meta.clone(),
            choke_notify,
            output_dir,
            added_at,
            completed_at,
            torrent_cache_path,
            metadata: metadata.clone(),
            category,
            tags,
            sequential,
            file_priorities,
            pr_rx: pr_rx.clone(),
            rates,
        });
        let id = TorrentId::new(torrent.info_hash);
        self.torrents
            .insert(torrent.info_hash, torrent_session.clone());

        if let Some(pending) = reuse {
            if torrent.is_private() {
                if let Some(dht) = self.dht() {
                    dht.remove_torrent(torrent.info_hash);
                }
            }
            pending.promote_notify.notify_waiters();
            self.pending.remove(&torrent.info_hash);
        }

        if let Some(state_dir) = self.options.state_dir.clone() {
            if let Err(e) = persist_torrent(&state_dir, &torrent_session) {
                warn!(
                    name = %torrent_session.torrent.name,
                    error = %e,
                    "failed to persist resume data on add"
                );
            }
            spawn_resume_timer(torrent_session, state_dir, self.cancel.clone());
        }

        if initial_state.transfer_enabled() {
            let mut global = self.download_state.lock().unwrap();
            if *global != DownloadState::Paused {
                *global = DownloadState::Downloading;
            }
        }

        self.emit(SessionEvent::Added { id });
        self.emit(SessionEvent::StateChanged {
            id,
            state: initial_state,
        });

        Ok(AddTorrentResult {
            id,
            torrent: (*torrent).clone(),
            torrent_meta: Some(torrent_meta),
            pr_rx,
            resume_status,
            already_have,
            fetching_metadata: false,
            metadata,
            peer_states,
            ready: ready_rx,
        })
    }

    async fn add_magnet(
        &self,
        magnet: Magnet,
        add_torrent: AddTorrentOptions,
    ) -> anyhow::Result<AddTorrentResult> {
        if let Some(existing) = self.torrents.get(&magnet.info_hash) {
            return Ok(self.result_from_existing(existing.value()));
        }
        if let Some(pending) = self.pending.get(&magnet.info_hash) {
            return Ok(result_from_pending(pending.value(), magnet.info_hash));
        }

        let output_dir = add_torrent
            .output_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(magnet.name_or_hash()));
        let pending = self
            .register_pending(
                magnet.info_hash,
                magnet.name_or_hash(),
                magnet.trackers.clone(),
                output_dir,
                &add_torrent,
            )
            .await?;

        let (ready_tx, ready_rx) = tokio::sync::watch::channel(None);
        let placeholder = pending.torrent.get();
        let id = TorrentId::new(magnet.info_hash);
        let result = AddTorrentResult {
            id,
            torrent: (*placeholder).clone(),
            torrent_meta: None,
            pr_rx: pending.pr_rx.clone(),
            resume_status: ResumeStatus::Fresh,
            already_have: Vec::new(),
            fetching_metadata: true,
            metadata: pending.metadata.clone(),
            peer_states: pending.peer_states.clone(),
            ready: ready_rx,
        };

        {
            let mut global = self.download_state.lock().unwrap();
            if *global != DownloadState::Paused {
                *global = DownloadState::Downloading;
            }
        }

        if !magnet.peers.is_empty() {
            self.add_peers(
                &magnet.info_hash,
                DiscoverySource::Tracker,
                magnet.peers.clone(),
            );
        }

        if let Some(dht) = self.dht() {
            let listen_port = self.listen_port();
            dht.add_torrent(
                magnet.info_hash,
                listen_port,
                pending.torrent_state.lock().unwrap().transfer_enabled(),
            );
        }

        self.emit(SessionEvent::Added { id });
        self.emit(SessionEvent::StateChanged {
            id,
            state: TorrentState::Metadata,
        });

        let promoter = self.share();
        let pending_task = pending.clone();

        tokio::spawn(async move {
            let bytes = pending_task.metadata.wait().await;
            let mut meta = match TorrentMeta::from_info_bytes(bytes) {
                Ok(meta) => meta,
                Err(e) => {
                    warn!(error = %e, "failed to parse fetched metadata");
                    let mut state = pending_task.torrent_state.lock().unwrap();
                    *state = TorrentState::Error(e.to_string());
                    let _ = promoter.event_tx.send(SessionEvent::Error {
                        id,
                        message: e.to_string(),
                    });
                    return;
                }
            };
            if !pending_task.trackers.is_empty() {
                meta.torrent_file.announce = Some(pending_task.trackers[0].clone());
                meta.torrent_file.announce_list = Some(
                    pending_task
                        .trackers
                        .iter()
                        .cloned()
                        .map(|url| vec![url])
                        .collect(),
                );
            }

            let mut opts = AddTorrentOptions::from(meta.clone());
            opts.output_dir = Some(pending_task.output_dir.clone());
            opts.verify = pending_task.verify;
            opts.skip_checking = pending_task.skip_checking;
            opts.paused = Some(pending_task.start_paused);
            opts.category = pending_task.category.clone();
            opts.tags = pending_task.tags.clone();
            opts.sequential = pending_task.sequential;
            opts.file_priorities = pending_task.file_priorities.clone();

            match promoter
                .add_torrent_meta(meta, opts, Some(pending_task.clone()))
                .await
            {
                Ok(added) => {
                    let _ = ready_tx.send(Some(Arc::new(added.torrent)));
                }
                Err(e) => {
                    warn!(error = %e, "failed to promote magnet after metadata");
                    let mut state = pending_task.torrent_state.lock().unwrap();
                    *state = TorrentState::Error(e.to_string());
                    let _ = promoter.event_tx.send(SessionEvent::Error {
                        id,
                        message: e.to_string(),
                    });
                }
            }
        });

        Ok(result)
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
    category: &str,
    tags: &[String],
    sequential: bool,
    file_priorities: &[i64],
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
            category,
            tags,
            sequential,
            file_priorities,
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
        *torrent.torrent_state.lock().unwrap() == TorrentState::Paused,
        &torrent.torrent_cache_path,
        torrent.added_at,
        *torrent.completed_at.lock().unwrap(),
        &torrent.category,
        &torrent.tags,
        torrent.sequential,
        &torrent.file_priorities,
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
                    let downloading = torrent
                        .torrent_state
                        .lock()
                        .unwrap()
                        .transfer_enabled();
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

fn already_have_from(downloaded_state: &TorrentDownloadedState) -> Vec<PieceResult> {
    downloaded_state
        .pieces
        .iter()
        .filter(|pw| pw.downloaded.load(Ordering::Relaxed))
        .map(|pw| PieceResult {
            index: pw.piece_work.index,
            length: pw.piece_work.length,
        })
        .collect()
}

fn result_from_pending(pending: &PendingTorrent, info_hash: [u8; 20]) -> AddTorrentResult {
    let torrent = pending.torrent.get();
    let fetching = pending.metadata.info_bytes().is_none();
    let (ready_tx, ready_rx) = if fetching {
        tokio::sync::watch::channel(None)
    } else {
        tokio::sync::watch::channel(Some(torrent.clone()))
    };
    let _ = ready_tx;
    AddTorrentResult {
        id: TorrentId::new(info_hash),
        torrent: (*torrent).clone(),
        torrent_meta: None,
        pr_rx: pending.pr_rx.clone(),
        resume_status: ResumeStatus::Fresh,
        already_have: Vec::new(),
        fetching_metadata: fetching,
        metadata: pending.metadata.clone(),
        peer_states: pending.peer_states.clone(),
        ready: ready_rx,
    }
}

fn active_state(downloaded: &TorrentDownloadedState) -> TorrentState {
    if downloaded.is_complete() {
        TorrentState::Seeding
    } else {
        TorrentState::Downloading
    }
}

fn sync_download_state(download_state: &Mutex<DownloadState>, state: &TorrentState) {
    *download_state.lock().unwrap() = if state.transfer_enabled() {
        DownloadState::Downloading
    } else {
        DownloadState::Paused
    };
}

fn connected_peer_counts(peer_states: &PeerStates, piece_count: usize) -> (u32, u32) {
    let mut peers = 0u32;
    let mut seeds = 0u32;
    for state in peer_states.states.iter() {
        if state.writer_tx.is_none() {
            continue;
        }
        peers += 1;
        if piece_count > 0 && (0..piece_count).all(|i| state.bitfield.has_piece(i)) {
            seeds += 1;
        }
    }
    (peers, seeds)
}

fn snapshot_rates(rates: &Mutex<RateSample>) -> (u64, u64) {
    let rates = rates.lock().unwrap();
    (
        rates.download_rate.max(0.0).round() as u64,
        rates.upload_rate.max(0.0).round() as u64,
    )
}

fn torrent_snapshot(torrent: &TorrentSession) -> TorrentSnapshot {
    let state = torrent.torrent_state.lock().unwrap().clone();
    let downloaded = torrent.downloaded_state.downloaded_bytes();
    let uploaded = torrent.uploaded.load(Ordering::Relaxed);
    let left = torrent.downloaded_state.left_bytes();
    let size = torrent.torrent.length.max(0) as u64;
    let piece_count = torrent.downloaded_state.piece_count() as u32;
    let pieces_have = torrent.downloaded_state.have_count();
    let (peers, seeds) = connected_peer_counts(&torrent.peer_states, piece_count as usize);
    let (download_rate, upload_rate) = snapshot_rates(&torrent.rates);
    TorrentSnapshot {
        id: TorrentId::new(torrent.torrent.info_hash),
        name: torrent.torrent.name.clone(),
        info_hash: torrent.torrent.info_hash,
        error: state.error_message(),
        state,
        progress: if size == 0 {
            0.0
        } else {
            downloaded as f64 / size as f64
        },
        downloaded,
        uploaded,
        left,
        size,
        download_rate,
        upload_rate,
        peers,
        seeds,
        save_path: torrent.output_dir.clone(),
        torrent_path: torrent.torrent_cache_path.clone(),
        category: torrent.category.clone(),
        tags: torrent.tags.clone(),
        sequential: torrent.sequential,
        added_at: torrent.added_at,
        completed_at: *torrent.completed_at.lock().unwrap(),
        piece_count,
        pieces_have,
        ratio: if downloaded == 0 {
            0.0
        } else {
            uploaded as f64 / downloaded as f64
        },
    }
}

fn pending_snapshot(info_hash: [u8; 20], pending: &PendingTorrent) -> TorrentSnapshot {
    let torrent = pending.torrent.get();
    let state = pending.torrent_state.lock().unwrap().clone();
    let downloaded_state = pending.downloaded_state.get();
    let downloaded = downloaded_state.downloaded_bytes();
    let uploaded = pending.uploaded.load(Ordering::Relaxed);
    let left = downloaded_state.left_bytes();
    let size = torrent.length.max(0) as u64;
    let piece_count = downloaded_state.piece_count() as u32;
    let pieces_have = downloaded_state.have_count();
    let (peers, seeds) = connected_peer_counts(&pending.peer_states, piece_count as usize);
    let (download_rate, upload_rate) = snapshot_rates(&pending.rates);
    TorrentSnapshot {
        id: TorrentId::new(info_hash),
        name: torrent.name.clone(),
        info_hash,
        error: state.error_message(),
        state,
        progress: if size == 0 {
            0.0
        } else {
            downloaded as f64 / size as f64
        },
        downloaded,
        uploaded,
        left,
        size,
        download_rate,
        upload_rate,
        peers,
        seeds,
        save_path: pending.output_dir.clone(),
        torrent_path: PathBuf::new(),
        category: pending.category.clone(),
        tags: pending.tags.clone(),
        sequential: pending.sequential,
        added_at: pending.added_at,
        completed_at: None,
        piece_count,
        pieces_have,
        ratio: 0.0,
    }
}

fn tick_session(
    torrents: &DashMap<[u8; 20], Arc<TorrentSession>>,
    pending: &DashMap<[u8; 20], Arc<PendingTorrent>>,
    event_tx: &tokio::sync::broadcast::Sender<SessionEvent>,
) {
    for entry in torrents.iter() {
        let torrent = entry.value();
        let downloaded = torrent.downloaded_state.downloaded_bytes();
        let uploaded = torrent.uploaded.load(Ordering::Relaxed);
        torrent
            .rates
            .lock()
            .unwrap()
            .tick(downloaded, uploaded, RATE_TICK.as_secs_f64());
        if torrent.downloaded_state.is_complete() {
            let first = {
                let mut done = torrent.completed_at.lock().unwrap();
                if done.is_none() {
                    *done = Some(resume::now_unix());
                    true
                } else {
                    false
                }
            };
            if first {
                let _ = event_tx.send(SessionEvent::Completed {
                    id: TorrentId::new(*entry.key()),
                });
                let mut state = torrent.torrent_state.lock().unwrap();
                if *state == TorrentState::Downloading {
                    *state = TorrentState::Seeding;
                    let _ = event_tx.send(SessionEvent::StateChanged {
                        id: TorrentId::new(*entry.key()),
                        state: TorrentState::Seeding,
                    });
                }
            }
        }
        let state = torrent.torrent_state.lock().unwrap().clone();
        if matches!(
            state,
            TorrentState::Downloading
                | TorrentState::Seeding
                | TorrentState::Metadata
                | TorrentState::Checking
        ) {
            let snapshot = torrent_snapshot(torrent);
            let _ = event_tx.send(SessionEvent::Progress {
                id: snapshot.id,
                snapshot: Box::new(snapshot),
            });
        }
    }
    for entry in pending.iter() {
        if torrents.contains_key(entry.key()) {
            continue;
        }
        let pending = entry.value();
        let downloaded = pending.downloaded_state.get().downloaded_bytes();
        let uploaded = pending.uploaded.load(Ordering::Relaxed);
        pending
            .rates
            .lock()
            .unwrap()
            .tick(downloaded, uploaded, RATE_TICK.as_secs_f64());
        let state = pending.torrent_state.lock().unwrap().clone();
        if matches!(state, TorrentState::Metadata | TorrentState::Checking) {
            let snapshot = pending_snapshot(*entry.key(), pending);
            let _ = event_tx.send(SessionEvent::Progress {
                id: snapshot.id,
                snapshot: Box::new(snapshot),
            });
        }
    }
}

fn disconnect_all_peers(peer_states: &PeerStates) {
    for state in peer_states.states.iter() {
        if let Some(tx) = &state.writer_tx {
            let _ = tx.send(WriterRequest::Disconnect);
        }
    }
}

fn delete_torrent_files(torrent: &Torrent, output_dir: &std::path::Path) {
    for index in 0..torrent.files.len() {
        let path = crate::storage::file_path(torrent, output_dir, index);
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                debug!(path = %path.display(), error = %e, "failed to delete torrent file");
            }
        }
    }
}

fn choke_all_peers(peer_states: &PeerStates) {
    for mut state in peer_states.states.iter_mut() {
        if !state.stats.am_choking.load(Ordering::Relaxed) {
            state.set_am_choking(true);
            state.is_optimistic = false;
            if let Some(tx) = &state.writer_tx {
                let _ = tx.send(WriterRequest::Message(Message::Choke));
            }
            state.stats.upload_notify.notify_one();
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
    encryption: EncryptionPolicy,
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
            pending.torrent.get().allows_source(source)
        } else {
            return 0;
        };
        if !allowed {
            return 0;
        }
        let (peer_states, max_per, transferring) =
            if let Some(torrent) = self.torrents.get(info_hash) {
                (
                    torrent.peer_states.clone(),
                    self.max_peers_per_torrent,
                    *torrent.download_state.lock().unwrap() == DownloadState::Downloading,
                )
            } else if let Some(pending) = self.pending.get(info_hash) {
                (
                    pending.peer_states.clone(),
                    self.max_peers_per_torrent,
                    *pending.download_state.lock().unwrap() == DownloadState::Downloading,
                )
            } else {
                return 0;
            };
        if !transferring {
            return 0;
        }

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
            return try_spawn_peer(spawn_from_session(
                addr,
                *info_hash,
                self.peer_id,
                &torrent,
                None,
                self.extensions.clone(),
                self.listen_port(),
                advertise_dht,
                dht_port,
                dht,
                self.global_peers.clone(),
                self.max_peers_per_torrent,
                self.max_peers_global,
                self.connector.clone(),
                self.encryption,
            ));
        }
        let Some(pending) = self.pending.get(info_hash).map(|entry| entry.clone()) else {
            return false;
        };
        try_spawn_peer(spawn_from_pending(
            addr,
            *info_hash,
            self.peer_id,
            &pending,
            None,
            self.extensions.clone(),
            self.listen_port(),
            advertise_dht,
            dht_port,
            dht,
            self.global_peers.clone(),
            self.max_peers_per_torrent,
            self.max_peers_global,
            self.connector.clone(),
            self.encryption,
        ))
    }
}

#[derive(Clone)]
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
    pub encryption: EncryptionPolicy,
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
            encryption: self.options.encryption,
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
    accept_incoming_kind(stream, addr, ctx, false).await;
}

async fn accept_incoming_utp(stream: BoxedPeerStream, addr: SocketAddr, ctx: IncomingPeerContext) {
    accept_incoming_kind(stream, addr, ctx, true).await;
}

async fn accept_incoming_kind(
    stream: BoxedPeerStream,
    addr: SocketAddr,
    ctx: IncomingPeerContext,
    utp: bool,
) {
    let incoming = match peek_incoming(stream, addr).await {
        Ok(mut incoming) => {
            incoming.utp = utp;
            incoming
        }
        Err(e) => {
            debug!(%addr, error = %e, "incoming peek failed");
            return;
        }
    };
    accept_incoming_stream(incoming, ctx).await;
}

async fn peek_incoming(
    mut stream: BoxedPeerStream,
    addr: SocketAddr,
) -> std::io::Result<IncomingStream> {
    let mut prefix = vec![0u8; BT_HANDSHAKE_HEAD.len()];
    match tokio::time::timeout(
        crate::protocol::PeerTimeouts::default().handshake,
        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut prefix),
    )
    .await
    {
        Ok(Ok(_)) => Ok(IncomingStream::with_peek(prefix, stream, addr)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "incoming peek timed out",
        )),
    }
}

pub async fn accept_incoming_stream(incoming: IncomingStream, ctx: IncomingPeerContext) {
    let IncomingStream {
        mut stream,
        addr,
        kind,
        utp,
    } = incoming;
    debug!(%addr, ?kind, "accepting incoming peer");
    let mut encrypted = false;
    match kind {
        IncomingKind::Plaintext => {
            if !ctx.encryption.allows_plaintext() {
                debug!(%addr, "refusing plaintext peer under require_encrypted");
                return;
            }
        }
        IncomingKind::MaybeEncrypted => {
            if !ctx.encryption.allows_mse() {
                debug!(%addr, "refusing mse peer under encryption=disabled");
                return;
            }
            let policy = ctx.encryption;
            let torrents = ctx.torrents.clone();
            let pending = ctx.pending.clone();
            match mse::respond(
                stream,
                |req2| {
                    let keys: Vec<[u8; 20]> = torrents
                        .iter()
                        .map(|e| *e.key())
                        .chain(pending.iter().map(|e| *e.key()))
                        .collect();
                    mse::lookup_skey(req2, keys.iter())
                },
                |provided| policy.select(provided),
            )
            .await
            {
                Ok(outcome) => {
                    encrypted = outcome.selected.is_rc4();
                    stream = boxed_stream(outcome.stream);
                }
                Err(e) => {
                    debug!(%addr, error = %e, "incoming mse handshake failed");
                    return;
                }
            }
        }
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
        .map(|entry| IncomingTarget::from_session(&entry))
        .or_else(|| {
            ctx.pending
                .get(&handshake.info_hash)
                .map(|entry| IncomingTarget::from_pending(&entry))
        });
    let Some(target) = known else {
        debug!(%addr, "incoming peer for unknown info hash");
        return;
    };
    if target.peer_states.is_banned(addr) {
        debug!(%addr, "refusing banned incoming peer");
        return;
    }
    let advertise_dht = ctx.dht.is_some() && target.torrent.get().allows_dht();
    let mut reply = Handshake::outgoing(handshake.info_hash, ctx.peer_id);
    if advertise_dht {
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
        advertise_dht,
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
        incoming_encrypted: encrypted,
        incoming_utp: utp,
        encryption: ctx.encryption,
        global_peers: ctx.global_peers,
        max_peers_per_torrent: ctx.max_peers_per_torrent,
        max_peers_global: ctx.max_peers_global,
        connector: ctx.connector,
        promote_notify: target.promote_notify,
    });
}

struct IncomingTarget {
    piece_tx: Arc<Slot<flume::Sender<crate::peer_connection::FullPiece>>>,
    have_broadcast: Arc<tokio::sync::broadcast::Sender<u32>>,
    downloaded_state: Arc<Slot<Arc<TorrentDownloadedState>>>,
    peer_states: Arc<PeerStates>,
    download_state: Arc<Mutex<DownloadState>>,
    storage: Arc<Slot<Arc<Storage>>>,
    uploaded: Arc<AtomicU64>,
    torrent: Arc<Slot<Arc<Torrent>>>,
    choke_notify: Arc<Notify>,
    metadata: Arc<crate::extension::MetadataStore>,
    promote_notify: Arc<Notify>,
}

impl IncomingTarget {
    fn from_session(entry: &TorrentSession) -> Self {
        Self {
            piece_tx: Slot::new(entry.piece_tx.clone()),
            have_broadcast: entry.have_broadcast.clone(),
            downloaded_state: Slot::new(entry.downloaded_state.clone()),
            peer_states: entry.peer_states.clone(),
            download_state: entry.download_state.clone(),
            storage: Slot::new(entry.storage.clone()),
            uploaded: entry.uploaded.clone(),
            torrent: Slot::new(entry.torrent.clone()),
            choke_notify: entry.choke_notify.clone(),
            metadata: entry.metadata.clone(),
            promote_notify: Arc::new(Notify::new()),
        }
    }

    fn from_pending(entry: &PendingTorrent) -> Self {
        Self {
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
            promote_notify: entry.promote_notify.clone(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_from_session(
    peer: SocketAddr,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    torrent: &TorrentSession,
    incoming: Option<crate::transport::BoxedPeerStream>,
    extensions: crate::extension::ExtensionRegistry,
    listen_port: u16,
    advertise_dht: bool,
    dht_port: Option<u16>,
    dht: Option<DhtHandle>,
    global_peers: Arc<AtomicUsize>,
    max_peers_per_torrent: usize,
    max_peers_global: usize,
    connector: Arc<dyn crate::transport::Connector>,
    encryption: EncryptionPolicy,
) -> SpawnPeerParams {
    SpawnPeerParams {
        peer,
        info_hash,
        peer_id,
        piece_tx: Slot::new(torrent.piece_tx.clone()),
        have_broadcast: torrent.have_broadcast.clone(),
        torrent_downloaded_state: Slot::new(torrent.downloaded_state.clone()),
        peer_states: torrent.peer_states.clone(),
        download_state: torrent.download_state.clone(),
        storage: Slot::new(torrent.storage.clone()),
        uploaded: torrent.uploaded.clone(),
        torrent: Slot::new(torrent.torrent.clone()),
        choke_notify: torrent.choke_notify.clone(),
        incoming,
        incoming_fast_extension: None,
        incoming_extension_protocol: None,
        incoming_dht: None,
        incoming_encrypted: false,
        incoming_utp: false,
        encryption,
        extensions,
        listen_port,
        metadata: torrent.metadata.clone(),
        advertise_dht: advertise_dht && torrent.torrent.allows_dht(),
        dht_port,
        dht,
        global_peers,
        max_peers_per_torrent,
        max_peers_global,
        connector,
        promote_notify: Arc::new(Notify::new()),
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_from_pending(
    peer: SocketAddr,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    pending: &PendingTorrent,
    incoming: Option<crate::transport::BoxedPeerStream>,
    extensions: crate::extension::ExtensionRegistry,
    listen_port: u16,
    advertise_dht: bool,
    dht_port: Option<u16>,
    dht: Option<DhtHandle>,
    global_peers: Arc<AtomicUsize>,
    max_peers_per_torrent: usize,
    max_peers_global: usize,
    connector: Arc<dyn crate::transport::Connector>,
    encryption: EncryptionPolicy,
) -> SpawnPeerParams {
    SpawnPeerParams {
        peer,
        info_hash,
        peer_id,
        piece_tx: pending.piece_tx.clone(),
        have_broadcast: pending.have_broadcast.clone(),
        torrent_downloaded_state: pending.downloaded_state.clone(),
        peer_states: pending.peer_states.clone(),
        download_state: pending.download_state.clone(),
        storage: pending.storage.clone(),
        uploaded: pending.uploaded.clone(),
        torrent: pending.torrent.clone(),
        choke_notify: pending.choke_notify.clone(),
        incoming,
        incoming_fast_extension: None,
        incoming_extension_protocol: None,
        incoming_dht: None,
        incoming_encrypted: false,
        incoming_utp: false,
        encryption,
        extensions,
        listen_port,
        metadata: pending.metadata.clone(),
        advertise_dht: advertise_dht && pending.torrent.get().allows_dht(),
        dht_port,
        dht,
        global_peers,
        max_peers_per_torrent,
        max_peers_global,
        connector,
        promote_notify: pending.promote_notify.clone(),
    }
}

fn spawn_choke_loop(
    peer_states: Arc<PeerStates>,
    downloaded_state: Arc<TorrentDownloadedState>,
    download_state: Arc<Mutex<DownloadState>>,
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
            if *download_state.lock().unwrap() != DownloadState::Downloading {
                choke_all_peers(&peer_states);
                continue;
            }
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
                state.stats.upload_notify.notify_one();
            }
            ChokeAction::Unchoke => {
                state.set_am_choking(false);
                state.last_unchoked = Some(now);
                state.is_optimistic = !regular.contains(&addr);
                if let Some(tx) = &state.writer_tx {
                    let _ = tx.send(WriterRequest::Message(Message::Unchoke));
                }
                state.stats.upload_notify.notify_one();
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.owns_lifecycle {
            self.shutdown();
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod add_options_tests {
    use super::{AddTorrentOptions, TorrentId};

    #[test]
    fn from_path_is_fallible() {
        let err = match AddTorrentOptions::from_path("/does/not/exist.torrent") {
            Ok(_) => panic!("expected a parse error"),
            Err(err) => err,
        };
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn try_from_dispatches_magnet_prefix() {
        let hex = "123456789abcdef0112233445566778899aabbcc";
        let opts = AddTorrentOptions::try_from(format!("magnet:?xt=urn:btih:{hex}").as_str())
            .expect("parse magnet");
        assert!(opts.is_magnet());
        assert_eq!(opts.name(), hex);
    }

    #[test]
    fn torrent_id_displays_lowercase_hex() {
        let id = TorrentId::new([
            0x0f, 0xab, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
            0xcc, 0xdd, 0xee, 0xff, 0x01, 0x02,
        ]);
        assert_eq!(id.to_string(), "0fab00112233445566778899aabbccddeeff0102");
        assert_eq!(
            format!("{id:?}"),
            "TorrentId(0fab00112233445566778899aabbccddeeff0102)"
        );
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
        utils::sha1_digest(data)
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
        assert!(
            !torrent.peer_states.states.get(&addr).unwrap().encrypted,
            "plaintext incoming is not marked encrypted"
        );

        drop(remote_task);
        session.shutdown();
    }

    #[tokio::test]
    async fn require_encrypted_refuses_plaintext_incoming() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::with_options(SessionOptions {
            listen_port: 0,
            state_dir: None,
            encryption: EncryptionPolicy::RequireEncrypted,
            ..SessionOptions::default()
        });
        let _ = tokio::time::timeout(Duration::from_secs(2), session.wait_listening()).await;

        let meta = tiny_meta();
        let path = dir.path().join("tiny.bin");
        session
            .add_torrent(AddTorrentOptions::from(meta.clone()).output_dir(path))
            .await
            .expect("add torrent");

        let addr: SocketAddr = "127.0.0.1:51414".parse().unwrap();
        let (mut remote, server) = tokio::io::duplex(256);
        let handshake = Handshake::outgoing(meta.info_hash, *b"-LC0001-0123456789ab");
        let remote_task = tokio::spawn(async move {
            let _ = remote.write_all(&handshake.serialize()).await;
            let mut reply = [0u8; 68];
            let _ = remote.read_exact(&mut reply).await;
            remote
        });

        session.accept_incoming(boxed_stream(server), addr).await;

        let torrent = session
            .torrent_session(&meta.info_hash)
            .expect("torrent registered");
        assert!(
            !torrent.peer_states.states.contains_key(&addr),
            "require_encrypted must refuse a plaintext handshake"
        );

        drop(remote_task);
        session.shutdown();
    }

    #[tokio::test]
    async fn utp_enabled_binds_a_dedicated_udp_socket() {
        let session = Session::with_options(SessionOptions {
            listen_port: 0,
            state_dir: None,
            utp: crate::utp::UtpOptions {
                enabled: true,
                port: 0,
            },
            ..SessionOptions::default()
        });
        let _ = tokio::time::timeout(Duration::from_secs(2), session.wait_listening()).await;
        let tcp = session.wait_listening().await;
        let utp = session.utp_local_addr().expect("uTP bound");
        assert_eq!(
            utp.port(),
            tcp.port(),
            "uTP uses the TCP listen port number"
        );
        session.shutdown();
    }

    #[tokio::test]
    async fn utp_disabled_does_not_bind() {
        let session = Session::with_options(SessionOptions {
            listen_port: 0,
            state_dir: None,
            utp: crate::utp::UtpOptions {
                enabled: false,
                port: 0,
            },
            ..SessionOptions::default()
        });
        let _ = tokio::time::timeout(Duration::from_secs(2), session.wait_listening()).await;
        assert!(session.utp_local_addr().is_none());
        session.shutdown();
    }
}
