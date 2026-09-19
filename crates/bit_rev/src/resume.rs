use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use tracing::warn;

use crate::bitfield::Bitfield;
use crate::peer_connection::TorrentDownloadedState;
use crate::storage;
use crate::torrent::Torrent;
use crate::utils;

pub const RESUME_VERSION: i64 = 2;
pub const RESUME_VERSION_V1: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ResumeError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("unsupported resume version {0}")]
    UnsupportedVersion(i64),
    #[error("info hash mismatch")]
    InfoHashMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeFile {
    pub path: Vec<String>,
    pub length: i64,
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeData {
    pub version: i64,
    pub info_hash: ByteBuf,
    pub bitfield: ByteBuf,
    pub output_dir: String,
    pub files: Vec<ResumeFile>,
    pub uploaded: i64,
    pub downloaded: i64,
    pub torrent_path: String,
    pub paused: i64,
    pub added_at: i64,
    #[serde(default)]
    pub completed_at: i64,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub sequential: i64,
    #[serde(default)]
    pub file_priorities: Vec<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeStatus {
    Fresh,
    FastPath,
    SlowPath,
    Corrupt,
}

impl ResumeData {
    pub fn info_hash(&self) -> Option<[u8; 20]> {
        self.info_hash.as_ref().try_into().ok()
    }

    pub fn is_paused(&self) -> bool {
        self.paused != 0
    }

    pub fn completed_at(&self) -> Option<i64> {
        (self.completed_at > 0).then_some(self.completed_at)
    }

    pub fn is_sequential(&self) -> bool {
        self.sequential != 0
    }

    pub fn bitfield(&self) -> Bitfield {
        Bitfield::new(self.bitfield.to_vec())
    }
}

pub fn info_hash_hex(info_hash: &[u8; 20]) -> String {
    info_hash.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn resume_path(state_dir: &Path, info_hash: &[u8; 20]) -> PathBuf {
    util::paths::resume_dir(state_dir).join(format!("{}.resume", info_hash_hex(info_hash)))
}

pub fn torrent_cache_path(state_dir: &Path, info_hash: &[u8; 20]) -> PathBuf {
    util::paths::torrents_dir(state_dir).join(format!("{}.torrent", info_hash_hex(info_hash)))
}

/// Resume files under `<state_dir>/resume/*.resume`, sorted by path.
pub fn list_resume_files(state_dir: &Path) -> Vec<PathBuf> {
    let dir = util::paths::resume_dir(state_dir);
    let mut paths = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return paths;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("resume") {
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

pub fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn encode(data: &ResumeData) -> Result<Vec<u8>, ResumeError> {
    serde_bencode::to_bytes(data).map_err(|e| ResumeError::Decode(e.to_string()))
}

pub fn decode(bytes: &[u8]) -> Result<ResumeData, ResumeError> {
    crate::file::check_bencode_depth(bytes).map_err(|e| ResumeError::Decode(e.to_string()))?;
    let data: ResumeData =
        serde_bencode::from_bytes(bytes).map_err(|e| ResumeError::Decode(e.to_string()))?;
    if data.version != RESUME_VERSION && data.version != RESUME_VERSION_V1 {
        return Err(ResumeError::UnsupportedVersion(data.version));
    }
    if data.info_hash.len() != 20 {
        return Err(ResumeError::Decode("info_hash must be 20 bytes".into()));
    }
    Ok(data)
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ResumeError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path(path);
    match std::fs::write(&tmp, bytes) {
        Ok(()) => {}
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

pub fn save(path: &Path, data: &ResumeData) -> Result<(), ResumeError> {
    let bytes = encode(data)?;
    write_atomic(path, &bytes)
}

pub fn load(path: &Path) -> Result<ResumeData, ResumeError> {
    let bytes = std::fs::read(path)?;
    decode(&bytes)
}

/// Load resume data if the file exists. Corrupt or unreadable files warn and yield `None`.
pub fn load_optional(path: &Path) -> Result<Option<ResumeData>, ResumeError> {
    match load(path) {
        Ok(data) => Ok(Some(data)),
        Err(ResumeError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "ignoring corrupt resume file");
            Ok(None)
        }
    }
}

pub fn collect_file_layout(torrent: &Torrent, output_dir: &Path) -> Vec<ResumeFile> {
    torrent
        .files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            let path = storage::file_path(torrent, output_dir, i);
            let (length, mtime) = match std::fs::metadata(&path) {
                Ok(meta) => (meta.len() as i64, mtime_secs(&meta)),
                Err(_) => (0, 0),
            };
            ResumeFile {
                path: file.path.clone(),
                length,
                mtime,
            }
        })
        .collect()
}

pub fn files_match(recorded: &[ResumeFile], torrent: &Torrent, output_dir: &Path) -> bool {
    if recorded.len() != torrent.files.len() {
        return false;
    }
    recorded.iter().enumerate().all(|(i, rec)| {
        if rec.path != torrent.files[i].path {
            return false;
        }
        let path = storage::file_path(torrent, output_dir, i);
        let Ok(meta) = std::fs::metadata(&path) else {
            return false;
        };
        meta.len() as i64 == rec.length && mtime_secs(&meta) == rec.mtime
    })
}

pub fn apply_bitfield(state: &TorrentDownloadedState, bitfield: &Bitfield) {
    for i in 0..state.piece_count() {
        if bitfield.has_piece(i) {
            state.mark_downloaded(i as u32);
        }
    }
}

pub async fn verify_existing_pieces(
    storage: &Arc<storage::Storage>,
    state: &TorrentDownloadedState,
) {
    let torrent = storage.torrent();
    let hasher = storage.hasher().clone();
    let permits = hasher.permits();
    let count = torrent.piece_hashes.len();
    let mut next = 0;
    let mut in_flight = 0;
    let mut set = tokio::task::JoinSet::new();

    loop {
        while in_flight < permits && next < count {
            let length = utils::calculate_piece_size(torrent, next) as u32;
            if length == 0 {
                next += 1;
                continue;
            }
            let index = next as u32;
            let expected = torrent.piece_hashes[next];
            next += 1;
            in_flight += 1;
            let storage = Arc::clone(storage);
            let hasher = hasher.clone();
            set.spawn(async move {
                match storage.read_block(index, 0, length).await {
                    Ok(buf) => hasher.verify(expected, buf).await.then_some(index),
                    Err(_) => None,
                }
            });
        }
        match set.join_next().await {
            Some(Ok(Some(index))) => {
                in_flight -= 1;
                state.mark_downloaded(index);
            }
            Some(_) => in_flight -= 1,
            None => break,
        }
    }
}

pub fn cache_torrent_file(
    state_dir: &Path,
    info_hash: &[u8; 20],
    torrent_file: &crate::file::TorrentFile,
    info_bytes: &[u8],
) -> PathBuf {
    let path = torrent_cache_path(state_dir, info_hash);
    let bytes = crate::file::encode_torrent_preserving_info(torrent_file, info_bytes);
    if let Err(e) = write_atomic(&path, &bytes) {
        warn!(path = %path.display(), error = %e, "failed to cache torrent metainfo");
    }
    path
}

pub struct ResumeSnapshot<'a> {
    pub info_hash: &'a [u8; 20],
    pub output_dir: &'a Path,
    pub torrent: &'a Torrent,
    pub downloaded_state: &'a TorrentDownloadedState,
    pub uploaded: u64,
    pub paused: bool,
    pub torrent_path: &'a Path,
    pub added_at: i64,
    pub completed_at: Option<i64>,
    pub category: &'a str,
    pub tags: &'a [String],
    pub sequential: bool,
    pub file_priorities: &'a [i64],
}

pub fn snapshot(snap: ResumeSnapshot<'_>) -> ResumeData {
    let bitfield = snap.downloaded_state.our_bitfield();
    ResumeData {
        version: RESUME_VERSION,
        info_hash: ByteBuf::from(snap.info_hash.to_vec()),
        bitfield: ByteBuf::from(bitfield.as_bytes().to_vec()),
        output_dir: snap.output_dir.to_string_lossy().into_owned(),
        files: collect_file_layout(snap.torrent, snap.output_dir),
        uploaded: snap.uploaded as i64,
        downloaded: snap.downloaded_state.downloaded_bytes() as i64,
        torrent_path: snap.torrent_path.to_string_lossy().into_owned(),
        paused: i64::from(snap.paused),
        added_at: snap.added_at,
        completed_at: snap.completed_at.unwrap_or(0),
        category: snap.category.to_string(),
        tags: snap.tags.to_vec(),
        sequential: i64::from(snap.sequential),
        file_priorities: snap.file_priorities.to_vec(),
    }
}

pub fn persist(state_dir: &Path, snap: ResumeSnapshot<'_>) -> Result<PathBuf, ResumeError> {
    let path = resume_path(state_dir, snap.info_hash);
    let data = snapshot(snap);
    save(&path, &data)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::TorrentFileInfo;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bitrev-resume-{label}-{}-{}",
            std::process::id(),
            now_unix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_data() -> ResumeData {
        ResumeData {
            version: RESUME_VERSION,
            info_hash: ByteBuf::from(vec![0xab; 20]),
            bitfield: ByteBuf::from(vec![0b1010_0000]),
            output_dir: "/tmp/out.bin".into(),
            files: vec![ResumeFile {
                path: vec!["out.bin".into()],
                length: 16,
                mtime: 1_700_000_000,
            }],
            uploaded: 11,
            downloaded: 16,
            torrent_path: "/tmp/torrents/ab.torrent".into(),
            paused: 1,
            added_at: 1_700_000_000,
            completed_at: 0,
            category: String::new(),
            tags: Vec::new(),
            sequential: 0,
            file_priorities: Vec::new(),
        }
    }

    fn torrent_one_file(name: &str, length: i64) -> Torrent {
        Torrent {
            info_hash: [0xab; 20],
            piece_hashes: vec![[0u8; 20]],
            piece_length: length,
            length,
            files: vec![TorrentFileInfo {
                path: vec![name.into()],
                length,
                offset: 0,
            }],
            name: name.into(),
            private: false,
        }
    }

    #[test]
    fn encode_roundtrip() {
        let data = sample_data();
        let bytes = encode(&data).unwrap();
        let loaded = decode(&bytes).unwrap();
        assert_eq!(loaded, data);
        assert!(loaded.is_paused());
        assert_eq!(loaded.info_hash().unwrap(), [0xab; 20]);
    }

    #[test]
    fn atomic_write_leaves_no_partial_file() {
        let dir = temp_dir("atomic");
        let path = dir.join("deadbeef.resume");
        let data = sample_data();
        save(&path, &data).unwrap();

        assert!(path.exists());
        assert!(!tmp_path(&path).exists());
        assert_eq!(load(&path).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_optional_missing_is_none() {
        let dir = temp_dir("missing");
        let path = dir.join("nope.resume");
        assert!(load_optional(&path).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_resume_is_ignored() {
        let dir = temp_dir("corrupt");
        let path = dir.join("bad.resume");
        std::fs::write(&path, b"not bencode!!!!").unwrap();
        assert!(load_optional(&path).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_version_is_error() {
        let mut data = sample_data();
        data.version = 99;
        let bytes = serde_bencode::to_bytes(&data).unwrap();
        assert!(matches!(
            decode(&bytes),
            Err(ResumeError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn v1_resume_loads_with_v2_defaults() {
        #[derive(Serialize)]
        struct ResumeV1 {
            version: i64,
            info_hash: ByteBuf,
            bitfield: ByteBuf,
            output_dir: String,
            files: Vec<ResumeFile>,
            uploaded: i64,
            downloaded: i64,
            torrent_path: String,
            paused: i64,
            added_at: i64,
            completed_at: i64,
        }
        let bytes = serde_bencode::to_bytes(&ResumeV1 {
            version: RESUME_VERSION_V1,
            info_hash: ByteBuf::from(vec![0xab; 20]),
            bitfield: ByteBuf::from(vec![0b1010_0000]),
            output_dir: "/tmp/out.bin".into(),
            files: vec![ResumeFile {
                path: vec!["out.bin".into()],
                length: 16,
                mtime: 1_700_000_000,
            }],
            uploaded: 11,
            downloaded: 16,
            torrent_path: "/tmp/torrents/ab.torrent".into(),
            paused: 1,
            added_at: 1_700_000_000,
            completed_at: 0,
        })
        .unwrap();
        let loaded = decode(&bytes).unwrap();
        assert_eq!(loaded.version, RESUME_VERSION_V1);
        assert!(loaded.is_paused());
        assert!(loaded.category.is_empty());
        assert!(loaded.tags.is_empty());
        assert!(!loaded.is_sequential());
        assert!(loaded.file_priorities.is_empty());
    }

    #[test]
    fn list_resume_files_finds_sorted_resume_suffix() {
        let dir = temp_dir("list");
        let resume = util::paths::resume_dir(&dir);
        std::fs::create_dir_all(&resume).unwrap();
        std::fs::write(resume.join("b.resume"), b"x").unwrap();
        std::fs::write(resume.join("a.resume"), b"x").unwrap();
        std::fs::write(resume.join("ignore.txt"), b"x").unwrap();
        let listed = list_resume_files(&dir);
        assert_eq!(listed.len(), 2);
        assert!(listed[0].ends_with("a.resume"));
        assert!(listed[1].ends_with("b.resume"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_match_requires_size_and_mtime() {
        let dir = temp_dir("mtime");
        let file = dir.join("out.bin");
        std::fs::write(&file, [1u8; 8]).unwrap();
        let torrent = torrent_one_file("out.bin", 8);
        let layout = collect_file_layout(&torrent, &file);
        assert!(files_match(&layout, &torrent, &file));

        let mut mismatch = layout.clone();
        mismatch[0].mtime += 1;
        assert!(!files_match(&mismatch, &torrent, &file));

        mismatch = layout;
        mismatch[0].length += 1;
        assert!(!files_match(&mismatch, &torrent, &file));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_match_accepts_preallocated_size() {
        let dir = temp_dir("prealloc");
        let file = dir.join("out.bin");
        std::fs::File::create(&file).unwrap().set_len(8).unwrap();
        let torrent = torrent_one_file("out.bin", 8);
        let layout = collect_file_layout(&torrent, &file);
        assert_eq!(layout[0].length, 8);
        assert!(files_match(&layout, &torrent, &file));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_and_cache_paths_use_hex_hash() {
        let hash = [
            0x0f, 0xab, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
            0xcc, 0xdd, 0xee, 0xff, 0x01, 0x02,
        ];
        let hex = info_hash_hex(&hash);
        assert_eq!(hex.len(), 40);
        let state = Path::new("/tmp/state");
        assert_eq!(
            resume_path(state, &hash),
            PathBuf::from("/tmp/state/resume").join(format!("{hex}.resume"))
        );
        assert_eq!(
            torrent_cache_path(state, &hash),
            PathBuf::from("/tmp/state/torrents").join(format!("{hex}.torrent"))
        );
    }
}
