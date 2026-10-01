//! Build a BEP-0003 `.torrent` from a file or directory.
//!
//! [`create_torrent`] reads and hashes on the calling thread. Async callers
//! should run it inside [`tokio::task::spawn_blocking`] so a large tree does
//! not stall the runtime.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_bencode::ser;
use serde_bytes::ByteBuf;
use sha1::{Digest, Sha1};

use crate::file::{self, Info, TorrentFile};
use crate::identity;

/// Smallest piece length [`auto_piece_length`] will pick.
pub const AUTO_PIECE_MIN: u64 = 16 * 1024;
/// Largest piece length [`auto_piece_length`] will pick.
pub const AUTO_PIECE_MAX: u64 = 16 * 1024 * 1024;

const TARGET_PIECES: u64 = 2_000;
const READ_CHUNK: usize = 64 * 1024;

/// Inputs for [`create_torrent`].
///
/// `piece_length` of `0` selects a power of two from the total size.
#[derive(Clone)]
pub struct CreateOptions {
    source: PathBuf,
    piece_length: u64,
    announce: Option<String>,
    announce_tiers: Vec<Vec<String>>,
    private: bool,
    comment: Option<String>,
    created_by: Option<String>,
    url_list: Vec<String>,
    name: Option<String>,
    exclude: Option<PathBuf>,
    progress: Option<Arc<dyn Fn(u64, u64) + Send + Sync>>,
}

impl CreateOptions {
    pub fn new(source: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            piece_length: 0,
            announce: None,
            announce_tiers: Vec::new(),
            private: false,
            comment: None,
            created_by: None,
            url_list: Vec::new(),
            name: None,
            exclude: None,
            progress: None,
        }
    }

    /// Piece length in bytes. `0` picks one with [`auto_piece_length`].
    pub fn piece_length(mut self, piece_length: u64) -> Self {
        self.piece_length = piece_length;
        self
    }

    /// Top-level `announce` key. When unset, the first tier URL is used.
    pub fn announce(mut self, url: impl Into<String>) -> Self {
        let url = url.into();
        if !url.is_empty() {
            self.announce = Some(url);
        }
        self
    }

    /// One tracker URL as its own BEP-0012 tier. Repeat for fallback order.
    pub fn announce_url(mut self, url: impl Into<String>) -> Self {
        let url = url.into();
        if !url.is_empty() {
            self.announce_tiers.push(vec![url]);
        }
        self
    }

    /// One BEP-0012 tier. URLs in a tier are backups of each other.
    pub fn announce_tier(mut self, urls: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let tier: Vec<String> = urls
            .into_iter()
            .map(Into::into)
            .filter(|url| !url.is_empty())
            .collect();
        if !tier.is_empty() {
            self.announce_tiers.push(tier);
        }
        self
    }

    /// Replace the announce tiers.
    pub fn announce_tiers(mut self, tiers: Vec<Vec<String>>) -> Self {
        self.announce_tiers = tiers;
        self
    }

    /// BEP-0027 `private=1` when `private` is set. Otherwise the key is omitted.
    pub fn private(mut self, private: bool) -> Self {
        self.private = private;
        self
    }

    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        let comment = comment.into();
        if !comment.is_empty() {
            self.comment = Some(comment);
        }
        self
    }

    /// Overrides the default `bitrev <version>` created-by string.
    pub fn created_by(mut self, created_by: impl Into<String>) -> Self {
        let created_by = created_by.into();
        if !created_by.is_empty() {
            self.created_by = Some(created_by);
        }
        self
    }

    /// BEP-0019 web seed. Repeat to add another. Order is preserved.
    pub fn web_seed(mut self, url: impl Into<String>) -> Self {
        let url = url.into();
        if !url.is_empty() {
            self.url_list.push(url);
        }
        self
    }

    pub fn url_list(mut self, urls: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.url_list = urls
            .into_iter()
            .map(Into::into)
            .filter(|url| !url.is_empty())
            .collect();
        self
    }

    /// Override `info.name`. The default is the source file or directory name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Skip this path while walking a directory. [`create_torrent_file`] sets
    /// it to the output torrent so a tree does not include itself.
    pub fn exclude(mut self, path: impl Into<PathBuf>) -> Self {
        self.exclude = Some(path.into());
        self
    }

    /// Called with `(bytes_hashed, total_bytes)` as each piece finishes.
    /// Runs on the hashing thread.
    pub fn progress<F>(mut self, callback: F) -> Self
    where
        F: Fn(u64, u64) + Send + Sync + 'static,
    {
        self.progress = Some(Arc::new(callback));
        self
    }
}

