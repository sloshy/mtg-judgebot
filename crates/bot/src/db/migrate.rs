//! Schema migrations from inside the binaries: [`MIGRATOR`] (embedded from
//! `crates/bot/migrations`) applied by [`run`], so a deploy host with nothing
//! but Docker never needs `sqlx-cli`.
//!
//! Two callers, one behaviour:
//!
//! * `judgebot` calls [`at_startup`] before anything else touches the
//!   pool, so `docker compose pull && docker compose up -d` is a complete
//!   deploy. A failure exits the process, which under `restart:
//!   unless-stopped` is a crash-loop with the reason in the log — loud, where
//!   the quiet alternative is a bot that answers but cannot persist.
//!   `JUDGE_AUTO_MIGRATE=false` opts out for an operator who moves the schema
//!   by hand. Only the long-lived services do this: the one-shot `refresh`
//!   container runs whatever image `docker compose pull` last fetched, which
//!   may be newer than the running bot, and a tool (`judge-cli`, `judge-mcp`)
//!   does not own the schema.
//! * `judgebot ingest migrate` calls [`run`] with [`Ahead::Refuse`]: the
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
//! After the migrations it brings each extension they create ([`EXTENSIONS`])
//! up to the version the server's files carry (`ALTER EXTENSION … UPDATE`): a
//! new `db` image ships a new pgvector library, but the extension's SQL objects
//! stay at the version that created them until updated. A failure there is a
//! WARN, never a failed run: the old objects keep working with the new library.
//!
//! For the whole run it holds the exclusive side of [`CALLS_REWRITE_LOCK`]
//! on a dedicated connection, the key the CR loader, the retirement pass,
//! `reembed` and every vector write take, so a refresh that fires
//! mid-migration waits instead of interleaving a `calls` rewrite with a
//! migration that rewrites `calls` rows. It does not stop another process:
//! a persist that writes no vector takes no lock, so a migration the release
//! notes flag as rewriting rows still wants every other process on the
//! database stopped first (docs/DEPLOYMENT.md §8). Two processes starting
//! together serialise on that same lock, taken before sqlx's own migrator
//! lock and in that order by every caller (`sqlx-cli` takes only the
//! second), so no cycle is possible; the second finds nothing pending. Both locks live on the one connection, which
//! is closed rather than returned on failure: sqlx's migrator does not release
//! its lock on an error, and a pooled connection still holding it would wedge
//! the next run in the same process.

use sqlx::{
    PgConnection, PgPool,
    migrate::{AppliedMigration, Migrate as _, MigrateError, Migration},
};

use judge_core::{
    CommitHash,
    release::{self, Compatibility, Release},
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
    /// Extensions this run updated, in [`EXTENSIONS`] order.
    pub extensions: Vec<ExtensionUpdate>,
}

/// The extensions the migrations create, which [`run`] keeps at the version
/// the server's files carry, each with its update (an identifier cannot be a
/// bind parameter, so the statement is a constant).
pub const EXTENSIONS: [(&str, &str); 2] = [
    ("vector", "ALTER EXTENSION vector UPDATE"),
    ("pg_trgm", "ALTER EXTENSION pg_trgm UPDATE"),
];

