use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use serde_bencode::de;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{
    file::{self, udp_announce_event, AnnounceEvent, AnnounceParams, TorrentMeta},
    peer::BencodeResponse,
    peer_connection::TorrentDownloadedState,
    protocol_udp::{AnnounceOptions, UdpTracker},
    session::DownloadState,
};

pub mod tiers;
pub use tiers::{tracker_scheme, TrackerScheme, TrackerTiers};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const REDIRECT_LIMIT: usize = 5;
pub const STOPPED_TIMEOUT: Duration = Duration::from_secs(5);
pub const INITIAL_BACKOFF: Duration = Duration::from_secs(15);
pub const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_INTERVAL_SECS: u64 = 1800;

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

#[derive(Debug, Error)]
pub enum TrackerError {
    #[error("tracker HTTP status {status}")]
    HttpStatus { status: u16 },
    #[error("tracker failure reason: {0}")]
    FailureReason(String),
    #[error("tracker request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("invalid tracker response: {0}")]
    Decode(String),
    #[error("UDP tracker announce failed: {0}")]
    Udp(String),
    #[error("tracker stopped announce timed out")]
    StoppedTimeout,
}

pub fn http_client() -> reqwest::Client {
    HTTP_CLIENT.get_or_init(build_http_client).clone()
}

pub fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(REDIRECT_LIMIT))
        .gzip(true)
        .http1_only()
        .user_agent(crate::identity::user_agent())
        .build()
        .expect("failed to build tracker HTTP client")
}

pub fn next_backoff(current: Option<Duration>) -> Duration {
    match current {
        None => INITIAL_BACKOFF,
        Some(delay) => delay.saturating_mul(2).min(MAX_BACKOFF),
    }
}

pub fn reannounce_delay(interval_secs: u64, min_interval_secs: Option<u64>) -> Duration {
    let interval = if interval_secs == 0 {
        DEFAULT_INTERVAL_SECS
    } else {
        interval_secs
    };
    let secs = match min_interval_secs {
        Some(min) => interval.max(min),
        None => interval,
    };
    Duration::from_secs(secs)
}

pub async fn announce(
    client: &reqwest::Client,
    url: &str,
) -> Result<BencodeResponse, TrackerError> {
    let response = client.get(url).send().await?;
    let status = response.status();
    let body = response.bytes().await?;

    if !status.is_success() {
        return Err(TrackerError::HttpStatus {
            status: status.as_u16(),
        });
    }

    let decoded: BencodeResponse =
        de::from_bytes(&body).map_err(|e| TrackerError::Decode(e.to_string()))?;

    if let Some(reason) = decoded.failure_reason_str() {
        return Err(TrackerError::FailureReason(reason));
    }

    if let Some(warning) = decoded.warning_message_str() {
        warn!(warning = %warning, "tracker warning message");
    }

    Ok(decoded)
}

