//! The run record: what each [`super::refresh`] did, in `refresh_runs`.
//!
//! A run inserts its row when it starts ([`begin`]) and fills it in when it
//! ends ([`finish`]): the finish time, the stored CR version after it, each
//! step's [`StepReport`] and its [`RunOutcome`] in `ok`: true, false, or null
//! for a run that stopped because the schema is not its binary's (neither a
//! success nor a failure). A row with no `finished_at` is a run in progress or
//! one whose process died; once it is older than [`ABANDONED_AFTER`],
//! [`history`] counts it as a failure. A run the scheduler dropped at that
//! limit is closed by it ([`close_dropped`]) as failed, with no steps. Times are the
//! database's clock (`now()`), and every age is computed there too
//! ([`history`]), so every process agrees on them whatever its own clock says.
//!
//! The record is bookkeeping, never a gate: a failure to write it is logged
//! and the steps run regardless. That includes a database whose schema
//! predates the table (the new image's `judgebot ingest refresh` run before
//! anything migrated, or `JUDGE_AUTO_MIGRATE=false`): the run warns once and
//! goes unrecorded.
//!
//! # Stored shape
//!
//! `steps` is a JSON array, one object per step in the order run, tagged by
//! `step` and `outcome` (it is stored, so the tags are explicit and the shape
//! is pinned by a test):
//!
//! ```json
//! [{"step": "cards",  "outcome": "ok", "summary": null},
//!  {"step": "rules",  "outcome": "ok", "summary": {"cr": "updated", "version": "20260819", "url": "https://…"}},
//!  {"step": "retire", "outcome": "ok", "summary": {"checked": 40, "retired": 1, "restored": 0, "still_retired": 2}},
//!  {"step": "embed",  "outcome": "skipped", "reason": "no_embedder"},
//!  {"step": "emoji",  "outcome": "failed", "error": "GET https://api.scryfall.com/symbology: …"}]
//! ```
//!
//! `judgebot ingest init` records the refresh steps it ran the same way, and
//! the ones it did not reach after a failure as skipped with
//! `"reason": "init_stopped"`.
//!
//! A scheduled run that finds more empty vectors than it may embed
//! unattended skips `embed` with
//! `{"reason": {"embed_ceiling": {"rows": 1912, "ceiling": 800}}}`.
//!
//! A rules summary is `{"cr": "unchanged", "version"}` when the published
//! release was already loaded; an embed summary is rows embedded per table
//! (`{"rules": 12, "glossary": 0, "calls": 3}`); an emoji summary is
//! `{"uploaded", "skipped", "unusable", "failed"}` counts.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use judge_core::{CrVersion, Freshness};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::{cr, emoji};
use crate::db::RetireSummary;

/// What started a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// A long-running process's own schedule.
    Schedule,
    /// A command: `judgebot ingest refresh`, by hand or from cron.
    Manual,
}

impl Trigger {
    /// The stored value (the table's check constraint lists both).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Schedule => "schedule",
            Self::Manual => "manual",
        }
    }
}

/// A step of [`super::refresh`], in the order it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The Scryfall sync.
    Cards,
    /// The current Comprehensive Rules release, if new.
    Rules,
    /// The retirement pass over stored calls.
    Retire,
    /// Vectors for every empty row.
    Embed,
    /// New card-symbol emoji for the Discord application.
    Emoji,
}

impl Step {
    /// Every step, in the order [`super::refresh`] runs them.
    pub const ALL: [Self; 5] = [
        Self::Cards,
        Self::Rules,
        Self::Retire,
        Self::Embed,
        Self::Emoji,
    ];

    /// This step's report as skipped for `reason`, for a step a run did not
    /// start (the schema is not its binary's, or the run timed out).
    #[must_use]
    pub const fn skipped(self, reason: Skip) -> StepReport {
        match self {
            Self::Cards => StepReport::Cards(Outcome::Skipped { reason }),
            Self::Rules => StepReport::Rules(Outcome::Skipped { reason }),
            Self::Retire => StepReport::Retire(Outcome::Skipped { reason }),
            Self::Embed => StepReport::Embed(Outcome::Skipped { reason }),
            Self::Emoji => StepReport::Emoji(Outcome::Skipped { reason }),
        }
    }

    /// This step's report as failed with `error`, for a step a run could not
    /// start (its lease was lost).
    #[must_use]
    pub const fn failed(self, error: String) -> StepReport {
        match self {
            Self::Cards => StepReport::Cards(Outcome::Failed { error }),
            Self::Rules => StepReport::Rules(Outcome::Failed { error }),
            Self::Retire => StepReport::Retire(Outcome::Failed { error }),
            Self::Embed => StepReport::Embed(Outcome::Failed { error }),
            Self::Emoji => StepReport::Emoji(Outcome::Failed { error }),
        }
    }

    /// The name logged and stored.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cards => "cards",
            Self::Rules => "rules",
            Self::Retire => "retire",
            Self::Embed => "embed",
            Self::Emoji => "emoji",
        }
    }
}

/// Why a step did not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Skip {
    /// The emoji belong to the bot's Discord application; a deployment with
    /// no `DISCORD_TOKEN` has nothing to upload them to.
    NoDiscordToken,
    /// Neither `[models.embed]` nor `VOYAGE_API_KEY` names an embedder.
    NoEmbedder,
    /// A scheduled run found more rows to embed than it may pay for unattended
    /// ([`super::embed::UNATTENDED_CEILING`]): that many empty vectors means
    /// something other than a new CR emptied them, and `judgebot ingest embed`
    /// by hand is the operator's call.
    EmbedCeiling {
        /// Rows waiting for a vector.
        rows: u64,
        /// The ceiling they were over.
        ceiling: u64,
    },
    /// A newer release migrated the database during or before the run; this
    /// binary does not write into a schema it does not know. The run stops
    /// ([`RunOutcome::Stopped`]).
    SchemaAhead,
    /// Migrations this binary carries are pending (`judgebot ingest migrate`).
    /// The run stops ([`RunOutcome::Stopped`]).
    SchemaBehind,
    /// An earlier step passed the run's time limit ([`super::RUN_TIMEOUT`]),
    /// so this one was not started.
    RunTimedOut,
    /// `judgebot ingest init` stops at its first failure (each step needs the
    /// one before it), so this one was not started.
    InitStopped,
}

