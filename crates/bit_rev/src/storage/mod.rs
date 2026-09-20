use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};

use crate::hash::PieceHasher;
use crate::torrent::Torrent;
use crate::utils::{calculate_piece_size, map_piece_to_files, PieceFileMapping};

mod preallocate;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("piece {0} out of range")]
    PieceOutOfRange(u32),
    #[error("block out of bounds: piece={index} begin={begin} length={length}")]
    BlockOutOfBounds { index: u32, begin: u32, length: u32 },
    #[error("destination already exists: {0}")]
    DestinationExists(PathBuf),
}

/// How `Storage::open` grows files for a fresh download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preallocate {
    /// `set_len` only. Sparse on every target OS. Default.
    #[default]
    Sparse,
    /// `fallocate` / `F_PREALLOCATE`, falling back to sparse.
    Full,
    /// Leave the file size alone.
    Off,
}

#[derive(Clone)]
pub struct StorageOptions {
    pub preallocate: Preallocate,
    /// Number of pieces kept in the read cache. 0 disables it.
    pub piece_cache_pieces: usize,
    pub hasher: Arc<PieceHasher>,
}

impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            preallocate: Preallocate::Sparse,
            piece_cache_pieces: 0,
            hasher: Arc::new(PieceHasher::new()),
        }
    }
}

struct StorageFile {
    path: Mutex<PathBuf>,
    /// Shared fd for positional I/O. Write-locked for `set_len` and relocation.
    file: RwLock<Arc<File>>,
}

impl StorageFile {
    fn handle(&self) -> Arc<File> {
        self.file.read().expect("storage file lock").clone()
    }

    fn allocate(&self, len: u64, mode: Preallocate) -> io::Result<()> {
        if len == 0 || mode == Preallocate::Off {
            return Ok(());
        }
        let file = self.file.write().expect("storage file lock");
        let current = file.metadata()?.len();
        if current >= len {
            return Ok(());
        }
        match mode {
            Preallocate::Off => Ok(()),
            Preallocate::Sparse => file.set_len(len),
            Preallocate::Full => preallocate::allocate_full(&file, len),
        }
    }
}

struct PieceCache {
    capacity: usize,
    map: HashMap<u32, Vec<u8>>,
    order: VecDeque<u32>,
}

impl PieceCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, index: u32) -> Option<&[u8]> {
        if !self.map.contains_key(&index) {
            return None;
        }
        self.order.retain(|&i| i != index);
        self.order.push_back(index);
        self.map.get(&index).map(|v| v.as_slice())
    }

    fn put(&mut self, index: u32, data: Vec<u8>) {
        if self.capacity == 0 {
            return;
        }
        if self.map.contains_key(&index) {
            self.order.retain(|&i| i != index);
        } else if self.map.len() >= self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.order.push_back(index);
        self.map.insert(index, data);
    }

    fn invalidate(&mut self, index: u32) {
        self.map.remove(&index);
        self.order.retain(|&i| i != index);
    }
}

pub struct Storage {
    torrent: Torrent,
    output_dir: Mutex<PathBuf>,
    files: Vec<StorageFile>,
    cache: Mutex<PieceCache>,
    cache_capacity: usize,
    hasher: Arc<PieceHasher>,
}

/// Resolve the on-disk path for torrent file `file_index`.
/// Single-file torrent: `output_dir` is the file path.
/// Multi-file torrent: `output_dir` is the root directory.
pub fn file_path(torrent: &Torrent, output_dir: &Path, file_index: usize) -> PathBuf {
    if torrent.files.len() == 1 {
        output_dir.to_path_buf()
    } else {
        let mut path = output_dir.to_path_buf();
        if let Some(file_info) = torrent.files.get(file_index) {
            for component in &file_info.path {
                path.push(component);
            }
        }
        path
    }
}

impl Storage {
    /// Open (or create) files under `output_dir` without truncating existing data.
    /// Single-file torrent: `output_dir` is the file path (may be a file name, not a directory).
    /// Multi-file torrent: `output_dir` is the root directory; join each file's path components.
    /// Create parent directories as needed.
    /// Use OpenOptions: read+write+create, do NOT truncate.
    pub async fn open(
        torrent: &Torrent,
        output_dir: impl AsRef<Path>,
    ) -> Result<Arc<Self>, StorageError> {
        Self::open_with(torrent, output_dir, StorageOptions::default()).await
    }

