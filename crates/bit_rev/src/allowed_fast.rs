use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};

pub const DEFAULT_ALLOWED_FAST_SET_SIZE: usize = 10;

pub fn generate_allowed_fast(
    ip: Ipv4Addr,
    info_hash: &[u8; 20],
    piece_count: u32,
    k: usize,
) -> Vec<u32> {
    if piece_count == 0 || k == 0 {
        return Vec::new();
    }

    // BEP-0006 zeros the last IPv4 octet so nearby addresses share a set.
    let mut octets = ip.octets();
    octets[3] = 0;

    let mut x = Vec::with_capacity(24);
    x.extend_from_slice(&octets);
    x.extend_from_slice(info_hash);

    let target = k.min(piece_count as usize);
    let mut set = Vec::with_capacity(target);
    let mut seen = HashSet::with_capacity(target);

    while set.len() < target {
        let mut hasher = sha1_smol::Sha1::new();
        hasher.update(&x);
        let digest = hasher.digest().bytes();
        x.clear();
        x.extend_from_slice(&digest);

        for i in 0..5 {
            if set.len() >= target {
                break;
            }
            let j = i * 4;
            let y = u32::from_be_bytes([digest[j], digest[j + 1], digest[j + 2], digest[j + 3]]);
            let index = (y as u64 % piece_count as u64) as u32;
            if seen.insert(index) {
                set.push(index);
            }
        }
    }

    set
}

pub fn generate_allowed_fast_for_ip(
    ip: IpAddr,
    info_hash: &[u8; 20],
    piece_count: u32,
    k: usize,
) -> Vec<u32> {
    match ip {
        IpAddr::V4(v4) => generate_allowed_fast(v4, info_hash, piece_count, k),
        IpAddr::V6(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    fn bep6_info_hash() -> [u8; 20] {
        [0xaa; 20]
    }

    fn bep6_ip() -> Ipv4Addr {
        Ipv4Addr::new(80, 4, 4, 200)
    }

    #[test]
    fn bep6_seven_piece_set() {
        let set = generate_allowed_fast(bep6_ip(), &bep6_info_hash(), 1313, 7);
        assert_eq!(set, vec![1059, 431, 808, 1217, 287, 376, 1188]);
    }

    #[test]
    fn bep6_nine_piece_set() {
        let set = generate_allowed_fast(bep6_ip(), &bep6_info_hash(), 1313, 9);
        assert_eq!(set, vec![1059, 431, 808, 1217, 287, 376, 1188, 353, 508]);
    }

    #[test]
    fn empty_when_k_or_piece_count_is_zero() {
        let ip = bep6_ip();
        let hash = bep6_info_hash();
        assert!(generate_allowed_fast(ip, &hash, 1313, 0).is_empty());
        assert!(generate_allowed_fast(ip, &hash, 0, 10).is_empty());
        assert!(generate_allowed_fast(ip, &hash, 0, 0).is_empty());
    }

    #[test]
    fn ipv6_returns_empty() {
        let ip = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let set = generate_allowed_fast_for_ip(ip, &bep6_info_hash(), 1313, 10);
        assert!(set.is_empty());
    }

    #[test]
    fn k_larger_than_piece_count_yields_at_most_piece_count_unique() {
        let set = generate_allowed_fast(bep6_ip(), &bep6_info_hash(), 5, 20);
        assert_eq!(set.len(), 5);
        let mut unique = set.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 5);
        assert!(set.iter().all(|&index| index < 5));
    }
}
