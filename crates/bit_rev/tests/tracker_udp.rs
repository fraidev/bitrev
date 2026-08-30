use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use bit_rev::{
    file::{Info, TorrentFile, TorrentMeta},
    peer_connection::{PieceWorkState, TorrentDownloadedState},
    peer_state::PeerStates,
    session::{DownloadState, PieceWork},
    tracker::{self, run_announce_loop, HttpAnnounceContext},
};
use serde_bytes::ByteBuf;
use tokio::{
    net::UdpSocket,
    sync::{mpsc, Semaphore},
};
use tokio_util::sync::CancellationToken;

const INFO_HASH: [u8; 20] = [1u8; 20];
const PEER_ID: [u8; 20] = *b"-BR0100-0123456789ab";
const ANNOUNCE_KEY: u32 = 0xDF45_C574;
const PROTOCOL_ID: u64 = 0x0417_2710_1980;
const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;

fn test_meta(announce: String) -> TorrentMeta {
    TorrentMeta {
        torrent_file: TorrentFile {
            info: Info {
                name: "test".into(),
                pieces: ByteBuf::from(vec![0u8; 20]),
                piece_length: 16384,
                md5sum: None,
                length: Some(16384),
                files: None,
                private: None,
                path: None,
                root_hash: None,
            },
            announce: Some(announce),
            nodes: None,
            encoding: None,
            httpseeds: None,
            announce_list: None,
            creation_date: None,
            comment: None,
            created_by: None,
        },
        info_hash: INFO_HASH,
        piece_hashes: vec![INFO_HASH],
    }
}

fn incomplete_download() -> Arc<TorrentDownloadedState> {
    Arc::new(TorrentDownloadedState {
        semaphore: Semaphore::new(1),
        pieces: vec![PieceWorkState {
            piece_work: PieceWork {
                index: 0,
                length: 16384,
                hash: INFO_HASH,
            },
            chuncks: Mutex::new(vec![]),
            downloaded: AtomicBool::new(false),
            reserved: Mutex::new(None),
        }],
    })
}

fn announce_ctx(url: String, download: Arc<TorrentDownloadedState>) -> HttpAnnounceContext {
    HttpAnnounceContext {
        client: tracker::http_client(),
        torrent_meta: test_meta(url.clone()),
        peer_id: PEER_ID,
        tracker_url: url,
        announce_key: ANNOUNCE_KEY,
        port: 6881,
        download_state: Arc::new(Mutex::new(DownloadState::Downloading)),
        torrent_downloaded_state: download,
    }
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn connect_response(transaction_id: u32, connection_id: u64) -> [u8; 16] {
    let mut data = [0u8; 16];
    data[0..4].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    data[4..8].copy_from_slice(&transaction_id.to_be_bytes());
    data[8..16].copy_from_slice(&connection_id.to_be_bytes());
    data
}

fn announce_response(transaction_id: u32, interval: u32, peer: SocketAddr) -> Vec<u8> {
    let SocketAddr::V4(peer) = peer else {
        panic!("test helper expects IPv4 peer");
    };
    let mut data = Vec::with_capacity(26);
    data.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    data.extend_from_slice(&transaction_id.to_be_bytes());
    data.extend_from_slice(&interval.to_be_bytes());
    data.extend_from_slice(&1u32.to_be_bytes());
    data.extend_from_slice(&1u32.to_be_bytes());
    data.extend_from_slice(&peer.ip().octets());
    data.extend_from_slice(&peer.port().to_be_bytes());
    data
}

fn error_response(transaction_id: u32, message: &str) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&ACTION_ERROR.to_be_bytes());
    data.extend_from_slice(&transaction_id.to_be_bytes());
    data.extend_from_slice(message.as_bytes());
    data
}

async fn recv(socket: &UdpSocket) -> (Vec<u8>, SocketAddr) {
    let mut buf = vec![0u8; 512];
    let (len, from) = socket.recv_from(&mut buf).await.expect("mock recv");
    (buf[..len].to_vec(), from)
}