/// One extension moved to the version the server's files carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionUpdate {
    /// Its name, one of [`EXTENSIONS`].
    pub name: &'static str,
    /// The version it was at.
    pub from: String,
    /// The version it is at now.
    pub to: String,
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
    /// The database was migrated by a build this one cannot run against
    /// ([`rollback_refusal`] says why and what to do).
    #[error("{0}")]
    Rollback(String),
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
    let mut guard = pool
        .acquire()
        .await
        .map_err(db("connecting for the migration lock"))?;
    // Say so before blocking: a deploy that lands during a refresh's CR load
    // waits minutes here, and an empty log reads as a hang.
    let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(CALLS_REWRITE_LOCK)
        .fetch_one(&mut *guard)
        .await
        .map_err(db("trying the calls rewrite lock"))?;
    if !free {
        tracing::warn!(
            "another job holds the calls rewrite lock (a refresh CR load, retirement pass or reembed); waiting for it before migrating"
        );
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(CALLS_REWRITE_LOCK)
            .execute(&mut *guard)
            .await
            .map_err(db("taking the calls rewrite lock"))?;
    }
    match apply(&mut guard, ahead).await {
        Ok(report) => {
            if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(CALLS_REWRITE_LOCK)
                .execute(&mut *guard)
                .await
            {
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
    let unknown = unknown_to_binary(&before);
    if !unknown.is_empty() {
        match ahead {
            Ahead::Refuse => return Err(Error::Ahead(unknown)),
            Ahead::Skip => {
                // Decided under the lock this run holds, so a migration in
                // progress elsewhere has finished and recorded its release.
                let recorded = instance_release(&mut *conn).await?;
                if let Some(why) = rollback_refusal(recorded.as_ref()) {
                    return Err(Error::Rollback(why));
                }
                tracing::warn!(versions = ?unknown, "database is ahead of this binary (a newer release of its major migrated it); not migrating");
                return Ok(Report {
                    applied: Vec::new(),
                    already: before.len(),
                    ahead: unknown,
                    extensions: Vec::new(),
                });
            }
        }
    }
    let pending: Vec<&Migration> = MIGRATOR
        .iter()
        .filter(|m| !before.contains(&m.version))
        .collect();
    for m in &pending {
        tracing::info!(version = m.version, description = %m.description, "migration pending");
    }
    // Run even with nothing pending: this is where an applied migration whose
    // file changed is caught, as `sqlx migrate run` catches it — and it is
    // checked before anything is applied.
    MIGRATOR.run(&mut *conn).await.map_err(explain)?;
    let after: Vec<i64> = applied(conn).await?.iter().map(|a| a.version).collect();
    let applied: Vec<i64> = pending
        .iter()
        .map(|m| m.version)
        .filter(|v| after.contains(v))
        .collect();
    if applied.is_empty() {
        tracing::info!(in_place = after.len(), "schema is current");
    } else {
        tracing::info!(?applied, in_place = after.len(), "schema migrated");
    }
    let extensions = update_extensions(conn).await;
    // The binary carries every migration in the ledger, so the database is
    // now this release's (`instance_release`, which client tools compare with).
    let release = Release::this_build().to_string();
    let commit = crate::config::build_commit();
    sqlx::query(
        "INSERT INTO instance_release (id, version, commit, dirty) VALUES (true, $1, $2, $3) \
         ON CONFLICT (id) DO UPDATE SET version = EXCLUDED.version, commit = EXCLUDED.commit, \
         dirty = EXCLUDED.dirty, recorded_at = now()",
    )
    .bind(&release)
    .bind(commit.hash().map(|h| h.as_ref().to_owned()))
    .bind(commit.dirty())
    .execute(&mut *conn)
    .await
    .map_err(db("recording the instance's release"))?;
    Ok(Report {
        applied,
        already: before.len(),
        ahead: Vec::new(),
        extensions,
    })
}

/// `ALTER EXTENSION … UPDATE` for each of [`EXTENSIONS`] whose installed
/// version is not the server's default (the one its files carry). Never fails
/// the run, but says what it found:
///
/// * an extension the server has no files for (a `db` image without
///   pgvector): ERROR, since every query that uses it fails;
/// * no update path from the installed version to the server's (an image
///   older than the extension, which has no path down): ERROR, since the
///   older library may lack what the newer SQL objects call. The path, not
///   the versions the server lists: pgvector ships install scripts for its
///   current version only, with update scripts from every earlier one;
/// * an update that fails (a role that does not own the extension, as on a
///   managed server): WARN, since the older objects keep working with the
///   newer library.
async fn update_extensions(conn: &mut PgConnection) -> Vec<ExtensionUpdate> {
    let rows: Vec<(String, String, Option<String>, bool)> = match sqlx::query_as(
        // pg_extension_update_paths raises for an extension with no control
        // file, so it is asked only about one the server has files for.
        "SELECT e.extname::text, e.extversion, a.default_version, \
                CASE WHEN a.default_version IS NULL THEN false ELSE \
                  EXISTS (SELECT 1 FROM pg_extension_update_paths(e.extname) p \
                          WHERE p.source = e.extversion AND p.target = a.default_version \
                            AND p.path IS NOT NULL) END \
         FROM pg_extension e LEFT JOIN pg_available_extensions a ON a.name = e.extname \
         WHERE e.extname = ANY($1)",
    )
    .bind(EXTENSIONS.map(|(name, _)| name))
    .fetch_all(&mut *conn)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "reading the extensions' versions; not updating them");
            return Vec::new();
        }
    };
    let mut updated = Vec::new();
    for (name, update) in EXTENSIONS {
        let Some((_, from, available, has_path)) = rows.iter().find(|(n, ..)| n == name) else {
            continue;
        };
        let Some(to) = available else {
            tracing::error!(
                extension = name,
                installed = %from,
                "the database image has no files for this extension, so every query using it fails: \
                 run the pgvector image docker-compose.yml pins (docs/DEPLOYMENT.md §8, Database image)"
            );
            continue;
        };
        if from == to {
            continue;
        }
        if !has_path {
            tracing::error!(
                extension = name,
                installed = %from,
                available = %to,
                "the database image is older than the extension, which has no way back down, and its \
                 library may lack what the extension calls: run an image carrying the installed version \
                 or newer, or restore the backup taken before the update (docs/DEPLOYMENT.md §8, Database image)"
            );
            continue;
        }
        match sqlx::query(update).execute(&mut *conn).await {
            Ok(_) => {
                tracing::info!(extension = name, %from, %to, "extension updated");
                updated.push(ExtensionUpdate {
                    name,
                    from: from.clone(),
                    to: to.clone(),
                });
            }
            Err(e) => tracing::warn!(
                extension = name,
                installed = %from,
                available = %to,
                error = %e,
                "could not update the extension to the version the database image carries; \
                 it keeps working at the installed one (docs/DEPLOYMENT.md §8, Database image)"
            ),
        }
    }
    updated
}

