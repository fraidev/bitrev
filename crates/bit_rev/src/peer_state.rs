use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::Notify;

use crate::{bitfield::Bitfield, message::WriterRequest, peer::PeerAddr};

#[derive(Debug, Default)]
pub struct PeerStates {
    pub states: DashMap<PeerAddr, PeerState>,
}

impl PeerStates {
    pub fn add_if_not_seen(&self, peer: PeerAddr) -> bool {
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
        use dashmap::mapref::entry::Entry;
        match self.states.entry(peer) {
            Entry::Occupied(_) => false,
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
    pub peer_allowed_fast: HashSet<u32>,
    pub our_allowed_fast: HashSet<u32>,
    pub suggested_pieces: Vec<u32>,
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
            peer_allowed_fast: HashSet::new(),
            our_allowed_fast: HashSet::new(),
            suggested_pieces: Vec::new(),
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
}
