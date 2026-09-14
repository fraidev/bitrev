//! Per-connection state machine and [`UtpStream`] (AsyncRead/AsyncWrite).

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, ErrorKind};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Notify};
use tokio::time::{sleep_until, Instant};
use tokio_util::sync::CancellationToken;
use tracing::trace;

use super::delay::{Ledbat, RttEstimator};
use super::header::{
    sack_contains, sack_set, Extension, Packet, PacketType, EXT_SELECTIVE_ACK, HEADER_LEN,
};

pub const MSS: usize = 1372;
pub const MAX_UDP: usize = 1400;
pub const RECV_BUF_MAX: usize = 1024 * 1024;
pub const SEND_BUF_MAX: usize = 1024 * 1024;
pub const REORDER_MAX: u16 = 256;
pub const MAX_RETRANSMITS: u32 = 10;
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
pub const KEEPALIVE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    SynSent,
    SynRecv,
    Connected,
    Closed,
    Reset,
}

#[derive(Debug, Clone)]
struct InFlight {
    seq: u16,
    payload: Vec<u8>,
    ty: PacketType,
    sent_at: Instant,
    transmits: u32,
    sacked: bool,
}

#[derive(Debug)]
enum RecvSlot {
    Data(Vec<u8>),
    Fin,
}

pub(crate) struct Conn {
    pub remote: SocketAddr,
    pub recv_id: u16,
    pub send_id: u16,
    state: State,
    next_seq: u16,
    ack_nr: u16,
    ack_valid: bool,
    peer_wnd: u32,
    send_buf: VecDeque<u8>,
    recv_buf: VecDeque<u8>,
    in_flight: VecDeque<InFlight>,
    in_flight_bytes: usize,
    reorder: BTreeMap<u16, RecvSlot>,
    rtt: RttEstimator,
    cc: Ledbat,
    last_our_delay: u32,
    last_queuing: u32,
    their_timestamp: u32,
    have_their_ts: bool,
    last_recv: Instant,
    last_ack: u16,
    dup_acks: u8,
    need_ack: bool,
    write_shutdown: bool,
    fin_sent: bool,
    fin_seq: u16,
    fin_acked: bool,
    read_eof: bool,
    stream_dropped: bool,
    want_reset: bool,
    error: Option<io::ErrorKind>,
    error_msg: &'static str,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
    flush_waker: Option<Waker>,
    shutdown_waker: Option<Waker>,
    retransmits: u64,
    packets_sent: u64,
    packets_recv: u64,
}

impl Conn {
    fn new(remote: SocketAddr, recv_id: u16, send_id: u16, iss: u16, now: Instant) -> Self {
        Self {
            remote,
            recv_id,
            send_id,
            state: State::SynSent,
            next_seq: iss,
            ack_nr: 0,
            ack_valid: false,
            peer_wnd: u32::MAX,
            send_buf: VecDeque::new(),
            recv_buf: VecDeque::new(),
            in_flight: VecDeque::new(),
            in_flight_bytes: 0,
            reorder: BTreeMap::new(),
            rtt: RttEstimator::new(),
            cc: Ledbat::new(MSS as u32),
            last_our_delay: 0,
            last_queuing: 0,
            their_timestamp: 0,
            have_their_ts: false,
            last_recv: now,
            last_ack: 0,
            dup_acks: 0,
            need_ack: false,
            write_shutdown: false,
            fin_sent: false,
            fin_seq: 0,
            fin_acked: false,
            read_eof: false,
            stream_dropped: false,
            want_reset: false,
            error: None,
            error_msg: "uTP connection closed",
            read_waker: None,
            write_waker: None,
            flush_waker: None,
            shutdown_waker: None,
            retransmits: 0,
            packets_sent: 0,
            packets_recv: 0,
        }
    }

