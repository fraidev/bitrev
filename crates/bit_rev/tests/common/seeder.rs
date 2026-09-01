#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bit_rev::bitfield::Bitfield;
use bit_rev::handshake::Handshake;
use bit_rev::message::{self, BlockRequest, Message};
use bit_rev::protocol::Protocol;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::fixture::TorrentFixture;

#[derive(Clone, Debug)]
pub struct SeederConfig {
    pub pieces: Option<HashSet<u32>>,
    pub read_latency: Duration,
    pub write_latency: Duration,
    pub corrupt_pieces: HashSet<u32>,
    pub disconnect_after_blocks: Option<u64>,
    pub never_unchoke: bool,
    pub peer_id: [u8; 20],
}

impl Default for SeederConfig {
    fn default() -> Self {
        Self {
            pieces: None,
            read_latency: Duration::ZERO,
            write_latency: Duration::ZERO,
            corrupt_pieces: HashSet::new(),
            disconnect_after_blocks: None,
            never_unchoke: false,
            peer_id: *b"-SD0001-0123456789ab",
        }
    }
}

impl SeederConfig {
    pub fn all_pieces() -> Self {
        Self::default()
    }

    pub fn with_pieces(pieces: impl IntoIterator<Item = u32>) -> Self {
        Self {
            pieces: Some(pieces.into_iter().collect()),
            ..Self::default()
        }
    }

    pub fn peer_id(mut self, peer_id: [u8; 20]) -> Self {
        self.peer_id = peer_id;
        self
    }

    pub fn latency(mut self, latency: Duration) -> Self {
        self.read_latency = latency;
        self.write_latency = latency;
        self
    }

    pub fn corrupt(mut self, piece: u32) -> Self {
        self.corrupt_pieces.insert(piece);
        self
    }

    pub fn disconnect_after_blocks(mut self, n: u64) -> Self {
        self.disconnect_after_blocks = Some(n);
        self
    }

    pub fn never_unchoke(mut self) -> Self {
        self.never_unchoke = true;
        self
    }
}

pub struct SeederPeer {
    pub addr: std::net::SocketAddr,
    pub blocks_sent: Arc<AtomicU64>,
    block_notify: Arc<Notify>,
    cancel: CancellationToken,
}

impl Drop for SeederPeer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl SeederPeer {
    pub async fn start(fixture: Arc<TorrentFixture>, config: SeederConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind seeder");
        let addr = listener.local_addr().expect("seeder addr");
        let blocks_sent = Arc::new(AtomicU64::new(0));
        let block_notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let blocks_task = blocks_sent.clone();
        let notify_task = block_notify.clone();
        let cancel_task = cancel.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_task.cancelled() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let fixture = fixture.clone();
                        let config = config.clone();
                        let blocks_sent = blocks_task.clone();
                        let block_notify = notify_task.clone();
                        let cancel = cancel_task.clone();
                        tokio::spawn(async move {
                            let _ = serve_peer(
                                stream,
                                fixture,
                                config,
                                blocks_sent,
                                block_notify,
                                cancel,
                            )
                            .await;
                        });
                    }
                }
            }
        });

        Self {
            addr,
            blocks_sent,
            block_notify,
            cancel,
        }
    }

    pub fn blocks_sent(&self) -> u64 {
        self.blocks_sent.load(Ordering::Relaxed)
    }

    pub async fn wait_blocks_sent(&self, n: u64, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.blocks_sent() >= n {
                return;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                panic!(
                    "seeder sent {} blocks, wanted {n} before timeout",
                    self.blocks_sent()
                );
            }
            tokio::select! {
                _ = self.block_notify.notified() => {}
                _ = tokio::time::sleep(remaining) => {
                    panic!(
                        "seeder sent {} blocks, wanted {n} before timeout",
                        self.blocks_sent()
                    );
                }
            }
        }
    }
}

async fn write_msg(stream: &mut TcpStream, msg: Message) -> std::io::Result<()> {
    stream.write_all(&message::serialize(Some(msg))).await
}

fn advertised_bitfield(piece_count: usize, pieces: &Option<HashSet<u32>>) -> Bitfield {
    let mut bf = Bitfield::with_piece_count(piece_count);
    for i in 0..piece_count {
        let have = pieces
            .as_ref()
            .map(|set| set.contains(&(i as u32)))
            .unwrap_or(true);
        if have {
            bf.set_piece(i);
        }
    }
    bf
}

fn has_piece(config: &SeederConfig, index: u32) -> bool {
    config
        .pieces
        .as_ref()
        .map(|set| set.contains(&index))
        .unwrap_or(true)
}

async fn serve_peer(
    mut stream: TcpStream,
    fixture: Arc<TorrentFixture>,
    config: SeederConfig,
    blocks_sent: Arc<AtomicU64>,
    block_notify: Arc<Notify>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let _hs = Protocol::read_handshake(&mut stream).await?;
    let reply = Handshake::new(fixture.torrent_meta.info_hash, config.peer_id);
    Protocol::write_handshake(&mut stream, &reply).await?;

    let bf = advertised_bitfield(fixture.piece_count(), &config.pieces);
    write_msg(&mut stream, Message::Bitfield(bf.as_bytes().to_vec())).await?;
    if !config.never_unchoke {
        write_msg(&mut stream, Message::Unchoke).await?;
    }

    let proto = Protocol::connect(
        stream.peer_addr()?,
        fixture.torrent_meta.info_hash,
        config.peer_id,
    )
    .await?;

    loop {
        if cancel.is_cancelled() {
            break;
        }
        if config.read_latency > Duration::ZERO {
            tokio::time::sleep(config.read_latency).await;
        }
        let msg = tokio::select! {
            _ = cancel.cancelled() => break,
            msg = proto.read(&mut stream) => msg,
        };
        let msg = match msg {
            Ok(Some(msg)) => msg,
            Ok(None) => continue,
            Err(_) => break,
        };
        match msg {
            Message::Interested => {
                if !config.never_unchoke {
                    write_msg(&mut stream, Message::Unchoke).await?;
                }
            }
            Message::Request(payload) => {
                let Some(req) = BlockRequest::from_payload(&payload) else {
                    continue;
                };
                if !has_piece(&config, req.index) {
                    continue;
                }
                if config.write_latency > Duration::ZERO {
                    tokio::time::sleep(config.write_latency).await;
                }
                let mut data = fixture.read_block(req.index, req.begin, req.length);
                if config.corrupt_pieces.contains(&req.index) {
                    if let Some(byte) = data.first_mut() {
                        *byte ^= 0xFF;
                    }
                }
                write_msg(
                    &mut stream,
                    message::format_piece(req.index, req.begin, data),
                )
                .await?;
                let sent = blocks_sent.fetch_add(1, Ordering::Relaxed) + 1;
                block_notify.notify_waiters();
                if let Some(limit) = config.disconnect_after_blocks {
                    if sent >= limit {
                        break;
                    }
                }
            }
            Message::NotInterested
            | Message::Bitfield(_)
            | Message::Have(_)
            | Message::KeepAlive
            | Message::Choke
            | Message::Unchoke
            | Message::Cancel(_)
            | Message::SuggestPiece(_)
            | Message::HaveAll
            | Message::HaveNone
            | Message::RejectRequest { .. }
            | Message::AllowedFast(_) => {}
            _ => {}
        }
    }
    Ok(())
}