fn assert_connect(data: &[u8]) -> u32 {
    assert_eq!(data.len(), 16);
    assert_eq!(&data[0..8], &PROTOCOL_ID.to_be_bytes());
    assert_eq!(read_u32(data, 8), ACTION_CONNECT);
    read_u32(data, 12)
}

struct RecordedAnnounce {
    event: u32,
    downloaded: u64,
    left: u64,
    uploaded: u64,
    key: u32,
    num_want: i32,
    transaction_id: u32,
    connection_id: u64,
}

fn parse_announce(data: &[u8]) -> RecordedAnnounce {
    assert_eq!(data.len(), 98);
    assert_eq!(read_u32(data, 8), ACTION_ANNOUNCE);
    assert_eq!(&data[16..36], &INFO_HASH);
    assert_eq!(&data[36..56], &PEER_ID);
    RecordedAnnounce {
        connection_id: read_u64(data, 0),
        transaction_id: read_u32(data, 12),
        downloaded: u64::from_be_bytes(data[56..64].try_into().unwrap()),
        left: u64::from_be_bytes(data[64..72].try_into().unwrap()),
        uploaded: u64::from_be_bytes(data[72..80].try_into().unwrap()),
        event: read_u32(data, 80),
        key: read_u32(data, 88),
        num_want: i32::from_be_bytes(data[92..96].try_into().unwrap()),
    }
}

#[derive(Clone, Copy)]
enum AnnounceReply {
    Peers { interval: u32 },
    Error,
}

async fn spawn_udp_mock(
    replies: Vec<AnnounceReply>,
) -> (
    String,
    mpsc::UnboundedReceiver<RecordedAnnounce>,
    tokio::task::JoinHandle<()>,
) {
    let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let addr = socket.local_addr().unwrap();
    let url = format!("udp://{addr}/announce");
    let (tx, rx) = mpsc::unbounded_channel();
    let peer = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 51413));
    let idx = Arc::new(AtomicUsize::new(0));

    let handle = tokio::spawn(async move {
        let mut connection_id = 1u64;
        loop {
            let (packet, from) = recv(&socket).await;
            if packet.len() == 16 {
                let tid = assert_connect(&packet);
                connection_id += 1;
                if socket
                    .send_to(&connect_response(tid, connection_id), from)
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }
            if packet.len() != 98 {
                continue;
            }
            let announce = parse_announce(&packet);
            let i = idx.fetch_add(1, Ordering::SeqCst);
            let reply = replies
                .get(i)
                .copied()
                .unwrap_or(AnnounceReply::Peers { interval: 1800 });
            let payload = match reply {
                AnnounceReply::Peers { interval } => {
                    announce_response(announce.transaction_id, interval, peer)
                }
                AnnounceReply::Error => error_response(announce.transaction_id, "try again later"),
            };
            if socket.send_to(&payload, from).await.is_err() {
                break;
            }
            if tx.send(announce).is_err() {
                break;
            }
        }
    });

    (url, rx, handle)
}

async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn announce_loop_round_trips_started_and_interval() {
    let (url, mut rx, handle) = spawn_udp_mock(vec![AnnounceReply::Peers { interval: 1800 }]).await;
    let peer_states = Arc::new(PeerStates::default());
    let shutdown = CancellationToken::new();
    let loop_shutdown = shutdown.clone();
    let ctx = announce_ctx(url, incomplete_download());
    let peers_for_loop = peer_states.clone();

    let loop_task = tokio::spawn(async move {
        run_announce_loop(ctx, loop_shutdown, |peers| {
            let peer_states = peers_for_loop.clone();
            async move {
                for peer in peers {
                    peer_states.add_if_not_seen(peer);
                }
            }
        })
        .await;
    });

    let announce = rx.recv().await.expect("started announce");
    assert_eq!(announce.event, 2);
    assert_eq!(announce.downloaded, 0);
    assert_eq!(announce.left, 16384);
    assert_eq!(announce.uploaded, 0);
    assert_eq!(announce.key, ANNOUNCE_KEY);
    assert_eq!(announce.num_want, 50);

    settle().await;
    assert_eq!(peer_states.states.len(), 1);

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_task).await;
    handle.abort();
}

