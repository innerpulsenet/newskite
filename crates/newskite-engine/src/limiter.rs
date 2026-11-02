//! Shared token-bucket speed limiter.
//!
//! Tokens are bytes. The bucket refills continuously at the current limit and holds at
//! most ~50 ms worth of tokens. A taker may proceed whenever the balance is positive and
//! may drive it negative by a whole segment (a burst of one segment); later takers wait
//! until the debt is repaid. Over any window longer than a few segments the average rate
//! therefore converges on the limit.
//!
//! The limit is a [`SpeedLimit`] read on every step, so changing it takes effect on a
//! running job within one wait (at most [`MAX_SLEEP`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How much refill the bucket may bank, as a fraction of a second.
const BURST_WINDOW_SECS: f64 = 0.05;
/// Longest single sleep, so a changed situation (debt repaid by refill, a new limit) is
/// noticed soon.
const MAX_SLEEP: Duration = Duration::from_millis(250);

/// A speed limit in bytes per second (0 = unlimited) that can be changed while jobs run:
/// clones share one value, and every job's limiter reads it as it goes.
#[derive(Clone, Debug, Default)]
pub struct SpeedLimit(Arc<AtomicU64>);

impl SpeedLimit {
    /// A limit of `bytes_per_sec` (0 = unlimited).
    pub fn new(bytes_per_sec: u64) -> Self {
        Self(Arc::new(AtomicU64::new(bytes_per_sec)))
    }

    /// The limit now, in bytes per second (0 = unlimited).
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Changes the limit for every holder of this value, running jobs included.
    pub fn set(&self, bytes_per_sec: u64) {
        self.0.store(bytes_per_sec, Ordering::Relaxed);
    }
}

/// Compares the limits' current values.
impl PartialEq for SpeedLimit {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl Eq for SpeedLimit {}

#[derive(Debug)]
struct State {
    tokens: f64,
    last: Instant,
}

/// Byte-rate limiter shared by all workers of one job.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    limit: SpeedLimit,
    state: Mutex<State>,
}

impl RateLimiter {
    /// A limiter that follows `limit`; it starts with a full bucket.
    pub(crate) fn new(limit: SpeedLimit) -> Self {
        Self { limit, state: Mutex::new(State { tokens: f64::INFINITY, last: Instant::now() }) }
    }

    /// Whether a limit is set right now (takers can skip [`RateLimiter::acquire`] if not).
    pub(crate) fn limited(&self) -> bool {
        self.limit.get() > 0
    }

    /// Waits until `n` bytes may pass under the current limit (at once when there is
    /// none, including when it is lifted while waiting). Cancel-safe: dropping the future
    /// takes nothing.
    pub(crate) async fn acquire(&self, n: usize) {
        loop {
            let wait = {
                let rate = self.limit.get() as f64;
                if rate <= 0.0 {
                    return;
                }
                let capacity = (rate * BURST_WINDOW_SECS).max(1.0);
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                let elapsed = now.saturating_duration_since(s.last).as_secs_f64();
                s.last = now;
                s.tokens = (s.tokens + elapsed * rate).min(capacity);
                if s.tokens > 0.0 {
                    s.tokens -= n as f64;
                    return;
                }
                Duration::from_secs_f64((-s.tokens / rate).max(0.001))
            };
            tokio::time::sleep(wait.min(MAX_SLEEP)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_unlimited() {
        let l = RateLimiter::new(SpeedLimit::new(0));
        assert!(!l.limited());
    }

    #[tokio::test]
    async fn unlimited_never_waits() {
        let l = RateLimiter::new(SpeedLimit::default());
        let start = Instant::now();
        for _ in 0..100 {
            l.acquire(10_000_000).await;
        }
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn average_rate_is_respected() {
        let l = RateLimiter::new(SpeedLimit::new(100_000));
        let start = Instant::now();
        for _ in 0..4 {
            l.acquire(10_000).await;
        }
        // First 10 KB pass on the initial balance; the remaining ~35 KB of debt take ~0.35 s.
        let e = start.elapsed();
        assert!(e >= Duration::from_millis(250), "{e:?}");
        assert!(e < Duration::from_secs(2), "{e:?}");
    }

    #[tokio::test]
    async fn a_new_limit_applies_to_a_waiting_taker() {
        let limit = SpeedLimit::new(1_000);
        let l = Arc::new(RateLimiter::new(limit.clone()));
        // 1 MB of debt at 1 KB/s would take over 16 minutes.
        l.acquire(1_000_000).await;
        let waiter = {
            let l = l.clone();
            tokio::spawn(async move { l.acquire(1).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "still paying the debt");
        // Raised: the debt is repaid at the new rate within one wait.
        limit.set(100_000_000);
        tokio::time::timeout(Duration::from_secs(1), waiter).await.expect("released").expect("task");
        // Lifted: no waiting at all.
        limit.set(0);
        assert!(!l.limited());
        tokio::time::timeout(Duration::from_millis(50), l.acquire(1_000_000_000)).await.expect("unlimited");
    }

    #[test]
    fn clones_share_one_value() {
        let a = SpeedLimit::new(5);
        let b = a.clone();
        b.set(7);
        assert_eq!(a.get(), 7);
        assert_eq!(a, SpeedLimit::new(7), "compared by value");
    }
}
