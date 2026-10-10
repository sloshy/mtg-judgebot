//! What a long-running process does beside answering questions: today, the
//! scheduled data refresh ([`crate::ingest::refresh`]) every
//! `JUDGE_REFRESH_HOURS`, so an instance stays current with no host scheduler.
//! `judgebot --jobs` calls [`start`] (as do the compatibility names
//! `judge-bot` and `judge-api`); the record and the lease in Postgres make
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
//! **Alerts** go to `JUDGE_ALERT_WEBHOOK` ([`alerts`]): the first failure of
//! a streak (a time-out says so), the recovery after one, a run that left rows
//! unembedded because there were more than it may pay for unattended
//! ([`ingest::embed::UNATTENDED_CEILING`]) when the run before it did not, and
//! the first panicking check until a run succeeds. A retry that fails again is
//! not news, so it is only logged.

use std::{num::NonZeroU16, panic::AssertUnwindSafe, time::Duration};

use judge_llm::SpendMeter;
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::time::Instant;

use crate::{
    alert::{self, AlertWebhook},
    clock::{Clock, Tokio},
    db::migrate,
    ingest::{
        self, RefreshLease,
        runs::{self, Outcome, RunHistory, RunOutcome, RunReport, Skip, StepReport, Trigger},
    },
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
/// A run that stopped because the schema is not its binary's (a deploy in
/// progress) is news to nobody: it alerts nothing. The texts name steps and
/// counts, never an error message: those can carry URLs, and the log has them.
#[must_use]
pub fn alerts(process: &str, previous: &RunHistory, report: &RunReport) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(started) = &previous.abandoned_started_at
        && previous.failed_streak == 1
    {
        out.push(format!(
            "judgebot: a data refresh started at {started} never finished; its process may have \
             died (out of memory?) or lost the database. `{process}` is running the next one."
        ));
    }
    let failed_before = previous.last_finished_ok == Some(false);
    match report.outcome() {
        RunOutcome::Ok if failed_before => out.push(format!(
            "judgebot `{process}`: a scheduled data refresh succeeded. The refresh before it had \
             failed."
        )),
        RunOutcome::Failed if !failed_before => {
            let steps: Vec<&str> = report.failed().iter().map(|s| s.name()).collect();
            let what = match (steps.as_slice(), report.timed_out) {
                ([], true) => format!(
                    "timed out: it was dropped after {} min",
                    runs::ABANDONED_AFTER.as_secs() / 60
                ),
                ([one], timed_out) => format!(
                    "failed (step {one}){}",
                    if timed_out {
                        timed_out_text()
                    } else {
                        String::new()
                    }
                ),
                (many, timed_out) => format!(
                    "failed (steps {}){}",
                    many.join(", "),
                    if timed_out {
                        timed_out_text()
                    } else {
                        String::new()
                    }
                ),
            };
            out.push(format!(
                "judgebot `{process}`: a scheduled data refresh {what}. It is retried after an \
                 hour, then less often while it keeps failing; the log has each error \
                 (`refresh step failed`). You will be told again when it succeeds, not on each \
                 retry."
            ));
        }
        RunOutcome::Ok | RunOutcome::Failed | RunOutcome::Stopped => {}
    }
    if !previous.last_finished_ceiling {
        for step in &report.steps {
            if let StepReport::Embed(Outcome::Skipped {
                reason: Skip::EmbedCeiling { rows, ceiling },
            }) = step
            {
                out.push(format!(
                    "judgebot `{process}`: a scheduled data refresh did not embed {rows} rows, \
                     more than the {ceiling} it embeds unattended. If that spend is expected (a \
                     new embedder, an interrupted reembed), run `judgebot ingest embed` \
                     (`docker compose run --rm refresh embed`); vector search misses those rows \
                     until then."
                ));
            }
        }
    }
    out
}

/// The time-out clause of a failure alert.
fn timed_out_text() -> String {
    format!(
        ", timing out after {} h; the steps after it were not run",
        ingest::RUN_TIMEOUT.as_secs() / 3600
    )
}