impl std::fmt::Debug for CreateOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateOptions")
            .field("source", &self.source)
            .field("piece_length", &self.piece_length)
            .field("announce", &self.announce)
            .field("announce_tiers", &self.announce_tiers)
            .field("private", &self.private)
            .field("comment", &self.comment)
            .field("created_by", &self.created_by)
            .field("url_list", &self.url_list)
            .field("name", &self.name)
            .field("exclude", &self.exclude)
            .field("progress", &self.progress.as_ref().map(|_| ()))
            .finish()
    }
}

/// Power-of-two piece length for `total_length` bytes.
///
/// The result stays between 16 KiB and 16 MiB. It is the smallest size in
/// that range whose piece count is at most 2000. Files too small to fill
/// 1000 pieces stay at 16 KiB. Files that still exceed 2000 pieces at 16 MiB
/// stay at 16 MiB.
pub fn auto_piece_length(total_length: u64) -> u64 {
    let mut length = AUTO_PIECE_MIN;
    if total_length == 0 {
        return length;
    }
    while length < AUTO_PIECE_MAX && total_length.div_ceil(length) > TARGET_PIECES {
        length *= 2;
    }
    length
}

/// Bencoded metainfo for `opts`.
///
/// A directory becomes a multi-file torrent, including a directory that holds
/// one file. A file becomes a single-file torrent. Paths are checked with the
/// same rules as the parser. File order is sorted. Empty directories are
/// skipped. Zero-length files are kept. Symlinks are skipped.
pub fn create_torrent(opts: CreateOptions) -> Result<Vec<u8>> {
    let (layout, total) = collect(&opts)?;
    let piece_length = if opts.piece_length == 0 {
        auto_piece_length(total)
    } else {
        opts.piece_length
    };
    if piece_length == 0 {
        anyhow::bail!("piece length must be positive");
    }
    let piece_length_i64 =
        i64::try_from(piece_length).context("piece length does not fit in the info dict")?;
    let piece_length_usize =
        usize::try_from(piece_length).context("piece length does not fit in memory")?;

    let pieces = hash_pieces(
        layout.files(),
        total,
        piece_length_usize,
        opts.progress.as_deref(),
    )?;
    encode_metainfo(&opts, &layout, piece_length_i64, pieces)
}

/// Write metainfo for `opts` to `out_path`, creating parent directories.
///
/// `out_path` is left out of a directory walk so the torrent does not list itself.
pub fn create_torrent_file(opts: CreateOptions, out_path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let out_path = out_path.as_ref();
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
    }
    let bytes = create_torrent(opts.exclude(out_path))?;
    std::fs::write(out_path, &bytes).with_context(|| format!("write {}", out_path.display()))?;
    Ok(bytes)
}

struct SourceFile {
    rel: Vec<String>,
    abs: PathBuf,
    length: u64,
}

enum Layout {
    Single {
        file: SourceFile,
        name: String,
    },
    Multi {
        files: Vec<SourceFile>,
        name: String,
    },
}

impl Layout {
    fn name(&self) -> &str {
        match self {
            Self::Single { name, .. } | Self::Multi { name, .. } => name,
        }
    }

    fn files(&self) -> &[SourceFile] {
        match self {
            Self::Single { file, .. } => std::slice::from_ref(file),
            Self::Multi { files, .. } => files,
        }
    }
}

