use std::path::PathBuf;

use crate::torrent::Torrent;
use crate::utils;

/// Per-file download priority. Default is `Normal`.
///
/// Resume data stores these as `i64`: 0 = Skip, 1 = Low, 2 = Normal, 3 = High.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(u8)]
pub enum FilePriority {
    Skip = 0,
    Low = 1,
    #[default]
    Normal = 2,
    High = 3,
}

impl FilePriority {
    pub fn from_resume(value: i64) -> Self {
        match value {
            0 => Self::Skip,
            1 => Self::Low,
            2 => Self::Normal,
            3 => Self::High,
            _ => Self::Normal,
        }
    }

    pub fn to_resume(self) -> i64 {
        i64::from(self as u8)
    }

    pub fn decode_list(values: &[i64]) -> Vec<Self> {
        values.iter().copied().map(Self::from_resume).collect()
    }

    pub fn encode_list(values: &[Self]) -> Vec<i64> {
        values.iter().copied().map(Self::to_resume).collect()
    }

    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Skip,
            1 => Self::Low,
            3 => Self::High,
            _ => Self::Normal,
        }
    }
}

/// One file in a torrent, with live progress and priority.
#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub index: usize,
    pub path: PathBuf,
    pub length: u64,
    pub progress: f64,
    pub priority: FilePriority,
}

/// Pad or truncate `given` to `file_count`. Missing entries are `Normal`.
pub fn normalize_file_priorities(file_count: usize, given: &[FilePriority]) -> Vec<FilePriority> {
    let mut out = vec![FilePriority::Normal; file_count];
    for (i, prio) in given.iter().copied().take(file_count).enumerate() {
        out[i] = prio;
    }
    out
}

/// Piece priority is the max over overlapping files. `Skip` only when every
/// overlapping file is skipped.
pub fn piece_priority(
    torrent: &Torrent,
    file_prios: &[FilePriority],
    piece_index: usize,
) -> FilePriority {
    let mappings = utils::map_piece_to_files(torrent, piece_index);
    if mappings.is_empty() {
        return FilePriority::Normal;
    }
    mappings
        .iter()
        .map(|m| {
            file_prios
                .get(m.file_index)
                .copied()
                .unwrap_or(FilePriority::Normal)
        })
        .max()
        .unwrap_or(FilePriority::Skip)
}

/// First and last piece of each wanted file, in file order, without duplicates.
pub fn first_last_wanted_pieces(torrent: &Torrent, file_prios: &[FilePriority]) -> Vec<u32> {
    let piece_length = torrent.piece_length;
    if piece_length <= 0 {
        return Vec::new();
    }
    let piece_length = piece_length as u64;
    let mut out = Vec::new();
    for (i, file) in torrent.files.iter().enumerate() {
        if file.length <= 0 {
            continue;
        }
        if file_prios.get(i).copied().unwrap_or(FilePriority::Normal) == FilePriority::Skip {
            continue;
        }
        let offset = file.offset.max(0) as u64;
        let length = file.length as u64;
        let first = (offset / piece_length) as u32;
        let last = ((offset + length - 1) / piece_length) as u32;
        if !out.contains(&first) {
            out.push(first);
        }
        if last != first && !out.contains(&last) {
            out.push(last);
        }
    }
    out
}

/// Bytes of `file_index` covered by verified pieces.
pub fn file_have_bytes(
    torrent: &Torrent,
    file_index: usize,
    has_piece: impl Fn(u32) -> bool,
) -> u64 {
    let Some(file) = torrent.files.get(file_index) else {
        return 0;
    };
    if file.length <= 0 {
        return 0;
    }
    let mut have = 0u64;
    for mapping in file_piece_mappings(torrent, file_index) {
        if has_piece(mapping.piece) {
            have += mapping.length as u64;
        }
    }
    have.min(file.length as u64)
}

struct FilePieceOverlap {
    piece: u32,
    length: usize,
}

fn file_piece_mappings(torrent: &Torrent, file_index: usize) -> Vec<FilePieceOverlap> {
    let Some(file) = torrent.files.get(file_index) else {
        return Vec::new();
    };
    if file.length <= 0 || torrent.piece_length <= 0 {
        return Vec::new();
    }
    let piece_length = torrent.piece_length as u64;
    let start = file.offset.max(0) as u64;
    let end = start + file.length as u64;
    let first = (start / piece_length) as usize;
    let last = ((end - 1) / piece_length) as usize;
    let last = last.min(torrent.piece_hashes.len().saturating_sub(1));
    let mut out = Vec::new();
    for piece in first..=last {
        for mapping in utils::map_piece_to_files(torrent, piece) {
            if mapping.file_index == file_index {
                out.push(FilePieceOverlap {
                    piece: piece as u32,
                    length: mapping.length,
                });
            }
        }
    }
    out
}

