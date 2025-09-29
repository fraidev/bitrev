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
        })
    }
}
