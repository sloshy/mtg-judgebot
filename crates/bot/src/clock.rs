//! The time the gateway hand-off ([`crate::discord::gateway`]), the
//! scheduler ([`crate::jobs`]) and a refresh run's limit ([`crate::ingest`])
//! wait on.
//!
//! [`Tokio`] is tokio's timer, which every process runs on: each method is
//! the tokio call itself. A test runs the same code on a
//! [`manual::Manual`] clock that moves only when the test moves it, while
//! its database round trips stay real. Tokio's paused time cannot do that:
//! it jumps ahead whenever the runtime idles, and a round trip to Postgres
//! idles it. On a manual clock no limit expires because the machine is
//! slow, and a wait is asserted as the deadline the code asked for, not as
//! an elapsed time.

use std::time::Duration;

use tokio::time::{Instant, Interval, MissedTickBehavior};

/// A clock to wait on; see the module docs.
pub(crate) trait Clock {
    /// What an expired limit returns.
    type Elapsed: std::error::Error + Send + Sync + 'static;
    /// What [`Clock::ticks`] hands out.
    type Ticks: Ticks;

    /// The time now.
    fn now(&self) -> Instant;

    /// Wait for `period`.
    fn sleep(&self, period: Duration) -> impl Future<Output = ()>;

    /// `future`'s output, or [`Clock::Elapsed`] once `limit` has passed
    /// first.
    fn timeout<F: Future>(
        &self,
        limit: Duration,
        future: F,
    ) -> impl Future<Output = Result<F::Output, Self::Elapsed>>;

    /// `future`'s output, or [`Clock::Elapsed`] once `deadline` has come
    /// first.
    fn timeout_at<F: Future>(
        &self,
        deadline: Instant,
        future: F,
    ) -> impl Future<Output = Result<F::Output, Self::Elapsed>>;

    /// A tick every `period`, the first one `period` from now; a tick that
    /// came late moves the ones after it ([`MissedTickBehavior::Delay`]).
    fn ticks(&self, period: Duration) -> Self::Ticks;
}

/// A clock's ticks.
pub(crate) trait Ticks {
    /// Wait for the next tick.
    fn tick(&mut self) -> impl Future<Output = ()>;
}

/// Tokio's timer.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tokio;

impl Clock for Tokio {
    type Elapsed = tokio::time::error::Elapsed;
    type Ticks = Interval;

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, period: Duration) -> impl Future<Output = ()> {
        tokio::time::sleep(period)
    }

    fn timeout<F: Future>(
        &self,
        limit: Duration,
        future: F,
    ) -> impl Future<Output = Result<F::Output, Self::Elapsed>> {
        tokio::time::timeout(limit, future)
    }

    fn timeout_at<F: Future>(
        &self,
        deadline: Instant,
        future: F,
    ) -> impl Future<Output = Result<F::Output, Self::Elapsed>> {
        tokio::time::timeout_at(deadline, future)
    }

    fn ticks(&self, period: Duration) -> Interval {
        let mut ticks = tokio::time::interval_at(Instant::now() + period, period);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticks
    }
}

impl Ticks for Interval {
    async fn tick(&mut self) {
        Self::tick(self).await;
    }
}

#[cfg(test)]
pub(crate) mod manual {
    //! A clock a test moves by hand.
    //!
    //! Every wait on it registers its deadline, so a test can wait until the
    //! code it drives is parked on exactly the deadlines it expects
    //! ([`Manual::parked`]) before moving the clock there
    //! ([`Manual::advance_to`]). Times are given as offsets from the clock's
    //! start.

    use std::{
        pin::Pin,
        sync::{Arc, Mutex, PoisonError},
        task::{Context, Poll},
        time::Duration,
    };

    use anyhow::{Context as _, Result};
    use tokio::{
        sync::{oneshot, watch},
        time::Instant,
    };

    use super::{Clock, Ticks};

    /// A limit that expired on a [`Manual`] clock.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Elapsed;

