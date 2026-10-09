//! The data steps behind `judgebot ingest`: Scryfall sync ([`scryfall`]), the
//! Comprehensive Rules loader ([`cr`]), the curated lists ([`aliases`],
//! [`notes`]), embedding ([`embed`], [`reembed`]), the Discord emoji upload
//! ([`emoji`]) and the explicit migration ([`schema`]), plus the two
//! sequences built from them: [`init`], the first load, and [`refresh`], the
//! scheduled job ([`crate::jobs`] runs it on a timer). `judgebot ingest` is
//! argument parsing over this module.
//!
//! Every step that writes data takes `&mut` [`RefreshLease`] ([`lease`]), so
//! two runs never overlap, in one process or several, and a step run without
//! it, or beside another step under the same lease, does not compile. The exceptions say why: [`schema::migrate`] has its own
//! lock and must run before any table exists, and [`emoji::run`] writes no
//! database (inside [`refresh`] it runs under the lease the run holds).
//! [`refresh`] also records each run in `refresh_runs` ([`runs`]).
//!
//! They live in the library, beside the other Postgres adapters, so that the
//! long-running roles (`judgebot --jobs`) run the same code as the command
//! line.
//!
//! The embedder comes from `judge.toml` / `VOYAGE_API_KEY` through
//! [`crate::config`], the same loader the bot uses, so `embed` writes the space
//! the bot queries. `emoji` needs no database at all, only `DISCORD_TOKEN`.
//! Downloads are cached under [`cache_dir`].

pub mod aliases;
pub mod cr;
pub mod embed;
pub mod emoji;
pub mod lease;
pub mod notes;
pub mod reembed;
mod renumber;
pub mod runs;
pub mod schema;
pub mod scryfall;

pub use lease::{REFRESH_LOCK, RefreshLease, lease, try_lease};

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use judge_embed::WithSpace;
use sqlx::PgPool;

use crate::db::RetireSummary;
use runs::{Outcome, RunReport, Skip, Step, StepReport, Trigger};

/// Default download cache, relative to the working directory (gitignored).
pub const DEFAULT_CACHE_DIR: &str = ".cache";

/// The download cache: `INGEST_CACHE_DIR`, else [`DEFAULT_CACHE_DIR`].
#[must_use]
pub fn cache_dir() -> PathBuf {
    std::env::var_os("INGEST_CACHE_DIR")
        .map_or_else(|| PathBuf::from(DEFAULT_CACHE_DIR), PathBuf::from)
}

/// A pool on `DATABASE_URL` for the ingest steps.
///
/// # Errors
/// When `DATABASE_URL` is unset or the database cannot be reached.
pub async fn connect() -> Result<PgPool> {
    let url = std::env::var("DATABASE_URL").context("DATABASE_URL is not set")?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .context("connecting to DATABASE_URL")
}

/// Connect timeout for every download the steps make.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest a response may go silent mid-body. An idle bound rather than a
/// total one: a ~100 MB Scryfall bulk file over a slow NAS link takes minutes
/// and must not be cut off, but a stalled connection must not hang the run
/// (and with it the refresh lease) forever.
const READ_TIMEOUT: Duration = Duration::from_mins(2);

/// The HTTP client the download steps share: `user_agent`, plus
/// [`CONNECT_TIMEOUT`] and [`READ_TIMEOUT`].
///
/// # Errors
/// When the TLS backend cannot be initialised.
pub(crate) fn http_client(user_agent: &str) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .context("building the HTTP client")
}

/// An embedder as the configuration builds it.
pub type Embedder = Arc<dyn WithSpace>;

/// The configured embedder (`judge.toml`, else `VOYAGE_API_KEY`), or `None`
/// with a warning when neither names one, so an unconfigured environment
/// degrades instead of failing. A configuration that does not load is an
/// error: a typo must not silently skip the embedding step.
///
/// # Errors
/// When the configuration does not load or its embedder cannot be built.
pub fn embedder_from_config() -> Result<Option<Embedder>> {
    let config = load_config()?;
    tracing::info!("{}", config.summary());
    let embedder = config.embedder()?;
    if embedder.is_none() {
        tracing::warn!(
            "no embedder configured (VOYAGE_API_KEY or [models.embed]); embedding steps will be skipped"
        );
    }
    Ok(embedder)
}

