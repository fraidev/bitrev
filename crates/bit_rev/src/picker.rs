use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::Rng;

use crate::bitfield::Bitfield;

pub const BOOTSTRAP_VERIFIED: usize = 4;
pub const MAX_BLOCK_REQUESTERS: usize = 2;

pub struct Availability {
    pub counts: Vec<AtomicU16>,
}

impl Availability {
    pub fn new(piece_count: usize) -> Self {
        Self {
            counts: (0..piece_count).map(|_| AtomicU16::new(0)).collect(),
        }
    }

    pub fn piece_count(&self) -> usize {
        self.counts.len()
    }

    pub fn count(&self, index: u32) -> u16 {
        self.counts
            .get(index as usize)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    pub fn add_bitfield(&self, bitfield: &Bitfield) {
        for (index, slot) in self.counts.iter().enumerate() {
            if bitfield.has_piece(index) {
                saturating_inc(slot);
            }
        }
    }

    pub fn remove_bitfield(&self, bitfield: &Bitfield) {
        for (index, slot) in self.counts.iter().enumerate() {
            if bitfield.has_piece(index) {
                saturating_dec(slot);
            }
        }
    }

    pub fn add_have(&self, index: u32) {
        if let Some(slot) = self.counts.get(index as usize) {
            saturating_inc(slot);
        }
    }

    pub fn add_have_all(&self) {
        for slot in &self.counts {
            saturating_inc(slot);
        }
    }
}

fn saturating_inc(slot: &AtomicU16) {
    slot.update(Ordering::Relaxed, Ordering::Relaxed, |c| {
        c.saturating_add(1)
    });
}

fn saturating_dec(slot: &AtomicU16) {
    slot.update(Ordering::Relaxed, Ordering::Relaxed, |c| {
        c.saturating_sub(1)
    });
}

pub fn select_piece(
    candidates: &[u32],
    prefer: &[u32],
    availability: &Availability,
    verified: usize,
    bootstrap_until: usize,
    rng: &mut impl Rng,
) -> Option<u32> {
    if candidates.is_empty() {
        return None;
    }
    if verified < bootstrap_until {
        return candidates.choose(rng).copied();
    }
    for &index in prefer {
        if candidates.contains(&index) {
            return Some(index);
        }
    }
    let mut best_avail = u16::MAX;
    let mut best = Vec::new();
    for &index in candidates {
        let avail = availability.count(index);
        if avail < best_avail {
            best_avail = avail;
            best.clear();
            best.push(index);
        } else if avail == best_avail {
            best.push(index);
        }
    }
    best.choose(rng).copied()
}

pub fn seeded_rng(seed: u64) -> StdRng {
    use rand::SeedableRng;
    StdRng::seed_from_u64(seed)
}

/// Pieces verified so far. Shared so pick and tests can read it without walking the table.
pub struct VerifiedCount(AtomicUsize);

impl VerifiedCount {
    pub fn new(n: usize) -> Self {
        Self(AtomicUsize::new(n))
    }

    pub fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec(&self) {
        self.0.update(Ordering::Relaxed, Ordering::Relaxed, |c| {
            c.saturating_sub(1)
        });
    }

