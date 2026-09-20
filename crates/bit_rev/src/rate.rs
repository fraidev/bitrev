use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// One 16 KiB block. A larger piece still acquires in one call (cap is max(burst, bytes)).
pub const DEFAULT_BURST: u64 = 16 * 1024;

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn refill(&mut self, rate: u64, burst: u64) {
        if rate == 0 {
            self.tokens = burst as f64;
            self.last = Instant::now();
            return;
        }
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * rate as f64).min(burst as f64);
    }
}

/// Async token bucket. `rate == 0` is unlimited and `acquire` returns without locking.
pub struct RateLimiter {
    rate: AtomicU64,
    burst: AtomicU64,
    bucket: Mutex<Bucket>,
    notify: Notify,
}

impl RateLimiter {
    pub fn new(rate: u64, burst: u64) -> Self {
        let burst = if rate == 0 { 0 } else { burst.max(1) };
        Self {
            rate: AtomicU64::new(rate),
            burst: AtomicU64::new(burst),
            bucket: Mutex::new(Bucket {
                tokens: burst as f64,
                last: Instant::now(),
            }),
            notify: Notify::new(),
        }
    }

    pub fn unlimited() -> Self {
        Self::new(0, 0)
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn burst(&self) -> u64 {
        self.burst.load(Ordering::Relaxed)
    }

    pub fn set_rate(&self, rate: u64) {
        self.rate.store(rate, Ordering::Relaxed);
        if rate > 0 && self.burst.load(Ordering::Relaxed) == 0 {
            self.burst.store(DEFAULT_BURST, Ordering::Relaxed);
        }
        self.notify.notify_waiters();
    }

    pub fn set_burst(&self, burst: u64) {
        self.burst.store(burst, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    #[inline]
    pub async fn acquire(&self, bytes: u64) {
        if bytes == 0 || self.rate.load(Ordering::Relaxed) == 0 {
            return;
        }
        self.acquire_slow(bytes).await;
    }

    async fn acquire_slow(&self, bytes: u64) {
        loop {
            match self.try_take(bytes) {
                None => return,
                Some(wait) => {
                    if wait.is_zero() {
                        tokio::task::yield_now().await;
                        continue;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = self.notify.notified() => {}
                    }
                }
            }
        }
    }

    fn try_take(&self, bytes: u64) -> Option<Duration> {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return None;
        }
        let burst = self.burst.load(Ordering::Relaxed).max(bytes).max(1);
        let mut bucket = self.bucket.lock().unwrap();
        bucket.refill(rate, burst);
        let need = bytes as f64;
        if bucket.tokens >= need {
            bucket.tokens -= need;
            None
        } else {
            let wait_secs = (need - bucket.tokens) / rate as f64;
            Some(Duration::from_secs_f64(wait_secs.max(0.0)))
        }
    }
}

/// Global plus alternative (scheduled) pair. Alt mode swaps the pair atomically.
pub struct SessionLimits {
    pub download: Arc<RateLimiter>,
    pub upload: Arc<RateLimiter>,
    pub alt_download: Arc<RateLimiter>,
    pub alt_upload: Arc<RateLimiter>,
    alt_mode: AtomicBool,
}

impl SessionLimits {
    pub fn new(download: u64, upload: u64, alt_download: u64, alt_upload: u64) -> Self {
        Self {
            download: Arc::new(RateLimiter::new(download, DEFAULT_BURST)),
            upload: Arc::new(RateLimiter::new(upload, DEFAULT_BURST)),
            alt_download: Arc::new(RateLimiter::new(alt_download, DEFAULT_BURST)),
            alt_upload: Arc::new(RateLimiter::new(alt_upload, DEFAULT_BURST)),
            alt_mode: AtomicBool::new(false),
        }
    }

    pub fn unlimited() -> Self {
        Self::new(0, 0, 0, 0)
    }

    pub fn set_alt_mode(&self, on: bool) {
        self.alt_mode.store(on, Ordering::Relaxed);
    }

    pub fn alt_mode(&self) -> bool {
        self.alt_mode.load(Ordering::Relaxed)
    }

    pub fn current_download(&self) -> Arc<RateLimiter> {
        if self.alt_mode() {
            Arc::clone(&self.alt_download)
        } else {
            Arc::clone(&self.download)
        }
    }

    pub fn current_upload(&self) -> Arc<RateLimiter> {
        if self.alt_mode() {
            Arc::clone(&self.alt_upload)
        } else {
            Arc::clone(&self.upload)
        }
    }
}

/// Per-peer handle: torrent limiter (0 = inherit) then the current global pair.
#[derive(Clone)]
pub struct BandwidthLimiters {
    session: Arc<SessionLimits>,
    torrent_down: Arc<RateLimiter>,
    torrent_up: Arc<RateLimiter>,
}

impl BandwidthLimiters {
    pub fn new(
        session: Arc<SessionLimits>,
        torrent_down: Arc<RateLimiter>,
        torrent_up: Arc<RateLimiter>,
    ) -> Self {
        Self {
            session,
            torrent_down,
            torrent_up,
        }
    }

    pub fn unlimited() -> Self {
        Self::new(
            Arc::new(SessionLimits::unlimited()),
            Arc::new(RateLimiter::unlimited()),
            Arc::new(RateLimiter::unlimited()),
        )
    }

    pub async fn acquire_download(&self, bytes: u64) {
        self.torrent_down.acquire(bytes).await;
        self.session.current_download().acquire(bytes).await;
    }

    pub async fn acquire_upload(&self, bytes: u64) {
        self.torrent_up.acquire(bytes).await;
        self.session.current_upload().acquire(bytes).await;
    }
}

/// Daily window `HH:MM-HH:MM` in local time. Empty / unparsable is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AltSchedule {
    pub start_min: u16,
    pub end_min: u16,
}

impl AltSchedule {
    pub fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        let (start, end) = spec.split_once('-')?;
        Some(Self {
            start_min: parse_hhmm(start.trim())?,
            end_min: parse_hhmm(end.trim())?,
        })
    }