    pub fn initiator(
        remote: SocketAddr,
        recv_id: u16,
        send_id: u16,
        iss: u16,
        now: Instant,
    ) -> Self {
        let mut conn = Self::new(remote, recv_id, send_id, iss, now);
        conn.state = State::SynSent;
        conn
    }

    pub fn acceptor(
        remote: SocketAddr,
        recv_id: u16,
        send_id: u16,
        iss: u16,
        syn_seq: u16,
        now: Instant,
    ) -> Self {
        let mut conn = Self::new(remote, recv_id, send_id, iss, now);
        conn.state = State::SynRecv;
        conn.ack_nr = syn_seq;
        conn.ack_valid = true;
        conn.need_ack = true;
        conn
    }

    pub fn is_dead(&self) -> bool {
        matches!(self.state, State::Closed | State::Reset)
    }

    pub fn is_open(&self) -> bool {
        matches!(
            self.state,
            State::SynSent | State::SynRecv | State::Connected
        )
    }

    fn fail(&mut self, kind: io::ErrorKind, msg: &'static str) {
        self.state = State::Reset;
        self.error = Some(kind);
        self.error_msg = msg;
        self.read_eof = true;
        self.wake_all();
    }

    fn wake_all(&mut self) {
        if let Some(w) = self.read_waker.take() {
            w.wake();
        }
        if let Some(w) = self.write_waker.take() {
            w.wake();
        }
        if let Some(w) = self.flush_waker.take() {
            w.wake();
        }
        if let Some(w) = self.shutdown_waker.take() {
            w.wake();
        }
    }

    fn recv_window(&self) -> u32 {
        RECV_BUF_MAX.saturating_sub(self.recv_buf.len()) as u32
    }

    fn send_window(&self) -> usize {
        self.cc.clamp_to_peer(self.peer_wnd) as usize
    }

    fn connection_id_for(&self, ty: PacketType) -> u16 {
        if ty == PacketType::Syn {
            self.recv_id
        } else {
            self.send_id
        }
    }

    fn sack_mask(&self) -> Option<Vec<u8>> {
        if self.reorder.is_empty() {
            return None;
        }
        let mut mask = vec![0u8; 4];
        for &seq in self.reorder.keys() {
            let dist = seq.wrapping_sub(self.ack_nr.wrapping_add(2));
            if dist < 32 {
                sack_set(&mut mask, self.ack_nr, seq);
            }
        }
        if mask.iter().all(|b| *b == 0) {
            None
        } else {
            Some(mask)
        }
    }

    fn make_packet(&self, ty: PacketType, seq: u16, payload: Vec<u8>) -> Packet {
        let mut pkt = Packet::new(ty, self.connection_id_for(ty), seq, self.ack_nr);
        pkt.timestamp = timestamp_us();
        pkt.timestamp_diff = if self.have_their_ts {
            pkt.timestamp.wrapping_sub(self.their_timestamp)
        } else {
            0
        };
        pkt.wnd_size = self.recv_window();
        if ty != PacketType::Syn {
            if let Some(mask) = self.sack_mask() {
                pkt.extensions.push(Extension {
                    ty: EXT_SELECTIVE_ACK,
                    data: mask,
                });
            }
        }
        pkt.payload = payload;
        pkt
    }

    pub fn take_syn(&mut self, now: Instant) -> Packet {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let pkt = self.make_packet(PacketType::Syn, seq, Vec::new());
        self.track_inflight(seq, PacketType::Syn, Vec::new(), now);
        self.packets_sent += 1;
        pkt
    }

    pub fn take_syn_ack(&mut self) -> Packet {
        self.need_ack = false;
        self.packets_sent += 1;
        self.make_packet(PacketType::State, self.next_seq, Vec::new())
    }

    fn track_inflight(&mut self, seq: u16, ty: PacketType, payload: Vec<u8>, now: Instant) {
        self.in_flight_bytes += payload.len();
        self.in_flight.push_back(InFlight {
            seq,
            payload,
            ty,
            sent_at: now,
            transmits: 1,
            sacked: false,
        });
    }