/// [`embedder_from_config`] without the warning, for a caller that reports
/// the skip itself, and with the summary at DEBUG: a long-running process
/// logged it at startup, and reloads it on every scheduled run.
fn configured_embedder() -> Result<Option<Embedder>> {
    let config = load_config()?;
    tracing::debug!("{}", config.summary());
    Ok(config.embedder()?)
}

fn load_config() -> Result<crate::config::Config> {
    crate::config::Config::load().context("loading the model configuration")
}

/// The whole first load, in dependency order: the schema, the cards the
/// curated lists name, the rules, the retirement pass (nothing to retire in a
/// fresh database), then the vectors over both and the emoji.
///
/// It is recorded in `refresh_runs` as a [`Trigger::Manual`] run of the
/// refresh steps it runs, so the schedule, `/help` and the
/// page count a first load as fresh data rather than refreshing right after
/// it.
///
/// Unlike [`refresh`] it stops at the first failure, because each step needs
/// the one before it (aliases resolve against cards, embeddings read rules).
/// Every step is idempotent, so the fix for a failed `init` is to run it
/// again. It loads the built-in alias and note lists, replacing the tables,
/// so an operator who keeps their own list loads that afterwards. With no
/// embedder configured `embed` skips itself with a warning, and with no
/// `DISCORD_TOKEN` the emoji step is skipped the same way.
///
/// The migration runs first, under its own lock; every later step runs under
/// the [`RefreshLease`] (taken as `process`), waiting for a refresh in
/// progress, and stops if the lease is lost. It takes the lease itself (and
/// only after the migration released its lock, so the two locks are never
/// taken in the other order), which is why it takes a pool.
///
/// # Errors
/// The first step that failed, named.
pub async fn init(pool: &PgPool, cache_dir: &Path, process: &'static str) -> Result<()> {
    let started = Instant::now();
    // Before the download: a judge.toml that does not load should fail in a
    // second, not at step 6.
    let embedder = embedder_from_config().context("init: the model configuration")?;

    let mut steps = InitSteps::default();
    let t = steps.begin("migrate");
    schema::migrate(pool).await.context("init: migrate")?;
    InitSteps::done("migrate", t);
    let mut held = lease(pool, process)
        .await
        .context("init: the refresh lease")?;
    // Recorded as a manual refresh run of the steps it shares with one, so
    // the schedule, `/help` and the page count a fresh load as fresh data.
    let cr_before = runs::stored_cr(pool).await;
    let record = runs::begin(pool, Trigger::Manual, process, cr_before.as_deref()).await;
    let mut reports = Vec::with_capacity(Step::ALL.len());
    let result = init_data(
        &mut held,
        cache_dir,
        embedder.as_deref(),
        &mut steps,
        &mut reports,
    )
    .await;
    let report = init_report(
        reports,
        result.is_err(),
        cr_before,
        runs::stored_cr(pool).await,
        record.is_some(),
    );
    runs::finish(pool, record, &report).await;
    held.release().await;
    result?;
    tracing::info!(
        secs = started.elapsed().as_secs(),
        "init done: start the api (`docker compose up -d api`) and ask a question"
    );
    Ok(())
}

/// `init`'s step counter and log lines.
#[derive(Default)]
struct InitSteps(u8);

impl InitSteps {
    fn begin(&mut self, name: &'static str) -> Instant {
        self.0 = self.0.saturating_add(1);
        tracing::info!(step = name, "init step {} of 8", self.0);
        Instant::now()
    }

    fn done(name: &'static str, t: Instant) {
        tracing::info!(step = name, secs = t.elapsed().as_secs(), "init step ok");
    }