#[tokio::test]
async fn announce_loop_respects_interval_and_sends_stopped() {
    tokio::time::pause();
    let (url, mut rx, handle) = spawn_udp_mock(vec![AnnounceReply::Peers { interval: 1800 }]).await;
    let shutdown = CancellationToken::new();
    let loop_shutdown = shutdown.clone();
    let ctx = announce_ctx(url, incomplete_download());

    let loop_task = tokio::spawn(async move {
        run_announce_loop(ctx, loop_shutdown, |_| async {}).await;
    });

    let first = rx.recv().await.expect("started announce");
    assert_eq!(first.event, 2);
    settle().await;

    tokio::time::advance(Duration::from_secs(1799)).await;
    settle().await;
    assert!(rx.try_recv().is_err(), "re-announce sent before interval");

    tokio::time::advance(Duration::from_secs(2)).await;
    let second = rx.recv().await.expect("interval re-announce");
    assert_eq!(second.event, 0);
    assert_eq!(second.key, ANNOUNCE_KEY);

    shutdown.cancel();
    settle().await;
    let stopped = rx.recv().await.expect("stopped announce");
    assert_eq!(stopped.event, 3);
    assert_eq!(stopped.num_want, 0);
    assert_eq!(stopped.key, ANNOUNCE_KEY);

    let _ = tokio::time::timeout(Duration::from_secs(2), loop_task).await;
    handle.abort();
}

#[tokio::test]
async fn announce_loop_retries_error_packet_with_backoff() {
    tokio::time::pause();
    let (url, mut rx, handle) = spawn_udp_mock(vec![
        AnnounceReply::Error,
        AnnounceReply::Peers { interval: 1800 },
    ])
    .await;
    let shutdown = CancellationToken::new();
    let loop_shutdown = shutdown.clone();
    let ctx = announce_ctx(url, incomplete_download());

    let loop_task = tokio::spawn(async move {
        run_announce_loop(ctx, loop_shutdown, |_| async {}).await;
    });

    let first = rx.recv().await.expect("failed started announce");
    assert_eq!(first.event, 2);
    settle().await;

    tokio::time::advance(Duration::from_secs(14)).await;
    settle().await;
    assert!(rx.try_recv().is_err(), "retried before backoff elapsed");

    tokio::time::advance(Duration::from_secs(2)).await;
    let retry = rx.recv().await.expect("backoff retry");
    assert_eq!(retry.event, 2);
    assert_eq!(retry.key, ANNOUNCE_KEY);

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_task).await;
    handle.abort();
}

#[tokio::test]
async fn announce_loop_reuses_connection_id_until_expiry() {
    let (url, mut rx, handle) = spawn_udp_mock(vec![
        AnnounceReply::Peers { interval: 1800 },
        AnnounceReply::Peers { interval: 1800 },
    ])
    .await;
    let shutdown = CancellationToken::new();
    let loop_shutdown = shutdown.clone();
    let ctx = announce_ctx(url, incomplete_download());

    let loop_task = tokio::spawn(async move {
        run_announce_loop(ctx, loop_shutdown, |_| async {}).await;
    });

    let first = rx.recv().await.expect("started announce");
    let first_cid = first.connection_id;

    shutdown.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("stopped announce timed out")
        .expect("stopped announce");
    assert_eq!(stopped.event, 3);
    assert_eq!(stopped.connection_id, first_cid);

    let _ = tokio::time::timeout(Duration::from_secs(2), loop_task).await;
    handle.abort();
}