impl Skip {
    /// Whether this skip stops the run without failing it: the schema is not
    /// the binary's.
    #[must_use]
    pub const fn stops(self) -> bool {
        matches!(self, Self::SchemaAhead | Self::SchemaBehind)
    }
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoDiscordToken => "DISCORD_TOKEN is not set",
            Self::NoEmbedder => "no embedder configured (VOYAGE_API_KEY or [models.embed])",
            Self::SchemaAhead => {
                "a newer release migrated the database, and this binary does not write into a \
                 schema it does not know: run the newer image"
            }
            Self::SchemaBehind => "migrations are pending: run `judgebot ingest migrate`",
            Self::RunTimedOut => "not run: the refresh timed out at an earlier step",
            Self::InitStopped => "not run: init stops at its first failure",
            Self::EmbedCeiling { rows, ceiling } => {
                return write!(
                    f,
                    "{rows} rows to embed, over the {ceiling} a scheduled run embeds unattended; \
                     run `judgebot ingest embed` by hand"
                );
            }
        })
    }
}

/// How one step ended, with `T` the step's own summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome<T> {
    /// It ran to completion.
    Ok {
        /// What it did.
        summary: T,
    },
    /// It did not run.
    Skipped {
        /// Why not.
        reason: Skip,
    },
    /// It failed; the later steps still ran.
    Failed {
        /// The error and its causes (`{:#}`).
        error: String,
    },
}

impl<T> Outcome<T> {
    /// `Ok` or `Failed` from a step's result.
    pub fn of(result: Result<T>) -> Self {
        match result {
            Ok(summary) => Self::Ok { summary },
            Err(e) => Self::Failed {
                error: format!("{e:#}"),
            },
        }
    }

    const fn failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    const fn skip(&self) -> Option<Skip> {
        match self {
            Self::Skipped { reason } => Some(*reason),
            Self::Ok { .. } | Self::Failed { .. } => None,
        }
    }
}

impl<T: Serialize> Outcome<T> {
    /// One line per step; operators grep for `refresh step ok`.
    fn log(&self, step: Step) {
        let step = step.name();
        match self {
            Self::Ok { summary } => {
                let summary = serde_json::to_string(summary).unwrap_or_default();
                tracing::info!(step, %summary, "refresh step ok");
            }
            Self::Skipped { reason } => tracing::warn!(step, %reason, "refresh step skipped"),
            Self::Failed { error } => tracing::error!(step, %error, "refresh step failed"),
        }
    }
}

/// Rows embedded per table.
pub type Embedded = BTreeMap<String, usize>;

/// One step's outcome, keyed by the step so a summary can only belong to its
/// own step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum StepReport {
    /// The Scryfall sync reports nothing beyond success.
    Cards(Outcome<()>),
    /// Whether a new CR was loaded.
    Rules(Outcome<cr::Outcome>),
    /// What the retirement pass changed.
    Retire(Outcome<RetireSummary>),
    /// Rows embedded per table.
    Embed(Outcome<Embedded>),
    /// What the emoji upload did.
    Emoji(Outcome<emoji::Summary>),
}

impl StepReport {
    /// Which step this is.
    #[must_use]
    pub const fn step(&self) -> Step {
        match self {
            Self::Cards(_) => Step::Cards,
            Self::Rules(_) => Step::Rules,
            Self::Retire(_) => Step::Retire,
            Self::Embed(_) => Step::Embed,
            Self::Emoji(_) => Step::Emoji,
        }
    }

    /// Why the step was skipped, if it was.
    #[must_use]
    pub const fn skipped(&self) -> Option<Skip> {
        match self {
            Self::Cards(o) => o.skip(),
            Self::Rules(o) => o.skip(),
            Self::Retire(o) => o.skip(),
            Self::Embed(o) => o.skip(),
            Self::Emoji(o) => o.skip(),
        }
    }

    /// Whether the step failed (a skipped step did not).
    #[must_use]
    pub const fn failed(&self) -> bool {
        match self {
            Self::Cards(o) => o.failed(),
            Self::Rules(o) => o.failed(),
            Self::Retire(o) => o.failed(),
            Self::Embed(o) => o.failed(),
            Self::Emoji(o) => o.failed(),
        }
    }

    pub(super) fn log(&self) {
        let step = self.step();
        match self {
            Self::Cards(o) => o.log(step),
            Self::Rules(o) => o.log(step),
            Self::Retire(o) => o.log(step),
            Self::Embed(o) => o.log(step),
            Self::Emoji(o) => o.log(step),
        }
    }
}

/// How a whole run ended, as `refresh_runs.ok` stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// Every step succeeded or was skipped for want of configuration
    /// (`ok = true`).
    Ok,
    /// A step failed, or the run timed out (`ok = false`).
    Failed,
    /// The run stopped because the schema is not its binary's, a deploy in
    /// progress or migrations pending (`ok = null`): neither a success that
    /// resets the schedule nor a failure that alerts or backs off.
    Stopped,
}

impl RunOutcome {
    /// The stored `ok`.
    #[must_use]
    pub const fn stored(self) -> Option<bool> {
        match self {
            Self::Ok => Some(true),
            Self::Failed => Some(false),
            Self::Stopped => None,
        }
    }
}

/// What a whole [`super::refresh`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    /// Every step, in the order run.
    pub steps: Vec<StepReport>,
    /// `max(rules.cr_version)` before the run (`None`: no rules, or unreadable).
    pub cr_before: Option<String>,
    /// The same after it.
    pub cr_after: Option<String>,
    /// Whether the run has a row in `refresh_runs` (see the module docs:
    /// a failure to write one never stops the run). An unrecorded run is
    /// invisible to [`history`], so a scheduler remembers it itself.
    pub recorded: bool,
    /// Whether the run passed [`super::RUN_TIMEOUT`] and its remaining steps
    /// were abandoned.
    pub timed_out: bool,
    /// Whether the run stopped at a failure outside its recorded steps, which
    /// fails it: `init`'s aliases or notes, or its lease lost between steps.
    pub aborted: bool,
}

impl RunReport {
    /// A report of `steps` alone: no CR versions, recorded, not timed out.
    #[must_use]
    pub const fn of(steps: Vec<StepReport>) -> Self {
        Self {
            steps,
            cr_before: None,
            cr_after: None,
            recorded: true,
            timed_out: false,
            aborted: false,
        }
    }

    /// The steps that failed, in order.
    #[must_use]
    pub fn failed(&self) -> Vec<Step> {
        self.steps
            .iter()
            .filter(|s| s.failed())
            .map(StepReport::step)
            .collect()
    }