    pub fn set(&self, n: usize) {
        self.0.store(n, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(n: usize) -> Bitfield {
        Bitfield::filled(n)
    }

    fn bits(n: usize, set: &[usize]) -> Bitfield {
        let mut bf = Bitfield::with_piece_count(n);
        for &i in set {
            bf.set_piece(i);
        }
        bf
    }

    #[test]
    fn bitfield_increments_set_bits() {
        let avail = Availability::new(8);
        avail.add_bitfield(&bits(8, &[0, 2, 7]));
        assert_eq!(avail.count(0), 1);
        assert_eq!(avail.count(1), 0);
        assert_eq!(avail.count(2), 1);
        assert_eq!(avail.count(7), 1);
    }

    #[test]
    fn have_all_increments_every_piece() {
        let avail = Availability::new(4);
        avail.add_have_all();
        for i in 0..4 {
            assert_eq!(avail.count(i), 1);
        }
    }

    #[test]
    fn have_none_does_not_increment() {
        let avail = Availability::new(4);
        avail.add_bitfield(&Bitfield::with_piece_count(4));
        for i in 0..4 {
            assert_eq!(avail.count(i), 0);
        }
    }

    #[test]
    fn have_increments_once() {
        let avail = Availability::new(3);
        avail.add_have(1);
        avail.add_have(1);
        assert_eq!(avail.count(0), 0);
        assert_eq!(avail.count(1), 2);
        assert_eq!(avail.count(2), 0);
    }

    #[test]
    fn disconnect_decrements_stored_bitfield() {
        let avail = Availability::new(4);
        let bf = bits(4, &[0, 3]);
        avail.add_bitfield(&bf);
        avail.add_bitfield(&filled(4));
        assert_eq!(avail.count(0), 2);
        assert_eq!(avail.count(1), 1);
        avail.remove_bitfield(&bf);
        assert_eq!(avail.count(0), 1);
        assert_eq!(avail.count(1), 1);
        assert_eq!(avail.count(3), 1);
    }

    #[test]
    fn never_goes_negative() {
        let avail = Availability::new(2);
        avail.remove_bitfield(&filled(2));
        avail.remove_bitfield(&bits(2, &[0]));
        assert_eq!(avail.count(0), 0);
        assert_eq!(avail.count(1), 0);
        avail.add_have(0);
        avail.remove_bitfield(&filled(2));
        assert_eq!(avail.count(0), 0);
        assert_eq!(avail.count(1), 0);
    }

    #[test]
    fn rarest_piece_chosen_first() {
        let avail = Availability::new(5);
        for _ in 0..5 {
            avail.add_bitfield(&bits(5, &[0, 1, 2, 4]));
        }
        avail.add_have(3);
        let candidates = [0u32, 1, 2, 3, 4];
        let mut rng = seeded_rng(1);
        let picked = select_piece(&candidates, &[], &avail, 4, 4, &mut rng);
        assert_eq!(picked, Some(3));
    }

    #[test]
    fn prefer_list_wins_over_rarity() {
        let avail = Availability::new(4);
        avail.add_have(0);
        for _ in 0..8 {
            avail.add_have(1);
            avail.add_have(2);
            avail.add_have(3);
        }
        let candidates = [0u32, 1, 2, 3];
        let mut rng = seeded_rng(7);
        let picked = select_piece(&candidates, &[2, 1], &avail, 4, 4, &mut rng);
        assert_eq!(picked, Some(2));
    }

    #[test]
    fn seeded_ties_are_deterministic() {
        let avail = Availability::new(6);
        for i in 0..6 {
            avail.add_have(i);
            avail.add_have(i);
        }
        let candidates = [0u32, 1, 2, 3, 4, 5];
        let mut a = Vec::new();
        let mut remaining = candidates.to_vec();
        let mut rng = seeded_rng(42);
        while !remaining.is_empty() {
            let pick = select_piece(&remaining, &[], &avail, 4, 4, &mut rng).unwrap();
            remaining.retain(|i| *i != pick);
            a.push(pick);
        }
        let mut b = Vec::new();
        let mut remaining = candidates.to_vec();
        let mut rng = seeded_rng(42);
        while !remaining.is_empty() {
            let pick = select_piece(&remaining, &[], &avail, 4, 4, &mut rng).unwrap();
            remaining.retain(|i| *i != pick);
            b.push(pick);
        }
        assert_eq!(a, b);
        assert_ne!(a, candidates.to_vec());
    }

    #[test]
    fn bootstrap_picks_uniformly_until_threshold() {
        let avail = Availability::new(5);
        avail.add_have(0);
        for _ in 0..10 {
            avail.add_have(1);
            avail.add_have(2);
            avail.add_have(3);
            avail.add_have(4);
        }
        let candidates = [0u32, 1, 2, 3, 4];
        let mut rng = seeded_rng(99);
        let mut saw_non_rarest = false;
        for _ in 0..40 {
            let pick = select_piece(&candidates, &[], &avail, 0, 4, &mut rng).unwrap();
            if pick != 0 {
                saw_non_rarest = true;
                break;
            }
        }
        assert!(
            saw_non_rarest,
            "bootstrap should not lock onto the rarest piece"
        );
        let mut rng = seeded_rng(99);
        assert_eq!(
            select_piece(&candidates, &[], &avail, 4, 4, &mut rng),
            Some(0)
        );
    }

    #[test]
    fn empty_candidates_yield_none() {
        let avail = Availability::new(1);
        let mut rng = seeded_rng(1);
        assert!(select_piece(&[], &[], &avail, 4, 4, &mut rng).is_none());
    }
}
