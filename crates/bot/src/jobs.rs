//! What a long-running process does beside answering questions: today, the
//! scheduled data refresh ([`crate::ingest::refresh`]) every
//! `JUDGE_REFRESH_HOURS`, so an instance stays current with no host scheduler.
//! `judgebot --jobs` calls [`start`]; the record and the lease in Postgres make
//! every such process, a cron'd `judgebot ingest refresh` and a manual run
//! take turns.
//!
//! **When a run is due** is [`due`], pure: the last *successful* run is older
//! than the interval (or there has been none), and the last attempt of any
//! kind is older than the [`backoff`]: an hour after one failure, doubling
//! with each further failure in a row, never more than the interval. So a
//! failing refresh is retried soon, then less and less often, never on every
//! check. Every age is the database's ([`runs::history`]), so processes on
//! different hosts and a restart agree on it, and a process that dies mid-run
//! blocks nothing: its row never finishes, and only its start counts as an
//! attempt. A run that left no row (its insert failed) is remembered in
//! memory instead, so it is backed off too.
//!
//! **A check** ([`tick`]) runs every [`CHECK_EVERY`] plus jitter, the first
//! [`FIRST_CHECK`] after start (a crash loop must not hammer Scryfall). It
//! reads the migration ledger first: a schema ahead of this binary (a newer
//! release migrated it, and this process was left running) or behind it
//! (`JUDGE_AUTO_MIGRATE=false` with migrations pending) is never written to,
//! with one warning until it changes. [`ingest::refresh`] checks the same
//! before every step, for every trigger; this check only saves taking the
//! lease. Then the record: a database with no rules loaded has not had its
//! first load (`judgebot ingest init`), which a refresh is not, so the schedule
//! waits for it. When a run is due it tries the [`RefreshLease`] without
//! waiting (held: another process or cron is running one, so this check is
//! done) and, holding it, reads the record again, because the holder before it
//! may have just finished that run.
//!
//! **Isolation.** The scheduler is an OS thread of its own with a
//! current-thread runtime and a small pool of its own ([`POOL_SIZE`]), so a
//! step's blocking file I/O and a CR parse never take a worker the requests
//! need, and its queries never take a connection from their pool. What it does
//! not isolate is the database itself: the CR load and the retirement pass
//! hold the exclusive side of `CALLS_REWRITE_LOCK` for their transactions,
//! and a persist that writes a vector waits for it, so a reply can be held up
//! for as long as that takes, exactly as under a cron run. Each check is a task
//! on that runtime: a panic in one is logged (and alerted once) and the next
//! check runs as scheduled. If the thread itself dies it says so at ERROR;
//! the process goes on answering. A run is bounded by
//! [`ingest::RUN_TIMEOUT`], and the whole check by [`runs::ABANDONED_AFTER`],
//! the age at which the record counts an unfinished run as dead. A step
//! abandoned at the limit may leave a statement running on the server (sqlx
//! sends no cancel), so the scheduler's connections carry
//! [`STATEMENT_TIMEOUT`] and [`LOCK_TIMEOUT`]: such a statement, or a wait for
//! a lock, ends there at the latest, and with it any lock it held.
//!
//! **Shutdown** (D40). [`start`] takes the process's [`Shutdown`]. Once it
//! is requested the scheduler starts no check, and a run in progress
//! ([`ingest::refresh`], [`Caller::Schedule`]) cuts its step off at once, as
//! the run timeout does: the step's future is dropped, so its transaction
//! rolls back. The rest are skipped ([`Skip::Shutdown`]), and the row is
//! closed as stopped (`ok` null): no alert, no backoff, and not an attempt,
//! so the next process's first check finds the refresh due and runs it. The
//! lease is released and the thread ends ([`Ended::Shutdown`]), shutting its
//! runtime down, which closes every connection it opened. A statement the
//! cut step had sent may still be running on the server (sqlx sends no
//! cancel): the connections carry [`CLIENT_CHECK_INTERVAL`], so the server
//! finds its client gone within seconds and ends the statement, its
//! transaction and any lock it held, `CALLS_REWRITE_LOCK` included, which
//! the answers still draining may be waiting on. `main` waits for the thread
//! within the drain ([`Scheduler::stopped`]). Every other wait is raced
//! against the request too (reads and checks dropped at once, the writes
//! that record the run bounded from it), so everything after the request is
//! bounded by [`STOP_LIMIT`]. A run that finished before the request still
//! posts its alerts.
//!
//! **Alerts** go to `JUDGE_ALERT_WEBHOOK` ([`alerts`]): the first failure of
//! a streak (a time-out says so), the recovery after one, a run that left rows
//! unembedded because there were more than it may pay for unattended
//! ([`ingest::embed::UNATTENDED_CEILING`]) when the run before it did not, and
//! the first panicking check until a run succeeds. A retry that fails again is
//! not news, so it is only logged.

use std::{num::NonZeroU16, panic::AssertUnwindSafe, rc::Rc, time::Duration};

use judge_core::Locale;
use judge_llm::SpendMeter;
use sqlx::{
    PgConnection, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::time::Instant;

use crate::{
    alert::{self, AlertWebhook, catalog::RefreshFailure},
    clock::{Clock, Tokio},
    db::migrate,
    ingest::{
        self, Caller, RefreshLease, STOP_WRITES,
        runs::{self, Outcome, RunHistory, RunOutcome, RunReport, Skip, StepReport, Trigger},
    },
    shutdown::{DRAIN_LIMIT, Shutdown},
};

/// `JUDGE_REFRESH_HOURS`.
pub const REFRESH_HOURS_ENV: &str = "JUDGE_REFRESH_HOURS";
/// The longest interval `JUDGE_REFRESH_HOURS` takes: 30 days. Scryfall's
/// rulings change daily and a CR release lands every few months; a longer gap
/// is "off" with extra steps.
pub const MAX_REFRESH_HOURS: u16 = 720;
/// The wait after one failed attempt; each further failure in a row doubles
/// it, up to the interval ([`backoff`]).
pub const RETRY_AFTER: Duration = Duration::from_hours(1);
/// The first check after start, before jitter.
pub const FIRST_CHECK: Duration = Duration::from_secs(90);
/// The time between checks, before jitter.
pub const CHECK_EVERY: Duration = Duration::from_mins(10);
/// At most this is added to each wait, so two processes started together do
/// not check in step.
pub const JITTER: Duration = Duration::from_mins(2);
/// `statement_timeout` on the scheduler's connections. The longest single
/// statement a refresh sends is a bulk upsert of one Scryfall batch or one
/// table of a CR load, seconds on a NAS; the retirement pass and an embed
/// batch are shorter still. Half an hour is far above any of them and still
/// bounds a statement left running by a step the run abandoned.
pub const STATEMENT_TIMEOUT: &str = "30min";
/// `lock_timeout` on the scheduler's connections: the longest a step waits
/// for a lock, such as `CALLS_REWRITE_LOCK` behind a migration that takes
/// minutes. A wait that long means something is stuck.
pub const LOCK_TIMEOUT: &str = "15min";
/// How long the lease's release, and closing a dropped run's row, may take
/// after a run the scheduler dropped: its connection may be what hung. Past
/// it the lease is dropped, which closes its session and frees the lock.
const AFTER_DROP: Duration = Duration::from_secs(30);
/// `client_connection_check_interval` on the scheduler's connections
/// (Postgres 14 on): while a statement runs, the server checks this often
/// that the client is still connected, and ends the statement, its
/// transaction and the locks they hold once it is not. A step a shutdown cut
/// off may have left a statement running (sqlx sends no cancel); the
/// scheduler's connections close with its thread, so this ends the
/// statement within seconds rather than at [`STATEMENT_TIMEOUT`], and
/// frees `CALLS_REWRITE_LOCK` for the answers still draining.
pub const CLIENT_CHECK_INTERVAL: &str = "2s";
/// The most texts one check posts to the webhook: that the run before never
/// finished, this run's failure or recovery, and an embedding skipped at the
/// ceiling ([`alerts`]; a panicking check posts one).
const MOST_ALERTS: u32 = 3;
/// How long the scheduler's runtime waits for a blocking task (a DNS lookup)
/// as the thread ends.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);
/// The longest the scheduler takes to end after a shutdown request: every
/// read and check outside a step is dropped at once, the step in progress is
/// cut off at its next await point, the writes that record the run and the
/// lease's release share [`STOP_WRITES`] from the request, a run that had
/// finished posts its alerts (each [`alert::ALERT_TIMEOUT`]), and the runtime
/// shuts down. `main` waits for the thread at most until the drain's limit,
/// which this fits inside, so the run is closed before the process exits.
pub const STOP_LIMIT: Duration = STOP_WRITES
    .saturating_add(alert::ALERT_TIMEOUT.saturating_mul(MOST_ALERTS))
    .saturating_add(RUNTIME_SHUTDOWN);

const _: () = assert!(STOP_LIMIT.as_nanos() < DRAIN_LIMIT.as_nanos());
/// The scheduler's own pool. The lease's connection is detached from it once
/// won and does not count, so a run uses at most this many plus one.
pub const POOL_SIZE: u32 = 3;
/// Idle connections in the scheduler's pool close after this: between checks
/// it holds none.
const POOL_IDLE: Duration = Duration::from_mins(1);
/// The scheduler thread's name, as a panic message or `top -H` shows it.
const THREAD_NAME: &str = "judgebot-jobs";

/// A refresh interval: a whole number of hours, 1 to [`MAX_REFRESH_HOURS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hours(NonZeroU16);

impl Hours {
    /// The default interval: daily.
    pub const DEFAULT: Self = Self(NonZeroU16::MIN.saturating_add(23));

    /// `hours` if it is in range.
    #[must_use]
    pub const fn new(hours: u16) -> Option<Self> {
        match NonZeroU16::new(hours) {
            Some(h) if hours <= MAX_REFRESH_HOURS => Some(Self(h)),
            _ => None,
        }
    }

    /// The number of hours.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }

    /// The interval as a duration.
    #[must_use]
    pub fn interval(self) -> Duration {
        Duration::from_hours(u64::from(self.get()))
    }
}

const _: () = assert!(Hours::DEFAULT.get() == 24);

/// Whether, and how often, a long-running process refreshes the data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// Never: the operator runs `judgebot ingest refresh` (cron, by hand).
    Off,
    /// Once the last successful run is this old.
    Every(Hours),
}