    pub async fn open_with(
        torrent: &Torrent,
        output_dir: impl AsRef<Path>,
        opts: StorageOptions,
    ) -> Result<Arc<Self>, StorageError> {
        let torrent = torrent.clone();
        let output_dir = output_dir.as_ref().to_path_buf();
        spawn_blocking_io(move || open_sync(&torrent, &output_dir, opts)).await
    }

    pub fn hasher(&self) -> &PieceHasher {
        &self.hasher
    }

    pub fn torrent(&self) -> &Torrent {
        &self.torrent
    }

    pub async fn write_piece(&self, index: u32, buf: &[u8]) -> Result<(), StorageError> {
        self.check_piece_index(index)?;

        let expected = calculate_piece_size(&self.torrent, index as usize);
        if buf.len() != expected {
            return Err(StorageError::BlockOutOfBounds {
                index,
                begin: 0,
                length: buf.len() as u32,
            });
        }

        let mappings = map_piece_to_files(&self.torrent, index as usize);
        let files = self.file_handles();
        let buf = buf.to_vec();
        self.invalidate_cache(index);
        spawn_blocking_io(move || write_mapped(&files, &mappings, &buf)).await
    }

    pub async fn read_block(
        &self,
        index: u32,
        begin: u32,
        length: u32,
    ) -> Result<Vec<u8>, StorageError> {
        self.check_piece_index(index)?;

        let piece_size = calculate_piece_size(&self.torrent, index as usize);
        let begin_us = begin as usize;
        let length_us = length as usize;
        if length == 0 || begin_us.saturating_add(length_us) > piece_size {
            return Err(StorageError::BlockOutOfBounds {
                index,
                begin,
                length,
            });
        }

        if let Some(hit) = self.cache_get(index, begin_us, length_us) {
            return Ok(hit);
        }

        let mappings = map_piece_to_files(&self.torrent, index as usize);
        let files = self.file_handles();
        let cache_capacity = self.cache_capacity;
        let piece = spawn_blocking_io(move || {
            if cache_capacity > 0 {
                read_mapped(&files, &mappings, 0, piece_size)
            } else {
                read_mapped(&files, &mappings, begin_us, length_us)
            }
        })
        .await?;

        if self.cache_capacity > 0 {
            let out = piece[begin_us..begin_us + length_us].to_vec();
            self.cache_put(index, piece);
            Ok(out)
        } else {
            Ok(piece)
        }
    }

    /// `sync_all` every file. Call on torrent completion and before a move.
    pub async fn sync_all(&self) -> Result<(), StorageError> {
        let files = self.file_handles();
        spawn_blocking_io(move || {
            for file in files {
                file.sync_all()?;
            }
            Ok(())
        })
        .await
    }

    pub fn output_dir(&self) -> PathBuf {
        self.output_dir
            .lock()
            .expect("storage output_dir lock")
            .clone()
    }

    /// Close, rename, and reopen every file under `new_output`.
    ///
    /// Single-file torrent: `new_output` is the file path.
    /// Multi-file torrent: `new_output` is the root directory.
    /// On failure, already-moved files are renamed back and handles stay on the original paths.
    pub async fn relocate(&self, new_output: impl AsRef<Path>) -> Result<(), StorageError> {
        self.sync_all().await?;
        let new_output = new_output.as_ref().to_path_buf();
        let current = self.output_dir();
        if current == new_output {
            return Ok(());
        }

        let mut plan = Vec::with_capacity(self.files.len());
        for index in 0..self.files.len() {
            let src = self.files[index]
                .path
                .lock()
                .expect("storage file path lock")
                .clone();
            let dest = file_path(&self.torrent, &new_output, index);
            if src != dest && dest.exists() {
                return Err(StorageError::DestinationExists(dest));
            }
            plan.push((index, src, dest));
        }

        let mut moved: Vec<(usize, PathBuf, PathBuf)> = Vec::new();
        for (index, src, dest) in plan {
            if src == dest {
                continue;
            }
            if let Err(e) = relocate_file(&self.files[index], &src, &dest) {
                for (moved_index, old, new) in moved.into_iter().rev() {
                    let _ = relocate_file(&self.files[moved_index], &new, &old);
                }
                return Err(e);
            }
            moved.push((index, src, dest));
        }

        for (_, old, _) in &moved {
            remove_empty_parents(old, &current);
        }
        *self.output_dir.lock().expect("storage output_dir lock") = new_output;
        Ok(())
    }

