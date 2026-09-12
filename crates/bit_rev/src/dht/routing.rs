//! Kademlia routing table (BEP-0005).
//!
//! XOR distance, K=8, split only the bucket that contains our node ID.
//! Node states follow the 15-minute good / questionable / bad rules.

use std::cmp::Ordering;
use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

use super::krpc::{decode_compact_nodes, encode_compact_nodes, CompactNode, NodeId, NODE_ID_LEN};

pub const K: usize = 8;
pub const NODE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
pub const BAD_FAILS: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    Good,
    Questionable,
    Bad,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: NodeId,
    pub addr: SocketAddr,
    last_response: Option<Instant>,
    last_query: Option<Instant>,
    fails: u8,
    ever_responded: bool,
}

impl Node {
    pub fn new(id: NodeId, addr: SocketAddr, now: Instant) -> Self {
        Self {
            id,
            addr,
            last_response: None,
            last_query: Some(now),
            fails: 0,
            ever_responded: false,
        }
    }

    pub fn from_compact(node: CompactNode, now: Instant) -> Self {
        Self::new(node.id, node.addr, now)
    }

    pub fn contact(&self) -> CompactNode {
        CompactNode {
            id: self.id,
            addr: self.addr,
        }
    }

    pub fn status(&self, now: Instant) -> NodeStatus {
        if self.fails >= BAD_FAILS {
            return NodeStatus::Bad;
        }
        let responded_recently = self
            .last_response
            .is_some_and(|t| now.saturating_duration_since(t) < NODE_TIMEOUT);
        let queried_recently = self
            .last_query
            .is_some_and(|t| now.saturating_duration_since(t) < NODE_TIMEOUT);
        if responded_recently || (self.ever_responded && queried_recently) {
            NodeStatus::Good
        } else {
            NodeStatus::Questionable
        }
    }

    pub fn on_response(&mut self, now: Instant) {
        self.last_response = Some(now);
        self.ever_responded = true;
        self.fails = 0;
    }

    pub fn on_query(&mut self, now: Instant) {
        self.last_query = Some(now);
    }

    pub fn on_timeout(&mut self) {
        self.fails = self.fails.saturating_add(1);
    }
}

#[derive(Debug, Clone)]
pub struct Bucket {
    /// Shared prefix length of IDs in this bucket (0 = whole space).
    prefix_len: u8,
    /// First `prefix_len` bits identify the range.
    prefix: NodeId,
    nodes: Vec<Node>,
    last_changed: Instant,
}

impl Bucket {
    fn covers(&self, id: &NodeId) -> bool {
        matching_prefix_bits(&self.prefix, id) >= u32::from(self.prefix_len)
    }

    fn contains_id(&self, id: &NodeId) -> bool {
        self.covers(id)
    }

    pub fn is_stale(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_changed) >= NODE_TIMEOUT
    }

    pub fn random_id(&self) -> NodeId {
        let mut id = [0u8; NODE_ID_LEN];
        rand::thread_rng().fill(&mut id);
        copy_prefix(&self.prefix, &mut id, self.prefix_len);
        id
    }

    fn split(self, now: Instant) -> (Bucket, Bucket) {
        let mut left_prefix = self.prefix;
        let mut right_prefix = self.prefix;
        set_bit(&mut left_prefix, self.prefix_len, false);
        set_bit(&mut right_prefix, self.prefix_len, true);
        let next = self.prefix_len + 1;
        let mut left = Bucket {
            prefix_len: next,
            prefix: left_prefix,
            nodes: Vec::new(),
            last_changed: now,
        };
        let mut right = Bucket {
            prefix_len: next,
            prefix: right_prefix,
            nodes: Vec::new(),
            last_changed: now,
        };
        for node in self.nodes {
            if left.covers(&node.id) {
                left.nodes.push(node);
            } else {
                right.nodes.push(node);
            }
        }
        (left, right)
    }
}

#[derive(Debug, Clone)]
pub enum InsertResult {
    Added,
    Updated,
    Split,
    Replaced,
    Dropped,
    NeedPing {
        addr: SocketAddr,
        replacement: CompactNode,
    },
}

#[derive(Debug, Clone)]
pub struct RoutingTable {
    id: NodeId,
    buckets: Vec<Bucket>,
}

impl RoutingTable {
    pub fn new(id: NodeId, now: Instant) -> Self {
        Self {
            id,
            buckets: vec![Bucket {
                prefix_len: 0,
                prefix: [0u8; NODE_ID_LEN],
                nodes: Vec::new(),
                last_changed: now,
            }],
        }
    }

