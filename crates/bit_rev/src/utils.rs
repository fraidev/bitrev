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
    let mut id = [0u8; 20];
    id[..8].copy_from_slice(&crate::identity::peer_id_prefix());
    rand::thread_rng().fill(&mut id[8..]);
    id
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity;
    use crate::torrent::{Torrent, TorrentFileInfo};

    fn torrent(files: &[i64], piece_length: i64) -> Torrent {
        let mut offset = 0i64;
        let files = files
            .iter()
            .enumerate()
            .map(|(i, &len)| {
                let f = TorrentFileInfo {
                    path: vec![format!("f{i}")],
                    length: len,
                    offset,
                };
                offset += len;
                f
            })
            .collect();
        Torrent {
            info_hash: [0; 20],
            piece_hashes: vec![],
            piece_length,
            length: offset,
            files,
            name: "t".into(),
        }
    }

    fn assert_mapping(m: &PieceFileMapping, file_index: usize, file_offset: usize, length: usize) {
        assert_eq!(m.file_index, file_index);
        assert_eq!(m.file_offset, file_offset);
        assert_eq!(m.length, length);
    }

    #[test]
    fn peer_id_matches_bep20_for_current_version() {
        let id = generate_peer_id();
        assert_eq!(id.len(), 20);
        assert_eq!(&id[..8], b"-BR0100-");
        assert_eq!(&id[..8], &identity::peer_id_prefix());
        assert_eq!(id[0], b'-');
        assert_eq!(&id[1..3], b"BR");
        assert_eq!(id[7], b'-');
    }

    #[test]
    fn peer_ids_differ_in_random_suffix() {
        let a = generate_peer_id();
        let b = generate_peer_id();
        assert_eq!(&a[..8], &b[..8]);
        assert_ne!(&a[8..], &b[8..]);
    }

    #[test]
    fn map_single_file_first_and_last_short_piece() {
        let t = torrent(&[100], 40);
        let piece0 = map_piece_to_files(&t, 0);
        assert_eq!(piece0.len(), 1);
        assert_mapping(&piece0[0], 0, 0, 40);

        let piece2 = map_piece_to_files(&t, 2);
        assert_eq!(piece2.len(), 1);
        assert_mapping(&piece2[0], 0, 80, 20);
    }

    #[test]
    fn map_piece_spanning_two_files() {
        let t = torrent(&[30, 30], 40);
        let piece0 = map_piece_to_files(&t, 0);
        assert_eq!(piece0.len(), 2);
        assert_mapping(&piece0[0], 0, 0, 30);
        assert_mapping(&piece0[1], 1, 0, 10);
    }

    #[test]
    fn map_piece_spanning_three_files() {
        let t = torrent(&[10, 10, 10], 25);
        let piece0 = map_piece_to_files(&t, 0);
        assert_eq!(piece0.len(), 3);
        assert_mapping(&piece0[0], 0, 0, 10);
        assert_mapping(&piece0[1], 1, 0, 10);
        assert_mapping(&piece0[2], 2, 0, 5);
    }

    #[test]
    fn map_piece_includes_zero_length_file() {
        let t = torrent(&[20, 0, 20], 30);
        let piece0 = map_piece_to_files(&t, 0);
        assert_eq!(piece0.len(), 3);
        assert_mapping(&piece0[0], 0, 0, 20);
        assert_mapping(&piece0[1], 1, 0, 0);
        assert_mapping(&piece0[2], 2, 0, 10);
    }
}
