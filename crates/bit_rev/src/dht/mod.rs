//! Session-wide Mainline DHT (BEP-0005).

mod announce;
mod krpc;
mod lookup;
mod routing;
mod token;

pub use krpc::{decode, CompactNode, DecodeError, NodeId};
pub use routing::{RoutingTable, K};

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::peer::PeerAddr;

use announce::AnnounceStore;
use krpc::{
    encode, peek_is_query, peek_transaction_id, DecodeError as KrpcDecodeError, ErrorMsg,
    KrpcMessage, Payload, Query, Response, ERR_METHOD, ERR_PROTOCOL,
};
use lookup::{Lookup, LookupKind};
use routing::persist_path;
use token::TokenSecrets;

pub const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
pub const REANNOUNCE_INTERVAL: Duration = Duration::from_secs(15 * 60);
pub const PERSIST_INTERVAL: Duration = Duration::from_secs(5 * 60);
const TICK: Duration = Duration::from_millis(500);
const MAX_PACKET: usize = 2048;

pub const DEFAULT_BOOTSTRAP: &[&str] = &[
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
];

pub type PeerSink = Arc<dyn Fn([u8; 20], Vec<PeerAddr>) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DhtOptions {
    pub enabled: bool,
    pub port: u16,
    pub bootstrap_nodes: Vec<String>,
}

