use std::sync::{
    atomic::{AtomicU64, AtomicUsize},
    Arc, Mutex,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::{
    discovery::{DiscoverySource, SourceDenied, SourceRegistry},
    file::TorrentMeta,
    identity::TrackerIdentity,
    peer::BencodeResponse,
    peer_connection::{try_spawn_peer, FullPiece, SpawnPeerParams, TorrentDownloadedState},
    peer_state::PeerStates,
    session::{DownloadState, PieceResult},
    storage::Storage,
    torrent::Torrent,
    tracker::{self, HttpAnnounceContext, TrackerError},
};

pub struct PeerSpawnRuntime {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub storage: Arc<Storage>,
    pub downloaded_state: Arc<TorrentDownloadedState>,
    pub uploaded: Arc<AtomicU64>,
    pub torrent: Arc<Torrent>,
    pub choke_notify: Arc<Notify>,
    pub global_peers: Arc<AtomicUsize>,
    pub max_peers_per_torrent: usize,
    pub max_peers_global: usize,
    pub listen_port: u16,
}

#[derive(Debug, Clone)]
pub struct TrackerPeers {
    torrent_meta: TorrentMeta,
    peer_id: [u8; 20],
    pub peer_states: Arc<PeerStates>,
    pub piece_tx: flume::Sender<FullPiece>,
    pub piece_rx: flume::Receiver<FullPiece>,
    pub pr_rx: flume::Receiver<PieceResult>,
    pub have_broadcast: Arc<tokio::sync::broadcast::Sender<u32>>,
    pub download_state: Arc<Mutex<DownloadState>>,
    sources: Arc<Mutex<SourceRegistry>>,
    cancel: CancellationToken,
}

impl TrackerPeers {
    pub fn new(
        torrent_meta: TorrentMeta,
        _max_size: usize,
        peer_id: [u8; 20],
        peer_states: Arc<PeerStates>,
        have_broadcast: Arc<tokio::sync::broadcast::Sender<u32>>,
        pr_rx: flume::Receiver<PieceResult>,
        download_state: Arc<Mutex<DownloadState>>,
    ) -> TrackerPeers {
        let (sender, receiver) = flume::unbounded();
        let sources = SourceRegistry::new(torrent_meta.torrent_file.info.is_private());
        TrackerPeers {
            torrent_meta,
            peer_id,
            piece_tx: sender,
            piece_rx: receiver,
            pr_rx,
            peer_states,
            have_broadcast,
            download_state,
            sources: Arc::new(Mutex::new(sources)),
            cancel: CancellationToken::new(),
        }
    }

    pub fn register_source(&self, source: DiscoverySource) -> Result<(), SourceDenied> {
        self.sources.lock().unwrap().register(source)
    }

    pub fn allows_source(&self, source: DiscoverySource) -> bool {
        self.sources.lock().unwrap().allows(source)
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn set_download_state(&self, state: DownloadState) {
        let mut current_state = self.download_state.lock().unwrap();
        *current_state = state;
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

    pub async fn connect(&self, runtime: PeerSpawnRuntime) {
        let all_tracker_urls = all_trackers(&self.torrent_meta);
        debug!(trackers = ?all_tracker_urls, "connecting to trackers");
        let torrent_meta = self.torrent_meta.clone();
        let peer_states = self.peer_states.clone();
        let piece_tx = self.piece_tx.clone();
        let have_broadcast = self.have_broadcast.clone();
        let download_state = self.download_state.clone();
        let cancel = self.cancel.clone();
        let http_client = tracker::http_client();

        if let Err(denied) = self.register_source(DiscoverySource::Tracker) {
            debug!(error = %denied, "refused tracker source");
            return;
        }

        for tracker_url in all_tracker_urls {
            let identity = TrackerIdentity::new(tracker_url);
            let ctx = HttpAnnounceContext {
                client: http_client.clone(),
                torrent_meta: torrent_meta.clone(),
                peer_id: self.peer_id,
                tracker_url: identity.url().to_string(),
                announce_key: identity.key(),
                port: runtime.listen_port,
                download_state: download_state.clone(),
                torrent_downloaded_state: runtime.downloaded_state.clone(),
                uploaded: runtime.uploaded.clone(),
            };
            let peer_states = peer_states.clone();
            let piece_tx = piece_tx.clone();
            let have_broadcast = have_broadcast.clone();
            let download_state = download_state.clone();
            let shutdown = cancel.clone();
            let runtime = PeerSpawnRuntime {
                info_hash: runtime.info_hash,
                peer_id: runtime.peer_id,
                storage: runtime.storage.clone(),
                downloaded_state: runtime.downloaded_state.clone(),
                uploaded: runtime.uploaded.clone(),
                torrent: runtime.torrent.clone(),
                choke_notify: runtime.choke_notify.clone(),
                global_peers: runtime.global_peers.clone(),
                max_peers_per_torrent: runtime.max_peers_per_torrent,
                max_peers_global: runtime.max_peers_global,
                listen_port: runtime.listen_port,
            };
            tokio::spawn(async move {
                tracker::run_announce_loop(ctx, shutdown, |new_peers| {
                    let peer_states = peer_states.clone();
                    let piece_tx = piece_tx.clone();
                    let have_broadcast = have_broadcast.clone();
                    let download_state = download_state.clone();
                    let runtime = PeerSpawnRuntime {
                        info_hash: runtime.info_hash,
                        peer_id: runtime.peer_id,
                        storage: runtime.storage.clone(),
                        downloaded_state: runtime.downloaded_state.clone(),
                        uploaded: runtime.uploaded.clone(),
                        torrent: runtime.torrent.clone(),
                        choke_notify: runtime.choke_notify.clone(),
                        global_peers: runtime.global_peers.clone(),
                        max_peers_per_torrent: runtime.max_peers_per_torrent,
                        max_peers_global: runtime.max_peers_global,
                        listen_port: runtime.listen_port,
                    };
                    async move {
                        process_peers(
                            new_peers,
                            peer_states,
                            piece_tx,
                            have_broadcast,
                            download_state,
                            runtime,
                        )
                        .await;
                    }
                })
                .await;
            });
        }
    }
}

async fn process_peers(
    new_peers: Vec<std::net::SocketAddr>,
    peer_states: Arc<PeerStates>,
    piece_tx: flume::Sender<FullPiece>,
    have_broadcast: Arc<tokio::sync::broadcast::Sender<u32>>,
    download_state: Arc<Mutex<DownloadState>>,
    runtime: PeerSpawnRuntime,
) {
    for peer in new_peers {
        let current_state = *download_state.lock().unwrap();
        if current_state != DownloadState::Downloading {
            continue;
        }
        if peer_states.states.contains_key(&peer) {
            continue;
        }

        try_spawn_peer(SpawnPeerParams {
            peer,
            info_hash: runtime.info_hash,
            peer_id: runtime.peer_id,
            piece_tx: piece_tx.clone(),
            have_broadcast: have_broadcast.clone(),
            torrent_downloaded_state: runtime.downloaded_state.clone(),
            peer_states: peer_states.clone(),
            download_state: download_state.clone(),
            storage: runtime.storage.clone(),
            uploaded: runtime.uploaded.clone(),
            torrent: runtime.torrent.clone(),
            choke_notify: runtime.choke_notify.clone(),
            incoming: None,
            incoming_fast_extension: None,
            global_peers: runtime.global_peers.clone(),
            max_peers_per_torrent: runtime.max_peers_per_torrent,
            max_peers_global: runtime.max_peers_global,
        });
    }
}

fn all_trackers(torrent_meta: &TorrentMeta) -> Vec<String> {
    match (
        &torrent_meta.torrent_file.announce,
        &torrent_meta.torrent_file.announce_list,
    ) {
        (Some(announce), None) => vec![announce.clone()],
        (Some(announce), Some(announce_list)) => {
            let mut h = Vec::<String>::from_iter(announce_list.iter().flatten().cloned());
            if !h.contains(announce) {
                h.push(announce.clone());
            }
            h.into_iter().collect()
        }
        (None, Some(announce_list)) => announce_list.clone().into_iter().flatten().collect(),
        (None, None) => vec![],
    }
}

pub async fn request_peers(uri: &str) -> Result<BencodeResponse, TrackerError> {
    tracker::announce(&tracker::http_client(), uri).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::from_filename;

    fn fixture_meta(name: &str) -> TorrentMeta {
        from_filename(&format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("parse fixture")
    }

    fn tracker_peers(meta: TorrentMeta) -> TrackerPeers {
        let (_, pr_rx) = flume::unbounded();
        TrackerPeers::new(
            meta,
            15,
            *b"-BR0100-0123456789ab",
            Arc::new(PeerStates::default()),
            Arc::new(tokio::sync::broadcast::channel(8).0),
            pr_rx,
            Arc::new(Mutex::new(DownloadState::Init)),
        )
    }

    #[test]
    fn private_torrent_refuses_non_tracker_source() {
        let peers = tracker_peers(fixture_meta("private.torrent"));
        assert!(peers.register_source(DiscoverySource::Tracker).is_ok());
        assert!(peers.register_source(DiscoverySource::Dht).is_err());
        assert!(peers.register_source(DiscoverySource::Pex).is_err());
        assert!(peers.register_source(DiscoverySource::Lsd).is_err());
        assert!(!peers.allows_source(DiscoverySource::Dht));
    }

    #[test]
    fn public_torrent_accepts_dht() {
        let peers = tracker_peers(fixture_meta("private-zero.torrent"));
        assert!(peers.register_source(DiscoverySource::Dht).is_ok());
        assert!(peers.allows_source(DiscoverySource::Dht));
    }
}
