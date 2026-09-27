//! IP blocklists (eMule `ipfilter.dat`, CIDR, and hyphen ranges).
//!
//! An empty path disables filtering. A file that fails to parse is logged
//! and the previous list is kept (empty on the first load).

use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tracing::{info, warn};

/// Access level above this is not blocked. qBittorrent / eMule treat `> 127`
/// as permitted.
const EMULE_BLOCK_MAX: u32 = 127;

const RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IpFilter {
    v4: Vec<(u32, u32)>,
    v6: Vec<(u128, u128)>,
}

impl IpFilter {
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        for (idx, raw) in text.lines().enumerate() {
            let line_no = idx + 1;
            let line = raw.trim().trim_end_matches('\r').trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            match parse_line(line).map_err(|message| ParseError {
                line: line_no,
                message,
            })? {
                None => {}
                Some(ParsedRange::V4(start, end)) => v4.push((start, end)),
                Some(ParsedRange::V6(start, end)) => v6.push((start, end)),
            }
        }
        Ok(Self {
            v4: merge_ranges(v4),
            v6: merge_ranges(v6),
        })
    }

    /// Lossy entry point for fuzzing. Invalid UTF-8 is an error, never a panic.
    pub fn parse_bytes(data: &[u8]) -> Result<Self, ParseError> {
        let text = std::str::from_utf8(data).map_err(|_| ParseError {
            line: 0,
            message: "input is not utf-8".to_string(),
        })?;
        Self::parse(text)
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => contains_range(&self.v4, u32::from(v4)),
            IpAddr::V6(v6) => contains_range(&self.v6, u128::from(v6)),
        }
    }

    pub fn range_count(&self) -> usize {
        self.v4.len() + self.v6.len()
    }

    pub fn is_empty(&self) -> bool {
        self.v4.is_empty() && self.v6.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedRange {
    V4(u32, u32),
    V6(u128, u128),
}

fn parse_line(line: &str) -> Result<Option<ParsedRange>, String> {
    let head = strip_inline_comment(line);
    if head.is_empty() {
        return Ok(None);
    }
    if let Some((addr, prefix)) = head.split_once('/') {
        if !prefix.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("bad prefix in {head}"));
        }
        return parse_cidr(addr.trim(), prefix.trim());
    }
    if let Some((start, end)) = split_hyphen(head) {
        return parse_bounds(start, end);
    }
    let ip = parse_ip(head).map_err(|_| format!("unrecognized filter line: {head}"))?;
    Ok(Some(single(ip)))
}

/// `#` starts a comment except inside an eMule description (after the level).
fn strip_inline_comment(line: &str) -> &str {
    if let Some(idx) = line.find('#') {
        let before = &line[..idx];
        // eMule: start - end , level , description. Keep the description.
        if before.matches(',').count() >= 2 {
            return line.trim();
        }
        return before.trim();
    }
    line.trim()
}

fn split_hyphen(line: &str) -> Option<(&str, &str)> {
    if let Some((left, right)) = line.split_once(" - ") {
        return Some((left.trim(), right.trim()));
    }
    let (left, right) = line.split_once('-')?;
    if left.is_empty() || right.is_empty() {
        return None;
    }
    Some((left.trim(), right.trim()))
}

fn parse_ip(text: &str) -> Result<IpAddr, String> {
    let text = text.trim();
    if text.contains(':') {
        return text.parse().map_err(|_| format!("bad address {text}"));
    }
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 4 {
        return Err(format!("bad address {text}"));
    }
    let mut octets = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("bad address {text}"));
        }
        octets[i] = part
            .parse::<u8>()
            .map_err(|_| format!("bad address {text}"))?;
    }
    Ok(IpAddr::V4(octets.into()))
}

fn parse_bounds(start_tok: &str, rest: &str) -> Result<Option<ParsedRange>, String> {
    let start = parse_ip(start_tok)?;
    let (end_tok, level) = match rest.split_once(',') {
        Some((end, after)) => {
            let level_tok = after.split(',').next().unwrap_or("").trim();
            (end.trim(), Some(level_tok))
        }
        None => {
            let end_tok = rest.split_whitespace().next().unwrap_or("");
            let extra = rest.split_whitespace().nth(1);
            if let Some(extra) = extra {
                if !extra.starts_with('#') {
                    return Err(format!("trailing junk after range: {extra}"));
                }
            }
            (end_tok, None)
        }
    };
    if let Some(level_tok) = level {
        if !level_tok.is_empty() {
            let level: u32 = level_tok
                .parse()
                .map_err(|_| format!("bad access level {level_tok}"))?;
            if level > EMULE_BLOCK_MAX {
                return Ok(None);
            }
        }
    }
    let end = parse_ip(end_tok)?;
    range_from_ips(start, end)
}

