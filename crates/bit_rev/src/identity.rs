use rand::Rng;

/// Product name used in HTTP User-Agent and the BEP-0010 handshake `v` field.
pub const CLIENT_NAME: &str = "bitrev";

/// Azureus-style two-character client code (BEP-0020).
pub const CLIENT_CODE: [u8; 2] = *b"BR";

pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// HTTP User-Agent, e.g. `bitrev/0.1.0`.
pub fn user_agent() -> String {
    format!("{CLIENT_NAME}/{CLIENT_VERSION}")
}

/// BEP-0010 handshake `v` value, e.g. `bitrev 0.1.0`.
pub fn extension_version() -> String {
    format!("{CLIENT_NAME} {CLIENT_VERSION}")
}

/// Azureus-style 8-byte peer id prefix: `-BRxyzw-`.
pub fn peer_id_prefix() -> [u8; 8] {
    let version = azureus_version(CLIENT_VERSION);
    [
        b'-',
        CLIENT_CODE[0],
        CLIENT_CODE[1],
        version[0],
        version[1],
        version[2],
        version[3],
        b'-',
    ]
}

/// Azureus xyzw: 1-digit major, minor, patch, then 0. 0.1.0 -> 0100.
pub fn azureus_version(version: &str) -> [u8; 4] {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = core.split('.');
    let major = parse_component(parts.next());
    let minor = parse_component(parts.next());
    let patch = parse_component(parts.next());
    [
        b'0' + (major % 10) as u8,
        b'0' + (minor % 10) as u8,
        b'0' + (patch % 10) as u8,
        b'0',
    ]
}

fn parse_component(part: Option<&str>) -> u32 {
    part.unwrap_or("0").parse().unwrap_or(0)
}

/// Random announce `key` (BEP-0003). Stable for one tracker URL, rotated on switch.
pub fn random_announce_key() -> u32 {
    rand::thread_rng().gen()
}

/// Per-tracker announce identity. A new `key` is minted when the URL changes (BEP-0027).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerIdentity {
    url: String,
    key: u32,
}

impl TrackerIdentity {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            key: random_announce_key(),
        }
    }

    pub fn with_key(url: impl Into<String>, key: u32) -> Self {
        Self {
            url: url.into(),
            key,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn key(&self) -> u32 {
        self.key
    }

    /// Switch to a different tracker URL and mint a fresh `key`.
    ///
    /// Issue #19 (tier fallback) should call this instead of reusing the old key.
    pub fn switch_url(&mut self, url: impl Into<String>) -> bool {
        let url = url.into();
        if url == self.url {
            return false;
        }
        self.url = url;
        self.key = random_announce_key();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_strings_share_name_and_version() {
        assert_eq!(user_agent(), format!("{CLIENT_NAME}/{CLIENT_VERSION}"));
        assert_eq!(
            extension_version(),
            format!("{CLIENT_NAME} {CLIENT_VERSION}")
        );
        assert_eq!(&peer_id_prefix(), b"-BR0100-");
    }

    #[test]
    fn azureus_version_encodes_major_minor_patch() {
        assert_eq!(azureus_version("0.1.0"), *b"0100");
        assert_eq!(azureus_version("1.2.3"), *b"1230");
        assert_eq!(azureus_version("0.1.0-alpha"), *b"0100");
    }

    #[test]
    fn switch_url_mints_a_fresh_key() {
        let mut identity = TrackerIdentity::with_key("http://a.example/announce", 1);
        assert!(!identity.switch_url("http://a.example/announce"));
        assert_eq!(identity.key(), 1);

        assert!(identity.switch_url("http://b.example/announce"));
        assert_eq!(identity.url(), "http://b.example/announce");
        assert_ne!(identity.key(), 1);
    }
}
