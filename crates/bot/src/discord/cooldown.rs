//! Per-user `/judge` allowance: at most [`UserLimit::max`] questions per
//! [`UserLimit::window`], so one member cannot spend the whole cap.
//!
//! A fixed window per user, the same shape as the HTTP API's per-address
//! limiter. Held in memory: a restart forgets it, which costs a member
//! nothing and the operator at most one window's worth of questions. Pure,
//! with the clock passed in, so it is tested without sleeping.

use std::{
    collections::HashMap,
    num::NonZeroU32,
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

/// How many questions one user may ask per window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserLimit {
    /// Questions per window.
    pub max: NonZeroU32,
    /// Window length.
    pub window: Duration,
}

/// Most users tracked at once. Past this, expired windows are swept first and
/// then the oldest window is dropped, so a busy server cannot grow the map
/// without bound.
pub const MAX_TRACKED: usize = 4096;

#[derive(Clone, Copy, Debug)]
struct Window {
    started: Instant,
    used: u32,
}

/// The windows in flight, keyed by Discord user id.
#[derive(Debug)]
pub struct Cooldowns {
    limit: UserLimit,
    windows: Mutex<HashMap<u64, Window>>,
}

impl Cooldowns {
    /// No user has asked anything yet.
    #[must_use]
    pub fn new(limit: UserLimit) -> Self {
        Self {
            limit,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// The limit this was built with.
    #[must_use]
    pub const fn limit(&self) -> UserLimit {
        self.limit
    }

    /// Record a question from `user` at `now`.
    ///
    /// # Errors
    /// The window is used up: how long until it ends.
    pub fn take(&self, user: u64, now: Instant) -> Result<(), Duration> {
        let mut windows = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        let window = self.limit.window;
        let live = |w: &Window| now.saturating_duration_since(w.started) < window;
        match windows.get_mut(&user) {
            Some(w) if live(w) => {
                if w.used >= self.limit.max.get() {
                    return Err(window.saturating_sub(now.saturating_duration_since(w.started)));
                }
                w.used = w.used.saturating_add(1);
            }
            _ => {
                if windows.len() >= MAX_TRACKED {
                    windows.retain(|_, w| live(w));
                }
                if windows.len() >= MAX_TRACKED
                    && let Some(oldest) = windows
                        .iter()
                        .min_by_key(|(_, w)| w.started)
                        .map(|(k, _)| *k)
                {
                    windows.remove(&oldest);
                }
                windows.insert(
                    user,
                    Window {
                        started: now,
                        used: 1,
                    },
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(max: u32, secs: u64) -> UserLimit {
        UserLimit {
            max: NonZeroU32::new(max).unwrap_or(NonZeroU32::MIN),
            window: Duration::from_secs(secs),
        }
    }

    #[test]
    fn a_user_is_refused_past_the_limit_and_told_how_long_is_left() {
        let c = Cooldowns::new(limit(2, 600));
        let t0 = Instant::now();
        assert_eq!(c.take(1, t0), Ok(()));
        assert_eq!(c.take(1, t0 + Duration::from_secs(10)), Ok(()));
        assert_eq!(
            c.take(1, t0 + Duration::from_secs(100)),
            Err(Duration::from_secs(500))
        );
    }

    #[test]
    fn each_user_has_their_own_window() {
        let c = Cooldowns::new(limit(1, 600));
        let t0 = Instant::now();
        assert_eq!(c.take(1, t0), Ok(()));
        assert!(c.take(1, t0).is_err());
        assert_eq!(c.take(2, t0), Ok(()));
    }

    #[test]
    fn the_window_ends_and_a_new_one_starts() {
        let c = Cooldowns::new(limit(1, 600));
        let t0 = Instant::now();
        assert_eq!(c.take(1, t0), Ok(()));
        assert!(c.take(1, t0 + Duration::from_secs(599)).is_err());
        assert_eq!(c.take(1, t0 + Duration::from_secs(600)), Ok(()));
        assert!(c.take(1, t0 + Duration::from_secs(601)).is_err());
    }

    #[test]
    fn the_map_stays_bounded_without_forgiving_a_live_window_early() {
        let c = Cooldowns::new(limit(1, 600));
        let t0 = Instant::now();
        for user in 0..u64::try_from(MAX_TRACKED).unwrap_or(u64::MAX) {
            assert_eq!(c.take(user, t0 + Duration::from_millis(user)), Ok(()));
        }
        // One more user evicts the oldest window only.
        let late = t0 + Duration::from_secs(10);
        assert_eq!(c.take(u64::MAX, late), Ok(()));
        let tracked = c.windows.lock().map(|w| w.len()).unwrap_or_default();
        assert_eq!(tracked, MAX_TRACKED);
        assert!(
            c.take(1, late).is_err(),
            "a live window survived the eviction"
        );
    }
}