    /// [`Self::begin`], after checking the lease is still held.
    async fn next(&mut self, lease: &mut RefreshLease, name: &'static str) -> Result<Instant> {
        lease
            .check()
            .await
            .with_context(|| format!("init: before {name}"))?;
        Ok(self.begin(name))
    }
}

/// What `init` ran of the refresh steps, as the run record stores it: each
/// step it reached, then every one it did not reach as skipped
/// ([`Skip::InitStopped`]), in [`Step::ALL`] order (`init` runs them in that
/// order). A failure outside those steps (`aliases`, `notes`, the lease)
/// fails the run all the same ([`RunReport::aborted`]).
fn init_report(
    mut steps: Vec<StepReport>,
    failed: bool,
    cr_before: Option<String>,
    cr_after: Option<String>,
    recorded: bool,
) -> RunReport {
    for step in Step::ALL {
        if !steps.iter().any(|s| s.step() == step) {
            steps.push(step.skipped(Skip::InitStopped));
        }
    }
    let aborted = failed && !steps.iter().any(StepReport::failed);
    RunReport {
        steps,
        cr_before,
        cr_after,
        recorded,
        timed_out: false,
        aborted,
    }
}

/// A step's result as its record, keeping the result for `?`.
fn noted<T: Clone>(result: &Result<T>) -> Outcome<T> {
    match result {
        Ok(summary) => Outcome::Ok {
            summary: summary.clone(),
        },
        Err(e) => Outcome::Failed {
            error: format!("{e:#}"),
        },
    }
}

