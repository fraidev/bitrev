use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use crate::message::{format_extended, Message};
use crate::peer::PeerAddr;
use crate::peer_state::PeerStates;

use super::handshake::{ExtensionHandshake, PeerExtensionInfo};
use super::ut_metadata::MetadataStore;

#[derive(Clone)]
pub struct ExtensionContext {
    pub info_hash: [u8; 20],
    pub peer: PeerAddr,
    pub metadata: Arc<MetadataStore>,
    pub peer_states: Arc<PeerStates>,
}

impl ExtensionContext {
    pub fn probe() -> Self {
        Self {
            info_hash: [0; 20],
            peer: "0.0.0.0:0".parse().expect("probe addr"),
            metadata: MetadataStore::new([0; 20]),
            peer_states: Arc::new(PeerStates::default()),
        }
    }
}

pub trait Extension: Send {
    fn name(&self) -> &str;
    fn on_handshake(&mut self, peer_info: &PeerExtensionInfo);
    fn on_message(&mut self, payload: &[u8]) -> Vec<Vec<u8>>;
    fn on_tick(&mut self) -> Vec<Vec<u8>> {
        Vec::new()
    }
    fn should_disconnect(&self) -> bool {
        false
    }
}

type ExtensionFactory = Arc<dyn Fn(&ExtensionContext) -> Box<dyn Extension> + Send + Sync>;

struct RegisteredExtension {
    name: String,
    local_id: u8,
    factory: ExtensionFactory,
}

#[derive(Clone, Default)]
pub struct ExtensionRegistry {
    inner: Arc<Mutex<Vec<RegisteredExtension>>>,
}

impl ExtensionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<F>(&self, factory: F) -> u8
    where
        F: Fn(&ExtensionContext) -> Box<dyn Extension> + Send + Sync + 'static,
    {
        let probe_ctx = ExtensionContext::probe();
        let probe = factory(&probe_ctx);
        let name = probe.name().to_string();
        drop(probe);

        let mut entries = self.inner.lock().unwrap();
        if let Some(existing) = entries.iter().find(|entry| entry.name == name) {
            return existing.local_id;
        }

        let local_id = u8::try_from(entries.len() + 1).unwrap_or(u8::MAX);
        entries.push(RegisteredExtension {
            name,
            local_id,
            factory: Arc::new(factory),
        });
        local_id
    }

    pub fn contains(&self, name: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry.name == name)
    }

    pub fn local_map(&self) -> BTreeMap<String, i64> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|entry| (entry.name.clone(), i64::from(entry.local_id)))
            .collect()
    }

    pub fn bind(&self, ctx: &ExtensionContext) -> ExtensionSession {
        let entries = self.inner.lock().unwrap();
        let mut local_ids = BTreeMap::new();
        let mut by_local_id = HashMap::new();
        for entry in entries.iter() {
            local_ids.insert(entry.name.clone(), entry.local_id);
            by_local_id.insert(entry.local_id, (entry.factory)(ctx));
        }
        ExtensionSession {
            local_ids,
            by_local_id,
            peer: PeerExtensionInfo::default(),
        }
    }
}

pub struct ExtensionSession {
    local_ids: BTreeMap<String, u8>,
    by_local_id: HashMap<u8, Box<dyn Extension>>,
    peer: PeerExtensionInfo,
}

impl ExtensionSession {
    pub fn peer_info(&self) -> &PeerExtensionInfo {
        &self.peer
    }

    pub fn local_id(&self, name: &str) -> Option<u8> {
        self.local_ids.get(name).copied()
    }

    pub fn outgoing_handshake(
        &self,
        listen_port: Option<u16>,
        metadata_size: Option<i64>,
    ) -> Message {
        let handshake = ExtensionHandshake::outgoing(
            self.local_ids
                .iter()
                .map(|(name, id)| (name.clone(), i64::from(*id)))
                .collect(),
            listen_port,
            metadata_size,
        );
        format_extended(0, handshake.encode())
    }

    pub fn encode_outgoing(&self, name: &str, payload: Vec<u8>) -> Option<Message> {
        let ext_id = self.peer.peer_ext_id(name)?;
        Some(format_extended(ext_id, payload))
    }

    pub fn handle_extended(&mut self, ext_id: u8, payload: Vec<u8>) -> Vec<Message> {
        if ext_id == 0 {
            let handshake = ExtensionHandshake::decode(&payload);
            self.peer.merge(&handshake);
            let info = self.peer.clone();
            for ext in self.by_local_id.values_mut() {
                ext.on_handshake(&info);
            }
            return self.collect_ticks();
        }

        let (name, replies) = {
            let Some(ext) = self.by_local_id.get_mut(&ext_id) else {
                return Vec::new();
            };
            (ext.name().to_string(), ext.on_message(&payload))
        };
        replies
            .into_iter()
            .filter_map(|payload| self.encode_outgoing(&name, payload))
            .collect()
    }

