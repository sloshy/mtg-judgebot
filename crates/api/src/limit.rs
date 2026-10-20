//! Per-IP fixed-window rate limiting.
//!
//! Anonymous web questions are real Anthropic spend, so the API refuses a
//! client that has used its window before any LLM call happens. Fixed-window
//! is deliberately simple: the operator's real backstop is the client's spend
//! cap (`JUDGE_MAX_USD`); this only keeps one browser from draining it.
//!
//! A window belongs to a [`Bucket`], not to an address: an IPv4 address is
//! its own bucket, an IPv6 address shares one with its whole /64. A
//! subscriber is handed at least a /64 and picks any address in it, so
//! keying on the /128 would hand one visitor 2^64 allowances.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv6Addr},
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

/// Tracked IPs before expired windows are swept (bounds memory, not fairness).
const SWEEP_AT: usize = 1024;

/// What one window counts: an IPv4 address, or an IPv6 /64 prefix. Made
/// only by [`Bucket::of`], so every limiter keys the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bucket(IpAddr);

impl Bucket {
    /// The bucket `ip` counts in. An IPv4-mapped IPv6 address is its IPv4
    /// address; any other IPv6 address is cut to its first 64 bits.
    #[must_use]
    pub fn of(ip: IpAddr) -> Self {
        match ip.to_canonical() {
            IpAddr::V4(v4) => Self(IpAddr::V4(v4)),
            IpAddr::V6(v6) => {
                let prefix = v6.to_bits() & (u128::MAX << 64);
                Self(IpAddr::V6(Ipv6Addr::from_bits(prefix)))
            }
        }
    }
}

/// Allows `limit` requests per `window` per [`Bucket`].
#[derive(Debug)]
pub struct RateLimiter {
    limit: u32,
    window: Duration,
    seen: Mutex<HashMap<Bucket, Window>>,
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

    /// Record a request from `ip` now; `false` means its bucket's window is
    /// used up.
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
        let w = seen.entry(Bucket::of(ip)).or_insert(Window {
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

    /// An IPv6 /64 is one visitor's: every address in it shares a window,
    /// the next /64 has its own, and an IPv4-mapped address is its IPv4 one.
    #[test]
    fn an_ipv6_slash_64_is_one_bucket() -> Result<(), std::net::AddrParseError> {
        let l = RateLimiter::new(1, Duration::from_mins(1));
        let t0 = Instant::now();
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse()?;
        let same: IpAddr = "2001:db8:1:2:ffff:ffff:ffff:ffff".parse()?;
        let next: IpAddr = "2001:db8:1:3::1".parse()?;
        assert!(l.allow_at(a, t0));
        assert!(!l.allow_at(same, t0), "another address in the same /64");
        assert!(l.allow_at(next, t0), "the neighbouring /64");
        let v4: IpAddr = "198.51.100.7".parse()?;
        let mapped: IpAddr = "::ffff:198.51.100.7".parse()?;
        assert!(l.allow_at(v4, t0));
        assert!(
            !l.allow_at(mapped, t0),
            "the IPv4-mapped form is the same address"
        );
        assert!(l.allow_at(ip(9), t0), "IPv4 stays per address");
        assert_eq!(Bucket::of(a), Bucket::of(same));
        assert_ne!(Bucket::of(a), Bucket::of(next));
        Ok(())
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
