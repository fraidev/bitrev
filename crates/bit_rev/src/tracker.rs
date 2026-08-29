use std::{
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use serde_bencode::de;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{
    file::{self, AnnounceEvent, AnnounceParams, TorrentMeta},
    peer::BencodeResponse,
    peer_connection::TorrentDownloadedState,
    session::DownloadState,
};

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
        // TODO(#16): set User-Agent to bitrev/<version>
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

pub struct HttpAnnounceContext {
    pub client: reqwest::Client,
    pub torrent_meta: TorrentMeta,
    pub peer_id: [u8; 20],
    pub tracker_url: String,
    pub announce_key: u32,
    pub port: u16,
    pub download_state: Arc<Mutex<DownloadState>>,
    pub torrent_downloaded_state: Arc<TorrentDownloadedState>,
}

enum Wake {
    IntervalElapsed,
    Completed,
    Shutdown,
}

pub async fn run_http_announce_loop<F, Fut>(
    ctx: HttpAnnounceContext,
    shutdown: CancellationToken,
    mut on_peers: F,
) where
    F: FnMut(Vec<std::net::SocketAddr>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut sent_started = false;
    let mut sent_completed = false;
    let mut tracker_id: Option<Vec<u8>> = None;
    let mut backoff: Option<Duration> = None;
    let mut joined = false;

    loop {
        if !wait_until_downloading(&ctx.download_state, &shutdown).await {
            break;
        }

        let event = next_announce_event(
            ctx.torrent_downloaded_state.is_complete(),
            sent_started,
            sent_completed,
        );
        let params = current_announce_params(&ctx, event, tracker_id.clone());
        let url =
            file::build_tracker_url(&ctx.torrent_meta, &ctx.peer_id, &ctx.tracker_url, &params);

        joined = true;
        let result = announce(&ctx.client, &url).await;
        if shutdown.is_cancelled() {
            if let Ok(resp) = result {
                if let Some(id) = resp.tracker_id.as_ref() {
                    tracker_id = Some(id.to_vec());
                }
            }
            break;
        }

        match result {
            Ok(resp) => {
                backoff = None;
                if event == Some(AnnounceEvent::Started) {
                    sent_started = true;
                }
                if event == Some(AnnounceEvent::Completed) {
                    sent_completed = true;
                }
                if let Some(id) = resp.tracker_id.as_ref() {
                    tracker_id = Some(id.to_vec());
                }
                match resp.get_peers() {
                    Ok(peers) => on_peers(peers).await,
                    Err(e) => {
                        debug!(
                            tracker = %ctx.tracker_url,
                            error = %e,
                            "failed to parse peers from HTTP tracker"
                        );
                    }
                }
                let interval = resp.interval.unwrap_or(DEFAULT_INTERVAL_SECS);
                let delay = reannounce_delay(interval, resp.min_interval);
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
            Err(e) => {
                debug!(tracker = %ctx.tracker_url, error = %e, "HTTP tracker announce failed");
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
        send_stopped(&ctx, tracker_id).await;
    }
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
    ctx: &HttpAnnounceContext,
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

async fn send_stopped(ctx: &HttpAnnounceContext, tracker_id: Option<Vec<u8>>) {
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
    let url = file::build_tracker_url(&ctx.torrent_meta, &ctx.peer_id, &ctx.tracker_url, &params);
    if let Err(e) = announce_with_timeout(&ctx.client, &url, STOPPED_TIMEOUT).await {
        debug!(
            tracker = %ctx.tracker_url,
            error = %e,
            "HTTP tracker stopped announce failed"
        );
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