    impl std::fmt::Display for Elapsed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("deadline has elapsed")
        }
    }

    impl std::error::Error for Elapsed {}

    /// How long, in real time, [`Manual::parked`] waits before it fails: a
    /// guard against a hang, never the thing a test asserts.
    pub const HANG: Duration = Duration::from_secs(30);

    /// A clock that moves only when [`Manual::advance_to`] moves it.
    #[derive(Clone, Debug)]
    pub struct Manual(Arc<Inner>);

    #[derive(Debug)]
    struct Inner {
        start: Instant,
        state: Mutex<State>,
        /// Bumped whenever a wait begins or ends.
        changed: watch::Sender<u64>,
    }

    #[derive(Debug)]
    struct State {
        now: Instant,
        waits: Vec<(Instant, oneshot::Sender<()>)>,
    }

    impl Inner {
        fn state(&self) -> std::sync::MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn bump(&self) {
            self.changed.send_modify(|n| *n = n.wrapping_add(1));
        }
    }

    /// One wait, registered while it lives.
    struct Wait {
        fired: Option<oneshot::Receiver<()>>,
        clock: Arc<Inner>,
    }

    impl Future for Wait {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            let Some(fired) = self.fired.as_mut() else {
                return Poll::Ready(());
            };
            match Pin::new(fired).poll(cx) {
                Poll::Ready(Ok(())) => {
                    self.fired = None;
                    Poll::Ready(())
                }
                // The clock dropped the wait without firing it: it never ends.
                Poll::Ready(Err(_)) | Poll::Pending => Poll::Pending,
            }
        }
    }

    impl Drop for Wait {
        fn drop(&mut self) {
            if self.fired.take().is_some() {
                self.clock.bump();
            }
        }
    }

    impl Default for Manual {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Manual {
        /// A clock standing at its start.
        #[must_use]
        pub fn new() -> Self {
            let start = Instant::now();
            Self(Arc::new(Inner {
                start,
                state: Mutex::new(State {
                    now: start,
                    waits: Vec::new(),
                }),
                changed: watch::Sender::new(0),
            }))
        }

        /// The clock's time as an offset from its start.
        #[must_use]
        pub fn elapsed(&self) -> Duration {
            self.now() - self.0.start
        }

        /// Wait until the clock reaches `deadline`.
        fn sleep_until(&self, deadline: Instant) -> impl Future<Output = ()> + use<> {
            let fired = {
                let mut state = self.0.state();
                if deadline <= state.now {
                    None
                } else {
                    let (tx, rx) = oneshot::channel();
                    state.waits.push((deadline, tx));
                    Some(rx)
                }
            };
            if fired.is_some() {
                self.0.bump();
            }
            Wait {
                fired,
                clock: Arc::clone(&self.0),
            }
        }

        /// The deadlines being waited on, as offsets from the start, in
        /// order.
        #[must_use]
        pub fn deadlines(&self) -> Vec<Duration> {
            let mut state = self.0.state();
            state.waits.retain(|(_, tx)| !tx.is_closed());
            let mut out: Vec<Duration> = state
                .waits
                .iter()
                .map(|(at, _)| *at - self.0.start)
                .collect();
            out.sort_unstable();
            out
        }

        /// Wait until the deadlines being waited on are exactly `expected`
        /// (offsets from the start, in any order).
        ///
        /// Code mid-way between two waits shows a passing set, such as a
        /// check's own timeout while the check is in flight. `expected` must
        /// be a set no such state can show, or a test moves the clock under
        /// the code it drives: the gateway's tests pin that with a
        /// compile-time check on their timings.
        ///
        /// # Errors
        /// When they are not, [`HANG`] later, naming what they are.
        pub async fn parked(&self, expected: &[Duration]) -> Result<()> {
            let mut expected = expected.to_vec();
            expected.sort_unstable();
            let mut changed = self.0.changed.subscribe();
            let settled = async {
                loop {
                    changed.mark_unchanged();
                    if self.deadlines() == expected {
                        return;
                    }
                    if changed.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
            };
            tokio::time::timeout(HANG, settled).await.with_context(|| {
                format!(
                    "waiting on {:?}, not {expected:?}, after {} s",
                    self.deadlines(),
                    HANG.as_secs()
                )
            })
        }

        /// Move the clock to `offset` after its start and end every wait due
        /// by then. A clock never goes back: an earlier offset ends what is
        /// due now and leaves the time as it is.
        pub fn advance_to(&self, offset: Duration) {
            let fired = {
                let mut state = self.0.state();
                state.now = state.now.max(self.0.start + offset);
                let now = state.now;
                let (due, waiting) = std::mem::take(&mut state.waits)
                    .into_iter()
                    .partition::<Vec<_>, _>(|(at, _)| *at <= now);
                state.waits = waiting;
                due
            };
            for (_, tx) in fired {
                // A receiver that is gone was a wait that ended anyway.
                let _ = tx.send(());
            }
            self.0.bump();
        }
    }

    impl Clock for Manual {
        type Elapsed = Elapsed;
        type Ticks = ManualTicks;

        fn now(&self) -> Instant {
            self.0.state().now
        }

        fn sleep(&self, period: Duration) -> impl Future<Output = ()> {
            self.sleep_until(self.now() + period)
        }

        fn timeout<F: Future>(
            &self,
            limit: Duration,
            future: F,
        ) -> impl Future<Output = Result<F::Output, Elapsed>> {
            self.timeout_at(self.now() + limit, future)
        }

        /// The future is polled before the limit, as tokio's is.
        fn timeout_at<F: Future>(
            &self,
            deadline: Instant,
            future: F,
        ) -> impl Future<Output = Result<F::Output, Elapsed>> {
            let limit = self.sleep_until(deadline);
            async move {
                tokio::select! {
                    biased;
                    out = future => Ok(out),
                    () = limit => Err(Elapsed),
                }
            }
        }

        fn ticks(&self, period: Duration) -> ManualTicks {
            ManualTicks {
                clock: self.clone(),
                next: self.now() + period,
                period,
            }
        }
    }

    /// A manual clock's ticks, delayed after a late tick as tokio's are.
    #[derive(Debug)]
    pub struct ManualTicks {
        clock: Manual,
        next: Instant,
        period: Duration,
    }

    impl Ticks for ManualTicks {
        async fn tick(&mut self) {
            self.clock.sleep_until(self.next).await;
            self.next = self.next.max(self.clock.now()) + self.period;
        }
    }

    mod tests {
        use super::*;

        const SEC: Duration = Duration::from_secs(1);

        #[tokio::test]
        async fn a_wait_ends_when_the_clock_reaches_it_and_not_before() -> Result<()> {
            let m = Manual::new();
            let clock = m.clone();
            let slept = tokio::spawn({
                let clock = clock.clone();
                async move { clock.sleep(5 * SEC).await }
            });
            m.parked(&[5 * SEC]).await?;
            m.advance_to(4 * SEC);
            m.parked(&[5 * SEC]).await?;
            assert!(!slept.is_finished());
            m.advance_to(5 * SEC);
            tokio::time::timeout(HANG, slept).await??;
            assert!(m.deadlines().is_empty());
            assert_eq!(m.elapsed(), 5 * SEC);
            Ok(())
        }

        #[tokio::test]
        async fn a_timeout_expires_only_at_its_deadline_and_a_finished_one_is_forgotten()
        -> Result<()> {
            let m = Manual::new();
            let clock = m.clone();
            assert_eq!(clock.timeout(SEC, async { 7 }).await, Ok(7));
            assert!(m.deadlines().is_empty(), "the finished one's wait is gone");
            let hung = tokio::spawn({
                let clock = clock.clone();
                async move { clock.timeout(3 * SEC, std::future::pending::<()>()).await }
            });
            m.parked(&[3 * SEC]).await?;
            m.advance_to(3 * SEC);
            assert_eq!(tokio::time::timeout(HANG, hung).await??, Err(Elapsed));
            Ok(())
        }

        #[tokio::test]
        async fn ticks_come_every_period_and_a_late_one_delays_the_rest() -> Result<()> {
            let m = Manual::new();
            let mut ticks = m.ticks(5 * SEC);
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn({
                let m = m.clone();
                async move {
                    loop {
                        ticks.tick().await;
                        if tx.send(m.elapsed()).is_err() {
                            return;
                        }
                    }
                }
            });
            m.parked(&[5 * SEC]).await?;
            m.advance_to(5 * SEC);
            assert_eq!(rx.recv().await, Some(5 * SEC));
            m.parked(&[10 * SEC]).await?;
            // Late: the next comes a period after this one, not at 15.
            m.advance_to(12 * SEC);
            assert_eq!(rx.recv().await, Some(12 * SEC));
            m.parked(&[17 * SEC]).await?;
            task.abort();
            Ok(())
        }
    }
}
