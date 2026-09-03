//! Schema migrations from inside the binaries: [`MIGRATOR`] (embedded from
//! `crates/bot/migrations`) applied by [`run`], so a deploy host with nothing
//! but Docker never needs `sqlx-cli`.
//!
//! Two callers, one behaviour:
//!
//! * `bot` and `api` call [`at_startup`] before anything else touches the
//!   pool, so `docker compose pull && docker compose up -d` is a complete
//!   deploy. A failure exits the process, which under `restart:
//!   unless-stopped` is a crash-loop with the reason in the log — loud, where
//!   the old quiet failure was a bot that answered but could not persist.
//!   `JUDGE_AUTO_MIGRATE=false` opts out for an operator who moves the schema
//!   by hand. Only the long-lived services do this: the nightly `refresh`
//!   container runs whatever image `docker compose pull` last fetched, which
//!   may be newer than the running bot, and a tool (`judge-cli`, `judge-mcp`)
//!   does not own the schema.
//! * `judge-ingest migrate` calls [`run`] with [`Ahead::Refuse`]: the
//!   explicit form, for an empty database before the first `up -d` and for
//!   the opted-out operator.
//!
//! What `run` does is what `sqlx migrate run` does — apply every migration
//! not yet in `_sqlx_migrations`, in version order, each with its ledger row
//! in one transaction, refusing when an applied migration's file has changed
//! — with one difference in how a database *ahead* of the binary is treated
//! (a rollback to an older image tag): the explicit command refuses, and
//! startup skips with a warning so the rollback still boots (the runbook says
//! when an older binary cannot run against a newer schema at all).
//!
//! For the whole run it holds the exclusive side of [`CALLS_REWRITE_LOCK`]
//! on a dedicated connection, the key the CR loader, the retirement pass,
//! `reembed` and every vector write take, so a nightly `refresh` that fires
//! mid-migration waits instead of interleaving a `calls` rewrite with a
//! migration that rewrites `calls` rows. It does not stop the other service:
//! a persist that writes no vector takes no lock, so a migration the release
//! notes flag as rewriting rows still wants `bot`/`api` stopped first
//! (docs/DEPLOYMENT.md §8). Two services starting together serialise on that
//! same lock, taken before sqlx's own migrator lock and in that order by every
//! caller (`sqlx-cli` takes only the second), so no cycle is possible; the
//! second finds nothing pending. Both locks live on the one connection, which
//! is closed rather than returned on failure: sqlx's migrator does not release
//! its lock on an error, and a pooled connection still holding it would wedge
//! the next run in the same process.

use sqlx::{
    PgConnection, PgPool,
    migrate::{AppliedMigration, Migrate as _, MigrateError, Migration},
};

use super::CALLS_REWRITE_LOCK;
use crate::MIGRATOR;

/// The opt-out for [`at_startup`]. Unset or blank means on.
pub const AUTO_MIGRATE_ENV: &str = "JUDGE_AUTO_MIGRATE";

/// What to do when the ledger holds versions this binary does not carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ahead {
    /// Fail with [`Error::Ahead`]: the explicit command.
    Refuse,
    /// Apply nothing, report the versions: startup of a rolled-back image.
    Skip,
}

/// What a run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Versions applied by this run, in order.
    pub applied: Vec<i64>,
    /// Versions that were already in place before it.
    pub already: usize,
    /// Versions in the ledger this binary does not know (non-empty only with
    /// [`Ahead::Skip`], and then nothing was applied).
    pub ahead: Vec<i64>,
}

