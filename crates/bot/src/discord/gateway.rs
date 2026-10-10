//! One gateway holder: of every process running `--discord` on a database,
//! only the one holding the [`GatewayLease`] is connected to the Discord
//! gateway, so two replicas (or an old container left beside a new one)
//! never both answer an interaction.
//!
//! [`hold`] runs the hand-off around a gateway it is given, in a loop:
//!
//! 1. **Stand by.** Take the lease, or stand by until the process holding it
//!    lets go ([`GatewayLease::stand_by`], logged at INFO with the holder's
//!    pid and `application_name`). A process with HTTP roles serves them
//!    throughout: the standby is inside the Discord role only.
//! 2. **Grace.** Wait [`Timing::grace`] after taking it, then check it is
//!    still held, before connecting. A holder that lost its lock (the
//!    database restarted, its session was ended) notices at its next check
//!    and disconnects within [`Timing::worst_disconnect`], which the grace
//!    exceeds (a compile-time relation), so the two never answer side by side.
//! 3. **Hold.** Connect, and check the lease every [`Timing::check`]. A check
//!    that fails stops at once; one with no answer within
//!    [`Timing::check_timeout`] counts as failed. Then ask the gateway to
//!    stop ([`Stop`]) and give it [`Timing::shutdown_limit`] to close. A
//!    gateway that closed in time goes back to step 1, so a database restart
//!    costs the bot a reconnect and the process's other roles nothing. One
//!    that did not ends the role with an error: the process exits, which
//!    closes the connection, and its restart policy brings it back.
//!
//! **Takeover.** A process that exits (a deploy, a crash) closes the lease's
//! connection, and Postgres frees the lock with the session: a standby takes
//! over after the grace period and its own gateway login. A holder that
//! vanished without closing its socket (power loss, a partition, a container
//! network torn down) is noticed by the server's TCP keepalives, which every
//! lease session sets ([`crate::lease::SESSION_SETTINGS`]): its session ends
//! within about 25 s, and a standby connects the grace period after that.
//!
//! **What the lease cannot cover.** A holder whose process is frozen (SIGSTOP,
//! `docker pause`, a paused VM) cannot run its check. If its database session
//! ends meanwhile (a database restart; or a paused VM, whose kernel stops
//! answering the keepalives, after about 25 s), a standby takes over, and
//! when the frozen process resumes its gateway connection may answer beside
//! the new holder's until its next check disconnects it, at most
//! [`Timing::worst_disconnect`] after it resumes. A frozen process whose
//! kernel still runs (SIGSTOP, `docker pause`) keeps its session, and so the
//! lock: no overlap, but no bot until it resumes. The mirror case is a
//! standby paused while it waits: the server can grant it the lock during
//! its round (up to [`crate::lease::STANDBY_ROUND`] per request), and the bot
//! is offline until it resumes and waits out the grace. That delays a
//! takeover without causing an overlap.
//!
//! A holder cut off from the database fails its next check and disconnects
//! within [`Timing::worst_disconnect`] (the wait for that check, its
//! timeout and the shutdown), and the server frees its lock when the
//! keepalives give up on the session, so the standby takes over about 25 s
//! plus the grace later, never earlier.
//!
//! **Discord's login budget.** Every connection sends an IDENTIFY, and
//! Discord allows 1000 a day per token before it resets the token. A lease
//! lost again soon after the last loss (a flapping database) therefore
//! pauses before standing by again: [`Timing::reconnect_pause`], doubling up
//! to ten minutes, which caps a sustained loop at under 150 logins a day. A
//! hold of [`Timing::healthy_hold`] resets it. The pause only lengthens the
//! wait before a connection, so the no-overlap relation is untouched.

use std::{future::Future, time::Duration};

use anyhow::{Context as _, Result};
use sqlx::PgPool;
use tokio::sync::watch;

use crate::{
    clock::{Clock, Ticks as _, Tokio},
    lease::GatewayLease,
};

