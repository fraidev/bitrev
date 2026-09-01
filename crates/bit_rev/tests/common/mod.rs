//! Shared integration-test harness. Binds only to 127.0.0.1.
//!
//! The types here are a reusable fixture API. Individual scenarios only
//! touch a subset, so unused items are expected.
#![allow(dead_code)]

pub mod fixture;
pub mod http_tracker;
pub mod seeder;
pub mod udp_tracker;

use std::path::{Path, PathBuf};
use std::time::Duration;

use bit_rev::file::TorrentMeta;
use bit_rev::session::{AddTorrentOptions, AddTorrentResult, PieceResult, Session, SessionOptions};
use bit_rev::torrent::Torrent;
use tempfile::TempDir;

pub use fixture::{FileSpec, TorrentFixture};
#[allow(unused_imports)]
pub use http_tracker::{HttpAnnounceBody, MockHttpTracker, RecordedHttpRequest};
pub use seeder::{SeederConfig, SeederPeer};
#[allow(unused_imports)]
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
    already_have: usize,
    timeout: Duration,
) {
    let needed = torrent.piece_hashes.len().saturating_sub(already_have);
    let deadline = tokio::time::Instant::now() + timeout;
    for i in 0..needed {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::time::timeout(remaining, pr_rx.recv_async())
            .await
            .unwrap_or_else(|_| panic!("piece {} / {needed} timed out", i + 1))
            .expect("piece channel closed");
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

/// Best-effort peak RSS. `None` when the platform helper is unavailable.
pub fn peak_rss_bytes() -> Option<u64> {
    let pid = std::process::id();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let kb: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    Some(kb.saturating_mul(1024))
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
