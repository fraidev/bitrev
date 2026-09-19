//! Shared test and bench harness. Binds only to 127.0.0.1.
//!
//! Integration tests re-export this crate from `tests/common/mod.rs`. Benches
//! and examples depend on it directly; `tests/` modules are not importable
//! from those targets.

#![allow(dead_code)]

pub mod fixture;
pub mod http_tracker;
pub mod seeder;
pub mod udp_tracker;

use std::path::{Path, PathBuf};
use std::time::Duration;

use bit_rev::file::TorrentMeta;
use bit_rev::mse::EncryptionPolicy;
use bit_rev::session::{AddTorrentOptions, AddTorrentResult, PieceResult, Session, SessionOptions};
use bit_rev::torrent::Torrent;
use tempfile::TempDir;

pub use fixture::{FileSpec, PersistedFixture, TorrentFixture};

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}
pub use http_tracker::{HttpAnnounceBody, MockHttpTracker, RecordedHttpRequest};
pub use seeder::{SeederConfig, SeederPeer};
pub use udp_tracker::{MockUdpTracker, RecordedUdpAnnounce, UdpAnnounceBody};

pub const DEFAULT_PIECE_LENGTH: u32 = 32 * 1024;
pub const BLOCK_SIZE: u32 = 16 * 1024;
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(45);
pub const LISTEN_TIMEOUT: Duration = Duration::from_secs(2);

pub fn unique_temp_dir() -> TempDir {
    tempfile::Builder::new()
        .prefix("bitrev-it-")
        .tempdir()
        .expect("temp dir")
}

pub async fn test_session(state_dir: Option<PathBuf>) -> Session {
    let session = Session::with_options(SessionOptions {
        listen_port: 0,
        state_dir,
        encryption: EncryptionPolicy::Disabled,
        ..SessionOptions::default()
    });
    tokio::time::timeout(LISTEN_TIMEOUT, session.wait_listening())
        .await
        .expect("session listen timeout");
    session
}

pub async fn add_download(
    session: &Session,
    meta: TorrentMeta,
    output: impl Into<PathBuf>,
) -> AddTorrentResult {
    session
        .add_torrent(AddTorrentOptions::from(meta).output_dir(output))
        .await
        .expect("add torrent")
}

pub async fn wait_for_completion(
    pr_rx: &flume::Receiver<PieceResult>,
    torrent: &Torrent,
    already_have: &[PieceResult],
    timeout: Duration,
) {
    let total = torrent.piece_hashes.len();
    let mut seen = vec![false; total];
    for pr in already_have {
        if let Some(slot) = seen.get_mut(pr.index as usize) {
            *slot = true;
        }
    }
    let deadline = tokio::time::Instant::now() + timeout;
    while seen.iter().any(|have| !have) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let pr = tokio::time::timeout(remaining, pr_rx.recv_async())
            .await
            .unwrap_or_else(|_| {
                let have: Vec<u32> = seen
                    .iter()
                    .enumerate()
                    .filter(|(_, have)| **have)
                    .map(|(i, _)| i as u32)
                    .collect();
                panic!(
                    "waiting for pieces timed out ({}/ {total}, have {have:?})",
                    have.len()
                )
            })
            .expect("piece channel closed");
        if let Some(slot) = seen.get_mut(pr.index as usize) {
            *slot = true;
        }
    }
}

pub fn sha1_bytes(data: &[u8]) -> [u8; 20] {
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(data);
    hasher.digest().bytes()
}

pub fn sha1_file(path: &Path) -> [u8; 20] {
    let mut hasher = sha1_smol::Sha1::new();
    let mut file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {path:?}: {e}"));
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf).expect("read");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    hasher.digest().bytes()
}

/// Snapshot of `getrusage(RUSAGE_SELF)`. All fields are `None` off Unix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceUsage {
    pub peak_rss_bytes: Option<u64>,
    pub user_secs: Option<f64>,
    pub sys_secs: Option<f64>,
}