    pub fn random_id() -> NodeId {
        let mut id = [0u8; NODE_ID_LEN];
        rand::thread_rng().fill(&mut id);
        id
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.iter().all(|b| b.nodes.is_empty())
    }

    pub fn len(&self) -> usize {
        self.buckets.iter().map(|b| b.nodes.len()).sum()
    }

    pub fn good_len(&self, now: Instant) -> usize {
        self.nodes()
            .filter(|n| n.status(now) == NodeStatus::Good)
            .count()
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.buckets.iter().flat_map(|b| b.nodes.iter())
    }

    pub fn nodes_mut(&mut self) -> impl Iterator<Item = &mut Node> {
        self.buckets.iter_mut().flat_map(|b| b.nodes.iter_mut())
    }

    pub fn good_contacts(&self, now: Instant) -> Vec<CompactNode> {
        self.nodes()
            .filter(|n| n.status(now) == NodeStatus::Good)
            .map(Node::contact)
            .collect()
    }

    pub fn stale_buckets(&self, now: Instant) -> Vec<NodeId> {
        self.buckets
            .iter()
            .filter(|b| b.is_stale(now) && !b.nodes.is_empty())
            .map(Bucket::random_id)
            .collect()
    }

    pub fn insert(&mut self, node: CompactNode, now: Instant) -> InsertResult {
        if node.id == self.id {
            return InsertResult::Dropped;
        }
        if !matches!(node.addr, SocketAddr::V4(_)) {
            return InsertResult::Dropped;
        }
        if let Some(existing) = self.find_mut(&node.id) {
            existing.addr = node.addr;
            existing.on_response(now);
            if let Some(bucket) = self.bucket_mut_for(&node.id) {
                bucket.last_changed = now;
            }
            return InsertResult::Updated;
        }

        loop {
            let idx = self.bucket_index(&node.id);
            let our_bucket = self.buckets[idx].contains_id(&self.id);
            if self.buckets[idx].nodes.len() < K {
                self.buckets[idx].nodes.push(Node::from_compact(node, now));
                self.buckets[idx].last_changed = now;
                return InsertResult::Added;
            }

            if our_bucket && self.buckets[idx].prefix_len < 160 {
                let bucket = self.buckets.remove(idx);
                let (left, right) = bucket.split(now);
                self.buckets.insert(idx, left);
                self.buckets.insert(idx + 1, right);
                continue;
            }

            let bucket = &mut self.buckets[idx];
            if let Some(pos) = bucket
                .nodes
                .iter()
                .position(|n| n.status(now) == NodeStatus::Bad)
            {
                bucket.nodes[pos] = Node::from_compact(node, now);
                bucket.last_changed = now;
                return InsertResult::Replaced;
            }

            if let Some(pos) = bucket
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.status(now) == NodeStatus::Questionable)
                .min_by_key(|(_, n)| n.last_response.or(n.last_query))
                .map(|(i, _)| i)
            {
                return InsertResult::NeedPing {
                    addr: bucket.nodes[pos].addr,
                    replacement: node,
                };
            }

            return InsertResult::Dropped;
        }
    }

    pub fn replace(&mut self, old: SocketAddr, new: CompactNode, now: Instant) -> bool {
        for bucket in &mut self.buckets {
            if let Some(pos) = bucket.nodes.iter().position(|n| n.addr == old) {
                bucket.nodes[pos] = Node::from_compact(new, now);
                bucket.last_changed = now;
                return true;
            }
        }
        false
    }

    pub fn seen_query(&mut self, id: NodeId, addr: SocketAddr, now: Instant) {
        if let Some(node) = self.find_mut(&id) {
            node.addr = addr;
            node.on_query(now);
            return;
        }
        let _ = self.insert(CompactNode { id, addr }, now);
    }

    pub fn seen_response(&mut self, id: NodeId, addr: SocketAddr, now: Instant) {
        if let Some(node) = self.find_mut(&id) {
            node.addr = addr;
            node.on_response(now);
            if let Some(bucket) = self.bucket_mut_for(&id) {
                bucket.last_changed = now;
            }
            return;
        }
        let mut node = Node::new(id, addr, now);
        node.on_response(now);
        let contact = node.contact();
        match self.insert(contact, now) {
            InsertResult::Added | InsertResult::Updated | InsertResult::Replaced => {}
            _ => {}
        }
        if let Some(existing) = self.find_mut(&id) {
            existing.on_response(now);
        }
    }

    pub fn timed_out(&mut self, addr: SocketAddr) {
        for bucket in &mut self.buckets {
            if let Some(node) = bucket.nodes.iter_mut().find(|n| n.addr == addr) {
                node.on_timeout();
                return;
            }
        }
    }

    pub fn closest(&self, target: &NodeId, count: usize) -> Vec<CompactNode> {
        let mut nodes: Vec<&Node> = self.nodes().collect();
        nodes.sort_by(|a, b| cmp_xor(&a.id, &b.id, target));
        nodes.into_iter().take(count).map(Node::contact).collect()
    }

    pub fn find(&self, id: &NodeId) -> Option<&Node> {
        self.nodes().find(|n| n.id == *id)
    }

    fn find_mut(&mut self, id: &NodeId) -> Option<&mut Node> {
        self.nodes_mut().find(|n| n.id == *id)
    }

    fn bucket_index(&self, id: &NodeId) -> usize {
        self.buckets
            .iter()
            .position(|b| b.covers(id))
            .expect("buckets cover the full space")
    }

    fn bucket_mut_for(&mut self, id: &NodeId) -> Option<&mut Bucket> {
        let idx = self.bucket_index(id);
        self.buckets.get_mut(idx)
    }

    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }
}