/// What the webhook is told the first time a check panics.
#[must_use]
pub fn panic_alert(process: &str) -> String {
    format!(
        "judgebot `{process}`: the scheduled data refresh check crashed (a panic; the log has \
         it). It is tried again at every check; you will not be told again until a refresh \
         succeeds."
    )
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
/// how long it may take ([`runs::ABANDONED_AFTER`]), on `clock`.
async fn tick<F>(
    clock: &impl Clock,
    pool: &PgPool,
    every: Hours,
    process: &'static str,
    memory: &mut Memory,
    limit: Duration,
    run: F,
) -> Ticked
where
    F: AsyncFnOnce(&mut RefreshLease) -> RunReport,
{
    match migrate::skew(pool).await {
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
    let Some(history) = read_history(pool, memory).await else {
        return Ticked::Unreadable;
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
    run_due(clock, pool, every, process, memory, since, limit, run).await
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
    run: F,
) -> Ticked
where
    F: AsyncFnOnce(&mut RefreshLease) -> RunReport,
{
    let mut lease = match ingest::try_lease(pool, process).await {
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
    let previous = match read_history(pool, memory).await {
        Some(h) if due(every, &with_unrecorded(h.clone(), since)) == Due::Now => h,
        Some(_) => {
            lease.release().await;
            tracing::debug!("scheduled refresh: another run has just done it");
            return Ticked::NotDue;
        }
        None => {
            lease.release().await;
            return Ticked::Unreadable;
        }
    };
    tracing::info!(
        trigger = Trigger::Schedule.as_str(),
        process,
        last_ok = %ago(previous.last_ok_age_secs),
        "refresh starting"
    );
    let started = clock.now();
    // The database's clock at the start, so a dropped run's row can be found.
    let since = runs::db_now(pool).await.ok();
    let report = if let Ok(report) = clock.timeout(limit, run(&mut lease)).await {
        lease.release().await;
        report
    } else {
        // Dropped mid-step: its transaction rolls back unless it was
        // committing. Under the lease still, the row it began is closed as
        // failed, so the record does not read as a process that died; then
        // the lease goes, bounded, because its connection may be what hung.
        let closed = match &since {
            Some(since) => clock
                .timeout(AFTER_DROP, runs::close_dropped(pool, since))
                .await
                .is_ok_and(|r| r.unwrap_or(false)),
            None => false,
        };
        if clock.timeout(AFTER_DROP, lease.release()).await.is_err() {
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

/// A random part of [`JITTER`]. A v4 UUID is the randomness the crate already
/// has; this needs no more than that.
fn jitter() -> Duration {
    let max = JITTER.as_millis().max(1);
    let ms = uuid::Uuid::new_v4().as_u128() % max;
    Duration::from_millis(u64::try_from(ms).unwrap_or_default())
}

/// How the scheduler thread ended. It never ends on its own: each of these
/// is a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The thread could not be started at all.
    NotStarted,
    /// Its runtime could not be built, or its loop returned.
    Stopped,
    /// It panicked outside a check (a check's own panic is caught and the
    /// loop goes on).
    Panicked,
}

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
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
}

/// Start the jobs `jobs` asks for on a thread of their own; see the module
/// docs. `process` names the caller in the record, the lease and an alert
/// (`judgebot`, or `bot`/`api` under the compatibility names). With the
/// schedule off this logs one line, starts nothing and returns `None`.
///
/// The record is read once here, on the caller's pool, so the startup line
/// sits beside the configuration summary; everything after runs on the
/// scheduler's own runtime and pool. The refresh's embedding step bills to
/// `meter`, the process's one, so `JUDGE_MAX_USD` caps it with the questions
/// and the spend ledger records it.
pub async fn start(
    pool: &PgPool,
    jobs: Jobs,
    meter: SpendMeter,
    process: &'static str,
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
    let alert = jobs.alert;
    let (tx, rx) = tokio::sync::oneshot::channel();
    // The sender goes back if the thread cannot be spawned, so the caller
    // still hears about it.
    let tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
    let thread_tx = std::sync::Arc::clone(&tx);
    let spawned = std::thread::Builder::new()
        .name(THREAD_NAME.to_owned())
        .spawn(move || {
            let ended = run_thread(options, every, alert, &meter, process);
            tell(&thread_tx, ended);
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "could not start the scheduler thread; no scheduled refresh in this process");
        tell(&tx, Ended::NotStarted);
    }
    Some(Scheduler(rx))
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
/// [`schedule_loop`], which never returns, so getting past it is a failure.
fn run_thread(
    options: PgConnectOptions,
    every: Hours,
    alert: Option<AlertWebhook>,
    meter: &SpendMeter,
    process: &'static str,
) -> Ended {
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(error = %e, "building the scheduler's runtime");
                return;
            }
        };
        let local = tokio::task::LocalSet::new();
        local.block_on(
            &runtime,
            schedule_loop(options, every, alert, meter, process),
        );
    }));
    let ended = if outcome.is_err() {
        Ended::Panicked
    } else {
        Ended::Stopped
    };
    tracing::error!(
        "the scheduler thread {ended}: no scheduled refresh until this process restarts \
         (serving roles keep answering; `judgebot ingest refresh` still works)"
    );
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

/// Check, sleep, repeat. Each check is its own task, so a panic in one is
/// logged and the loop goes on.
async fn schedule_loop(
    options: PgConnectOptions,
    every: Hours,
    alert: Option<AlertWebhook>,
    meter: &SpendMeter,
    process: &'static str,
) {
    let pool = PgPoolOptions::new()
        .max_connections(POOL_SIZE)
        .min_connections(0)
        .idle_timeout(POOL_IDLE)
        .connect_lazy_with(bounded(options));
    let client = alert::client();
    let cache_dir = ingest::cache_dir();
    let mut memory = Memory::default();
    // Outside the task, so a panic cannot lose it.
    let mut panic_told = false;
    tokio::time::sleep(FIRST_CHECK + jitter()).await;
    loop {
        let check = tokio::task::spawn_local({
            let (pool, dir, meter) = (pool.clone(), cache_dir.clone(), meter.clone());
            async move {
                let refresh = async move |lease: &mut RefreshLease| {
                    ingest::refresh(lease, &dir, Trigger::Schedule, &meter).await
                };
                let limit = runs::ABANDONED_AFTER;
                let ticked = tick(&Tokio, &pool, every, process, &mut memory, limit, refresh).await;
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
                    send(alert.as_ref(), &client, alerts(process, &previous, &report)).await;
                }
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "the scheduled refresh check panicked; the next check runs as scheduled"
                );
                if !std::mem::replace(&mut panic_told, true) {
                    send(alert.as_ref(), &client, vec![panic_alert(process)]).await;
                }
            }
        }
        tokio::time::sleep(CHECK_EVERY + jitter()).await;
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
        RunOutcome::Stopped => tracing::info!(
            trigger,
            process,
            secs,
            "refresh stopped before writing: the schema is not this binary's"
        ),
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

    #[test]
    fn a_failure_is_told_once_per_streak_and_the_recovery_once() {
        let first = alerts(
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
            "bot",
            &RunHistory::default(),
            &failed(&[Step::Cards, Step::Rules]),
        );
        assert!(
            two.iter().any(|t| t.contains("(steps cards, rules)")),
            "{two:?}"
        );
        let retry = alerts(
            "api",
            &h(None, Some(hours(2)), Some(false)),
            &failed(&[Step::Cards]),
        );
        assert!(retry.is_empty(), "a retry that fails again is not news");
        let back = alerts("api", &h(None, Some(hours(2)), Some(false)), &fine());
        assert_eq!(back.len(), 1);
        assert!(
            back.iter()
                .all(|t| t.contains("succeeded") && t.contains("before it had failed")),
            "{back:?}"
        );
        assert!(alerts("api", &h(None, None, Some(true)), &fine()).is_empty());
    }

    #[test]
    fn a_time_out_says_so() {
        let dropped = alerts("bot", &RunHistory::default(), &timed_out(true));
        assert!(
            dropped
                .iter()
                .any(|t| t.contains("timed out: it was dropped after 190 min")),
            "{dropped:?}"
        );
        let mut inner = failed(&[Step::Rules]);
        inner.timed_out = true;
        let told = alerts("bot", &RunHistory::default(), &inner);
        assert!(
            told.iter()
                .any(|t| t.contains("(step rules), timing out after 3 h")),
            "{told:?}"
        );
    }

    #[test]
    fn a_run_stopped_by_a_schema_change_tells_nobody() {
        let stopped = RunReport::of(
            Step::ALL
                .iter()
                .map(|s| s.skipped(Skip::SchemaAhead))
                .collect(),
        );
        assert_eq!(stopped.outcome(), RunOutcome::Stopped);
        for previous in [RunHistory::default(), h(None, Some(hours(2)), Some(false))] {
            assert!(alerts("bot", &previous, &stopped).is_empty());
        }
    }

    #[test]
    fn a_run_that_never_finished_is_told_once() {
        let died = RunHistory {
            abandoned_started_at: Some("2026-10-09 05:00Z".into()),
            ..h(Some(hours(30)), Some(hours(4)), Some(false))
        };
        let told = alerts("api", &died, &failed(&[Step::Cards]));
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
            alerts("api", &again, &failed(&[Step::Cards])).is_empty(),
            "a crash loop is told once"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_schedulers_connections_carry_the_server_side_bounds(pool: PgPool) -> Result<()> {
        let options = bounded((*pool.connect_options()).clone());
        let mut conn = <sqlx::PgConnection as sqlx::Connection>::connect_with(&options).await?;
        let (statement, lock): (String, String) = sqlx::query_as(
            "SELECT current_setting('statement_timeout'), current_setting('lock_timeout')",
        )
        .fetch_one(&mut conn)
        .await?;
        assert_eq!((statement.as_str(), lock.as_str()), ("30min", "15min"));
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
        let told = alerts("api", &previous, &run);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told.iter()
                .all(|t| t.contains("1912") && t.contains("judgebot ingest embed"))
        );
        let again = RunHistory {
            last_finished_ceiling: true,
            ..previous
        };
        assert!(alerts("api", &again, &run).is_empty(), "already told");
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
        let (x, y) = tokio::join!(
            tick(
                &Tokio,
                &pool,
                DAY,
                "a",
                &mut a,
                LIMIT,
                async |_: &mut RefreshLease| { fake(&pool, &ran).await }
            ),
            tick(
                &Tokio,
                &pool,
                DAY,
                "b",
                &mut b,
                LIMIT,
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
        let mut ticked = std::pin::pin!(tick(clock, pool, DAY, "bot", memory, LIMIT, run));
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