fn parse_cidr(addr: &str, prefix: &str) -> Result<Option<ParsedRange>, String> {
    let ip = parse_ip(addr)?;
    let bits: u32 = prefix
        .parse()
        .map_err(|_| format!("bad cidr prefix {prefix}"))?;
    match ip {
        IpAddr::V4(v4) => {
            if bits > 32 {
                return Err(format!("ipv4 prefix {bits} is out of range"));
            }
            let ip = u32::from(v4);
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            let start = ip & mask;
            let end = start | !mask;
            Ok(Some(ParsedRange::V4(start, end)))
        }
        IpAddr::V6(v6) => {
            if bits > 128 {
                return Err(format!("ipv6 prefix {bits} is out of range"));
            }
            let ip = u128::from(v6);
            let mask = if bits == 0 {
                0
            } else {
                u128::MAX << (128 - bits)
            };
            let start = ip & mask;
            let end = start | !mask;
            Ok(Some(ParsedRange::V6(start, end)))
        }
    }
}

fn single(ip: IpAddr) -> ParsedRange {
    match ip {
        IpAddr::V4(v4) => {
            let n = u32::from(v4);
            ParsedRange::V4(n, n)
        }
        IpAddr::V6(v6) => {
            let n = u128::from(v6);
            ParsedRange::V6(n, n)
        }
    }
}

fn range_from_ips(start: IpAddr, end: IpAddr) -> Result<Option<ParsedRange>, String> {
    match (start, end) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let a = u32::from(a);
            let b = u32::from(b);
            if a > b {
                return Err("end address is lower than start".to_string());
            }
            Ok(Some(ParsedRange::V4(a, b)))
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let a = u128::from(a);
            let b = u128::from(b);
            if a > b {
                return Err("end address is lower than start".to_string());
            }
            Ok(Some(ParsedRange::V6(a, b)))
        }
        _ => Err("start and end address families differ".to_string()),
    }
}

fn merge_ranges<T>(mut ranges: Vec<(T, T)>) -> Vec<(T, T)>
where
    T: Copy + Ord + SaturatingAdd,
{
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut out = Vec::with_capacity(ranges.len());
    let (mut start, mut end) = ranges[0];
    for (next_start, next_end) in ranges.into_iter().skip(1) {
        if next_start <= end.saturating_add_one() {
            if next_end > end {
                end = next_end;
            }
        } else {
            out.push((start, end));
            start = next_start;
            end = next_end;
        }
    }
    out.push((start, end));
    out
}

trait SaturatingAdd: Copy {
    fn saturating_add_one(self) -> Self;
}

impl SaturatingAdd for u32 {
    fn saturating_add_one(self) -> Self {
        self.saturating_add(1)
    }
}

impl SaturatingAdd for u128 {
    fn saturating_add_one(self) -> Self {
        self.saturating_add(1)
    }
}

