//! Peer discovery source gating (BEP-0027).
//!
//! All peer sources register here. Private torrents accept only trackers
//! listed in the metainfo. DHT, PEX, and LSD must go through this registry
//! so they stay off for private torrents from day one.

use std::collections::BTreeSet;
use std::fmt;

/// How a torrent learns about peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiscoverySource {
    /// HTTP or UDP tracker URLs from `announce` / `announce-list`.
    Tracker,
    /// Mainline DHT (BEP-0005).
    Dht,
    /// Peer exchange (BEP-0011).
    Pex,
    /// Local service / peer discovery (BEP-0014).
    Lsd,
}

impl DiscoverySource {
    pub const ALL: [Self; 4] = [Self::Tracker, Self::Dht, Self::Pex, Self::Lsd];

    /// Non-tracker sources that BEP-0027 forbids on private torrents.
    pub const PRIVATE_DISABLED: [Self; 3] = [Self::Dht, Self::Pex, Self::Lsd];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tracker => "tracker",
            Self::Dht => "DHT",
            Self::Pex => "PEX",
            Self::Lsd => "LSD",
        }
    }

    pub fn is_tracker(self) -> bool {
        matches!(self, Self::Tracker)
    }

    /// Whether this source may be used for a torrent with the given privacy.
    pub fn allowed_for(self, is_private: bool) -> bool {
        !is_private || self.is_tracker()
    }
}

impl fmt::Display for DiscoverySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A peer source was refused for a private torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceDenied {
    pub source: DiscoverySource,
}

impl fmt::Display for SourceDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is disabled for private torrents (BEP-0027)",
            self.source
        )
    }
}

impl std::error::Error for SourceDenied {}

/// Single choke point for registering peer discovery sources on a torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRegistry {
    private: bool,
    registered: BTreeSet<DiscoverySource>,
}

impl SourceRegistry {
    pub fn new(is_private: bool) -> Self {
        Self {
            private: is_private,
            registered: BTreeSet::new(),
        }
    }

    pub fn is_private(&self) -> bool {
        self.private
    }

    pub fn allows(&self, source: DiscoverySource) -> bool {
        source.allowed_for(self.private)
    }

    pub fn allows_dht(&self) -> bool {
        self.allows(DiscoverySource::Dht)
    }

    pub fn allows_pex(&self) -> bool {
        self.allows(DiscoverySource::Pex)
    }

    pub fn allows_lsd(&self) -> bool {
        self.allows(DiscoverySource::Lsd)
    }

    /// Sources BEP-0027 disables when this torrent is private.
    pub fn disabled_sources(&self) -> &'static [DiscoverySource] {
        if self.private {
            &DiscoverySource::PRIVATE_DISABLED
        } else {
            &[]
        }
    }

    /// Register a source. Private torrents accept only metainfo trackers.
    pub fn register(&mut self, source: DiscoverySource) -> Result<(), SourceDenied> {
        if !self.allows(source) {
            return Err(SourceDenied { source });
        }
        self.registered.insert(source);
        Ok(())
    }

    pub fn is_registered(&self, source: DiscoverySource) -> bool {
        self.registered.contains(&source)
    }

    pub fn registered(&self) -> impl Iterator<Item = DiscoverySource> + '_ {
        self.registered.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_registry_accepts_every_source() {
        let mut registry = SourceRegistry::new(false);
        assert!(!registry.is_private());
        assert!(registry.disabled_sources().is_empty());
        for source in DiscoverySource::ALL {
            assert!(registry.allows(source), "{source} should be allowed");
            assert!(registry.register(source).is_ok());
            assert!(registry.is_registered(source));
        }
        assert!(registry.allows_dht());
        assert!(registry.allows_pex());
        assert!(registry.allows_lsd());
    }

    #[test]
    fn private_registry_refuses_non_tracker_sources() {
        let mut registry = SourceRegistry::new(true);
        assert!(registry.is_private());
        assert_eq!(
            registry.disabled_sources(),
            &DiscoverySource::PRIVATE_DISABLED
        );

        assert!(registry.register(DiscoverySource::Tracker).is_ok());
        assert!(registry.is_registered(DiscoverySource::Tracker));

        for source in DiscoverySource::PRIVATE_DISABLED {
            let err = registry
                .register(source)
                .expect_err("non-tracker source must be refused");
            assert_eq!(err, SourceDenied { source });
            assert!(!registry.is_registered(source));
            assert!(!registry.allows(source));
        }

        assert!(!registry.allows_dht());
        assert!(!registry.allows_pex());
        assert!(!registry.allows_lsd());
    }
}
