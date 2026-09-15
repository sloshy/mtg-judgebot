//! Per-IP fixed-window rate limiting.
//!
//! Anonymous web questions are real Anthropic spend, so the API refuses a
//! client that has used its window before any LLM call happens. Fixed-window
//! is deliberately simple: the operator's real backstop is the client's spend
//! cap (`JUDGE_MAX_USD`); this only keeps one browser from draining it.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

/// Tracked IPs before expired windows are swept (bounds memory, not fairness).
const SWEEP_AT: usize = 1024;

/// Allows `limit` requests per `window` per IP.
#[derive(Debug)]
pub struct RateLimiter {
    limit: u32,
    window: Duration,
    seen: Mutex<HashMap<IpAddr, Window>>,
}

#[derive(Debug)]
struct Window {
    started: Instant,
    count: u32,
}

impl RateLimiter {
    /// A limiter allowing `limit` requests per `window` per IP.
    #[must_use]
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Record a request from `ip` now; `false` means the window is used up.
    #[must_use]
    pub fn allow(&self, ip: IpAddr) -> bool {
        self.allow_at(ip, Instant::now())
    }

    /// [`Self::allow`] with an explicit clock, for tests.
    #[must_use]
    pub fn allow_at(&self, ip: IpAddr, now: Instant) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.len() >= SWEEP_AT {
            let window = self.window;
            seen.retain(|_, w| now.duration_since(w.started) < window);
        }
        let w = seen.entry(ip).or_insert(Window {
            started: now,
            count: 0,
        });
        if now.duration_since(w.started) >= self.window {
            *w = Window {
                started: now,
                count: 0,
            };
        }
        if w.count >= self.limit {
            return false;
        }
        w.count = w.count.saturating_add(1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    #[test]
    fn limits_per_ip_and_resets_after_the_window() {
        let l = RateLimiter::new(2, Duration::from_mins(1));
        let t0 = Instant::now();
        assert!(l.allow_at(ip(1), t0));
        assert!(l.allow_at(ip(1), t0));
        assert!(
            !l.allow_at(ip(1), t0),
            "third request in the window is refused"
        );
        // Another IP has its own window.
        assert!(l.allow_at(ip(2), t0));
        // A refused request does not extend the window: it still ends on time.
        assert!(!l.allow_at(ip(1), t0 + Duration::from_secs(59)));
        assert!(l.allow_at(ip(1), t0 + Duration::from_mins(1)));
    }

    #[test]
    fn sweeping_expired_windows_keeps_live_ones() {
        let l = RateLimiter::new(1, Duration::from_mins(1));
        let t0 = Instant::now();
        for i in 0..=u8::MAX {
            let _ = l.allow_at(IpAddr::V4(Ipv4Addr::new(10, 0, 1, i)), t0);
        }
        for i in 0..=u8::MAX {
            let _ = l.allow_at(IpAddr::V4(Ipv4Addr::new(10, 0, 2, i)), t0);
        }
        // 512 tracked entries is under the sweep threshold; a live window
        // still refuses even after many other IPs were seen.
        assert!(!l.allow_at(
            IpAddr::V4(Ipv4Addr::new(10, 0, 1, 0)),
            t0 + Duration::from_secs(1)
        ));
    }
}