impl Default for Schedule {
    fn default() -> Self {
        Self::Every(Hours::DEFAULT)
    }
}

impl Schedule {
    /// Parse `JUDGE_REFRESH_HOURS`: blank is the default, `0` is off.
    ///
    /// # Errors
    /// The value, when it is not a whole number from 0 to
    /// [`MAX_REFRESH_HOURS`].
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        let Some(v) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(Self::default());
        };
        match v.parse::<u16>() {
            Ok(0) => Ok(Self::Off),
            Ok(n) => Hours::new(n).map(Self::Every).ok_or_else(|| v.to_owned()),
            Err(_) => Err(v.to_owned()),
        }
    }
}

impl std::fmt::Display for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::Every(h) => write!(f, "every {} h", h.get()),
        }
    }
}

/// The jobs settings of one process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Jobs {
    /// The data refresh.
    pub refresh: Schedule,
    /// Where a failed or recovered refresh is reported, if anywhere.
    pub alert: Option<AlertWebhook>,
}

/// Whether a scheduled refresh should start now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    /// Start one.
    Now,
    /// Not before this many seconds have passed.
    NotYet {
        /// Seconds until it is due, as far as the record knows.
        in_secs: u64,
    },
}

/// The least time between two attempts after `failed_streak` failures in a
/// row: [`RETRY_AFTER`] for none or one, doubling with each further failure,
/// never more than the interval.
#[must_use]
pub fn backoff(every: Hours, failed_streak: u32) -> Duration {
    let doublings = failed_streak.saturating_sub(1).min(16);
    RETRY_AFTER
        .saturating_mul(1 << doublings)
        .min(every.interval())
}

/// When the next run is due, from the record and the interval; see the module
/// docs. A negative age (a timestamp ahead of the database's clock, which
/// only a clock moved back can make) counts as zero.
#[must_use]
pub fn due(every: Hours, history: &RunHistory) -> Due {
    let wait = |age: Option<i64>, gap: Duration| {
        age.map_or(0, |age| {
            gap.as_secs()
                .saturating_sub(u64::try_from(age).unwrap_or_default())
        })
    };
    let in_secs = wait(history.last_ok_age_secs, every.interval()).max(wait(
        history.last_attempt_age_secs,
        backoff(every, history.failed_streak),
    ));
    if in_secs == 0 {
        Due::Now
    } else {
        Due::NotYet { in_secs }
    }
}

/// The record with an attempt this process made `since` ago and could not
/// record counted as the latest attempt, if it is.
#[must_use]
pub fn with_unrecorded(mut history: RunHistory, since: Option<Duration>) -> RunHistory {
    if let Some(since) = since {
        let age = i64::try_from(since.as_secs()).unwrap_or(i64::MAX);
        history.last_attempt_age_secs = Some(
            history
                .last_attempt_age_secs
                .map_or(age, |recorded| recorded.min(age)),
        );
    }
    history
}

/// What the webhook is told after a scheduled run, given the record as it
/// stood before the run:
///
/// * that the latest run with a verdict never finished, when it is the only
///   failure of the current streak (its process died, so nobody was told);
/// * a failure only when the previous run with a verdict did not fail (or
///   there was none), saying so when the run timed out;
/// * a recovery only when it did (that run may have been cron's, so the text
///   does not say whose);
/// * a skipped embedding only when the previous run did not skip it too.
///
/// A run that stopped (the schema is not its binary's, or its process shut
/// down) is news to nobody: it alerts nothing, not even about the record
/// before it, which the next run with a verdict tells. The texts name steps and
/// counts, never an error message: those can carry URLs, and the log has them.
#[must_use]
pub fn alerts(
    locale: Locale,
    process: &str,
    previous: &RunHistory,
    report: &RunReport,
) -> Vec<String> {
    let m = alert::catalog::messages(locale);
    let mut out = Vec::new();
    if report.outcome() == RunOutcome::Stopped {
        // The next run with a verdict tells what this one would have.
        return out;
    }
    if let Some(started) = &previous.abandoned_started_at
        && previous.failed_streak == 1
    {
        out.push((m.refresh_abandoned)(process, started));
    }
    let failed_before = previous.last_finished_ok == Some(false);
    match report.outcome() {
        RunOutcome::Ok if failed_before => out.push((m.refresh_recovered)(process)),
        RunOutcome::Failed if !failed_before => {
            let steps: Vec<&str> = report.failed().iter().map(|s| s.name()).collect();
            let failure = if steps.is_empty() && report.timed_out {
                RefreshFailure::Dropped {
                    after_mins: runs::ABANDONED_AFTER.as_secs() / 60,
                }
            } else {
                RefreshFailure::Steps {
                    steps: &steps,
                    timed_out_hours: report
                        .timed_out
                        .then_some(ingest::RUN_TIMEOUT.as_secs() / 3600),
                }
            };
            out.push((m.refresh_failed)(process, failure));
        }
        RunOutcome::Ok | RunOutcome::Failed | RunOutcome::Stopped => {}
    }
    if !previous.last_finished_ceiling {
        for step in &report.steps {
            if let StepReport::Embed(Outcome::Skipped {
                reason: Skip::EmbedCeiling { rows, ceiling },
            }) = step
            {
                out.push((m.embed_ceiling)(process, *rows, *ceiling));
            }
        }
    }
    out
}

/// What the webhook is told the first time a check panics, in `locale`.
#[must_use]
pub fn panic_alert(locale: Locale, process: &str) -> String {
    (alert::catalog::messages(locale).refresh_panicked)(process)
}

/// Why the schedule is paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pause {
    /// A newer release migrated the database.
    Ahead,
    /// Migrations are pending.
    Behind,
    /// `refresh_runs` does not exist.
    Missing,
    /// No rules are loaded.
    Uninitialised,
}

/// What the scheduler remembers between checks.
#[derive(Clone, Copy, Debug, Default)]
struct Memory {
    /// Why the last check paused, if it did: a pause is warned about once,
    /// and again only after it cleared or its reason changed.
    paused: Option<Pause>,
    /// When the latest run that left no row in `refresh_runs` ended.
    unrecorded: Option<Instant>,
}

impl Memory {
    /// Record a pause; whether it is news (and so worth a warning).
    fn pause(&mut self, why: Pause) -> bool {
        self.paused.replace(why) != Some(why)
    }
}

/// What a check did.
#[derive(Debug)]
enum Ticked {
    /// Nothing is due yet.
    NotDue,
    /// Another process (or cron) holds the lease.
    Busy,
    /// The schema is not this binary's; nothing was written.
    Skewed,
    /// No rules are loaded: the first load has not run.
    Uninitialised,
    /// The ledger or the record could not be read.
    Unreadable,
    /// The process is shutting down: no run starts.
    ShuttingDown,
    /// A run happened.
    Ran {
        /// The record as it stood before the run, under the lease.
        previous: RunHistory,
        /// What the run did.
        report: RunReport,
        /// How long it took.
        secs: u64,
    },
}

/// One check; see the module docs. `run` is the refresh itself, handed the
/// lease, so a test can count runs without downloading anything; `limit` is
/// how long it may take ([`runs::ABANDONED_AFTER`]), on `clock`. A requested
/// `shutdown` starts no run (`run` itself stops at it, [`ingest::refresh`]).
#[expect(
    clippy::too_many_arguments,
    reason = "the check's inputs, each from a different owner"
)]
async fn tick<F>(
    clock: &impl Clock,
    pool: &PgPool,
    every: Hours,
    process: &'static str,
    memory: &mut Memory,
    limit: Duration,
    shutdown: &Shutdown,
    run: F,
) -> Ticked
where
    F: AsyncFnOnce(&mut RefreshLease) -> RunReport,
{
    // Each wait before the run is dropped at a shutdown request: a database
    // that does not answer must not hold the scheduler past its stop.
    let Some(skew) = ingest::unless_stopped(Some(shutdown), migrate::skew(pool)).await else {
        return Ticked::ShuttingDown;
    };
    match skew {
        Ok(s) => {
            if let Some(problem) = s.problem() {
                let why = if s.ahead.is_empty() {
                    Pause::Behind
                } else {
                    Pause::Ahead
                };
                if memory.pause(why) {
                    tracing::warn!("scheduled refresh paused: {problem}; it resumes by itself");
                }
                return Ticked::Skewed;
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "scheduled refresh: reading the migration ledger; trying again at the next check");
            return Ticked::Unreadable;
        }
    }
    let history = match ingest::unless_stopped(Some(shutdown), read_history(pool, memory)).await {
        None => return Ticked::ShuttingDown,
        Some(None) => return Ticked::Unreadable,
        Some(Some(history)) => history,
    };
    if history.stored_cr_version.is_none() {
        if memory.pause(Pause::Uninitialised) {
            tracing::warn!(
                "scheduled refresh paused: no rules are loaded, so the first load has not run; run \
                 `judgebot ingest init` (`docker compose run --rm refresh init`) and it resumes by itself"
            );
        }
        return Ticked::Uninitialised;
    }
    memory.paused = None;
    let since = memory
        .unrecorded
        .map(|t| clock.now().saturating_duration_since(t));
    if let Due::NotYet { in_secs } = due(every, &with_unrecorded(history, since)) {
        tracing::debug!(in_secs, "scheduled refresh not due");
        return Ticked::NotDue;
    }
    run_due(
        clock, pool, every, process, memory, since, limit, shutdown, run,
    )
    .await
}