/// Why a run failed, spelled out for the operator reading a crash log.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A connection or query failure outside the migrator. The cause is in
    /// the message (no `source` chain, so a crash log prints it once).
    #[error("{what}: {error}")]
    Db {
        /// Which step.
        what: &'static str,
        /// The sqlx error.
        error: sqlx::Error,
    },
    /// Only a migration whose first line is `-- no-transaction` can leave
    /// this (none exist today); a transactional one that fails is rolled
    /// back and recorded nowhere.
    #[error(
        "migration {0} ran outside a transaction and failed part-way, so the database is marked dirty at it; \
         inspect what it left behind and repair by hand (or restore the pre-release dump, docs/DEPLOYMENT.md §6), \
         then delete its `success = false` row from _sqlx_migrations and migrate again"
    )]
    Dirty(i64),
    /// An applied migration's embedded file no longer matches the checksum
    /// recorded when it ran.
    #[error(
        "migration {0} was applied from a different file than this binary carries (checksum mismatch); \
         a migration must never be edited after it has run — the image and the database disagree"
    )]
    VersionMismatch(i64),
    /// The ledger holds versions this binary does not carry.
    #[error(
        "the database holds migrations this binary does not know ({0:?}): it was migrated by a newer release; \
         run the newer image, or restore the dump taken before it (docs/DEPLOYMENT.md §8)"
    )]
    Ahead(Vec<i64>),
    /// Any other migrator failure (a migration's SQL failing, say).
    #[error("applying migrations: {0}")]
    Migrate(MigrateError),
    /// `JUDGE_AUTO_MIGRATE` is not a boolean.
    #[error("{AUTO_MIGRATE_ENV} must be true or false (also 1/0, yes/no, on/off), got {0:?}")]
    BadFlag(String),
}

fn db(what: &'static str) -> impl FnOnce(sqlx::Error) -> Error {
    move |error| Error::Db { what, error }
}

/// Apply every pending migration; see the module docs.
///
/// # Errors
/// [`Error`]: a connection failure; a migration whose SQL fails — Postgres
/// runs each migration's SQL and its ledger row in one transaction, so a
/// failure rolls the whole migration back, records nothing, and the next run
/// retries it; a checksum mismatch on an applied migration; or, with
/// [`Ahead::Refuse`], a ledger ahead of the binary.
pub async fn run(pool: &PgPool, ahead: Ahead) -> Result<Report, Error> {
    // Session-level (not transaction-level) so it spans every migration's
    // own transaction; released explicitly, and by the server if the
    // connection drops.
    let mut guard = pool.acquire().await.map_err(db("connecting for the migration lock"))?;
    // Say so before blocking: a deploy that lands during the nightly CR load
    // waits minutes here, and an empty log reads as a hang.
    let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(CALLS_REWRITE_LOCK)
        .fetch_one(&mut *guard)
        .await
        .map_err(db("trying the calls rewrite lock"))?;
    if !free {
        tracing::warn!("another job holds the calls rewrite lock (a refresh CR load, retirement pass or reembed); waiting for it before migrating");
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(CALLS_REWRITE_LOCK)
            .execute(&mut *guard)
            .await
            .map_err(db("taking the calls rewrite lock"))?;
    }
    match apply(&mut guard, ahead).await {
        Ok(report) => {
            if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)").bind(CALLS_REWRITE_LOCK).execute(&mut *guard).await {
                tracing::warn!(error = %e, "releasing the calls rewrite lock (the server releases it with the connection)");
            }
            Ok(report)
        }
        Err(e) => {
            // Both advisory locks die with the session; never hand the pool a
            // connection that still holds sqlx's.
            if let Err(close) = guard.close().await {
                tracing::debug!(error = %close, "closing the migration connection");
            }
            Err(e)
        }
    }
}

