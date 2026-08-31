use crate::discovery::{DiscoverySource, SourceRegistry};
use crate::file::TorrentMeta;

use anyhow::Result;

#[derive(Debug, Clone, PartialEq)]
pub struct Torrent {
    pub info_hash: [u8; 20],
    pub piece_hashes: Vec<[u8; 20]>,
    pub piece_length: i64,
    pub length: i64,
    pub files: Vec<TorrentFileInfo>,
    pub name: String,
    pub(crate) private: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TorrentFileInfo {
    pub path: Vec<String>,
    pub length: i64,
    pub offset: i64,
}

impl Torrent {
    pub fn new(torrent_meta: &TorrentMeta) -> Result<Torrent> {
        let info = &torrent_meta.torrent_file.info;

        let (total_length, files) = if let Some(file_list) = &info.files {
            // Multi-file torrent
            let mut total = 0i64;
            let mut torrent_files = Vec::new();

            for file in file_list {
                torrent_files.push(TorrentFileInfo {
                    path: file.path.clone(),
                    length: file.length,
                    offset: total,
                });
                total += file.length;
            }

            (total, torrent_files)
        } else if let Some(length) = info.length {
            // Single-file torrent
            let single_file = TorrentFileInfo {
                path: vec![info.name.clone()],
                length,
                offset: 0,
            };

            (length, vec![single_file])
        } else {
            return Err(anyhow::anyhow!(
                "Invalid torrent file: missing length information"
            ));
        };

        Ok(Torrent {
            info_hash: torrent_meta.info_hash,
            piece_hashes: torrent_meta.piece_hashes.clone(),
            piece_length: info.piece_length,
            length: total_length,
            files,
            name: info.name.clone(),
            private: info.is_private(),
        })
    }

    pub fn is_private(&self) -> bool {
        self.private
    }

    pub fn allows_source(&self, source: DiscoverySource) -> bool {
        source.allowed_for(self.private)
    }

    pub fn allows_dht(&self) -> bool {
        self.allows_source(DiscoverySource::Dht)
    }

    pub fn allows_pex(&self) -> bool {
        self.allows_source(DiscoverySource::Pex)
    }

    pub fn allows_lsd(&self) -> bool {
        self.allows_source(DiscoverySource::Lsd)
    }

    pub fn source_registry(&self) -> SourceRegistry {
        SourceRegistry::new(self.private)
    }

    /// Sources that stay off for this torrent (empty when public).
    pub fn disabled_discovery_sources(&self) -> &'static [DiscoverySource] {
        self.source_registry().disabled_sources()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::{File, Info, TorrentFile};
    use serde_bytes::ByteBuf;

    fn meta_with_private(private: Option<u8>) -> TorrentMeta {
        TorrentMeta {
            torrent_file: TorrentFile {
                info: Info {
                    name: "test".into(),
                    pieces: ByteBuf::from(vec![0u8; 20]),
                    piece_length: 16384,
                    md5sum: None,
                    length: Some(16384),
                    files: None,
                    private,
                    path: None,
                    root_hash: None,
                },
                announce: Some("http://tracker.example/announce".into()),
                nodes: None,
                encoding: None,
                httpseeds: None,
                announce_list: None,
                creation_date: None,
                comment: None,
                created_by: None,
            },
            info_hash: [0u8; 20],
            piece_hashes: vec![[0u8; 20]],
        }
    }

    #[test]
    fn absent_and_zero_private_are_public() {
        for flag in [None, Some(0)] {
            let torrent = Torrent::new(&meta_with_private(flag)).expect("torrent");
            assert!(!torrent.is_private());
            assert!(torrent.allows_dht());
            assert!(torrent.allows_pex());
            assert!(torrent.allows_lsd());
            assert!(torrent.disabled_discovery_sources().is_empty());
        }
    }

    #[test]
    fn private_flag_disables_non_tracker_sources() {
        let torrent = Torrent::new(&meta_with_private(Some(1))).expect("torrent");
        assert!(torrent.is_private());
        assert!(!torrent.allows_dht());
        assert!(!torrent.allows_pex());
        assert!(!torrent.allows_lsd());
        assert!(torrent.allows_source(DiscoverySource::Tracker));
        assert_eq!(
            torrent.disabled_discovery_sources(),
            &DiscoverySource::PRIVATE_DISABLED
        );

        let mut registry = torrent.source_registry();
        assert!(registry.register(DiscoverySource::Tracker).is_ok());
        assert!(registry.register(DiscoverySource::Dht).is_err());
        assert!(registry.register(DiscoverySource::Pex).is_err());
        assert!(registry.register(DiscoverySource::Lsd).is_err());
    }

    #[test]
    fn multi_file_layout_preserves_private_flag() {
        let mut meta = meta_with_private(Some(1));
        meta.torrent_file.info.length = None;
        meta.torrent_file.info.files = Some(vec![File {
            path: vec!["a.bin".into()],
            length: 4,
            md5sum: None,
        }]);
        let torrent = Torrent::new(&meta).expect("torrent");
        assert!(torrent.is_private());
        assert_eq!(torrent.files.len(), 1);
    }
}
