use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use rand::Rng;

pub const REGULAR_UNCHOKE_SLOTS: usize = 3;
pub const CHOKE_INTERVAL: Duration = Duration::from_secs(10);
pub const OPTIMISTIC_INTERVAL: Duration = Duration::from_secs(30);
pub const NEW_PEER_BIAS_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChokeAction {
    Choke,
    Unchoke,
}

#[derive(Debug, Clone)]
pub struct ChokePeer {
    pub addr: SocketAddr,
    pub peer_interested: bool,
    pub currently_unchoked: bool,
    /// Bytes downloaded FROM this peer since the last choke interval (leeching metric).
    pub download_bytes: u64,
    pub last_unchoked: Option<Instant>,
    pub connected_at: Instant,
    pub is_optimistic: bool,
}

/// Compute the set of peers that should be unchoked.
///
/// - Only interested peers are eligible.
/// - While `seeding` is false (leeching): pick up to REGULAR_UNCHOKE_SLOTS interested peers
///   with the highest `download_bytes` (reciprocation). Ties broken by addr for determinism.
/// - While `seeding` is true: pick up to REGULAR_UNCHOKE_SLOTS interested peers with the
///   oldest `last_unchoked` (None = never = oldest). Ties broken by addr.
/// - If `pick_optimistic` is true, add one extra optimistic unchoke chosen uniformly
///   among remaining choked interested peers, except newly-connected peers
///   (`now.saturating_duration_since(connected_at) < NEW_PEER_BIAS_WINDOW`) are 3x more
///   likely (add 3 tickets vs 1). If `pick_optimistic` is false, keep the previous
///   optimistic peer (the one with `is_optimistic == true`) if they are still interested
///   and not already in the regular set.
/// - Uninterested peers are never unchoked.
pub fn select_unchoked<R: Rng + ?Sized>(
    peers: &[ChokePeer],
    seeding: bool,
    pick_optimistic: bool,
    now: Instant,
    rng: &mut R,
) -> HashSet<SocketAddr> {
    let mut interested: Vec<&ChokePeer> = peers.iter().filter(|p| p.peer_interested).collect();

    if seeding {
        interested.sort_by(|a, b| match (a.last_unchoked, b.last_unchoked) {
            (None, None) => a.addr.cmp(&b.addr),
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(ta), Some(tb)) => ta.cmp(&tb).then(a.addr.cmp(&b.addr)),
        });
    } else {
        interested.sort_by(|a, b| {
            b.download_bytes
                .cmp(&a.download_bytes)
                .then(a.addr.cmp(&b.addr))
        });
    }

    let mut unchoked: HashSet<SocketAddr> = interested
        .iter()
        .take(REGULAR_UNCHOKE_SLOTS)
        .map(|p| p.addr)
        .collect();

    if pick_optimistic {
        let remaining: Vec<&ChokePeer> = interested
            .iter()
            .copied()
            .filter(|p| !unchoked.contains(&p.addr))
            .collect();
        if let Some(addr) = pick_optimistic_peer(&remaining, now, rng) {
            unchoked.insert(addr);
        }
    } else {
        for peer in &interested {
            if peer.is_optimistic && !unchoked.contains(&peer.addr) {
                unchoked.insert(peer.addr);
            }
        }
    }

    unchoked
}

/// Diff previous unchoked set vs next. Emit Choke for peers that left, Unchoke for peers
/// that joined. Peers in both sets produce no action. Order: chokes first (sorted by addr),
/// then unchokes (sorted by addr). This is what "emit choke/unchoke only on transitions"
/// means, and tests assert exactly one message per change.
pub fn choke_transitions(
    previous: &HashSet<SocketAddr>,
    next: &HashSet<SocketAddr>,
) -> Vec<(SocketAddr, ChokeAction)> {
    let mut chokes: Vec<SocketAddr> = previous.difference(next).copied().collect();
    chokes.sort();
    let mut unchokes: Vec<SocketAddr> = next.difference(previous).copied().collect();
    unchokes.sort();

    let mut actions = Vec::with_capacity(chokes.len() + unchokes.len());
    actions.extend(chokes.into_iter().map(|addr| (addr, ChokeAction::Choke)));
    actions.extend(
        unchokes
            .into_iter()
            .map(|addr| (addr, ChokeAction::Unchoke)),
    );
    actions
}