impl DhtOptions {
    pub fn default_bootstrap() -> Vec<String> {
        DEFAULT_BOOTSTRAP.iter().map(|s| (*s).to_string()).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DhtStats {
    pub nodes: usize,
    pub good_nodes: usize,
}

#[derive(Clone)]
pub struct DhtHandle {
    tx: mpsc::UnboundedSender<DhtCmd>,
    stats: Arc<Mutex<DhtStats>>,
    watched: Arc<Mutex<Vec<NodeId>>>,
    local_addr: SocketAddr,
    node_id: NodeId,
}

impl DhtHandle {
    pub fn stats(&self) -> DhtStats {
        *self.stats.lock().unwrap()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    pub fn udp_port(&self) -> u16 {
        self.local_addr.port()
    }

    pub fn add_torrent(&self, info_hash: [u8; 20], listen_port: u16, active: bool) {
        let _ = self.tx.send(DhtCmd::AddTorrent {
            info_hash,
            listen_port,
            active,
        });
    }

    pub fn remove_torrent(&self, info_hash: [u8; 20]) {
        let _ = self.tx.send(DhtCmd::RemoveTorrent { info_hash });
    }

    pub fn set_active(&self, info_hash: [u8; 20], active: bool) {
        let _ = self.tx.send(DhtCmd::SetActive { info_hash, active });
    }

    pub fn set_all_active(&self, active: bool) {
        let _ = self.tx.send(DhtCmd::SetAllActive { active });
    }

    pub fn ping_node(&self, addr: SocketAddr) {
        let _ = self.tx.send(DhtCmd::PingNode { addr });
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(DhtCmd::Shutdown);
    }

    pub fn watched(&self) -> Vec<NodeId> {
        self.watched.lock().unwrap().clone()
    }

    pub fn spawn(
        options: DhtOptions,
        state_dir: Option<PathBuf>,
        sink: PeerSink,
        cancel: CancellationToken,
    ) -> anyhow::Result<Self> {
        let bind = SocketAddr::from(([0, 0, 0, 0], options.port));
        let std_sock = std::net::UdpSocket::bind(bind)?;
        std_sock.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(std_sock)?;
        let local_addr = socket.local_addr()?;
        let now = Instant::now();
        let table = state_dir
            .as_ref()
            .and_then(|dir| routing::load(&persist_path(dir), now))
            .unwrap_or_else(|| RoutingTable::new(RoutingTable::random_id(), now));
        let node_id = table.id();
        let stats = Arc::new(Mutex::new(DhtStats {
            nodes: table.len(),
            good_nodes: table.good_len(now),
        }));
        let watched = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = DhtActor {
            socket,
            table,
            tokens: TokenSecrets::new(now),
            store: AnnounceStore::new(),
            pending: HashMap::new(),
            lookups: HashMap::new(),
            torrents: HashMap::new(),
            next_tid: 0,
            next_lookup: 0,
            peer_sink: sink,
            state_path: state_dir.map(|d| persist_path(&d)),
            last_persist: now,
            bootstrap: options.bootstrap_nodes,
            bootstrapped: false,
            stats: stats.clone(),
            watched: watched.clone(),
            cancel,
            rx,
        };
        tokio::spawn(actor.run());
        Ok(Self {
            tx,
            stats,
            watched,
            local_addr,
            node_id,
        })
    }
}

enum DhtCmd {
    AddTorrent {
        info_hash: NodeId,
        listen_port: u16,
        active: bool,
    },
    RemoveTorrent {
        info_hash: NodeId,
    },
    SetActive {
        info_hash: NodeId,
        active: bool,
    },
    SetAllActive {
        active: bool,
    },
    PingNode {
        addr: SocketAddr,
    },
    Shutdown,
}

struct Pending {
    sent: Instant,
    kind: PendingKind,
}

enum PendingKind {
    Ping { replacement: Option<CompactNode> },
    Lookup { lookup_id: u64 },
    Announce,
}

struct TorrentDht {
    listen_port: u16,
    active: bool,
    last_announce: Instant,
    lookup_id: Option<u64>,
}

struct DhtActor {
    socket: UdpSocket,
    table: RoutingTable,
    tokens: TokenSecrets,
    store: AnnounceStore,
    pending: HashMap<(Vec<u8>, SocketAddr), Pending>,
    lookups: HashMap<u64, Lookup>,
    torrents: HashMap<NodeId, TorrentDht>,
    next_tid: u16,
    next_lookup: u64,
    peer_sink: PeerSink,
    state_path: Option<PathBuf>,
    last_persist: Instant,
    bootstrap: Vec<String>,
    bootstrapped: bool,
    stats: Arc<Mutex<DhtStats>>,
    watched: Arc<Mutex<Vec<NodeId>>>,
    cancel: CancellationToken,
    rx: mpsc::UnboundedReceiver<DhtCmd>,
}

impl DhtActor {
    async fn run(mut self) {
        self.bootstrap().await;
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut buf = vec![0u8; MAX_PACKET];
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                cmd = self.rx.recv() => {
                    match cmd {
                        Some(DhtCmd::Shutdown) | None => break,
                        Some(cmd) => self.handle_cmd(cmd).await,
                    }
                }
                recv = self.socket.recv_from(&mut buf) => {
                    match recv {
                        Ok((n, from)) => self.on_packet(&buf[..n], from).await,
                        Err(e) => debug!(error = %e, "dht recv failed"),
                    }
                }
                _ = interval.tick() => self.tick().await,
            }
        }
        self.persist();
    }

    async fn bootstrap(&mut self) {
        if self.table.is_empty() && !self.bootstrap.is_empty() {
            for addr in resolve_bootstrap(&self.bootstrap).await {
                self.send_ping(addr, None).await;
            }
        } else if !self.table.is_empty() {
            self.start_find_self();
            self.kick_lookups().await;
        }
    }

    async fn handle_cmd(&mut self, cmd: DhtCmd) {
        match cmd {
            DhtCmd::AddTorrent {
                info_hash,
                listen_port,
                active,
            } => {
                if let Some(t) = self.torrents.get(&info_hash) {
                    if let Some(id) = t.lookup_id {
                        self.lookups.remove(&id);
                    }
                }
                self.torrents.insert(
                    info_hash,
                    TorrentDht {
                        listen_port,
                        active,
                        last_announce: Instant::now() - REANNOUNCE_INTERVAL,
                        lookup_id: None,
                    },
                );
                self.sync_watched();
                if active {
                    self.start_get_peers(info_hash);
                    self.kick_lookups().await;
                }
            }
            DhtCmd::RemoveTorrent { info_hash } => {
                if let Some(t) = self.torrents.remove(&info_hash) {
                    if let Some(id) = t.lookup_id {
                        self.lookups.remove(&id);
                    }
                }
                self.sync_watched();
            }
            DhtCmd::SetActive { info_hash, active } => {
                if let Some(t) = self.torrents.get_mut(&info_hash) {
                    t.active = active;
                    if !active {
                        if let Some(id) = t.lookup_id.take() {
                            self.lookups.remove(&id);
                        }
                    }
                }
                if active {
                    self.start_get_peers(info_hash);
                    self.kick_lookups().await;
                }
            }
            DhtCmd::SetAllActive { active } => {
                let hashes: Vec<NodeId> = self.torrents.keys().copied().collect();
                for hash in hashes {
                    if let Some(t) = self.torrents.get_mut(&hash) {
                        t.active = active;
                        if !active {
                            if let Some(id) = t.lookup_id.take() {
                                self.lookups.remove(&id);
                            }
                        }
                    }
                    if active {
                        self.start_get_peers(hash);
                    }
                }
                if active {
                    self.kick_lookups().await;
                }
            }
            DhtCmd::PingNode { addr } => {
                self.send_ping(addr, None).await;
            }
            DhtCmd::Shutdown => {}
        }
    }

    async fn on_packet(&mut self, buf: &[u8], from: SocketAddr) {
        match decode(buf) {
            Ok(msg) => match msg.payload {
                Payload::Query(q) => self.on_query(msg.transaction_id, q, from).await,
                Payload::Response(r) => self.on_response(msg.transaction_id, r, from).await,
                Payload::Error(e) => self.on_error(msg.transaction_id, e, from),
            },
            Err(KrpcDecodeError::UnknownMethod) => {
                if let Some(t) = peek_transaction_id(buf) {
                    self.send_msg(from, KrpcMessage::error(t, ERR_METHOD, "Method Unknown"))
                        .await;
                }
            }
            Err(_) if peek_is_query(buf) => {
                if let Some(t) = peek_transaction_id(buf) {
                    self.send_msg(from, KrpcMessage::error(t, ERR_PROTOCOL, "Protocol Error"))
                        .await;
                }
            }
            Err(_) => {}
        }
    }

    async fn on_query(&mut self, t: Vec<u8>, query: Query, from: SocketAddr) {
        let now = Instant::now();
        self.table.seen_query(query.id(), from, now);
        self.refresh_stats(now);
        let our_id = self.table.id();
        let reply = match query {
            Query::Ping { .. } => KrpcMessage::response(
                t,
                Response {
                    id: our_id,
                    nodes: vec![],
                    values: vec![],
                    token: None,
                },
            ),
            Query::FindNode { target, .. } => KrpcMessage::response(
                t,
                Response {
                    id: our_id,
                    nodes: self.table.closest(&target, K),
                    values: vec![],
                    token: None,
                },
            ),
            Query::GetPeers { info_hash, .. } => {
                let token = self.tokens.issue(from.ip());
                let values = self.store.get(&info_hash, now);
                let nodes = if values.is_empty() {
                    self.table.closest(&info_hash, K)
                } else {
                    vec![]
                };
                KrpcMessage::response(
                    t,
                    Response {
                        id: our_id,
                        nodes,
                        values,
                        token: Some(token),
                    },
                )
            }
            Query::AnnouncePeer {
                info_hash,
                port,
                token,
                implied_port,
                ..
            } => {
                if !self.tokens.validate(from.ip(), &token) {
                    KrpcMessage::error(t, ERR_PROTOCOL, "bad token")
                } else {
                    let peer_port = if implied_port { from.port() } else { port };
                    let peer = SocketAddr::new(from.ip(), peer_port);
                    self.store.announce(info_hash, peer, now);
                    KrpcMessage::response(
                        t,
                        Response {
                            id: our_id,
                            nodes: vec![],
                            values: vec![],
                            token: None,
                        },
                    )
                }
            }
        };
        self.send_msg(from, reply).await;
    }

    async fn on_response(&mut self, t: Vec<u8>, response: Response, from: SocketAddr) {
        let Some(pending) = self.pending.remove(&(t, from)) else {
            return;
        };
        let now = Instant::now();
        self.table.seen_response(response.id, from, now);
        for node in &response.nodes {
            let _ = self.table.insert(node.clone(), now);
        }
        self.refresh_stats(now);

        match pending.kind {
            PendingKind::Ping { replacement } => {
                if !self.bootstrapped && !self.table.is_empty() {
                    self.start_find_self();
                    self.kick_lookups().await;
                }
                let _ = replacement;
            }
            PendingKind::Lookup { lookup_id } => {
                self.on_lookup_response(lookup_id, from, response).await;
            }
            PendingKind::Announce => {}
        }
    }

    fn on_error(&mut self, t: Vec<u8>, _err: ErrorMsg, from: SocketAddr) {
        if let Some(pending) = self.pending.remove(&(t, from)) {
            if let PendingKind::Lookup { lookup_id } = pending.kind {
                if let Some(lookup) = self.lookups.get_mut(&lookup_id) {
                    lookup.on_timeout(from);
                }
            }
        }
    }

    async fn on_lookup_response(&mut self, lookup_id: u64, from: SocketAddr, response: Response) {
        let Some(lookup) = self.lookups.get_mut(&lookup_id) else {
            return;
        };
        let new_values = lookup.on_response(from, response.id, &response);
        if !new_values.is_empty() {
            if let Some(hash) = self.lookup_info_hash(lookup_id) {
                (self.peer_sink)(hash, new_values);
            }
        }
        self.drive_lookup(lookup_id).await;
    }

    fn lookup_info_hash(&self, lookup_id: u64) -> Option<NodeId> {
        self.torrents
            .iter()
            .find(|(_, t)| t.lookup_id == Some(lookup_id))
            .map(|(h, _)| *h)
            .or_else(|| {
                self.lookups.get(&lookup_id).and_then(|l| {
                    if l.kind == LookupKind::GetPeers {
                        Some(l.target)
                    } else {
                        None
                    }
                })
            })
    }

    fn start_find_self(&mut self) {
        if self.bootstrapped {
            return;
        }
        self.bootstrapped = true;
        let id = self.table.id();
        let seeds = self.table.closest(&id, K);
        self.start_lookup(id, LookupKind::FindNode, seeds, None);
    }

    fn start_get_peers(&mut self, info_hash: NodeId) {
        if let Some(t) = self.torrents.get(&info_hash) {
            if let Some(id) = t.lookup_id {
                self.lookups.remove(&id);
            }
        }
        let now = Instant::now();
        let local = self.store.get(&info_hash, now);
        if !local.is_empty() {
            (self.peer_sink)(info_hash, local);
        }
        let seeds = self.table.closest(&info_hash, K);
        let lookup_id = self.start_lookup(info_hash, LookupKind::GetPeers, seeds, Some(info_hash));
        if let Some(t) = self.torrents.get_mut(&info_hash) {
            t.lookup_id = Some(lookup_id);
        }
    }

    fn start_lookup(
        &mut self,
        target: NodeId,
        kind: LookupKind,
        seeds: Vec<CompactNode>,
        _info_hash: Option<NodeId>,
    ) -> u64 {
        let id = self.next_lookup;
        self.next_lookup = self.next_lookup.wrapping_add(1);
        self.lookups.insert(id, Lookup::new(target, kind, seeds));
        id
    }

    async fn kick_lookups(&mut self) {
        let ids: Vec<u64> = self.lookups.keys().copied().collect();
        for id in ids {
            self.drive_lookup(id).await;
        }
    }

    async fn drive_lookup(&mut self, lookup_id: u64) {
        loop {
            let Some(lookup) = self.lookups.get_mut(&lookup_id) else {
                return;
            };
            if lookup.is_done() {
                self.finish_lookup(lookup_id).await;
                return;
            }
            let queries = lookup.next_queries();
            if queries.is_empty() {
                if lookup.is_done() {
                    self.finish_lookup(lookup_id).await;
                }
                return;
            }
            let kind = lookup.kind;
            let target = lookup.target;
            for node in queries {
                match kind {
                    LookupKind::FindNode => {
                        self.send_query(
                            node.addr,
                            Query::FindNode {
                                id: self.table.id(),
                                target,
                            },
                            PendingKind::Lookup { lookup_id },
                        )
                        .await;
                    }
                    LookupKind::GetPeers => {
                        self.send_query(
                            node.addr,
                            Query::GetPeers {
                                id: self.table.id(),
                                info_hash: target,
                            },
                            PendingKind::Lookup { lookup_id },
                        )
                        .await;
                    }
                }
            }
        }
    }

    async fn finish_lookup(&mut self, lookup_id: u64) {
        let Some(lookup) = self.lookups.remove(&lookup_id) else {
            return;
        };
        if lookup.kind != LookupKind::GetPeers {
            return;
        }
        if !lookup.values().is_empty() {
            (self.peer_sink)(lookup.target, lookup.values().to_vec());
        }
        let info_hash = lookup.target;
        let Some(torrent) = self.torrents.get(&info_hash) else {
            return;
        };
        if !torrent.active || torrent.listen_port == 0 {
            return;
        }
        let port = torrent.listen_port;
        let targets = lookup.announce_targets();
        for (node, token) in targets {
            self.send_query(
                node.addr,
                Query::AnnouncePeer {
                    id: self.table.id(),
                    info_hash,
                    port,
                    token,
                    implied_port: false,
                },
                PendingKind::Announce,
            )
            .await;
        }
        if let Some(t) = self.torrents.get_mut(&info_hash) {
            t.last_announce = Instant::now();
            t.lookup_id = None;
        }
    }

    async fn send_ping(&mut self, addr: SocketAddr, replacement: Option<CompactNode>) {
        self.send_query(
            addr,
            Query::Ping {
                id: self.table.id(),
            },
            PendingKind::Ping { replacement },
        )
        .await;
    }

    async fn send_query(&mut self, addr: SocketAddr, query: Query, kind: PendingKind) {
        let t = self.next_tid.to_be_bytes()[..krpc::TRANSACTION_ID_LEN].to_vec();
        self.next_tid = self.next_tid.wrapping_add(1);
        let msg = KrpcMessage::query(t.clone(), query);
        self.pending.insert(
            (t, addr),
            Pending {
                sent: Instant::now(),
                kind,
            },
        );
        self.send_msg(addr, msg).await;
    }

    async fn send_msg(&self, addr: SocketAddr, msg: KrpcMessage) {
        let bytes = encode(&msg);
        if let Err(e) = self.socket.send_to(&bytes, addr).await {
            debug!(%addr, error = %e, "dht send failed");
        }
    }

    async fn tick(&mut self) {
        let now = Instant::now();
        self.tokens.maybe_rotate(now);
        self.expire_pending(now).await;
        if !self.bootstrapped && !self.table.is_empty() {
            self.start_find_self();
        }
        self.refresh_buckets(now);
        self.reannounce(now);
        if now.saturating_duration_since(self.last_persist) >= PERSIST_INTERVAL {
            self.persist();
            self.last_persist = now;
        }
        self.refresh_stats(now);

        let lookup_ids: Vec<u64> = self.lookups.keys().copied().collect();
        for id in lookup_ids {
            self.drive_lookup(id).await;
        }
    }

    async fn expire_pending(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| now.saturating_duration_since(p.sent) >= QUERY_TIMEOUT)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            if let Some(pending) = self.pending.remove(&key) {
                let addr = key.1;
                self.table.timed_out(addr);
                match pending.kind {
                    PendingKind::Ping { replacement } => {
                        if let Some(node) = replacement {
                            self.table.replace(addr, node, now);
                        }
                    }
                    PendingKind::Lookup { lookup_id } => {
                        if let Some(lookup) = self.lookups.get_mut(&lookup_id) {
                            lookup.on_timeout(addr);
                        }
                    }
                    PendingKind::Announce => {}
                }
            }
        }
    }

    fn refresh_buckets(&mut self, now: Instant) {
        let targets = self.table.stale_buckets(now);
        for target in targets {
            let seeds = self.table.closest(&target, K);
            self.start_lookup(target, LookupKind::FindNode, seeds, None);
        }
    }

    fn reannounce(&mut self, now: Instant) {
        let due: Vec<NodeId> = self
            .torrents
            .iter()
            .filter(|(_, t)| {
                t.active && now.saturating_duration_since(t.last_announce) >= REANNOUNCE_INTERVAL
            })
            .map(|(h, _)| *h)
            .collect();
        for hash in due {
            self.start_get_peers(hash);
        }
    }

    fn persist(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        if let Err(e) = routing::save(&self.table, path, Instant::now()) {
            warn!(error = %e, "failed to persist dht.dat");
        }
    }

    fn refresh_stats(&self, now: Instant) {
        *self.stats.lock().unwrap() = DhtStats {
            nodes: self.table.len(),
            good_nodes: self.table.good_len(now),
        };
    }

    fn sync_watched(&self) {
        *self.watched.lock().unwrap() = self.torrents.keys().copied().collect();
    }
}

async fn resolve_bootstrap(nodes: &[String]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for spec in nodes {
        if let Ok(addr) = spec.parse::<SocketAddr>() {
            if addr.is_ipv4() {
                out.push(addr);
            }
            continue;
        }
        match tokio::net::lookup_host(spec).await {
            Ok(iter) => out.extend(iter.filter(SocketAddr::is_ipv4)),
            Err(e) => debug!(node = %spec, error = %e, "dht bootstrap resolve failed"),
        }
    }
    out
}