    pub fn on_packet(&mut self, pkt: Packet, now: Instant) -> Vec<Packet> {
        if self.is_dead() {
            return Vec::new();
        }
        self.packets_recv += 1;
        self.last_recv = now;
        if pkt.ty == PacketType::Reset {
            self.fail(ErrorKind::ConnectionReset, "uTP reset");
            return Vec::new();
        }

        self.peer_wnd = pkt.wnd_size;
        self.their_timestamp = pkt.timestamp;
        self.have_their_ts = true;
        if pkt.timestamp_diff != 0 {
            self.last_our_delay = pkt.timestamp_diff;
            // Base-delay tracking is applied as a running min over the connection.
            // LEDBAT uses queuing delay = our_delay - min(our_delay).
            if self.last_queuing == 0 && self.last_our_delay > 0 {
                self.last_queuing = 0;
            }
        }

        if pkt.ty == PacketType::Syn {
            if self.state == State::SynRecv {
                self.need_ack = true;
                return self.flush(now);
            }
            return Vec::new();
        }

        self.process_ack(pkt.ack_nr, pkt.selective_ack(), now);

        match pkt.ty {
            PacketType::State => {
                if self.state == State::SynSent && self.syn_acked() {
                    // STATE does not consume a seq. ack_nr is last in-order
                    // packet, so the peer's first DATA uses this seq_nr.
                    self.ack_nr = pkt.seq_nr.wrapping_sub(1);
                    self.ack_valid = true;
                    self.state = State::Connected;
                    self.need_ack = true;
                } else if self.state == State::SynRecv {
                    self.state = State::Connected;
                }
            }
            PacketType::Data => {
                if self.state == State::SynSent {
                    self.ack_nr = pkt.seq_nr.wrapping_sub(1);
                    self.ack_valid = true;
                    self.state = State::Connected;
                } else if self.state == State::SynRecv {
                    self.state = State::Connected;
                }
                self.recv_segment(pkt.seq_nr, RecvSlot::Data(pkt.payload));
            }
            PacketType::Fin => {
                if self.state == State::SynRecv || self.state == State::SynSent {
                    self.state = State::Connected;
                    if !self.ack_valid {
                        self.ack_nr = pkt.seq_nr.wrapping_sub(1);
                        self.ack_valid = true;
                    }
                }
                self.recv_segment(pkt.seq_nr, RecvSlot::Fin);
            }
            PacketType::Syn | PacketType::Reset => {}
        }

        if self.read_eof && self.fin_acked {
            self.state = State::Closed;
            self.wake_all();
        }

        self.flush(now)
    }

    fn syn_acked(&self) -> bool {
        !self.in_flight.iter().any(|p| p.ty == PacketType::Syn)
    }

    fn process_ack(&mut self, ack: u16, sack: Option<&[u8]>, now: Instant) {
        if !self.ack_valid && self.state == State::SynSent {
            // SYN-ACK: ack must cover our SYN.
        }

        let mut acked_bytes = 0u32;
        let mut newly_acked = false;
        while let Some(front) = self.in_flight.front() {
            if seq_le(front.seq, ack) {
                let pkt = self.in_flight.pop_front().unwrap();
                if pkt.transmits == 1 {
                    if let Some(sample) = now.checked_duration_since(pkt.sent_at) {
                        self.rtt.update(sample);
                    }
                }
                if pkt.ty == PacketType::Fin {
                    self.fin_acked = true;
                }
                acked_bytes += pkt.payload.len() as u32;
                self.in_flight_bytes = self.in_flight_bytes.saturating_sub(pkt.payload.len());
                newly_acked = true;
            } else {
                break;
            }
        }

        if newly_acked {
            let queuing = self.queuing_delay();
            self.cc.on_ack(acked_bytes.max(HEADER_LEN as u32), queuing);
            self.dup_acks = 0;
            self.last_ack = ack;
            if let Some(w) = self.write_waker.take() {
                w.wake();
            }
            if self.send_buf.is_empty() && self.in_flight.is_empty() {
                if let Some(w) = self.flush_waker.take() {
                    w.wake();
                }
            }
            if self.fin_sent && self.send_buf.is_empty() {
                if let Some(w) = self.shutdown_waker.take() {
                    w.wake();
                }
            }
        } else if !self.in_flight.is_empty() && ack == self.last_ack {
            self.dup_acks = self.dup_acks.saturating_add(1);
            if self.dup_acks >= 3 {
                self.mark_fast_retransmit();
                self.dup_acks = 0;
            }
        } else {
            self.last_ack = ack;
        }

        if let Some(mask) = sack {
            self.apply_sack(ack, mask);
        }
    }

