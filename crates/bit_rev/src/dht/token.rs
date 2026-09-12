//! Announce tokens: SHA1(secret || requester IP), secret rotates every 5 minutes.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use rand::Rng;

pub const TOKEN_ROTATE: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone)]
pub struct TokenSecrets {
    current: [u8; 16],
    previous: [u8; 16],
    rotated_at: Instant,
}

impl TokenSecrets {
    pub fn new(now: Instant) -> Self {
        let mut current = [0u8; 16];
        let mut previous = [0u8; 16];
        rand::thread_rng().fill(&mut current);
        rand::thread_rng().fill(&mut previous);
        Self {
            current,
            previous,
            rotated_at: now,
        }
    }

    pub fn maybe_rotate(&mut self, now: Instant) {
        if now.saturating_duration_since(self.rotated_at) >= TOKEN_ROTATE {
            self.rotate(now);
        }
    }

    pub fn rotate(&mut self, now: Instant) {
        self.previous = self.current;
        rand::thread_rng().fill(&mut self.current);
        self.rotated_at = now;
    }

    pub fn issue(&self, ip: IpAddr) -> Vec<u8> {
        hash_token(&self.current, ip).to_vec()
    }

    pub fn validate(&self, ip: IpAddr, token: &[u8]) -> bool {
        let current = hash_token(&self.current, ip);
        let previous = hash_token(&self.previous, ip);
        token == current.as_slice() || token == previous.as_slice()
    }
}

fn hash_token(secret: &[u8; 16], ip: IpAddr) -> [u8; 20] {
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(secret);
    match ip {
        IpAddr::V4(v4) => hasher.update(&v4.octets()),
        IpAddr::V6(v6) => hasher.update(&v6.octets()),
    }
    hasher.digest().bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn issue_and_validate() {
        let now = Instant::now();
        let secrets = TokenSecrets::new(now);
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let token = secrets.issue(ip);
        assert!(secrets.validate(ip, &token));
        assert!(!secrets.validate(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), &token));
    }

    #[test]
    fn previous_secret_still_valid_after_one_rotate() {
        let now = Instant::now();
        let mut secrets = TokenSecrets::new(now);
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let token = secrets.issue(ip);
        secrets.rotate(now + TOKEN_ROTATE);
        assert!(secrets.validate(ip, &token));
        secrets.rotate(now + TOKEN_ROTATE * 2);
        assert!(!secrets.validate(ip, &token));
    }
}
