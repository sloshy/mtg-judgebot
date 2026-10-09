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
//! predates the table (the new image's `judge-ingest refresh` run before
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
//! A scheduled run that finds more empty vectors than it may embed
//! unattended skips `embed` with
//! `{"reason": {"embed_ceiling": {"rows": 1912, "ceiling": 800}}}`.
//!
//! A rules summary is `{"cr": "unchanged", "version"}` when the published
//! release was already loaded; an embed summary is rows embedded per table
//! (`{"rules": 12, "glossary": 0, "calls": 3}`); an emoji summary is
//! `{"uploaded", "skipped", "unusable", "failed"}` counts.

use std::{collections::BTreeMap, time::Duration};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::{cr, emoji};
use crate::db::RetireSummary;

/// What started a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// A long-running process's own schedule.
    Schedule,
    /// A command: `judge-ingest refresh`, by hand or from cron.
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
    /// something other than a new CR emptied them, and `judge-ingest embed`
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
    /// Migrations this binary carries are pending (`judge-ingest migrate`).
    /// The run stops ([`RunOutcome::Stopped`]).
    SchemaBehind,
    /// An earlier step passed the run's time limit ([`super::RUN_TIMEOUT`]),
    /// so this one was not started.
    RunTimedOut,
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
            Self::SchemaBehind => "migrations are pending: run `judge-ingest migrate`",
            Self::RunTimedOut => "not run: the refresh timed out at an earlier step",
            Self::EmbedCeiling { rows, ceiling } => {
                return write!(
                    f,
                    "{rows} rows to embed, over the {ceiling} a scheduled run embeds unattended; \
                     run `judge-ingest embed` by hand"
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
        if self.timed_out || self.steps.iter().any(StepReport::failed) {
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
                "refresh_runs does not exist (the schema predates this binary; `judge-ingest migrate` adds it): running unrecorded"
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
    /// `judge-ingest rules` is not a refresh run, so the record alone would
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
