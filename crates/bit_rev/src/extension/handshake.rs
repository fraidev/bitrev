use std::collections::BTreeMap;

use serde_bencode::value::Value;

use crate::identity;

pub const DEFAULT_REQQ: i64 = 250;
pub const UT_METADATA: &str = "ut_metadata";
pub const MAX_METADATA_SIZE: i64 = 2 * 1024 * 1024;
pub const MAX_EXTENSION_PAYLOAD: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtensionHandshake {
    pub m: BTreeMap<String, i64>,
    pub v: Option<String>,
    pub p: Option<i64>,
    pub reqq: Option<i64>,
    pub metadata_size: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PeerExtensionInfo {
    pub m: BTreeMap<String, u8>,
    pub v: Option<String>,
    pub p: Option<u16>,
    pub reqq: Option<i64>,
    pub metadata_size: Option<i64>,
}

impl ExtensionHandshake {
    pub fn outgoing(
        m: BTreeMap<String, i64>,
        listen_port: Option<u16>,
        metadata_size: Option<i64>,
    ) -> Self {
        let include_metadata_size = m.get(UT_METADATA).copied().unwrap_or(0) != 0;
        Self {
            m,
            v: Some(identity::extension_version()),
            p: listen_port.map(i64::from),
            reqq: Some(DEFAULT_REQQ),
            metadata_size: if include_metadata_size {
                metadata_size
            } else {
                None
            },
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

        let m_pairs: Vec<(Vec<u8>, Vec<u8>)> = self
            .m
            .iter()
            .map(|(name, id)| (name.as_bytes().to_vec(), encode_int(*id)))
            .collect();
        pairs.push((b"m".to_vec(), encode_dict(&m_pairs)));

        if let Some(size) = self.metadata_size {
            pairs.push((b"metadata_size".to_vec(), encode_int(size)));
        }
        if let Some(p) = self.p {
            pairs.push((b"p".to_vec(), encode_int(p)));
        }
        if let Some(reqq) = self.reqq {
            pairs.push((b"reqq".to_vec(), encode_int(reqq)));
        }
        if let Some(v) = &self.v {
            pairs.push((b"v".to_vec(), encode_bytes(v.as_bytes())));
        }

        encode_dict(&pairs)
    }

    pub fn decode(bytes: &[u8]) -> Self {
        if bytes.len() > MAX_EXTENSION_PAYLOAD {
            return Self::default();
        }
        if crate::file::check_bencode_depth(bytes).is_err() {
            return Self::default();
        }
        let Ok(Value::Dict(dict)) = serde_bencode::from_bytes::<Value>(bytes) else {
            return Self::default();
        };

        let mut handshake = Self::default();
        if let Some(Value::Dict(m)) = dict.get(&b"m"[..]) {
            for (name, value) in m {
                let Ok(name) = std::str::from_utf8(name) else {
                    continue;
                };
                if let Value::Int(id) = value {
                    handshake.m.insert(name.to_string(), *id);
                }
            }
        }
        if let Some(Value::Bytes(v)) = dict.get(&b"v"[..]) {
            handshake.v = String::from_utf8(v.clone()).ok();
        }
        if let Some(Value::Int(p)) = dict.get(&b"p"[..]) {
            handshake.p = Some(*p);
        }
        if let Some(Value::Int(reqq)) = dict.get(&b"reqq"[..]) {
            handshake.reqq = Some(*reqq);
        }
        if let Some(Value::Int(size)) = dict.get(&b"metadata_size"[..]) {
            if (0..=MAX_METADATA_SIZE).contains(size) {
                handshake.metadata_size = Some(*size);
            }
        }
        handshake
    }

    pub fn into_peer_info(self) -> PeerExtensionInfo {
        let mut info = PeerExtensionInfo::default();
        info.merge(&self);
        info
    }
}

impl PeerExtensionInfo {
    pub fn merge(&mut self, handshake: &ExtensionHandshake) {
        for (name, id) in &handshake.m {
            if *id == 0 {
                self.m.remove(name);
            } else if let Ok(id) = u8::try_from(*id) {
                self.m.insert(name.clone(), id);
            }
        }
        if handshake.v.is_some() {
            self.v.clone_from(&handshake.v);
        }
        if let Some(p) = handshake.p.and_then(|p| u16::try_from(p).ok()) {
            self.p = Some(p);
        }
        if handshake.reqq.is_some() {
            self.reqq = handshake.reqq;
        }
        if handshake.metadata_size.is_some() {
            self.metadata_size = handshake.metadata_size;
        }
    }

    pub fn peer_ext_id(&self, name: &str) -> Option<u8> {
        self.m.get(name).copied().filter(|id| *id != 0)
    }
}

fn encode_int(value: i64) -> Vec<u8> {
    format!("i{value}e").into_bytes()
}

fn encode_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = format!("{}:", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out
}

fn encode_dict(pairs: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut pairs = pairs.to_vec();
    pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut out = vec![b'd'];
    for (key, value) in pairs {
        out.extend(encode_bytes(&key));
        out.extend(value);
    }
    out.push(b'e');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utorrent_handshake_bytes() -> Vec<u8> {
        // Captured-style payload modeled on the BEP-0010 µTorrent example,
        // plus reqq / metadata_size and an unknown key that must be ignored.
        let mut body = Vec::new();
        body.extend_from_slice(b"d1:md11:LT_metadatai1e6:ut_pexi2ee");
        body.extend_from_slice(b"13:metadata_sizei32768e");
        body.extend_from_slice(b"1:pi6881e4:reqqi250e");
        body.extend_from_slice(b"6:yourip4:");
        body.extend_from_slice(&[127, 0, 0, 1]);
        body.extend_from_slice(b"1:v13:");
        body.extend_from_slice("\u{00b5}Torrent 1.2".as_bytes());
        body.push(b'e');
        body
    }

    #[test]
    fn encode_matches_sorted_bencode_fixture() {
        let mut m = BTreeMap::new();
        m.insert("ut_metadata".into(), 3);
        let hs = ExtensionHandshake {
            m,
            v: Some("bitrev 0.1.0".into()),
            p: Some(6881),
            reqq: Some(250),
            metadata_size: Some(16384),
        };
        let expected = b"d1:md11:ut_metadatai3ee13:metadata_sizei16384e1:pi6881e4:reqqi250e1:v12:bitrev 0.1.0e";
        assert_eq!(hs.encode(), expected);
    }

    #[test]
    fn encode_decode_round_trip_our_handshake() {
        let mut m = BTreeMap::new();
        m.insert("ut_metadata".into(), 3);
        let encoded = ExtensionHandshake::outgoing(m, Some(6881), Some(16384)).encode();
        let decoded = ExtensionHandshake::decode(&encoded);

        assert_eq!(decoded.m.get("ut_metadata").copied(), Some(3));
        assert_eq!(
            decoded.v.as_deref(),
            Some(identity::extension_version().as_str())
        );
        assert_eq!(decoded.p, Some(6881));
        assert_eq!(decoded.reqq, Some(DEFAULT_REQQ));
        assert_eq!(decoded.metadata_size, Some(16384));
    }

    #[test]
    fn outgoing_omits_metadata_size_unless_ut_metadata_is_registered() {
        let mut m = BTreeMap::new();
        m.insert("ut_pex".into(), 1);
        let hs = ExtensionHandshake::outgoing(m, Some(6881), Some(16384));
        assert!(hs.metadata_size.is_none());

        let mut m = BTreeMap::new();
        m.insert(UT_METADATA.into(), 3);
        let hs = ExtensionHandshake::outgoing(m, None, Some(4096));
        assert_eq!(hs.metadata_size, Some(4096));
        assert!(hs.p.is_none());
    }

    #[test]
    fn decode_utorrent_style_fixture_ignores_unknown_keys() {
        let decoded = ExtensionHandshake::decode(&utorrent_handshake_bytes());
        assert_eq!(decoded.m.get("LT_metadata").copied(), Some(1));
        assert_eq!(decoded.m.get("ut_pex").copied(), Some(2));
        assert_eq!(decoded.p, Some(6881));
        assert_eq!(decoded.reqq, Some(250));
        assert_eq!(decoded.metadata_size, Some(32768));
        assert_eq!(decoded.v.as_deref(), Some("\u{00b5}Torrent 1.2"));
    }

    #[test]
    fn decode_rejects_oversized_payload_and_metadata_size() {
        let huge = vec![0u8; MAX_EXTENSION_PAYLOAD + 1];
        assert_eq!(
            ExtensionHandshake::decode(&huge),
            ExtensionHandshake::default()
        );

        let oversized = b"d13:metadata_sizei999999999ee";
        let decoded = ExtensionHandshake::decode(oversized);
        assert!(decoded.metadata_size.is_none());
    }

    #[test]
    fn decode_is_lenient_on_garbage() {
        assert_eq!(
            ExtensionHandshake::decode(b"not bencode"),
            ExtensionHandshake::default()
        );
        assert_eq!(
            ExtensionHandshake::decode(b"i4e"),
            ExtensionHandshake::default()
        );
        assert_eq!(
            ExtensionHandshake::decode(b"d1:m4:nope1:vi1ee"),
            ExtensionHandshake {
                v: None,
                ..ExtensionHandshake::default()
            }
        );
    }

    #[test]
    fn merge_second_handshake_updates_and_disables() {
        let first = ExtensionHandshake::decode(&utorrent_handshake_bytes());
        let mut peer = first.into_peer_info();
        assert_eq!(peer.peer_ext_id("LT_metadata"), Some(1));
        assert_eq!(peer.peer_ext_id("ut_pex"), Some(2));

        let mut second_m = BTreeMap::new();
        second_m.insert("LT_metadata".into(), 0);
        second_m.insert("ut_holepunch".into(), 4);
        let second = ExtensionHandshake {
            m: second_m,
            v: Some("µTorrent 1.3".into()),
            reqq: Some(100),
            ..ExtensionHandshake::default()
        };
        peer.merge(&second);

        assert_eq!(peer.peer_ext_id("LT_metadata"), None);
        assert!(!peer.m.contains_key("LT_metadata"));
        assert_eq!(peer.peer_ext_id("ut_pex"), Some(2));
        assert_eq!(peer.peer_ext_id("ut_holepunch"), Some(4));
        assert_eq!(peer.v.as_deref(), Some("µTorrent 1.3"));
        assert_eq!(peer.reqq, Some(100));
        assert_eq!(peer.p, Some(6881));
        assert_eq!(peer.metadata_size, Some(32768));
    }
}