fn collect(opts: &CreateOptions) -> Result<(Layout, u64)> {
    let meta = std::fs::metadata(&opts.source)
        .with_context(|| format!("open {}", opts.source.display()))?;
    let name = resolve_name(opts)?;
    if meta.is_file() {
        let total = meta.len();
        return Ok((
            Layout::Single {
                file: SourceFile {
                    rel: Vec::new(),
                    abs: opts.source.clone(),
                    length: total,
                },
                name,
            },
            total,
        ));
    }
    if !meta.is_dir() {
        anyhow::bail!("{} is not a file or directory", opts.source.display());
    }

    let mut files = Vec::new();
    walk(
        &opts.source,
        &opts.source,
        opts.exclude.as_deref(),
        &mut files,
    )?;
    if files.is_empty() {
        anyhow::bail!("no files to include");
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    for pair in files.windows(2) {
        if pair[0].rel == pair[1].rel {
            anyhow::bail!("duplicate file path: {}", pair[0].rel.join("/"));
        }
    }
    let total = files
        .iter()
        .try_fold(0u64, |acc, file| acc.checked_add(file.length))
        .context("total size overflow")?;
    Ok((Layout::Multi { files, name }, total))
}

fn resolve_name(opts: &CreateOptions) -> Result<String> {
    if let Some(name) = opts.name.as_deref() {
        validate_component(name)?;
        return Ok(name.to_string());
    }
    let raw = opts.source.file_name().ok_or_else(|| {
        anyhow::anyhow!("source path has no file name: {}", opts.source.display())
    })?;
    let name = raw.to_str().ok_or_else(|| {
        anyhow::anyhow!("file name is not valid Unicode: {}", opts.source.display())
    })?;
    validate_component(name)?;
    Ok(name.to_string())
}

fn validate_component(name: &str) -> Result<()> {
    if !file::path_component_is_safe(name) {
        anyhow::bail!("unsafe file path component: {name:?}");
    }
    Ok(())
}

fn walk(dir: &Path, root: &Path, exclude: Option<&Path>, out: &mut Vec<SourceFile>) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", dir.display()))?;
        let path = entry.path();
        if exclude.is_some_and(|excluded| paths_match(&path, excluded)) {
            continue;
        }
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat {}", path.display()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            walk(&path, root, exclude, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .with_context(|| format!("{} is outside {}", path.display(), root.display()))?;
        let length = entry
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?
            .len();
        out.push(SourceFile {
            rel: components_from_rel(rel)?,
            abs: path,
            length,
        });
    }
    Ok(())
}

fn components_from_rel(rel: &Path) -> Result<Vec<String>> {
    let mut components = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => {
                let text = part.to_str().ok_or_else(|| {
                    anyhow::anyhow!("file name is not valid Unicode: {}", rel.display())
                })?;
                components.push(text.to_string());
            }
            Component::CurDir => {}
            _ => anyhow::bail!("unsafe file path: {}", rel.display()),
        }
    }
    file::validate_file_path(&components)?;
    Ok(components)
}

