//! Shared token-bucket speed limiter.
//!
//! Tokens are bytes. The bucket refills continuously at `rate` bytes/second and holds at
//! most ~50 ms worth of tokens. A taker may proceed whenever the balance is positive and
//! may drive it negative by a whole segment (a burst of one segment); later takers wait
//! until the debt is repaid. Over any window longer than a few segments the average rate
//! therefore converges on the limit.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How much refill the bucket may bank, as a fraction of a second.
const BURST_WINDOW_SECS: f64 = 0.05;
/// Longest single sleep, so a changed situation (debt repaid by refill) is noticed soon.
const MAX_SLEEP: Duration = Duration::from_millis(250);

#[derive(Debug)]
struct State {
    tokens: f64,
    last: Instant,
}

/// Byte-rate limiter shared by all workers of one job.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    rate: f64,
    capacity: f64,
    state: Mutex<State>,
}

impl RateLimiter {
    /// `None` when `bytes_per_sec` is 0 (unlimited).
    pub(crate) fn new(bytes_per_sec: u64) -> Option<Self> {
        if bytes_per_sec == 0 {
            return None;
        }
        let rate = bytes_per_sec as f64;
        let capacity = (rate * BURST_WINDOW_SECS).max(1.0);
        Some(Self { rate, capacity, state: Mutex::new(State { tokens: capacity, last: Instant::now() }) })
    }

    /// Waits until `n` bytes may pass. Cancel-safe: dropping the future takes nothing.
    pub(crate) async fn acquire(&self, n: usize) {
        loop {
            let wait = {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                let elapsed = now.saturating_duration_since(s.last).as_secs_f64();
                s.last = now;
                s.tokens = (s.tokens + elapsed * self.rate).min(self.capacity);
                if s.tokens > 0.0 {
                    s.tokens -= n as f64;
                    return;
                }
                Duration::from_secs_f64((-s.tokens / self.rate).max(0.001))
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
        assert!(RateLimiter::new(0).is_none());
    }

    #[tokio::test]
    async fn average_rate_is_respected() {
        let l = RateLimiter::new(100_000).unwrap();
        let start = Instant::now();
        for _ in 0..4 {
            l.acquire(10_000).await;
        }
        // First 10 KB pass on the initial balance; the remaining ~35 KB of debt take ~0.35 s.
        let e = start.elapsed();
        assert!(e >= Duration::from_millis(250), "{e:?}");
        assert!(e < Duration::from_secs(2), "{e:?}");
    }
}
