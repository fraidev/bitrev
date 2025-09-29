use crate::torrent::{Torrent, TorrentFileInfo};
use rand::Rng;

const BLOCK_SIZE: u32 = 16384;

pub fn calculate_bounds_for_piece(torrent: &Torrent, index: usize) -> (usize, usize) {
    let start = index * torrent.piece_length as usize;
    let end = start + torrent.piece_length as usize;
    let torrent_length = torrent.length as usize;

    if end > torrent_length {
        (start, torrent_length)
    } else {
        (start, end)
    }
}

pub fn calculate_piece_size(torrent: &Torrent, index: usize) -> usize {
    let (start, end) = calculate_bounds_for_piece(torrent, index);
    end - start
}

pub fn calculate_block_size(piece_length: u32, requested: u32) -> u32 {
    if piece_length - requested < BLOCK_SIZE {
        return piece_length - requested;
    };
    BLOCK_SIZE
}

pub fn check_integrity(hash: &[u8], buf: &[u8]) -> bool {
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(buf);
    let result = hasher.digest().bytes();
    result == hash
}

pub fn generate_peer_id() -> [u8; 20] {
    let mut rng = rand::prelude::ThreadRng::default();
    (0..20)
        .map(|_| rng.gen())
        .collect::<Vec<u8>>()
        .try_into()
        .unwrap()
}

#[derive(Debug, Clone)]
pub struct PieceFileMapping {
    pub file_index: usize,
    pub file_offset: usize,
    pub length: usize,
}

pub fn map_piece_to_files(torrent: &Torrent, piece_index: usize) -> Vec<PieceFileMapping> {
    let (piece_start, piece_end) = calculate_bounds_for_piece(torrent, piece_index);
    let mut mappings = Vec::new();

    for (file_index, file) in torrent.files.iter().enumerate() {
        let file_start = file.offset as usize;
        let file_end = file_start + file.length as usize;

        // Check if piece overlaps with this file
        if piece_start < file_end && piece_end > file_start {
            let overlap_start = piece_start.max(file_start);
            let overlap_end = piece_end.min(file_end);
            let file_offset = overlap_start - file_start;
            let length = overlap_end - overlap_start;

            mappings.push(PieceFileMapping {
                file_index,
                file_offset,
                length,
            });
        }
    }

    mappings
}

pub fn get_full_file_path(torrent: &Torrent, file_info: &TorrentFileInfo) -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from(&torrent.name);
    for component in &file_info.path {
        path.push(component);
    }
    path
}
