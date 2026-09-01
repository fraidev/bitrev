#![allow(dead_code)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const PROTOCOL_ID: u64 = 0x0417_2710_1980;
const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;

#[derive(Clone, Debug)]
pub struct RecordedUdpAnnounce {
    pub from: SocketAddr,
    pub connection_id: u64,
    pub transaction_id: u32,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub downloaded: u64,
    pub left: u64,
    pub uploaded: u64,
    pub event: u32,
    pub key: u32,
    pub num_want: i32,
    pub port: u16,
}

#[derive(Clone, Debug)]
pub enum UdpAnnounceBody {
    Peers {
        interval: u32,
        leechers: u32,
        seeders: u32,
        peers: Vec<SocketAddr>,
    },
    Error(String),
    /// Receive the announce and send nothing back.
    Hang,
}

impl UdpAnnounceBody {
    pub fn peers(interval: u32, peers: Vec<SocketAddr>) -> Self {
        Self::Peers {
            interval,
            leechers: 0,
            seeders: peers.len() as u32,
            peers,
        }
    }
}

pub struct MockUdpTracker {
    pub addr: SocketAddr,
    pub url: String,
    announces: Arc<Mutex<Vec<RecordedUdpAnnounce>>>,
    announce_notify: Arc<Notify>,
    cancel: CancellationToken,
}

impl Drop for MockUdpTracker {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl MockUdpTracker {
    pub async fn start(responses: Vec<UdpAnnounceBody>) -> Self {
        let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind mock udp tracker");
        let addr = socket.local_addr().expect("udp local addr");
        let announces = Arc::new(Mutex::new(Vec::new()));
        let announce_notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let idx = Arc::new(AtomicUsize::new(0));
        let responses = Arc::new(responses);
        let announces_task = announces.clone();
        let notify_task = announce_notify.clone();
        let cancel_task = cancel.clone();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 512];
            let mut next_connection = 1u64;
            loop {
                tokio::select! {
                    _ = cancel_task.cancelled() => break,
                    recv = socket.recv_from(&mut buf) => {
                        let Ok((len, from)) = recv else { break };
                        let packet = &buf[..len];
                        if packet.len() == 16 {
                            if read_u64(packet, 0) != Some(PROTOCOL_ID)
                                || read_u32(packet, 8) != Some(ACTION_CONNECT)
                            {
                                continue;
                            }
                            let Some(tid) = read_u32(packet, 12) else { continue };
                            next_connection += 1;
                            let _ = socket
                                .send_to(&connect_response(tid, next_connection), from)
                                .await;
                            continue;
                        }
                        if packet.len() != 98 {
                            continue;
                        }
                        let Some(announce) = parse_announce(packet, from) else { continue };
                        announces_task.lock().unwrap().push(announce.clone());
                        notify_task.notify_waiters();
                        let i = idx.fetch_add(1, Ordering::SeqCst);
                        let reply = responses
                            .get(i)
                            .cloned()
                            .or_else(|| responses.last().cloned())
                            .unwrap_or_else(|| UdpAnnounceBody::peers(1800, Vec::new()));
                        match reply {
                            UdpAnnounceBody::Hang => {}
                            UdpAnnounceBody::Error(message) => {
                                let _ = socket
                                    .send_to(&error_response(announce.transaction_id, &message), from)
                                    .await;
                            }
                            UdpAnnounceBody::Peers {
                                interval,
                                leechers,
                                seeders,
                                peers,
                            } => {
                                let _ = socket
                                    .send_to(
                                        &announce_response(
                                            announce.transaction_id,
                                            interval,
                                            leechers,
                                            seeders,
                                            &peers,
                                        ),
                                        from,
                                    )
                                    .await;
                            }
                        }
                    }
                }
            }
        });

        Self {
            addr,
            url: format!("udp://{addr}/announce"),
            announces,
            announce_notify,
            cancel,
        }
    }

    pub fn announces(&self) -> Vec<RecordedUdpAnnounce> {
        self.announces.lock().unwrap().clone()
    }

    pub async fn wait_announces(&self, n: usize, timeout: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.announces().len() >= n {
                return;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                panic!(
                    "udp tracker saw {} announces, wanted {n} before timeout",
                    self.announces().len()
                );
            }
            tokio::select! {
                _ = self.announce_notify.notified() => {}
                _ = tokio::time::sleep(remaining) => {
                    panic!(
                        "udp tracker saw {} announces, wanted {n} before timeout",
                        self.announces().len()
                    );
                }
            }
        }
    }
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    data.get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_be_bytes)
}

fn read_u64(data: &[u8], offset: usize) -> Option<u64> {
    data.get(offset..offset + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_be_bytes)
}

fn connect_response(transaction_id: u32, connection_id: u64) -> [u8; 16] {
    let mut data = [0u8; 16];
    data[0..4].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    data[4..8].copy_from_slice(&transaction_id.to_be_bytes());
    data[8..16].copy_from_slice(&connection_id.to_be_bytes());
    data
}

fn error_response(transaction_id: u32, message: &str) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&ACTION_ERROR.to_be_bytes());
    data.extend_from_slice(&transaction_id.to_be_bytes());
    data.extend_from_slice(message.as_bytes());
    data
}

fn announce_response(
    transaction_id: u32,
    interval: u32,
    leechers: u32,
    seeders: u32,
    peers: &[SocketAddr],
) -> Vec<u8> {
    let mut data = Vec::with_capacity(20 + peers.len() * 6);
    data.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    data.extend_from_slice(&transaction_id.to_be_bytes());
    data.extend_from_slice(&interval.to_be_bytes());
    data.extend_from_slice(&leechers.to_be_bytes());
    data.extend_from_slice(&seeders.to_be_bytes());
    data.extend_from_slice(&super::compact_peers(peers));
    data
}

fn parse_announce(data: &[u8], from: SocketAddr) -> Option<RecordedUdpAnnounce> {
    if read_u32(data, 8)? != ACTION_ANNOUNCE {
        return None;
    }
    Some(RecordedUdpAnnounce {
        from,
        connection_id: read_u64(data, 0)?,
        transaction_id: read_u32(data, 12)?,
        info_hash: data[16..36].try_into().ok()?,
        peer_id: data[36..56].try_into().ok()?,
        downloaded: read_u64(data, 56)?,
        left: read_u64(data, 64)?,
        uploaded: read_u64(data, 72)?,
        event: read_u32(data, 80)?,
        key: read_u32(data, 88)?,
        num_want: i32::from_be_bytes(data[92..96].try_into().ok()?),
        port: u16::from_be_bytes(data[96..98].try_into().ok()?),
    })
}
