//! Sliding-window per-IP rate limiting for the sensitive entry points.
//!
//! One limiter instance lives in the app state and guards the routes
//! where a cheap unauthenticated request has a cost worth defending:
//! magic-link requests (outbound email / log spam) and websocket
//! connections (session tasks). The limiter is intentionally tiny —
//! a `HashMap` of timestamp deques behind a mutex is plenty for a
//! single-binary self-hoster.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Tracks one bucket of recent event timestamps per client IP.
pub struct RateLimiter {
    max: usize,
    window: Duration,
    buckets: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

impl RateLimiter {
    /// Allow at most `max` events per IP per `window`. `max == 0`
    /// disables limiting entirely.
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Record one event for `ip`; `true` if it fits under the limit.
    pub fn allow(&self, ip: IpAddr) -> bool {
        self.allow_at(ip, Instant::now())
    }

    /// Testable core of [`Self::allow`] with an injected clock.
    pub(crate) fn allow_at(&self, ip: IpAddr, now: Instant) -> bool {
        if self.max == 0 {
            return true;
        }
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let Some(bucket) = buckets.get_mut(&ip) else {
            // First event from this IP: fast path.
            buckets.insert(ip, std::iter::once(now).collect());
            return true;
        };
        // Slide the window: drop events older than `window`.
        while bucket
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.window)
        {
            bucket.pop_front();
        }
        if bucket.len() >= self.max {
            return false;
        }
        bucket.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, n))
    }

    #[test]
    fn allows_up_to_max_then_denies() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60));
        let t = Instant::now();
        assert!(limiter.allow_at(ip(1), t));
        assert!(limiter.allow_at(ip(1), t));
        assert!(limiter.allow_at(ip(1), t));
        assert!(!limiter.allow_at(ip(1), t), "4th event within window");
    }

    #[test]
    fn window_slides() {
        let limiter = RateLimiter::new(2, Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(limiter.allow_at(ip(1), t0));
        assert!(limiter.allow_at(ip(1), t0 + Duration::from_secs(10)));
        assert!(!limiter.allow_at(ip(1), t0 + Duration::from_secs(20)));
        // Both events have left the window by now.
        assert!(limiter.allow_at(ip(1), t0 + Duration::from_secs(71)));
    }

    #[test]
    fn zero_disables_limiting() {
        let limiter = RateLimiter::new(0, Duration::from_secs(60));
        let t = Instant::now();
        for _ in 0..10 {
            assert!(limiter.allow_at(ip(1), t));
        }
    }

    #[test]
    fn ips_are_independent() {
        let limiter = RateLimiter::new(1, Duration::from_secs(60));
        let t = Instant::now();
        assert!(limiter.allow_at(ip(1), t));
        assert!(!limiter.allow_at(ip(1), t));
        assert!(limiter.allow_at(ip(2), t), "other IP has its own bucket");
    }
}