pub fn xor_distance(a: &NodeId, b: &NodeId) -> NodeId {
    let mut out = [0u8; NODE_ID_LEN];
    for i in 0..NODE_ID_LEN {
        out[i] = a[i] ^ b[i];
    }
    out
}

pub fn cmp_xor(a: &NodeId, b: &NodeId, target: &NodeId) -> Ordering {
    xor_distance(a, target).cmp(&xor_distance(b, target))
}

fn matching_prefix_bits(a: &NodeId, b: &NodeId) -> u32 {
    let mut bits = 0u32;
    for i in 0..NODE_ID_LEN {
        let x = a[i] ^ b[i];
        if x == 0 {
            bits += 8;
            continue;
        }
        bits += x.leading_zeros();
        break;
    }
    bits.min(160)
}

fn set_bit(id: &mut NodeId, bit: u8, value: bool) {
    let byte = (bit / 8) as usize;
    let shift = 7 - (bit % 8);
    if value {
        id[byte] |= 1 << shift;
    } else {
        id[byte] &= !(1 << shift);
    }
}

fn copy_prefix(from: &NodeId, to: &mut NodeId, prefix_len: u8) {
    if prefix_len == 0 {
        return;
    }
    let full = (prefix_len / 8) as usize;
    to[..full].copy_from_slice(&from[..full]);
    let rem = prefix_len % 8;
    if rem > 0 {
        let mask = !((1u8 << (8 - rem)) - 1);
        to[full] = (to[full] & !mask) | (from[full] & mask);
    }
}

#[derive(Serialize, Deserialize)]
struct DhtDat {
    #[serde(with = "serde_bytes")]
    id: Vec<u8>,
    #[serde(default)]
    nodes: ByteBuf,
}

pub fn persist_path(state_dir: &Path) -> std::path::PathBuf {
    util::paths::dht_dat(state_dir)
}