    /// How the run ended; see [`RunOutcome`].
    #[must_use]
    pub fn outcome(&self) -> RunOutcome {
        if self.timed_out || self.aborted || self.steps.iter().any(StepReport::failed) {
            RunOutcome::Failed
        } else if self
            .steps
            .iter()
            .any(|s| s.skipped().is_some_and(Skip::stops))
        {
            RunOutcome::Stopped
        } else {
            RunOutcome::Ok
        }
    }

    /// The run succeeded ([`RunOutcome::Ok`]).
    #[must_use]
    pub fn ok(&self) -> bool {
        self.outcome() == RunOutcome::Ok
    }

    /// `Err` naming every failed step, for a command's exit status.
    ///
    /// # Errors
    /// When any step failed.
    pub fn ensure_ok(&self) -> Result<()> {
        match self.outcome() {
            RunOutcome::Ok => Ok(()),
            RunOutcome::Stopped => {
                let why = self
                    .steps
                    .iter()
                    .find_map(|s| s.skipped().filter(|k| k.stops()))
                    .map_or_else(String::new, |k| format!(": {k}"));
                anyhow::bail!("refresh stopped before writing{why}")
            }
            RunOutcome::Failed => {
                let names: Vec<&str> = self.failed().iter().map(|s| s.name()).collect();
                if names.is_empty() && self.aborted {
                    anyhow::bail!("refresh: stopped at a failure outside its steps");
                }
                let timed_out = if self.timed_out { ", timed out" } else { "" };
                anyhow::bail!(
                    "refresh: {} step(s) failed{timed_out}: {}",
                    names.len(),
                    names.join(", ")
                )
            }
        }
    }
}

/// The row a run writes; `None` when it could not be inserted.
#[derive(Clone, Copy, Debug)]
pub(super) struct RunId(i64);

/// `max(rules.cr_version)`, or `None` (logged) when it cannot be read; the
/// record is not worth failing a run over.
pub(super) async fn stored_cr(pool: &PgPool) -> Option<String> {
    match sqlx::query_scalar!("SELECT max(cr_version) FROM rules")
        .fetch_one(pool)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "reading the stored CR version for the run record");
            None
        }
    }
}

/// Insert the run's row. Never fails the run: see the module docs.
pub(super) async fn begin(
    pool: &PgPool,
    trigger: Trigger,
    process: &'static str,
    cr_before: Option<&str>,
) -> Option<RunId> {
    match sqlx::query_scalar!(
        "INSERT INTO refresh_runs (trigger, process, cr_before) VALUES ($1, $2, $3) RETURNING id",
        trigger.as_str(),
        process,
        cr_before,
    )
    .fetch_one(pool)
    .await
    {
        Ok(id) => Some(RunId(id)),
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(UNDEFINED_TABLE) => {
            tracing::warn!(
                "refresh_runs does not exist (the schema predates this binary; `judgebot ingest migrate` adds it): running unrecorded"
            );
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "recording the refresh start: running unrecorded");
            None
        }
    }
}

/// Postgres's SQLSTATE for a missing table.
const UNDEFINED_TABLE: &str = "42P01";

/// Whether [`history`] failed because `refresh_runs` does not exist (a schema
/// older than this binary), rather than for want of a database.
#[must_use]
pub fn is_missing_table(e: &anyhow::Error) -> bool {
    e.downcast_ref::<sqlx::Error>().is_some_and(
        |e| matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some(UNDEFINED_TABLE)),
    )
}

/// Fill in the run's row. Never fails the run: see the module docs.
pub(super) async fn finish(pool: &PgPool, id: Option<RunId>, report: &RunReport) {
    let Some(RunId(id)) = id else { return };
    let steps = match serde_json::to_value(&report.steps) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "serialising the refresh steps; recording none");
            serde_json::Value::Array(Vec::new())
        }
    };
    if let Err(e) = sqlx::query!(
        "UPDATE refresh_runs SET finished_at = now(), cr_after = $2, steps = $3, ok = $4 WHERE id = $1",
        id,
        report.cr_after.as_deref(),
        steps,
        report.outcome().stored(),
    )
    .execute(pool)
    .await
    {
        tracing::error!(error = %e, run = id, "recording the refresh result");
    }
}

/// How old an unfinished row must be before [`history`] counts it as a failed
/// run whose process died: [`super::RUN_TIMEOUT`], after which a live run
/// stops by itself, plus ten minutes for it to record that. The scheduler
/// drops a run at this same limit ([`crate::jobs`]), so no live run is older.
pub const ABANDONED_AFTER: Duration = super::RUN_TIMEOUT.saturating_add(Duration::from_mins(10));

/// What the record says about past runs, for deciding when one is due.
///
/// A run "with a verdict" is one that finished ok or failed (a stopped run,
/// `ok` null, has none), or one left unfinished for longer than
/// [`ABANDONED_AFTER`], which counts as failed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunHistory {
    /// Seconds since the latest run that finished with every step ok.
    pub last_ok_age_secs: Option<i64>,
    /// Seconds since the latest run started, finished or not (a run whose
    /// process died never finishes, and still counts as an attempt).
    pub last_attempt_age_secs: Option<i64>,
    /// Whether the latest run with a verdict succeeded.
    pub last_finished_ok: Option<bool>,
    /// `max(rules.cr_version)` now, whatever loaded it: a manual
    /// `judgebot ingest rules` is not a refresh run, so the record alone would
    /// miss it.
    pub stored_cr_version: Option<String>,
    /// Runs with a failed verdict since the latest that succeeded (all of
    /// them when none has): the length of the current failure streak.
    pub failed_streak: u32,
    /// Whether the latest run with a verdict skipped `embed` at the
    /// unattended ceiling ([`Skip::EmbedCeiling`]).
    pub last_finished_ceiling: bool,
    /// When the latest run with a verdict started (UTC, to the minute), if it
    /// never finished: its process died, or the database went away under it.
    pub abandoned_started_at: Option<String>,
}