async fn apply(conn: &mut PgConnection, ahead: Ahead) -> Result<Report, Error> {
    let ledger = applied(conn).await?;
    // A known version applied from a different file is refused on every
    // path, a rolled-back image included: the migrator only checks this when
    // it runs, and the ahead branch below returns before it does.
    for a in &ledger {
        if let Some(m) = MIGRATOR.iter().find(|m| m.version == a.version)
            && m.checksum != a.checksum
        {
            return Err(Error::VersionMismatch(a.version));
        }
    }
    let before: Vec<i64> = ledger.iter().map(|a| a.version).collect();
    let unknown: Vec<i64> = before.iter().copied().filter(|v| !MIGRATOR.version_exists(*v)).collect();
    if !unknown.is_empty() {
        match ahead {
            Ahead::Refuse => return Err(Error::Ahead(unknown)),
            Ahead::Skip => {
                tracing::warn!(versions = ?unknown, "database is ahead of this binary (a newer release migrated it); not migrating");
                return Ok(Report { applied: Vec::new(), already: before.len(), ahead: unknown });
            }
        }
    }
    let pending: Vec<&Migration> = MIGRATOR.iter().filter(|m| !before.contains(&m.version)).collect();
    for m in &pending {
        tracing::info!(version = m.version, description = %m.description, "migration pending");
    }
    // Run even with nothing pending: this is where an applied migration whose
    // file changed is caught, as `sqlx migrate run` catches it — and it is
    // checked before anything is applied.
    MIGRATOR.run(&mut *conn).await.map_err(explain)?;
    let after: Vec<i64> = applied(conn).await?.iter().map(|a| a.version).collect();
    let applied: Vec<i64> = pending.iter().map(|m| m.version).filter(|v| after.contains(v)).collect();
    if applied.is_empty() {
        tracing::info!(in_place = after.len(), "schema is current");
    } else {
        tracing::info!(?applied, in_place = after.len(), "schema migrated");
    }
    Ok(Report { applied, already: before.len(), ahead: Vec::new() })
}

/// The `_sqlx_migrations` ledger (version + checksum), creating the table if
/// the database is empty (idempotent, and what the migrator does first anyway).
async fn applied(conn: &mut PgConnection) -> Result<Vec<AppliedMigration>, Error> {
    let table = MIGRATOR.table_name.as_ref();
    conn.ensure_migrations_table(table).await.map_err(explain)?;
    conn.list_applied_migrations(table).await.map_err(explain)
}

fn explain(e: MigrateError) -> Error {
    match e {
        MigrateError::Dirty(v) => Error::Dirty(v),
        MigrateError::VersionMismatch(v) => Error::VersionMismatch(v),
        MigrateError::VersionMissing(v) => Error::Ahead(vec![v]),
        other => Error::Migrate(other),
    }
}

/// Whether [`at_startup`] should migrate: [`AUTO_MIGRATE_ENV`], on unless
/// set to a false value.
///
/// # Errors
/// [`Error::BadFlag`] for anything that is not a boolean spelling.
pub fn auto_migrate_enabled() -> Result<bool, Error> {
    parse_flag(std::env::var(AUTO_MIGRATE_ENV).ok().as_deref())
}

fn parse_flag(raw: Option<&str>) -> Result<bool, Error> {
    let Some(raw) = raw else { return Ok(true) };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(Error::BadFlag(raw.to_owned())),
    }
}

