//! The run record: what each [`super::refresh`] did, in `refresh_runs`.
//!
//! A run inserts its row when it starts ([`begin`]) and fills it in when it
//! ends ([`finish`]): the finish time, the stored CR version after it, each
//! step's [`StepReport`] and whether every step succeeded. A row with no
//! `finished_at` is a run in progress or one whose process died. Times are the
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
//! A rules summary is `{"cr": "unchanged", "version"}` when the published
//! release was already loaded; an embed summary is rows embedded per table
//! (`{"rules": 12, "glossary": 0, "calls": 3}`); an emoji summary is
//! `{"uploaded", "skipped", "unusable", "failed"}` counts.

use std::collections::BTreeMap;

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
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoDiscordToken => "DISCORD_TOKEN is not set",
            Self::NoEmbedder => "no embedder configured (VOYAGE_API_KEY or [models.embed])",
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

/// What a whole [`super::refresh`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    /// Every step, in the order run.
    pub steps: Vec<StepReport>,
    /// `max(rules.cr_version)` before the run (`None`: no rules, or unreadable).
    pub cr_before: Option<String>,
    /// The same after it.
    pub cr_after: Option<String>,
}

impl RunReport {
    /// The steps that failed, in order.
    #[must_use]
    pub fn failed(&self) -> Vec<Step> {
        self.steps
            .iter()
            .filter(|s| s.failed())
            .map(StepReport::step)
            .collect()
    }

    /// No step failed. Skipped steps count as fine: they are configuration.
    #[must_use]
    pub fn ok(&self) -> bool {
        !self.steps.iter().any(StepReport::failed)
    }

    /// `Err` naming every failed step, for a command's exit status.
    ///
    /// # Errors
    /// When any step failed.
    pub fn ensure_ok(&self) -> Result<()> {
        let failed = self.failed();
        if failed.is_empty() {
            return Ok(());
        }
        let names: Vec<&str> = failed.iter().map(|s| s.name()).collect();
        anyhow::bail!(
            "refresh: {} step(s) failed: {}",
            names.len(),
            names.join(", ")
        )
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
        report.ok(),
    )
    .execute(pool)
    .await
    {
        tracing::error!(error = %e, run = id, "recording the refresh result");
    }
}

/// What the record says about past runs, for deciding when one is due.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunHistory {
    /// Seconds since the latest run that finished with every step ok.
    pub last_ok_age_secs: Option<i64>,
    /// Seconds since the latest run started, finished or not (a run whose
    /// process died never finishes, and still counts as an attempt).
    pub last_attempt_age_secs: Option<i64>,
    /// Whether the latest finished run was ok.
    pub last_finished_ok: Option<bool>,
    /// `max(rules.cr_version)` now, whatever loaded it: a manual
    /// `judge-ingest rules` is not a refresh run, so the record alone would
    /// miss it.
    pub stored_cr_version: Option<String>,
}

/// Read [`RunHistory`], ages on the database's clock. `None` fields mean no
/// such run (or no rules) exists.
///
/// # Errors
/// On a database failure, including a schema without `refresh_runs`.
pub async fn history(pool: &PgPool) -> Result<RunHistory> {
    let r = sqlx::query!(
        r#"
        SELECT
          (SELECT extract(epoch FROM now() - max(finished_at))::bigint
             FROM refresh_runs WHERE ok) AS "last_ok_age_secs?",
          (SELECT extract(epoch FROM now() - max(started_at))::bigint
             FROM refresh_runs) AS "last_attempt_age_secs?",
          (SELECT ok FROM refresh_runs WHERE finished_at IS NOT NULL
             ORDER BY finished_at DESC, id DESC LIMIT 1) AS "last_finished_ok?",
          (SELECT max(cr_version) FROM rules) AS "stored_cr_version?"
        "#
    )
    .fetch_one(pool)
    .await
    .context("reading refresh_runs")?;
    Ok(RunHistory {
        last_ok_age_secs: r.last_ok_age_secs,
        last_attempt_age_secs: r.last_attempt_age_secs,
        last_finished_ok: r.last_finished_ok,
        stored_cr_version: r.stored_cr_version,
    })
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
        let report = RunReport {
            steps: sample(),
            cr_before: None,
            cr_after: None,
        };
        assert_eq!(report.failed(), vec![Step::Emoji]);
        assert!(!report.ok());
        let err = report.ensure_ok().err().map(|e| e.to_string());
        assert_eq!(err.as_deref(), Some("refresh: 1 step(s) failed: emoji"));
        let fine = RunReport {
            steps: vec![
                StepReport::Cards(Outcome::Ok { summary: () }),
                StepReport::Emoji(Outcome::Skipped {
                    reason: Skip::NoDiscordToken,
                }),
            ],
            cr_before: None,
            cr_after: None,
        };
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
            steps: sample(),
            cr_before: Some("20260101".into()),
            cr_after: Some("20260819".into()),
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
        finish(
            &pool,
            id,
            &RunReport {
                steps: Vec::new(),
                cr_before: None,
                cr_after: None,
            },
        )
        .await;
        assert!(history(&pool).await.is_err(), "the reader says so");
        Ok(())
    }
}
