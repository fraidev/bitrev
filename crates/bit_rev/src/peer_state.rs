use dashmap::DashMap;

use crate::{bitfield::Bitfield, peer::PeerAddr};

#[derive(Debug, Clone, Default)]
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
}

#[derive(Debug, Clone)]
pub struct PeerState {
    /// This is used to track if the peer is interested in us.
    pub peer_interested: bool,
    /// This is used to track the pieces the peer has.
    pub bitfield: Bitfield,
}

impl Default for PeerState {
    fn default() -> Self {
        Self {
            peer_interested: true,
            bitfield: Bitfield::new(vec![]),
        }
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
