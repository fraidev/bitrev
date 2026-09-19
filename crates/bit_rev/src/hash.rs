use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// SHA-1 piece verification off the async runtime, capped so a recheck cannot
/// flood the blocking pool.
#[derive(Clone)]
pub struct PieceHasher {
    inner: Arc<Inner>,
}

struct Inner {
    semaphore: Arc<Semaphore>,
    permits: usize,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

struct InFlight {
    inner: Arc<Inner>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.inner.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `min(4, available_parallelism)`, at least 1.
pub fn hash_permits() -> usize {
    std::cmp::min(
        4,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    )
    .max(1)
}

impl Default for PieceHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl PieceHasher {
    pub fn new() -> Self {
        Self::with_permits(hash_permits())
    }

    pub fn with_permits(permits: usize) -> Self {
        let permits = permits.max(1);
        Self {
            inner: Arc::new(Inner {
                semaphore: Arc::new(Semaphore::new(permits)),
                permits,
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
            }),
        }
    }

    pub fn permits(&self) -> usize {
        self.inner.permits
    }

    pub fn max_in_flight(&self) -> usize {
        self.inner.max_in_flight.load(Ordering::SeqCst)
    }

    async fn acquire(&self) -> OwnedSemaphorePermit {
        self.inner
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("hasher semaphore closed")
    }

    pub async fn run_blocking<T, F>(&self, f: F) -> Result<T, tokio::task::JoinError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let permit = self.acquire().await;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let now = inner.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            inner.max_in_flight.fetch_max(now, Ordering::SeqCst);
            let _guard = InFlight { inner };
            f()
        })
        .await
    }

    /// Hash `buf` and return it so the caller can write without a second copy.
    pub async fn verify_owned(&self, expected: [u8; 20], buf: Vec<u8>) -> (Vec<u8>, bool) {
        self.run_blocking(move || {
            let ok = crate::utils::check_integrity(&expected, &buf);
            (buf, ok)
        })
        .await
        .unwrap_or_else(|_| (Vec::new(), false))
    }

    pub async fn verify(&self, expected: [u8; 20], buf: Vec<u8>) -> bool {
        self.verify_owned(expected, buf).await.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn hashing_permit_cap_holds() {
        let hasher = PieceHasher::with_permits(2);
        let mut joins = Vec::new();
        for _ in 0..16 {
            let hasher = hasher.clone();
            joins.push(tokio::spawn(async move {
                hasher
                    .run_blocking(|| std::thread::sleep(Duration::from_millis(15)))
                    .await
                    .unwrap();
            }));
        }
        for join in joins {
            join.await.unwrap();
        }
        assert!(
            hasher.max_in_flight() <= 2,
            "max in-flight {} exceeded cap 2",
            hasher.max_in_flight()
        );
        assert_eq!(hasher.max_in_flight(), 2);
        assert_eq!(hasher.permits(), 2);
    }

    #[test]
    fn hash_permits_never_exceeds_four() {
        assert!(hash_permits() <= 4);
        assert!(hash_permits() >= 1);
    }
}
