//! Token-bucket rate limiting.
//!
//! Steam budgets appdetails by IP: roughly 200 requests per five minutes. The
//! bucket is deliberately clock-injectable so the behaviour can be tested
//! without sleeping.

use std::time::{Duration, Instant};

/// A burst budget over a rolling window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// Requests allowed back-to-back after an idle period.
    pub capacity: u32,
    /// Time it takes to refill `capacity` requests.
    pub window: Duration,
}

impl RateLimit {
    pub const fn new(capacity: u32, window: Duration) -> Self {
        Self { capacity, window }
    }

    /// Steam's documented budget for `appdetails`: 200 requests / 5 minutes.
    pub const fn steam_app_details() -> Self {
        Self::new(200, Duration::from_secs(5 * 60))
    }

    /// Tokens added per second.
    fn per_second(&self) -> f64 {
        self.capacity as f64 / self.window.as_secs_f64()
    }
}

/// A token bucket that refills continuously.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    limit: RateLimit,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// A full bucket: the first `capacity` acquisitions are immediate.
    pub fn new(limit: RateLimit, now: Instant) -> Self {
        Self {
            limit,
            tokens: limit.capacity as f64,
            last: now,
        }
    }

    pub fn limit(&self) -> RateLimit {
        self.limit
    }

    /// Tokens available right now, for diagnostics and tests.
    pub fn available(&self) -> f64 {
        self.tokens
    }

    /// Consume a token, or report how long the caller must wait for one.
    ///
    /// `None` means a token was taken. `Some(delay)` means no token was
    /// available and the caller should sleep for `delay` before asking again;
    /// nothing is consumed in that case.
    pub fn acquire(&mut self, now: Instant) -> Option<Duration> {
        if now > self.last {
            let elapsed = now.duration_since(self.last).as_secs_f64();
            self.tokens =
                (self.tokens + elapsed * self.limit.per_second()).min(self.limit.capacity as f64);
            self.last = now;
        }

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            None
        } else {
            let needed = (1.0 - self.tokens) / self.limit.per_second();
            Some(Duration::from_secs_f64(needed.max(0.0)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(3);

    fn bucket(capacity: u32) -> (TokenBucket, Instant) {
        let start = Instant::now();
        (
            TokenBucket::new(RateLimit::new(capacity, WINDOW), start),
            start,
        )
    }

    #[test]
    fn a_burst_is_allowed_then_the_bucket_drips() {
        let (mut bucket, start) = bucket(3);

        for index in 0..3 {
            assert_eq!(
                bucket.acquire(start),
                None,
                "request {index} is in the burst"
            );
        }

        // 3 tokens per 3 s = one token per second.
        assert_eq!(bucket.acquire(start), Some(Duration::from_secs(1)));
        assert_eq!(
            bucket.acquire(start + Duration::from_millis(500)),
            Some(Duration::from_millis(500))
        );
        assert_eq!(bucket.acquire(start + Duration::from_secs(1)), None);
    }

    #[test]
    fn a_failed_acquire_does_not_consume_a_token() {
        let (mut bucket, start) = bucket(1);
        assert_eq!(bucket.acquire(start), None);
        assert!(bucket.acquire(start).is_some());
        assert!(bucket.acquire(start).is_some());
        assert_eq!(bucket.acquire(start + Duration::from_secs(3)), None);
    }

    #[test]
    fn idle_time_refills_but_never_overfills() {
        let (mut bucket, start) = bucket(2);
        assert_eq!(bucket.acquire(start), None);
        assert_eq!(bucket.acquire(start), None);

        // An hour later the bucket is full again, not a thousand tokens deep.
        let later = start + Duration::from_secs(3600);
        assert_eq!(bucket.available(), 0.0);
        assert_eq!(bucket.acquire(later), None);
        assert_eq!(bucket.acquire(later), None);
        assert!(bucket.acquire(later).is_some());
        assert!(bucket.available() < 2.0);
    }

    #[test]
    fn a_backwards_clock_does_not_grant_tokens() {
        let (mut bucket, start) = bucket(1);
        assert_eq!(bucket.acquire(start), None);
        assert!(bucket.acquire(start - Duration::from_secs(10)).is_some());
    }

    #[test]
    fn steam_budget_is_two_hundred_per_five_minutes() {
        let limit = RateLimit::steam_app_details();
        assert_eq!(limit.capacity, 200);
        assert_eq!(limit.window, Duration::from_secs(300));

        let start = Instant::now();
        let mut bucket = TokenBucket::new(limit, start);
        for _ in 0..200 {
            assert_eq!(bucket.acquire(start), None);
        }
        // The 201st request waits one 1.5 s drip.
        assert_eq!(bucket.acquire(start), Some(Duration::from_millis(1500)));
    }
}
