use std::sync::OnceLock;

use num_bigint::BigUint;
use num_traits::Zero;
use rand::rngs::OsRng;
use rand::RngCore;

/// RFC 2412 768-bit MODP group. This is the prime every MSE client uses.
pub const MSE_PRIME: [u8; 96] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xC9, 0x0F, 0xDA, 0xA2, 0x21, 0x68, 0xC2, 0x34,
    0xC4, 0xC6, 0x62, 0x8B, 0x80, 0xDC, 0x1C, 0xD1, 0x29, 0x02, 0x4E, 0x08, 0x8A, 0x67, 0xCC, 0x74,
    0x02, 0x0B, 0xBE, 0xA6, 0x3B, 0x13, 0x9B, 0x22, 0x51, 0x4A, 0x08, 0x79, 0x8E, 0x34, 0x04, 0xDD,
    0xEF, 0x95, 0x19, 0xB3, 0xCD, 0x3A, 0x43, 0x1B, 0x30, 0x2B, 0x0A, 0x6D, 0xF2, 0x5F, 0x14, 0x37,
    0x4F, 0xE1, 0x35, 0x6D, 0x6D, 0x51, 0xC2, 0x45, 0xE4, 0x85, 0xB5, 0x76, 0x62, 0x5E, 0x7E, 0xC6,
    0xF4, 0x4C, 0x42, 0xE9, 0xA6, 0x3A, 0x36, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x05, 0x63,
];

pub const PUBLIC_KEY_LEN: usize = 96;
pub const PRIVATE_KEY_LEN: usize = 20;

fn prime() -> &'static BigUint {
    static PRIME: OnceLock<BigUint> = OnceLock::new();
    PRIME.get_or_init(|| BigUint::from_bytes_be(&MSE_PRIME))
}

pub struct DhKeys {
    private: BigUint,
    pub public: [u8; PUBLIC_KEY_LEN],
}

impl DhKeys {
    pub fn generate() -> Self {
        let private = random_private();
        let public = modpow_g2(&private);
        Self { private, public }
    }

    pub fn shared_secret(&self, peer_public: &[u8; PUBLIC_KEY_LEN]) -> [u8; PUBLIC_KEY_LEN] {
        let peer = BigUint::from_bytes_be(peer_public);
        let secret = peer.modpow(&self.private, prime());
        pad_96(&secret)
    }
}

fn random_private() -> BigUint {
    let mut bytes = [0u8; PRIVATE_KEY_LEN];
    loop {
        OsRng.fill_bytes(&mut bytes);
        let n = BigUint::from_bytes_be(&bytes);
        if !n.is_zero() {
            return n;
        }
    }
}

fn modpow_g2(private: &BigUint) -> [u8; PUBLIC_KEY_LEN] {
    let y = BigUint::from(2u32).modpow(private, prime());
    pad_96(&y)
}

fn pad_96(n: &BigUint) -> [u8; PUBLIC_KEY_LEN] {
    let bytes = n.to_bytes_be();
    let mut out = [0u8; PUBLIC_KEY_LEN];
    if bytes.len() >= PUBLIC_KEY_LEN {
        out.copy_from_slice(&bytes[bytes.len() - PUBLIC_KEY_LEN..]);
    } else {
        out[PUBLIC_KEY_LEN - bytes.len()..].copy_from_slice(&bytes);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::DhKeys;

    #[test]
    fn both_sides_derive_the_same_secret() {
        let a = DhKeys::generate();
        let b = DhKeys::generate();
        let sa = a.shared_secret(&b.public);
        let sb = b.shared_secret(&a.public);
        assert_eq!(sa, sb);
        assert_ne!(sa, [0u8; 96]);
        assert_ne!(a.public, b.public);
    }
}