/// How often the holder checks that it still holds the gateway lease.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// The hand-off's timings, all derived from the check interval so their
/// relation holds at any scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    check: Duration,
}

impl Timing {
    /// What the Discord role runs on: checks every [`CHECK_INTERVAL`].
    pub const DEFAULT: Self = Self {
        check: CHECK_INTERVAL,
    };

    /// How often the holder checks the lease.
    #[must_use]
    pub const fn check(self) -> Duration {
        self.check
    }

    /// One fifth of the interval (1 s at the default), unless `check` is too
    /// small to divide.
    const fn fifth(self) -> Duration {
        match self.check.checked_div(5) {
            Some(d) => d,
            None => Duration::ZERO,
        }
    }

    /// How long one check may go unanswered before it counts as lost: four
    /// fifths of the interval (4 s). A definite answer (an error, or the
    /// lock not held) counts at once. Generous, so a slow database is not
    /// mistaken for a lost lease; its ceiling is the grace relation.
    #[must_use]
    pub const fn check_timeout(self) -> Duration {
        self.fifth().saturating_mul(4)
    }

    /// How long a holder that lost the lease gives its gateway to close
    /// before the role ends and the process exits regardless: one fifth of
    /// the interval (1 s). The bot runs one shard, which closes with one
    /// websocket close frame; a gateway that takes longer is ended by the
    /// process exiting, which drops its socket.
    #[must_use]
    pub const fn shutdown_limit(self) -> Duration {
        self.fifth()
    }

    /// The longest from a holder's lock being freed to its gateway being
    /// closed: the wait for its next check, that check's timeout, and the
    /// shutdown (5 + 4 + 1 = 10 s).
    #[must_use]
    pub const fn worst_disconnect(self) -> Duration {
        self.check
            .saturating_add(self.check_timeout())
            .saturating_add(self.shutdown_limit())
    }

    /// How long a process that has just taken the lease waits before it
    /// connects: three intervals (15 s), more than
    /// [`Self::worst_disconnect`] (two), the rest being room for a holder
    /// whose gateway did not close in time to exit.
    #[must_use]
    pub const fn grace(self) -> Duration {
        self.check.saturating_mul(3)
    }

    /// The pause before standing by again after the `losses`-th lease loss
    /// in a row (each within [`Self::healthy_hold`] of the connection
    /// before): none after the first, then six intervals (30 s), doubling
    /// to [`Self::reconnect_pause_max`].
    #[must_use]
    pub const fn reconnect_pause(self, losses: u32) -> Duration {
        let Some(doublings) = losses.checked_sub(2) else {
            return Duration::ZERO;
        };
        let first = self.check.saturating_mul(6);
        let max = self.reconnect_pause_max();
        let pause = first.saturating_mul(2_u32.saturating_pow(doublings));
        if pause.as_nanos() < max.as_nanos() {
            pause
        } else {
            max
        }
    }

    /// The longest [`Self::reconnect_pause`]: 120 intervals (10 min).
    #[must_use]
    pub const fn reconnect_pause_max(self) -> Duration {
        self.check.saturating_mul(120)
    }

    /// How long a connection must have held the lease for its loss to start
    /// a new streak: 360 intervals (30 min).
    #[must_use]
    pub const fn healthy_hold(self) -> Duration {
        self.check.saturating_mul(360)
    }
}

// The interval the bot runs on is one tokio's `interval` accepts.
const _: () = assert!(!CHECK_INTERVAL.is_zero());

// The no-overlap relation, for the timings the bot runs on.
const _: () =
    assert!(Timing::DEFAULT.worst_disconnect().as_nanos() < Timing::DEFAULT.grace().as_nanos());

/// The hand-off's request that the gateway close. A gateway given one must
/// disconnect and return once [`Stop::requested`] resolves.
#[derive(Debug)]
pub struct Stop(watch::Receiver<bool>);

