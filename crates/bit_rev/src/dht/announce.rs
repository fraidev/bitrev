//! Bounded per-info-hash announce store.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::krpc::NodeId;

pub const ANNOUNCE_TTL: Duration = Duration::from_secs(30 * 60);
pub const MAX_PEERS_PER_HASH: usize = 50;
pub const MAX_ANNOUNCE_HASHES: usize = 1024;

#[derive(Debug, Clone)]
struct PeerEntry {
    addr: SocketAddr,
    seen: Instant,
}

#[derive(Debug, Default)]
pub struct AnnounceStore {
    peers: HashMap<NodeId, Vec<PeerEntry>>,
}

impl AnnounceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn announce(&mut self, info_hash: NodeId, addr: SocketAddr, now: Instant) {
        if addr.port() == 0 {
            return;
        }
        self.expire(now);
        if !self.peers.contains_key(&info_hash) && self.peers.len() >= MAX_ANNOUNCE_HASHES {
            if let Some(oldest) = self
                .peers
                .iter()
                .min_by_key(|(_, v)| v.first().map(|p| p.seen).unwrap_or(now))
                .map(|(k, _)| *k)
            {
                self.peers.remove(&oldest);
            }
        }
        let list = self.peers.entry(info_hash).or_default();
        if let Some(existing) = list.iter_mut().find(|p| p.addr == addr) {
            existing.seen = now;
            return;
        }
        if list.len() >= MAX_PEERS_PER_HASH {
            list.sort_by_key(|p| p.seen);
            list.remove(0);
        }
        list.push(PeerEntry { addr, seen: now });
    }

    pub fn get(&mut self, info_hash: &NodeId, now: Instant) -> Vec<SocketAddr> {
        self.expire(now);
        self.peers
            .get(info_hash)
            .map(|list| list.iter().map(|p| p.addr).collect())
            .unwrap_or_default()
    }

    fn expire(&mut self, now: Instant) {
        for list in self.peers.values_mut() {
            list.retain(|p| now.saturating_duration_since(p.seen) < ANNOUNCE_TTL);
        }
        self.peers.retain(|_, list| !list.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    #[test]
    fn stores_and_expires() {
        let mut store = AnnounceStore::new();
        let hash = [1u8; 20];
        let t0 = Instant::now();
        store.announce(hash, addr(1), t0);
        assert_eq!(store.get(&hash, t0), vec![addr(1)]);
        assert!(store
            .get(&hash, t0 + ANNOUNCE_TTL + Duration::from_secs(1))
            .is_empty());
    }

    #[test]
    fn caps_per_hash() {
        let mut store = AnnounceStore::new();
        let hash = [2u8; 20];
        let t0 = Instant::now();
        for i in 0..(MAX_PEERS_PER_HASH as u16 + 5) {
            store.announce(hash, addr(1000 + i), t0 + Duration::from_millis(i as u64));
        }
        assert_eq!(
            store.get(&hash, t0 + Duration::from_secs(1)).len(),
            MAX_PEERS_PER_HASH
        );
    }
}