fn contains_range<T: Ord>(ranges: &[(T, T)], ip: T) -> bool {
    let idx = ranges.partition_point(|range| range.0 <= ip);
    idx > 0 && ranges[idx - 1].1 >= ip
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IpFilterStats {
    pub ranges: usize,
    pub hits: u64,
    pub loaded_at: Option<SystemTime>,
}

struct FilterState {
    path: PathBuf,
    filter: IpFilter,
    hits: u64,
    loaded_at: Option<SystemTime>,
    mtime: Option<SystemTime>,
}

/// Shared blocklist. `blocks` increments `hits` on a match.
pub struct IpFilterHandle {
    enabled: AtomicBool,
    state: Mutex<FilterState>,
}

impl IpFilterHandle {
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self::open(PathBuf::new()))
    }

    /// Load `path`. An empty path is off. A missing or broken file logs and
    /// starts from an empty list.
    pub fn open(path: PathBuf) -> Self {
        let enabled = !path.as_os_str().is_empty();
        let mut state = FilterState {
            path,
            filter: IpFilter::default(),
            hits: 0,
            loaded_at: None,
            mtime: None,
        };
        if enabled {
            apply_load(&mut state, true);
        }
        Self {
            enabled: AtomicBool::new(enabled),
            state: Mutex::new(state),
        }
    }

    pub fn blocks(&self, addr: SocketAddr) -> bool {
        if !self.enabled.load(Ordering::Relaxed) {
            return false;
        }
        let mut state = self.lock();
        if state.filter.contains(addr.ip()) {
            state.hits = state.hits.saturating_add(1);
            debug_block(addr);
            true
        } else {
            false
        }
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        if !self.enabled.load(Ordering::Relaxed) {
            return false;
        }
        self.lock().filter.contains(ip)
    }

    pub fn stats(&self) -> IpFilterStats {
        let state = self.lock();
        IpFilterStats {
            ranges: state.filter.range_count(),
            hits: state.hits,
            loaded_at: state.loaded_at,
        }
    }

    /// Re-read the configured file. A broken file keeps the previous list.
    pub fn reload(&self) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        apply_load(&mut self.lock(), false);
    }

    /// Reload when the file mtime changes. Missing mtime is left alone.
    pub fn poll_mtime(&self) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let mut state = self.lock();
        let Ok(meta) = fs::metadata(&state.path) else {
            return;
        };
        let Ok(mtime) = meta.modified() else {
            return;
        };
        if state.mtime == Some(mtime) {
            return;
        }
        apply_load(&mut state, false);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FilterState> {
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }
}

fn debug_block(addr: SocketAddr) {
    tracing::debug!(%addr, "ip filter blocked peer");
}

fn apply_load(state: &mut FilterState, first: bool) {
    let path = state.path.clone();
    match load_file(&path) {
        Ok((filter, mtime)) => {
            let ranges = filter.range_count();
            state.filter = filter;
            state.loaded_at = Some(SystemTime::now());
            state.mtime = mtime;
            info!(path = %path.display(), ranges, "loaded ip filter");
        }
        Err(err) => {
            if first {
                state.filter = IpFilter::default();
                state.loaded_at = None;
                state.mtime = None;
            }
            warn!(
                path = %path.display(),
                error = %err,
                "ip filter load failed, keeping the previous list"
            );
        }
    }
}

fn load_file(path: &Path) -> Result<(IpFilter, Option<SystemTime>), String> {
    let bytes = fs::read(path).map_err(|err| err.to_string())?;
    let mtime = fs::metadata(path).and_then(|meta| meta.modified()).ok();
    let text = String::from_utf8(bytes).map_err(|_| "ip filter is not utf-8".to_string())?;
    let filter = IpFilter::parse(&text).map_err(|err| err.to_string())?;
    Ok((filter, mtime))
}

