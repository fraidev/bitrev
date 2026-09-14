//! LEDBAT window and RTT/RTO estimation (BEP-0029).

use std::time::Duration;

use tokio::time::Instant;

pub const TARGET_DELAY_US: u32 = 100_000;
pub const MAX_CWND_INCREASE_BYTES_PER_RTT: u32 = 3000;
pub const BASE_DELAY_BUCKETS: usize = 13;
pub const BASE_DELAY_BUCKET: Duration = Duration::from_secs(60);
pub const MIN_RTO: Duration = Duration::from_millis(500);
pub const MAX_RTO: Duration = Duration::from_secs(60);
pub const DEFAULT_RTO: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone)]
pub struct BaseDelay {
    buckets: [u32; BASE_DELAY_BUCKETS],
    current: usize,
    last_rotate: Instant,
}

impl BaseDelay {
    pub fn new(now: Instant) -> Self {
        Self {
            buckets: [u32::MAX; BASE_DELAY_BUCKETS],
            current: 0,
            last_rotate: now,
        }
    }

    pub fn on_sample(&mut self, delay_us: u32, now: Instant) {
        while now.saturating_duration_since(self.last_rotate) >= BASE_DELAY_BUCKET {
            self.current = (self.current + 1) % BASE_DELAY_BUCKETS;
            self.buckets[self.current] = u32::MAX;
            self.last_rotate += BASE_DELAY_BUCKET;
        }
        if delay_us < self.buckets[self.current] {
            self.buckets[self.current] = delay_us;
        }
    }

    pub fn min(&self) -> u32 {
        self.buckets.iter().copied().min().unwrap_or(0)
    }

    pub fn queuing_delay(&self, our_delay_us: u32) -> u32 {
        let base = self.min();
        if base == u32::MAX {
            0
        } else {
            our_delay_us.saturating_sub(base)
        }
    }
}

#[derive(Debug, Clone)]
pub struct Ledbat {
    pub cwnd: u32,
    min_cwnd: u32,
    max_cwnd: u32,
}

impl Ledbat {
    pub fn new(mss: u32) -> Self {
        Self {
            cwnd: mss.saturating_mul(2).max(mss),
            min_cwnd: mss.max(1),
            max_cwnd: 1024 * 1024,
        }
    }

    pub fn on_ack(&mut self, bytes_acked: u32, queuing_delay_us: u32) {
        if bytes_acked == 0 {
            return;
        }
        let off_target = TARGET_DELAY_US as i64 - i64::from(queuing_delay_us);
        let scaled =
            i64::from(MAX_CWND_INCREASE_BYTES_PER_RTT) * off_target / i64::from(TARGET_DELAY_US);
        let increment = scaled * i64::from(bytes_acked) / i64::from(self.cwnd.max(1));
        let next = i64::from(self.cwnd) + increment;
        self.cwnd = next.clamp(i64::from(self.min_cwnd), i64::from(self.max_cwnd)) as u32;
    }

    pub fn on_loss(&mut self) {
        self.cwnd = (self.cwnd / 2).max(self.min_cwnd);
    }

    pub fn clamp_to_peer(&self, peer_wnd: u32) -> u32 {
        if peer_wnd == 0 {
            self.min_cwnd
        } else {
            self.cwnd.min(peer_wnd).max(self.min_cwnd)
        }
    }
}

#[derive(Debug, Clone)]
pub struct RttEstimator {
    srtt: Option<Duration>,
    rttvar: Duration,
    rto: Duration,
}

impl RttEstimator {
    pub fn new() -> Self {
        Self {
            srtt: None,
            rttvar: Duration::ZERO,
            rto: DEFAULT_RTO,
        }
    }

    pub fn rto(&self) -> Duration {
        self.rto
    }

    pub fn update(&mut self, sample: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(sample);
                self.rttvar = sample / 2;
            }
            Some(srtt) => {
                let diff = abs_duration(srtt, sample);
                self.rttvar = (self.rttvar * 3 + diff) / 4;
                self.srtt = Some((srtt * 7 + sample) / 8);
            }
        }
        let rto = self.srtt.unwrap_or(DEFAULT_RTO) + self.rttvar * 4;
        self.rto = rto.clamp(MIN_RTO, MAX_RTO);
    }

    pub fn backoff(&mut self) {
        self.rto = self.rto.saturating_mul(2).min(MAX_RTO);
    }
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self::new()
    }
}

fn abs_duration(a: Duration, b: Duration) -> Duration {
    a.abs_diff(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_delay_tracks_minimum_across_buckets() {
        let start = Instant::now();
        let mut base = BaseDelay::new(start);
        base.on_sample(80_000, start);
        base.on_sample(120_000, start);
        assert_eq!(base.min(), 80_000);
        assert_eq!(base.queuing_delay(120_000), 40_000);

        let later = start + BASE_DELAY_BUCKET;
        base.on_sample(90_000, later);
        assert_eq!(base.min(), 80_000);

        let wrapped = start + BASE_DELAY_BUCKET * (BASE_DELAY_BUCKETS as u32);
        base.on_sample(200_000, wrapped);
        assert_eq!(base.min(), 90_000);
    }

    #[test]
    fn ledbat_grows_below_target_and_shrinks_above() {
        let mut cc = Ledbat::new(1400);
        let before = cc.cwnd;
        cc.on_ack(1400, 20_000);
        assert!(
            cc.cwnd > before,
            "below-target delay should grow the window"
        );
        cc.on_ack(1400, 200_000);
        assert!(
            cc.cwnd < before + 10_000,
            "above-target delay should not keep growing"
        );
        let high = cc.cwnd;
        cc.on_loss();
        assert!(cc.cwnd <= high / 2 + 1400);
        assert!(cc.cwnd >= 1400);
    }

    #[test]
    fn rtt_estimator_sets_rto_and_backs_off() {
        let mut rtt = RttEstimator::new();
        rtt.update(Duration::from_millis(100));
        assert!(rtt.rto() >= MIN_RTO);
        assert!(rtt.rto() <= MAX_RTO);
        let prev = rtt.rto();
        rtt.backoff();
        assert_eq!(rtt.rto(), (prev * 2).min(MAX_RTO));
    }
}