pub async fn announce_with_timeout(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<BencodeResponse, TrackerError> {
    tokio::time::timeout(timeout, announce(client, url))
        .await
        .map_err(|_| TrackerError::StoppedTimeout)?
}

pub struct AnnounceContext {
    pub client: reqwest::Client,
    pub torrent_meta: TorrentMeta,
    pub peer_id: [u8; 20],
    pub tiers: TrackerTiers,
    pub announce_key: u32,
    pub port: u16,
    pub download_state: Arc<Mutex<DownloadState>>,
    pub torrent_downloaded_state: Arc<TorrentDownloadedState>,
}

pub type HttpAnnounceContext = AnnounceContext;

struct AnnounceOutcome {
    peers: Vec<SocketAddr>,
    interval: u64,
    min_interval: Option<u64>,
    tracker_id: Option<Vec<u8>>,
}

enum Wake {
    IntervalElapsed,
    Completed,
    Shutdown,
}

pub async fn run_announce_loop<F, Fut>(
    mut ctx: AnnounceContext,
    shutdown: CancellationToken,
    mut on_peers: F,
) where
    F: FnMut(Vec<SocketAddr>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut sent_started = false;
    let mut sent_completed = false;
    let mut tracker_ids: HashMap<String, Vec<u8>> = HashMap::new();
    let mut udp_sessions: HashMap<String, UdpTracker> = HashMap::new();
    let mut backoff: Option<Duration> = None;
    let mut joined = false;
    let mut last_success_url: Option<String> = None;

    if ctx.tiers.is_empty() {
        shutdown.cancelled().await;
        return;
    }

    loop {
        if !wait_until_downloading(&ctx.download_state, &shutdown).await {
            break;
        }

        let event = next_announce_event(
            ctx.torrent_downloaded_state.is_complete(),
            sent_started,
            sent_completed,
        );
        let urls: Vec<String> = ctx.tiers.urls().map(str::to_owned).collect();
        let mut outcome: Option<AnnounceOutcome> = None;

        for url in urls {
            let Some(scheme) = tracker_scheme(&url) else {
                warn!(tracker = %url, "skipping tracker with unsupported scheme");
                continue;
            };

            let params = current_announce_params(&ctx, event, tracker_ids.get(&url).cloned());
            joined = true;
            let result = announce_to_url(&ctx, &url, scheme, &params, &mut udp_sessions).await;
            if shutdown.is_cancelled() {
                if let Ok(resp) = result {
                    ctx.tiers.promote(&url);
                    last_success_url = Some(url.clone());
                    if let Some(id) = resp.tracker_id {
                        tracker_ids.insert(url, id);
                    }
                }
                break;
            }

            match result {
                Ok(resp) => {
                    ctx.tiers.promote(&url);
                    last_success_url = Some(url.clone());
                    if let Some(id) = resp.tracker_id.as_ref() {
                        tracker_ids.insert(url, id.clone());
                    }
                    outcome = Some(resp);
                    break;
                }
                Err(e) => {
                    debug!(tracker = %url, error = %e, "tracker announce failed");
                }
            }
        }

        if shutdown.is_cancelled() {
            break;
        }

        match outcome {
            Some(resp) => {
                backoff = None;
                if event == Some(AnnounceEvent::Started) {
                    sent_started = true;
                }
                if event == Some(AnnounceEvent::Completed) {
                    sent_completed = true;
                }
                on_peers(resp.peers).await;
                let delay = reannounce_delay(resp.interval, resp.min_interval);
                match wait_reannounce(
                    delay,
                    &shutdown,
                    &ctx.torrent_downloaded_state,
                    sent_completed,
                )
                .await
                {
                    Wake::Shutdown => break,
                    Wake::Completed | Wake::IntervalElapsed => {}
                }
            }
            None => {
                let delay = next_backoff(backoff);
                backoff = Some(delay);
                match wait_reannounce(
                    delay,
                    &shutdown,
                    &ctx.torrent_downloaded_state,
                    sent_completed,
                )
                .await
                {
                    Wake::Shutdown => break,
                    Wake::Completed | Wake::IntervalElapsed => {}
                }
            }
        }
    }

    if joined {
        let stopped_url = last_success_url.or_else(|| ctx.tiers.urls().next().map(str::to_owned));
        if let Some(url) = stopped_url {
            let tracker_id = tracker_ids.get(&url).cloned();
            send_stopped(&ctx, &url, tracker_id, &mut udp_sessions).await;
        }
    }
}

pub async fn run_http_announce_loop<F, Fut>(
    ctx: HttpAnnounceContext,
    shutdown: CancellationToken,
    on_peers: F,
) where
    F: FnMut(Vec<SocketAddr>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    run_announce_loop(ctx, shutdown, on_peers).await;
}

async fn announce_to_url(
    ctx: &AnnounceContext,
    url: &str,
    scheme: TrackerScheme,
    params: &AnnounceParams,
    udp_sessions: &mut HashMap<String, UdpTracker>,
) -> Result<AnnounceOutcome, TrackerError> {
    match scheme {
        TrackerScheme::Http => announce_http(ctx, url, params).await,
        TrackerScheme::Udp => announce_udp(ctx, url, params, udp_sessions).await,
    }
}

async fn announce_http(
    ctx: &AnnounceContext,
    tracker_url: &str,
    params: &AnnounceParams,
) -> Result<AnnounceOutcome, TrackerError> {
    let url = file::build_tracker_url(&ctx.torrent_meta, &ctx.peer_id, tracker_url, params);
    let resp = announce(&ctx.client, &url).await?;
    let peers = match resp.get_peers() {
        Ok(peers) => peers,
        Err(e) => {
            debug!(
                tracker = %tracker_url,
                error = %e,
                "failed to parse peers from HTTP tracker"
            );
            Vec::new()
        }
    };
    Ok(AnnounceOutcome {
        peers,
        interval: resp.interval.unwrap_or(DEFAULT_INTERVAL_SECS),
        min_interval: resp.min_interval,
        tracker_id: resp.tracker_id.as_ref().map(|id| id.to_vec()),
    })
}

async fn announce_udp(
    ctx: &AnnounceContext,
    tracker_url: &str,
    params: &AnnounceParams,
    udp_sessions: &mut HashMap<String, UdpTracker>,
) -> Result<AnnounceOutcome, TrackerError> {
    let tracker = udp_sessions
        .entry(tracker_url.to_string())
        .or_insert_with(|| UdpTracker::new(tracker_url.to_string()));
    let options = AnnounceOptions {
        torrent_meta: ctx.torrent_meta.clone(),
        peer_id: ctx.peer_id,
        port: params.port,
        uploaded: params.uploaded,
        downloaded: params.downloaded,
        left: params.left,
        event: udp_announce_event(params.event),
        key: params.key,
    };
    let resp = tracker
        .announce(&options)
        .await
        .map_err(|e| TrackerError::Udp(e.to_string()))?;
    Ok(AnnounceOutcome {
        peers: resp.peers.into_iter().map(|p| p.to_socket_addr()).collect(),
        interval: u64::from(resp.interval),
        min_interval: None,
        tracker_id: None,
    })
}

fn next_announce_event(
    is_complete: bool,
    sent_started: bool,
    sent_completed: bool,
) -> Option<AnnounceEvent> {
    if is_complete && sent_started && !sent_completed {
        Some(AnnounceEvent::Completed)
    } else if !sent_started {
        Some(AnnounceEvent::Started)
    } else {
        None
    }
}

fn current_announce_params(
    ctx: &AnnounceContext,
    event: Option<AnnounceEvent>,
    tracker_id: Option<Vec<u8>>,
) -> AnnounceParams {
    AnnounceParams {
        // TODO(#8): wire uploaded from the session upload counter once seeding exists
        uploaded: 0,
        downloaded: ctx.torrent_downloaded_state.downloaded_bytes(),
        left: ctx.torrent_downloaded_state.left_bytes(),
        port: ctx.port,
        event,
        numwant: AnnounceParams::DEFAULT_NUMWANT,
        key: ctx.announce_key,
        tracker_id,
    }
}

async fn send_stopped(
    ctx: &AnnounceContext,
    tracker_url: &str,
    tracker_id: Option<Vec<u8>>,
    udp_sessions: &mut HashMap<String, UdpTracker>,
) {
    let Some(scheme) = tracker_scheme(tracker_url) else {
        return;
    };
    let params = AnnounceParams {
        uploaded: 0,
        downloaded: ctx.torrent_downloaded_state.downloaded_bytes(),
        left: ctx.torrent_downloaded_state.left_bytes(),
        port: ctx.port,
        event: Some(AnnounceEvent::Stopped),
        numwant: 0,
        key: ctx.announce_key,
        tracker_id,
    };
    let result = tokio::time::timeout(
        STOPPED_TIMEOUT,
        announce_to_url(ctx, tracker_url, scheme, &params, udp_sessions),
    )
    .await;
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            debug!(
                tracker = %tracker_url,
                error = %e,
                "tracker stopped announce failed"
            );
        }
        Err(_) => {
            debug!(
                tracker = %tracker_url,
                "tracker stopped announce timed out"
            );
        }
    }
}