/// The ledger's versions this binary does not carry: the database was
/// migrated by a newer release.
fn unknown_to_binary(ledger: &[i64]) -> Vec<i64> {
    ledger
        .iter()
        .copied()
        .filter(|v| !MIGRATOR.version_exists(*v))
        .collect()
}

/// How the database's schema stands against this binary's migrations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Skew {
    /// Versions in the ledger this binary does not carry (a newer release
    /// migrated the database), as [`Error::Ahead`] names them.
    pub ahead: Vec<i64>,
    /// Versions this binary carries that the ledger lacks (migrations still
    /// pending: `JUDGE_AUTO_MIGRATE=false` and nobody ran `judgebot ingest
    /// migrate`). A database with no ledger at all lacks every one.
    pub behind: Vec<i64>,
}

impl Skew {
    /// The schema is exactly the one this binary was built against.
    #[must_use]
    pub const fn current(&self) -> bool {
        self.ahead.is_empty() && self.behind.is_empty()
    }

    /// Why a job must not write, for the operator, or `None` when the schema
    /// is current.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        if !self.ahead.is_empty() {
            Some(format!(
                "a newer release migrated the database (migrations {:?} are unknown to this binary), and this \
                 binary does not write into a schema it does not know: run the newer image",
                self.ahead
            ))
        } else if !self.behind.is_empty() {
            Some(format!(
                "the schema is behind this binary (migrations {:?} are pending, JUDGE_AUTO_MIGRATE=false?): \
                 run `judgebot ingest migrate`",
                self.behind
            ))
        } else {
            None
        }
    }
}

/// Read the ledger and compare it with [`MIGRATOR`], changing nothing (no
/// lock, no ledger table created): what an unattended job checks before it
/// writes, so a process left running after a newer release migrated the
/// database never writes into a schema it does not know.
///
/// # Errors
/// [`Error::Db`] / [`Error::Migrate`] when the ledger cannot be read.
pub async fn skew(pool: &PgPool) -> Result<Skew, Error> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(db("connecting to read the migration ledger"))?;
    let ledger: Vec<i64> = match conn
        .list_applied_migrations(MIGRATOR.table_name.as_ref())
        .await
    {
        Ok(applied) => applied.iter().map(|a| a.version).collect(),
        Err(MigrateError::Execute(sqlx::Error::Database(e)))
            if e.code().as_deref() == Some(UNDEFINED_TABLE) =>
        {
            Vec::new()
        }
        Err(e) => return Err(explain(e)),
    };
    Ok(Skew {
        ahead: unknown_to_binary(&ledger),
        behind: MIGRATOR
            .iter()
            .filter(|m| !m.migration_type.is_down_migration())
            .map(|m| m.version)
            .filter(|v| !ledger.contains(v))
            .collect(),
    })
}

/// Postgres's SQLSTATE for a missing table.
const UNDEFINED_TABLE: &str = "42P01";

/// What `instance_release` records: the release and the commit of the binary
/// that last migrated the database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceRelease {
    /// Its release.
    pub release: Release,
    /// Its commit, when the build was stamped.
    pub commit: Option<CommitHash>,
    /// Whether that build's tree had uncommitted changes.
    pub dirty: bool,
}

