//! Magnet URI parser (BEP-0009 appendix).
//!
//! Tracker announce is deferred until metadata arrives. Before that `left` is
//! unknown, so we do not announce `event=completed`. After the info dict is
//! verified the usual announce loop runs with the real piece table.
//!
//! Repeated `tr` values become one BEP-0012 tier each (common practice).

use std::fmt;
use std::net::SocketAddr;

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Magnet {
    pub info_hash: [u8; 20],
    pub display_name: Option<String>,
    pub trackers: Vec<String>,
    pub peers: Vec<SocketAddr>,
    /// Web seed URLs (`ws=`). Stored for #43; not consumed here.
    pub webseeds: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MagnetError {
    #[error("magnet link is missing xt=urn:btih:<info-hash>")]
    MissingXt,
    #[error("BitTorrent v2 magnets (urn:btmh) are not supported")]
    V2NotSupported,
    #[error("info hash has {got} characters, expected 40 hex or 32 base32")]
    BadHashLength { got: usize },
    #[error("info hash is not valid hex")]
    BadHex,
    #[error("info hash is not valid base32")]
    BadBase32,
    #[error("not a magnet URI")]
    InvalidUri,
}

impl Magnet {
    pub fn parse(input: &str) -> Result<Self, MagnetError> {
        let rest = strip_magnet_prefix(input).ok_or(MagnetError::InvalidUri)?;
        let query = match rest.split_once('?') {
            Some(("", query)) => query,
            Some((_, query)) => query,
            None => rest,
        };
        if query.is_empty() {
            return Err(MagnetError::MissingXt);
        }

        let mut info_hash = None;
        let mut saw_v2 = false;
        let mut display_name = None;
        let mut trackers = Vec::new();
        let mut peers = Vec::new();
        let mut webseeds = Vec::new();

        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (raw_key, raw_value) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            let key = percent_decode(raw_key);
            let value = percent_decode(raw_value);
            match key.to_ascii_lowercase().as_str() {
                "xt" => match parse_xt(&value) {
                    Ok(hash) if info_hash.is_none() => info_hash = Some(hash),
                    Ok(_) => {}
                    Err(MagnetError::V2NotSupported) => saw_v2 = true,
                    Err(err) => return Err(err),
                },
                "dn" if display_name.is_none() && !value.is_empty() => {
                    display_name = Some(value);
                }
                "tr" if !value.is_empty() => trackers.push(value),
                "x.pe" => {
                    if let Some(addr) = parse_peer(&value) {
                        peers.push(addr);
                    }
                }
                "ws" if !value.is_empty() => webseeds.push(value),
                _ => {}
            }
        }

        match info_hash {
            Some(info_hash) => Ok(Self {
                info_hash,
                display_name,
                trackers,
                peers,
                webseeds,
            }),
            None if saw_v2 => Err(MagnetError::V2NotSupported),
            None => Err(MagnetError::MissingXt),
        }
    }

    pub fn name_or_hash(&self) -> String {
        self.display_name
            .clone()
            .unwrap_or_else(|| hex_info_hash(&self.info_hash))
    }
}

pub fn hex_info_hash(info_hash: &[u8; 20]) -> String {
    info_hash.iter().map(|b| format!("{b:02x}")).collect()
}

fn strip_magnet_prefix(input: &str) -> Option<&str> {
    let rest = input.strip_prefix("magnet:")?;
    Some(rest.strip_prefix("//").unwrap_or(rest))
}

fn parse_xt(value: &str) -> Result<[u8; 20], MagnetError> {
    let lower = value.to_ascii_lowercase();
    if let Some(hash) = lower.strip_prefix("urn:btmh:") {
        let _ = hash;
        return Err(MagnetError::V2NotSupported);
    }
    let Some(hash) = lower.strip_prefix("urn:btih:") else {
        return Err(MagnetError::MissingXt);
    };
    match hash.len() {
        40 => decode_hex(hash),
        32 => decode_base32(hash),
        n => Err(MagnetError::BadHashLength { got: n }),
    }
}

