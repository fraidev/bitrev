use serde_bencode::de;
use std::sync::{atomic::AtomicBool, Arc, Mutex};
use tokio::{select, sync::Semaphore};
use tracing::{debug, error};

use crate::{
    file::{self, TorrentMeta},
    peer::BencodeResponse,
    peer_connection::{
        FullPiece, PeerConnection, PeerHandler, PieceWorkState, TorrentDownloadedState,
    },
    peer_state::PeerStates,
    protocol_udp::request_udp_peers,
    session::PieceWork,
};

#[derive(Debug, Clone)]
pub struct TrackerPeers {
    torrent_meta: TorrentMeta,
    peer_id: [u8; 20],
    pub peer_states: Arc<PeerStates>,
    pub piece_tx: flume::Sender<FullPiece>,
    pub piece_rx: flume::Receiver<FullPiece>,
    pub have_broadcast: Arc<tokio::sync::broadcast::Sender<u32>>,
}

impl TrackerPeers {
    pub fn new(
        torrent_meta: TorrentMeta,
        _max_size: usize,
        peer_id: [u8; 20],
        peer_states: Arc<PeerStates>,
        have_broadcast: Arc<tokio::sync::broadcast::Sender<u32>>,
    ) -> TrackerPeers {
        let (sender, receiver) = flume::unbounded();
        TrackerPeers {
            torrent_meta,
            peer_id,
            piece_tx: sender,
            piece_rx: receiver,
            peer_states,
            have_broadcast,
        }
    }