    fn queuing_delay(&self) -> u32 {
        // our_delay is the peer's timestamp_difference. Track a running min as base.
        // Full 13-bucket history lives on the socket path via repeated samples here.
        self.last_queuing
    }

    fn apply_sack(&mut self, ack: u16, mask: &[u8]) {
        for pkt in &mut self.in_flight {
            if sack_contains(mask, ack, pkt.seq) {
                pkt.sacked = true;
            }
        }
        let next = ack.wrapping_add(1);
        if let Some(pkt) = self.in_flight.iter_mut().find(|p| p.seq == next) {
            if !pkt.sacked {
                pkt.sacked = false;
                // Force a resend of the hole on the next flush.
                pkt.sent_at = Instant::now() - self.rtt.rto();
            }
        }
    }

    fn mark_fast_retransmit(&mut self) {
        if let Some(pkt) = self.in_flight.front_mut() {
            pkt.sent_at = Instant::now() - self.rtt.rto();
        }
    }

    fn recv_segment(&mut self, seq: u16, slot: RecvSlot) {
        if !self.ack_valid {
            return;
        }
        let expected = self.ack_nr.wrapping_add(1);
        if seq == expected {
            self.commit_slot(slot);
            self.ack_nr = seq;
            loop {
                let next = self.ack_nr.wrapping_add(1);
                match self.reorder.remove(&next) {
                    Some(item) => {
                        self.commit_slot(item);
                        self.ack_nr = next;
                    }
                    None => break,
                }
            }
        } else {
            let dist = seq.wrapping_sub(self.ack_nr);
            if (1..=REORDER_MAX).contains(&dist) && self.reorder.len() < REORDER_MAX as usize {
                self.reorder.entry(seq).or_insert(slot);
            }
        }
        self.need_ack = true;
    }

    fn commit_slot(&mut self, slot: RecvSlot) {
        match slot {
            RecvSlot::Data(data) => {
                if self.recv_buf.len() + data.len() <= RECV_BUF_MAX {
                    self.recv_buf.extend(data);
                }
                if let Some(w) = self.read_waker.take() {
                    w.wake();
                }
            }
            RecvSlot::Fin => {
                self.read_eof = true;
                if let Some(w) = self.read_waker.take() {
                    w.wake();
                }
            }
        }
    }