fn parse_peer(value: &str) -> Option<SocketAddr> {
    value.parse().ok()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) =
                (from_hex_digit(bytes[i + 1]), from_hex_digit(bytes[i + 2]))
            {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

fn decode_hex(input: &str) -> Result<[u8; 20], MagnetError> {
    if input.len() != 40 {
        return Err(MagnetError::BadHashLength { got: input.len() });
    }
    let mut out = [0u8; 20];
    let bytes = input.as_bytes();
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = from_hex_digit(bytes[i * 2]).ok_or(MagnetError::BadHex)?;
        let lo = from_hex_digit(bytes[i * 2 + 1]).ok_or(MagnetError::BadHex)?;
        *slot = (hi << 4) | lo;
    }
    Ok(out)
}

/// RFC 4648 base32 (`A-Z2-7`), case-insensitive, no padding. 32 chars = 20 bytes.
fn decode_base32(input: &str) -> Result<[u8; 20], MagnetError> {
    if input.len() != 32 {
        return Err(MagnetError::BadHashLength { got: input.len() });
    }
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = [0u8; 20];
    let mut idx = 0usize;
    for raw in input.bytes() {
        let val = match raw.to_ascii_uppercase() {
            b'A'..=b'Z' => raw.to_ascii_uppercase() - b'A',
            b'2'..=b'7' => raw.to_ascii_uppercase() - b'2' + 26,
            _ => return Err(MagnetError::BadBase32),
        };
        acc = (acc << 5) | u32::from(val);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            if idx >= 20 {
                return Err(MagnetError::BadBase32);
            }
            out[idx] = (acc >> bits) as u8;
            idx += 1;
            acc &= (1 << bits) - 1;
        }
    }
    if idx != 20 {
        return Err(MagnetError::BadBase32);
    }
    Ok(out)
}

fn from_hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for Magnet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "magnet:?xt=urn:btih:{}", hex_info_hash(&self.info_hash))?;
        if let Some(name) = &self.display_name {
            write!(f, "&dn={}", percent_encode(name.as_bytes()))?;
        }
        for tr in &self.trackers {
            write!(f, "&tr={}", percent_encode(tr.as_bytes()))?;
        }
        for peer in &self.peers {
            write!(f, "&x.pe={peer}")?;
        }
        for ws in &self.webseeds {
            write!(f, "&ws={}", percent_encode(ws.as_bytes()))?;
        }
        Ok(())
    }
}

