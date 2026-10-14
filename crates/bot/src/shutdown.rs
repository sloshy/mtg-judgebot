//! A process's graceful shutdown: what a SIGTERM (a deploy's `docker stop`)
//! or a Ctrl-C asks of the serving roles, so the new instance of a
//! blue-green deploy takes over without a question in progress dying with
//! the old one.
//!
//! [`Shutdown`] is one handle per process, shared by every role. Once it is
//! [requested](Shutdown::request):
//!
//! - **New work stops.** The HTTP listener stops accepting connections, and
//!   the Discord role closes its gateway connection, so no interaction
//!   reaches this process again. A role still standing by for the gateway
//!   stops waiting.
//! - **The gateway lease goes at once.** The Discord role releases it as soon
//!   as its connection has closed, not when the process exits, so a standby
//!   starts its grace period while this process drains.
//! - **In-flight work finishes.** Each Discord event holds a [`Work`] guard
//!   for as long as its handler runs ([`crate::discord`]), and the HTTP
//!   listener waits for the requests it has accepted. An answer already
//!   deferred to Discord is sent over the REST API, which needs no gateway.
//! - **The wait is bounded.** Everything must be done within the
//!   [limit](Shutdown::limit) of the request ([`DRAIN_LIMIT`] in production);
//!   what is still running then is cut off. Compose's `stop_grace_period`
//!   is longer than the limit, so the process exits before it is killed.

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use tokio::time::Instant;
use tokio_util::{
    sync::CancellationToken,
    task::{TaskTracker, task_tracker::TaskTrackerToken},
};

/// How long a requested shutdown waits for in-flight work: two minutes,
/// long enough for an answer that retried. `docker-compose.yml`'s
/// `stop_grace_period` for `judgebot` exceeds it by [`EXIT_MARGIN`] (a test
/// holds the two together).
pub const DRAIN_LIMIT: Duration = Duration::from_secs(120);

/// What a process needs after the drain to exit cleanly (the last spend
/// ledger write, closing the pool) before compose's `stop_grace_period`
/// ends it with SIGKILL.
pub const EXIT_MARGIN: Duration = Duration::from_secs(10);

/// The bound on the spend ledger's last write after the drain: a database
/// that does not answer must not hold the exit past [`EXIT_MARGIN`].
pub const FLUSH_LIMIT: Duration = Duration::from_secs(5);

const _: () = assert!(FLUSH_LIMIT.as_nanos() < EXIT_MARGIN.as_nanos());

/// The process's shutdown, shared by its roles; cheap to clone.
#[derive(Clone, Debug)]
pub struct Shutdown(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    limit: Duration,
    requested: CancellationToken,
    /// When the drain ends, set by the first request.
    deadline: OnceLock<Instant>,
    work: TaskTracker,
}

/// One piece of in-flight work (a Discord event's handler): a requested
/// shutdown waits until every `Work` is dropped, or its limit passes.
#[derive(Debug)]
#[must_use = "the work is counted only while this is held"]
pub struct Work(#[expect(dead_code, reason = "held for its drop")] TaskTrackerToken);

/// How the drain ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drained {
    /// Every piece of work finished.
    Finished,
    /// The limit passed with this many still running.
    Abandoned(usize),
}

impl Shutdown {
    /// A shutdown not yet requested, whose drain is bounded by `limit`
    /// ([`DRAIN_LIMIT`] in production).
    #[must_use]
    pub fn new(limit: Duration) -> Self {
        Self(Arc::new(Inner {
            limit,
            requested: CancellationToken::new(),
            deadline: OnceLock::new(),
            work: TaskTracker::new(),
        }))
    }

    /// The bound on the drain.
    #[must_use]
    pub fn limit(&self) -> Duration {
        self.0.limit
    }

    /// Ask for the shutdown; `why` is logged. Only the first request counts:
    /// the drain's deadline runs from it.
    pub fn request(&self, why: &str) {
        let mut first = false;
        self.0.deadline.get_or_init(|| {
            first = true;
            Instant::now() + self.0.limit
        });
        if first {
            tracing::info!(
                why,
                drain_limit_secs = self.0.limit.as_secs(),
                "shutting down: taking no new work, finishing what is in flight"
            );
            self.0.requested.cancel();
        }
    }