/// Read [`RunHistory`], ages on the database's clock. `None` fields mean no
/// such run (or no rules) exists.
///
/// # Errors
/// On a database failure, including a schema without `refresh_runs`.
pub async fn history(pool: &PgPool) -> Result<RunHistory> {
    let abandoned_after = f64::from(u32::try_from(ABANDONED_AFTER.as_secs()).unwrap_or(u32::MAX));
    let r = sqlx::query!(
        r#"
        WITH r AS (
          SELECT id, started_at, steps, finished_at IS NULL AS unfinished,
                 CASE WHEN finished_at IS NULL THEN false ELSE ok END AS ok,
                 COALESCE(finished_at,
                   CASE WHEN started_at < now() - make_interval(secs => $1)
                        THEN started_at + make_interval(secs => $1) END) AS ended_at
          FROM refresh_runs
        ),
        verdicts AS (
          SELECT * FROM r WHERE ended_at IS NOT NULL AND ok IS NOT NULL
        ),
        latest AS (
          SELECT * FROM verdicts ORDER BY ended_at DESC, id DESC LIMIT 1
        )
        SELECT
          (SELECT extract(epoch FROM now() - max(ended_at))::bigint
             FROM verdicts WHERE ok) AS "last_ok_age_secs?",
          (SELECT extract(epoch FROM now() - max(started_at))::bigint
             FROM r) AS "last_attempt_age_secs?",
          (SELECT ok FROM latest) AS "last_finished_ok?",
          (SELECT max(cr_version) FROM rules) AS "stored_cr_version?",
          (SELECT count(*) FROM verdicts
             WHERE NOT ok AND ended_at > COALESCE(
               (SELECT max(ended_at) FROM verdicts WHERE ok), '-infinity'))
            AS "failed_streak!",
          (SELECT steps @> '[{"step": "embed", "reason": {"embed_ceiling": {}}}]'
             FROM latest) AS "last_finished_ceiling?",
          (SELECT to_char(started_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI"Z"')
             FROM latest WHERE unfinished) AS "abandoned_started_at?"
        "#,
        abandoned_after
    )
    .fetch_one(pool)
    .await
    .context("reading refresh_runs")?;
    Ok(RunHistory {
        last_ok_age_secs: r.last_ok_age_secs,
        last_attempt_age_secs: r.last_attempt_age_secs,
        last_finished_ok: r.last_finished_ok,
        stored_cr_version: r.stored_cr_version,
        failed_streak: u32::try_from(r.failed_streak).unwrap_or(u32::MAX),
        last_finished_ceiling: r.last_finished_ceiling.unwrap_or(false),
        abandoned_started_at: r.abandoned_started_at,
    })
}

impl RunHistory {
    /// What the record says about the data, for [`judge_core::About`]. A
    /// stored CR version that is not eight digits (none is ever written) is
    /// left out rather than shown.
    #[must_use]
    pub fn freshness(&self) -> Freshness {
        Freshness {
            cr_version: self
                .stored_cr_version
                .clone()
                .and_then(|v| CrVersion::try_new(v).ok()),
            refreshed_secs_ago: self.last_ok_age_secs.map(|s| u64::try_from(s).unwrap_or(0)),
            last_refresh_failed: self.last_finished_ok == Some(false),
        }
    }
}

/// The longest [`freshness`] waits for the database, pool acquisition
/// included: `/help` answers inside Discord's interaction deadline and
/// `GET /api/about` must not hang on an outage.
pub const FRESHNESS_TIMEOUT: Duration = Duration::from_secs(2);

/// [`RunHistory::freshness`], read within [`FRESHNESS_TIMEOUT`]. `None`
/// (logged at WARN) when it cannot be read: an interface shows "unknown",
/// never an error, for want of it.
///
/// A schema without `refresh_runs` (migrations pending with
/// `JUDGE_AUTO_MIGRATE=false`) still has its rules: the CR version is read
/// alone and no run is reported, logged at DEBUG (the startup log already
/// says the schema is behind).
pub async fn freshness(pool: &PgPool) -> Option<Freshness> {
    match tokio::time::timeout(FRESHNESS_TIMEOUT, history(pool)).await {
        Ok(Ok(h)) => Some(h.freshness()),
        Ok(Err(e)) if is_missing_table(&e) => {
            tracing::debug!(
                error = format_args!("{e:#}"),
                "no run record: the data's freshness is the CR version alone"
            );
            let cr = tokio::time::timeout(FRESHNESS_TIMEOUT, stored_cr(pool))
                .await
                .ok()??;
            Some(Freshness {
                cr_version: CrVersion::try_new(cr).ok(),
                ..Freshness::default()
            })
        }
        Ok(Err(e)) => {
            tracing::warn!(
                error = format_args!("{e:#}"),
                "reading the data's freshness"
            );
            None
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = FRESHNESS_TIMEOUT.as_secs(),
                "reading the data's freshness: no answer in time"
            );
            None
        }
    }
}

/// How long a [`FreshnessReader`] reuses what it read.
pub const FRESHNESS_TTL: Duration = Duration::from_mins(1);

/// [`freshness`] at most once per [`FRESHNESS_TTL`], for an endpoint anyone
/// can call (`GET /api/about`, the page's footer, on every page load). A
/// cached age is advanced by the time since it was read, so it stays exact; a
/// failed read is cached too, so an outage costs one [`FRESHNESS_TIMEOUT`]
/// per TTL rather than one per request.
///
/// Single-flight: the lock is held across a read, so callers arriving while
/// one is in flight wait for it (at most [`FRESHNESS_TIMEOUT`]) and are then
/// served its result. A flood at expiry is one query, never one per
/// connection in the pool.
#[derive(Debug)]
pub struct FreshnessReader {
    pool: PgPool,
    cached: tokio::sync::Mutex<Option<(Instant, Option<Freshness>)>>,
}

impl FreshnessReader {
    /// A reader over `pool`, with nothing cached.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            cached: tokio::sync::Mutex::new(None),
        }
    }

    /// The data's freshness, read again once the cached read is
    /// [`FRESHNESS_TTL`] old; one read at a time.
    pub async fn read(&self) -> Option<Freshness> {
        let mut cached = self.cached.lock().await;
        if let Some((at, f)) = cached
            .as_ref()
            .filter(|(at, _)| at.elapsed() < FRESHNESS_TTL)
        {
            return f.clone().map(|f| aged(f, at.elapsed()));
        }
        let read = freshness(&self.pool).await;
        *cached = Some((Instant::now(), read.clone()));
        read
    }
}

/// `f` as it reads `elapsed` after it was read.
fn aged(f: Freshness, elapsed: Duration) -> Freshness {
    Freshness {
        refreshed_secs_ago: f
            .refreshed_secs_ago
            .map(|s| s.saturating_add(elapsed.as_secs())),
        ..f
    }
}

