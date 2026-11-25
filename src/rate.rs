//! A token-bucket rate limiter for adapters.
//!
//! [`RateLimiter`] lets `capacity` requests through per `per` with bursts up to `capacity`
//! (the generic cell rate algorithm, on integer durations). Clones share one bucket, so every
//! task using an adapter waits its turn. [`RateLimiter::penalize`] holds everyone back when the
//! venue answers with a retry-after. It uses Tokio's clock, so paused-time tests run instantly.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

/// `capacity` requests per `per`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketSpec {
    pub capacity: u32,
    pub per: Duration,
}

impl BucketSpec {
    pub fn per_second(capacity: u32) -> Self {
        Self { capacity, per: Duration::from_secs(1) }
    }
}

#[derive(Debug)]
struct Bucket {
    spec: BucketSpec,
    /// Time between requests at the steady rate.
    interval: Duration,
    /// The theoretical arrival time of the next request.
    tat: Instant,
}

/// A shared token bucket.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    bucket: Arc<Mutex<Bucket>>,
}

impl RateLimiter {
    /// A full bucket. A zero capacity or period is treated as one request per second.
    pub fn new(spec: BucketSpec) -> Self {
        let spec = if spec.capacity == 0 || spec.per.is_zero() { BucketSpec::per_second(1) } else { spec };
        let interval = spec.per / spec.capacity;
        Self { bucket: Arc::new(Mutex::new(Bucket { spec, interval, tat: Instant::now() })) }
    }

    pub fn spec(&self) -> BucketSpec {
        self.lock().spec
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Bucket> {
        self.bucket.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes `cost` tokens now, or says how long to wait for them. A cost above the capacity
    /// is charged as the capacity.
    pub fn try_acquire(&self, cost: u32) -> Result<(), Duration> {
        let mut b = self.lock();
        let now = Instant::now();
        let cost = cost.clamp(1, b.spec.capacity);
        let tat = b.tat.max(now) + b.interval * cost;
        let allow_at = tat.checked_sub(b.spec.per).unwrap_or(now);
        if allow_at <= now {
            b.tat = tat;
            Ok(())
        } else {
            Err(allow_at - now)
        }
    }

    /// Waits for `cost` tokens and takes them.
    pub async fn acquire(&self, cost: u32) {
        while let Err(wait) = self.try_acquire(cost) {
            tokio::time::sleep(wait).await;
        }
    }

    /// Lets nothing through for `retry_after` (the venue said to back off).
    pub fn penalize(&self, retry_after: Duration) {
        let mut b = self.lock();
        let until = Instant::now() + retry_after + b.spec.per - b.interval;
        b.tat = b.tat.max(until);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn bursts_then_paces_and_backs_off() {
        let r = RateLimiter::new(BucketSpec::per_second(10));
        for _ in 0..10 {
            assert!(r.try_acquire(1).is_ok());
        }
        assert_eq!(r.try_acquire(1), Err(Duration::from_millis(100)));
        let start = Instant::now();
        r.clone().acquire(1).await;
        assert_eq!(start.elapsed(), Duration::from_millis(100));
        tokio::time::advance(Duration::from_secs(5)).await;
        r.penalize(Duration::from_secs(2));
        assert_eq!(r.try_acquire(1), Err(Duration::from_secs(2)));
        r.acquire(3).await;
        assert_eq!(start.elapsed(), Duration::from_millis(100 + 5000 + 2200));
    }
}
