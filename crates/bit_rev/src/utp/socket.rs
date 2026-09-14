//! One UDP socket multiplexing all uTP connections.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use super::conn::{drive_conn, UtpStream};
use super::header::{Packet, PacketType, HEADER_LEN};

type ConnKey = (SocketAddr, u16);

struct SocketInner {
    udp: Arc<UdpSocket>,
    conns: Arc<Mutex<HashMap<ConnKey, mpsc::UnboundedSender<Packet>>>>,
    accept_tx: mpsc::Sender<(UtpStream, SocketAddr)>,
    accept_rx: tokio::sync::Mutex<mpsc::Receiver<(UtpStream, SocketAddr)>>,
    cancel: CancellationToken,
}

#[derive(Clone)]
pub struct UtpSocket {
    inner: Arc<SocketInner>,
}

impl UtpSocket {
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        let udp = UdpSocket::bind(addr).await?;
        Ok(Self::from_udp(Arc::new(udp)))
    }

    pub fn bind_std(addr: SocketAddr) -> io::Result<Self> {
        let std_sock = std::net::UdpSocket::bind(addr)?;
        std_sock.set_nonblocking(true)?;
        let udp = UdpSocket::from_std(std_sock)?;
        Ok(Self::from_udp(Arc::new(udp)))
    }

    pub fn from_udp(udp: Arc<UdpSocket>) -> Self {
        let (accept_tx, accept_rx) = mpsc::channel(128);
        let cancel = CancellationToken::new();
        let inner = Arc::new(SocketInner {
            udp: udp.clone(),
            conns: Arc::new(Mutex::new(HashMap::new())),
            accept_tx,
            accept_rx: tokio::sync::Mutex::new(accept_rx),
            cancel: cancel.clone(),
        });
        let dispatch = inner.clone();
        tokio::spawn(async move {
            recv_loop(dispatch).await;
        });
        Self { inner }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.udp.local_addr()
    }

    pub fn udp(&self) -> Arc<UdpSocket> {
        self.inner.udp.clone()
    }

    pub async fn connect(&self, addr: SocketAddr) -> io::Result<UtpStream> {
        let now = tokio::time::Instant::now();
        let recv_id = rand::random::<u16>();
        let send_id = recv_id.wrapping_add(1);
        let iss = rand::random::<u16>();
        let stream = UtpStream::initiator(addr, recv_id, send_id, iss, now);
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.conns.lock().unwrap().insert((addr, recv_id), tx);
        let syn = stream.with_conn(|c| c.take_syn(now));
        self.inner.udp.send_to(&syn.encode(), addr).await?;
        self.spawn_conn(stream.clone(), rx, (addr, recv_id));
        stream.notify().notify_one();
        stream.wait_connected().await?;
        Ok(stream)
    }

    pub async fn accept(&self) -> io::Result<(UtpStream, SocketAddr)> {
        let mut rx = self.inner.accept_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::ConnectionAborted, "uTP socket closed"))
    }

    pub fn close(&self) {
        self.inner.cancel.cancel();
        self.inner.conns.lock().unwrap().clear();
    }

    fn spawn_conn(&self, stream: UtpStream, rx: mpsc::UnboundedReceiver<Packet>, key: ConnKey) {
        let udp = self.inner.udp.clone();
        let cancel = self.inner.cancel.clone();
        let conns = self.inner.conns.clone();
        tokio::spawn(async move {
            drive_conn(stream, udp, rx, cancel, move || {
                conns.lock().unwrap().remove(&key);
            })
            .await;
        });
    }
}

impl Drop for UtpSocket {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.close();
        }
    }
}

async fn recv_loop(inner: Arc<SocketInner>) {
    let mut buf = vec![0u8; 2048];
    loop {
        tokio::select! {
            _ = inner.cancel.cancelled() => break,
            result = inner.udp.recv_from(&mut buf) => {
                let (n, addr) = match result {
                    Ok(pair) => pair,
                    Err(e) => {
                        trace!(error = %e, "uTP recv failed");
                        continue;
                    }
                };
                if n < HEADER_LEN {
                    continue;
                }
                let Ok(pkt) = Packet::decode(&buf[..n]) else {
                    continue;
                };
                dispatch(&inner, addr, pkt).await;
            }
        }
    }
}

async fn dispatch(inner: &SocketInner, addr: SocketAddr, pkt: Packet) {
    let key = (addr, pkt.connection_id);
    let existing = inner.conns.lock().unwrap().get(&key).cloned();
    if let Some(tx) = existing {
        let _ = tx.send(pkt);
        return;
    }

    if pkt.ty == PacketType::Syn {
        accept_syn(inner, addr, pkt).await;
        return;
    }

    debug!(%addr, conn = pkt.connection_id, "ST_RESET for unknown uTP connection");
    let mut rst = Packet::new(PacketType::Reset, pkt.connection_id, pkt.ack_nr, pkt.seq_nr);
    rst.timestamp = pkt.timestamp;
    let _ = inner.udp.send_to(&rst.encode(), addr).await;
}

async fn accept_syn(inner: &SocketInner, addr: SocketAddr, pkt: Packet) {
    let send_id = pkt.connection_id;
    let recv_id = send_id.wrapping_add(1);
    let key = (addr, recv_id);
    if let Some(tx) = inner.conns.lock().unwrap().get(&key).cloned() {
        let _ = tx.send(pkt);
        return;
    }
    let now = tokio::time::Instant::now();
    let iss = rand::random::<u16>();
    let stream = UtpStream::acceptor(addr, recv_id, send_id, iss, pkt.seq_nr, now);
    let (tx, rx) = mpsc::unbounded_channel();
    inner.conns.lock().unwrap().insert(key, tx);

    let syn_ack = stream.with_conn(|c| c.take_syn_ack());
    if let Err(e) = inner.udp.send_to(&syn_ack.encode(), addr).await {
        debug!(error = %e, %addr, "failed to send uTP SYN-ACK");
        inner.conns.lock().unwrap().remove(&key);
        return;
    }

    let udp = inner.udp.clone();
    let cancel = inner.cancel.clone();
    let conns = inner.conns.clone();
    let driven = stream.clone();
    tokio::spawn(async move {
        drive_conn(driven, udp, rx, cancel, move || {
            conns.lock().unwrap().remove(&key);
        })
        .await;
    });

    if inner.accept_tx.try_send((stream, addr)).is_err() {
        debug!(%addr, "uTP accept queue full, resetting");
        let rst = Packet::new(PacketType::Reset, send_id, 0, pkt.seq_nr);
        let _ = inner.udp.send_to(&rst.encode(), addr).await;
        inner.conns.lock().unwrap().remove(&key);
    }
}