/// Runs [`recent`] lists for `judge-cli stats`.
pub const RECENT_RUNS: i64 = 5;

/// Where a run stands, as `judge-cli stats` shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Finished, every step ok or skipped for want of configuration.
    Ok,
    /// Finished with a failed step, or timed out, or dropped at its limit.
    Failed,
    /// Stopped before writing: the schema is not its binary's.
    Stopped,
    /// Not finished, and younger than [`ABANDONED_AFTER`]: in progress.
    Running,
    /// Not finished, and older than [`ABANDONED_AFTER`]: its process died.
    Abandoned,
}

impl RunState {
    /// From a row: whether it finished, its `ok`, and whether an unfinished
    /// one is past [`ABANDONED_AFTER`].
    #[must_use]
    pub const fn of(finished: bool, ok: Option<bool>, past_limit: bool) -> Self {
        match (finished, ok) {
            (true, Some(true)) => Self::Ok,
            (true, Some(false)) => Self::Failed,
            (true, None) => Self::Stopped,
            (false, _) if past_limit => Self::Abandoned,
            (false, _) => Self::Running,
        }
    }
}

/// One run of the record, for `judge-cli stats`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RecentRun {
    /// When it started, UTC, to the minute (`2026-10-09 03:12Z`).
    pub started_at: String,
    /// What started it: `schedule` or `manual`.
    pub trigger: String,
    /// The process that ran it: `bot`, `api` or `ingest`.
    pub process: String,
    /// Where it stands.
    pub outcome: RunState,
    /// The stored CR version before it.
    pub cr_before: Option<String>,
    /// The same after it (`None` while it runs).
    pub cr_after: Option<String>,
    /// The steps that failed, in the order run.
    pub failed_steps: Vec<String>,
}

/// The latest `limit` runs, newest first.
///
/// # Errors
/// On a database failure, including a schema without `refresh_runs`.
pub async fn recent(pool: &PgPool, limit: i64) -> Result<Vec<RecentRun>> {
    let abandoned_after = f64::from(u32::try_from(ABANDONED_AFTER.as_secs()).unwrap_or(u32::MAX));
    let rows = sqlx::query!(
        r#"
        SELECT to_char(started_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI"Z"') AS "started_at!",
               trigger, process, ok, cr_before, cr_after,
               finished_at IS NOT NULL AS "finished!",
               started_at < now() - make_interval(secs => $2) AS "past_limit!",
               ARRAY(SELECT s->>'step' FROM jsonb_array_elements(steps) WITH ORDINALITY AS e(s, n)
                     WHERE s->>'outcome' = 'failed' ORDER BY n) AS "failed_steps!: Vec<String>"
        FROM refresh_runs
        ORDER BY started_at DESC, id DESC
        LIMIT $1
        "#,
        limit,
        abandoned_after
    )
    .fetch_all(pool)
    .await
    .context("reading refresh_runs")?;
    Ok(rows
        .into_iter()
        .map(|r| RecentRun {
            started_at: r.started_at,
            trigger: r.trigger,
            process: r.process,
            outcome: RunState::of(r.finished, r.ok, r.past_limit),
            cr_before: r.cr_before,
            cr_after: r.cr_after,
            failed_steps: r.failed_steps,
        })
        .collect())
}

/// What [`emoji_since`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EmojiCheck {
    /// The latest `finished_at` among the runs it read, as text a later call
    /// takes back; `None` when none finished after the mark.
    pub latest: Option<String>,
    /// Whether one of those runs may have uploaded emoji.
    pub uploaded: bool,
}

/// The runs that finished after `seen` (a [`EmojiCheck::latest`]; `None`:
/// every run), and whether any of them may have uploaded emoji, so the bot
/// reloads its table ([`crate::discord`]).
///
/// A run may have uploaded unless its `emoji` step is recorded as skipped or
/// as ok with `uploaded: 0`. A failed step, a run dropped at its limit (no
/// steps recorded) and a timed-out one all may have, and a reload costs one
/// Discord call, so they count.
///
/// # Errors
/// On a database failure, including a schema without `refresh_runs`.
pub async fn emoji_since(pool: &PgPool, seen: Option<&str>) -> Result<EmojiCheck> {
    let r = sqlx::query!(
        r#"
        SELECT max(finished_at)::text AS "latest?",
               COALESCE(bool_or(NOT (
                 steps @> '[{"step": "emoji", "outcome": "skipped"}]'
                 OR steps @> '[{"step": "emoji", "outcome": "ok", "summary": {"uploaded": 0}}]'
               )), false) AS "uploaded!"
        FROM refresh_runs
        WHERE finished_at > COALESCE($1::text::timestamptz, '-infinity')
        "#,
        seen
    )
    .fetch_one(pool)
    .await
    .context("reading refresh_runs for emoji uploads")?;
    Ok(EmojiCheck {
        latest: r.latest,
        uploaded: r.uploaded,
    })
}