impl ResourceUsage {
    pub fn unavailable() -> Self {
        Self {
            peak_rss_bytes: None,
            user_secs: None,
            sys_secs: None,
        }
    }
}

/// Process-lifetime peak RSS, user CPU, and sys CPU via `libc::getrusage`.
///
/// `ru_maxrss` is bytes on macOS and kilobytes on Linux. This helper always
/// returns bytes. Replaces the earlier `ps` shim so benches do not shell out.
pub fn resource_usage() -> ResourceUsage {
    resource_usage_impl()
}

/// Best-effort peak RSS. `None` when the platform helper is unavailable.
pub fn peak_rss_bytes() -> Option<u64> {
    resource_usage().peak_rss_bytes
}

#[cfg(unix)]
fn resource_usage_impl() -> ResourceUsage {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return ResourceUsage::unavailable();
    }
    let usage = unsafe { usage.assume_init() };
    ResourceUsage {
        peak_rss_bytes: Some(maxrss_to_bytes(usage.ru_maxrss)),
        user_secs: Some(timeval_secs(usage.ru_utime)),
        sys_secs: Some(timeval_secs(usage.ru_stime)),
    }
}

#[cfg(not(unix))]
fn resource_usage_impl() -> ResourceUsage {
    ResourceUsage::unavailable()
}

/// macOS reports `ru_maxrss` in bytes. Linux (and other Unix) report kilobytes.
#[cfg(unix)]
fn maxrss_to_bytes(maxrss: i64) -> u64 {
    let raw = maxrss.max(0) as u64;
    if cfg!(any(target_os = "macos", target_os = "ios")) {
        raw
    } else {
        raw.saturating_mul(1024)
    }
}

#[cfg(unix)]
fn timeval_secs(tv: libc::timeval) -> f64 {
    tv.tv_sec as f64 + (tv.tv_usec as f64) / 1_000_000.0
}

pub fn compact_peers(addrs: &[std::net::SocketAddr]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(addrs.len() * 6);
    for addr in addrs {
        match addr {
            std::net::SocketAddr::V4(v4) => {
                buf.extend_from_slice(&v4.ip().octets());
                buf.extend_from_slice(&v4.port().to_be_bytes());
            }
            std::net::SocketAddr::V6(_) => panic!("compact v4 helper got IPv6 {addr}"),
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    #[test]
    fn peak_rss_available_on_unix() {
        #[cfg(unix)]
        assert!(crate::peak_rss_bytes().is_some_and(|n| n > 0));
        #[cfg(not(unix))]
        assert!(crate::peak_rss_bytes().is_none());
    }

    #[test]
    fn hex_encode_lower_nibble() {
        assert_eq!(crate::hex_encode(&[0x00, 0xab, 0xff]), "00abff");
    }

    #[test]
    fn private_fixture_round_trip_and_persist() {
        let fixture = crate::TorrentFixture::builder()
            .single_file("payload.bin", 32 * 1024)
            .piece_length(16 * 1024)
            .announce("http://tracker:6969/announce")
            .private(true)
            .build();
        assert!(fixture.torrent_meta.torrent_file.info.is_private());
        assert!(!fixture.torrent().allows_dht());
        assert!(!fixture.torrent().allows_pex());

        let dir = crate::unique_temp_dir();
        let persisted = fixture.persist_to(dir.path());
        assert!(persisted.torrent_path.is_file());
        assert!(persisted.data_dir.join("payload.bin").is_file());
        assert_eq!(persisted.sha1_hex, fixture.payload_sha1_hex());
        assert_eq!(
            crate::hex_encode(&crate::sha1_file(&persisted.data_dir.join("payload.bin"))),
            persisted.sha1_hex
        );
        let reloaded = bit_rev::file::from_filename(persisted.torrent_path.to_str().unwrap())
            .expect("reload persisted torrent");
        assert!(reloaded.torrent_file.info.is_private());
        assert_eq!(
            crate::hex_encode(&reloaded.info_hash),
            persisted.info_hash_hex
        );
    }
}
