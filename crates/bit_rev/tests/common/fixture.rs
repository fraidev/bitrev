#![allow(dead_code)]

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use bit_rev::file::{File, Info, TorrentFile, TorrentMeta};
use bit_rev::torrent::Torrent;
use bit_rev::utils::{calculate_piece_size, map_piece_to_files};
use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};
use serde_bencode::ser;
use serde_bytes::ByteBuf;
use tempfile::TempDir;

use super::sha1_bytes;

const STREAM_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct FileSpec {
    pub path: Vec<String>,
    pub length: u64,
}

impl FileSpec {
    pub fn new(path: impl IntoIterator<Item = impl Into<String>>, length: u64) -> Self {
        Self {
            path: path.into_iter().map(Into::into).collect(),
            length,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WrittenFile {
    pub path: Vec<String>,
    pub length: u64,
    pub disk_path: PathBuf,
}

pub struct TorrentFixture {
    pub temp_dir: TempDir,
    pub name: String,
    pub piece_length: u32,
    pub total_length: u64,
    pub files: Vec<WrittenFile>,
    pub piece_hashes: Vec<[u8; 20]>,
    pub torrent_meta: TorrentMeta,
    pub torrent_bytes: Vec<u8>,
    pub torrent_path: PathBuf,
    payload: Option<Vec<u8>>,
}

pub struct FixtureBuilder {
    seed: u64,
    name: String,
    piece_length: u32,
    files: Vec<FileSpec>,
    announce: Option<String>,
    announce_list: Option<Vec<Vec<String>>>,
    keep_payload: bool,
}

impl FixtureBuilder {
    pub fn new() -> Self {
        Self {
            seed: 0xB174_0001,
            name: "fixture".into(),
            piece_length: super::DEFAULT_PIECE_LENGTH,
            files: Vec::new(),
            announce: None,
            announce_list: None,
            keep_payload: true,
        }
    }

    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn piece_length(mut self, piece_length: u32) -> Self {
        self.piece_length = piece_length;
        self
    }

    pub fn single_file(mut self, name: impl Into<String>, length: u64) -> Self {
        let name = name.into();
        self.name = name.clone();
        self.files = vec![FileSpec::new([name], length)];
        self
    }

    pub fn files(mut self, files: Vec<FileSpec>) -> Self {
        self.files = files;
        self
    }

    pub fn announce(mut self, url: impl Into<String>) -> Self {
        self.announce = Some(url.into());
        self
    }

    pub fn announce_list(mut self, list: Vec<Vec<String>>) -> Self {
        self.announce_list = Some(list);
        self
    }

    pub fn keep_payload(mut self, keep: bool) -> Self {
        self.keep_payload = keep;
        self
    }

    pub fn build(self) -> TorrentFixture {
        assert!(self.piece_length > 0, "piece length must be positive");
        assert!(!self.files.is_empty(), "at least one file is required");
        TorrentFixture::generate(self)
    }
}

impl Default for FixtureBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TorrentFixture {
    pub fn builder() -> FixtureBuilder {
        FixtureBuilder::new()
    }

    pub fn single(length: u64, piece_length: u32, seed: u64) -> Self {
        Self::builder()
            .single_file("payload.bin", length)
            .piece_length(piece_length)
            .seed(seed)
            .build()
    }

    pub fn torrent(&self) -> Torrent {
        Torrent::new(&self.torrent_meta).expect("torrent from fixture")
    }

    pub fn piece_count(&self) -> usize {
        self.piece_hashes.len()
    }

    pub fn is_single_file(&self) -> bool {
        self.files.len() == 1
    }

    /// Session `output_dir`: file path for single-file torrents, directory for multi-file.
    pub fn session_output(&self, download_dir: &Path) -> PathBuf {
        if self.is_single_file() {
            download_dir.join(&self.name)
        } else {
            download_dir.to_path_buf()
        }
    }

    pub fn payload_bytes(&self) -> Option<&[u8]> {
        self.payload.as_deref()
    }

    pub fn set_announce(&mut self, announce: impl Into<String>) {
        self.torrent_meta.torrent_file.announce = Some(announce.into());
        self.rewrite_torrent();
    }

    pub fn set_announce_list(&mut self, list: Vec<Vec<String>>) {
        if let Some(first) = list.first().and_then(|tier| tier.first()) {
            if self.torrent_meta.torrent_file.announce.is_none() {
                self.torrent_meta.torrent_file.announce = Some(first.clone());
            }
        }
        self.torrent_meta.torrent_file.announce_list = Some(list);
        self.rewrite_torrent();
    }

    pub fn meta_with_trackers(
        &self,
        announce: Option<String>,
        announce_list: Option<Vec<Vec<String>>>,
    ) -> TorrentMeta {
        let mut meta = self.torrent_meta.clone();
        if let Some(url) = announce {
            meta.torrent_file.announce = Some(url);
        }
        if let Some(list) = announce_list {
            if meta.torrent_file.announce.is_none() {
                if let Some(first) = list.first().and_then(|tier| tier.first()) {
                    meta.torrent_file.announce = Some(first.clone());
                }
            }
            meta.torrent_file.announce_list = Some(list);
        }
        meta
    }

    pub fn piece_len(&self, index: u32) -> u32 {
        calculate_piece_size(&self.torrent(), index as usize) as u32
    }

    pub fn read_block(&self, index: u32, begin: u32, length: u32) -> Vec<u8> {
        let torrent = self.torrent();
        let piece_len = calculate_piece_size(&torrent, index as usize);
        let start = begin as usize;
        let end = start
            .checked_add(length as usize)
            .filter(|&e| e <= piece_len)
            .expect("block out of range");
        if let Some(payload) = &self.payload {
            let piece_off = index as usize * self.piece_length as usize;
            return payload[piece_off + start..piece_off + end].to_vec();
        }

        let mappings = map_piece_to_files(&torrent, index as usize);
        let mut out = Vec::with_capacity(end - start);
        let mut piece_offset = 0usize;
        for mapping in mappings {
            let map_start = piece_offset;
            let map_end = piece_offset + mapping.length;
            let overlap_start = map_start.max(start);
            let overlap_end = map_end.min(end);
            if overlap_start < overlap_end {
                let file_offset = mapping.file_offset + (overlap_start - map_start);
                let read_len = overlap_end - overlap_start;
                let disk = &self.files[mapping.file_index].disk_path;
                let mut file = std::fs::File::open(disk).expect("open fixture file");
                file.seek(SeekFrom::Start(file_offset as u64))
                    .expect("seek");
                let mut chunk = vec![0u8; read_len];
                file.read_exact(&mut chunk).expect("read fixture block");
                out.extend_from_slice(&chunk);
            }
            piece_offset = map_end;
        }
        out
    }

    pub fn assert_output_matches(&self, output: &Path) {
        if self.is_single_file() {
            let expected = self.files[0].disk_path.as_path();
            assert_eq!(
                super::sha1_file(output),
                super::sha1_file(expected),
                "single-file output hash mismatch"
            );
            return;
        }

        for file in &self.files {
            let mut got = output.to_path_buf();
            for component in &file.path {
                got.push(component);
            }
            assert!(got.is_file(), "missing output file {got:?}");
            assert_eq!(
                std::fs::metadata(&got).expect("meta").len(),
                file.length,
                "length mismatch for {got:?}"
            );
            assert_eq!(
                super::sha1_file(&got),
                super::sha1_file(&file.disk_path),
                "hash mismatch for {got:?}"
            );
        }
    }

    fn generate(builder: FixtureBuilder) -> Self {
        let temp_dir = super::unique_temp_dir();
        let payload_root = temp_dir.path().join("payload");
        std::fs::create_dir_all(&payload_root).expect("payload root");

        let total_length: u64 = builder.files.iter().map(|f| f.length).sum();
        let piece_length = builder.piece_length;
        let mut rng = StdRng::seed_from_u64(builder.seed);
        let mut piece_hashes = Vec::new();
        let mut piece_buf = Vec::with_capacity(piece_length as usize);
        let mut payload = if builder.keep_payload {
            Some(Vec::with_capacity(total_length as usize))
        } else {
            None
        };

        let mut written = Vec::with_capacity(builder.files.len());
        for spec in &builder.files {
            let mut disk_path = payload_root.clone();
            for component in &spec.path {
                disk_path.push(component);
            }
            if let Some(parent) = disk_path.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            let mut file = std::fs::File::create(&disk_path).expect("create payload file");
            let mut remaining = spec.length;
            let mut chunk = vec![0u8; STREAM_CHUNK];
            while remaining > 0 {
                let n = (remaining as usize).min(STREAM_CHUNK);
                rng.fill_bytes(&mut chunk[..n]);
                file.write_all(&chunk[..n]).expect("write payload");
                if let Some(payload) = &mut payload {
                    payload.extend_from_slice(&chunk[..n]);
                }
                let mut off = 0;
                while off < n {
                    let take = (piece_length as usize - piece_buf.len()).min(n - off);
                    piece_buf.extend_from_slice(&chunk[off..off + take]);
                    off += take;
                    if piece_buf.len() == piece_length as usize {
                        piece_hashes.push(sha1_bytes(&piece_buf));
                        piece_buf.clear();
                    }
                }
                remaining -= n as u64;
            }
            written.push(WrittenFile {
                path: spec.path.clone(),
                length: spec.length,
                disk_path,
            });
        }
        if !piece_buf.is_empty() {
            piece_hashes.push(sha1_bytes(&piece_buf));
        }

        let mut pieces_concat = Vec::with_capacity(piece_hashes.len() * 20);
        for hash in &piece_hashes {
            pieces_concat.extend_from_slice(hash);
        }

        let (length, files) = if builder.files.len() == 1 {
            (Some(builder.files[0].length as i64), None)
        } else {
            (
                None,
                Some(
                    builder
                        .files
                        .iter()
                        .map(|f| File {
                            path: f.path.clone(),
                            length: f.length as i64,
                            md5sum: None,
                        })
                        .collect(),
                ),
            )
        };

        let torrent_file = TorrentFile {
            info: Info {
                name: builder.name.clone(),
                pieces: ByteBuf::from(pieces_concat),
                piece_length: i64::from(piece_length),
                md5sum: None,
                length,
                files,
                private: None,
                path: None,
                root_hash: None,
            },
            announce: builder.announce,
            nodes: None,
            encoding: None,
            httpseeds: None,
            announce_list: builder.announce_list,
            creation_date: None,
            comment: None,
            created_by: None,
        };
        let torrent_meta = TorrentMeta::new(torrent_file).expect("fixture metainfo");
        let torrent_bytes = ser::to_bytes(&torrent_meta.torrent_file).expect("bencode torrent");
        let torrent_path = temp_dir.path().join(format!("{}.torrent", builder.name));
        std::fs::write(&torrent_path, &torrent_bytes).expect("write torrent");

        Self {
            temp_dir,
            name: builder.name,
            piece_length,
            total_length,
            files: written,
            piece_hashes,
            torrent_meta,
            torrent_bytes,
            torrent_path,
            payload,
        }
    }

    fn rewrite_torrent(&mut self) {
        self.torrent_bytes =
            ser::to_bytes(&self.torrent_meta.torrent_file).expect("bencode torrent");
        std::fs::write(&self.torrent_path, &self.torrent_bytes).expect("rewrite torrent");
    }
}