    pub fn flush(&mut self, now: Instant) -> Vec<Packet> {
        if self.is_dead() {
            return Vec::new();
        }
        if self.stream_dropped && !self.fin_sent {
            self.want_reset = true;
        }
        if self.want_reset {
            self.state = State::Reset;
            self.packets_sent += 1;
            return vec![self.make_packet(PacketType::Reset, self.next_seq, Vec::new())];
        }

        let mut out = Vec::new();

        // Retransmit timed-out or fast-resend packets first.
        if let Some(mut pkt) = self
            .in_flight
            .pop_front_if(|pkt| now.saturating_duration_since(pkt.sent_at) >= self.rtt.rto())
        {
            if pkt.transmits >= MAX_RETRANSMITS {
                self.fail(ErrorKind::TimedOut, "uTP retransmit limit");
                return Vec::new();
            }
            pkt.transmits += 1;
            pkt.sent_at = now;
            self.retransmits += 1;
            self.rtt.backoff();
            self.cc.on_loss();
            let rebuilt = self.make_packet(pkt.ty, pkt.seq, pkt.payload.clone());
            self.in_flight.push_front(pkt);
            self.packets_sent += 1;
            out.push(rebuilt);
        }

        let window = self.send_window();
        while self.in_flight_bytes < window {
            if self.send_buf.is_empty() {
                break;
            }
            if !matches!(self.state, State::Connected | State::SynRecv) {
                break;
            }
            let space = window.saturating_sub(self.in_flight_bytes).max(1);
            let n = self.send_buf.len().min(MSS).min(space);
            if n == 0 {
                break;
            }
            let payload: Vec<u8> = self.send_buf.drain(..n).collect();
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            let pkt = self.make_packet(PacketType::Data, seq, payload.clone());
            self.track_inflight(seq, PacketType::Data, payload, now);
            self.packets_sent += 1;
            self.need_ack = false;
            out.push(pkt);
        }

        if self.write_shutdown
            && self.send_buf.is_empty()
            && !self.fin_sent
            && matches!(self.state, State::Connected | State::SynRecv)
        {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            self.fin_sent = true;
            self.fin_seq = seq;
            let pkt = self.make_packet(PacketType::Fin, seq, Vec::new());
            self.track_inflight(seq, PacketType::Fin, Vec::new(), now);
            self.packets_sent += 1;
            self.need_ack = false;
            out.push(pkt);
            if let Some(w) = self.shutdown_waker.take() {
                w.wake();
            }
        }

        if self.need_ack && out.is_empty() {
            self.need_ack = false;
            self.packets_sent += 1;
            out.push(self.make_packet(PacketType::State, self.next_seq, Vec::new()));
        }

        if self.read_eof && self.fin_acked {
            self.state = State::Closed;
            self.wake_all();
        }

        out
    }

    pub fn on_timer(&mut self, now: Instant) -> Vec<Packet> {
        if self.is_dead() {
            return Vec::new();
        }
        if now.saturating_duration_since(self.last_recv) >= IDLE_TIMEOUT {
            self.fail(ErrorKind::TimedOut, "uTP idle timeout");
            return vec![self.make_packet(PacketType::Reset, self.next_seq, Vec::new())];
        }
        let packets = self.flush(now);
        if packets.is_empty()
            && self.in_flight.is_empty()
            && now.saturating_duration_since(self.last_recv) >= KEEPALIVE
            && self.is_open()
        {
            self.packets_sent += 1;
            return vec![self.make_packet(PacketType::State, self.next_seq, Vec::new())];
        }
        packets
    }

    pub fn next_deadline(&self, now: Instant) -> Instant {
        let idle = self.last_recv + IDLE_TIMEOUT;
        let keep = self.last_recv + KEEPALIVE;
        let rto = self
            .in_flight
            .front()
            .map(|p| p.sent_at + self.rtt.rto())
            .unwrap_or(keep);
        [rto, keep, idle]
            .into_iter()
            .min()
            .unwrap_or(now + MIN_TICK)
    }
}

const MIN_TICK: Duration = Duration::from_millis(50);

fn seq_le(a: u16, b: u16) -> bool {
    b.wrapping_sub(a) < 0x8000
}

fn timestamp_us() -> u32 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_micros() as u32
}

/// Rolling min of `our_delay` used as LEDBAT base delay (13 one-minute buckets).
pub struct DelayHist {
    inner: super::delay::BaseDelay,
}

impl DelayHist {
    pub fn new(now: Instant) -> Self {
        Self {
            inner: super::delay::BaseDelay::new(now),
        }
    }