/// The database's clock now, as text a later [`close_dropped`] takes.
///
/// # Errors
/// On a database failure.
pub async fn db_now(pool: &PgPool) -> Result<String> {
    sqlx::query_scalar!(r#"SELECT now()::text AS "now!""#)
        .fetch_one(pool)
        .await
        .context("reading the database's clock")
}

/// Close, as failed with no steps, the run a caller holding the lease started
/// at or after `since` ([`db_now`]) and then dropped at its time limit, so the
/// record says what happened rather than leaving it to look like a process
/// that died. Whether a row was closed: `false` when the run never wrote one.
///
/// # Errors
/// On a database failure.
pub async fn close_dropped(pool: &PgPool, since: &str) -> Result<bool> {
    let closed = sqlx::query!(
        "UPDATE refresh_runs SET finished_at = now(), steps = '[]', ok = false
         WHERE finished_at IS NULL AND started_at >= $1::text::timestamptz",
        since
    )
    .execute(pool)
    .await
    .context("closing the dropped run's row")?;
    Ok(closed.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sample() -> Vec<StepReport> {
        vec![
            StepReport::Cards(Outcome::Ok { summary: () }),
            StepReport::Rules(Outcome::Ok {
                summary: cr::Outcome::Updated {
                    version: "20260819".into(),
                    url: "https://example.test/cr.txt".into(),
                },
            }),
            StepReport::Retire(Outcome::Ok {
                summary: RetireSummary {
                    checked: 40,
                    retired: 1,
                    restored: 0,
                    still_retired: 2,
                },
            }),
            StepReport::Embed(Outcome::Skipped {
                reason: Skip::NoEmbedder,
            }),
            StepReport::Emoji(Outcome::Failed {
                error: "boom".into(),
            }),
        ]
    }

    /// The stored shape (module docs). Changing it strands every row already
    /// written, so this test changes only on purpose.
    #[test]
    fn the_stored_shape_is_pinned() -> Result<()> {
        let steps = sample();
        let value = serde_json::to_value(&steps)?;
        assert_eq!(
            value,
            json!([
                {"step": "cards", "outcome": "ok", "summary": null},
                {"step": "rules", "outcome": "ok",
                 "summary": {"cr": "updated", "version": "20260819", "url": "https://example.test/cr.txt"}},
                {"step": "retire", "outcome": "ok",
                 "summary": {"checked": 40, "retired": 1, "restored": 0, "still_retired": 2}},
                {"step": "embed", "outcome": "skipped", "reason": "no_embedder"},
                {"step": "emoji", "outcome": "failed", "error": "boom"},
            ])
        );
        let more = json!([
            {"step": "rules", "outcome": "ok", "summary": {"cr": "unchanged", "version": "20260819"}},
            {"step": "embed", "outcome": "ok", "summary": {"calls": 3, "glossary": 0, "rules": 12}},
            {"step": "emoji", "outcome": "ok",
             "summary": {"uploaded": 1, "skipped": 80, "unusable": 0, "failed": 0}},
            {"step": "emoji", "outcome": "skipped", "reason": "no_discord_token"},
            {"step": "retire", "outcome": "skipped", "reason": "init_stopped"},
            {"step": "embed", "outcome": "skipped",
             "reason": {"embed_ceiling": {"rows": 1912, "ceiling": 800}}},
        ]);
        let back: Vec<StepReport> = serde_json::from_value(more.clone())?;
        assert_eq!(serde_json::to_value(&back)?, more);
        assert_eq!(
            serde_json::from_value::<Vec<StepReport>>(value)?,
            steps,
            "round-trips"
        );
        Ok(())
    }

    #[test]
    fn a_report_names_its_failed_steps_and_skips_are_not_failures() {
        let report = RunReport::of(sample());
        assert_eq!(report.failed(), vec![Step::Emoji]);
        assert!(!report.ok());
        let err = report.ensure_ok().err().map(|e| e.to_string());
        assert_eq!(err.as_deref(), Some("refresh: 1 step(s) failed: emoji"));
        let fine = RunReport::of(vec![
            StepReport::Cards(Outcome::Ok { summary: () }),
            StepReport::Emoji(Outcome::Skipped {
                reason: Skip::NoDiscordToken,
            }),
        ]);
        assert!(fine.ok() && fine.ensure_ok().is_ok());
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_run_is_recorded_and_summarised(pool: PgPool) -> Result<()> {
        assert_eq!(history(&pool).await?, RunHistory::default(), "no runs yet");

        let id = begin(&pool, Trigger::Manual, "ingest", Some("20260101")).await;
        let h = history(&pool).await?;
        assert!(h.last_attempt_age_secs.is_some_and(|s| s <= 5), "{h:?}");
        assert_eq!(
            (h.last_ok_age_secs, h.last_finished_ok),
            (None, None),
            "a run in progress has finished nothing"
        );

        let report = RunReport {
            cr_before: Some("20260101".into()),
            cr_after: Some("20260819".into()),
            ..RunReport::of(sample())
        };
        finish(&pool, id, &report).await;
        let row = sqlx::query!(
            r#"SELECT trigger, process, cr_before, cr_after, steps, ok, finished_at IS NOT NULL AS "finished!" FROM refresh_runs"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            (row.trigger.as_str(), row.process.as_str(), row.finished),
            ("manual", "ingest", true)
        );
        assert_eq!(
            (row.cr_before.as_deref(), row.cr_after.as_deref(), row.ok),
            (Some("20260101"), Some("20260819"), Some(false))
        );
        assert_eq!(
            serde_json::from_value::<Vec<StepReport>>(row.steps)?,
            report.steps
        );
        let h = history(&pool).await?;
        assert_eq!(
            (h.last_ok_age_secs, h.last_finished_ok),
            (None, Some(false)),
            "a failed run is an attempt, not a success"
        );

        // An ok run an hour ago, then a run still going: the ages come from
        // different rows, and the unfinished one does not hide the finished.
        sqlx::query!(
            "INSERT INTO refresh_runs (started_at, finished_at, trigger, process, cr_after, ok)
             VALUES (now() - interval '2 hours', now() - interval '1 hour', 'schedule', 'bot', NULL, true)"
        )
        .execute(&pool)
        .await?;
        sqlx::query!("UPDATE refresh_runs SET finished_at = now() - interval '3 hours', started_at = now() - interval '3 hours' WHERE ok = false")
            .execute(&pool)
            .await?;
        let _running = begin(&pool, Trigger::Schedule, "api", None).await;
        let h = history(&pool).await?;
        assert!(h.last_attempt_age_secs.is_some_and(|s| s <= 5), "{h:?}");
        assert!(
            h.last_ok_age_secs
                .is_some_and(|s| (3595..=3605).contains(&s)),
            "{h:?}"
        );
        assert_eq!(h.last_finished_ok, Some(true));
        // The stored CR is read from `rules`, not from the record: a manual
        // `rules` load is no refresh run.
        assert_eq!(h.stored_cr_version, None, "no rules loaded");
        sqlx::query!(
            "INSERT INTO rules (id, subsection, body, cr_version) VALUES ('100.1', '100', 'x', '20261001')"
        )
        .execute(&pool)
        .await?;
        assert_eq!(
            history(&pool).await?.stored_cr_version.as_deref(),
            Some("20261001")
        );
        Ok(())
    }

    /// The failure streak counts finished failures since the last success,
    /// ignores unfinished rows, and the ceiling bit is the latest finished
    /// run's.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn the_streak_and_the_ceiling_come_from_the_finished_runs(pool: PgPool) -> Result<()> {
        let run = async |hours_ago: i32, ok: bool, steps: serde_json::Value| {
            sqlx::query(
                "INSERT INTO refresh_runs (started_at, finished_at, trigger, process, ok, steps)
                 VALUES (now() - make_interval(hours => $1), now() - make_interval(hours => $1),
                         'schedule', 'bot', $2, $3)",
            )
            .bind(hours_ago)
            .bind(ok)
            .bind(steps)
            .execute(&pool)
            .await
        };
        let ceiling = json!([{"step": "embed", "outcome": "skipped",
                              "reason": {"embed_ceiling": {"rows": 900, "ceiling": 800}}}]);
        run(10, false, json!([])).await?;
        assert_eq!(history(&pool).await?.failed_streak, 1, "no success yet");
        run(9, true, ceiling.clone()).await?;
        let h = history(&pool).await?;
        assert_eq!((h.failed_streak, h.last_finished_ceiling), (0, true));
        run(3, false, json!([])).await?;
        run(2, false, json!([])).await?;
        begin(&pool, Trigger::Schedule, "api", None).await;
        let h = history(&pool).await?;
        assert_eq!(
            (h.failed_streak, h.last_finished_ceiling),
            (2, false),
            "the unfinished run is not part of the streak"
        );
        run(
            1,
            true,
            json!([{"step": "embed", "outcome": "skipped", "reason": "no_embedder"}]),
        )
        .await?;
        let h = history(&pool).await?;
        assert_eq!((h.failed_streak, h.last_finished_ceiling), (0, false));
        Ok(())
    }

    /// An unfinished row older than [`ABANDONED_AFTER`] is a failed run, and
    /// says when it started; a younger one is a run in progress; a stopped
    /// run (`ok` null) is neither success nor failure.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_run_that_never_finished_counts_as_failed_once_it_is_old(pool: PgPool) -> Result<()> {
        sqlx::query(
            "INSERT INTO refresh_runs (started_at, finished_at, trigger, process, ok)
             VALUES (now() - interval '30 hours', now() - interval '30 hours', 'schedule', 'bot', true),
                    (now() - interval '5 hours', NULL, 'schedule', 'bot', NULL)",
        )
        .execute(&pool)
        .await?;
        let h = history(&pool).await?;
        assert_eq!((h.last_finished_ok, h.failed_streak), (Some(false), 1));
        assert!(h.abandoned_started_at.is_some(), "{h:?}");
        assert!(h.last_ok_age_secs.is_some_and(|s| s >= 30 * 3600 - 5));

        // A stopped run after it changes nothing; a run in progress neither.
        sqlx::query(
            "INSERT INTO refresh_runs (started_at, finished_at, trigger, process, ok)
             VALUES (now() - interval '2 hours', now() - interval '2 hours', 'schedule', 'api', NULL),
                    (now() - interval '1 minute', NULL, 'manual', 'ingest', NULL)",
        )
        .execute(&pool)
        .await?;
        let later = history(&pool).await?;
        assert_eq!(
            (
                later.last_finished_ok,
                later.failed_streak,
                later.abandoned_started_at.clone()
            ),
            (Some(false), 1, h.abandoned_started_at)
        );
        assert!(
            later.last_attempt_age_secs.is_some_and(|s| s <= 65),
            "{later:?}"
        );
        Ok(())
    }

    /// A finished run `hours_ago`, with `ok` and `steps`.
    async fn finished(
        pool: &PgPool,
        hours_ago: i32,
        ok: Option<bool>,
        steps: serde_json::Value,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO refresh_runs (started_at, finished_at, trigger, process, ok, steps)
             VALUES (now() - make_interval(hours => $1), now() - make_interval(hours => $1),
                     'schedule', 'bot', $2, $3)",
        )
        .bind(hours_ago)
        .bind(ok)
        .bind(steps)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// No runs and no rules; an ok run; a failure after it; rules loaded.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn freshness_reads_the_rules_and_the_last_runs(pool: PgPool) -> Result<()> {
        assert_eq!(
            freshness(&pool).await,
            Some(Freshness::default()),
            "nothing yet"
        );

        finished(&pool, 3, Some(true), json!([])).await?;
        let f = freshness(&pool).await.context("read")?;
        assert!(
            f.refreshed_secs_ago
                .is_some_and(|s| (3 * 3600 - 5..=3 * 3600 + 5).contains(&s)),
            "{f:?}"
        );
        assert!(!f.last_refresh_failed);
        assert_eq!(f.cr_version, None, "no rules loaded");

        // A failure after the success: the success's age stands, and the
        // failure is said. A stopped run after that changes neither.
        finished(&pool, 1, Some(false), json!([])).await?;
        finished(&pool, 0, None, json!([])).await?;
        sqlx::query!(
            "INSERT INTO rules (id, subsection, body, cr_version) VALUES ('100.1', '100', 'x', '20260925')"
        )
        .execute(&pool)
        .await?;
        let f = freshness(&pool).await.context("read")?;
        assert!(f.last_refresh_failed, "{f:?}");
        assert!(
            f.refreshed_secs_ago.is_some_and(|s| s >= 3 * 3600 - 5),
            "{f:?}"
        );
        assert_eq!(
            f.cr_version.as_ref().map(CrVersion::date).as_deref(),
            Some("2026-09-25")
        );

        // A later success clears the failure.
        finished(&pool, 0, Some(true), json!([])).await?;
        let f = freshness(&pool).await.context("read")?;
        assert!(
            !f.last_refresh_failed && f.refreshed_secs_ago.is_some_and(|s| s <= 5),
            "{f:?}"
        );

        // A schema without the run table still reports its rules; with
        // nothing readable it is None, never an error, and the cached
        // reader says the same.
        sqlx::query("DROP TABLE refresh_runs")
            .execute(&pool)
            .await?;
        let rules_only = freshness(&pool).await.context("read")?;
        assert_eq!(
            (
                rules_only
                    .cr_version
                    .as_ref()
                    .map(CrVersion::date)
                    .as_deref(),
                rules_only.refreshed_secs_ago,
                rules_only.last_refresh_failed
            ),
            (Some("2026-09-25"), None, false)
        );
        sqlx::query("DROP TABLE rules CASCADE")
            .execute(&pool)
            .await?;
        assert_eq!(freshness(&pool).await, None);
        assert_eq!(FreshnessReader::new(pool.clone()).read().await, None);
        Ok(())
    }

    /// Within the TTL the reader answers from its cache, whatever the
    /// database does meanwhile.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn the_reader_serves_its_cache_within_the_ttl(pool: PgPool) -> Result<()> {
        finished(&pool, 1, Some(true), json!([])).await?;
        let reader = FreshnessReader::new(pool.clone());
        let first = reader.read().await.context("read")?;
        sqlx::query("DROP TABLE refresh_runs")
            .execute(&pool)
            .await?;
        let (a, b) = tokio::join!(reader.read(), reader.read());
        assert_eq!(a.as_ref(), Some(&first));
        assert_eq!(b.as_ref(), Some(&first));
        Ok(())
    }

    /// The cache serves what it read, advanced by the time since.
    #[test]
    fn a_cached_age_is_advanced_by_the_time_since_the_read() {
        let f = Freshness {
            refreshed_secs_ago: Some(100),
            ..Freshness::default()
        };
        assert_eq!(
            aged(f, Duration::from_secs(30)).refreshed_secs_ago,
            Some(130)
        );
        assert_eq!(
            aged(Freshness::default(), Duration::from_secs(30)).refreshed_secs_ago,
            None
        );
    }

    #[test]
    fn a_run_state_comes_from_its_row() {
        assert_eq!(RunState::of(true, Some(true), false), RunState::Ok);
        assert_eq!(RunState::of(true, Some(false), true), RunState::Failed);
        assert_eq!(RunState::of(true, None, false), RunState::Stopped);
        assert_eq!(RunState::of(false, None, false), RunState::Running);
        assert_eq!(RunState::of(false, None, true), RunState::Abandoned);
    }

    /// Newest first, at most `limit`, each with its state and failed steps.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn recent_runs_list_the_latest_with_their_failed_steps(pool: PgPool) -> Result<()> {
        assert!(recent(&pool, RECENT_RUNS).await?.is_empty());
        finished(&pool, 50, Some(true), json!([])).await?;
        finished(&pool, 30, Some(false), serde_json::to_value(sample())?).await?;
        sqlx::query(
            "INSERT INTO refresh_runs (started_at, trigger, process, cr_before)
             VALUES (now() - interval '10 hours', 'manual', 'ingest', '20260101'),
                    (now() - interval '1 minute', 'schedule', 'api', '20260101')",
        )
        .execute(&pool)
        .await?;
        let runs = recent(&pool, 3).await?;
        let got: Vec<(RunState, &str, &str)> = runs
            .iter()
            .map(|r| (r.outcome, r.trigger.as_str(), r.process.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (RunState::Running, "schedule", "api"),
                (RunState::Abandoned, "manual", "ingest"),
                (RunState::Failed, "schedule", "bot"),
            ]
        );
        assert_eq!(
            runs.get(2).map(|r| r.failed_steps.clone()),
            Some(vec!["emoji".to_owned()])
        );
        assert_eq!(
            runs.first().and_then(|r| r.cr_before.as_deref()),
            Some("20260101")
        );
        assert!(
            runs.first()
                .is_some_and(|r| r.started_at.ends_with('Z') && r.started_at.len() == 17),
            "{runs:?}"
        );
        assert_eq!(
            serde_json::to_value(runs.first())?.get("outcome"),
            Some(&json!("running"))
        );
        Ok(())
    }

    /// A run that uploaded emoji is seen once, after the mark; one that
    /// uploaded none, or skipped the step, is not a reason to reload.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn emoji_since_finds_the_runs_that_may_have_uploaded(pool: PgPool) -> Result<()> {
        let emoji = |uploaded: u32| {
            json!([{"step": "emoji", "outcome": "ok",
                    "summary": {"uploaded": uploaded, "skipped": 84, "unusable": 0, "failed": 0}}])
        };
        assert_eq!(
            emoji_since(&pool, None).await?,
            EmojiCheck::default(),
            "no runs"
        );

        finished(&pool, 5, Some(true), emoji(3)).await?;
        let first = emoji_since(&pool, None).await?;
        assert!(first.uploaded && first.latest.is_some(), "{first:?}");
        let mark = first.latest;

        // Nothing since the mark: nothing to do, and no new mark.
        assert_eq!(
            emoji_since(&pool, mark.as_deref()).await?,
            EmojiCheck::default()
        );

        // Newer runs that uploaded nothing or skipped the step.
        finished(&pool, 4, Some(true), emoji(0)).await?;
        finished(
            &pool,
            3,
            Some(true),
            json!([{"step": "emoji", "outcome": "skipped", "reason": "no_discord_token"}]),
        )
        .await?;
        let quiet = emoji_since(&pool, mark.as_deref()).await?;
        assert!(
            !quiet.uploaded && quiet.latest.is_some() && quiet.latest != mark,
            "{quiet:?}"
        );

        // An older run that uploaded, behind the mark, stays seen.
        finished(&pool, 6, Some(true), emoji(1)).await?;
        let still = emoji_since(&pool, quiet.latest.as_deref()).await?;
        assert_eq!(still, EmojiCheck::default());

        // A newer upload, and a failed step (it may have uploaded first),
        // and a run dropped with no steps recorded, each count.
        for steps in [
            emoji(2),
            json!([{"step": "emoji", "outcome": "failed", "error": "boom"}]),
            json!([]),
        ] {
            let mark = emoji_since(&pool, None).await?.latest;
            sqlx::query(
                "INSERT INTO refresh_runs (started_at, finished_at, trigger, process, ok, steps)
                 VALUES (now(), clock_timestamp(), 'schedule', 'bot', false, $1)",
            )
            .bind(&steps)
            .execute(&pool)
            .await?;
            let c = emoji_since(&pool, mark.as_deref()).await?;
            assert!(c.uploaded && c.latest > mark, "{steps}: {c:?}");
        }
        // An unfinished run is not looked at.
        let mark = emoji_since(&pool, None).await?.latest;
        begin(&pool, Trigger::Schedule, "api", None).await;
        assert_eq!(
            emoji_since(&pool, mark.as_deref()).await?,
            EmojiCheck::default()
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn an_unknown_trigger_is_refused_by_the_table(pool: PgPool) -> Result<()> {
        let r =
            sqlx::query!("INSERT INTO refresh_runs (trigger, process) VALUES ('cron', 'ingest')")
                .execute(&pool)
                .await;
        assert!(r.is_err());
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_missing_table_is_a_run_left_unrecorded(pool: PgPool) -> Result<()> {
        sqlx::query("DROP TABLE refresh_runs")
            .execute(&pool)
            .await?;
        let id = begin(&pool, Trigger::Manual, "ingest", None).await;
        assert!(id.is_none());
        // And finishing with no row is a no-op, not an error.
        finish(&pool, id, &RunReport::of(Vec::new())).await;
        let err = history(&pool).await.err();
        assert!(
            err.as_ref().is_some_and(is_missing_table),
            "the reader says so: {err:?}"
        );
        Ok(())
    }
}
