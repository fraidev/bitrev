//! HTTP web seeds (BEP-0019 GetRight `url-list`, BEP-0017 `httpseeds`).
//!
//! One task per URL. Pieces are reserved with [`TorrentDownloadedState::pick`]
//! and committed through the same chunk, hash, and `FullPiece` path as peers.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::bitfield::Bitfield;
use crate::file::{self, TorrentMeta};
use crate::peer::PeerAddr;
use crate::peer_connection::{FullPiece, TorrentDownloadedState};
use crate::peer_state::PeerStates;
use crate::rate::BandwidthLimiters;
use crate::session::DownloadState;
use crate::storage::Storage;
use crate::torrent::Torrent;
use crate::tracker;

const PIPELINE: usize = 4;
const HASH_FAIL_LIMIT: u32 = 3;
const CLIENT_ERR_LIMIT: u32 = 3;
const FAST_SWARM_BYTES: u64 = 8 * 1024;
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WebSeedKind {
    /// BEP-0019 byte ranges against the file URL.
    GetRight,
    /// BEP-0017 `?info_hash=&piece=&ranges=`.
    Hoffman,
}

#[derive(Clone)]
pub struct WebSeedCtx {
    pub torrent: Arc<Torrent>,
    pub meta: TorrentMeta,
    pub downloaded: Arc<TorrentDownloadedState>,
    pub storage: Arc<Storage>,
    pub piece_tx: flume::Sender<FullPiece>,
    pub peer_states: Arc<PeerStates>,
    pub download_state: Arc<Mutex<DownloadState>>,
    pub limits: BandwidthLimiters,
    pub cancel: CancellationToken,
}

pub fn spawn(ctx: WebSeedCtx) {
    for (index, (kind, url)) in collect_urls(&ctx.meta).into_iter().enumerate() {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            run_url(ctx, kind, url, index as u16).await;
        });
    }
}

fn collect_urls(meta: &TorrentMeta) -> Vec<(WebSeedKind, String)> {
    let mut urls = Vec::new();
    if let Some(list) = &meta.torrent_file.url_list {
        for url in list {
            if is_http(url) {
                urls.push((WebSeedKind::GetRight, url.clone()));
            }
        }
    }
    if let Some(list) = &meta.torrent_file.httpseeds {
        for url in list {
            if is_http(url) {
                urls.push((WebSeedKind::Hoffman, url.clone()));
            }
        }
    }
    urls
}

