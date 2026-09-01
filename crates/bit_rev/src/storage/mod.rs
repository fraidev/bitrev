use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::torrent::Torrent;
use crate::utils::{calculate_piece_size, map_piece_to_files};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("piece {0} out of range")]
    PieceOutOfRange(u32),
    #[error("block out of bounds: piece={index} begin={begin} length={length}")]
    BlockOutOfBounds { index: u32, begin: u32, length: u32 },
}

pub struct Storage {
    torrent: Torrent,
    files: Vec<Mutex<File>>,
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
        let output_dir = output_dir.as_ref();
        let mut files = Vec::with_capacity(torrent.files.len());

        for file_index in 0..torrent.files.len() {
            let disk_path = file_path(torrent, output_dir, file_index);

            if let Some(parent) = disk_path.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent).await?;
                }
            }

            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&disk_path)
                .await?;
            files.push(Mutex::new(file));
        }

        Ok(Arc::new(Self {
            torrent: torrent.clone(),
            files,
        }))
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
        let mut buf_offset = 0;
        for mapping in mappings {
            let slice = &buf[buf_offset..buf_offset + mapping.length];
            {
                let mut file = self.files[mapping.file_index].lock().await;
                file.seek(SeekFrom::Start(mapping.file_offset as u64))
                    .await?;
                file.write_all(slice).await?;
            }
            buf_offset += mapping.length;
        }

        Ok(())
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

        let window_start = begin_us;
        let window_end = begin_us + length_us;
        let mappings = map_piece_to_files(&self.torrent, index as usize);

        let mut out = Vec::with_capacity(length_us);
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
                {
                    let mut file = self.files[mapping.file_index].lock().await;
                    file.seek(SeekFrom::Start(file_offset as u64)).await?;
                    file.read_exact(&mut chunk).await?;
                }
                out.extend_from_slice(&chunk);
            }

            piece_offset = map_end;
        }

        Ok(out)
    }

    pub fn torrent(&self) -> &Torrent {
        &self.torrent
    }

    fn check_piece_index(&self, index: u32) -> Result<(), StorageError> {
        if (index as usize) >= self.torrent.piece_hashes.len() {
            return Err(StorageError::PieceOutOfRange(index));
        }
        Ok(())
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
}