    fn file_handles(&self) -> Vec<Arc<File>> {
        self.files.iter().map(StorageFile::handle).collect()
    }

    fn cache_get(&self, index: u32, begin: usize, length: usize) -> Option<Vec<u8>> {
        if self.cache_capacity == 0 {
            return None;
        }
        let mut cache = self.cache.lock().expect("piece cache lock");
        cache
            .get(index)
            .map(|piece| piece[begin..begin + length].to_vec())
    }

    fn cache_put(&self, index: u32, data: Vec<u8>) {
        if self.cache_capacity == 0 {
            return;
        }
        self.cache
            .lock()
            .expect("piece cache lock")
            .put(index, data);
    }

    fn invalidate_cache(&self, index: u32) {
        if self.cache_capacity == 0 {
            return;
        }
        self.cache
            .lock()
            .expect("piece cache lock")
            .invalidate(index);
    }

    fn check_piece_index(&self, index: u32) -> Result<(), StorageError> {
        if (index as usize) >= self.torrent.piece_hashes.len() {
            return Err(StorageError::PieceOutOfRange(index));
        }
        Ok(())
    }
}

fn open_sync(
    torrent: &Torrent,
    output_dir: &Path,
    opts: StorageOptions,
) -> io::Result<Arc<Storage>> {
    let mut files = Vec::with_capacity(torrent.files.len());

    for file_index in 0..torrent.files.len() {
        let disk_path = file_path(torrent, output_dir, file_index);

        if let Some(parent) = disk_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&disk_path)?;
        let storage_file = StorageFile {
            path: Mutex::new(disk_path),
            file: RwLock::new(Arc::new(file)),
        };
        let length = torrent.files[file_index].length.max(0) as u64;
        storage_file.allocate(length, opts.preallocate)?;
        files.push(storage_file);
    }

    Ok(Arc::new(Storage {
        torrent: torrent.clone(),
        output_dir: Mutex::new(output_dir.to_path_buf()),
        files,
        cache: Mutex::new(PieceCache::new(opts.piece_cache_pieces)),
        cache_capacity: opts.piece_cache_pieces,
        hasher: opts.hasher,
    }))
}

fn relocate_file(file: &StorageFile, src: &Path, dest: &Path) -> Result<(), StorageError> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut path = file.path.lock().expect("storage file path lock");
    let mut handle = file.file.write().expect("storage file lock");
    handle.sync_all()?;
    std::fs::rename(src, dest)?;
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dest)
    {
        Ok(opened) => {
            *handle = Arc::new(opened);
            *path = dest.to_path_buf();
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::rename(dest, src);
            Err(StorageError::Io(e))
        }
    }
}

fn remove_empty_parents(path: &Path, root: &Path) {
    let mut current = path.parent().map(Path::to_path_buf);
    while let Some(dir) = current {
        if dir.as_os_str().is_empty() {
            break;
        }
        if dir != root && !dir.starts_with(root) {
            break;
        }
        if std::fs::remove_dir(&dir).is_err() {
            break;
        }
        if dir == root {
            break;
        }
        current = dir.parent().map(Path::to_path_buf);
    }
}

fn write_mapped(files: &[Arc<File>], mappings: &[PieceFileMapping], buf: &[u8]) -> io::Result<()> {
    let mut buf_offset = 0;
    for mapping in mappings {
        let slice = &buf[buf_offset..buf_offset + mapping.length];
        if !slice.is_empty() {
            write_at(
                &files[mapping.file_index],
                slice,
                mapping.file_offset as u64,
            )?;
        }
        buf_offset += mapping.length;
    }
    Ok(())
}

