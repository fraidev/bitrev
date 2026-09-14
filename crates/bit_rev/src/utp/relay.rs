//! In-process UDP relay that can drop, duplicate, and reorder packets.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use super::header::PacketType;

#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub drop_rate: f64,
    pub dup_rate: f64,
    pub reorder_rate: f64,
    pub seed: u64,
    /// Drop this many ST_DATA packets, then forward normally.
    pub drop_first_data: usize,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            drop_rate: 0.0,
            dup_rate: 0.0,
            reorder_rate: 0.0,
            seed: 1,
            drop_first_data: 0,
        }
    }
}

impl RelayConfig {
    pub fn lossy(drop_rate: f64) -> Self {
        Self {
            drop_rate,
            dup_rate: 0.02,
            reorder_rate: 0.05,
            seed: 0x55_70,
            drop_first_data: 0,
        }
    }
}

struct RelayState {
    backend: Option<SocketAddr>,
    client: Option<SocketAddr>,
    rng: StdRng,
    data_dropped: usize,
}

pub struct LossyRelay {
    local: SocketAddr,
    state: Arc<Mutex<RelayState>>,
    cancel: CancellationToken,
}

impl LossyRelay {
    pub async fn bind(config: RelayConfig) -> io::Result<Self> {
        let socket = Arc::new(UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?);
        let local = socket.local_addr()?;
        let drop_first_left = Arc::new(AtomicUsize::new(config.drop_first_data));
        let cancel = CancellationToken::new();
        let state = Arc::new(Mutex::new(RelayState {
            backend: None,
            client: None,
            rng: StdRng::seed_from_u64(config.seed),
            data_dropped: 0,
        }));
        let relay = Self {
            local,
            state: state.clone(),
            cancel: cancel.clone(),
        };
        tokio::spawn(run_relay(socket, state, config, drop_first_left, cancel));
        Ok(relay)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    pub fn set_backend(&self, addr: SocketAddr) {
        self.state.lock().unwrap().backend = Some(addr);
    }

    pub fn data_dropped(&self) -> usize {
        self.state.lock().unwrap().data_dropped
    }
}

impl Drop for LossyRelay {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn run_relay(
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<RelayState>>,
    config: RelayConfig,
    drop_first_left: Arc<AtomicUsize>,
    cancel: CancellationToken,
) {
    let mut buf = vec![0u8; 2048];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = result else { continue };
                let packet = buf[..n].to_vec();
                let dest = {
                    let mut g = state.lock().unwrap();
                    let backend = g.backend;
                    if backend == Some(from) {
                        g.client
                    } else {
                        g.client = Some(from);
                        backend
                    }
                };
                let Some(dest) = dest else { continue };

                if is_data(&packet) {
                    let left = drop_first_left.load(Ordering::Relaxed);
                    if left > 0
                        && drop_first_left
                            .compare_exchange(left, left - 1, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                    {
                        state.lock().unwrap().data_dropped += 1;
                        continue;
                    }
                }

                let (drop, dup, reorder) = {
                    let mut g = state.lock().unwrap();
                    let drop = g.rng.gen::<f64>() < config.drop_rate;
                    let dup = g.rng.gen::<f64>() < config.dup_rate;
                    let reorder = g.rng.gen::<f64>() < config.reorder_rate;
                    if drop {
                        g.data_dropped += 1;
                    }
                    (drop, dup, reorder)
                };
                if drop {
                    continue;
                }

                if reorder {
                    let sock = socket.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(3)).await;
                        let _ = sock.send_to(&packet, dest).await;
                    });
                    continue;
                }

                let _ = socket.send_to(&packet, dest).await;
                if dup {
                    let _ = socket.send_to(&packet, dest).await;
                }
            }
        }
    }
}

fn is_data(buf: &[u8]) -> bool {
    if buf.is_empty() {
        return false;
    }
    PacketType::from_u4(buf[0] >> 4) == Some(PacketType::Data) && buf.len() > 20
}