fn is_http(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn webseed_peer(index: u16) -> PeerAddr {
    SocketAddr::from((Ipv4Addr::new(192, 0, 2, 1), index.saturating_add(1)))
}

#[derive(Debug)]
enum JobResult {
    Ok,
    HashFail,
    RetryAfter(Duration),
    Server,
    Client,
    SkipUrl,
}

async fn run_url(ctx: WebSeedCtx, kind: WebSeedKind, url: String, slot: u16) {
    let peer = webseed_peer(slot);
    let client = tracker::http_client();
    let mut inflight: JoinSet<JobResult> = JoinSet::new();
    let mut hash_fails = 0u32;
    let mut client_fails = 0u32;
    let mut backoff = INITIAL_BACKOFF;
    let mut pause_until = tokio::time::Instant::now();

    loop {
        if ctx.cancel.is_cancelled() || ctx.downloaded.is_complete() {
            break;
        }
        if *ctx.download_state.lock().unwrap() != DownloadState::Downloading {
            if !wait_tick(&ctx.cancel, Duration::from_millis(250)).await {
                break;
            }
            continue;
        }
        let now = tokio::time::Instant::now();
        if pause_until > now {
            tokio::select! {
                _ = ctx.cancel.cancelled() => break,
                _ = tokio::time::sleep_until(pause_until) => {}
                Some(joined) = inflight.join_next() => {
                    if apply_result(
                        joined,
                        &mut hash_fails,
                        &mut client_fails,
                        &mut backoff,
                        &mut pause_until,
                    ) {
                        break;
                    }
                }
            }
            continue;
        }

        while inflight.len() < PIPELINE {
            let Some(index) = pick_piece(&ctx, peer) else {
                break;
            };
            let ctx = ctx.clone();
            let url = url.clone();
            let client = client.clone();
            inflight
                .spawn(async move { fetch_reserved(ctx, client, kind, url, peer, index).await });
        }

        if inflight.is_empty() {
            tokio::select! {
                _ = ctx.cancel.cancelled() => break,
                _ = ctx.downloaded.piece_notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
            continue;
        }

        let Some(joined) = inflight.join_next().await else {
            break;
        };
        if apply_result(
            joined,
            &mut hash_fails,
            &mut client_fails,
            &mut backoff,
            &mut pause_until,
        ) {
            break;
        }
    }

    inflight.abort_all();
    ctx.downloaded.remove_reserved(peer);
}

fn apply_result(
    joined: Result<JobResult, tokio::task::JoinError>,
    hash_fails: &mut u32,
    client_fails: &mut u32,
    backoff: &mut Duration,
    pause_until: &mut tokio::time::Instant,
) -> bool {
    let result = match joined {
        Ok(result) => result,
        Err(_) => JobResult::Server,
    };
    match result {
        JobResult::Ok => {
            *client_fails = 0;
            *backoff = INITIAL_BACKOFF;
            false
        }
        JobResult::HashFail => {
            *hash_fails = hash_fails.saturating_add(1);
            *hash_fails >= HASH_FAIL_LIMIT
        }
        JobResult::RetryAfter(delay) => {
            *client_fails = 0;
            *pause_until = tokio::time::Instant::now() + delay;
            false
        }
        JobResult::Server => {
            *pause_until = tokio::time::Instant::now() + *backoff;
            *backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
            false
        }
        JobResult::Client => {
            *client_fails = client_fails.saturating_add(1);
            *client_fails >= CLIENT_ERR_LIMIT
        }
        JobResult::SkipUrl => true,
    }
}

async fn wait_tick(cancel: &CancellationToken, delay: Duration) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

fn pick_piece(ctx: &WebSeedCtx, peer: PeerAddr) -> Option<u32> {
    let has = pieces_for_webseed(&ctx.downloaded, &ctx.peer_states);
    ctx.downloaded.pick(peer, &has, &[])
}

fn pieces_for_webseed(downloaded: &TorrentDownloadedState, peers: &PeerStates) -> Bitfield {
    let count = downloaded.piece_count();
    let mut blocked = vec![false; count];
    for entry in peers.states.iter() {
        let state = entry.value();
        if state.writer_tx.is_none() || state.peer_choking || state.snubbed {
            continue;
        }
        if state.stats.bytes_downloaded.load(Ordering::Relaxed) < FAST_SWARM_BYTES {
            continue;
        }
        for (index, slot) in blocked.iter_mut().enumerate() {
            if state.bitfield.has_piece(index) {
                *slot = true;
            }
        }
    }
    let mut has = Bitfield::with_piece_count(count);
    for (index, blocked) in blocked.iter().enumerate() {
        if !blocked {
            has.set_piece(index);
        }
    }
    has
}

async fn fetch_reserved(
    ctx: WebSeedCtx,
    client: reqwest::Client,
    kind: WebSeedKind,
    url: String,
    peer: PeerAddr,
    index: u32,
) -> JobResult {
    let result = fetch_piece(&ctx, &client, kind, &url, peer, index).await;
    match result {
        JobResult::Ok => {
            ctx.downloaded.release_reservation(index, peer);
        }
        JobResult::HashFail => {
            ctx.downloaded.remove_downloaded(index);
            ctx.downloaded.release_piece(index);
        }
        _ => {
            ctx.downloaded.release_piece(index);
        }
    }
    result
}

async fn fetch_piece(
    ctx: &WebSeedCtx,
    client: &reqwest::Client,
    kind: WebSeedKind,
    url: &str,
    peer: PeerAddr,
    index: u32,
) -> JobResult {
    let Some(length) = ctx.downloaded.piece_length(index) else {
        return JobResult::Client;
    };
    if length == 0 {
        return JobResult::Ok;
    }
    ctx.limits.acquire_download(u64::from(length)).await;
    if ctx.cancel.is_cancelled() || !ctx.downloaded.wanted(index) {
        return JobResult::Ok;
    }

    let fetched = match kind {
        WebSeedKind::GetRight => fetch_getright(client, &ctx.torrent, url, index).await,
        WebSeedKind::Hoffman => fetch_hoffman(client, &ctx.torrent, url, index, length).await,
    };
    let buf = match fetched {
        Ok(buf) => buf,
        Err(err) => return err,
    };
    if buf.len() != length as usize {
        return JobResult::Server;
    }

    ctx.downloaded.set_chuncks(index, 0, buf, peer);
    let (expected, raw) = {
        let Some(full) = ctx.downloaded.set_downloaded_if_all_chunks(index) else {
            return JobResult::Ok;
        };
        (full.piece_work.hash, full.chunk_to_buf())
    };
    let ok = crate::utils::check_integrity(&expected, &raw);
    if !ok {
        debug!(index, url, "web seed piece failed hash");
        return JobResult::HashFail;
    }
    if ctx.downloaded.claim_write(index) {
        ctx.downloaded.clear_piece_chunks(index);
        let piece = FullPiece {
            index,
            length,
            buf: raw,
        };
        if ctx.piece_tx.send(piece).is_err() {
            return JobResult::SkipUrl;
        }
    }
    JobResult::Ok
}

async fn fetch_getright(
    client: &reqwest::Client,
    torrent: &Torrent,
    base: &str,
    index: u32,
) -> Result<Vec<u8>, JobResult> {
    let mappings = crate::utils::map_piece_to_files(torrent, index as usize);
    let mut buf = Vec::new();
    for mapping in mappings {
        if mapping.length == 0 {
            continue;
        }
        let file_url = getright_file_url(base, torrent, mapping.file_index);
        let start = mapping.file_offset;
        let end = start + mapping.length - 1;
        let chunk = http_range(client, &file_url, start, end, mapping.length, true).await?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

async fn fetch_hoffman(
    client: &reqwest::Client,
    torrent: &Torrent,
    base: &str,
    index: u32,
    length: u32,
) -> Result<Vec<u8>, JobResult> {
    let url = hoffman_url(base, &torrent.info_hash, index, length);
    http_range(client, &url, 0, 0, length as usize, false).await
}

async fn http_range(
    client: &reqwest::Client,
    url: &str,
    start: usize,
    end: usize,
    want: usize,
    ranged: bool,
) -> Result<Vec<u8>, JobResult> {
    let mut request = client.get(url);
    if ranged {
        request = request.header("Range", format!("bytes={start}-{end}"));
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            debug!(url, error = %err, "web seed request failed");
            return Err(JobResult::Server);
        }
    };
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.bytes().await.unwrap_or_default();
    let code = status.as_u16();

    if code == 206 {
        return exact_len(&body, want);
    }
    if code == 200 {
        if body.len() == want {
            return Ok(body.to_vec());
        }
        if ranged {
            debug!(url, bytes = body.len(), want, "web seed ignored range");
            return Err(JobResult::SkipUrl);
        }
        return Err(JobResult::Server);
    }
    if code == 429 || (500..600).contains(&code) {
        let delay = retry_after(&headers, &body).unwrap_or(INITIAL_BACKOFF);
        debug!(url, code, ?delay, "web seed backing off");
        return Err(JobResult::RetryAfter(delay));
    }
    if (400..500).contains(&code) {
        debug!(url, code, "web seed client error");
        return Err(JobResult::Client);
    }
    debug!(url, code, "web seed unexpected status");
    Err(JobResult::Server)
}

fn exact_len(body: &[u8], want: usize) -> Result<Vec<u8>, JobResult> {
    if body.len() == want {
        Ok(body.to_vec())
    } else {
        Err(JobResult::Server)
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap, body: &[u8]) -> Option<Duration> {
    if let Some(value) = headers.get(reqwest::header::RETRY_AFTER) {
        if let Ok(text) = value.to_str() {
            if let Some(delay) = parse_retry_secs(text) {
                return Some(delay);
            }
        }
    }
    parse_retry_secs(std::str::from_utf8(body).ok()?.trim())
}

fn parse_retry_secs(text: &str) -> Option<Duration> {
    let secs = text.parse::<u64>().ok()?;
    Some(Duration::from_secs(secs))
}

pub fn getright_file_url(base: &str, torrent: &Torrent, file_index: usize) -> String {
    let single = torrent.files.len() == 1;
    let (path, query) = split_query(base);
    if single && !path.ends_with('/') {
        return base.to_string();
    }
    let mut segments = Vec::new();
    if path.ends_with('/') || !single {
        segments.push(torrent.name.clone());
    }
    if !single {
        if let Some(file) = torrent.files.get(file_index) {
            segments.extend(file.path.iter().cloned());
        }
    }
    join_segments(path, query, &segments)
}

pub fn hoffman_url(base: &str, info_hash: &[u8; 20], piece: u32, length: u32) -> String {
    let mut url = base.to_string();
    push_query(&mut url, "info_hash", &file::url_encode_bytes(info_hash));
    push_query(&mut url, "piece", &piece.to_string());
    let end = length.saturating_sub(1);
    push_query(&mut url, "ranges", &format!("0-{end}"));
    url
}

fn split_query(url: &str) -> (&str, Option<&str>) {
    match url.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (url, None),
    }
}

fn join_segments(path: &str, query: Option<&str>, segments: &[String]) -> String {
    let mut url = path.to_string();
    if !segments.is_empty() {
        if !url.ends_with('/') {
            url.push('/');
        }
        for (index, segment) in segments.iter().enumerate() {
            if index > 0 {
                url.push('/');
            }
            url.push_str(&file::url_encode_bytes(segment.as_bytes()));
        }
    }
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    url
}

fn push_query(url: &mut String, key: &str, value: &str) {
    let sep = if url.contains('?') { '&' } else { '?' };
    url.push(sep);
    url.push_str(key);
    url.push('=');
    url.push_str(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::TorrentFileInfo;

    fn sample_torrent(files: Vec<TorrentFileInfo>) -> Torrent {
        let length = files.iter().map(|f| f.length).sum();
        Torrent {
            info_hash: [0xAB; 20],
            piece_hashes: vec![[0; 20]],
            piece_length: 32,
            length,
            files,
            name: "michael".into(),
            private: true,
        }
    }

    fn single() -> Torrent {
        sample_torrent(vec![TorrentFileInfo {
            path: vec!["michael".into()],
            length: 32,
            offset: 0,
        }])
    }

    fn multi() -> Torrent {
        sample_torrent(vec![
            TorrentFileInfo {
                path: vec!["Readme.txt".into()],
                length: 10,
                offset: 0,
            },
            TorrentFileInfo {
                path: vec!["dir".into(), "a.bin".into()],
                length: 22,
                offset: 10,
            },
        ])
    }

    #[test]
    fn single_file_url_is_used_as_is() {
        let torrent = single();
        assert_eq!(
            getright_file_url("http://mirror.com/file.exe", &torrent, 0),
            "http://mirror.com/file.exe"
        );
    }

    #[test]
    fn slash_url_appends_name() {
        let torrent = single();
        assert_eq!(
            getright_file_url("http://mirror.com/pub/", &torrent, 0),
            "http://mirror.com/pub/michael"
        );
    }

    #[test]
    fn multi_file_appends_name_and_path() {
        let torrent = multi();
        assert_eq!(
            getright_file_url("http://mirror.com/pub/", &torrent, 0),
            "http://mirror.com/pub/michael/Readme.txt"
        );
        assert_eq!(
            getright_file_url("http://mirror.com/pub", &torrent, 1),
            "http://mirror.com/pub/michael/dir/a.bin"
        );
    }

    #[test]
    fn hoffman_query_encodes_info_hash_and_ranges() {
        let url = hoffman_url("http://www.whatever.com/seed.php", &[0xAB; 20], 3, 16);
        assert!(url.starts_with("http://www.whatever.com/seed.php?"));
        assert!(url.contains("piece=3"));
        assert!(url.contains("ranges=0-15"));
        assert!(url.contains(&format!(
            "info_hash={}",
            file::url_encode_bytes(&[0xAB; 20])
        )));
    }

    #[tokio::test(start_paused = true, flavor = "current_thread")]
    async fn retry_after_is_honored_under_pause() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("5"),
        );
        let delay = retry_after(&headers, b"").expect("retry-after");
        assert_eq!(delay, Duration::from_secs(5));
        assert_eq!(
            retry_after(&reqwest::header::HeaderMap::new(), b"12"),
            Some(Duration::from_secs(12))
        );

        let mut hash_fails = 0;
        let mut client_fails = 0;
        let mut backoff = INITIAL_BACKOFF;
        let mut pause_until = tokio::time::Instant::now();
        let stop = apply_result(
            Ok(JobResult::RetryAfter(delay)),
            &mut hash_fails,
            &mut client_fails,
            &mut backoff,
            &mut pause_until,
        );
        assert!(!stop);
        let started = tokio::time::Instant::now();
        tokio::time::sleep_until(pause_until).await;
        assert!(started.elapsed() >= Duration::from_secs(5));
    }

    #[tokio::test]
    async fn retry_after_header_on_429() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let _ = read_headers(&mut socket).await;
            let body = b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 5\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, body).await;
        });
        let client = tracker::build_http_client();
        let url = format!("http://{addr}/file");
        let err = http_range(&client, &url, 0, 3, 4, true).await.unwrap_err();
        assert!(matches!(err, JobResult::RetryAfter(d) if d == Duration::from_secs(5)));
    }

    async fn read_headers(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if socket.read(&mut byte).await.ok() != Some(1) {
                break;
            }
            buf.push(byte[0]);
            if buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        buf
    }
}
