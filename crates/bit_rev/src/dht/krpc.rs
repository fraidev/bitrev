//! KRPC (BEP-0005): bencoded RPC over UDP.
//!
//! Encode is hand-rolled so BEP example packets match byte-exactly. Decode
//! never panics; malformed packets return `Err`.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use serde_bencode::value::Value;

use crate::file::check_bencode_depth;
use crate::identity::{azureus_version, CLIENT_CODE, CLIENT_VERSION};

pub const COMPACT_PEER_LEN: usize = 6;
pub const COMPACT_NODE_LEN: usize = 26;
pub const NODE_ID_LEN: usize = 20;
pub const TRANSACTION_ID_LEN: usize = 2; // used when minting 2-byte `t` values

#[allow(dead_code)]
pub const ERR_GENERIC: i64 = 201;
#[allow(dead_code)]
pub const ERR_SERVER: i64 = 202;
pub const ERR_PROTOCOL: i64 = 203;
pub const ERR_METHOD: i64 = 204;

pub type NodeId = [u8; NODE_ID_LEN];

/// BEP-0020 two-character client id plus two version digits (`BR01`).
pub fn client_version() -> [u8; 4] {
    let ver = azureus_version(CLIENT_VERSION);
    [CLIENT_CODE[0], CLIENT_CODE[1], ver[0], ver[1]]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactNode {
    pub id: NodeId,
    pub addr: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Ping {
        id: NodeId,
    },
    FindNode {
        id: NodeId,
        target: NodeId,
    },
    GetPeers {
        id: NodeId,
        info_hash: NodeId,
    },
    AnnouncePeer {
        id: NodeId,
        info_hash: NodeId,
        port: u16,
        token: Vec<u8>,
        implied_port: bool,
    },
}

impl Query {
    pub fn id(&self) -> NodeId {
        match self {
            Query::Ping { id }
            | Query::FindNode { id, .. }
            | Query::GetPeers { id, .. }
            | Query::AnnouncePeer { id, .. } => *id,
        }
    }

    pub fn method(&self) -> &'static str {
        match self {
            Query::Ping { .. } => "ping",
            Query::FindNode { .. } => "find_node",
            Query::GetPeers { .. } => "get_peers",
            Query::AnnouncePeer { .. } => "announce_peer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub id: NodeId,
    pub nodes: Vec<CompactNode>,
    pub values: Vec<SocketAddr>,
    pub token: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorMsg {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Query(Query),
    Response(Response),
    Error(ErrorMsg),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KrpcMessage {
    pub transaction_id: Vec<u8>,
    pub payload: Payload,
    pub version: Option<Vec<u8>>,
}

impl KrpcMessage {
    pub fn query(t: impl Into<Vec<u8>>, query: Query) -> Self {
        Self {
            transaction_id: t.into(),
            payload: Payload::Query(query),
            version: Some(client_version().to_vec()),
        }
    }

    pub fn query_no_v(t: impl Into<Vec<u8>>, query: Query) -> Self {
        Self {
            transaction_id: t.into(),
            payload: Payload::Query(query),
            version: None,
        }
    }

    pub fn response(t: impl Into<Vec<u8>>, response: Response) -> Self {
        Self {
            transaction_id: t.into(),
            payload: Payload::Response(response),
            version: Some(client_version().to_vec()),
        }
    }

    pub fn response_no_v(t: impl Into<Vec<u8>>, response: Response) -> Self {
        Self {
            transaction_id: t.into(),
            payload: Payload::Response(response),
            version: None,
        }
    }

    pub fn error(t: impl Into<Vec<u8>>, code: i64, message: impl Into<String>) -> Self {
        Self {
            transaction_id: t.into(),
            payload: Payload::Error(ErrorMsg {
                code,
                message: message.into(),
            }),
            version: Some(client_version().to_vec()),
        }
    }

    pub fn error_no_v(t: impl Into<Vec<u8>>, code: i64, message: impl Into<String>) -> Self {
        Self {
            transaction_id: t.into(),
            payload: Payload::Error(ErrorMsg {
                code,
                message: message.into(),
            }),
            version: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    NotBencode,
    NotADict,
    MissingTransaction,
    MissingType,
    UnknownType,
    MalformedQuery,
    UnknownMethod,
    MalformedResponse,
    MalformedError,
}

/// Decode a KRPC packet. Never panics on attacker-controlled input.
pub fn decode(buf: &[u8]) -> Result<KrpcMessage, DecodeError> {
    if buf.is_empty() {
        return Err(DecodeError::NotBencode);
    }
    if check_bencode_depth(buf).is_err() {
        return Err(DecodeError::NotBencode);
    }
    let value: Value = serde_bencode::from_bytes(buf).map_err(|_| DecodeError::NotBencode)?;
    let dict = match value {
        Value::Dict(d) => d,
        _ => return Err(DecodeError::NotADict),
    };

    let transaction_id = dict_bytes(&dict, b"t").ok_or(DecodeError::MissingTransaction)?;
    if transaction_id.is_empty() {
        return Err(DecodeError::MissingTransaction);
    }
    let y = dict_bytes(&dict, b"y").ok_or(DecodeError::MissingType)?;
    let version = dict_bytes(&dict, b"v");

    let payload = match y.as_slice() {
        b"q" => Payload::Query(parse_query(&dict)?),
        b"r" => Payload::Response(parse_response(&dict)?),
        b"e" => Payload::Error(parse_error(&dict)?),
        _ => return Err(DecodeError::UnknownType),
    };

    Ok(KrpcMessage {
        transaction_id,
        payload,
        version,
    })
}

/// Best-effort extract of `t` from a packet that may otherwise be malformed.
pub fn peek_transaction_id(buf: &[u8]) -> Option<Vec<u8>> {
    if check_bencode_depth(buf).is_err() {
        return None;
    }
    let Value::Dict(dict) = serde_bencode::from_bytes(buf).ok()? else {
        return None;
    };
    let t = dict_bytes(&dict, b"t")?;
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// Whether a packet looks like a query (`y=q`) even if args are bad.
pub fn peek_is_query(buf: &[u8]) -> bool {
    if check_bencode_depth(buf).is_err() {
        return false;
    }
    let Ok(Value::Dict(dict)) = serde_bencode::from_bytes(buf) else {
        return false;
    };
    dict_bytes(&dict, b"y").as_deref() == Some(&b"q"[..])
}

fn parse_query(dict: &HashMap<Vec<u8>, Value>) -> Result<Query, DecodeError> {
    let method = dict_bytes(dict, b"q").ok_or(DecodeError::MalformedQuery)?;
    let args = dict_dict(dict, b"a").ok_or(DecodeError::MalformedQuery)?;
    let id = dict_id(&args, b"id").ok_or(DecodeError::MalformedQuery)?;
    match method.as_slice() {
        b"ping" => Ok(Query::Ping { id }),
        b"find_node" => {
            let target = dict_id(&args, b"target").ok_or(DecodeError::MalformedQuery)?;
            Ok(Query::FindNode { id, target })
        }
        b"get_peers" => {
            let info_hash = dict_id(&args, b"info_hash").ok_or(DecodeError::MalformedQuery)?;
            Ok(Query::GetPeers { id, info_hash })
        }
        b"announce_peer" => {
            let info_hash = dict_id(&args, b"info_hash").ok_or(DecodeError::MalformedQuery)?;
            let token = dict_bytes(&args, b"token").ok_or(DecodeError::MalformedQuery)?;
            let implied_port = dict_int(&args, b"implied_port").unwrap_or(0) != 0;
            let port = match dict_int(&args, b"port") {
                Some(p) if (0..=i64::from(u16::MAX)).contains(&p) => p as u16,
                _ if implied_port => 0,
                _ => return Err(DecodeError::MalformedQuery),
            };
            Ok(Query::AnnouncePeer {
                id,
                info_hash,
                port,
                token,
                implied_port,
            })
        }
        _ => Err(DecodeError::UnknownMethod),
    }
}

fn parse_response(dict: &HashMap<Vec<u8>, Value>) -> Result<Response, DecodeError> {
    let r = dict_dict(dict, b"r").ok_or(DecodeError::MalformedResponse)?;
    let id = dict_id(&r, b"id").ok_or(DecodeError::MalformedResponse)?;
    let nodes = dict_bytes(&r, b"nodes")
        .map(|b| decode_compact_nodes(&b))
        .unwrap_or_default();
    let values = match r.get(b"values".as_slice()) {
        Some(Value::List(list)) => list
            .iter()
            .filter_map(|item| match item {
                Value::Bytes(b) => decode_compact_peer(b),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let token = dict_bytes(&r, b"token");
    Ok(Response {
        id,
        nodes,
        values,
        token,
    })
}

fn parse_error(dict: &HashMap<Vec<u8>, Value>) -> Result<ErrorMsg, DecodeError> {
    let list = match dict.get(b"e".as_slice()) {
        Some(Value::List(l)) if l.len() >= 2 => l,
        _ => return Err(DecodeError::MalformedError),
    };
    let code = match list[0] {
        Value::Int(n) => n,
        _ => return Err(DecodeError::MalformedError),
    };
    let message = match &list[1] {
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        _ => return Err(DecodeError::MalformedError),
    };
    Ok(ErrorMsg { code, message })
}

fn dict_bytes(dict: &HashMap<Vec<u8>, Value>, key: &[u8]) -> Option<Vec<u8>> {
    match dict.get(key) {
        Some(Value::Bytes(b)) => Some(b.clone()),
        _ => None,
    }
}

fn dict_int(dict: &HashMap<Vec<u8>, Value>, key: &[u8]) -> Option<i64> {
    match dict.get(key) {
        Some(Value::Int(n)) => Some(*n),
        _ => None,
    }
}

fn dict_dict(dict: &HashMap<Vec<u8>, Value>, key: &[u8]) -> Option<HashMap<Vec<u8>, Value>> {
    match dict.get(key) {
        Some(Value::Dict(d)) => Some(d.clone()),
        _ => None,
    }
}

fn dict_id(dict: &HashMap<Vec<u8>, Value>, key: &[u8]) -> Option<NodeId> {
    let bytes = dict_bytes(dict, key)?;
    bytes.as_slice().try_into().ok()
}

pub fn encode(msg: &KrpcMessage) -> Vec<u8> {
    let mut pairs: Vec<(Vec<u8>, Encoded)> = Vec::new();
    match &msg.payload {
        Payload::Query(q) => {
            pairs.push((b"a".to_vec(), Encoded::Dict(query_args(q))));
            pairs.push((
                b"q".to_vec(),
                Encoded::Bytes(q.method().as_bytes().to_vec()),
            ));
            pairs.push((b"t".to_vec(), Encoded::Bytes(msg.transaction_id.clone())));
            if let Some(v) = &msg.version {
                pairs.push((b"v".to_vec(), Encoded::Bytes(v.clone())));
            }
            pairs.push((b"y".to_vec(), Encoded::Bytes(b"q".to_vec())));
        }
        Payload::Response(r) => {
            pairs.push((b"r".to_vec(), Encoded::Dict(response_args(r))));
            pairs.push((b"t".to_vec(), Encoded::Bytes(msg.transaction_id.clone())));
            if let Some(v) = &msg.version {
                pairs.push((b"v".to_vec(), Encoded::Bytes(v.clone())));
            }
            pairs.push((b"y".to_vec(), Encoded::Bytes(b"r".to_vec())));
        }
        Payload::Error(e) => {
            pairs.push((
                b"e".to_vec(),
                Encoded::List(vec![
                    Encoded::Int(e.code),
                    Encoded::Bytes(e.message.as_bytes().to_vec()),
                ]),
            ));
            pairs.push((b"t".to_vec(), Encoded::Bytes(msg.transaction_id.clone())));
            if let Some(v) = &msg.version {
                pairs.push((b"v".to_vec(), Encoded::Bytes(v.clone())));
            }
            pairs.push((b"y".to_vec(), Encoded::Bytes(b"e".to_vec())));
        }
    }
    encode_dict(&pairs)
}

fn query_args(q: &Query) -> Vec<(Vec<u8>, Encoded)> {
    match q {
        Query::Ping { id } => vec![(b"id".to_vec(), Encoded::Bytes(id.to_vec()))],
        Query::FindNode { id, target } => vec![
            (b"id".to_vec(), Encoded::Bytes(id.to_vec())),
            (b"target".to_vec(), Encoded::Bytes(target.to_vec())),
        ],
        Query::GetPeers { id, info_hash } => vec![
            (b"id".to_vec(), Encoded::Bytes(id.to_vec())),
            (b"info_hash".to_vec(), Encoded::Bytes(info_hash.to_vec())),
        ],
        Query::AnnouncePeer {
            id,
            info_hash,
            port,
            token,
            implied_port,
        } => {
            let mut args = vec![(b"id".to_vec(), Encoded::Bytes(id.to_vec()))];
            if *implied_port {
                args.push((b"implied_port".to_vec(), Encoded::Int(1)));
            }
            args.push((b"info_hash".to_vec(), Encoded::Bytes(info_hash.to_vec())));
            args.push((b"port".to_vec(), Encoded::Int(i64::from(*port))));
            args.push((b"token".to_vec(), Encoded::Bytes(token.clone())));
            args
        }
    }
}

fn response_args(r: &Response) -> Vec<(Vec<u8>, Encoded)> {
    let mut args = vec![(b"id".to_vec(), Encoded::Bytes(r.id.to_vec()))];
    if !r.nodes.is_empty() {
        args.push((
            b"nodes".to_vec(),
            Encoded::Bytes(encode_compact_nodes(&r.nodes)),
        ));
    }
    if let Some(token) = &r.token {
        args.push((b"token".to_vec(), Encoded::Bytes(token.clone())));
    }
    if !r.values.is_empty() {
        let values = r
            .values
            .iter()
            .filter_map(|addr| encode_compact_peer(*addr).map(Encoded::Bytes))
            .collect();
        args.push((b"values".to_vec(), Encoded::List(values)));
    }
    args
}

enum Encoded {
    Bytes(Vec<u8>),
    Int(i64),
    List(Vec<Encoded>),
    Dict(Vec<(Vec<u8>, Encoded)>),
}

fn encode_value(out: &mut Vec<u8>, value: &Encoded) {
    match value {
        Encoded::Bytes(b) => {
            out.extend_from_slice(b.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(b);
        }
        Encoded::Int(n) => {
            out.push(b'i');
            out.extend_from_slice(n.to_string().as_bytes());
            out.push(b'e');
        }
        Encoded::List(items) => {
            out.push(b'l');
            for item in items {
                encode_value(out, item);
            }
            out.push(b'e');
        }
        Encoded::Dict(pairs) => {
            out.extend_from_slice(&encode_dict(pairs));
        }
    }
}

fn encode_dict(pairs: &[(Vec<u8>, Encoded)]) -> Vec<u8> {
    let mut sorted: Vec<_> = pairs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = vec![b'd'];
    for (key, value) in sorted {
        out.extend_from_slice(key.len().to_string().as_bytes());
        out.push(b':');
        out.extend_from_slice(key);
        encode_value(&mut out, value);
    }
    out.push(b'e');
    out
}

pub fn encode_compact_peer(addr: SocketAddr) -> Option<Vec<u8>> {
    let SocketAddr::V4(v4) = addr else {
        return None;
    };
    let mut buf = Vec::with_capacity(COMPACT_PEER_LEN);
    buf.extend_from_slice(&v4.ip().octets());
    buf.extend_from_slice(&v4.port().to_be_bytes());
    Some(buf)
}

pub fn decode_compact_peer(buf: &[u8]) -> Option<SocketAddr> {
    if buf.len() != COMPACT_PEER_LEN {
        return None;
    }
    let ip = Ipv4Addr::new(buf[0], buf[1], buf[2], buf[3]);
    let port = u16::from_be_bytes([buf[4], buf[5]]);
    Some(SocketAddr::V4(SocketAddrV4::new(ip, port)))
}

#[allow(dead_code)]
pub fn decode_compact_peers(buf: &[u8]) -> Vec<SocketAddr> {
    buf.as_chunks::<COMPACT_PEER_LEN>()
        .0
        .iter()
        .filter_map(|chunk| decode_compact_peer(chunk))
        .collect()
}

pub fn encode_compact_node(node: &CompactNode) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(COMPACT_NODE_LEN);
    buf.extend_from_slice(&node.id);
    buf.extend_from_slice(&encode_compact_peer(node.addr)?);
    Some(buf)
}

pub fn encode_compact_nodes(nodes: &[CompactNode]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(nodes.len() * COMPACT_NODE_LEN);
    for node in nodes {
        if let Some(entry) = encode_compact_node(node) {
            buf.extend_from_slice(&entry);
        }
    }
    buf
}

pub fn decode_compact_nodes(buf: &[u8]) -> Vec<CompactNode> {
    buf.as_chunks::<COMPACT_NODE_LEN>()
        .0
        .iter()
        .filter_map(|chunk| {
            let id: NodeId = chunk[..NODE_ID_LEN].try_into().ok()?;
            let addr = decode_compact_peer(&chunk[NODE_ID_LEN..])?;
            Some(CompactNode { id, addr })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID_A: &str = "abcdefghij0123456789";
    const ID_B: &str = "mnopqrstuvwxyz123456";
    const ID_C: &str = "0123456789abcdefghij";

    fn id(s: &str) -> NodeId {
        s.as_bytes().try_into().expect("20-byte id")
    }

    #[test]
    fn ping_query_matches_bep() {
        let msg = KrpcMessage::query_no_v(*b"aa", Query::Ping { id: id(ID_A) });
        let expected = b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe";
        assert_eq!(encode(&msg), expected);
        assert_eq!(decode(expected).unwrap(), msg);
    }

    #[test]
    fn ping_response_matches_bep() {
        let msg = KrpcMessage::response_no_v(
            *b"aa",
            Response {
                id: id(ID_B),
                nodes: vec![],
                values: vec![],
                token: None,
            },
        );
        let expected = b"d1:rd2:id20:mnopqrstuvwxyz123456e1:t2:aa1:y1:re";
        assert_eq!(encode(&msg), expected);
        assert_eq!(decode(expected).unwrap(), msg);
    }

    #[test]
    fn find_node_query_matches_bep() {
        let msg = KrpcMessage::query_no_v(
            *b"aa",
            Query::FindNode {
                id: id(ID_A),
                target: id(ID_B),
            },
        );
        // BEP-0005 writes `5:target` in the example string. `target` is 6
        // bytes, so the valid encoding is `6:target`.
        let expected =
            b"d1:ad2:id20:abcdefghij01234567896:target20:mnopqrstuvwxyz123456e1:q9:find_node1:t2:aa1:y1:qe";
        assert_eq!(encode(&msg), expected);
        assert_eq!(decode(expected).unwrap(), msg);
    }

    #[test]
    fn get_peers_query_matches_bep() {
        let msg = KrpcMessage::query_no_v(
            *b"aa",
            Query::GetPeers {
                id: id(ID_A),
                info_hash: id(ID_B),
            },
        );
        let expected =
            b"d1:ad2:id20:abcdefghij01234567899:info_hash20:mnopqrstuvwxyz123456e1:q9:get_peers1:t2:aa1:y1:qe";
        assert_eq!(encode(&msg), expected);
        assert_eq!(decode(expected).unwrap(), msg);
    }

    #[test]
    fn get_peers_values_response_matches_bep() {
        let values = vec![
            decode_compact_peer(b"axje.u").unwrap(),
            decode_compact_peer(b"idhtnm").unwrap(),
        ];
        let msg = KrpcMessage::response_no_v(
            *b"aa",
            Response {
                id: id(ID_A),
                nodes: vec![],
                values,
                token: Some(b"aoeusnth".to_vec()),
            },
        );
        let expected =
            b"d1:rd2:id20:abcdefghij01234567895:token8:aoeusnth6:valuesl6:axje.u6:idhtnmee1:t2:aa1:y1:re";
        assert_eq!(encode(&msg), expected);
        assert_eq!(decode(expected).unwrap(), msg);
    }

    #[test]
    fn announce_peer_query_matches_bep() {
        let msg = KrpcMessage::query_no_v(
            *b"aa",
            Query::AnnouncePeer {
                id: id(ID_A),
                info_hash: id(ID_B),
                port: 6881,
                token: b"aoeusnth".to_vec(),
                implied_port: true,
            },
        );
        let expected = b"d1:ad2:id20:abcdefghij012345678912:implied_porti1e9:info_hash20:mnopqrstuvwxyz1234564:porti6881e5:token8:aoeusnthe1:q13:announce_peer1:t2:aa1:y1:qe";
        assert_eq!(encode(&msg), expected);
        assert_eq!(decode(expected).unwrap(), msg);
    }

    #[test]
    fn announce_peer_response_matches_bep() {
        let msg = KrpcMessage::response_no_v(
            *b"aa",
            Response {
                id: id(ID_B),
                nodes: vec![],
                values: vec![],
                token: None,
            },
        );
        let expected = b"d1:rd2:id20:mnopqrstuvwxyz123456e1:t2:aa1:y1:re";
        assert_eq!(encode(&msg), expected);
        assert_eq!(decode(expected).unwrap(), msg);
    }

    #[test]
    fn error_matches_bep() {
        let msg = KrpcMessage::error_no_v(*b"aa", 201, "A Generic Error Ocurred");
        let expected = b"d1:eli201e23:A Generic Error Ocurrede1:t2:aa1:y1:ee";
        assert_eq!(encode(&msg), expected);
        assert_eq!(decode(expected).unwrap(), msg);
    }

    #[test]
    fn find_node_response_round_trip() {
        let node = CompactNode {
            id: id(ID_C),
            addr: "127.0.0.1:6881".parse().unwrap(),
        };
        let msg = KrpcMessage::response_no_v(
            *b"aa",
            Response {
                id: id(ID_C),
                nodes: vec![node],
                values: vec![],
                token: None,
            },
        );
        let bytes = encode(&msg);
        assert_eq!(decode(&bytes).unwrap(), msg);
    }

    #[test]
    fn encode_includes_version_when_set() {
        let msg = KrpcMessage::query(*b"aa", Query::Ping { id: id(ID_A) });
        let bytes = encode(&msg);
        assert!(bytes.windows(4).any(|w| w == b"1:v4"));
        let decoded = decode(&bytes).unwrap();
        assert_eq!(
            decoded.version.as_deref(),
            Some(client_version().as_slice())
        );
    }

    #[test]
    fn compact_peer_round_trip() {
        let addr: SocketAddr = "1.2.3.4:6881".parse().unwrap();
        let bytes = encode_compact_peer(addr).unwrap();
        assert_eq!(bytes.len(), 6);
        assert_eq!(decode_compact_peer(&bytes), Some(addr));
    }

    #[test]
    fn compact_node_round_trip() {
        let node = CompactNode {
            id: id(ID_A),
            addr: "10.0.0.1:51413".parse().unwrap(),
        };
        let bytes = encode_compact_node(&node).unwrap();
        assert_eq!(bytes.len(), 26);
        assert_eq!(decode_compact_nodes(&bytes), vec![node]);
    }

    #[test]
    fn decode_drops_garbage() {
        assert_eq!(decode(b""), Err(DecodeError::NotBencode));
        assert_eq!(decode(b"not bencode"), Err(DecodeError::NotBencode));
        assert_eq!(decode(b"i42e"), Err(DecodeError::NotADict));
        assert_eq!(decode(b"de"), Err(DecodeError::MissingTransaction));
        assert!(decode(&[0xff; 64]).is_err());
    }

    #[test]
    fn peek_helpers() {
        let q = b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe";
        assert!(peek_is_query(q));
        assert_eq!(peek_transaction_id(q).as_deref(), Some(&b"aa"[..]));
        assert!(!peek_is_query(b"i1e"));
        assert!(peek_transaction_id(b"xxx").is_none());
    }
}