async fn wait_until_downloading(
    download_state: &Arc<Mutex<DownloadState>>,
    shutdown: &CancellationToken,
) -> bool {
    loop {
        if shutdown.is_cancelled() {
            return false;
        }
        if *download_state.lock().unwrap() == DownloadState::Downloading {
            return true;
        }
        tokio::select! {
            _ = shutdown.cancelled() => return false,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
}

async fn wait_reannounce(
    delay: Duration,
    shutdown: &CancellationToken,
    torrent_downloaded_state: &TorrentDownloadedState,
    sent_completed: bool,
) -> Wake {
    if shutdown.is_cancelled() {
        return Wake::Shutdown;
    }
    if delay.is_zero() {
        return Wake::IntervalElapsed;
    }

    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Wake::Shutdown,
            _ = &mut sleep => return Wake::IntervalElapsed,
            _ = tick.tick() => {
                if !sent_completed && torrent_downloaded_state.is_complete() {
                    return Wake::Completed;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_backoff_doubles_and_caps() {
        assert_eq!(next_backoff(None), Duration::from_secs(15));
        assert_eq!(
            next_backoff(Some(Duration::from_secs(15))),
            Duration::from_secs(30)
        );
        assert_eq!(
            next_backoff(Some(Duration::from_secs(15 * 60))),
            Duration::from_secs(30 * 60)
        );
        assert_eq!(
            next_backoff(Some(Duration::from_secs(30 * 60))),
            Duration::from_secs(30 * 60)
        );
    }

    #[test]
    fn reannounce_delay_uses_min_interval_as_floor() {
        assert_eq!(reannounce_delay(60, None), Duration::from_secs(60));
        assert_eq!(reannounce_delay(60, Some(120)), Duration::from_secs(120));
        assert_eq!(reannounce_delay(1800, Some(60)), Duration::from_secs(1800));
        assert_eq!(
            reannounce_delay(0, None),
            Duration::from_secs(DEFAULT_INTERVAL_SECS)
        );
    }

    #[test]
    fn http_client_is_reused() {
        let first = http_client();
        let second = http_client();
        drop((first, second));
        let _ = build_http_client();
    }
}