    pub fn on_tick(&mut self) -> Vec<Message> {
        self.collect_ticks()
    }

    pub fn should_disconnect(&self) -> bool {
        self.by_local_id.values().any(|ext| ext.should_disconnect())
    }

    fn collect_ticks(&mut self) -> Vec<Message> {
        let ticks: Vec<(String, Vec<Vec<u8>>)> = self
            .by_local_id
            .values_mut()
            .map(|ext| (ext.name().to_string(), ext.on_tick()))
            .collect();
        let mut out = Vec::new();
        for (name, replies) in ticks {
            for payload in replies {
                if let Some(msg) = self.encode_outgoing(&name, payload) {
                    out.push(msg);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct DummyExt {
        name: &'static str,
        seen: Arc<Mutex<Vec<Vec<u8>>>>,
        handshakes: Arc<Mutex<Vec<PeerExtensionInfo>>>,
        replies: Vec<Vec<u8>>,
    }

    impl Extension for DummyExt {
        fn name(&self) -> &str {
            self.name
        }

        fn on_handshake(&mut self, peer_info: &PeerExtensionInfo) {
            self.handshakes.lock().unwrap().push(peer_info.clone());
        }

        fn on_message(&mut self, payload: &[u8]) -> Vec<Vec<u8>> {
            self.seen.lock().unwrap().push(payload.to_vec());
            self.replies.clone()
        }
    }

    fn registry_with_dummy(
        seen: Arc<Mutex<Vec<Vec<u8>>>>,
        handshakes: Arc<Mutex<Vec<PeerExtensionInfo>>>,
        replies: Vec<Vec<u8>>,
    ) -> ExtensionRegistry {
        let registry = ExtensionRegistry::new();
        registry.register(move |_ctx| {
            Box::new(DummyExt {
                name: "ut_dummy",
                seen: seen.clone(),
                handshakes: handshakes.clone(),
                replies: replies.clone(),
            })
        });
        registry
    }

    #[test]
    fn assigns_local_ids_starting_at_one() {
        let registry = ExtensionRegistry::new();
        let first = registry.register(|_ctx| {
            Box::new(DummyExt {
                name: "ut_a",
                seen: Arc::new(Mutex::new(Vec::new())),
                handshakes: Arc::new(Mutex::new(Vec::new())),
                replies: Vec::new(),
            })
        });
        let second = registry.register(|_ctx| {
            Box::new(DummyExt {
                name: "ut_b",
                seen: Arc::new(Mutex::new(Vec::new())),
                handshakes: Arc::new(Mutex::new(Vec::new())),
                replies: Vec::new(),
            })
        });
        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(registry.local_map().get("ut_a").copied(), Some(1));
        assert_eq!(registry.local_map().get("ut_b").copied(), Some(2));
    }

    #[test]
    fn dispatch_uses_peer_advertised_id_and_ignores_unknown() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handshakes = Arc::new(Mutex::new(Vec::new()));
        let registry =
            registry_with_dummy(seen.clone(), handshakes.clone(), vec![b"pong".to_vec()]);
        let mut session = registry.bind(&ExtensionContext::probe());

        let mut m = BTreeMap::new();
        m.insert("ut_dummy".into(), 7);
        let peer_hs = ExtensionHandshake {
            m,
            v: Some("peer".into()),
            ..ExtensionHandshake::default()
        };
        assert!(session.handle_extended(0, peer_hs.encode()).is_empty());
        assert_eq!(handshakes.lock().unwrap().len(), 1);
        assert_eq!(session.peer_info().peer_ext_id("ut_dummy"), Some(7));

        let outgoing = session.handle_extended(1, b"ping".to_vec());
        assert_eq!(seen.lock().unwrap().as_slice(), [b"ping".to_vec()]);
        assert_eq!(
            outgoing,
            vec![Message::Extended {
                ext_id: 7,
                payload: b"pong".to_vec(),
            }]
        );

        assert!(session.handle_extended(99, b"nope".to_vec()).is_empty());
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn outgoing_uses_peer_map_never_local_id() {
        let registry = registry_with_dummy(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
            Vec::new(),
        );
        let mut session = registry.bind(&ExtensionContext::probe());
        assert_eq!(session.local_id("ut_dummy"), Some(1));
        assert!(session.encode_outgoing("ut_dummy", b"x".to_vec()).is_none());

        let mut m = BTreeMap::new();
        m.insert("ut_dummy".into(), 9);
        session.handle_extended(
            0,
            ExtensionHandshake {
                m,
                ..ExtensionHandshake::default()
            }
            .encode(),
        );

        let msg = session.encode_outgoing("ut_dummy", b"x".to_vec()).unwrap();
        assert_eq!(
            msg,
            Message::Extended {
                ext_id: 9,
                payload: b"x".to_vec(),
            }
        );
    }
}