fn read_mapped(
    files: &[Arc<File>],
    mappings: &[PieceFileMapping],
    window_start: usize,
    window_len: usize,
) -> io::Result<Vec<u8>> {
    let window_end = window_start + window_len;
    let mut out = Vec::with_capacity(window_len);
    let mut piece_offset = 0;
    for mapping in mappings {
        let map_start = piece_offset;
        let map_end = piece_offset + mapping.length;
        let overlap_start = map_start.max(window_start);
        let overlap_end = map_end.min(window_end);

        if overlap_start < overlap_end {
            let file_offset = mapping.file_offset + (overlap_start - map_start);
            let read_len = overlap_end - overlap_start;
            let mut chunk = vec![0u8; read_len];
            read_at(&files[mapping.file_index], &mut chunk, file_offset as u64)?;
            out.extend_from_slice(&chunk);
        }

        piece_offset = map_end;
    }
    Ok(out)
}

fn write_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = positional_write(file, buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(ErrorKind::WriteZero, "write_at returned 0"));
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

fn read_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = positional_read(file, buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "read_at returned 0",
            ));
        }
        let tmp = buf;
        buf = &mut tmp[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn positional_write(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

#[cfg(unix)]
fn positional_read(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(windows)]
fn positional_write(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

#[cfg(windows)]
fn positional_read(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

#[cfg(not(any(unix, windows)))]
fn positional_write(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = file;
    file.seek(SeekFrom::Start(offset))?;
    file.write(buf)
}

#[cfg(not(any(unix, windows)))]
fn positional_read(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = file;
    file.seek(SeekFrom::Start(offset))?;
    file.read(buf)
}

async fn spawn_blocking_io<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, io::Error> + Send + 'static,
) -> Result<T, StorageError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(StorageError::Io(e)),
        Err(e) => Err(StorageError::Io(io::Error::other(e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::{Torrent, TorrentFileInfo};
    use rand::RngCore;
    use std::path::{Path, PathBuf};

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
        let piece_count = if piece_length <= 0 || offset <= 0 {
            0
        } else {
            ((offset + piece_length - 1) / piece_length) as usize
        };
        Torrent {
            info_hash: [0; 20],
            piece_hashes: vec![[0u8; 20]; piece_count],
            piece_length,
            length: offset,
            files,
            name: "t".into(),
            private: false,
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let mut suffix = [0u8; 8];
            rand::thread_rng().fill_bytes(&mut suffix);
            let dir = std::env::temp_dir().join(format!(
                "bitrev-storage-{}-{}",
                std::process::id(),
                suffix
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn disk_len(dir: &Path, torrent: &Torrent, index: usize) -> u64 {
        std::fs::metadata(file_path(torrent, dir, index))
            .unwrap()
            .len()
    }

    #[tokio::test]
    async fn write_and_read_single_file_piece() {
        let t = torrent(&[100], 40);
        let tmp = TempDir::new();
        let path = tmp.path().join("out.bin");
        let storage = Storage::open(&t, &path).await.unwrap();

        let data: Vec<u8> = (0..40)
            .map(|i| (i as u8).wrapping_mul(3).wrapping_add(1))
            .collect();
        storage.write_piece(0, &data).await.unwrap();

        let block = storage.read_block(0, 10, 15).await.unwrap();
        assert_eq!(block, &data[10..25]);
        assert_eq!(storage.torrent().name, "t");
    }

    #[tokio::test]
    async fn write_and_read_block_spanning_two_files() {
        let t = torrent(&[30, 30], 40);
        let tmp = TempDir::new();
        let storage = Storage::open(&t, tmp.path()).await.unwrap();

        let data: Vec<u8> = (0..40).collect();
        storage.write_piece(0, &data).await.unwrap();

        let block = storage.read_block(0, 20, 20).await.unwrap();
        assert_eq!(block, &data[20..40]);
        assert_eq!(&block[..10], &data[20..30]);
        assert_eq!(&block[10..], &data[30..40]);
    }

    #[tokio::test]
    async fn read_block_out_of_bounds_errors() {
        let t = torrent(&[100], 40);
        let tmp = TempDir::new();
        let path = tmp.path().join("out.bin");
        let storage = Storage::open(&t, &path).await.unwrap();
        storage.write_piece(0, &[7u8; 40]).await.unwrap();

        let past_end = storage.read_block(0, 30, 20).await.unwrap_err();
        assert!(matches!(
            past_end,
            StorageError::BlockOutOfBounds {
                index: 0,
                begin: 30,
                length: 20
            }
        ));

        let zero_len = storage.read_block(0, 0, 0).await.unwrap_err();
        assert!(matches!(
            zero_len,
            StorageError::BlockOutOfBounds {
                index: 0,
                begin: 0,
                length: 0
            }
        ));

        let bad_index = storage.read_block(99, 0, 1).await.unwrap_err();
        assert!(matches!(bad_index, StorageError::PieceOutOfRange(99)));
    }

    #[tokio::test]
    async fn write_piece_out_of_range_errors() {
        let t = torrent(&[100], 40);
        let tmp = TempDir::new();
        let path = tmp.path().join("out.bin");
        let storage = Storage::open(&t, &path).await.unwrap();

        let err = storage.write_piece(99, &[0u8; 40]).await.unwrap_err();
        assert!(matches!(err, StorageError::PieceOutOfRange(99)));
    }

    #[tokio::test]
    async fn open_does_not_truncate_existing() {
        let t = torrent(&[40], 40);
        let tmp = TempDir::new();
        let path = tmp.path().join("existing.bin");
        let expected: Vec<u8> = (0..40).map(|i| (i as u8).wrapping_add(9)).collect();
        std::fs::write(&path, &expected).unwrap();

        let storage = Storage::open(&t, &path).await.unwrap();
        let got = storage.read_block(0, 0, 40).await.unwrap();
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn three_file_span() {
        let t = torrent(&[10, 10, 10], 25);
        let tmp = TempDir::new();
        let storage = Storage::open(&t, tmp.path()).await.unwrap();

        let data: Vec<u8> = (0..25).map(|i| (i as u8).wrapping_add(50)).collect();
        storage.write_piece(0, &data).await.unwrap();

        let got = storage.read_block(0, 0, 25).await.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn last_short_piece_and_zero_length_file() {
        let t = torrent(&[20, 0, 20], 30);
        let tmp = TempDir::new();
        let storage = Storage::open(&t, tmp.path()).await.unwrap();

        let data: Vec<u8> = (0..30).map(|i| (i as u8).wrapping_add(7)).collect();
        storage.write_piece(0, &data).await.unwrap();

        let got = storage.read_block(0, 0, 30).await.unwrap();
        assert_eq!(got, data);
        assert_eq!(disk_len(tmp.path(), &t, 1), 0);
    }

    #[tokio::test]
    async fn preallocate_sparse_sets_file_sizes() {
        let t = torrent(&[100, 50], 40);
        let tmp = TempDir::new();
        let _storage = Storage::open_with(
            &t,
            tmp.path(),
            StorageOptions {
                preallocate: Preallocate::Sparse,
                ..StorageOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(disk_len(tmp.path(), &t, 0), 100);
        assert_eq!(disk_len(tmp.path(), &t, 1), 50);
    }

    #[tokio::test]
    async fn preallocate_full_sets_file_sizes() {
        let t = torrent(&[80], 40);
        let tmp = TempDir::new();
        let path = tmp.path().join("full.bin");
        let _storage = Storage::open_with(
            &t,
            &path,
            StorageOptions {
                preallocate: Preallocate::Full,
                ..StorageOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 80);
    }

    #[tokio::test]
    async fn preallocate_off_leaves_empty_file() {
        let t = torrent(&[100], 40);
        let tmp = TempDir::new();
        let path = tmp.path().join("off.bin");
        let _storage = Storage::open_with(
            &t,
            &path,
            StorageOptions {
                preallocate: Preallocate::Off,
                ..StorageOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn preallocate_does_not_shrink_or_touch_matching_size() {
        let t = torrent(&[40], 40);
        let tmp = TempDir::new();
        let path = tmp.path().join("keep.bin");
        let expected: Vec<u8> = (0..40).map(|i| i as u8).collect();
        std::fs::write(&path, &expected).unwrap();
        let mtime_before = std::fs::metadata(&path).unwrap().modified().unwrap();

        let storage = Storage::open_with(
            &t,
            &path,
            StorageOptions {
                preallocate: Preallocate::Sparse,
                ..StorageOptions::default()
            },
        )
        .await
        .unwrap();
        let mtime_after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after);
        assert_eq!(storage.read_block(0, 0, 40).await.unwrap(), expected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reads_during_writes() {
        let t = torrent(&[1024 * 1024], 16 * 1024);
        let tmp = TempDir::new();
        let path = tmp.path().join("rw.bin");
        let storage = Storage::open(&t, &path).await.unwrap();
        let writer = storage.clone();
        let reader = storage.clone();
        let piece = vec![0xABu8; 16 * 1024];
        let piece_for_write = piece.clone();

        let write = tokio::spawn(async move {
            for i in 0..64u32 {
                writer.write_piece(i, &piece_for_write).await.unwrap();
            }
        });
        let read = tokio::spawn(async move {
            for i in 0..128u32 {
                let _ = reader.read_block(i % 64, 0, 16 * 1024).await;
            }
        });
        write.await.unwrap();
        read.await.unwrap();
        let got = storage.read_block(0, 0, 16 * 1024).await.unwrap();
        assert_eq!(got, piece);
    }

    #[tokio::test]
    async fn relocate_moves_single_file_and_reads_back() {
        let t = torrent(&[40], 40);
        let tmp = TempDir::new();
        let src = tmp.path().join("old.bin");
        let dest = tmp.path().join("moved").join("new.bin");
        let storage = Storage::open(&t, &src).await.unwrap();
        let data: Vec<u8> = (0..40).collect();
        storage.write_piece(0, &data).await.unwrap();

        storage.relocate(&dest).await.unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(storage.read_block(0, 0, 40).await.unwrap(), data);
        assert_eq!(storage.output_dir(), dest);
    }

    #[tokio::test]
    async fn relocate_moves_multi_file_tree() {
        let t = torrent(&[20, 20], 40);
        let tmp = TempDir::new();
        let src = tmp.path().join("old");
        let dest = tmp.path().join("new");
        let storage = Storage::open(&t, &src).await.unwrap();
        let data: Vec<u8> = (0..40).collect();
        storage.write_piece(0, &data).await.unwrap();

        storage.relocate(&dest).await.unwrap();
        assert!(!file_path(&t, &src, 0).exists());
        assert_eq!(std::fs::read(file_path(&t, &dest, 0)).unwrap(), &data[..20]);
        assert_eq!(std::fs::read(file_path(&t, &dest, 1)).unwrap(), &data[20..]);
        assert_eq!(storage.read_block(0, 0, 40).await.unwrap(), data);
    }

    #[tokio::test]
    async fn relocate_fails_when_destination_exists_and_leaves_source() {
        let t = torrent(&[40], 40);
        let tmp = TempDir::new();
        let src = tmp.path().join("old.bin");
        let dest = tmp.path().join("new.bin");
        let storage = Storage::open(&t, &src).await.unwrap();
        let data: Vec<u8> = (0..40).map(|i| (i as u8).wrapping_add(3)).collect();
        storage.write_piece(0, &data).await.unwrap();
        std::fs::write(&dest, b"occupied").unwrap();

        let err = storage.relocate(&dest).await.unwrap_err();
        assert!(matches!(err, StorageError::DestinationExists(_)));
        assert_eq!(std::fs::read(&src).unwrap(), data);
        assert_eq!(std::fs::read(&dest).unwrap(), b"occupied");
        assert_eq!(storage.read_block(0, 0, 40).await.unwrap(), data);
    }

    #[tokio::test]
    async fn piece_cache_serves_hot_range() {
        let t = torrent(&[80], 40);
        let tmp = TempDir::new();
        let path = tmp.path().join("cache.bin");
        let storage = Storage::open_with(
            &t,
            &path,
            StorageOptions {
                piece_cache_pieces: 16,
                ..StorageOptions::default()
            },
        )
        .await
        .unwrap();
        let data: Vec<u8> = (0..40).collect();
        storage.write_piece(0, &data).await.unwrap();
        let a = storage.read_block(0, 0, 10).await.unwrap();
        let b = storage.read_block(0, 10, 10).await.unwrap();
        assert_eq!(a, &data[0..10]);
        assert_eq!(b, &data[10..20]);
    }
}