fn pick_optimistic_peer<R: Rng + ?Sized>(
    candidates: &[&ChokePeer],
    now: Instant,
    rng: &mut R,
) -> Option<SocketAddr> {
    if candidates.is_empty() {
        return None;
    }

    let mut tickets = Vec::new();
    for (i, peer) in candidates.iter().enumerate() {
        let weight = if now.saturating_duration_since(peer.connected_at) < NEW_PEER_BIAS_WINDOW {
            3
        } else {
            1
        };
        tickets.extend(std::iter::repeat_n(i, weight));
    }

    let chosen = tickets[rng.gen_range(0..tickets.len())];
    Some(candidates[chosen].addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn test_peer(port: u16, download_bytes: u64) -> ChokePeer {
        ChokePeer {
            addr: addr(port),
            peer_interested: true,
            currently_unchoked: false,
            download_bytes,
            last_unchoked: None,
            connected_at: Instant::now() - Duration::from_secs(120),
            is_optimistic: false,
        }
    }

    fn rng() -> StdRng {
        StdRng::seed_from_u64(0xC10C4)
    }

    #[test]
    fn leeching_unchokes_top_downloaders() {
        let now = Instant::now();
        let peers = [
            test_peer(6881, 10),
            test_peer(6882, 50),
            test_peer(6883, 30),
            test_peer(6884, 5),
            test_peer(6885, 40),
        ];

        let unchoked = select_unchoked(&peers, false, false, now, &mut rng());

        assert_eq!(
            unchoked,
            HashSet::from([addr(6882), addr(6885), addr(6883)])
        );
    }

    #[test]
    fn seeding_round_robin_oldest_unchoke() {
        let now = Instant::now();
        let peers = [
            ChokePeer {
                last_unchoked: Some(now - Duration::from_secs(30)),
                ..test_peer(6881, 0)
            },
            ChokePeer {
                last_unchoked: Some(now - Duration::from_secs(10)),
                ..test_peer(6882, 0)
            },
            ChokePeer {
                last_unchoked: None,
                ..test_peer(6883, 0)
            },
            ChokePeer {
                last_unchoked: Some(now - Duration::from_secs(5)),
                ..test_peer(6884, 0)
            },
        ];

        let unchoked = select_unchoked(&peers, true, false, now, &mut rng());

        assert_eq!(
            unchoked,
            HashSet::from([addr(6883), addr(6881), addr(6882)])
        );
    }

    #[test]
    fn uninterested_never_unchoked() {
        let now = Instant::now();
        let peers = [
            ChokePeer {
                peer_interested: false,
                download_bytes: 1000,
                ..test_peer(6881, 1000)
            },
            test_peer(6882, 10),
            test_peer(6883, 20),
            test_peer(6884, 30),
            test_peer(6885, 5),
        ];

        let unchoked = select_unchoked(&peers, false, false, now, &mut rng());

        assert_eq!(
            unchoked,
            HashSet::from([addr(6884), addr(6883), addr(6882)])
        );
        assert!(!unchoked.contains(&addr(6881)));
    }

    #[test]
    fn optimistic_added_when_requested() {
        let now = Instant::now();
        let peers = [
            test_peer(6881, 10),
            test_peer(6882, 50),
            test_peer(6883, 30),
            test_peer(6884, 5),
            test_peer(6885, 40),
        ];
        let regular = HashSet::from([addr(6882), addr(6885), addr(6883)]);

        let unchoked = select_unchoked(&peers, false, true, now, &mut rng());

        assert_eq!(unchoked.len(), 4);
        assert!(regular.is_subset(&unchoked));
        let extra: Vec<_> = unchoked.difference(&regular).copied().collect();
        assert_eq!(extra.len(), 1);
        assert!(extra[0] == addr(6881) || extra[0] == addr(6884));
    }

    #[test]
    fn optimistic_kept_when_not_rotating() {
        let now = Instant::now();
        let peers = [
            test_peer(6881, 10),
            test_peer(6882, 50),
            test_peer(6883, 30),
            ChokePeer {
                is_optimistic: true,
                currently_unchoked: true,
                ..test_peer(6884, 5)
            },
            test_peer(6885, 40),
        ];

        let unchoked = select_unchoked(&peers, false, false, now, &mut rng());

        assert_eq!(
            unchoked,
            HashSet::from([addr(6882), addr(6885), addr(6883), addr(6884)])
        );
    }

    #[test]
    fn choke_transitions_emit_one_message_per_change() {
        let a = addr(6881);
        let b = addr(6882);
        let c = addr(6883);

        let previous = HashSet::from([a, b]);
        let next = HashSet::from([b, c]);
        assert_eq!(
            choke_transitions(&previous, &next),
            vec![(a, ChokeAction::Choke), (c, ChokeAction::Unchoke)]
        );

        assert!(choke_transitions(&previous, &previous).is_empty());

        assert_eq!(
            choke_transitions(&HashSet::new(), &HashSet::from([a])),
            vec![(a, ChokeAction::Unchoke)]
        );
    }

    #[test]
    fn no_interested_peers_empty_set() {
        let now = Instant::now();
        let peers = [
            ChokePeer {
                peer_interested: false,
                download_bytes: 100,
                ..test_peer(6881, 100)
            },
            ChokePeer {
                peer_interested: false,
                download_bytes: 200,
                is_optimistic: true,
                ..test_peer(6882, 200)
            },
        ];

        let unchoked = select_unchoked(&peers, false, true, now, &mut rng());
        assert!(unchoked.is_empty());

        let unchoked = select_unchoked(&[], false, true, now, &mut rng());
        assert!(unchoked.is_empty());
    }

    #[test]
    fn fewer_than_slots_unchokes_all_interested() {
        let now = Instant::now();
        let peers = [
            test_peer(6881, 10),
            test_peer(6882, 20),
            ChokePeer {
                peer_interested: false,
                ..test_peer(6883, 999)
            },
        ];

        let unchoked = select_unchoked(&peers, false, false, now, &mut rng());
        assert_eq!(unchoked, HashSet::from([addr(6881), addr(6882)]));

        let unchoked = select_unchoked(&peers, false, true, now, &mut rng());
        assert_eq!(unchoked, HashSet::from([addr(6881), addr(6882)]));
    }
}