impl Stop {
    /// Resolves when the holder has lost the lease, or when [`hold`] itself
    /// is gone (the process is ending). Resolves again at once if awaited
    /// after that, so a gateway can ask before each stage of connecting.
    pub async fn requested(&mut self) {
        // An error is a dropped sender: `hold` is gone, which is a stop too.
        if self.0.wait_for(|stop| *stop).await.is_err() {
            tracing::debug!("the gateway hand-off is gone; stopping");
        }
    }
}

/// Run `gateway` while, and only while, this process holds the gateway
/// lease; see the module docs. `gateway` makes the connection (in the bot,
/// [`super::run`]); its future returns when the connection ends, or when
/// the [`Stop`] it is given resolves and the connection is closed. It is
/// called once per time this process takes the lease.
///
/// # Errors
/// What a connection that ended by itself returns, or a lost lease whose
/// gateway did not close within [`Timing::shutdown_limit`].
pub async fn hold<G, F>(
    pool: &PgPool,
    process: &'static str,
    timing: Timing,
    gateway: G,
) -> Result<()>
where
    G: FnMut(Stop) -> F,
    F: Future<Output = Result<()>>,
{
    hold_on(&Tokio, pool, process, timing, gateway).await
}

/// [`hold`], waiting on `clock`.
async fn hold_on<C, G, F>(
    clock: &C,
    pool: &PgPool,
    process: &'static str,
    timing: Timing,
    mut gateway: G,
) -> Result<()>
where
    C: Clock,
    G: FnMut(Stop) -> F,
    F: Future<Output = Result<()>>,
{
    // Lease losses in a row, each soon after the connection before it.
    let mut losses: u32 = 0;
    loop {
        let mut lease = take(clock, pool, process, timing).await;
        let connected = clock.now();
        tracing::info!(
            lease = lease.application_name().unwrap_or("unlabelled"),
            "holding the Discord gateway: connecting"
        );
        let (stop, stopped) = watch::channel(false);
        let mut run = std::pin::pin!(gateway(Stop(stopped)));
        // The gateway and the checks are polled side by side, so a slow check
        // never holds up the gateway's future.
        let lost = tokio::select! {
            ended = &mut run => {
                // Bounded: a release on a dead link would hang, and dropping
                // the connection frees the lock anyway.
                if clock.timeout(timing.check_timeout(), lease.release()).await.is_err() {
                    tracing::warn!("releasing the gateway lease got no answer; its connection is closed instead");
                }
                return ended;
            }
            lost = keep_checking(clock, &mut lease, timing) => lost,
        };
        drop(lease);
        tracing::error!(
            error = %format!("{lost:#}"),
            "lost the Discord gateway lease; disconnecting so another instance can take over without answering beside this one"
        );
        if stop.send(true).is_err() {
            tracing::debug!("the gateway had already ended");
        }
        match clock.timeout(timing.shutdown_limit(), run).await {
            Ok(ended) => {
                if let Err(e) = ended {
                    tracing::warn!(error = %format!("{e:#}"), "the Discord gateway ended with an error while disconnecting");
                }
                tracing::info!(
                    "disconnected from the Discord gateway; standing by for the lease again"
                );
                if clock.now().saturating_duration_since(connected) >= timing.healthy_hold() {
                    losses = 0;
                }
                losses = losses.saturating_add(1);
                let pause = timing.reconnect_pause(losses);
                if !pause.is_zero() {
                    tracing::error!(
                        losses,
                        pause_secs = pause.as_secs_f32(),
                        "the Discord gateway lease was lost again soon after the last loss; pausing before \
                         standing by again, since every connection spends one of Discord's 1000 daily logins \
                         per token"
                    );
                    clock.sleep(pause).await;
                }
            }
            Err(_) => {
                return Err(lost.context(format!(
                    "the Discord gateway lease was lost, and the gateway did not close within {:.1} s; \
                     exiting, which closes it, so the process restarts and stands by",
                    timing.shutdown_limit().as_secs_f32()
                )));
            }
        }
    }
}