    pub fn active_now(&self) -> bool {
        let Some(now) = local_minutes() else {
            return false;
        };
        self.active_at(now)
    }

    pub fn active_at(&self, now_min: u16) -> bool {
        let now = now_min % (24 * 60);
        if self.start_min == self.end_min {
            false
        } else if self.start_min < self.end_min {
            now >= self.start_min && now < self.end_min
        } else {
            now >= self.start_min || now < self.end_min
        }
    }
}

fn parse_hhmm(text: &str) -> Option<u16> {
    let (h, m) = text.split_once(':')?;
    let hour: u16 = h.parse().ok()?;
    let min: u16 = m.parse().ok()?;
    if hour > 23 || min > 59 {
        return None;
    }
    Some(hour * 60 + min)
}

fn local_minutes() -> Option<u16> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as libc::time_t;
    unsafe {
        // localtime_r writes into our stack tm and is thread-safe.
        let mut tm = std::mem::zeroed::<libc::tm>();
        if libc::localtime_r(&ts, &mut tm).is_null() {
            return None;
        }
        let hour = tm.tm_hour;
        let min = tm.tm_min;
        if hour < 0 || min < 0 {
            return None;
        }
        Some((hour as u16) * 60 + min as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlimited_acquire_is_immediate() {
        let limiter = RateLimiter::unlimited();
        let start = Instant::now();
        limiter.acquire(1024 * 1024).await;
        assert!(start.elapsed() < Duration::from_millis(20));
    }

    #[tokio::test]
    async fn limited_acquire_waits_after_burst() {
        let limiter = RateLimiter::new(10_000, 10_000);
        limiter.acquire(10_000).await;
        let start = Instant::now();
        limiter.acquire(10_000).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(800),
            "expected ~1s wait, got {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(2500),
            "waited too long: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn set_rate_zero_unblocks() {
        let limiter = Arc::new(RateLimiter::new(1, 1));
        limiter.acquire(1).await;
        let limiter2 = limiter.clone();
        let task = tokio::spawn(async move {
            limiter2.acquire(1_000_000).await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        limiter.set_rate(0);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("acquire should unblock")
            .expect("join");
    }

    #[test]
    fn alt_schedule_parses_and_wraps() {
        let s = AltSchedule::parse("01:00-06:00").unwrap();
        assert_eq!(s.start_min, 60);
        assert_eq!(s.end_min, 360);
        assert!(s.active_at(60));
        assert!(s.active_at(359));
        assert!(!s.active_at(360));
        assert!(!s.active_at(0));

        let wrap = AltSchedule::parse("22:00-02:00").unwrap();
        assert!(wrap.active_at(22 * 60));
        assert!(wrap.active_at(30));
        assert!(!wrap.active_at(3 * 60));
        assert!(AltSchedule::parse("").is_none());
        assert!(AltSchedule::parse("nope").is_none());
    }

    #[tokio::test]
    async fn alt_mode_switches_global_pair() {
        let limits = Arc::new(SessionLimits::new(0, 0, 1, 1));
        let chained = BandwidthLimiters::new(
            limits.clone(),
            Arc::new(RateLimiter::unlimited()),
            Arc::new(RateLimiter::unlimited()),
        );
        chained.acquire_download(1024).await;
        limits.set_alt_mode(true);
        limits.alt_download.set_rate(1);
        limits.alt_download.set_burst(1);
        limits.alt_download.acquire(1).await;
        let start = Instant::now();
        chained.acquire_download(1).await;
        assert!(start.elapsed() >= Duration::from_millis(500));
    }
}
