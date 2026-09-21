use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};

use serde::{Deserialize, Serialize};
use serde_bencode::value::Value;
use serde_bytes::ByteBuf;

pub type PeerAddr = SocketAddr;

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct BencodeResponse {
    #[serde(default)]
    pub peers: Option<Value>,
    #[serde(default)]
    pub peers6: Option<ByteBuf>,
    #[serde(default)]
    pub interval: Option<u64>,
    #[serde(default, rename = "min interval")]
    pub min_interval: Option<u64>,
    #[serde(default, rename = "failure reason")]
    pub failure_reason: Option<ByteBuf>,
    #[serde(default, rename = "warning message")]
    pub warning_message: Option<ByteBuf>,
    #[serde(default, rename = "tracker id")]
    pub tracker_id: Option<ByteBuf>,
    #[serde(default)]
    pub complete: Option<i64>,
    #[serde(default)]
    pub incomplete: Option<i64>,
}

impl BencodeResponse {
    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        crate::file::check_bencode_depth(bytes)?;
        Ok(serde_bencode::from_bytes(bytes)?)
    }

    pub fn failure_reason_str(&self) -> Option<String> {
        self.failure_reason
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }

    pub fn warning_message_str(&self) -> Option<String> {
        self.warning_message
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }

    pub fn get_peers(&self) -> anyhow::Result<Vec<PeerAddr>> {
        let mut peers = Vec::new();
        if let Some(value) = self.peers.as_ref() {
            parse_peers_value(value, &mut peers)?;
        }
        if let Some(peers6) = self.peers6.as_ref() {
            parse_compact_v6(peers6, &mut peers)?;
        }
        Ok(peers)
    }
}

/// Compact IPv4 peer list (BEP-0023): 4-byte address + 2-byte port, network order.
pub fn encode_compact_v4(addrs: &[PeerAddr]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(addrs.len() * 6);
    for addr in addrs {
        if let SocketAddr::V4(v4) = addr {
            buf.extend_from_slice(&v4.ip().octets());
            buf.extend_from_slice(&v4.port().to_be_bytes());
        }
    }
    buf
}