/// The rest of a [`tick`] that found a run due: take the lease, read the
/// record again under it, and run. `since` is how long ago the latest
/// unrecorded run ended, as [`tick`] read it.
#[expect(
    clippy::too_many_arguments,
    reason = "tick's own parameters, and what it read before the lease"
)]
async fn run_due<F>(
    clock: &impl Clock,
    pool: &PgPool,
    every: Hours,
    process: &'static str,
    memory: &mut Memory,
    since: Option<Duration>,
    limit: Duration,
    shutdown: &Shutdown,
    run: F,
) -> Ticked
where
    F: AsyncFnOnce(&mut RefreshLease) -> RunReport,
{
    // A lease won as the shutdown came is dropped with its connection.
    let Some(tried) =
        ingest::unless_stopped(Some(shutdown), ingest::try_lease(pool, process)).await
    else {
        return Ticked::ShuttingDown;
    };
    let mut lease = match tried {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            tracing::debug!("scheduled refresh due, but another run holds the refresh lease");
            return Ticked::Busy;
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "scheduled refresh: taking the lease");
            return Ticked::Unreadable;
        }
    };
    // The run that held the lease before this one may have been the one due.
    let previous = match ingest::unless_stopped(Some(shutdown), read_history(pool, memory)).await {
        Some(Some(h)) if due(every, &with_unrecorded(h.clone(), since)) == Due::Now => h,
        Some(Some(_)) => {
            let_go(lease, shutdown).await;
            tracing::debug!("scheduled refresh: another run has just done it");
            return Ticked::NotDue;
        }
        Some(None) => {
            let_go(lease, shutdown).await;
            return Ticked::Unreadable;
        }
        None => {
            let_go(lease, shutdown).await;
            return Ticked::ShuttingDown;
        }
    };
    if shutdown.is_requested() {
        let_go(lease, shutdown).await;
        return Ticked::ShuttingDown;
    }
    tracing::info!(
        trigger = Trigger::Schedule.as_str(),
        process,
        last_ok = %ago(previous.last_ok_age_secs),
        "refresh starting"
    );
    let started = clock.now();
    // The database's clock at the start, so a dropped run's row can be found.
    let Some(since) = ingest::unless_stopped(Some(shutdown), runs::db_now(pool)).await else {
        let_go(lease, shutdown).await;
        return Ticked::ShuttingDown;
    };
    let since = since.ok();
    let report = if let Ok(report) = clock.timeout(limit, run(&mut lease)).await {
        let_go(lease, shutdown).await;
        report
    } else {
        // Dropped mid-step: its transaction rolls back unless it was
        // committing. Under the lease still, the row it began is closed as
        // failed, so the record does not read as a process that died; then
        // the lease goes, bounded, because its connection may be what hung.
        let shut = Some(shutdown);
        let closed = match &since {
            Some(since) => clock
                .timeout(
                    AFTER_DROP,
                    ingest::until_stopped(shut, runs::close_dropped(pool, since)),
                )
                .await
                .is_ok_and(|r| r.is_some_and(|r| r.unwrap_or(false))),
            None => false,
        };
        if clock
            .timeout(AFTER_DROP, ingest::until_stopped(shut, lease.release()))
            .await
            .is_err()
        {
            tracing::warn!("releasing the lease after a dropped run timed out; dropping it");
        }
        timed_out(closed)
    };
    if !report.recorded {
        memory.unrecorded = Some(clock.now());
    }
    Ticked::Ran {
        previous,
        report,
        secs: clock.now().saturating_duration_since(started).as_secs(),
    }
}

/// Release `lease`; after a shutdown request, only until [`STOP_WRITES`]
/// from it, then the lease is dropped, which closes its session and frees
/// the lock.
async fn let_go(lease: RefreshLease, shutdown: &Shutdown) {
    if ingest::until_stopped(Some(shutdown), lease.release())
        .await
        .is_none()
    {
        tracing::warn!(
            "releasing the refresh lease got no answer before the shutdown's deadline; dropping it"
        );
    }
}

/// The report of a run the scheduler dropped at its limit: timed out, with no
/// step of its own (which ran is not known here; the log has them), recorded
/// when its row was closed. When it was not, the scheduler backs off from it
/// itself.
fn timed_out(recorded: bool) -> RunReport {
    RunReport {
        recorded,
        timed_out: true,
        ..RunReport::of(Vec::new())
    }
}

/// The record, or `None` with a warning (once, for a missing table).
async fn read_history(pool: &PgPool, memory: &mut Memory) -> Option<RunHistory> {
    match runs::history(pool).await {
        Ok(h) => Some(h),
        Err(e) if runs::is_missing_table(&e) => {
            if memory.pause(Pause::Missing) {
                tracing::warn!(
                    "scheduled refresh paused: refresh_runs does not exist; run `judgebot ingest migrate` and it resumes by itself"
                );
            }
            None
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "scheduled refresh: reading the run record; trying again at the next check");
            None
        }
    }
}

/// `5 h ago`, for a log line.
fn ago(secs: Option<i64>) -> String {
    match secs {
        None => "never".to_owned(),
        Some(s) if s < 3600 => format!("{} min ago", s.max(0) / 60),
        Some(s) if s < 2 * 86_400 => format!("{} h ago", s / 3600),
        Some(s) => format!("{} days ago", s / 86_400),
    }
}

/// A random part of `max` ([`JITTER`]). A v4 UUID is the randomness the
/// crate already has; this needs no more than that.
fn jitter(max: Duration) -> Duration {
    let max = max.as_millis().max(1);
    let ms = uuid::Uuid::new_v4().as_u128() % max;
    Duration::from_millis(u64::try_from(ms).unwrap_or_default())
}

/// How the scheduler thread ended. It ends on its own only at the process's
/// shutdown: each of the others is a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The process is shutting down: the thread closed the run in progress
    /// (if any) as stopped, released the lease and closed its connections.
    Shutdown,
    /// The thread could not be started at all.
    NotStarted,
    /// Its runtime could not be built.
    Stopped,
    /// It panicked outside a check (a check's own panic is caught and the
    /// loop goes on).
    Panicked,
}

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Shutdown => "stopped for the shutdown",
            Self::NotStarted => "could not be started",
            Self::Stopped => "stopped",
            Self::Panicked => "panicked",
        })
    }
}

/// The running scheduler, as its caller holds it. [`Scheduler::ended`]
/// completes only when the thread has ended, so a process whose only role is
/// the jobs can exit (and be restarted) instead of idling without them.
/// Dropping it leaves the thread running.
#[derive(Debug)]
pub struct Scheduler(tokio::sync::oneshot::Receiver<Ended>);

impl Scheduler {
    /// Wait for the scheduler thread to end, and say how.
    pub async fn ended(self) -> Ended {
        // A sender dropped without a word is a thread that unwound past it.
        self.0.await.unwrap_or(Ended::Panicked)
    }

    /// Wait for the thread to end. It ends by itself once `shutdown` is
    /// requested, after closing its run ([`Ended::Shutdown`], within
    /// [`STOP_LIMIT`] of the request); the wait then lasts at most until the
    /// drain's limit ([`Shutdown::limit_passed`]), and the caller exits,
    /// the process's exit closing whatever is left. `None` when the limit
    /// came first. The thread's writes are on its own runtime and pool, so
    /// this wait cannot hold up, or be held up by, the serving roles' drain.
    pub async fn stopped(self, shutdown: &Shutdown) -> Option<Ended> {
        let ended = tokio::select! {
            biased;
            ended = self.ended() => Some(ended),
            () = shutdown.limit_passed() => None,
        };
        match ended {
            Some(Ended::Shutdown) => tracing::info!("the scheduled refresh stopped"),
            Some(other) => {
                tracing::debug!(ended = %other, "the scheduler thread had ended before the shutdown");
            }
            None => tracing::warn!(
                "the scheduled refresh did not stop within the drain; exiting under it \
                 (a run it had begun stays open, and counts as failed once it is old)"
            ),
        }
        ended
    }
}

/// When the scheduler checks: the first check `first` after start, then
/// `every`, each plus up to `jitter`.
#[derive(Clone, Copy, Debug)]
struct Timing {
    first: Duration,
    every: Duration,
    jitter: Duration,
}

impl Timing {
    const PRODUCTION: Self = Self {
        first: FIRST_CHECK,
        every: CHECK_EVERY,
        jitter: JITTER,
    };
}