    pub async fn connect(&self, pieces_of_work: Vec<PieceWork>) {
        let info_hash = self.torrent_meta.info_hash;
        let peer_id = self.peer_id;

        let all_tracker_urls = all_trackers(&self.torrent_meta.clone());
        let tcp_trackers: Vec<String> = all_tracker_urls
            .iter()
            .filter(|t| !t.starts_with("udp://"))
            .cloned()
            .collect();
        let udp_trackers: Vec<String> = all_tracker_urls
            .iter()
            .filter(|t| t.starts_with("udp://"))
            .cloned()
            .collect();

        debug!(
            "Connecting to trackers: TCP: {:?}, UDP: {:?}",
            tcp_trackers, udp_trackers
        );
        let torrent_meta = self.torrent_meta.clone();
        let peer_states = self.peer_states.clone();
        let piece_tx = self.piece_tx.clone();
        let have_broadcast = self.have_broadcast.clone();
        let torrent_downloaded_state = Arc::new(TorrentDownloadedState {
            semaphore: Semaphore::new(1),
            pieces: pieces_of_work
                .into_iter()
                .map(|pw| PieceWorkState {
                    piece_work: pw,
                    chuncks: Mutex::new(vec![]),
                    downloaded: AtomicBool::new(false),
                    reserved: Mutex::new(None),
                })
                .collect(),
        });
        tokio::spawn(async move {
            loop {
                // Handle TCP trackers
                for tracker in tcp_trackers.clone() {
                    let torrent_meta = torrent_meta.clone();
                    let peer_states = peer_states.clone();
                    let piece_tx = piece_tx.clone();
                    let have_broadcast = have_broadcast.clone();
                    let torrent_downloaded_state = torrent_downloaded_state.clone();
                    tokio::spawn(async move {
                        let url = file::build_tracker_url(&torrent_meta, &peer_id, 6881, &tracker);

                        match request_peers(&url).await {
                            Ok(request_peers_res) => {
                                match request_peers_res.clone().get_peers() {
                                    Ok(new_peers) => {
                                        process_peers(
                                            new_peers,
                                            info_hash,
                                            peer_id,
                                            peer_states.clone(),
                                            piece_tx.clone(),
                                            have_broadcast.clone(),
                                            torrent_downloaded_state.clone(),
                                        )
                                        .await;

                                        //sleep interval
                                        tokio::time::sleep(std::time::Duration::from_millis(
                                            request_peers_res.interval,
                                        ))
                                        .await;
                                    }
                                    Err(e) => debug!(
                                        "Failed to parse peers from TCP tracker {}: {}",
                                        tracker, e
                                    ),
                                }
                            }
                            Err(e) => debug!(
                                "Failed to request peers from TCP tracker {}: {}",
                                tracker, e
                            ),
                        }
                    });
                }

                // Handle UDP trackers
                for tracker in udp_trackers.clone() {
                    let torrent_meta = torrent_meta.clone();
                    let peer_states = peer_states.clone();
                    let piece_tx = piece_tx.clone();
                    let have_broadcast = have_broadcast.clone();
                    let torrent_downloaded_state = torrent_downloaded_state.clone();
                    tokio::spawn(async move {
                        match request_udp_peers(&tracker, &torrent_meta, &peer_id, 6881).await {
                            Ok(udp_response) => {
                                debug!(
                                    "Received UDP response from tracker {}: {:?}",
                                    tracker, udp_response
                                );
                                let new_peers: Vec<_> = udp_response
                                    .peers
                                    .into_iter()
                                    .map(|p| p.to_socket_addr())
                                    .collect();

                                process_peers(
                                    new_peers,
                                    info_hash,
                                    peer_id,
                                    peer_states.clone(),
                                    piece_tx.clone(),
                                    have_broadcast.clone(),
                                    torrent_downloaded_state.clone(),
                                )
                                .await;

                                //sleep interval
                                tokio::time::sleep(std::time::Duration::from_secs(
                                    udp_response.interval as u64,
                                ))
                                .await;
                            }
                            Err(e) => error!(
                                "Failed to request peers from UDP tracker {}: {}",
                                tracker, e
                            ),
                        }
                    });
                }

                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
    }
}

async fn process_peers(
    new_peers: Vec<std::net::SocketAddr>,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    peer_states: Arc<PeerStates>,
    piece_tx: flume::Sender<FullPiece>,
    have_broadcast: Arc<tokio::sync::broadcast::Sender<u32>>,
    torrent_downloaded_state: Arc<TorrentDownloadedState>,
) {
    for peer in new_peers {
        if peer_states.clone().states.contains_key(&peer) {
            continue;
        }

        let piece_tx = piece_tx.clone();
        let have_broadcast = have_broadcast.clone();
        let torrent_downloaded_state = torrent_downloaded_state.clone();
        let peer_states = peer_states.clone();

        tokio::spawn(async move {
            let unchoke_notify = tokio::sync::Notify::new();
            let (peer_writer_tx, peer_writer_rx) = flume::unbounded();

            let peer_handler = Arc::new(PeerHandler::new(
                peer,
                unchoke_notify,
                piece_tx.clone(),
                peer_writer_tx.clone(),
                peer_states.clone(),
                torrent_downloaded_state.clone(),
            ));

            let peer_connection =
                PeerConnection::new(peer, info_hash, peer_id, peer_handler.clone());

            let task_peer_chunk_req_fut = peer_handler.task_peer_chunk_requester();
            let connect_peer_fut =
                peer_connection.manage_peer_incoming(peer_writer_rx, have_broadcast.subscribe());

            let req = select! {
                r = connect_peer_fut => {
                    debug!("connect_peer_fut: {:#?}", r);
                    r
                }
                r = task_peer_chunk_req_fut => {
                    debug!("task_peer_chunk_req_fut: {:#?}", r);
                    r
                }
            };

            match req {
                Ok(_) => {
                    // We disconnected the peer ourselves as we don't need it
                    peer_handler.on_peer_died();
                }
                Err(e) => {
                    debug!("error managing peer: {:#}", e);
                    peer_handler.on_peer_died();
                }
            }
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

pub async fn request_peers(uri: &str) -> anyhow::Result<BencodeResponse> {
    let client = reqwest::Client::new();
    let response = client.get(uri).send().await?;
    let body_bytes = response.bytes().await?;

    let tracker_bencode_decode = de::from_bytes::<BencodeResponse>(&body_bytes)?;
    Ok(tracker_bencode_decode)
}