impl InstanceRelease {
    /// `2.0.0-dev (commit 6573c59)`, or the release alone.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.commit {
            Some(c) if self.dirty => {
                format!(
                    "{} (commit {}, with uncommitted changes)",
                    self.release,
                    c.short()
                )
            }
            Some(c) => format!("{} (commit {})", self.release, c.short()),
            None => self.release.to_string(),
        }
    }
}

/// The release recorded as having last migrated the database
/// (`instance_release`), or `None` when none is (a database migrated before
/// the table existed, or by `sqlx migrate run`). A recorded value that is not
/// semver is `None` too, with a WARN. Read as JSON, so a table from before
/// its `commit` column reads as having no commit.
///
/// # Errors
/// [`Error::Db`] for anything but a missing table.
pub async fn instance_release<'e>(
    exec: impl sqlx::PgExecutor<'e>,
) -> Result<Option<InstanceRelease>, Error> {
    let row: Result<Option<serde_json::Value>, sqlx::Error> =
        sqlx::query_scalar("SELECT to_jsonb(r) FROM instance_release r WHERE id")
            .fetch_optional(exec)
            .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return Ok(None),
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(UNDEFINED_TABLE) => {
            return Ok(None);
        }
        Err(error) => {
            return Err(Error::Db {
                what: "reading the instance's release",
                error,
            });
        }
    };
    let field = |k: &str| row.get(k).and_then(serde_json::Value::as_str);
    let version = field("version").unwrap_or_default();
    match Release::parse(version) {
        Ok(release) => Ok(Some(InstanceRelease {
            release,
            commit: field("commit").and_then(|c| CommitHash::try_new(c.to_owned()).ok()),
            dirty: row
                .get("dirty")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })),
        Err(e) => {
            tracing::warn!(version, error = %e, "instance_release holds a version that is not semver");
            Ok(None)
        }
    }
}

/// Where a client tool of this build stands against the database
/// ([`judge_core::release::compatibility`]), with the instance's record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientCheck {
    /// The verdict.
    pub compatibility: Compatibility,
    /// What the database records, when it records anything.
    pub instance: Option<InstanceRelease>,
}

/// Where a client tool of this build stands against the database: its
/// recorded release, and which migrations it lacks or holds beyond this
/// build's.
///
/// # Errors
/// [`Error`] from reading the ledger or the release.
pub async fn client_compatibility(pool: &PgPool) -> Result<ClientCheck, Error> {
    let skew = skew(pool).await?;
    let instance = instance_release(pool).await?;
    Ok(ClientCheck {
        compatibility: release::compatibility(
            &Release::this_build(),
            instance.as_ref().map(|i| &i.release),
            release::Ledger {
                lacks: !skew.behind.is_empty(),
                unknown: !skew.ahead.is_empty(),
            },
        ),
        instance,
    })
}

/// Why this `judgebot` must not start against a database holding
/// migrations it does not know, recorded as migrated by `recorded`, or `None`
/// when it may. Within a major every migration only adds, so a rollback
/// there runs. Across a major, between builds of a new major's pre-release,
/// or with nothing recorded to say the migrations only add, it is refused:
/// that schema may no longer hold what this build reads and writes.
#[must_use]
pub fn rollback_refusal(recorded: Option<&InstanceRelease>) -> Option<String> {
    refusal_for(&Release::this_build(), recorded)
}

/// [`rollback_refusal`] for a build of release `this`.
fn refusal_for(this: &Release, recorded: Option<&InstanceRelease>) -> Option<String> {
    let verdict = release::compatibility(
        this,
        recorded.map(|i| &i.release),
        release::Ledger {
            lacks: false,
            unknown: true,
        },
    );
    let Compatibility::Incompatible(why) = verdict else {
        return None;
    };
    let this = describe_build(this);
    let restore = "or restore the backup taken before the upgrade (docs/DEPLOYMENT.md §6)";
    let by = || recorded.map_or_else(String::new, InstanceRelease::describe);
    Some(match why {
        release::Incompatible::Major { .. } => format!(
            "the database was migrated by judgebot {}, another major release, and this judgebot {this} \
             cannot run against its schema: run that release again, {restore}",
            by()
        ),
        release::Incompatible::Prerelease { .. } => format!(
            "the database was migrated by judgebot {}, and this judgebot {this} cannot run against it: \
             a pre-release of a new major works only with the database its own build migrated. Run that \
             build again, {restore}",
            by()
        ),
        release::Incompatible::UnknownBuild { .. } if recorded.is_some() => format!(
            "the database holds migrations this judgebot {this} does not know, made by another build of {}: \
             run that build again, {restore}",
            by()
        ),
        release::Incompatible::UnknownBuild { .. }
        | release::Incompatible::InstanceBehind { .. } => {
            format!(
                "the database holds migrations this judgebot {this} does not know, and records no release \
                 that made them (a migration run with `sqlx migrate run` records none): run the build that \
                 migrated it, or `judgebot ingest migrate` from it to record its release, {restore}"
            )
        }
    })
}