fn paths_match(candidate: &Path, excluded: &Path) -> bool {
    if normalize_path(candidate) == normalize_path(excluded) {
        return true;
    }
    match (
        std::fs::canonicalize(candidate),
        std::fs::canonicalize(excluded),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn hash_pieces(
    files: &[SourceFile],
    total: u64,
    piece_length: usize,
    progress: Option<&(dyn Fn(u64, u64) + Send + Sync)>,
) -> Result<Vec<u8>> {
    let piece_count = if total == 0 {
        0
    } else {
        total.div_ceil(piece_length as u64)
    };
    let piece_count = usize::try_from(piece_count).context("too many pieces")?;
    if piece_count.checked_mul(20).is_none() {
        anyhow::bail!("too many pieces");
    }
    let mut pieces = Vec::with_capacity(piece_count * 20);
    if total == 0 {
        if let Some(callback) = progress {
            callback(0, 0);
        }
        return Ok(pieces);
    }

    let mut hasher = Sha1::new();
    let mut filled = 0usize;
    let mut hashed = 0u64;
    let mut buf = vec![0u8; READ_CHUNK];

    for file in files {
        if file.length == 0 {
            continue;
        }
        let mut handle = std::fs::File::open(&file.abs)
            .with_context(|| format!("open {}", file.abs.display()))?;
        let mut remaining = file.length;
        while remaining > 0 {
            let want = buf
                .len()
                .min(usize::try_from(remaining).unwrap_or(usize::MAX));
            handle
                .read_exact(&mut buf[..want])
                .with_context(|| format!("read {}", file.abs.display()))?;
            let mut off = 0;
            while off < want {
                let take = (piece_length - filled).min(want - off);
                hasher.update(&buf[off..off + take]);
                filled += take;
                off += take;
                hashed += take as u64;
                if filled == piece_length {
                    push_hash(&mut pieces, &mut hasher);
                    filled = 0;
                    if let Some(callback) = progress {
                        callback(hashed, total);
                    }
                }
            }
            remaining -= want as u64;
        }
    }
    if filled > 0 {
        let digest: [u8; 20] = hasher.finalize().into();
        pieces.extend_from_slice(&digest);
        if let Some(callback) = progress {
            callback(hashed, total);
        }
    }
    if pieces.len() != piece_count * 20 {
        anyhow::bail!(
            "piece hash length mismatch: got {} bytes for {piece_count} pieces",
            pieces.len()
        );
    }
    Ok(pieces)
}

fn push_hash(pieces: &mut Vec<u8>, hasher: &mut Sha1) {
    let digest: [u8; 20] = hasher.finalize_reset().into();
    pieces.extend_from_slice(&digest);
}

fn encode_metainfo(
    opts: &CreateOptions,
    layout: &Layout,
    piece_length: i64,
    pieces: Vec<u8>,
) -> Result<Vec<u8>> {
    let (length, files) = match layout {
        Layout::Single { file, .. } => (
            Some(i64::try_from(file.length).context("file length does not fit in the info dict")?),
            None,
        ),
        Layout::Multi { files, .. } => {
            let mut encoded = Vec::with_capacity(files.len());
            for file in files {
                encoded.push(file::File {
                    path: file.rel.clone(),
                    length: i64::try_from(file.length)
                        .context("file length does not fit in the info dict")?,
                    md5sum: None,
                });
            }
            (None, Some(encoded))
        }
    };

    let tiers = announce_tiers(opts);
    let announce = opts
        .announce
        .clone()
        .filter(|url| !url.is_empty())
        .or_else(|| tiers.first().and_then(|tier| tier.first().cloned()));
    let created_by = match opts.created_by.as_deref() {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => identity::extension_version(),
    };
    let url_list = if opts.url_list.is_empty() {
        None
    } else {
        Some(opts.url_list.clone())
    };

    let torrent_file = TorrentFile {
        info: Info {
            name: layout.name().to_string(),
            pieces: ByteBuf::from(pieces),
            piece_length,
            md5sum: None,
            length,
            files,
            private: opts.private.then_some(1),
            path: None,
            root_hash: None,
        },
        announce,
        nodes: None,
        encoding: None,
        httpseeds: None,
        url_list,
        announce_list: if tiers.is_empty() { None } else { Some(tiers) },
        creation_date: Some(unix_now()),
        comment: opts.comment.clone().filter(|text| !text.is_empty()),
        created_by: Some(created_by),
    };

    let encoded = ser::to_bytes(&torrent_file).context("bencode torrent")?;
    let info_bytes = ser::to_bytes(&torrent_file.info).context("bencode info dict")?;
    match file::raw_info_dict(&encoded) {
        Ok(raw) if raw == info_bytes.as_slice() => Ok(encoded),
        Ok(_) => anyhow::bail!("info dict encoding changed between the torrent and the info hash"),
        Err(err) => Err(err),
    }
}

fn announce_tiers(opts: &CreateOptions) -> Vec<Vec<String>> {
    opts.announce_tiers
        .iter()
        .filter_map(|tier| {
            let urls: Vec<String> = tier.iter().filter(|url| !url.is_empty()).cloned().collect();
            if urls.is_empty() {
                None
            } else {
                Some(urls)
            }
        })
        .collect()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::file::{self, from_bytes};
    use crate::utils::sha1_digest;

    fn hashes_of(data: &[u8], piece_length: usize) -> Vec<[u8; 20]> {
        if data.is_empty() {
            return Vec::new();
        }
        data.chunks(piece_length).map(sha1_digest).collect()
    }

    fn assert_info_round_trip(bytes: &[u8]) {
        let meta = from_bytes(bytes).expect("parse created torrent");
        let raw = file::raw_info_dict(bytes).expect("raw info");
        assert_eq!(meta.info_hash, sha1_digest(raw));
        assert_eq!(meta.info_bytes.as_ref(), raw);
        let reencoded = ser::to_bytes(&meta.torrent_file.info).expect("re-encode info");
        assert_eq!(reencoded.as_slice(), raw);
    }

    #[test]
    fn auto_piece_length_picks_power_of_two() {
        assert_eq!(auto_piece_length(0), AUTO_PIECE_MIN);
        assert_eq!(auto_piece_length(1), AUTO_PIECE_MIN);
        assert_eq!(
            auto_piece_length(TARGET_PIECES * AUTO_PIECE_MIN),
            AUTO_PIECE_MIN
        );
        assert_eq!(
            auto_piece_length(TARGET_PIECES * AUTO_PIECE_MIN + 1),
            AUTO_PIECE_MIN * 2
        );
        assert_eq!(
            auto_piece_length(TARGET_PIECES * AUTO_PIECE_MIN * 2),
            AUTO_PIECE_MIN * 2
        );
        assert_eq!(
            auto_piece_length(TARGET_PIECES * AUTO_PIECE_MIN * 2 + 1),
            AUTO_PIECE_MIN * 4
        );
        assert_eq!(
            auto_piece_length(TARGET_PIECES * AUTO_PIECE_MAX),
            AUTO_PIECE_MAX
        );
        assert_eq!(
            auto_piece_length(TARGET_PIECES * AUTO_PIECE_MAX + 1),
            AUTO_PIECE_MAX
        );
        let picked = auto_piece_length(50 * 1024 * 1024);
        assert!(picked.is_power_of_two());
        assert_eq!(picked, 32 * 1024);
        assert!((1_000..=TARGET_PIECES).contains(&(50 * 1024 * 1024u64).div_ceil(picked)));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let len = TARGET_PIECES * AUTO_PIECE_MIN + 1;
        {
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(len).unwrap();
        }
        let bytes = create_torrent(CreateOptions::new(&path)).unwrap();
        let meta = from_bytes(&bytes).unwrap();
        assert_eq!(
            meta.torrent_file.info.piece_length,
            (AUTO_PIECE_MIN * 2) as i64
        );
        assert_eq!(
            meta.piece_hashes.len() as u64,
            len.div_ceil(AUTO_PIECE_MIN * 2)
        );
        assert_eq!(
            meta.piece_hashes[0],
            sha1_digest(&vec![0u8; (AUTO_PIECE_MIN * 2) as usize])
        );
        assert_eq!(*meta.piece_hashes.last().unwrap(), sha1_digest(&[0u8]));
        assert_info_round_trip(&bytes);
    }

    #[test]
    fn multi_file_round_trip_is_sorted_stable_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("bundle");
        std::fs::create_dir_all(root.join("empty").join("nested")).unwrap();
        std::fs::create_dir_all(root.join("m")).unwrap();
        std::fs::write(root.join(".hidden"), b"h").unwrap();
        std::fs::write(root.join("z.txt"), b"zzz").unwrap();
        std::fs::write(root.join("a.txt"), b"alpha").unwrap();
        std::fs::write(root.join("empty.bin"), b"").unwrap();
        std::fs::write(root.join("m").join("b.txt"), b"0123456789abcdefghij").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("a.txt"), root.join("link.txt")).unwrap();

        let base = || {
            CreateOptions::new(&root)
                .piece_length(8)
                .private(true)
                .comment("note")
                .name("Renamed")
                .announce_url("http://a.example/announce")
                .announce_url("http://b.example/announce")
                .web_seed("http://cdn.example/a")
                .web_seed("http://cdn.example/b")
        };
        let log = Arc::new(Mutex::new(Vec::new()));
        let log_cb = Arc::clone(&log);
        let first = create_torrent(base().progress(move |done, total| {
            assert!(done <= total);
            log_cb.lock().unwrap().push((done, total));
        }))
        .unwrap();
        let second = create_torrent(base()).unwrap();
        assert_info_round_trip(&first);
        assert_info_round_trip(&second);

        let meta = from_bytes(&first).unwrap();
        let again = from_bytes(&second).unwrap();
        assert_eq!(meta.info_hash, again.info_hash);
        assert_eq!(
            file::raw_info_dict(&first).unwrap(),
            file::raw_info_dict(&second).unwrap()
        );

        assert_eq!(meta.torrent_file.info.name, "Renamed");
        assert_eq!(meta.torrent_file.info.private, Some(1));
        assert!(meta.torrent_file.info.is_private());
        assert!(meta.torrent_file.info.length.is_none());
        let raw = file::raw_info_dict(&first).unwrap();
        assert!(raw.windows(12).any(|window| window == b"7:privatei1e"));
        assert_eq!(meta.torrent_file.comment.as_deref(), Some("note"));
        let created_by = identity::extension_version();
        assert_eq!(
            meta.torrent_file.created_by.as_deref(),
            Some(created_by.as_str())
        );
        assert!(meta.torrent_file.creation_date.is_some());
        assert_eq!(
            meta.torrent_file.announce.as_deref(),
            Some("http://a.example/announce")
        );
        assert_eq!(
            meta.torrent_file.announce_list.unwrap(),
            vec![
                vec!["http://a.example/announce".to_string()],
                vec!["http://b.example/announce".to_string()]
            ]
        );
        assert_eq!(
            meta.torrent_file.url_list.unwrap(),
            ["http://cdn.example/a", "http://cdn.example/b"]
        );

        let files = meta.torrent_file.info.files.unwrap();
        let paths: Vec<&[String]> = files.iter().map(|file| file.path.as_slice()).collect();
        assert_eq!(
            paths,
            [
                [".hidden".to_string()].as_slice(),
                ["a.txt".to_string()].as_slice(),
                ["empty.bin".to_string()].as_slice(),
                ["m".to_string(), "b.txt".to_string()].as_slice(),
                ["z.txt".to_string()].as_slice(),
            ]
        );
        assert!(paths.iter().all(|path| !path
            .iter()
            .any(|part| part == "empty" || part == "link.txt")));
        assert_eq!(files[2].length, 0);

        let mut data = Vec::new();
        data.extend_from_slice(b"h");
        data.extend_from_slice(b"alpha");
        data.extend_from_slice(b"0123456789abcdefghij");
        data.extend_from_slice(b"zzz");
        assert_eq!(data.len(), 29);
        assert_eq!(meta.piece_hashes, hashes_of(&data, 8));

        let entries = log.lock().unwrap().clone();
        assert_eq!(entries.last().copied(), Some((29, 29)));
        assert!(entries.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    }

    #[test]
    fn file_source_is_single_and_directory_of_one_file_is_multi() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("movie.bin");
        std::fs::write(&file, b"hello world").unwrap();

        let bytes = create_torrent(CreateOptions::new(&file).piece_length(16 * 1024)).unwrap();
        let meta = from_bytes(&bytes).unwrap();
        assert_eq!(meta.torrent_file.info.name, "movie.bin");
        assert_eq!(meta.torrent_file.info.length, Some(11));
        assert!(meta.torrent_file.info.files.is_none());
        assert!(meta.torrent_file.info.private.is_none());
        assert_eq!(meta.piece_hashes, hashes_of(b"hello world", 16 * 1024));
        assert_info_round_trip(&bytes);

        let renamed = create_torrent(
            CreateOptions::new(&file)
                .piece_length(16 * 1024)
                .name("other"),
        )
        .unwrap();
        assert_eq!(
            from_bytes(&renamed).unwrap().torrent_file.info.name,
            "other"
        );

        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("movie.bin"), b"hello world").unwrap();
        let bytes = create_torrent(CreateOptions::new(&folder).piece_length(32)).unwrap();
        let meta = from_bytes(&bytes).unwrap();
        assert_eq!(meta.torrent_file.info.name, "folder");
        assert!(meta.torrent_file.info.length.is_none());
        let files = meta.torrent_file.info.files.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, ["movie.bin"]);
        assert_eq!(files[0].length, 11);
        assert_eq!(meta.piece_hashes, hashes_of(b"hello world", 32));
        assert_info_round_trip(&bytes);
    }

    #[test]
    fn zero_length_file_has_no_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("empty.bin");
        std::fs::write(&file, b"").unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let calls_cb = Arc::clone(&calls);
        let bytes = create_torrent(CreateOptions::new(&file).piece_length(16 * 1024).progress(
            move |done, total| {
                assert_eq!((done, total), (0, 0));
                calls_cb.fetch_add(1, Ordering::Relaxed);
            },
        ))
        .unwrap();
        let meta = from_bytes(&bytes).unwrap();
        assert_eq!(meta.torrent_file.info.length, Some(0));
        assert!(meta.piece_hashes.is_empty());
        assert!(meta.torrent_file.info.pieces.is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_info_round_trip(&bytes);
    }

    #[test]
    fn empty_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("empty").join("nested")).unwrap();
        let err = create_torrent(CreateOptions::new(dir.path())).unwrap_err();
        assert!(err.to_string().contains("no files"), "{err}");
    }

    #[test]
    fn unsafe_names_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ok.bin");
        std::fs::write(&file, b"x").unwrap();
        let err = create_torrent(CreateOptions::new(&file).name("..")).unwrap_err();
        assert!(err.to_string().contains("unsafe"), "{err}");
        let err = create_torrent(CreateOptions::new(&file).name("a/b")).unwrap_err();
        assert!(err.to_string().contains("unsafe"), "{err}");

        let tree = tempfile::tempdir().unwrap();
        std::fs::write(tree.path().join("C:foo"), b"x").unwrap();
        let err = create_torrent(CreateOptions::new(tree.path())).unwrap_err();
        assert!(err.to_string().contains("unsafe"), "{err}");

        #[cfg(unix)]
        {
            let slash = tempfile::tempdir().unwrap();
            std::fs::write(slash.path().join("foo\\bar"), b"x").unwrap();
            let err = create_torrent(CreateOptions::new(slash.path())).unwrap_err();
            assert!(err.to_string().contains("unsafe"), "{err}");
        }
    }

    #[test]
    fn create_torrent_file_excludes_itself_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("src");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"abc").unwrap();
        let out = root.join("out.torrent");
        std::fs::write(&out, b"stale torrent bytes").unwrap();

        let bytes =
            create_torrent_file(CreateOptions::new(&root).piece_length(16 * 1024), &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), bytes);
        let meta = from_bytes(&bytes).unwrap();
        let files = meta.torrent_file.info.files.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, ["a.txt"]);
        assert_eq!(meta.piece_hashes, hashes_of(b"abc", 16 * 1024));
        assert_info_round_trip(&bytes);

        let alongside = dir.path().join("src.torrent");
        create_torrent_file(
            CreateOptions::new(&root).piece_length(16 * 1024),
            &alongside,
        )
        .unwrap();
        assert!(alongside.is_file());
    }

    #[test]
    fn explicit_announce_is_not_replaced_by_the_tier_list() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"xyz").unwrap();
        let bytes = create_torrent(
            CreateOptions::new(&file)
                .piece_length(16 * 1024)
                .announce("http://primary.example/announce")
                .announce_tier(["http://backup.example/announce"]),
        )
        .unwrap();
        let meta = from_bytes(&bytes).unwrap();
        assert_eq!(
            meta.torrent_file.announce.as_deref(),
            Some("http://primary.example/announce")
        );
        assert_eq!(
            meta.torrent_file.announce_list.unwrap(),
            vec![vec!["http://backup.example/announce".to_string()]]
        );
    }
}
