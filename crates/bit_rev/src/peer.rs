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
    if !buf.len().is_multiple_of(6) {
        anyhow::bail!("invalid compact peers length");
    }
    for chunk in buf.chunks_exact(6) {
        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        out.push(SocketAddr::new(ip.into(), port));
    }
    Ok(())
}

fn parse_compact_v6(buf: &[u8], out: &mut Vec<PeerAddr>) -> anyhow::Result<()> {
    if !buf.len().is_multiple_of(18) {
        anyhow::bail!("invalid compact peers6 length");
    }
    for chunk in buf.chunks_exact(18) {
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
        let decoded = de::from_bytes::<BencodeResponse>(body).unwrap();
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
}