/// What a startup's migration is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Upgrade {
    /// Nothing to migrate, a fresh database, or a migration that only adds
    /// (within a major): run it and serve, beside the previous release.
    InPlace,
    /// A new major's migrations (or a pre-2.0 database, which records no
    /// release): the previous release must have stopped before they run,
    /// and the process serves maintenance replies until then
    /// ([`crate::maintenance`]).
    Major {
        /// What the database records, when anything.
        from: Option<InstanceRelease>,
    },
}

/// Whether the migrations this binary would apply are a major upgrade's.
///
/// # Errors
/// [`Error`] from reading the ledger or the release.
pub async fn upgrade(pool: &PgPool) -> Result<Upgrade, Error> {
    let skew = skew(pool).await?;
    let carried = MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .count();
    // Nothing pending, a rollback (decided by `at_startup`), or an empty
    // database with nothing running against it.
    if skew.behind.is_empty() || !skew.ahead.is_empty() || skew.behind.len() == carried {
        return Ok(Upgrade::InPlace);
    }
    let from = instance_release(pool).await?;
    let this = Release::this_build();
    let major = match &from {
        None => true,
        Some(r) => {
            r.release.major() != this.major()
                || r.release.is_major_prerelease()
                || this.is_major_prerelease()
        }
    };
    Ok(if major {
        Upgrade::Major { from }
    } else {
        Upgrade::InPlace
    })
}

/// [`rollback_refusal`] for a startup that does not migrate
/// (`JUDGE_AUTO_MIGRATE=false`); a migrating startup decides it under the
/// migration lock instead.
///
/// # Errors
/// [`Error::Rollback`]; [`Error`] from reading the ledger or the release.
pub async fn check_startup(pool: &PgPool) -> Result<(), Error> {
    if skew(pool).await?.ahead.is_empty() {
        return Ok(());
    }
    match rollback_refusal(instance_release(pool).await?.as_ref()) {
        Some(why) => Err(Error::Rollback(why)),
        None => Ok(()),
    }
}

/// A build of `release` at this binary's commit, as
/// [`InstanceRelease::describe`] writes one.
fn describe_build(release: &Release) -> String {
    let commit = crate::config::build_commit();
    InstanceRelease {
        release: release.clone(),
        commit: commit.hash().cloned(),
        dirty: commit.dirty(),
    }
    .describe()
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

/// `JUDGE_AUTO_MIGRATE`'s value: unset or blank is on.
///
/// # Errors
/// [`Error::BadFlag`] for anything but the accepted spellings.
pub fn parse_flag(raw: Option<&str>) -> Result<bool, Error> {
    let Some(raw) = raw else { return Ok(true) };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(Error::BadFlag(raw.to_owned())),
    }
}