/// The lease, held past the grace period: stand by, wait out the grace,
/// check. A lease lost during the grace goes back to standing by.
async fn take(
    clock: &impl Clock,
    pool: &PgPool,
    process: &'static str,
    timing: Timing,
) -> GatewayLease {
    loop {
        let mut lease = GatewayLease::stand_by(pool, process).await;
        tracing::info!(
            lease = lease.application_name().unwrap_or("unlabelled"),
            grace_secs = timing.grace().as_secs_f32(),
            "took the Discord gateway lease; waiting out the grace period so an instance that lost it has disconnected"
        );
        clock.sleep(timing.grace()).await;
        match check(clock, &mut lease, timing).await {
            Ok(()) => return lease,
            Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                "the Discord gateway lease was lost during the grace period; standing by again"
            ),
        }
    }
}

/// Check the lease every [`Timing::check`] until a check fails; the error is
/// why.
async fn keep_checking(
    clock: &impl Clock,
    lease: &mut GatewayLease,
    timing: Timing,
) -> anyhow::Error {
    let mut ticks = clock.ticks(timing.check());
    loop {
        ticks.tick().await;
        if let Err(e) = check(clock, lease, timing).await {
            return e;
        }
    }
}

/// [`GatewayLease::check`], bounded by [`Timing::check_timeout`]: a check
/// with no answer counts as lost.
async fn check(clock: &impl Clock, lease: &mut GatewayLease, timing: Timing) -> Result<()> {
    clock
        .timeout(timing.check_timeout(), lease.check())
        .await
        .with_context(|| {
            format!(
                "gateway lease lost: its session did not answer a check within {:.1} s",
                timing.check_timeout().as_secs_f32()
            )
        })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        clock::manual::Manual,
        lease::testing::{HANG, waiting},
    };
    use tokio::sync::mpsc;

    #[test]
    fn the_grace_outlasts_a_holder_that_lost_the_lease() {
        for ms in [1, 2, 3, 7, 50, 100, 999, 1_000, 5_000, 60_000] {
            let t = Timing {
                check: Duration::from_millis(ms),
            };
            assert!(t.worst_disconnect() < t.grace(), "{t:?}");
            assert!(t.check_timeout() < t.check(), "{t:?}");
        }
        let d = Timing::DEFAULT;
        assert_eq!(
            (
                d.check_timeout(),
                d.shutdown_limit(),
                d.worst_disconnect(),
                d.grace()
            ),
            (
                Duration::from_secs(4),
                Duration::from_secs(1),
                Duration::from_secs(10),
                Duration::from_secs(15)
            )
        );
    }

    #[test]
    fn reconnect_pauses_double_to_a_ceiling() {
        let pauses: Vec<u64> = (0..=10)
            .map(|n| Timing::DEFAULT.reconnect_pause(n).as_secs())
            .collect();
        assert_eq!(pauses, [0, 0, 30, 60, 120, 240, 480, 600, 600, 600, 600]);
        assert_eq!(Timing::DEFAULT.reconnect_pause(u32::MAX).as_secs(), 600);
        assert_eq!(Timing::DEFAULT.healthy_hold().as_secs(), 1_800);
        // A sustained loop stays far inside Discord's 1000 logins a day.
        let per_login = Timing::DEFAULT.reconnect_pause_max() + Timing::DEFAULT.grace();
        assert!(86_400 / per_login.as_secs() < 150);
    }

    /// What a gateway did.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        Connected(&'static str),
        Disconnected(&'static str),
    }

    /// An event, and when it happened on the test's clock.
    type Events = mpsc::UnboundedSender<(Event, Duration)>;

    /// The timings the bot runs on: the clock is the test's, so the real
    /// ones cost nothing to wait out.
    const T: Timing = Timing::DEFAULT;

    // The tests wait until the hand-off waits on exactly the deadlines they
    // expect (the next check, the grace, the pause) before they move the
    // clock. A check, a release or a shutdown in flight waits on a deadline
    // of its own, which must never equal one of those offsets: else a test
    // could take that passing state for the one it waits for, and move the
    // clock under a check.
    const _: () = {
        let (check, grace) = (T.check().as_nanos(), T.grace().as_nanos());
        let pause = T.reconnect_pause(2).as_nanos();
        let in_flight = [T.check_timeout().as_nanos(), T.shutdown_limit().as_nanos()];
        let [timeout, shutdown] = in_flight;
        assert!(timeout != check && timeout != grace && timeout != pause);
        assert!(shutdown != check && shutdown != grace && shutdown != pause);
    };

    fn report(clock: &Manual, events: &Events, e: Event) {
        if events.send((e, clock.elapsed())).is_err() {
            tracing::debug!("the test stopped listening");
        }
    }

    /// A gateway that connects, then waits for its stop or for `end` to turn
    /// true (its connection ending by itself).
    async fn fake(
        name: &'static str,
        clock: Manual,
        events: Events,
        mut end: watch::Receiver<bool>,
        mut stop: Stop,
    ) -> Result<()> {
        report(&clock, &events, Event::Connected(name));
        tokio::select! {
            () = stop.requested() => {}
            _ = end.wait_for(|e| *e) => {}
        }
        report(&clock, &events, Event::Disconnected(name));
        Ok(())
    }

    /// Run the hand-off for `name` on `clock`, around `gateway`.
    fn spawn_hold<G, F>(
        pool: &PgPool,
        clock: &Manual,
        name: &'static str,
        gateway: G,
    ) -> tokio::task::JoinHandle<Result<()>>
    where
        G: FnMut(Stop) -> F + Send + 'static,
        F: Future<Output = Result<()>> + Send,
    {
        let (pool, clock) = (pool.clone(), clock.clone());
        tokio::spawn(async move { hold_on(&clock, &pool, name, T, gateway).await })
    }

    fn spawn_holder(
        pool: &PgPool,
        clock: &Manual,
        name: &'static str,
        events: &Events,
    ) -> (tokio::task::JoinHandle<Result<()>>, watch::Sender<bool>) {
        let (end, ended) = watch::channel(false);
        let (c, events) = (clock.clone(), events.clone());
        let task = spawn_hold(pool, clock, name, move |stop| {
            fake(name, c.clone(), events.clone(), ended.clone(), stop)
        });
        (task, end)
    }

    async fn next(
        rx: &mut mpsc::UnboundedReceiver<(Event, Duration)>,
    ) -> Result<(Event, Duration)> {
        tokio::time::timeout(HANG, rx.recv())
            .await
            .context("an event in time")?
            .context("an event")
    }

    /// End `name`'s lease session, as a database restart or an operator
    /// would (any session of its that is still ending, too).
    async fn terminate(pool: &PgPool, name: &str) -> Result<()> {
        let ended: Vec<bool> = sqlx::query_scalar(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
             WHERE datname = current_database() AND application_name LIKE $1",
        )
        .bind(format!("judgebot gateway ({name}) since%"))
        .fetch_all(pool)
        .await?;
        anyhow::ensure!(!ended.is_empty(), "{name} holds no lease session");
        Ok(())
    }

    /// Spawn `name` on a free lease and see it connect after the grace,
    /// at `at` on the clock; then it is checking the lease.
    async fn connected(
        pool: &PgPool,
        clock: &Manual,
        name: &'static str,
        tx: &Events,
        rx: &mut mpsc::UnboundedReceiver<(Event, Duration)>,
    ) -> Result<(tokio::task::JoinHandle<Result<()>>, watch::Sender<bool>)> {
        let at = clock.elapsed() + T.grace();
        let holder = spawn_holder(pool, clock, name, tx);
        clock.parked(&[at]).await?;
        clock.advance_to(at);
        assert_eq!(next(rx).await?, (Event::Connected(name), at));
        clock.parked(&[at + T.check()]).await?;
        Ok(holder)
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn one_instance_connects_and_the_other_takes_over_when_it_ends(
        pool: PgPool,
    ) -> Result<()> {
        let clock = Manual::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (a, end_a) = connected(&pool, &clock, "a", &tx, &mut rx).await?;
        let (b, _end_b) = spawn_holder(&pool, &clock, "b", &tx);
        waiting(&pool, "judgebot gateway (b) waiting").await?;
        // a's checks pass, well past what would be b's grace; b stands by.
        let mut at = clock.elapsed();
        for _ in 0..4 {
            at += T.check();
            clock.parked(&[at]).await?;
            clock.advance_to(at);
        }
        clock.parked(&[at + T.check()]).await?;
        assert!(
            rx.try_recv().is_err(),
            "b stands by while a holds the gateway"
        );
        assert!(!b.is_finished());
        waiting(&pool, "judgebot gateway (b) waiting").await?;
        // a's connection ends by itself: its role ends, and b takes over
        // after the grace.
        anyhow::ensure!(end_a.send(true).is_ok(), "a is listening for its end");
        assert_eq!(next(&mut rx).await?, (Event::Disconnected("a"), at));
        tokio::time::timeout(HANG, a).await???;
        clock.parked(&[at + T.grace()]).await?;
        assert!(rx.try_recv().is_err(), "b waits out the grace");
        clock.advance_to(at + T.grace());
        assert_eq!(
            next(&mut rx).await?,
            (Event::Connected("b"), at + T.grace())
        );
        b.abort();
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_holder_that_loses_the_lease_disconnects_and_stands_by(pool: PgPool) -> Result<()> {
        let clock = Manual::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (a, _end_a) = connected(&pool, &clock, "a", &tx, &mut rx).await?;
        let (b, end_b) = spawn_holder(&pool, &clock, "b", &tx);
        waiting(&pool, "judgebot gateway (b) waiting").await?;
        // a's session ends: the lock goes to b at once, and b's grace starts.
        let lost = clock.elapsed();
        terminate(&pool, "a").await?;
        clock.parked(&[lost + T.check(), lost + T.grace()]).await?;
        // a's next check fails and a disconnects, before b's grace is out.
        clock.advance_to(lost + T.check());
        let (ev, a_gone) = next(&mut rx).await?;
        assert_eq!(ev, Event::Disconnected("a"));
        assert!(a_gone <= lost + T.worst_disconnect(), "{a_gone:?}");
        // a stands by again, behind b.
        waiting(&pool, "judgebot gateway (a) waiting").await?;
        clock.parked(&[lost + T.grace()]).await?;
        assert!(rx.try_recv().is_err(), "b waits out the grace");
        clock.advance_to(lost + T.grace());
        let (ev, b_up) = next(&mut rx).await?;
        assert_eq!(ev, Event::Connected("b"));
        assert!(b_up >= lost + T.grace() && b_up > a_gone);
        assert!(!a.is_finished(), "a stands by instead of exiting");
        // When b's connection ends, a takes the gateway back.
        anyhow::ensure!(end_b.send(true).is_ok(), "b is listening for its end");
        assert_eq!(next(&mut rx).await?, (Event::Disconnected("b"), b_up));
        tokio::time::timeout(HANG, b).await???;
        clock.parked(&[b_up + T.grace()]).await?;
        clock.advance_to(b_up + T.grace());
        assert_eq!(
            next(&mut rx).await?,
            (Event::Connected("a"), b_up + T.grace())
        );
        a.abort();
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_lone_holder_that_loses_the_lease_reconnects_after_the_grace(
        pool: PgPool,
    ) -> Result<()> {
        let clock = Manual::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (a, _end_a) = connected(&pool, &clock, "a", &tx, &mut rx).await?;
        terminate(&pool, "a").await?;
        let gone = clock.elapsed() + T.check();
        clock.advance_to(gone);
        assert_eq!(next(&mut rx).await?, (Event::Disconnected("a"), gone));
        // It takes the freed lease again, and waits out the grace first.
        clock.parked(&[gone + T.grace()]).await?;
        assert!(rx.try_recv().is_err(), "it waits out the grace");
        assert!(!a.is_finished());
        clock.advance_to(gone + T.grace());
        assert_eq!(
            next(&mut rx).await?,
            (Event::Connected("a"), gone + T.grace())
        );
        a.abort();
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_gateway_that_does_not_close_in_time_ends_the_role(pool: PgPool) -> Result<()> {
        let clock = Manual::new();
        let held = spawn_hold(&pool, &clock, "a", async |_stop: Stop| -> Result<()> {
            std::future::pending().await
        });
        clock.parked(&[T.grace()]).await?;
        clock.advance_to(T.grace());
        // Connected: checking the lease.
        let at = T.grace() + T.check();
        clock.parked(&[at]).await?;
        terminate(&pool, "a").await?;
        clock.advance_to(at);
        // The check failed, and the gateway was asked to stop: it has the
        // shutdown limit to close in, and does not.
        clock.parked(&[at + T.shutdown_limit()]).await?;
        assert!(!held.is_finished(), "the gateway is given its limit");
        clock.advance_to(at + T.shutdown_limit());
        let err = tokio::time::timeout(HANG, held)
            .await??
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("did not close within"), "{err}");
        assert!(err.contains("gateway lease lost"), "names the cause: {err}");
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_gateway_that_ends_releases_the_lease(pool: PgPool) -> Result<()> {
        let clock = Manual::new();
        let held = spawn_hold(&pool, &clock, "test", async |_stop: Stop| -> Result<()> {
            anyhow::bail!("the token was refused")
        });
        clock.parked(&[T.grace()]).await?;
        clock.advance_to(T.grace());
        let ended = tokio::time::timeout(HANG, held).await??;
        assert!(ended.is_err_and(|e| e.to_string().contains("refused")));
        let free = GatewayLease::try_acquire(&pool, "after")
            .await?
            .context("released when the gateway ended")?;
        free.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_second_loss_soon_after_the_first_pauses_before_reconnecting(
        pool: PgPool,
    ) -> Result<()> {
        let clock = Manual::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (a, _end_a) = connected(&pool, &clock, "a", &tx, &mut rx).await?;
        // Lose the lease at a's next check; when it disconnected.
        let lose = async |rx: &mut mpsc::UnboundedReceiver<(Event, Duration)>| -> Result<Duration> {
            terminate(&pool, "a").await?;
            let gone = clock.elapsed() + T.check();
            clock.parked(&[gone]).await?;
            clock.advance_to(gone);
            assert_eq!(next(rx).await?, (Event::Disconnected("a"), gone));
            Ok(gone)
        };
        // The first loss: back after the grace alone.
        let gone = lose(&mut rx).await?;
        clock.parked(&[gone + T.grace()]).await?;
        clock.advance_to(gone + T.grace());
        assert_eq!(
            next(&mut rx).await?,
            (Event::Connected("a"), gone + T.grace())
        );
        // The second, soon after: the pause, then the grace.
        let gone = lose(&mut rx).await?;
        let pause = T.reconnect_pause(2);
        assert!(!pause.is_zero());
        clock.parked(&[gone + pause]).await?;
        assert!(rx.try_recv().is_err(), "it pauses");
        clock.advance_to(gone + pause);
        clock.parked(&[gone + pause + T.grace()]).await?;
        clock.advance_to(gone + pause + T.grace());
        let (ev, up) = next(&mut rx).await?;
        assert_eq!((ev, up), (Event::Connected("a"), gone + pause + T.grace()));
        // A loss after a healthy hold starts a new streak: the grace alone.
        let late = up + T.healthy_hold();
        clock.parked(&[up + T.check()]).await?;
        clock.advance_to(late);
        clock.parked(&[late + T.check()]).await?;
        let gone = lose(&mut rx).await?;
        clock.parked(&[gone + T.grace()]).await?;
        clock.advance_to(gone + T.grace());
        assert_eq!(
            next(&mut rx).await?,
            (Event::Connected("a"), gone + T.grace())
        );
        a.abort();
        Ok(())
    }
}
