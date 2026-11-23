use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dashmap::DashMap;

/// A client spent its tokens; HTTP answers 429.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverLimit;

// ---------------------------------------------------------------------------
// TokenBucket
// ---------------------------------------------------------------------------

pub struct TokenBucket {
    tokens: i64,
    rate: i64,  // tokens per second
    burst: i64, // max capacity
    last_refill_ns: u64,
}

impl TokenBucket {
    pub fn new(rate: i64, burst: i64) -> Self {
        Self::with_now(rate, burst, now_ns())
    }

    pub fn with_now(rate: i64, burst: i64, now: u64) -> Self {
        Self {
            tokens: burst,
            rate,
            burst,
            last_refill_ns: now,
        }
    }

    /// Returns `true` if tokens were consumed, `false` if the bucket is depleted.
    pub fn try_consume(&mut self, cost: i64, now: u64) -> bool {
        self.refill(now);
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }

    fn refill(&mut self, now: u64) {
        let dt = now.saturating_sub(self.last_refill_ns);
        let add = (dt as i64 * self.rate) / 1_000_000_000;
        if add > 0 {
            self.tokens = (self.tokens + add).min(self.burst);
            self.last_refill_ns = now;
        }
    }
}

// ---------------------------------------------------------------------------
// ClientRateLimiter
// ---------------------------------------------------------------------------

pub struct ClientRateLimiter {
    buckets: Arc<DashMap<IpAddr, (Mutex<TokenBucket>, Instant)>>,
    rate: i64,
    burst: i64,
}

impl ClientRateLimiter {
    pub fn new(rate_per_sec: i64, burst: i64) -> Self {
        Self {
            buckets: Arc::new(DashMap::new()),
            rate: rate_per_sec,
            burst,
        }
    }

    /// Returns `Err(OverLimit)` if the client is over limit.
    pub fn check(&self, ip: IpAddr, cost: i64) -> Result<(), OverLimit> {
        let now = now_ns();

        // Evict stale entries (inactive > 5 min) on a best-effort basis.
        self.evict_stale();

        let entry = self.buckets.entry(ip).or_insert_with(|| {
            (
                Mutex::new(TokenBucket::new(self.rate, self.burst)),
                Instant::now(),
            )
        });

        let mut bucket = entry.0.lock().unwrap_or_else(|e| {
            tracing::warn!("rate-limiter bucket mutex poisoned for {ip} - recovering");
            e.into_inner()
        });
        if bucket.try_consume(cost, now) {
            Ok(())
        } else {
            Err(OverLimit)
        }
    }

    fn evict_stale(&self) {
        let cutoff = Instant::now() - std::time::Duration::from_secs(300);
        self.buckets.retain(|_, v| v.1 > cutoff);
    }
}

fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(secs: u64) -> u64 {
        secs * 1_000_000_000
    }

    #[test]
    fn new_bucket_try_consume_below_burst_ok() {
        let mut b = TokenBucket::new(10, 100);
        assert!(b.try_consume(50, ns(0)));
    }

    #[test]
    fn new_bucket_try_consume_exceeds_burst_err() {
        let mut b = TokenBucket::new(10, 100);
        assert!(!b.try_consume(101, ns(0)));
    }

    #[test]
    fn refill_after_elapsed_replenishes_tokens() {
        let mut b = TokenBucket::with_now(100, 200, ns(0));
        // Consume all tokens.
        assert!(b.try_consume(200, ns(0)));
        assert_eq!(b.tokens, 0);
        // After 1 second, 100 tokens should be added.
        b.try_consume(0, ns(1));
        assert_eq!(b.tokens, 100);
    }

    #[test]
    fn refill_does_not_exceed_burst() {
        let mut b = TokenBucket::with_now(1000, 50, ns(0));
        // After 10 seconds the calc would add 10000 tokens, but cap at 50.
        b.try_consume(0, ns(10));
        assert_eq!(b.tokens, 50);
    }

    #[test]
    fn client_rate_limiter_first_request_ok() {
        let rl = ClientRateLimiter::new(100, 200);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(rl.check(ip, 1).is_ok());
    }

    #[test]
    fn client_rate_limiter_burst_exceeded_returns_429() {
        let rl = ClientRateLimiter::new(100, 200);
        let ip: IpAddr = "127.0.0.2".parse().unwrap();
        // Consume entire burst.
        assert!(rl.check(ip, 200).is_ok());
        // Next request should fail.
        assert_eq!(rl.check(ip, 1), Err(OverLimit));
    }
}