/// Compact IPv6 peer list (BEP-0007): 16-byte address + 2-byte port, network order.
pub fn encode_compact_v6(addrs: &[PeerAddr]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(addrs.len() * 18);
    for addr in addrs {
        if let SocketAddr::V6(v6) = addr {
            buf.extend_from_slice(&v6.ip().octets());
            buf.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
    buf
}

pub fn decode_compact_v4(buf: &[u8]) -> anyhow::Result<Vec<PeerAddr>> {
    let mut peers = Vec::new();
    parse_compact_v4(buf, &mut peers)?;
    Ok(peers)
}

pub fn decode_compact_v6(buf: &[u8]) -> anyhow::Result<Vec<PeerAddr>> {
    let mut peers = Vec::new();
    parse_compact_v6(buf, &mut peers)?;
    Ok(peers)
}

fn parse_peers_value(value: &Value, out: &mut Vec<PeerAddr>) -> anyhow::Result<()> {
    match value {
        Value::Bytes(buf) => parse_compact_v4(buf, out),
        Value::List(list) => {
            for item in list {
                match item {
                    Value::Dict(dict) => out.push(peer_from_dict(dict)?),
                    _ => anyhow::bail!("invalid dictionary peer entry"),
                }
            }
            Ok(())
        }
        _ => anyhow::bail!("invalid peers field"),
    }
}

fn parse_compact_v4(buf: &[u8], out: &mut Vec<PeerAddr>) -> anyhow::Result<()> {
    let (chunks, remainder) = buf.as_chunks::<6>();
    if !remainder.is_empty() {
        anyhow::bail!("invalid compact peers length");
    }
    for chunk in chunks {
        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        out.push(SocketAddr::new(ip.into(), port));
    }
    Ok(())
}

fn parse_compact_v6(buf: &[u8], out: &mut Vec<PeerAddr>) -> anyhow::Result<()> {
    let (chunks, remainder) = buf.as_chunks::<18>();
    if !remainder.is_empty() {
        anyhow::bail!("invalid compact peers6 length");
    }
    for chunk in chunks {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&chunk[..16]);
        let ip = Ipv6Addr::from(octets);
        let port = u16::from_be_bytes([chunk[16], chunk[17]]);
        out.push(SocketAddr::new(ip.into(), port));
    }
    Ok(())
}

fn peer_from_dict(dict: &HashMap<Vec<u8>, Value>) -> anyhow::Result<PeerAddr> {
    let ip = match dict.get(&b"ip"[..]) {
        Some(Value::Bytes(bytes)) => String::from_utf8_lossy(bytes)
            .parse::<IpAddr>()
            .map_err(|e| anyhow::anyhow!("invalid peer ip: {e}"))?,
        _ => anyhow::bail!("peer dict missing ip"),
    };
    let port = match dict.get(&b"port"[..]) {
        Some(Value::Int(port)) if *port >= 0 && *port <= i64::from(u16::MAX) => *port as u16,
        _ => anyhow::bail!("peer dict missing port"),
    };
    Ok(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_bencode::de;

    #[test]
    fn decodes_failure_reason() {
        let body = b"d14:failure reason16:unregistered 123e";
        let decoded = BencodeResponse::from_bytes(body).unwrap();
        assert_eq!(
            decoded.failure_reason_str().as_deref(),
            Some("unregistered 123")
        );
        assert!(decoded.interval.is_none());
    }

    #[test]
    fn decodes_warning_and_compact_peers() {
        let mut body = b"d8:intervali1800e15:warning message7:slow ok5:peers6:".to_vec();
        body.extend_from_slice(&[127, 0, 0, 1, 0x1A, 0xE1]);
        body.push(b'e');
        let decoded = de::from_bytes::<BencodeResponse>(&body).unwrap();
        assert_eq!(decoded.interval, Some(1800));
        assert_eq!(decoded.warning_message_str().as_deref(), Some("slow ok"));
        assert_eq!(
            decoded.get_peers().unwrap(),
            vec!["127.0.0.1:6881".parse().unwrap()]
        );
    }

    #[test]
    fn decodes_dictionary_peers_and_min_interval() {
        let body = b"d8:intervali60e12:min intervali120e5:peersld2:ip9:127.0.0.14:porti51413eeee";
        let decoded = de::from_bytes::<BencodeResponse>(body).unwrap();
        assert_eq!(decoded.min_interval, Some(120));
        assert_eq!(
            decoded.get_peers().unwrap(),
            vec!["127.0.0.1:51413".parse().unwrap()]
        );
    }

    #[test]
    fn decodes_tracker_id() {
        let body = b"d8:intervali60e10:tracker id3:abc5:peers0:e";
        let decoded = de::from_bytes::<BencodeResponse>(body).unwrap();
        assert_eq!(
            decoded.tracker_id.as_ref().map(|id| id.as_ref()),
            Some(&b"abc"[..])
        );
        assert!(decoded.get_peers().unwrap().is_empty());
    }

    fn compact_v4_response(buf: &[u8]) -> BencodeResponse {
        BencodeResponse {
            peers: Some(Value::Bytes(buf.to_vec())),
            peers6: None,
            interval: None,
            min_interval: None,
            failure_reason: None,
            warning_message: None,
            tracker_id: None,
            complete: None,
            incomplete: None,
        }
    }

    fn compact_v6_response(buf: &[u8]) -> BencodeResponse {
        BencodeResponse {
            peers: None,
            peers6: Some(ByteBuf::from(buf.to_vec())),
            interval: None,
            min_interval: None,
            failure_reason: None,
            warning_message: None,
            tracker_id: None,
            complete: None,
            incomplete: None,
        }
    }

    #[test]
    fn compact_peers_decode_multiple_addresses() {
        let buf = [
            127, 0, 0, 1, 0x1A, 0xE1, 192, 168, 1, 2, 0x1A, 0xE9, 10, 0, 0, 5, 0x00, 0x50,
        ];
        let response = compact_v4_response(&buf);
        assert_eq!(
            response.get_peers().unwrap(),
            vec![
                "127.0.0.1:6881".parse().unwrap(),
                "192.168.1.2:6889".parse().unwrap(),
                "10.0.0.5:80".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn compact_peers_odd_length_is_error() {
        for len in [5, 7] {
            let buf = vec![0u8; len];
            let err = compact_v4_response(&buf).get_peers().unwrap_err();
            assert!(
                err.to_string().contains("invalid compact peers length"),
                "len={len}: {err}"
            );
        }
    }

    #[test]
    fn compact_peers_empty_is_empty_vec() {
        let response = compact_v4_response(&[]);
        assert!(response.get_peers().unwrap().is_empty());
    }

    #[test]
    fn compact_peers6_one_address() {
        let mut buf = [0u8; 18];
        buf[15] = 1;
        buf[16] = 0x1A;
        buf[17] = 0xE1;
        let response = compact_v6_response(&buf);
        assert_eq!(
            response.get_peers().unwrap(),
            vec!["[::1]:6881".parse().unwrap()]
        );
    }

    #[test]
    fn compact_peers6_odd_length_is_error() {
        let err = compact_v6_response(&[0u8; 17]).get_peers().unwrap_err();
        assert!(err.to_string().contains("invalid compact peers6 length"));
    }

    #[test]
    fn encode_decode_compact_v4_round_trip() {
        let addrs = vec![
            "127.0.0.1:6881".parse().unwrap(),
            "192.168.1.2:6889".parse().unwrap(),
        ];
        let encoded = encode_compact_v4(&addrs);
        assert_eq!(encoded.len(), 12);
        assert_eq!(decode_compact_v4(&encoded).unwrap(), addrs);
    }

    #[test]
    fn encode_compact_v4_skips_ipv6() {
        let addrs = vec![
            "127.0.0.1:6881".parse().unwrap(),
            "[::1]:6881".parse().unwrap(),
        ];
        assert_eq!(
            decode_compact_v4(&encode_compact_v4(&addrs)).unwrap(),
            vec!["127.0.0.1:6881".parse().unwrap()]
        );
    }

    #[test]
    fn encode_decode_compact_v6_round_trip() {
        let addrs = vec!["[::1]:6881".parse().unwrap()];
        let encoded = encode_compact_v6(&addrs);
        assert_eq!(encoded.len(), 18);
        assert_eq!(decode_compact_v6(&encoded).unwrap(), addrs);
    }
}