pub fn save(table: &RoutingTable, path: &Path, now: Instant) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let nodes = encode_compact_nodes(&table.good_contacts(now));
    let dat = DhtDat {
        id: table.id.to_vec(),
        nodes: ByteBuf::from(nodes),
    };
    let bytes = serde_bencode::to_bytes(&dat).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("dat.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub fn load(path: &Path, now: Instant) -> Option<RoutingTable> {
    let bytes = std::fs::read(path).ok()?;
    let dat: DhtDat = serde_bencode::from_bytes(&bytes).ok()?;
    let id: NodeId = dat.id.as_slice().try_into().ok()?;
    let mut table = RoutingTable::new(id, now);
    for node in decode_compact_nodes(&dat.nodes) {
        if let Some(existing) = table.find_mut(&node.id) {
            existing.on_response(now);
        } else {
            let mut stored = Node::from_compact(node, now);
            stored.on_response(now);
            let _ = table.insert(stored.contact(), now);
            if let Some(n) = table.find_mut(&stored.id) {
                n.on_response(now);
            }
        }
    }
    Some(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn now() -> Instant {
        Instant::now()
    }

    fn id_with_prefix(first: u8) -> NodeId {
        let mut id = [0u8; 20];
        id[0] = first;
        id
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), port))
    }

    fn contact(first: u8, port: u16) -> CompactNode {
        CompactNode {
            id: id_with_prefix(first),
            addr: addr(port),
        }
    }

    #[test]
    fn xor_orders_closer_ids() {
        let target = id_with_prefix(0b0000_0000);
        let close = id_with_prefix(0b0000_0001);
        let far = id_with_prefix(0b1000_0000);
        assert_eq!(cmp_xor(&close, &far, &target), Ordering::Less);
    }

    #[test]
    fn insert_then_closest() {
        let mut table = RoutingTable::new(id_with_prefix(0), now());
        for i in 1..=5 {
            assert!(matches!(
                table.insert(contact(i, 6880 + u16::from(i)), now()),
                InsertResult::Added
            ));
        }
        let closest = table.closest(&id_with_prefix(1), 3);
        assert_eq!(closest.len(), 3);
        assert_eq!(closest[0].id, id_with_prefix(1));
    }

    #[test]
    fn splits_only_bucket_containing_our_id() {
        let our = id_with_prefix(0);
        let mut table = RoutingTable::new(our, now());
        for i in 0..K {
            let mut id = [0u8; 20];
            id[19] = i as u8 + 1;
            table.insert(
                CompactNode {
                    id,
                    addr: addr(7000 + i as u16),
                },
                now(),
            );
        }
        assert_eq!(table.bucket_count(), 1);
        let mut extra = [0u8; 20];
        extra[19] = 9;
        table.insert(
            CompactNode {
                id: extra,
                addr: addr(7010),
            },
            now(),
        );
        assert!(
            table.bucket_count() > 1,
            "full bucket containing us must split"
        );

        let mut far = RoutingTable::new(id_with_prefix(0), now());
        for i in 0..K {
            let mut id = [0u8; 20];
            id[19] = i as u8 + 1;
            far.insert(
                CompactNode {
                    id,
                    addr: addr(8000 + i as u16),
                },
                now(),
            );
        }
        let mut extra = [0u8; 20];
        extra[19] = 9;
        far.insert(
            CompactNode {
                id: extra,
                addr: addr(8010),
            },
            now(),
        );
        let buckets_after_our_split = far.bucket_count();
        assert!(buckets_after_our_split > 1);
        for i in 0..K {
            far.insert(contact(0x80 | i as u8, 8100 + i as u16), now());
        }
        let before = far.bucket_count();
        let dropped = far.insert(contact(0x8F, 8199), now());
        assert!(
            matches!(
                dropped,
                InsertResult::Dropped | InsertResult::NeedPing { .. } | InsertResult::Replaced
            ),
            "full remote bucket must not split, got {dropped:?}"
        );
        assert_eq!(far.bucket_count(), before);
    }

    #[test]
    fn replaces_bad_node() {
        let mut table = RoutingTable::new(id_with_prefix(0), now());
        for i in 0..K {
            table.insert(contact(0x80 | i as u8, 9000 + i as u16), now());
        }
        {
            let node = table.nodes_mut().next().unwrap();
            node.fails = BAD_FAILS;
        }
        let result = table.insert(contact(0x8F, 9099), now());
        assert!(matches!(result, InsertResult::Replaced));
        assert!(table.find(&id_with_prefix(0x8F)).is_some());
    }

    #[test]
    fn pings_questionable_before_eviction() {
        let mut table = RoutingTable::new(id_with_prefix(0), now());
        let stale = now() - NODE_TIMEOUT - Duration::from_secs(1);
        for i in 0..K {
            table.insert(contact(0x80 | i as u8, 9100 + i as u16), now());
        }
        for node in table.nodes_mut() {
            node.last_response = Some(stale);
            node.last_query = Some(stale);
            node.ever_responded = true;
        }
        let result = table.insert(contact(0x8F, 9199), now());
        assert!(matches!(result, InsertResult::NeedPing { .. }));
    }

    #[test]
    fn persist_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = persist_path(dir.path());
        let now = now();
        let mut table = RoutingTable::new(id_with_prefix(1), now);
        table.insert(contact(2, 6881), now);
        if let Some(n) = table.find_mut(&id_with_prefix(2)) {
            n.on_response(now);
        }
        save(&table, &path, now).unwrap();
        let loaded = load(&path, now).expect("load dht.dat");
        assert_eq!(loaded.id(), table.id());
        assert!(loaded.find(&id_with_prefix(2)).is_some());
    }
}
