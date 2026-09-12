//! Iterative DHT lookup (alpha=3, terminate on K closest).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use super::krpc::{CompactNode, NodeId, Response};
use super::routing::{cmp_xor, K};

pub const ALPHA: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupKind {
    FindNode,
    GetPeers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContactState {
    Unknown,
    Pending,
    Responded,
    Failed,
}

#[derive(Debug, Clone)]
struct Contact {
    node: CompactNode,
    state: ContactState,
    token: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Lookup {
    pub target: NodeId,
    pub kind: LookupKind,
    contacts: HashMap<SocketAddr, Contact>,
    seen_ids: HashSet<NodeId>,
    values: Vec<SocketAddr>,
}

impl Lookup {
    pub fn new(
        target: NodeId,
        kind: LookupKind,
        seeds: impl IntoIterator<Item = CompactNode>,
    ) -> Self {
        let mut lookup = Self {
            target,
            kind,
            contacts: HashMap::new(),
            seen_ids: HashSet::new(),
            values: Vec::new(),
        };
        for seed in seeds {
            lookup.add_contact(seed);
        }
        lookup
    }

    pub fn add_contact(&mut self, node: CompactNode) {
        if self.seen_ids.contains(&node.id) {
            return;
        }
        if self.contacts.contains_key(&node.addr) {
            return;
        }
        self.seen_ids.insert(node.id);
        self.contacts.insert(
            node.addr,
            Contact {
                node,
                state: ContactState::Unknown,
                token: None,
            },
        );
    }

    pub fn next_queries(&mut self) -> Vec<CompactNode> {
        let mut unknown: Vec<&CompactNode> = self
            .contacts
            .values()
            .filter(|c| c.state == ContactState::Unknown)
            .map(|c| &c.node)
            .collect();
        unknown.sort_by(|a, b| cmp_xor(&a.id, &b.id, &self.target));
        let pending = self
            .contacts
            .values()
            .filter(|c| c.state == ContactState::Pending)
            .count();
        let want = ALPHA.saturating_sub(pending);
        let chosen: Vec<CompactNode> = unknown.into_iter().take(want).cloned().collect();
        for node in &chosen {
            if let Some(c) = self.contacts.get_mut(&node.addr) {
                c.state = ContactState::Pending;
            }
        }
        chosen
    }

    pub fn on_timeout(&mut self, addr: SocketAddr) {
        if let Some(c) = self.contacts.get_mut(&addr) {
            if c.state == ContactState::Pending {
                c.state = ContactState::Failed;
            }
        }
    }

    pub fn on_response(
        &mut self,
        from: SocketAddr,
        id: NodeId,
        response: &Response,
    ) -> Vec<SocketAddr> {
        if let Some(c) = self.contacts.get_mut(&from) {
            c.state = ContactState::Responded;
            c.node.id = id;
            c.token = response.token.clone();
        } else {
            self.contacts.insert(
                from,
                Contact {
                    node: CompactNode { id, addr: from },
                    state: ContactState::Responded,
                    token: response.token.clone(),
                },
            );
            self.seen_ids.insert(id);
        }
        for node in &response.nodes {
            self.add_contact(node.clone());
        }
        let mut new_values = Vec::new();
        for addr in &response.values {
            if !self.values.contains(addr) {
                self.values.push(*addr);
                new_values.push(*addr);
            }
        }
        new_values
    }

    pub fn is_done(&self) -> bool {
        if self
            .contacts
            .values()
            .any(|c| c.state == ContactState::Pending)
        {
            return false;
        }
        if self
            .contacts
            .values()
            .any(|c| c.state == ContactState::Unknown)
        {
            let closest = self.closest_contacts(K);
            if closest
                .iter()
                .any(|c| c.state == ContactState::Unknown || c.state == ContactState::Pending)
            {
                return false;
            }
            let worst = closest.last().map(|c| c.node.id);
            if let Some(worst) = worst {
                let has_closer_unknown = self.contacts.values().any(|c| {
                    c.state == ContactState::Unknown
                        && cmp_xor(&c.node.id, &worst, &self.target).is_lt()
                });
                if has_closer_unknown {
                    return false;
                }
            }
            return true;
        }
        true
    }

    pub fn values(&self) -> &[SocketAddr] {
        &self.values
    }

    pub fn announce_targets(&self) -> Vec<(CompactNode, Vec<u8>)> {
        self.closest_contacts(K)
            .into_iter()
            .filter_map(|c| {
                let token = c.token.clone()?;
                Some((c.node.clone(), token))
            })
            .collect()
    }

    fn closest_contacts(&self, count: usize) -> Vec<&Contact> {
        let mut contacts: Vec<&Contact> = self.contacts.values().collect();
        contacts.sort_by(|a, b| cmp_xor(&a.node.id, &b.node.id, &self.target));
        contacts.into_iter().take(count).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn id(byte: u8) -> NodeId {
        [byte; 20]
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    fn node(byte: u8, port: u16) -> CompactNode {
        CompactNode {
            id: id(byte),
            addr: addr(port),
        }
    }

    fn resp(
        id_byte: u8,
        nodes: Vec<CompactNode>,
        values: Vec<SocketAddr>,
        token: bool,
    ) -> Response {
        Response {
            id: id(id_byte),
            nodes,
            values,
            token: token.then(|| b"tok".to_vec()),
        }
    }

    #[test]
    fn converges_on_closer_nodes_and_collects_values() {
        let target = id(0);
        let seed = node(0x80, 1);
        let mut lookup = Lookup::new(target, LookupKind::GetPeers, [seed.clone()]);

        let first = lookup.next_queries();
        assert_eq!(first, vec![seed.clone()]);

        let closer = node(0x10, 2);
        let even_closer = node(0x01, 3);
        let peer: SocketAddr = "10.0.0.5:51413".parse().unwrap();
        let new = lookup.on_response(
            seed.addr,
            seed.id,
            &resp(
                0x80,
                vec![closer.clone(), even_closer.clone()],
                vec![],
                true,
            ),
        );
        assert!(new.is_empty());

        let next = lookup.next_queries();
        assert_eq!(next.len(), 2);
        assert!(next.contains(&closer));
        assert!(next.contains(&even_closer));

        lookup.on_response(
            even_closer.addr,
            even_closer.id,
            &resp(0x01, vec![], vec![peer], true),
        );
        lookup.on_response(closer.addr, closer.id, &resp(0x10, vec![], vec![], true));

        assert!(lookup.is_done());
        assert_eq!(lookup.values(), &[peer]);
        let announce = lookup.announce_targets();
        assert!(announce.iter().any(|(n, _)| n.id == even_closer.id));
    }

    #[test]
    fn alpha_limits_in_flight() {
        let seeds: Vec<_> = (1..=6).map(|i| node(i * 10, i as u16)).collect();
        let mut lookup = Lookup::new(id(0), LookupKind::FindNode, seeds);
        let first = lookup.next_queries();
        assert_eq!(first.len(), ALPHA);
        let second = lookup.next_queries();
        assert!(second.is_empty(), "do not exceed alpha in-flight");
    }

    #[test]
    fn timeout_marks_failed_and_continues() {
        let seed = node(0x40, 9);
        let backup = node(0x20, 10);
        let mut lookup = Lookup::new(id(0), LookupKind::FindNode, [seed.clone()]);
        let first = lookup.next_queries();
        assert_eq!(first, vec![seed.clone()]);
        lookup.on_timeout(seed.addr);
        lookup.add_contact(backup.clone());
        let next = lookup.next_queries();
        assert_eq!(next, vec![backup]);
    }
}