/// What `judgebot` does first: refuse a database another major (or another
/// pre-release build) migrated ([`check_startup`]), then migrate unless
/// opted out, tolerating a database a newer release of the same major is
/// ahead of (see the module docs).
///
/// # Errors
/// [`Error`] as [`run`] with [`Ahead::Skip`], [`Error::Rollback`], or a bad
/// opt-out value.
pub async fn at_startup(pool: &PgPool) -> Result<Report, Error> {
    if !auto_migrate_enabled()? {
        check_startup(pool).await?;
        tracing::info!(
            "{AUTO_MIGRATE_ENV} is off; schema left as it is (run `judgebot ingest migrate` yourself)"
        );
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

    /// An extension created at an older version than the server's files carry
    /// (what a new `db` image leaves) is updated by the next run, once; the
    /// migration's `CREATE EXTENSION IF NOT EXISTS` alone would leave it.
    #[sqlx::test(migrations = false)]
    async fn a_run_updates_an_extension_behind_the_servers_files(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let default: String = sqlx::query_scalar(
            "SELECT default_version FROM pg_available_extensions WHERE name = 'pg_trgm'",
        )
        .fetch_one(&pool)
        .await?;
        // pg_trgm installs at 1.3 and updates from there on every supported server.
        anyhow::ensure!(default != "1.3", "the server's pg_trgm is 1.3 already");
        sqlx::query("CREATE EXTENSION pg_trgm VERSION '1.3'")
            .execute(&pool)
            .await?;

        let first = run(&pool, Ahead::Refuse).await?;
        assert_eq!(
            first.extensions,
            vec![ExtensionUpdate {
                name: "pg_trgm",
                from: "1.3".to_owned(),
                to: default.clone(),
            }]
        );
        let now: String =
            sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname = 'pg_trgm'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(now, default);

        let second = run(&pool, Ahead::Refuse).await?;
        assert!(second.extensions.is_empty(), "{:?}", second.extensions);
        Ok(())
    }

    /// The read-only check: no ledger is behind everything, a migrated one
    /// is current, and an unknown version is ahead. It creates nothing.
    #[sqlx::test(migrations = false)]
    async fn the_skew_reads_the_ledger_without_touching_it(pool: PgPool) -> anyhow::Result<()> {
        let none = skew(&pool).await?;
        assert_eq!(none.behind, all_versions());
        assert!(none.ahead.is_empty() && !none.current());
        let created: bool =
            sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
                .fetch_one(&pool)
                .await?;
        assert!(!created, "reading made no ledger");
        run(&pool, Ahead::Refuse).await?;
        assert!(skew(&pool).await?.current());
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES ($1, 'future', true, $2, 0)",
        )
        .bind(99_990_101_000_001_i64)
        .bind(&b"future"[..])
        .execute(&pool)
        .await?;
        let ahead = skew(&pool).await?;
        assert_eq!(
            (ahead.ahead, ahead.behind),
            (vec![99_990_101_000_001], Vec::new())
        );
        Ok(())
    }

    #[sqlx::test(migrations = false)]
    async fn an_empty_database_gets_every_migration_and_a_second_run_does_nothing(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let first = run(&pool, Ahead::Refuse).await?;
        assert_eq!(first.applied, all_versions());
        assert_eq!(first.already, 0);
        let mut conn = pool.acquire().await?;
        let ledger: Vec<i64> = applied(&mut conn)
            .await?
            .iter()
            .map(|a| a.version)
            .collect();
        assert_eq!(ledger, all_versions());
        // The schema is real, not just the ledger.
        let one: bool = sqlx::query_scalar("SELECT to_regclass('embedding_space') IS NOT NULL")
            .fetch_one(&pool)
            .await?;
        assert!(one);

        let second = run(&pool, Ahead::Refuse).await?;
        assert_eq!(
            second,
            Report {
                applied: Vec::new(),
                already: all_versions().len(),
                ahead: Vec::new(),
                extensions: Vec::new(),
            }
        );
        Ok(())
    }

    /// A run records this build's release; a client of this build is current
    /// against it, stops before it, and is told about a newer one of its major.
    #[sqlx::test(migrations = false)]
    async fn a_run_records_the_release_client_tools_compare_with(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let this = Release::this_build();
        assert_eq!(instance_release(&pool).await?, None, "no table yet");
        assert_eq!(
            client_compatibility(&pool).await?.compatibility,
            Compatibility::Incompatible(release::Incompatible::InstanceBehind {
                client: this.clone(),
                instance: None
            })
        );
        check_startup(&pool).await?;
        run(&pool, Ahead::Refuse).await?;
        let recorded = instance_release(&pool).await?;
        assert_eq!(recorded.as_ref().map(|r| &r.release), Some(&this));
        assert_eq!(
            recorded
                .and_then(|r| r.commit)
                .map(|c| c.as_ref().to_owned()),
            crate::config::build_commit()
                .hash()
                .map(|h| h.as_ref().to_owned()),
            "the build's commit is recorded beside its release"
        );
        assert_eq!(
            client_compatibility(&pool).await?.compatibility,
            Compatibility::Current
        );
        let set = |v: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query("UPDATE instance_release SET version = $1")
                    .bind(v)
                    .execute(&pool)
                    .await
            }
        };
        set("99.0.0").await?;
        assert!(matches!(
            client_compatibility(&pool).await?.compatibility,
            Compatibility::Incompatible(release::Incompatible::Major { .. })
        ));
        set("not a version").await?;
        assert_eq!(instance_release(&pool).await?, None);
        // A rolled-back image that finds unknown migrations leaves the record.
        set("99.0.0").await?;
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES ($1, 'future', true, $2, 0)",
        )
        .bind(99_990_101_000_001_i64)
        .bind(&b"future"[..])
        .execute(&pool)
        .await?;
        // ... and refuses to start against it: a rollback across a major
        // cannot work. The decision is made under the migration lock.
        assert!(matches!(
            run(&pool, Ahead::Skip).await,
            Err(Error::Rollback(_))
        ));
        assert_eq!(
            instance_release(&pool).await?.map(|r| r.release),
            Some(Release::parse("99.0.0")?)
        );
        let refused = check_startup(&pool).await;
        assert!(
            matches!(&refused, Err(Error::Rollback(why)) if why.contains("99.0.0") && why.contains("another major")),
            "{refused:?}"
        );
        assert!(matches!(at_startup(&pool).await, Err(Error::Rollback(_))));
        Ok(())
    }

    /// A fresh database, a current one and a rollback migrate in place; a
    /// database with migrations pending is a major upgrade when it records
    /// no release (from before 2.0), another major, or a new major's
    /// pre-release, and in place within a major otherwise.
    #[sqlx::test(migrations = false)]
    async fn a_major_upgrade_is_told_from_one_in_place(pool: PgPool) -> anyhow::Result<()> {
        assert_eq!(upgrade(&pool).await?, Upgrade::InPlace, "a fresh database");
        let versions = all_versions();
        let Some(&first) = versions.first() else {
            return Err(anyhow::anyhow!("no migrations embedded"));
        };
        MIGRATOR.run_to(first, &pool).await?;
        assert!(
            matches!(upgrade(&pool).await?, Upgrade::Major { from: None }),
            "pending, from a release that records none"
        );
        run(&pool, Ahead::Refuse).await?;
        assert_eq!(upgrade(&pool).await?, Upgrade::InPlace, "nothing pending");
        // Pending again: forget the last migration, as an older release left it.
        let Some(&last) = versions.last() else {
            return Err(anyhow::anyhow!("no migrations embedded"));
        };
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
            .bind(last)
            .execute(&pool)
            .await?;
        let this = Release::this_build();
        let older_major = format!("{}.0.0", this.major().saturating_sub(1).max(1));
        let same_major = format!("{}.0.0", this.major());
        for (recorded, major) in [
            (older_major.as_str(), this.major() > 1),
            (same_major.as_str(), this.is_major_prerelease()),
        ] {
            sqlx::query("UPDATE instance_release SET version = $1")
                .bind(recorded)
                .execute(&pool)
                .await?;
            let got = upgrade(&pool).await?;
            assert_eq!(
                matches!(got, Upgrade::Major { .. }),
                major,
                "{recorded} to {this}: {got:?}"
            );
        }
        Ok(())
    }

    #[sqlx::test(migrations = false)]
    async fn a_partly_migrated_database_gets_only_the_rest(pool: PgPool) -> anyhow::Result<()> {
        let versions = all_versions();
        let Some(&first) = versions.first() else {
            return Err(anyhow::anyhow!("no migrations embedded"));
        };
        MIGRATOR.run_to(first, &pool).await?;
        let r = run(&pool, Ahead::Refuse).await?;
        assert_eq!(r.already, 1);
        assert_eq!(r.applied, versions.get(1..).unwrap_or_default());
        Ok(())
    }

    #[sqlx::test(migrations = false)]
    async fn a_migration_applied_from_a_changed_file_is_refused(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        MIGRATOR.run(&pool).await?;
        let Some(m) = MIGRATOR.iter().last() else {
            return Err(anyhow::anyhow!("no migrations embedded"));
        };
        // Forge the ledger: the last migration "ran" from different SQL.
        sqlx::query("UPDATE _sqlx_migrations SET checksum = $1 WHERE version = $2")
            .bind(&b"not the file"[..])
            .bind(m.version)
            .execute(&pool)
            .await?;
        assert!(
            matches!(run(&pool, Ahead::Refuse).await, Err(Error::VersionMismatch(v)) if v == m.version)
        );
        // Startup is just as strict about a changed file: only "ahead" is tolerated.
        assert!(matches!(
            run(&pool, Ahead::Skip).await,
            Err(Error::VersionMismatch(_))
        ));
        // A failed run must not leave a connection holding sqlx's migrator
        // lock (a session advisory lock) in the pool, or this third run would
        // block forever. A closed connection's session ends, and lets go of
        // the lock, a moment after the close.
        crate::lease::testing::eventually("no advisory lock left", || async {
            let held: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory'
                   AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
            )
            .fetch_one(&pool)
            .await?;
            Ok((held == 0).then_some(()))
        })
        .await?;
        let third = run(&pool, Ahead::Skip).await;
        assert!(matches!(third, Err(Error::VersionMismatch(_))), "{third:?}");
        Ok(())
    }

    #[sqlx::test(migrations = false)]
    async fn a_database_ahead_of_the_binary_is_refused_explicitly_and_skipped_at_startup(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        MIGRATOR.run(&pool).await?;
        // A version from a "newer release", as its migrator would have recorded it.
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES ($1, 'future', true, $2, 0)",
        )
        .bind(99_990_101_000_001_i64)
        .bind(&b"future"[..])
        .execute(&pool)
        .await?;
        assert!(
            matches!(run(&pool, Ahead::Refuse).await, Err(Error::Ahead(v)) if v == [99_990_101_000_001])
        );
        // Startup skips it when a newer release of this build's major made
        // it, and refuses otherwise (here nothing is recorded: refused).
        assert!(matches!(
            run(&pool, Ahead::Skip).await,
            Err(Error::Rollback(_))
        ));
        let newer_minor = Release::this_build().major().to_string() + ".999.0";
        sqlx::query("INSERT INTO instance_release (id, version) VALUES (true, $1)")
            .bind(&newer_minor)
            .execute(&pool)
            .await?;
        let skipped = run(&pool, Ahead::Skip).await;
        if Release::this_build().is_major_prerelease() {
            assert!(matches!(skipped, Err(Error::Rollback(_))), "{skipped:?}");
        } else {
            let r = skipped?;
            assert_eq!(r.ahead, [99_990_101_000_001]);
            assert!(r.applied.is_empty());
            assert_eq!(r.already, all_versions().len() + 1);
        }
        // Ahead does not excuse a changed known file: the rolled-back image
        // must not boot against a schema its own migration did not produce.
        let Some(m) = MIGRATOR.iter().last() else {
            return Err(anyhow::anyhow!("no migrations embedded"));
        };
        sqlx::query("UPDATE _sqlx_migrations SET checksum = $1 WHERE version = $2")
            .bind(&b"rewritten"[..])
            .bind(m.version)
            .execute(&pool)
            .await?;
        assert!(
            matches!(run(&pool, Ahead::Skip).await, Err(Error::VersionMismatch(v)) if v == m.version)
        );
        Ok(())
    }

    /// A rollback within a major runs; across one, between builds of a new
    /// major's pre-release, or with nothing recorded, it is refused.
    #[test]
    fn a_rollback_runs_only_within_a_major() -> anyhow::Result<()> {
        let recorded = |v: &str| -> anyhow::Result<InstanceRelease> {
            Ok(InstanceRelease {
                release: Release::parse(v)?,
                commit: None,
                dirty: false,
            })
        };
        let refused = |this: &str, by: Option<&str>| -> anyhow::Result<Option<String>> {
            let by = by.map(recorded).transpose()?;
            Ok(refusal_for(&Release::parse(this)?, by.as_ref()))
        };
        for (this, by) in [
            ("2.1.0", "2.3.0"),
            ("2.0.0", "2.1.0-dev"),
            ("2.1.0-dev", "2.1.0-dev"),
            ("2.1.0", "2.1.0"),
        ] {
            assert_eq!(refused(this, Some(by))?, None, "{this} after {by}");
        }
        assert!(refused("2.3.0", Some("3.0.0"))?.is_some_and(|w| w.contains("another major")));
        assert!(
            refused("3.0.0-dev", Some("3.0.0-dev"))?
                .is_some_and(|w| w.contains("another build of 3.0.0-dev"))
        );
        assert!(refused("2.9.0", Some("3.0.0-dev"))?.is_some());
        assert!(refused("2.1.0", None)?.is_some_and(|w| w.contains("records no release")));
        Ok(())
    }

    #[test]
    fn the_opt_out_flag_is_a_boolean_with_on_as_the_default() {
        for on in [
            None,
            Some(""),
            Some(" "),
            Some("true"),
            Some("1"),
            Some("YES"),
            Some("on"),
        ] {
            assert!(matches!(parse_flag(on), Ok(true)), "{on:?}");
        }
        for off in [Some("false"), Some("0"), Some("No"), Some("off ")] {
            assert!(matches!(parse_flag(off), Ok(false)), "{off:?}");
        }
        assert!(matches!(parse_flag(Some("maybe")), Err(Error::BadFlag(s)) if s == "maybe"));
    }
}