/// Rows embedded per table as the record stores them.
fn embedded(counts: impl IntoIterator<Item = (&'static str, usize)>) -> runs::Embedded {
    counts
        .into_iter()
        .map(|(table, n)| (table.to_owned(), n))
        .collect()
}

/// `init` after the migration, under the lease. Each refresh step it runs
/// is pushed to `reports` as it ends.
async fn init_data(
    lease: &mut RefreshLease,
    cache_dir: &Path,
    embedder: Option<&dyn WithSpace>,
    steps: &mut InitSteps,
    reports: &mut Vec<StepReport>,
) -> Result<()> {
    let t = steps.next(lease, "cards").await?;
    let r = scryfall::run(lease, cache_dir).await;
    reports.push(StepReport::Cards(noted(&r)));
    r.context("init: cards")?;
    InitSteps::done("cards", t);
    let t = steps.next(lease, "rules").await?;
    let r = cr::run_latest(lease, cache_dir).await;
    reports.push(StepReport::Rules(noted(&r)));
    r.context("init: rules latest")?;
    InitSteps::done("rules", t);
    let t = steps.next(lease, "aliases").await?;
    aliases::run(lease, aliases::BUILTIN)
        .await
        .context("init: aliases")?;
    InitSteps::done("aliases", t);
    let t = steps.next(lease, "notes").await?;
    notes::run(lease, notes::BUILTIN)
        .await
        .context("init: notes")?;
    InitSteps::done("notes", t);
    // Nothing to retire in a fresh database; a re-run over a used one does
    // what a refresh would after loading the same data.
    let t = steps.next(lease, "retire").await?;
    let r = retire(lease).await;
    reports.push(StepReport::Retire(noted(&r)));
    r.context("init: retire")?;
    InitSteps::done("retire", t);
    let t = steps.next(lease, "embed").await?;
    // A database that holds no vectors has nothing to lose, so it takes the
    // configured embedder's space whatever that is: `reembed` retypes the
    // columns for a width other than the schema's 1024, where `embed` would
    // refuse. One that already holds vectors is only ever filled, and a
    // mismatch there is refused with the way out (`reembed --yes`, which
    // pays for every row and is therefore never implied).
    let holds_vectors = crate::db::space::stored_counts(lease.pool())
        .await?
        .iter()
        .any(|(_, n)| *n > 0);
    // `None`: no embedder, the step skipped (`embed::run` logs it).
    let r: Result<Option<runs::Embedded>> = match (embedder, holds_vectors) {
        // The switch reports nothing itself; what the columns hold after it
        // is what it embedded, the database having held no vectors before.
        (Some(e), false) => {
            async {
                reembed::run(lease, Some(e), true, false).await?;
                let held = crate::db::space::stored_counts(lease.pool()).await?;
                Ok(Some(embedded(
                    held.into_iter()
                        .map(|(t, n)| (t, usize::try_from(n).unwrap_or(0))),
                )))
            }
            .await
        }
        (Some(e), true) => embed::run(lease, Some(e)).await.map(|c| Some(embedded(c))),
        (None, _) => embed::run(lease, None).await.map(|_| None),
    };
    reports.push(StepReport::Embed(match &r {
        Ok(Some(summary)) => Outcome::Ok {
            summary: summary.clone(),
        },
        Ok(None) => Outcome::Skipped {
            reason: Skip::NoEmbedder,
        },
        Err(e) => Outcome::Failed {
            error: format!("{e:#}"),
        },
    }));
    r.context("init: embed")?;
    InitSteps::done("embed", t);
    let t = steps.next(lease, "emoji").await?;
    if has_discord_token() {
        let r = emoji::run(cache_dir).await;
        reports.push(StepReport::Emoji(noted(&r)));
        r.context("init: emoji")?;
        InitSteps::done("emoji", t);
    } else {
        reports.push(StepReport::Emoji(Outcome::Skipped {
            reason: Skip::NoDiscordToken,
        }));
        tracing::warn!(
            step = "emoji",
            "init step skipped: DISCORD_TOKEN is not set; run `judgebot ingest emoji` once the Discord app exists"
        );
    }
    Ok(())
}

/// The retirement pass ([`crate::db::retire_unsupported`]) under the lease.
///
/// # Errors
/// On a database failure.
pub async fn retire(lease: &mut RefreshLease) -> Result<RetireSummary> {
    Ok(crate::db::retire_unsupported(lease.pool()).await?)
}

/// Whether `DISCORD_TOKEN` is set, so the emoji step has an application to
/// upload to.
fn has_discord_token() -> bool {
    std::env::var("DISCORD_TOKEN").is_ok_and(|t| !t.trim().is_empty())
}

/// Every scheduled step ([`Step::ALL`]), in dependency order: cards and rules
/// first, then the retirement pass over the calls that cite them, then `embed`
/// so a new CR's rows are embedded in the same run, then the emoji. A failed
/// step is logged and the rest still run; the report names every failure
/// ([`RunReport::ensure_ok`]).
///
/// Before each step the run checks that it may still write, and stops when it
/// may not; that step and every later one are recorded with the reason, and
/// none of them runs. It may not write when
///
/// * its lease is lost ([`RefreshLease::check`]): another run may already be
///   writing. The steps are failed.
/// * the schema is not this binary's ([`crate::db::migrate::skew`]): ahead
///   (a newer release migrated the database, as between `docker compose pull`
///   and `up -d`) or behind (migrations pending, `judgebot ingest migrate`). The
///   steps are skipped ([`Skip::SchemaAhead`], [`Skip::SchemaBehind`]) and the
///   run is [`runs::RunOutcome::Stopped`]: neither a success nor a failure.
/// * it has run for [`RUN_TIMEOUT`]: the step in progress is abandoned (its
///   future dropped, so its transaction rolls back unless it was committing)
///   and failed as timed out, and the rest are skipped
///   ([`Skip::RunTimedOut`]), so a hung download or query cannot hold the
///   lease, and so every later refresh, forever. A statement already sent
///   keeps running on the server until it ends: a scheduled run's
///   connections carry `statement_timeout` and `lock_timeout`
///   ([`crate::jobs`]) to bound that.
///
/// The run is recorded in `refresh_runs` ([`runs`]) as started by `trigger`
/// in the lease's process. Recording never stops a step: a record that
/// cannot be written is logged, and the run goes on unrecorded
/// ([`RunReport::recorded`]).
///
/// A step that cannot run for want of configuration is skipped, not failed:
/// `embed` with no embedder configured, `emoji` with no `DISCORD_TOKEN` (the
/// emoji belong to the bot's Discord application, and a database-only
/// deployment has none).
///
/// A [`Trigger::Schedule`] run also skips `embed` when more rows wait than it
/// may pay for unattended ([`embed::UNATTENDED_CEILING`]); see [`over_ceiling`].
pub async fn refresh(lease: &mut RefreshLease, cache_dir: &Path, trigger: Trigger) -> RunReport {
    let step = async |lease: &mut RefreshLease, step: Step| {
        refresh_step(lease, step, cache_dir, trigger).await
    };
    refresh_with(lease, trigger, RUN_TIMEOUT, step).await
}

/// [`refresh`] with its time limit and its steps as parameters, so a test can
/// run it without downloading anything.
async fn refresh_with<F>(
    lease: &mut RefreshLease,
    trigger: Trigger,
    limit: Duration,
    mut run_step: F,
) -> RunReport
where
    F: AsyncFnMut(&mut RefreshLease, Step) -> StepReport,
{
    let deadline = tokio::time::Instant::now() + limit;
    let pool = lease.pool().clone();
    let cr_before = runs::stored_cr(&pool).await;
    let record = runs::begin(&pool, trigger, lease.process(), cr_before.as_deref()).await;
    let mut steps = Vec::with_capacity(Step::ALL.len());
    let mut halted: Option<Halt> = None;
    let mut timed_out = false;
    for step in Step::ALL {
        if halted.is_none() {
            halted = may_write(lease).await.err();
        }
        let report = match &halted {
            Some(Halt::Lost(error)) => step.failed(error.clone()),
            Some(Halt::Skip(reason)) => step.skipped(*reason),
            None => {
                if let Ok(report) = tokio::time::timeout_at(deadline, run_step(lease, step)).await {
                    report
                } else {
                    timed_out = true;
                    halted = Some(Halt::Skip(Skip::RunTimedOut));
                    step.failed(format!(
                        "timed out: the run passed its {} min limit, and the rest of it was abandoned",
                        limit.as_secs() / 60
                    ))
                }
            }
        };
        report.log();
        steps.push(report);
    }

    let cr_after = runs::stored_cr(&pool).await;
    let loaded = steps.iter().any(|s| {
        matches!(
            s,
            StepReport::Rules(Outcome::Ok {
                summary: cr::Outcome::Updated { .. }
            })
        )
    });
    if loaded {
        let (before, after) = (
            cr_before.as_deref().unwrap_or("none"),
            cr_after.as_deref().unwrap_or("none"),
        );
        tracing::info!(before, after, "CR {before} → {after}");
    }
    let report = RunReport {
        steps,
        cr_before,
        cr_after,
        recorded: record.is_some(),
        timed_out,
        aborted: false,
    };
    runs::finish(&pool, record, &report).await;
    report
}

/// The longest a [`refresh`] runs before abandoning its remaining steps. A
/// healthy run takes minutes and a first full load on a NAS well under an
/// hour ([`lease::LEASE_WAIT`] is sized the same way); three hours is that with
/// room for a slow Scryfall day and a full re-embed after a new embedder, and
/// still far less than the daily interval, so a hang costs one day's refresh
/// at most.
pub const RUN_TIMEOUT: Duration = Duration::from_hours(3);

/// Why a run may not write its next step.
#[derive(Clone, Debug)]
enum Halt {
    /// The lease is lost, or the check could not be made: the steps fail.
    Lost(String),
    /// The steps are skipped: the schema is not this binary's
    /// ([`Skip::stops`]), or the run timed out.
    Skip(Skip),
}

/// `Ok` when the run may write its next step: the lease is held and the
/// schema is this binary's.
async fn may_write(lease: &mut RefreshLease) -> std::result::Result<(), Halt> {
    let lost = |e: anyhow::Error| Halt::Lost(format!("{e:#}"));
    lease.check().await.map_err(lost)?;
    let skew = crate::db::migrate::skew(lease.pool())
        .await
        .context("reading the migration ledger")
        .map_err(lost)?;
    if let Some(problem) = skew.problem() {
        tracing::warn!("not writing: {problem}");
        return Err(Halt::Skip(if skew.ahead.is_empty() {
            Skip::SchemaBehind
        } else {
            Skip::SchemaAhead
        }));
    }
    Ok(())
}

/// What a single-step command (`judgebot ingest cards`, `rules`, `embed`, …)
/// checks once after taking the lease, as [`refresh`] does before each step:
/// the lease is held and the schema is this binary's.
///
/// # Errors
/// Naming why it may not write.
pub async fn ensure_writable(lease: &mut RefreshLease) -> Result<()> {
    match may_write(lease).await {
        Ok(()) => Ok(()),
        Err(Halt::Lost(error)) => anyhow::bail!(error),
        Err(Halt::Skip(reason)) => anyhow::bail!("not writing: {reason}"),
    }
}

/// One step of [`refresh`].
async fn refresh_step(
    lease: &mut RefreshLease,
    step: Step,
    cache_dir: &Path,
    trigger: Trigger,
) -> StepReport {
    match step {
        Step::Cards => StepReport::Cards(Outcome::of(scryfall::run(lease, cache_dir).await)),
        Step::Rules => StepReport::Rules(Outcome::of(cr::run_latest(lease, cache_dir).await)),
        Step::Retire => StepReport::Retire(Outcome::of(retire(lease).await)),
        Step::Embed => StepReport::Embed(match configured_embedder() {
            Ok(Some(embedder)) => match over_ceiling(lease, trigger).await {
                Ok(Some(reason)) => Outcome::Skipped { reason },
                Ok(None) => Outcome::of(embed::run(lease, Some(&*embedder)).await.map(|counts| {
                    counts
                        .into_iter()
                        .map(|(table, n)| (table.to_owned(), n))
                        .collect()
                })),
                Err(e) => Outcome::of(Err(e)),
            },
            Ok(None) => Outcome::Skipped {
                reason: Skip::NoEmbedder,
            },
            Err(e) => Outcome::of(Err(e)),
        }),
        Step::Emoji => StepReport::Emoji(if has_discord_token() {
            Outcome::of(emoji::run(cache_dir).await)
        } else {
            Outcome::Skipped {
                reason: Skip::NoDiscordToken,
            }
        }),
    }
}

/// The spend guard on an unattended run: a [`Trigger::Schedule`] run with
/// more rows to embed than [`embed::UNATTENDED_CEILING`] skips the step with
/// the count. A manual run is someone deciding to pay, so it has no ceiling.
///
/// # Errors
/// When the rows cannot be counted.
async fn over_ceiling(lease: &mut RefreshLease, trigger: Trigger) -> Result<Option<Skip>> {
    match trigger {
        Trigger::Manual => Ok(None),
        Trigger::Schedule => {
            let rows = embed::pending(lease.pool()).await?;
            Ok(
                (rows > embed::UNATTENDED_CEILING).then_some(Skip::EmbedCeiling {
                    rows,
                    ceiling: embed::UNATTENDED_CEILING,
                }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `init`'s record has every refresh step, in order: those it reached,
    /// then the rest as skipped because it stopped; a failure outside them
    /// fails the run all the same.
    #[test]
    fn an_init_run_is_recorded_with_every_step() {
        let ok = |step: Step| match step {
            Step::Cards => StepReport::Cards(Outcome::Ok { summary: () }),
            other => other.skipped(Skip::NoEmbedder),
        };
        let all = vec![
            ok(Step::Cards),
            Step::Rules.skipped(Skip::NoEmbedder),
            Step::Retire.skipped(Skip::NoEmbedder),
            Step::Embed.skipped(Skip::NoEmbedder),
            Step::Emoji.skipped(Skip::NoDiscordToken),
        ];
        let done = init_report(all.clone(), false, None, Some("20260925".into()), true);
        assert_eq!(done.steps, all);
        assert_eq!(done.outcome(), runs::RunOutcome::Ok);

        // Stopped at aliases, after cards and rules: the rest are skipped,
        // and the run failed although no recorded step did.
        let reached = vec![ok(Step::Cards), Step::Rules.skipped(Skip::NoEmbedder)];
        let stopped = init_report(reached, true, None, None, true);
        let order: Vec<Step> = stopped.steps.iter().map(StepReport::step).collect();
        assert_eq!(order, Step::ALL.to_vec());
        assert!(
            stopped
                .steps
                .iter()
                .skip(2)
                .all(|s| s.skipped() == Some(Skip::InitStopped)),
            "{:?}",
            stopped.steps
        );
        assert!(stopped.aborted);
        assert_eq!(stopped.outcome(), runs::RunOutcome::Failed);
        assert_eq!(
            stopped.ensure_ok().err().map(|e| e.to_string()).as_deref(),
            Some("refresh: stopped at a failure outside its steps")
        );

        // A failed step names itself; the run is not also "aborted".
        let failed = init_report(
            vec![ok(Step::Cards), Step::Rules.failed("boom".into())],
            true,
            None,
            None,
            true,
        );
        assert!(!failed.aborted);
        assert_eq!(failed.failed(), vec![Step::Rules]);
        assert_eq!(failed.steps.len(), Step::ALL.len());
    }

    /// `init` loads these with no file to fall back on, so a list that stopped
    /// parsing must fail here rather than on an operator's first run.
    #[test]
    fn the_built_in_lists_parse() -> Result<()> {
        assert!(!scryfall::parse_alias_yaml(aliases::BUILTIN)?.is_empty());
        assert!(!notes::parse_notes_yaml(notes::BUILTIN)?.is_empty());
        Ok(())
    }

    /// The spend guard counts every embeddable table, and binds a scheduled
    /// run only: at the ceiling it embeds, one row over it skips.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_scheduled_run_skips_embedding_over_the_ceiling(pool: PgPool) -> Result<()> {
        let mut held = lease(&pool, "ceiling-test").await?;
        sqlx::query(
            "INSERT INTO glossary (term, text, cr_version)
             SELECT 'term ' || g, 'text', '20260101' FROM generate_series(1, $1) g",
        )
        .bind(i32::try_from(embed::UNATTENDED_CEILING)?)
        .execute(&pool)
        .await?;
        assert_eq!(embed::pending(&pool).await?, embed::UNATTENDED_CEILING);
        assert_eq!(over_ceiling(&mut held, Trigger::Schedule).await?, None);
        sqlx::query(
            "INSERT INTO rules (id, subsection, body, cr_version) VALUES ('100.1', '100', 'x', '20260101')",
        )
        .execute(&pool)
        .await?;
        let rows = embed::UNATTENDED_CEILING + 1;
        assert_eq!(
            over_ceiling(&mut held, Trigger::Schedule).await?,
            Some(Skip::EmbedCeiling {
                rows,
                ceiling: embed::UNATTENDED_CEILING
            })
        );
        assert_eq!(over_ceiling(&mut held, Trigger::Manual).await?, None);
        held.release().await;
        Ok(())
    }

    /// A schema from a newer release, or one with migrations pending, is
    /// never written: every step is skipped with the reason, none runs, and
    /// the run is stopped (stored `ok` null), not failed.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_run_on_a_schema_it_does_not_know_writes_nothing(pool: PgPool) -> Result<()> {
        let mut held = lease(&pool, "skew-test").await?;
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES ($1, 'future', true, $2, 0)",
        )
        .bind(99_990_101_000_001_i64)
        .bind(&b"future"[..])
        .execute(&pool)
        .await?;
        let ran = std::cell::Cell::new(0);
        let count = async |_: &mut RefreshLease, step: Step| {
            ran.set(ran.get() + 1);
            step.failed("ran".into())
        };
        let report = refresh_with(&mut held, Trigger::Manual, RUN_TIMEOUT, count).await;
        assert_eq!((ran.get(), report.failed()), (0, Vec::new()));
        assert_eq!(report.outcome(), runs::RunOutcome::Stopped);
        assert!(
            report
                .steps
                .iter()
                .all(|s| s.skipped() == Some(Skip::SchemaAhead))
        );
        let err = report.ensure_ok().err().map(|e| e.to_string());
        assert!(
            err.as_deref().is_some_and(|e| e.contains("newer release")),
            "{err:?}"
        );
        let ok: Option<bool> = sqlx::query_scalar("SELECT ok FROM refresh_runs")
            .fetch_one(&pool)
            .await?;
        assert_eq!(ok, None, "stored as neither");
        assert!(
            ensure_writable(&mut held).await.is_err(),
            "nor may a single step"
        );

        sqlx::query("DELETE FROM _sqlx_migrations WHERE version >= 20261008000001")
            .execute(&pool)
            .await?;
        let report = refresh_with(&mut held, Trigger::Manual, RUN_TIMEOUT, count).await;
        assert_eq!(ran.get(), 0);
        let err = report.ensure_ok().err().map(|e| e.to_string());
        assert!(
            err.as_deref()
                .is_some_and(|e| e.contains("judgebot ingest migrate")),
            "{err:?}"
        );
        held.release().await;
        Ok(())
    }

    /// A step that hangs is abandoned at the limit, recorded as timed out,
    /// and the rest are not started; the row is finished, not left open.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_hung_step_times_the_run_out_and_is_recorded(pool: PgPool) -> Result<()> {
        let mut held = lease(&pool, "timeout-test").await?;
        let started = std::cell::Cell::new(Vec::new());
        let hang = async |_: &mut RefreshLease, step: Step| {
            let mut seen = started.take();
            seen.push(step);
            started.set(seen);
            if step == Step::Rules {
                std::future::pending::<()>().await;
            }
            StepReport::Cards(Outcome::Ok { summary: () })
        };
        let report = refresh_with(
            &mut held,
            Trigger::Schedule,
            Duration::from_millis(300),
            hang,
        )
        .await;
        assert!(report.timed_out && report.recorded);
        assert_eq!(report.outcome(), runs::RunOutcome::Failed);
        assert_eq!(started.take(), vec![Step::Cards, Step::Rules]);
        assert_eq!(
            report.failed(),
            vec![Step::Rules],
            "only the step that hung"
        );
        assert!(
            report
                .steps
                .iter()
                .skip(2)
                .all(|s| s.skipped() == Some(Skip::RunTimedOut))
        );
        let (ok, finished): (Option<bool>, bool) =
            sqlx::query_as("SELECT ok, finished_at IS NOT NULL FROM refresh_runs")
                .fetch_one(&pool)
                .await?;
        assert_eq!((ok, finished), (Some(false), true));
        held.release().await;
        Ok(())
    }

    /// A run whose lease is gone runs nothing (no download, no write) and
    /// records every step as failed with the reason.
    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_lost_lease_stops_the_run_and_is_recorded(pool: PgPool) -> Result<()> {
        let mut held = lease(&pool, "lost-test").await?;
        let pid: i32 = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity
             WHERE datname = current_database() AND application_name LIKE 'judgebot refresh lease (lost-test)%'",
        )
        .fetch_one(&pool)
        .await?;
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(pid)
            .execute(&pool)
            .await?;
        let report = refresh(&mut held, Path::new("/nonexistent"), Trigger::Manual).await;
        assert_eq!(report.failed(), Step::ALL.to_vec());
        assert!(
            report.steps.iter().all(|s| {
                serde_json::to_string(s).is_ok_and(|j| j.contains("refresh lease lost"))
            })
        );
        let (process, ok): (String, Option<bool>) =
            sqlx::query_as("SELECT process, ok FROM refresh_runs")
                .fetch_one(&pool)
                .await?;
        assert_eq!((process.as_str(), ok), ("lost-test", Some(false)));
        Ok(())
    }
}