fn percent_encode(bytes: &[u8]) -> String {
    crate::file::url_encode_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: [u8; 20] = [
        0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
        0x88, 0x99, 0xaa, 0xbb, 0xcc,
    ];
    const HASH_HEX: &str = "123456789abcdef0112233445566778899aabbcc";

    fn encode_base32(bytes: &[u8; 20]) -> String {
        const ALPH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut acc: u32 = 0;
        let mut bits = 0u32;
        let mut out = String::new();
        for &b in bytes {
            acc = (acc << 8) | u32::from(b);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(ALPH[((acc >> bits) & 31) as usize] as char);
                acc &= (1 << bits) - 1;
            }
        }
        if bits > 0 {
            out.push(ALPH[((acc << (5 - bits)) & 31) as usize] as char);
        }
        out
    }

    #[test]
    fn hex_btih() {
        let magnet = Magnet::parse(&format!("magnet:?xt=urn:btih:{HASH_HEX}")).unwrap();
        assert_eq!(magnet.info_hash, HASH);
        assert!(magnet.display_name.is_none());
        assert!(magnet.trackers.is_empty());
    }

    #[test]
    fn base32_upper_and_lower_match_hex() {
        let encoded = encode_base32(&HASH);
        assert_eq!(encoded.len(), 32);
        let upper = Magnet::parse(&format!("magnet:?xt=urn:btih:{encoded}")).unwrap();
        let lower = Magnet::parse(&format!(
            "magnet:?xt=urn:btih:{}",
            encoded.to_ascii_lowercase()
        ))
        .unwrap();
        let hex = Magnet::parse(&format!("magnet:?xt=urn:btih:{HASH_HEX}")).unwrap();
        assert_eq!(upper.info_hash, HASH);
        assert_eq!(lower.info_hash, hex.info_hash);
        assert_eq!(upper.info_hash, lower.info_hash);
    }

    #[test]
    fn dn_tr_xpe_ws_combinations() {
        let uri = format!(
            "magnet:?xt=urn:btih:{HASH_HEX}&dn=Some%20Name&tr=udp%3A%2F%2Fa.example%3A80%2Fannounce&tr=http%3A%2F%2Fb.example%2Fannounce&x.pe=127.0.0.1%3A51413&x.pe=[::1]:6881&ws=https%3A%2F%2Fcdn.example%2Ffile&unknown=1"
        );
        let magnet = Magnet::parse(&uri).unwrap();
        assert_eq!(magnet.display_name.as_deref(), Some("Some Name"));
        assert_eq!(
            magnet.trackers,
            ["udp://a.example:80/announce", "http://b.example/announce"]
        );
        assert_eq!(
            magnet.peers,
            [
                "127.0.0.1:51413".parse().unwrap(),
                "[::1]:6881".parse().unwrap()
            ]
        );
        assert_eq!(magnet.webseeds, ["https://cdn.example/file"]);
    }

    #[test]
    fn first_btih_wins_among_several_xt() {
        let other = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let uri = format!("magnet:?xt=urn:btih:{HASH_HEX}&xt=urn:btih:{other}");
        let magnet = Magnet::parse(&uri).unwrap();
        assert_eq!(magnet.info_hash, HASH);
    }

    #[test]
    fn btih_preferred_over_btmh() {
        let uri = format!("magnet:?xt=urn:btmh:1220abcd&xt=urn:btih:{HASH_HEX}");
        let magnet = Magnet::parse(&uri).unwrap();
        assert_eq!(magnet.info_hash, HASH);
    }

    #[test]
    fn v2_only_is_rejected() {
        let err = Magnet::parse("magnet:?xt=urn:btmh:1220abcd").unwrap_err();
        assert_eq!(err, MagnetError::V2NotSupported);
        assert!(err.to_string().contains("v2"));
    }

    #[test]
    fn missing_xt() {
        assert_eq!(
            Magnet::parse("magnet:?dn=only-a-name").unwrap_err(),
            MagnetError::MissingXt
        );
        assert_eq!(
            Magnet::parse("magnet:?").unwrap_err(),
            MagnetError::MissingXt
        );
    }

    #[test]
    fn bad_hash_length() {
        assert!(matches!(
            Magnet::parse("magnet:?xt=urn:btih:abc").unwrap_err(),
            MagnetError::BadHashLength { got: 3 }
        ));
        assert!(matches!(
            Magnet::parse("magnet:?xt=urn:btih:1234567890abcdef1234567890abcdef1234567890")
                .unwrap_err(),
            MagnetError::BadHashLength { got: 42 }
        ));
    }

    #[test]
    fn bad_hex_and_base32() {
        assert_eq!(
            Magnet::parse("magnet:?xt=urn:btih:zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz")
                .unwrap_err(),
            MagnetError::BadHex
        );
        assert_eq!(
            Magnet::parse("magnet:?xt=urn:btih:018ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ").unwrap_err(),
            MagnetError::BadBase32
        );
    }

    #[test]
    fn garbage_is_invalid() {
        assert_eq!(
            Magnet::parse("http://example").unwrap_err(),
            MagnetError::InvalidUri
        );
        assert_eq!(Magnet::parse("").unwrap_err(), MagnetError::InvalidUri);
        assert_eq!(
            Magnet::parse("not-a-magnet").unwrap_err(),
            MagnetError::InvalidUri
        );
    }

    #[test]
    fn magnet_slash_slash_form() {
        let magnet = Magnet::parse(&format!("magnet://?xt=urn:btih:{HASH_HEX}")).unwrap();
        assert_eq!(magnet.info_hash, HASH);
    }

    #[test]
    fn xt_is_case_insensitive() {
        let magnet = Magnet::parse(&format!("magnet:?XT=URN:BTIH:{HASH_HEX}")).unwrap();
        assert_eq!(magnet.info_hash, HASH);
    }
}