pub fn file_path(file: &crate::torrent::TorrentFileInfo) -> PathBuf {
    let mut path = PathBuf::new();
    for component in &file.path {
        path.push(component);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::{Torrent, TorrentFileInfo};

    fn aligned_three_files(piece_length: i64, file_pieces: i64) -> Torrent {
        let file_len = piece_length * file_pieces;
        Torrent {
            info_hash: [0; 20],
            piece_hashes: vec![[0; 20]; (file_pieces * 3) as usize],
            piece_length,
            length: file_len * 3,
            files: vec![
                TorrentFileInfo {
                    path: vec!["a.bin".into()],
                    length: file_len,
                    offset: 0,
                },
                TorrentFileInfo {
                    path: vec!["b.bin".into()],
                    length: file_len,
                    offset: file_len,
                },
                TorrentFileInfo {
                    path: vec!["c.bin".into()],
                    length: file_len,
                    offset: file_len * 2,
                },
            ],
            name: "bundle".into(),
            private: false,
        }
    }

    #[test]
    fn resume_roundtrip_and_unknown_defaults_to_normal() {
        for prio in [
            FilePriority::Skip,
            FilePriority::Low,
            FilePriority::Normal,
            FilePriority::High,
        ] {
            assert_eq!(FilePriority::from_resume(prio.to_resume()), prio);
        }
        assert_eq!(FilePriority::from_resume(99), FilePriority::Normal);
        assert_eq!(FilePriority::from_resume(-1), FilePriority::Normal);
    }

    #[test]
    fn normalize_pads_and_truncates() {
        assert_eq!(
            normalize_file_priorities(3, &[]),
            vec![FilePriority::Normal; 3]
        );
        assert_eq!(
            normalize_file_priorities(
                2,
                &[FilePriority::Skip, FilePriority::High, FilePriority::Low]
            ),
            vec![FilePriority::Skip, FilePriority::High]
        );
        assert_eq!(
            normalize_file_priorities(3, &[FilePriority::Low]),
            vec![
                FilePriority::Low,
                FilePriority::Normal,
                FilePriority::Normal
            ]
        );
    }

    #[test]
    fn skip_only_when_every_overlapping_file_is_skipped() {
        let torrent = aligned_three_files(16, 2);
        let prios = vec![FilePriority::Normal, FilePriority::Skip, FilePriority::High];
        assert_eq!(piece_priority(&torrent, &prios, 0), FilePriority::Normal);
        assert_eq!(piece_priority(&torrent, &prios, 1), FilePriority::Normal);
        assert_eq!(piece_priority(&torrent, &prios, 2), FilePriority::Skip);
        assert_eq!(piece_priority(&torrent, &prios, 3), FilePriority::Skip);
        assert_eq!(piece_priority(&torrent, &prios, 4), FilePriority::High);
        assert_eq!(piece_priority(&torrent, &prios, 5), FilePriority::High);
    }

    #[test]
    fn boundary_piece_stays_wanted_if_any_file_is() {
        let torrent = Torrent {
            info_hash: [0; 20],
            piece_hashes: vec![[0; 20]; 2],
            piece_length: 16,
            length: 24,
            files: vec![
                TorrentFileInfo {
                    path: vec!["a.bin".into()],
                    length: 10,
                    offset: 0,
                },
                TorrentFileInfo {
                    path: vec!["b.bin".into()],
                    length: 14,
                    offset: 10,
                },
            ],
            name: "split".into(),
            private: false,
        };
        let prios = vec![FilePriority::Skip, FilePriority::Normal];
        assert_eq!(piece_priority(&torrent, &prios, 0), FilePriority::Normal);
        assert_eq!(piece_priority(&torrent, &prios, 1), FilePriority::Normal);
    }

    #[test]
    fn first_last_skips_unwanted_files() {
        let torrent = aligned_three_files(16, 4);
        let prios = vec![FilePriority::Normal, FilePriority::Skip, FilePriority::Low];
        assert_eq!(
            first_last_wanted_pieces(&torrent, &prios),
            vec![0, 3, 8, 11]
        );
    }

    #[test]
    fn file_have_bytes_counts_verified_overlap() {
        let torrent = aligned_three_files(16, 2);
        let have = |index| index == 0 || index == 2;
        assert_eq!(file_have_bytes(&torrent, 0, have), 16);
        assert_eq!(file_have_bytes(&torrent, 1, have), 16);
        assert_eq!(file_have_bytes(&torrent, 2, have), 0);
    }
}
