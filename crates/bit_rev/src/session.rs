use std::sync::Arc;

use crate::file::{self, TorrentMeta};
use crate::peer_state::PeerStates;
use crate::torrent::Torrent;
use crate::tracker_peers::TrackerPeers;
use crate::utils;
use dashmap::DashMap;
use flume::Receiver;

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
    pub buf: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct State {
    pub requested: u32,
    pub downloaded: u32,
    pub buf: Vec<u8>,
}

pub struct Session {
    pub streams: DashMap<[u8; 20], TrackerPeers>,
}

pub struct AddTorrentOptions {
    torrent_meta: TorrentMeta,
}

impl AddTorrentOptions {
    fn from_meta(torrent_meta: TorrentMeta) -> Self {
        Self { torrent_meta }
    }

    fn from_path(path: &str) -> Self {
        let torrent_meta = file::from_filename(path).unwrap();
        Self { torrent_meta }
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
        Self {
            streams: DashMap::new(),
        }
    }

    pub async fn add_torrent(
        &self,
        add_torrent: AddTorrentOptions,
    ) -> anyhow::Result<AddTorrentResult> {
        let torrent = Torrent::new(&add_torrent.torrent_meta.clone())?;
        let torrent_meta = add_torrent.torrent_meta.clone();
        let (pr_tx, pr_rx) = flume::bounded::<PieceResult>(torrent.piece_hashes.len());
        let have_broadcast = Arc::new(tokio::sync::broadcast::channel(128).0);
        let peer_states = Arc::new(PeerStates::default());
        let random_peers = utils::generate_peer_id();

        let tracker_stream = TrackerPeers::new(
            torrent_meta.clone(),
            15,
            random_peers,
            peer_states,
            have_broadcast.clone(),
            pr_rx.clone(),
        );

        let pieces_of_work = (0..(torrent.piece_hashes.len()) as u64)
            .map(|index| {
                let length = utils::calculate_piece_size(&torrent, index as usize);
                PieceWork {
                    index: index as u32,
                    length: length as u32,
                    hash: torrent.piece_hashes[index as usize],
                }
            })
            .collect::<Vec<PieceWork>>();

        tracker_stream.connect(pieces_of_work).await;

        let have_broadcast = have_broadcast.clone();
        let piece_rx = tracker_stream.piece_rx.clone();

        tokio::spawn(async move {
            loop {
                let pr_tx = pr_tx.clone();
                let piece_rx = piece_rx.clone();
                let piece = piece_rx.recv_async().await.unwrap();
                have_broadcast.send(piece.index).unwrap();

                let pr = PieceResult {
                    index: piece.index,
                    length: piece.length,
                    buf: piece.buf,
                };
                pr_tx.send_async(pr).await.unwrap();
            }
        });

        self.streams
            .insert(torrent.info_hash, tracker_stream.clone());

        Ok(AddTorrentResult {
            torrent,
            torrent_meta,
            pr_rx,
        })
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
