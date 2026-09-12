use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::Notify;

use crate::{
    bitfield::Bitfield,
    message::{BlockRequest, WriterRequest},
    peer::PeerAddr,
};

pub const BAN_TTL: Duration = Duration::from_secs(60 * 60);
pub const HASH_FAILURE_BAN_THRESHOLD: u32 = 3;

#[derive(Debug, Default)]
pub struct PeerStates {
    pub states: DashMap<PeerAddr, PeerState>,
    hash_failures: DashMap<PeerAddr, u32>,
    banned: DashMap<PeerAddr, Instant>,
}

impl PeerStates {
    pub fn add_if_not_seen(&self, peer: PeerAddr) -> bool {
        if self.is_banned(peer) {
            return false;
        }
        use dashmap::mapref::entry::Entry;
        match self.states.entry(peer) {
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                entry.insert(PeerState::default());
                true
            }
        }
    }

    pub fn insert_live(&self, peer: PeerAddr, writer_tx: flume::Sender<WriterRequest>) -> bool {
        if self.is_banned(peer) {
            return false;
        }
        use dashmap::mapref::entry::Entry;
        match self.states.entry(peer) {
            Entry::Occupied(mut entry) => {
                if entry.get().writer_tx.is_some() {
                    false
                } else {
                    *entry.get_mut() = PeerState::live(writer_tx);
                    true
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(PeerState::live(writer_tx));
                true
            }
        }
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    pub fn purge_expired_bans(&self) {
        let now = Instant::now();
        self.banned.retain(|_, until| *until > now);
    }

    pub fn is_banned(&self, peer: PeerAddr) -> bool {
        self.purge_expired_bans();
        self.banned.contains_key(&peer)
    }

    pub fn ban(&self, peer: PeerAddr) {
        self.banned.insert(peer, Instant::now() + BAN_TTL);
        if let Some(state) = self.states.get(&peer) {
            if let Some(tx) = &state.writer_tx {
                let _ = tx.send(WriterRequest::Disconnect);
            }
        }
    }

    pub fn banned_count(&self) -> usize {
        self.purge_expired_bans();
        self.banned.len()
    }

    pub fn hash_failures(&self, peer: PeerAddr) -> u32 {
        self.hash_failures.get(&peer).map(|n| *n).unwrap_or(0)
    }

    pub fn increment_failures(&self, peer: PeerAddr) -> u32 {
        let count = {
            let mut entry = self.hash_failures.entry(peer).or_insert(0);
            *entry = entry.saturating_add(1);
            *entry
        };
        if let Some(mut state) = self.states.get_mut(&peer) {
            state.hash_failures = count;
        }
        count
    }

    /// Returns true if the peer is now banned.
    pub fn record_hash_failure(&self, peer: PeerAddr, sole_contributor: bool) -> bool {
        if sole_contributor {
            self.increment_failures(peer);
            self.ban(peer);
            return true;
        }
        let count = self.increment_failures(peer);
        if count >= HASH_FAILURE_BAN_THRESHOLD {
            self.ban(peer);
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone)]
pub struct PeerState {
    pub peer_interested: bool,
    pub bitfield: Bitfield,
    pub am_choking: bool,
    pub am_interested: bool,
    pub peer_choking: bool,
    pub connected_at: Instant,
    pub last_unchoked: Option<Instant>,
    pub is_optimistic: bool,
    pub fast_extension: bool,
    pub extension_protocol: bool,
    pub peer_allowed_fast: HashSet<u32>,
    pub our_allowed_fast: HashSet<u32>,
    pub suggested_pieces: Vec<u32>,
    pub hash_failures: u32,
    pub snubbed: bool,
    pub stats: Arc<PeerLiveStats>,
    pub writer_tx: Option<flume::Sender<WriterRequest>>,
}

#[derive(Debug)]
pub struct PeerLiveStats {
    pub bytes_downloaded: AtomicU64,
    pub bytes_uploaded: AtomicU64,
    pub am_choking: AtomicBool,
    pub peer_interested: AtomicBool,
    pub am_interested: AtomicBool,
    pub peer_choking: AtomicBool,
    pub upload_notify: Notify,
    pub download_cancels: Mutex<Vec<BlockRequest>>,
}

impl Default for PeerLiveStats {
    fn default() -> Self {
        Self {
            bytes_downloaded: AtomicU64::new(0),
            bytes_uploaded: AtomicU64::new(0),
            am_choking: AtomicBool::new(true),
            peer_interested: AtomicBool::new(false),
            am_interested: AtomicBool::new(false),
            peer_choking: AtomicBool::new(true),
            upload_notify: Notify::new(),
            download_cancels: Mutex::new(Vec::new()),
        }
    }
}

impl Default for PeerState {
    fn default() -> Self {
        Self::live_unwired()
    }
}

impl PeerState {
    fn live_unwired() -> Self {
        Self {
            peer_interested: false,
            bitfield: Bitfield::new(vec![]),
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            connected_at: Instant::now(),
            last_unchoked: None,
            is_optimistic: false,
            fast_extension: false,
            extension_protocol: false,
            peer_allowed_fast: HashSet::new(),
            our_allowed_fast: HashSet::new(),
            suggested_pieces: Vec::new(),
            hash_failures: 0,
            snubbed: false,
            stats: Arc::new(PeerLiveStats::default()),
            writer_tx: None,
        }
    }

    pub fn live(writer_tx: flume::Sender<WriterRequest>) -> Self {
        let mut state = Self::live_unwired();
        state.writer_tx = Some(writer_tx);
        state
    }

    pub fn set_peer_interested(&mut self, interested: bool) {
        self.peer_interested = interested;
        self.stats
            .peer_interested
            .store(interested, Ordering::Relaxed);
    }

    pub fn set_am_interested(&mut self, interested: bool) {
        self.am_interested = interested;
        self.stats
            .am_interested
            .store(interested, Ordering::Relaxed);
    }

    pub fn set_am_choking(&mut self, choking: bool) {
        self.am_choking = choking;
        self.stats.am_choking.store(choking, Ordering::Relaxed);
    }

    pub fn set_peer_choking(&mut self, choking: bool) {
        self.peer_choking = choking;
        self.stats.peer_choking.store(choking, Ordering::Relaxed);
    }

    pub fn set_snubbed(&mut self, snubbed: bool) {
        self.snubbed = snubbed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_if_not_seen_dedups() {
        let states = PeerStates::default();
        let peer = "127.0.0.1:6881".parse().unwrap();
        assert!(states.add_if_not_seen(peer));
        assert!(!states.add_if_not_seen(peer));
        assert_eq!(states.states.len(), 1);
    }

    #[test]
    fn insert_live_upgrades_unwired_seen_peer() {
        let states = PeerStates::default();
        let peer = "127.0.0.1:6881".parse().unwrap();
        assert!(states.add_if_not_seen(peer));
        let (tx, _rx) = flume::unbounded();
        assert!(states.insert_live(peer, tx.clone()));
        assert!(states.states.get(&peer).unwrap().writer_tx.is_some());
        assert!(!states.insert_live(peer, tx));
    }

    #[test]
    fn sole_contributor_is_banned_immediately() {
        let states = PeerStates::default();
        let peer = "127.0.0.1:6881".parse().unwrap();
        assert!(states.record_hash_failure(peer, true));
        assert!(states.is_banned(peer));
        assert_eq!(states.banned_count(), 1);
        assert!(!states.add_if_not_seen(peer));
        let other_port: PeerAddr = "127.0.0.1:9999".parse().unwrap();
        assert!(!states.is_banned(other_port));
        assert!(states.add_if_not_seen(other_port));
    }

    #[test]
    fn shared_contributors_banned_after_three_failures() {
        let states = PeerStates::default();
        let peer = "10.0.0.2:6881".parse().unwrap();
        assert!(!states.record_hash_failure(peer, false));
        assert!(!states.record_hash_failure(peer, false));
        assert!(!states.is_banned(peer));
        assert_eq!(states.hash_failures(peer), 2);
        assert!(states.record_hash_failure(peer, false));
        assert!(states.is_banned(peer));
        assert_eq!(states.hash_failures(peer), 3);
    }

    #[test]
    fn expired_bans_are_purged() {
        let states = PeerStates::default();
        let peer = "192.168.1.8:6881".parse().unwrap();
        states
            .banned
            .insert(peer, Instant::now() - Duration::from_secs(1));
        assert!(!states.is_banned(peer));
        assert_eq!(states.banned_count(), 0);
        assert!(states.add_if_not_seen(peer));
    }
}