    pub fn on_sample(&mut self, delay: u32, now: Instant) -> u32 {
        self.inner.on_sample(delay, now);
        self.inner.queuing_delay(delay)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UtpStats {
    pub retransmits: u64,
    pub packets_sent: u64,
    pub packets_recv: u64,
}

struct StreamShared {
    conn: Mutex<Conn>,
    delay: Mutex<DelayHist>,
    notify: Notify,
    connected: Notify,
    closed: AtomicBool,
}

#[derive(Clone)]
pub struct UtpStream {
    shared: Arc<StreamShared>,
}

impl UtpStream {
    fn wrap(conn: Conn, now: Instant) -> Self {
        Self {
            shared: Arc::new(StreamShared {
                conn: Mutex::new(conn),
                delay: Mutex::new(DelayHist::new(now)),
                notify: Notify::new(),
                connected: Notify::new(),
                closed: AtomicBool::new(false),
            }),
        }
    }

    pub fn initiator(
        remote: SocketAddr,
        recv_id: u16,
        send_id: u16,
        iss: u16,
        now: Instant,
    ) -> Self {
        Self::wrap(Conn::initiator(remote, recv_id, send_id, iss, now), now)
    }

    pub fn acceptor(
        remote: SocketAddr,
        recv_id: u16,
        send_id: u16,
        iss: u16,
        syn_seq: u16,
        now: Instant,
    ) -> Self {
        Self::wrap(
            Conn::acceptor(remote, recv_id, send_id, iss, syn_seq, now),
            now,
        )
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.shared.conn.lock().unwrap().remote
    }

    pub fn recv_id(&self) -> u16 {
        self.shared.conn.lock().unwrap().recv_id
    }

    pub fn stats(&self) -> UtpStats {
        let g = self.shared.conn.lock().unwrap();
        UtpStats {
            retransmits: g.retransmits,
            packets_sent: g.packets_sent,
            packets_recv: g.packets_recv,
        }
    }

    pub fn notify(&self) -> &Notify {
        &self.shared.notify
    }

    pub async fn wait_connected(&self) -> io::Result<()> {
        loop {
            {
                let g = self.shared.conn.lock().unwrap();
                if matches!(g.state, State::Connected | State::SynRecv) {
                    return Ok(());
                }
                if g.is_dead() {
                    return Err(io::Error::new(
                        g.error.unwrap_or(ErrorKind::ConnectionAborted),
                        g.error_msg,
                    ));
                }
            }
            self.shared.connected.notified().await;
        }
    }

    pub(crate) fn with_conn<R>(&self, f: impl FnOnce(&mut Conn) -> R) -> R {
        let mut g = self.shared.conn.lock().unwrap();
        f(&mut g)
    }
}

impl Drop for UtpStream {
    fn drop(&mut self) {
        if Arc::strong_count(&self.shared) <= 2 {
            let mut g = self.shared.conn.lock().unwrap();
            g.stream_dropped = true;
            if !g.fin_sent && !g.is_dead() {
                g.want_reset = true;
            }
            drop(g);
            self.shared.notify.notify_waiters();
        }
    }
}

pub(crate) async fn drive_conn(
    stream: UtpStream,
    udp: Arc<UdpSocket>,
    mut rx: mpsc::UnboundedReceiver<Packet>,
    cancel: CancellationToken,
    on_exit: impl FnOnce(),
) {
    let remote = stream.remote_addr();
    loop {
        if stream.shared.closed.load(Ordering::Relaxed) {
            break;
        }
        let deadline = {
            let g = stream.shared.conn.lock().unwrap();
            if g.is_dead() {
                break;
            }
            g.next_deadline(Instant::now())
        };

        tokio::select! {
            _ = cancel.cancelled() => break,
            pkt = rx.recv() => {
                let Some(pkt) = pkt else { break };
                let packets = {
                    let mut g = stream.shared.conn.lock().unwrap();
                    if pkt.timestamp_diff != 0 {
                        let q = stream
                            .shared
                            .delay
                            .lock()
                            .unwrap()
                            .on_sample(pkt.timestamp_diff, Instant::now());
                        g.last_queuing = q;
                    }
                    let was = g.state;
                    let packets = g.on_packet(pkt, Instant::now());
                    if g.state != was
                        && matches!(g.state, State::Connected | State::Closed | State::Reset)
                    {
                        stream.shared.connected.notify_waiters();
                    }
                    packets
                };
                send_all(&udp, remote, &packets).await;
            }
            _ = stream.shared.notify.notified() => {
                let packets = stream.with_conn(|c| c.flush(Instant::now()));
                send_all(&udp, remote, &packets).await;
            }
            _ = sleep_until(deadline) => {
                let packets = stream.with_conn(|c| c.on_timer(Instant::now()));
                send_all(&udp, remote, &packets).await;
            }
        }
    }
    stream.shared.closed.store(true, Ordering::Relaxed);
    stream.shared.connected.notify_waiters();
    stream.with_conn(|c| c.wake_all());
    on_exit();
}

async fn send_all(udp: &UdpSocket, remote: SocketAddr, packets: &[Packet]) {
    for pkt in packets {
        let bytes = pkt.encode();
        if bytes.len() > MAX_UDP + 32 {
            trace!(len = bytes.len(), "uTP packet larger than target size");
        }
        if let Err(e) = udp.send_to(&bytes, remote).await {
            trace!(error = %e, %remote, "uTP send failed");
            break;
        }
    }
}

impl AsyncRead for UtpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut g = self.shared.conn.lock().unwrap();
        if !g.recv_buf.is_empty() {
            let n = g.recv_buf.len().min(buf.remaining());
            let (a, b) = g.recv_buf.as_slices();
            let first = n.min(a.len());
            buf.put_slice(&a[..first]);
            if first < n {
                buf.put_slice(&b[..n - first]);
            }
            g.recv_buf.drain(..n);
            drop(g);
            self.shared.notify.notify_one();
            return Poll::Ready(Ok(()));
        }
        if g.read_eof {
            return Poll::Ready(Ok(()));
        }
        if g.is_dead() {
            return Poll::Ready(Err(io::Error::new(
                g.error.unwrap_or(ErrorKind::ConnectionAborted),
                g.error_msg,
            )));
        }
        g.read_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl AsyncWrite for UtpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut g = self.shared.conn.lock().unwrap();
        if g.write_shutdown {
            return Poll::Ready(Err(io::Error::new(
                ErrorKind::BrokenPipe,
                "uTP write half closed",
            )));
        }
        if g.is_dead() {
            return Poll::Ready(Err(io::Error::new(
                g.error.unwrap_or(ErrorKind::BrokenPipe),
                g.error_msg,
            )));
        }
        if g.send_buf.len() >= SEND_BUF_MAX {
            g.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = buf.len().min(SEND_BUF_MAX - g.send_buf.len());
        g.send_buf.extend(buf[..n].iter().copied());
        drop(g);
        self.shared.notify.notify_one();
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut g = self.shared.conn.lock().unwrap();
        if g.send_buf.is_empty() && g.in_flight.is_empty() {
            return Poll::Ready(Ok(()));
        }
        if g.is_dead() {
            return Poll::Ready(Err(io::Error::new(
                g.error.unwrap_or(ErrorKind::BrokenPipe),
                g.error_msg,
            )));
        }
        g.flush_waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut g = self.shared.conn.lock().unwrap();
        g.write_shutdown = true;
        if g.fin_sent && g.send_buf.is_empty() {
            drop(g);
            self.shared.notify.notify_one();
            return Poll::Ready(Ok(()));
        }
        if g.is_dead() {
            return Poll::Ready(Ok(()));
        }
        g.shutdown_waker = Some(cx.waker().clone());
        drop(g);
        self.shared.notify.notify_one();
        Poll::Pending
    }
}