/// Start the jobs `jobs` asks for on a thread of their own; see the module
/// docs. `process` names the caller in the record, the lease and an alert
/// (`judgebot`). With the
/// schedule off this logs one line, starts nothing and returns `None`.
///
/// The record is read once here, on the caller's pool, so the startup line
/// sits beside the configuration summary; everything after runs on the
/// scheduler's own runtime and pool. The refresh's embedding step bills to
/// `meter`, the process's one, so `JUDGE_MAX_USD` caps it with the questions
/// and the spend ledger records it.
///
/// `shutdown` is the process's: once it is requested the scheduler starts no
/// check, cuts the run in progress off ([`ingest::refresh`]), closes its row
/// as stopped, releases the lease and ends its thread ([`Ended::Shutdown`]),
/// which [`Scheduler::stopped`] waits for.
pub async fn start(
    pool: &PgPool,
    jobs: Jobs,
    meter: SpendMeter,
    process: &'static str,
    shutdown: Shutdown,
) -> Option<Scheduler> {
    let Schedule::Every(every) = jobs.refresh else {
        tracing::info!(
            "scheduled data refresh is off ({REFRESH_HOURS_ENV}=0): run `judgebot ingest refresh` yourself (scripts/refresh-data.sh)"
        );
        return None;
    };
    match runs::history(pool).await {
        Ok(h) => tracing::info!(
            schedule = %jobs.refresh,
            process,
            last_ok = %ago(h.last_ok_age_secs),
            cr = h.stored_cr_version.as_deref().unwrap_or("none"),
            "scheduled data refresh on"
        ),
        Err(e) => tracing::info!(
            schedule = %jobs.refresh,
            process,
            record = %format!("{e:#}"),
            "scheduled data refresh on (the run record is unreadable for now)"
        ),
    }
    let options = (*pool.connect_options())
        .clone()
        .application_name(&format!("judgebot jobs ({process})"));
    let dir = ingest::cache_dir();
    let refresh = {
        let shutdown = shutdown.clone();
        async move |lease: &mut RefreshLease| {
            ingest::refresh(lease, &dir, Caller::Schedule(&shutdown), &meter).await
        }
    };
    Some(spawn(
        options,
        every,
        jobs.alert,
        process,
        shutdown,
        Timing::PRODUCTION,
        refresh,
    ))
}

/// [`start`]'s thread, with its timing and its refresh as parameters, so a
/// test can run the thread itself without downloading anything or waiting
/// for the first check.
fn spawn<R>(
    options: PgConnectOptions,
    every: Hours,
    alert: Option<AlertWebhook>,
    process: &'static str,
    shutdown: Shutdown,
    timing: Timing,
    refresh: R,
) -> Scheduler
where
    R: AsyncFn(&mut RefreshLease) -> RunReport + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    // The sender goes back if the thread cannot be spawned, so the caller
    // still hears about it.
    let tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
    let thread_tx = std::sync::Arc::clone(&tx);
    let spawned = std::thread::Builder::new()
        .name(THREAD_NAME.to_owned())
        .spawn(move || {
            let ended = run_thread(options, every, alert, process, &shutdown, timing, refresh);
            tell(&thread_tx, ended);
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "could not start the scheduler thread; no scheduled refresh in this process");
        tell(&tx, Ended::NotStarted);
    }
    Scheduler(rx)
}

/// Report how the thread ended, once. Nobody listening is fine.
fn tell(tx: &std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Ended>>>, ended: Ended) {
    let sender = match tx.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    };
    if let Some(sender) = sender
        && sender.send(ended).is_err()
    {
        tracing::debug!("nobody waits on the scheduler thread");
    }
}

/// The scheduler thread's body: a current-thread runtime running
/// [`schedule_loop`], which returns only at the shutdown, so any other way
/// out is a failure. The runtime is shut down before this returns, so every
/// connection the thread opened is closed by the time its caller hears.
fn run_thread<R>(
    options: PgConnectOptions,
    every: Hours,
    alert: Option<AlertWebhook>,
    process: &'static str,
    shutdown: &Shutdown,
    timing: Timing,
    refresh: R,
) -> Ended
where
    R: AsyncFn(&mut RefreshLease) -> RunReport + 'static,
{
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(error = %e, "building the scheduler's runtime");
                return Ended::Stopped;
            }
        };
        let local = tokio::task::LocalSet::new();
        local.block_on(
            &runtime,
            schedule_loop(options, every, alert, process, shutdown, timing, refresh),
        );
        // The tasks go first (a connection the cut-off step left, still
        // waiting for its statement, is one), then the runtime, without
        // waiting on a blocking task (a DNS lookup) past a moment.
        drop(local);
        runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
        Ended::Shutdown
    }));
    let ended = outcome.unwrap_or(Ended::Panicked);
    if ended != Ended::Shutdown {
        tracing::error!(
            "the scheduler thread {ended}: no scheduled refresh until this process restarts \
             (serving roles keep answering; `judgebot ingest refresh` still works)"
        );
    }
    ended
}

/// `options` with the server-side bounds every scheduler connection carries
/// ([`STATEMENT_TIMEOUT`], [`LOCK_TIMEOUT`]).
fn bounded(options: PgConnectOptions) -> PgConnectOptions {
    options.options([
        ("statement_timeout", STATEMENT_TIMEOUT),
        ("lock_timeout", LOCK_TIMEOUT),
    ])
}

/// The scheduler's own pool on `options`: [`bounded`], and each connection
/// given [`CLIENT_CHECK_INTERVAL`] as it opens. Lazy: it connects at the
/// first check.
fn scheduler_pool(options: PgConnectOptions) -> PgPool {
    PgPoolOptions::new()
        .max_connections(POOL_SIZE)
        .min_connections(0)
        .idle_timeout(POOL_IDLE)
        .after_connect(|conn, _| {
            Box::pin(async move {
                check_connection(conn).await;
                Ok(())
            })
        })
        .connect_lazy_with(bounded(options))
}

/// Postgres's SQLSTATE for an unrecognized configuration parameter.
const UNDEFINED_OBJECT: &str = "42704";
/// Postgres's SQLSTATE for a value a parameter does not take (a nonzero
/// check interval on a platform that cannot check).
const INVALID_PARAMETER_VALUE: &str = "22023";

/// Set [`CLIENT_CHECK_INTERVAL`] on a new scheduler connection. Set after
/// connecting rather than at startup, where an older server would refuse the
/// connection over it: a server without it (before Postgres 14, or a
/// platform that cannot check) keeps only [`STATEMENT_TIMEOUT`], said once
/// per connection at DEBUG.
async fn check_connection(conn: &mut PgConnection) {
    let set = sqlx::query_scalar!(
        r#"SELECT set_config($1, $2, false) AS "set!""#,
        "client_connection_check_interval",
        CLIENT_CHECK_INTERVAL
    )
    .fetch_one(conn)
    .await;
    match set {
        Ok(_) => {}
        Err(sqlx::Error::Database(e))
            if matches!(
                e.code().as_deref(),
                Some(UNDEFINED_OBJECT | INVALID_PARAMETER_VALUE)
            ) =>
        {
            tracing::debug!(
                error = %e,
                "the server cannot check for a closed client; a statement a shutdown left \
                 running ends at statement_timeout"
            );
        }
        Err(e) => tracing::warn!(error = %e, "setting client_connection_check_interval"),
    }
}

/// Wait `period`; `true` (at once) when the shutdown is requested first.
async fn pause(period: Duration, shutdown: &Shutdown) -> bool {
    tokio::select! {
        biased;
        () = shutdown.requested() => true,
        () = tokio::time::sleep(period) => false,
    }
}

/// Check, sleep, repeat, until the shutdown. Each check is its own task, so a
/// panic in one is logged and the loop goes on; a check in progress at the
/// shutdown is waited for, because the run in it closes its own row
/// ([`ingest::refresh`]).
async fn schedule_loop<R>(
    options: PgConnectOptions,
    every: Hours,
    alert: Option<AlertWebhook>,
    process: &'static str,
    shutdown: &Shutdown,
    timing: Timing,
    refresh: R,
) where
    R: AsyncFn(&mut RefreshLease) -> RunReport + 'static,
{
    let pool = scheduler_pool(options);
    let client = alert::client();
    let locale = alert
        .as_ref()
        .map_or_else(Locale::default, AlertWebhook::locale);
    let refresh = Rc::new(refresh);
    let mut memory = Memory::default();
    // Outside the task, so a panic cannot lose it.
    let mut panic_told = false;
    if pause(timing.first + jitter(timing.jitter), shutdown).await {
        return;
    }
    loop {
        let check = tokio::task::spawn_local({
            let (pool, refresh, shutdown) = (pool.clone(), Rc::clone(&refresh), shutdown.clone());
            async move {
                let limit = runs::ABANDONED_AFTER;
                let ticked = tick(
                    &Tokio,
                    &pool,
                    every,
                    process,
                    &mut memory,
                    limit,
                    &shutdown,
                    &*refresh,
                )
                .await;
                (ticked, memory)
            }
        });
        match check.await {
            Ok((ticked, m)) => {
                memory = m;
                if let Ticked::Ran {
                    previous,
                    report,
                    secs,
                } = ticked
                {
                    panic_told &= !report.ok();
                    finished(every, &previous, &report, secs, process);
                    // Told even at a shutdown, so a run that finished just
                    // before it is not lost for the streak; a run the
                    // shutdown stopped tells nothing ([`alerts`]).
                    let mut told = alerts(locale, process, &previous, &report);
                    told.truncate(usize::try_from(MOST_ALERTS).unwrap_or(usize::MAX));
                    send(alert.as_ref(), &client, told).await;
                }
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "the scheduled refresh check panicked; the next check runs as scheduled"
                );
                if !std::mem::replace(&mut panic_told, true) {
                    send(alert.as_ref(), &client, vec![panic_alert(locale, process)]).await;
                }
            }
        }
        if pause(timing.every + jitter(timing.jitter), shutdown).await {
            return;
        }
    }
}