    /// Whether a shutdown has been requested.
    #[must_use]
    pub fn is_requested(&self) -> bool {
        self.0.requested.is_cancelled()
    }

    /// Resolves once a shutdown has been requested (at once, after that).
    pub async fn requested(&self) {
        self.0.requested.cancelled().await;
    }

    /// Resolves once a shutdown has been requested and its limit has passed.
    pub async fn limit_passed(&self) {
        self.requested().await;
        if let Some(deadline) = self.0.deadline.get() {
            tokio::time::sleep_until(*deadline).await;
        }
    }

    /// Count a piece of work until the returned guard is dropped.
    pub fn work(&self) -> Work {
        Work(self.0.work.token())
    }

    /// Wait for the work in flight, at most until the limit of the request
    /// has passed. Call it once the work's source is closed (the gateway
    /// connection, the listener): work counted after it has returned is
    /// not waited for. Waiting again returns the same answer.
    pub async fn drained(&self) -> Drained {
        self.0.work.close();
        tokio::select! {
            biased;
            () = self.0.work.wait() => Drained::Finished,
            () = self.limit_passed() => match self.0.work.len() {
                0 => Drained::Finished,
                n => Drained::Abandoned(n),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    #[tokio::test(start_paused = true)]
    async fn the_drain_waits_for_work_up_to_the_limit() -> Result<()> {
        let shutdown = Shutdown::new(Duration::from_secs(10));
        let short = shutdown.work();
        let long = shutdown.work();
        let s = shutdown.clone();
        let drained = tokio::spawn(async move {
            s.requested().await;
            (s.drained().await, Instant::now())
        });
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!shutdown.is_requested());
        let asked = Instant::now();
        shutdown.request("test");
        // A second request does not move the deadline.
        tokio::time::sleep(Duration::from_secs(2)).await;
        shutdown.request("again");
        drop(short);
        let (how, at) = drained.await?;
        assert_eq!(how, Drained::Abandoned(1));
        assert_eq!(at - asked, Duration::from_secs(10));
        drop(long);
        // After the work is gone, the same handle reports it finished.
        assert_eq!(shutdown.drained().await, Drained::Finished);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn the_drain_ends_when_the_last_work_does() -> Result<()> {
        let shutdown = Shutdown::new(Duration::from_secs(60));
        let work = shutdown.work();
        shutdown.request("test");
        let s = shutdown.clone();
        let drained = tokio::spawn(async move { (s.drained().await, Instant::now()) });
        let start = Instant::now();
        tokio::time::sleep(Duration::from_secs(4)).await;
        drop(work);
        let (how, at) = drained.await?;
        assert_eq!(how, Drained::Finished);
        assert_eq!(at - start, Duration::from_secs(4));
        Ok(())
    }

    /// `docker stop` sends SIGKILL at the service's `stop_grace_period`: a
    /// period shorter than the drain would kill the answers it waits for.
    #[test]
    fn compose_waits_out_the_drain() -> Result<()> {
        let file =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docker-compose.yml");
        let compose: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&std::fs::read_to_string(file)?)?;
        let period = compose
            .get("services")
            .and_then(|s| s.get("judgebot"))
            .and_then(|j| j.get("stop_grace_period"))
            .and_then(serde_yaml_ng::Value::as_str)
            .unwrap_or_default();
        let secs: u64 = period
            .strip_suffix('s')
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| {
                anyhow::anyhow!("judgebot's stop_grace_period is not `<n>s`: {period:?}")
            })?;
        assert_eq!(
            Duration::from_secs(secs),
            DRAIN_LIMIT + EXIT_MARGIN,
            "docker-compose.yml's judgebot stop_grace_period"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_in_flight_drains_at_once() {
        let shutdown = Shutdown::new(DRAIN_LIMIT);
        shutdown.request("test");
        let start = Instant::now();
        assert_eq!(shutdown.drained().await, Drained::Finished);
        assert_eq!(Instant::now(), start);
    }
}