/// What `bot` and `api` do first: migrate unless opted out, tolerating a
/// database ahead of the binary (see the module docs).
///
/// # Errors
/// [`Error`] as [`run`] with [`Ahead::Skip`], or a bad opt-out value.
pub async fn at_startup(pool: &PgPool) -> Result<Report, Error> {
    if !auto_migrate_enabled()? {
        tracing::info!("{AUTO_MIGRATE_ENV} is off; schema left as it is (run `judge-ingest migrate` yourself)");
        return Ok(Report::default());
    }
    run(pool, Ahead::Skip).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_versions() -> Vec<i64> {
        MIGRATOR.iter().map(|m| m.version).collect()
    }

    #[sqlx::test(migrations = false)]
    async fn an_empty_database_gets_every_migration_and_a_second_run_does_nothing(pool: PgPool) -> anyhow::Result<()> {
        let first = run(&pool, Ahead::Refuse).await?;
        assert_eq!(first.applied, all_versions());
        assert_eq!(first.already, 0);
        let mut conn = pool.acquire().await?;
        let ledger: Vec<i64> = applied(&mut conn).await?.iter().map(|a| a.version).collect();
        assert_eq!(ledger, all_versions());
        // The schema is real, not just the ledger.
        let one: bool = sqlx::query_scalar("SELECT to_regclass('embedding_space') IS NOT NULL").fetch_one(&pool).await?;
        assert!(one);

        let second = run(&pool, Ahead::Refuse).await?;
        assert_eq!(second, Report { applied: Vec::new(), already: all_versions().len(), ahead: Vec::new() });
        Ok(())
    }

    #[sqlx::test(migrations = false)]
    async fn a_partly_migrated_database_gets_only_the_rest(pool: PgPool) -> anyhow::Result<()> {
        let versions = all_versions();
        let Some(&first) = versions.first() else { return Err(anyhow::anyhow!("no migrations embedded")) };
        MIGRATOR.run_to(first, &pool).await?;
        let r = run(&pool, Ahead::Refuse).await?;
        assert_eq!(r.already, 1);
        assert_eq!(r.applied, versions.get(1..).unwrap_or_default());
        Ok(())
    }

    #[sqlx::test(migrations = false)]
    async fn a_migration_applied_from_a_changed_file_is_refused(pool: PgPool) -> anyhow::Result<()> {
        MIGRATOR.run(&pool).await?;
        let Some(m) = MIGRATOR.iter().last() else { return Err(anyhow::anyhow!("no migrations embedded")) };
        // Forge the ledger: the last migration "ran" from different SQL.
        sqlx::query("UPDATE _sqlx_migrations SET checksum = $1 WHERE version = $2")
            .bind(&b"not the file"[..])
            .bind(m.version)
            .execute(&pool)
            .await?;
        assert!(matches!(run(&pool, Ahead::Refuse).await, Err(Error::VersionMismatch(v)) if v == m.version));
        // Startup is just as strict about a changed file: only "ahead" is tolerated.
        assert!(matches!(run(&pool, Ahead::Skip).await, Err(Error::VersionMismatch(_))));
        // A failed run must not leave a connection holding sqlx's migrator
        // lock in the pool, or this third run would block forever.
        let third = tokio::time::timeout(std::time::Duration::from_secs(10), run(&pool, Ahead::Skip)).await;
        assert!(matches!(third, Ok(Err(Error::VersionMismatch(_)))), "{third:?}");
        Ok(())
    }

    #[sqlx::test(migrations = false)]
    async fn a_database_ahead_of_the_binary_is_refused_explicitly_and_skipped_at_startup(pool: PgPool) -> anyhow::Result<()> {
        MIGRATOR.run(&pool).await?;
        // A version from a "newer release", as its migrator would have recorded it.
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES ($1, 'future', true, $2, 0)",
        )
        .bind(99_990_101_000_001_i64)
        .bind(&b"future"[..])
        .execute(&pool)
        .await?;
        assert!(matches!(run(&pool, Ahead::Refuse).await, Err(Error::Ahead(v)) if v == [99_990_101_000_001]));
        let r = run(&pool, Ahead::Skip).await?;
        assert_eq!(r.ahead, [99_990_101_000_001]);
        assert!(r.applied.is_empty());
        assert_eq!(r.already, all_versions().len() + 1);
        // Ahead does not excuse a changed known file: the rolled-back image
        // must not boot against a schema its own migration did not produce.
        let Some(m) = MIGRATOR.iter().last() else { return Err(anyhow::anyhow!("no migrations embedded")) };
        sqlx::query("UPDATE _sqlx_migrations SET checksum = $1 WHERE version = $2")
            .bind(&b"rewritten"[..])
            .bind(m.version)
            .execute(&pool)
            .await?;
        assert!(matches!(run(&pool, Ahead::Skip).await, Err(Error::VersionMismatch(v)) if v == m.version));
        Ok(())
    }

    #[test]
    fn the_opt_out_flag_is_a_boolean_with_on_as_the_default() {
        for on in [None, Some(""), Some(" "), Some("true"), Some("1"), Some("YES"), Some("on")] {
            assert!(matches!(parse_flag(on), Ok(true)), "{on:?}");
        }
        for off in [Some("false"), Some("0"), Some("No"), Some("off ")] {
            assert!(matches!(parse_flag(off), Ok(false)), "{off:?}");
        }
        assert!(matches!(parse_flag(Some("maybe")), Err(Error::BadFlag(s)) if s == "maybe"));
    }
}