pub fn reload_interval() -> std::time::Duration {
    RELOAD_INTERVAL
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    fn v4(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn fixture_dat_and_cidr_match_inside_and_outside() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let dat = fs::read_to_string(root.join("ipfilter.dat")).unwrap();
        let cidr = fs::read_to_string(root.join("ipfilter.cidr")).unwrap();
        let filter = IpFilter::parse(&dat).unwrap();
        let cidr_filter = IpFilter::parse(&cidr).unwrap();

        assert!(filter.contains(v4("1.2.3.5")));
        assert!(filter.contains(v4("1.2.3.10")));
        assert!(!filter.contains(v4("1.2.3.11")));
        assert!(!filter.contains(v4("8.8.8.8")));
        assert!(filter.contains("2001:db8::1".parse().unwrap()));
        assert!(!filter.contains("2001:db8::2".parse().unwrap()));
        // level 200 is permitted, so 9.9.9.9 is not blocked.
        assert!(!filter.contains(v4("9.9.9.9")));

        assert!(cidr_filter.contains(v4("1.2.3.255")));
        assert!(!cidr_filter.contains(v4("1.2.4.0")));
        assert!(cidr_filter.contains("2001:db8::ffff".parse().unwrap()));
        assert!(!cidr_filter.contains("2001:db9::1".parse().unwrap()));
    }

    #[test]
    fn merges_overlapping_and_adjacent_ranges() {
        let filter =
            IpFilter::parse("10.0.0.0-10.0.0.10\n10.0.0.8-10.0.0.20\n10.0.0.21\n").unwrap();
        assert_eq!(filter.range_count(), 1);
        assert!(filter.contains(v4("10.0.0.0")));
        assert!(filter.contains(v4("10.0.0.21")));
        assert!(!filter.contains(v4("10.0.0.22")));
    }

    #[test]
    fn crlf_and_comments_are_tolerated() {
        let text = "# comment\r\n\r\n1.2.3.0/30\r\n";
        let filter = IpFilter::parse(text).unwrap();
        assert!(filter.contains(v4("1.2.3.1")));
        assert!(!filter.contains(v4("1.2.3.4")));
    }

    #[test]
    fn bad_line_fails_the_parse() {
        let err = IpFilter::parse("1.2.3.4\nnot-an-ip\n").unwrap_err();
        assert_eq!(err.line, 2);
    }

    #[test]
    fn disabled_path_is_a_noop() {
        let handle = IpFilterHandle::open(PathBuf::new());
        let addr: SocketAddr = "1.2.3.4:6881".parse().unwrap();
        assert!(!handle.blocks(addr));
        handle.reload();
        handle.poll_mtime();
        let stats = handle.stats();
        assert_eq!(stats.ranges, 0);
        assert_eq!(stats.hits, 0);
        assert!(stats.loaded_at.is_none());
    }

    #[test]
    fn bad_file_keeps_the_previous_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipfilter.dat");
        fs::write(&path, "10.0.0.0 - 10.0.0.255 , 000 , block\n").unwrap();
        let handle = IpFilterHandle::open(path.clone());
        assert!(handle.contains(v4("10.0.0.1")));
        let loaded = handle.stats().ranges;
        assert_eq!(loaded, 1);

        fs::write(&path, "this is not a filter\n").unwrap();
        handle.reload();
        assert!(handle.contains(v4("10.0.0.1")));
        assert_eq!(handle.stats().ranges, 1);
    }

    #[test]
    fn hot_reload_picks_up_a_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipfilter.cidr");
        fs::write(&path, "10.0.0.0/8\n").unwrap();
        let handle = IpFilterHandle::open(path.clone());
        assert!(handle.contains(v4("10.1.2.3")));

        fs::write(&path, "11.0.0.0/8\n").unwrap();
        bump_mtime(path.as_path());
        handle.poll_mtime();
        assert!(!handle.contains(v4("10.1.2.3")));
        assert!(handle.contains(v4("11.1.2.3")));
        assert!(handle.stats().loaded_at.is_some());
    }

    fn bump_mtime(path: &Path) {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes().to_vec()).unwrap();
        let later = SystemTime::now() + Duration::from_secs(5);
        let secs = later
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as libc::time_t;
        let times = [
            libc::timeval {
                tv_sec: secs,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: secs,
                tv_usec: 0,
            },
        ];
        assert_eq!(unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) }, 0);
    }

    struct ReadBomb {
        touched: Arc<AtomicBool>,
    }

    impl AsyncRead for ReadBomb {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            self.touched.store(true, Ordering::SeqCst);
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for ReadBomb {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn incoming_blocked_address_is_dropped_before_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipfilter.dat");
        fs::write(&path, "127.0.0.2 - 127.0.0.2 , 000 , loopback alias\n").unwrap();
        let handle = Arc::new(IpFilterHandle::open(path));
        let touched = Arc::new(AtomicBool::new(false));
        let stream = crate::transport::boxed_stream(ReadBomb {
            touched: touched.clone(),
        });
        let addr: SocketAddr = "127.0.0.2:40000".parse().unwrap();
        let ctx = crate::session::IncomingPeerContext {
            peer_id: [0; 20],
            torrents: Arc::new(dashmap::DashMap::new()),
            global_peers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_peers_per_torrent: 1,
            max_peers_global: 1,
            extensions: crate::extension::ExtensionRegistry::new(),
            listen_port: 0,
            connector: Arc::new(crate::transport::TcpConnector::new()),
            pending: Arc::new(dashmap::DashMap::new()),
            dht: None,
            encryption: crate::mse::EncryptionPolicy::Disabled,
            add_peers: crate::extension::noop_add_peers(),
            ip_filter: handle.clone(),
        };
        crate::session::accept_incoming(stream, addr, ctx).await;
        assert!(
            !touched.load(Ordering::SeqCst),
            "blocked peer reached the handshake read"
        );
        assert_eq!(handle.stats().hits, 1);
    }
}