/// Log a finished run.
fn finished(
    every: Hours,
    previous: &RunHistory,
    report: &RunReport,
    secs: u64,
    process: &'static str,
) {
    let trigger = Trigger::Schedule.as_str();
    let cr = report.cr_after.as_deref().unwrap_or("none");
    match report.outcome() {
        RunOutcome::Ok => tracing::info!(trigger, process, secs, cr, "refresh finished"),
        RunOutcome::Stopped => {
            let why = report
                .steps
                .iter()
                .find_map(|s| s.skipped().filter(|k| k.stops()))
                .map_or_else(String::new, |k| k.to_string());
            tracing::info!(trigger, process, secs, %why, "refresh stopped");
        }
        RunOutcome::Failed => {
            let failed: Vec<&str> = report.failed().iter().map(|s| s.name()).collect();
            // This failure extends the streak the record showed before it.
            let streak = if previous.last_finished_ok == Some(false) {
                previous.failed_streak.saturating_add(1)
            } else {
                1
            };
            tracing::warn!(
                trigger,
                process,
                secs,
                cr,
                failed = %failed.join(", "),
                timed_out = report.timed_out,
                retry_in_mins = backoff(every, streak).as_secs() / 60,
                "refresh finished with failed steps"
            );
        }
    }
}

/// Post `texts` to the webhook, or note that there is none to post to.
async fn send(hook: Option<&AlertWebhook>, client: &reqwest::Client, texts: Vec<String>) {
    match hook {
        Some(hook) => {
            for text in texts {
                alert::post(client, hook, "refresh alert", &text).await;
            }
        }
        None if !texts.is_empty() => {
            tracing::debug!("no JUDGE_ALERT_WEBHOOK; the refresh alert is the log line above");
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{Context as _, Result};

    use super::*;
    use crate::{clock::manual::Manual, ingest::runs::Step, lease::testing::HANG};

    /// The caller hears how the thread ended, once, and a sender that went
    /// away without a word counts as a panic.
    #[tokio::test]
    async fn the_scheduler_handle_completes_when_the_thread_ends() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = std::sync::Mutex::new(Some(tx));
        tell(&tx, Ended::Stopped);
        tell(&tx, Ended::Panicked);
        assert_eq!(Scheduler(rx).ended().await, Ended::Stopped);

        let (tx, rx) = tokio::sync::oneshot::channel::<Ended>();
        drop(tx);
        assert_eq!(Scheduler(rx).ended().await, Ended::Panicked);
    }

    const DAY: Hours = Hours::DEFAULT;

    /// A shutdown nobody requests.
    fn idle() -> Shutdown {
        Shutdown::new(DRAIN_LIMIT)
    }
    const HOUR: u64 = 3600;
    const LIMIT: Duration = Duration::from_hours(4);

    /// `n` hours as an age in seconds.
    const fn hours(n: i64) -> i64 {
        n * 3600
    }

    fn h(ok: Option<i64>, attempt: Option<i64>, last_ok: Option<bool>) -> RunHistory {
        RunHistory {
            last_ok_age_secs: ok,
            last_attempt_age_secs: attempt,
            last_finished_ok: last_ok,
            stored_cr_version: Some("20260819".into()),
            failed_streak: u32::from(last_ok == Some(false)),
            last_finished_ceiling: false,
            abandoned_started_at: None,
        }
    }

    #[test]
    fn never_run_is_due_at_once() {
        assert_eq!(due(DAY, &RunHistory::default()), Due::Now);
    }

    #[test]
    fn a_recent_success_waits_out_the_interval() {
        let r = h(Some(hours(3)), Some(hours(3)), Some(true));
        assert_eq!(due(DAY, &r), Due::NotYet { in_secs: 21 * HOUR });
    }

    #[test]
    fn an_old_success_is_due() {
        let r = h(Some(hours(25)), Some(hours(25)), Some(true));
        assert_eq!(due(DAY, &r), Due::Now);
        let exactly = h(Some(hours(24)), Some(hours(24)), Some(true));
        assert_eq!(due(DAY, &exactly), Due::Now, "the boundary is due");
    }

    #[test]
    fn a_recent_failure_backs_off_for_an_hour() {
        // Last success two days ago, a failed attempt ten minutes ago.
        let r = h(Some(hours(48)), Some(600), Some(false));
        assert_eq!(due(DAY, &r), Due::NotYet { in_secs: 3000 });
    }

    #[test]
    fn an_old_failure_after_an_old_success_is_retried() {
        let r = h(Some(hours(48)), Some(hours(2)), Some(false));
        assert_eq!(due(DAY, &r), Due::Now);
        // And with no success ever.
        assert_eq!(due(DAY, &h(None, Some(hours(2)), Some(false))), Due::Now);
    }

    #[test]
    fn a_run_that_died_unfinished_blocks_only_for_the_backoff() {
        // Started 30 min ago, never finished: no `last_finished_ok` from it.
        let fresh = h(Some(hours(30)), Some(1800), Some(true));
        assert_eq!(due(DAY, &fresh), Due::NotYet { in_secs: 1800 });
        let stale = h(Some(hours(30)), Some(hours(3)), Some(true));
        assert_eq!(due(DAY, &stale), Due::Now);
    }

    #[test]
    fn the_backoff_never_exceeds_the_interval() -> Result<()> {
        let one = Hours::new(1).context("1 h")?;
        // A success 59 min ago: the interval, not more, is what is left.
        let r = h(Some(3540), Some(3540), Some(true));
        assert_eq!(due(one, &r), Due::NotYet { in_secs: 60 });
        let r = h(Some(3600), Some(3600), Some(true));
        assert_eq!(due(one, &r), Due::Now);
        let six = Hours::new(6).context("6 h")?;
        assert_eq!(backoff(six, 30), six.interval());
        Ok(())
    }

    #[test]
    fn a_failure_streak_doubles_the_wait_up_to_the_interval() {
        let waits: Vec<u64> = (0..=7).map(|n| backoff(DAY, n).as_secs() / HOUR).collect();
        assert_eq!(waits, [1, 1, 2, 4, 8, 16, 24, 24]);
        assert_eq!(backoff(DAY, u32::MAX), DAY.interval(), "no overflow");
        // Three failures in a row, the last three hours ago: one more hour.
        let r = RunHistory {
            failed_streak: 3,
            ..h(Some(hours(48)), Some(hours(3)), Some(false))
        };
        assert_eq!(due(DAY, &r), Due::NotYet { in_secs: HOUR });
    }

    #[test]
    fn an_unrecorded_attempt_counts_as_the_latest() {
        let recorded = h(Some(hours(48)), Some(hours(5)), Some(true));
        assert_eq!(due(DAY, &recorded), Due::Now);
        let ten_min = Some(Duration::from_secs(600));
        let remembered = with_unrecorded(recorded.clone(), ten_min);
        assert_eq!(remembered.last_attempt_age_secs, Some(600));
        assert_eq!(due(DAY, &remembered), Due::NotYet { in_secs: 3000 });
        // An older memory does not hide a newer recorded attempt.
        let old = with_unrecorded(recorded, Some(Duration::from_hours(9)));
        assert_eq!(old.last_attempt_age_secs, Some(hours(5)));
        assert_eq!(
            with_unrecorded(RunHistory::default(), ten_min).last_attempt_age_secs,
            Some(600),
            "and stands in for a record with no runs"
        );
    }

    #[test]
    fn a_timestamp_ahead_of_the_clock_leaves_the_whole_interval() {
        let r = h(Some(-5), Some(-5), Some(true));
        assert_eq!(due(DAY, &r), Due::NotYet { in_secs: 24 * HOUR });
    }

    #[test]
    fn the_schedule_parses_blank_as_daily_zero_as_off_and_refuses_the_rest() {
        assert_eq!(Schedule::parse(None), Ok(Schedule::Every(DAY)));
        assert_eq!(Schedule::parse(Some("  ")), Ok(Schedule::Every(DAY)));
        assert_eq!(Schedule::parse(Some("0")), Ok(Schedule::Off));
        assert_eq!(
            Schedule::parse(Some(" 6 ")).map(|s| s.to_string()),
            Ok("every 6 h".to_owned())
        );
        assert_eq!(
            Schedule::parse(Some("720")).map(|s| s.to_string()),
            Ok("every 720 h".to_owned())
        );
        for bad in ["721", "-1", "1.5", "24h", "daily", "70000"] {
            assert_eq!(Schedule::parse(Some(bad)), Err(bad.to_owned()), "{bad}");
        }
    }

    fn failed(steps: &[Step]) -> RunReport {
        RunReport::of(
            steps
                .iter()
                .map(|s| s.failed("GET https://user:secret@example.test/x: timed out".into()))
                .collect(),
        )
    }

    fn fine() -> RunReport {
        RunReport::of(vec![StepReport::Cards(Outcome::Ok { summary: () })])
    }

    /// An alert is written in the webhook's language (`JUDGE_LOCALE`).
    #[test]
    fn alerts_are_written_in_the_webhooks_language() {
        let told = alerts(
            Locale::Stub,
            "bot",
            &RunHistory::default(),
            &failed(&[Step::Cards]),
        );
        assert_eq!(told, vec!["⟦refresh_failed bot cards 0⟧".to_owned()]);
        assert_eq!(panic_alert(Locale::Stub, "bot"), "⟦refresh_panicked bot⟧");
    }

    #[test]
    fn a_failure_is_told_once_per_streak_and_the_recovery_once() {
        let first = alerts(
            Locale::En,
            "bot",
            &h(Some(hours(25)), None, Some(true)),
            &failed(&[Step::Cards]),
        );
        assert_eq!(first.len(), 1);
        assert!(
            first.iter().all(|t| t.contains("`bot`")
                && t.contains("(step cards)")
                && !t.contains("secret")
                && !t.contains("https://")),
            "{first:?}"
        );
        let two = alerts(
            Locale::En,
            "bot",
            &RunHistory::default(),
            &failed(&[Step::Cards, Step::Rules]),
        );
        assert!(
            two.iter().any(|t| t.contains("(steps cards, rules)")),
            "{two:?}"
        );
        let retry = alerts(
            Locale::En,
            "api",
            &h(None, Some(hours(2)), Some(false)),
            &failed(&[Step::Cards]),
        );
        assert!(retry.is_empty(), "a retry that fails again is not news");
        let back = alerts(
            Locale::En,
            "api",
            &h(None, Some(hours(2)), Some(false)),
            &fine(),
        );
        assert_eq!(back.len(), 1);
        assert!(
            back.iter()
                .all(|t| t.contains("succeeded") && t.contains("before it had failed")),
            "{back:?}"
        );
        assert!(alerts(Locale::En, "api", &h(None, None, Some(true)), &fine()).is_empty());
    }

    #[test]
    fn a_time_out_says_so() {
        let dropped = alerts(Locale::En, "bot", &RunHistory::default(), &timed_out(true));
        assert!(
            dropped
                .iter()
                .any(|t| t.contains("timed out: it was dropped after 190 min")),
            "{dropped:?}"
        );
        let mut inner = failed(&[Step::Rules]);
        inner.timed_out = true;
        let told = alerts(Locale::En, "bot", &RunHistory::default(), &inner);
        assert!(
            told.iter()
                .any(|t| t.contains("(step rules), timing out after 3 h")),
            "{told:?}"
        );
    }

    #[test]
    fn a_run_stopped_by_a_schema_change_or_a_shutdown_tells_nobody() {
        for reason in [Skip::SchemaAhead, Skip::Shutdown] {
            let mut steps: Vec<StepReport> = vec![Step::Cards.failed("boom".into())];
            steps.extend(Step::ALL.iter().skip(1).map(|s| s.skipped(reason)));
            let stopped = RunReport::of(steps);
            let expected = if reason == Skip::Shutdown {
                RunOutcome::Stopped
            } else {
                RunOutcome::Failed
            };
            assert_eq!(stopped.outcome(), expected, "{reason:?}");
            let stopped = RunReport::of(Step::ALL.iter().map(|s| s.skipped(reason)).collect());
            assert_eq!(stopped.outcome(), RunOutcome::Stopped);
            let died = RunHistory {
                abandoned_started_at: Some("2026-10-09 05:00Z".into()),
                ..h(Some(hours(30)), Some(hours(4)), Some(false))
            };
            for previous in [
                RunHistory::default(),
                h(None, Some(hours(2)), Some(false)),
                died,
            ] {
                assert!(alerts(Locale::En, "bot", &previous, &stopped).is_empty());
            }
        }
    }

    /// A run a shutdown stopped is no attempt: the next check finds the
    /// refresh due at once, with no backoff for it, whatever came before.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_stopped_run_leaves_the_refresh_due(pool: PgPool) -> Result<()> {
        seed(&pool).await?;
        let row = async |started: &str, finished: Option<&str>, ok: Option<bool>| {
            sqlx::query(
                "INSERT INTO refresh_runs (started_at, finished_at, trigger, process, ok)
                 VALUES (now() - $1::interval, now() - $2::interval, 'schedule', 'bot', $3)",
            )
            .bind(started)
            .bind(finished)
            .bind(ok)
            .execute(&pool)
            .await
        };
        row("30 hours", Some("30 hours"), Some(true)).await?;
        row("2 minutes", Some("1 minute"), None).await?;
        assert_eq!(due(DAY, &runs::history(&pool).await?), Due::Now);
        // A failure three hours ago has waited out its hour; the stopped
        // run after it adds none.
        row("3 hours", Some("3 hours"), Some(false)).await?;
        let h = runs::history(&pool).await?;
        assert_eq!((h.failed_streak, h.last_finished_ok), (1, Some(false)));
        assert_eq!(due(DAY, &h), Due::Now);
        // A run in progress is still an attempt: no second run beside it.
        row("1 minute", None, None).await?;
        assert!(matches!(
            due(DAY, &runs::history(&pool).await?),
            Due::NotYet { .. }
        ));
        Ok(())
    }

    /// A requested shutdown starts no run: not at the check, and not after
    /// the lease was taken, which goes back.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_requested_shutdown_starts_no_run(pool: PgPool) -> Result<()> {
        seed(&pool).await?;
        let shutdown = idle();
        shutdown.request("test");
        let ran = AtomicUsize::new(0);
        let t = tick(
            &Tokio,
            &pool,
            DAY,
            "bot",
            &mut Memory::default(),
            LIMIT,
            &shutdown,
            async |_: &mut RefreshLease| fake(&pool, &ran).await,
        )
        .await;
        assert!(matches!(t, Ticked::ShuttingDown), "{t:?}");
        let t = run_due(
            &Tokio,
            &pool,
            DAY,
            "bot",
            &mut Memory::default(),
            None,
            LIMIT,
            &shutdown,
            async |_: &mut RefreshLease| fake(&pool, &ran).await,
        )
        .await;
        assert!(matches!(t, Ticked::ShuttingDown), "{t:?}");
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert!(
            ingest::try_lease(&pool, "after").await?.is_some(),
            "the lease was released"
        );
        Ok(())
    }

    /// A check waiting on a database that does not answer (here, a table
    /// locked by another session) is dropped at the shutdown request, not
    /// waited for.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_shutdown_ends_a_check_waiting_on_the_database(pool: PgPool) -> Result<()> {
        seed(&pool).await?;
        let mut locker = pool.begin().await?;
        sqlx::query("LOCK TABLE refresh_runs IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *locker)
            .await?;
        let shutdown = idle();
        let ran = AtomicUsize::new(0);
        let mut memory = Memory::default();
        let check = tick(
            &Tokio,
            &pool,
            DAY,
            "bot",
            &mut memory,
            LIMIT,
            &shutdown,
            async |_: &mut RefreshLease| fake(&pool, &ran).await,
        );
        let request = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            shutdown.request("test");
        };
        let (t, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(check, request)
        })
        .await
        .context("the check ending at the request")?;
        assert!(matches!(t, Ticked::ShuttingDown), "{t:?}");
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        locker.rollback().await?;
        Ok(())
    }

    /// A run that ended failed as the shutdown came is still told: the
    /// streak's only alert must not be lost. A run the shutdown stopped is
    /// told nothing.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_failure_finished_at_the_shutdown_is_still_told(pool: PgPool) -> Result<()> {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
        seed(&pool).await?;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let hook = AlertWebhook::mock(&server.uri()).map_err(anyhow::Error::msg)?;
        for (stopped, posts) in [(false, 1), (true, 0)] {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(204))
                .mount(&server)
                .await;
            let shutdown = idle();
            let stop = shutdown.clone();
            let scheduler = spawn(
                (*pool.connect_options()).clone(),
                DAY,
                Some(hook.clone()),
                "alert-test",
                shutdown.clone(),
                AT_ONCE,
                async move |_: &mut RefreshLease| {
                    // The request lands as the run ends, during its writes.
                    stop.request("test");
                    if stopped {
                        RunReport::of(
                            Step::ALL
                                .iter()
                                .map(|s| s.skipped(Skip::Shutdown))
                                .collect(),
                        )
                    } else {
                        failed(&[Step::Cards])
                    }
                },
            );
            let ended = tokio::time::timeout(HANG, scheduler.stopped(&shutdown)).await?;
            assert_eq!(ended, Some(Ended::Shutdown));
            let got = server.received_requests().await.unwrap_or_default();
            assert_eq!(got.len(), posts, "stopped: {stopped}");
            if let Some(body) = got
                .first()
                .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            {
                assert!(body.contains("(step cards)"), "{body}");
            }
        }
        Ok(())
    }

    /// Timing for a thread under test: the first check at once, the next
    /// one never within the test.
    const AT_ONCE: Timing = Timing {
        first: Duration::ZERO,
        every: Duration::from_hours(1),
        jitter: Duration::ZERO,
    };

    /// A shutdown while the scheduler waits for its next check ends the
    /// thread at once, with nothing run.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_shutdown_between_checks_ends_the_thread(pool: PgPool) -> Result<()> {
        let shutdown = idle();
        let ran = std::sync::Arc::new(AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&ran);
        let scheduler = spawn(
            (*pool.connect_options()).clone(),
            DAY,
            None,
            "idle-test",
            shutdown.clone(),
            Timing {
                first: Duration::from_hours(1),
                ..AT_ONCE
            },
            async move |_: &mut RefreshLease| {
                counted.fetch_add(1, Ordering::SeqCst);
                fine()
            },
        );
        shutdown.request("test");
        let ended = tokio::time::timeout(HANG, scheduler.stopped(&shutdown)).await?;
        assert_eq!(ended, Some(Ended::Shutdown));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        Ok(())
    }

    /// The whole stop, on the scheduler's own thread: a step holding
    /// `CALLS_REWRITE_LOCK` with a statement running on the server is cut
    /// off at the shutdown, the run is closed as stopped and the lease
    /// released, the thread ends, and the server ends the abandoned
    /// statement (its client gone, `client_connection_check_interval`), so
    /// the lock the draining answers wait on is free within seconds, not at
    /// `statement_timeout`.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_shutdown_stops_the_run_and_frees_the_calls_lock(pool: PgPool) -> Result<()> {
        seed(&pool).await?;
        let shutdown = idle();
        let (began, beginning) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let began = std::sync::Mutex::new(Some(began));
        let stop = shutdown.clone();
        let refresh = async move |lease: &mut RefreshLease| {
            let step = async |lease: &mut RefreshLease, step: Step| {
                if step != Step::Cards {
                    return step.skipped(Skip::NoEmbedder);
                }
                let held = async {
                    let mut tx = lease.pool().begin().await?;
                    sqlx::query("SELECT pg_advisory_xact_lock($1)")
                        .bind(crate::db::CALLS_REWRITE_LOCK)
                        .execute(&mut *tx)
                        .await?;
                    let tell = began
                        .lock()
                        .map_or_else(|p| p.into_inner().take(), |mut g| g.take());
                    if let Some(tell) = tell {
                        drop(tell.send(Ok(())));
                    }
                    // Holds the lock for ten minutes unless something ends it.
                    sqlx::query("SELECT pg_sleep(600)")
                        .execute(&mut *tx)
                        .await?;
                    tx.commit().await
                }
                .await;
                match held {
                    Ok(()) => StepReport::Cards(Outcome::Ok { summary: () }),
                    Err(e) => step.failed(e.to_string()),
                }
            };
            ingest::refresh_with(
                &Tokio,
                lease,
                Caller::Schedule(&stop),
                ingest::RUN_TIMEOUT,
                step,
            )
            .await
        };
        let options = (*pool.connect_options())
            .clone()
            .application_name("judgebot jobs (stop-test)");
        let scheduler = spawn(
            options,
            DAY,
            None,
            "stop-test",
            shutdown.clone(),
            AT_ONCE,
            refresh,
        );
        tokio::time::timeout(HANG, beginning)
            .await
            .context("the step taking the lock in time")?
            .context("the step began")?
            .map_err(anyhow::Error::msg)?;
        let held: bool = sqlx::query_scalar("SELECT NOT pg_try_advisory_lock($1)")
            .bind(crate::db::CALLS_REWRITE_LOCK)
            .fetch_one(&pool)
            .await?;
        assert!(held, "the step holds the calls lock");

        let asked = std::time::Instant::now();
        shutdown.request("test");
        let ended = tokio::time::timeout(HANG, scheduler.stopped(&shutdown)).await?;
        assert_eq!(ended, Some(Ended::Shutdown));
        assert!(asked.elapsed() < STOP_LIMIT, "{:?}", asked.elapsed());

        // The record: closed, as stopped, the step cut off and the rest
        // not started.
        let (ok, finished, steps): (Option<bool>, bool, serde_json::Value) = sqlx::query_as(
            "SELECT ok, finished_at IS NOT NULL, steps FROM refresh_runs WHERE process = 'stop-test'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!((ok, finished), (None, true));
        let back: Vec<StepReport> = serde_json::from_value(steps)?;
        assert_eq!(
            back.first().and_then(StepReport::skipped),
            Some(Skip::Interrupted)
        );
        assert!(
            back.iter()
                .skip(1)
                .all(|s| s.skipped() == Some(Skip::Shutdown))
        );
        assert!(
            ingest::try_lease(&pool, "after").await?.is_some(),
            "the lease was released"
        );

        // The server ends the abandoned statement once it finds its client
        // gone, and the calls lock with it.
        let freed = async {
            loop {
                let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                    .bind(crate::db::CALLS_REWRITE_LOCK)
                    .fetch_one(&pool)
                    .await?;
                if free {
                    sqlx::query("SELECT pg_advisory_unlock($1)")
                        .bind(crate::db::CALLS_REWRITE_LOCK)
                        .execute(&pool)
                        .await?;
                    return anyhow::Ok(asked.elapsed());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        let after = tokio::time::timeout(Duration::from_secs(20), freed)
            .await
            .context("the calls lock freed in time")??;
        assert!(after < Duration::from_secs(10), "freed after {after:?}");
        let left: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE application_name = 'judgebot jobs (stop-test)'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(left, 0, "every scheduler connection is closed");
        Ok(())
    }

    #[test]
    fn a_run_that_never_finished_is_told_once() {
        let died = RunHistory {
            abandoned_started_at: Some("2026-10-09 05:00Z".into()),
            ..h(Some(hours(30)), Some(hours(4)), Some(false))
        };
        let told = alerts(Locale::En, "api", &died, &failed(&[Step::Cards]));
        assert_eq!(told.len(), 1, "the death, not the retry: {told:?}");
        assert!(
            told.iter()
                .all(|t| t.contains("2026-10-09 05:00Z") && t.contains("never finished"))
        );
        let again = RunHistory {
            failed_streak: 2,
            ..died
        };
        assert!(
            alerts(Locale::En, "api", &again, &failed(&[Step::Cards])).is_empty(),
            "a crash loop is told once"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_schedulers_connections_carry_the_server_side_bounds(pool: PgPool) -> Result<()> {
        let jobs = scheduler_pool((*pool.connect_options()).clone());
        let (statement, lock, check): (String, String, String) = sqlx::query_as(
            "SELECT current_setting('statement_timeout'), current_setting('lock_timeout'),
                    current_setting('client_connection_check_interval')",
        )
        .fetch_one(&jobs)
        .await?;
        assert_eq!(
            (statement.as_str(), lock.as_str(), check.as_str()),
            ("30min", "15min", CLIENT_CHECK_INTERVAL)
        );
        jobs.close().await;
        Ok(())
    }

    #[test]
    fn an_embedding_skipped_at_the_ceiling_is_told_once() {
        let run = RunReport::of(vec![StepReport::Embed(Outcome::Skipped {
            reason: Skip::EmbedCeiling {
                rows: 1912,
                ceiling: 800,
            },
        })]);
        let previous = h(Some(hours(25)), None, Some(true));
        let told = alerts(Locale::En, "api", &previous, &run);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told.iter()
                .all(|t| t.contains("1912") && t.contains("judgebot ingest embed"))
        );
        let again = RunHistory {
            last_finished_ceiling: true,
            ..previous
        };
        assert!(
            alerts(Locale::En, "api", &again, &run).is_empty(),
            "already told"
        );
    }

    /// The rules the schedule needs to see before it runs anything.
    async fn seed(pool: &PgPool) -> Result<()> {
        sqlx::query(
            "INSERT INTO rules (id, subsection, body, cr_version) VALUES ('100.1', '100', 'x', '20260101')",
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    /// A fake refresh: counts its calls and records a successful run, as the
    /// real one does, holding the lease a moment so a racing check meets it.
    async fn fake(pool: &PgPool, ran: &AtomicUsize) -> RunReport {
        ran.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let recorded = sqlx::query(
            "INSERT INTO refresh_runs (trigger, process, finished_at, ok) VALUES ('schedule', 'test', now(), true)",
        )
        .execute(pool)
        .await;
        assert!(recorded.is_ok(), "{recorded:?}");
        fine()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn two_racing_checks_run_the_refresh_once(pool: PgPool) -> Result<()> {
        seed(&pool).await?;
        let ran = AtomicUsize::new(0);
        let (mut a, mut b) = (Memory::default(), Memory::default());
        let idle = idle();
        let (x, y) = tokio::join!(
            tick(
                &Tokio,
                &pool,
                DAY,
                "a",
                &mut a,
                LIMIT,
                &idle,
                async |_: &mut RefreshLease| { fake(&pool, &ran).await }
            ),
            tick(
                &Tokio,
                &pool,
                DAY,
                "b",
                &mut b,
                LIMIT,
                &idle,
                async |_: &mut RefreshLease| { fake(&pool, &ran).await }
            ),
        );
        assert_eq!(ran.load(Ordering::SeqCst), 1, "{x:?} / {y:?}");
        let ran_count = [&x, &y]
            .iter()
            .filter(|t| matches!(t, Ticked::Ran { .. }))
            .count();
        assert_eq!(ran_count, 1);
        assert!(
            [&x, &y]
                .iter()
                .any(|t| matches!(t, Ticked::Busy | Ticked::NotDue)),
            "{x:?} / {y:?}"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_check_under_a_held_lease_skips(pool: PgPool) -> Result<()> {
        seed(&pool).await?;
        let held = ingest::try_lease(&pool, "cron").await?.context("free")?;
        let ran = AtomicUsize::new(0);
        let t = tick(
            &Tokio,
            &pool,
            DAY,
            "bot",
            &mut Memory::default(),
            LIMIT,
            &idle(),
            async |_: &mut RefreshLease| fake(&pool, &ran).await,
        )
        .await;
        assert!(matches!(t, Ticked::Busy), "{t:?}");
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        held.release().await;
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_check_after_a_fresh_success_does_nothing(pool: PgPool) -> Result<()> {
        seed(&pool).await?;
        sqlx::query(
            "INSERT INTO refresh_runs (trigger, process, finished_at, ok) VALUES ('manual', 'ingest', now(), true)",
        )
        .execute(&pool)
        .await?;
        let ran = AtomicUsize::new(0);
        let t = tick(
            &Tokio,
            &pool,
            DAY,
            "bot",
            &mut Memory::default(),
            LIMIT,
            &idle(),
            async |_: &mut RefreshLease| fake(&pool, &ran).await,
        )
        .await;
        assert!(matches!(t, Ticked::NotDue), "{t:?}");
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_database_with_no_rules_waits_for_its_first_load(pool: PgPool) -> Result<()> {
        let ran = AtomicUsize::new(0);
        let mut memory = Memory::default();
        let t = tick(
            &Tokio,
            &pool,
            DAY,
            "bot",
            &mut memory,
            LIMIT,
            &idle(),
            async |_: &mut RefreshLease| fake(&pool, &ran).await,
        )
        .await;
        assert!(matches!(t, Ticked::Uninitialised), "{t:?}");
        assert_eq!(
            memory.paused,
            Some(Pause::Uninitialised),
            "warned once, remembered"
        );
        seed(&pool).await?;
        let t = tick(
            &Tokio,
            &pool,
            DAY,
            "bot",
            &mut memory,
            LIMIT,
            &idle(),
            async |_: &mut RefreshLease| fake(&pool, &ran).await,
        )
        .await;
        assert!(matches!(t, Ticked::Ran { .. }), "{t:?}");
        assert_eq!(memory.paused, None);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_schema_from_a_newer_release_is_never_written(pool: PgPool) -> Result<()> {
        seed(&pool).await?;
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES ($1, 'future', true, $2, 0)",
        )
        .bind(99_990_101_000_001_i64)
        .bind(&b"future"[..])
        .execute(&pool)
        .await?;
        let ran = AtomicUsize::new(0);
        let mut memory = Memory::default();
        for _ in 0..2 {
            let t = tick(
                &Tokio,
                &pool,
                DAY,
                "bot",
                &mut memory,
                LIMIT,
                &idle(),
                async |_: &mut RefreshLease| fake(&pool, &ran).await,
            )
            .await;
            assert!(matches!(t, Ticked::Skewed), "{t:?}");
        }
        assert_eq!(memory.paused, Some(Pause::Ahead), "warned once, remembered");
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        Ok(())
    }

    /// No `refresh_runs` (a schema without it): every check reads again,
    /// warns once, and writes nothing; a schema with it pending is `Skewed`
    /// by the ledger before the record is read at all.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_missing_record_pauses_the_schedule_without_failing(pool: PgPool) -> Result<()> {
        sqlx::query("DROP TABLE refresh_runs")
            .execute(&pool)
            .await?;
        let ran = AtomicUsize::new(0);
        let mut memory = Memory::default();
        for _ in 0..2 {
            let t = tick(
                &Tokio,
                &pool,
                DAY,
                "bot",
                &mut memory,
                LIMIT,
                &idle(),
                async |_: &mut RefreshLease| fake(&pool, &ran).await,
            )
            .await;
            assert!(matches!(t, Ticked::Unreadable), "{t:?}");
        }
        assert_eq!(memory.paused, Some(Pause::Missing));
        sqlx::query("DELETE FROM _sqlx_migrations WHERE description LIKE '%refresh runs%'")
            .execute(&pool)
            .await?;
        let t = tick(
            &Tokio,
            &pool,
            DAY,
            "bot",
            &mut memory,
            LIMIT,
            &idle(),
            async |_: &mut RefreshLease| fake(&pool, &ran).await,
        )
        .await;
        assert!(matches!(t, Ticked::Skewed), "{t:?}");
        assert_eq!(memory.paused, Some(Pause::Behind));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_due_check_runs_the_refresh_and_hands_back_the_record_before_it(
        pool: PgPool,
    ) -> Result<()> {
        seed(&pool).await?;
        sqlx::query(
            "INSERT INTO refresh_runs (started_at, finished_at, trigger, process, ok)
             VALUES (now() - interval '30 hours', now() - interval '30 hours', 'schedule', 'api', false)",
        )
        .execute(&pool)
        .await?;
        let ran = AtomicUsize::new(0);
        let t = tick(
            &Tokio,
            &pool,
            DAY,
            "bot",
            &mut Memory::default(),
            LIMIT,
            &idle(),
            async |_: &mut RefreshLease| fake(&pool, &ran).await,
        )
        .await;
        let Ticked::Ran { previous, .. } = t else {
            anyhow::bail!("expected a run, got {t:?}");
        };
        assert_eq!(
            (previous.last_finished_ok, previous.failed_streak),
            (Some(false), 1)
        );
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert!(
            ingest::try_lease(&pool, "after").await?.is_some(),
            "the lease was released"
        );
        Ok(())
    }

    /// A run that finished between a check's first read and its lease:
    /// the read under the lease sees it, and nothing runs.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_run_done_while_the_lease_was_taken_is_not_repeated(pool: PgPool) -> Result<()> {
        seed(&pool).await?;
        // Due when the check first read the record...
        let before = runs::history(&pool).await?;
        assert_eq!(due(DAY, &before), Due::Now);
        // ...and done by another process before it took the lease.
        sqlx::query(
            "INSERT INTO refresh_runs (trigger, process, finished_at, ok) VALUES ('manual', 'ingest', now(), true)",
        )
        .execute(&pool)
        .await?;
        let ran = AtomicUsize::new(0);
        let t = run_due(
            &Tokio,
            &pool,
            DAY,
            "bot",
            &mut Memory::default(),
            None,
            LIMIT,
            &idle(),
            async |_: &mut RefreshLease| fake(&pool, &ran).await,
        )
        .await;
        assert!(matches!(t, Ticked::NotDue), "{t:?}");
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert!(
            ingest::try_lease(&pool, "after").await?.is_some(),
            "the lease was released"
        );
        Ok(())
    }

    /// [`tick`] on `clock` with a run that never ends, the clock moved to
    /// the run's limit once `begun` resolves and the run waits on it.
    async fn at_the_limit<F>(
        clock: &Manual,
        pool: &PgPool,
        memory: &mut Memory,
        begun: impl Future<Output = Result<()>>,
        run: F,
    ) -> Result<Ticked>
    where
        F: AsyncFnOnce(&mut RefreshLease) -> RunReport,
    {
        let limit = clock.elapsed() + LIMIT;
        let idle = idle();
        let mut ticked = std::pin::pin!(tick(clock, pool, DAY, "bot", memory, LIMIT, &idle, run));
        let drive = async {
            tokio::time::timeout(HANG, begun)
                .await
                .context("the run beginning in time")??;
            clock.parked(&[limit]).await?;
            clock.advance_to(limit);
            anyhow::Ok(())
        };
        tokio::select! {
            t = &mut ticked => anyhow::bail!("the check ended before the run's limit: {t:?}"),
            driven = drive => driven?,
        }
        Ok(tokio::time::timeout(HANG, ticked).await?)
    }

    /// A dropped run that had begun its row gets it closed as failed, so the
    /// record does not read as a process that died.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_dropped_runs_row_is_closed_as_failed(pool: PgPool) -> Result<()> {
        seed(&pool).await?;
        let (row, begun) = tokio::sync::oneshot::channel();
        let hang = async move |lease: &mut RefreshLease| {
            let inserted = sqlx::query(
                "INSERT INTO refresh_runs (trigger, process) VALUES ('schedule', 'bot')",
            )
            .execute(lease.pool())
            .await;
            drop(row.send(inserted.map(|_| ())));
            std::future::pending::<RunReport>().await
        };
        let begun = async { Ok(begun.await.context("the run began")??) };
        let clock = Manual::new();
        let t = at_the_limit(&clock, &pool, &mut Memory::default(), begun, hang).await?;
        let Ticked::Ran { report, .. } = t else {
            anyhow::bail!("expected a run, got {t:?}");
        };
        assert!(report.timed_out && report.recorded);
        let (ok, finished): (Option<bool>, bool) =
            sqlx::query_as("SELECT ok, finished_at IS NOT NULL FROM refresh_runs")
                .fetch_one(&pool)
                .await?;
        assert_eq!((ok, finished), (Some(false), true));
        let h = runs::history(&pool).await?;
        assert_eq!((h.failed_streak, h.abandoned_started_at), (1, None));
        Ok(())
    }

    /// A run that hangs past the limit is dropped, frees the lease, and is
    /// remembered as an attempt, so the next check does not start another.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_hung_run_is_dropped_and_backed_off(pool: PgPool) -> Result<()> {
        seed(&pool).await?;
        let mut memory = Memory::default();
        let hang = async |_: &mut RefreshLease| std::future::pending::<RunReport>().await;
        let clock = Manual::new();
        let t = at_the_limit(&clock, &pool, &mut memory, async { Ok(()) }, hang).await?;
        let Ticked::Ran { report, secs, .. } = t else {
            anyhow::bail!("expected a run, got {t:?}");
        };
        assert!(report.timed_out && !report.ok());
        assert_eq!(secs, LIMIT.as_secs(), "it ran for its limit");
        // The fake wrote no row, so there was none to close: remembered.
        assert!(!report.recorded && memory.unrecorded.is_some());
        let ran = AtomicUsize::new(0);
        let t = tick(
            &clock,
            &pool,
            DAY,
            "bot",
            &mut memory,
            LIMIT,
            &idle(),
            async |_: &mut RefreshLease| fake(&pool, &ran).await,
        )
        .await;
        assert!(matches!(t, Ticked::NotDue), "{t:?}");
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert!(
            ingest::try_lease(&pool, "after").await?.is_some(),
            "the lease was released"
        );
        Ok(())
    }
}
