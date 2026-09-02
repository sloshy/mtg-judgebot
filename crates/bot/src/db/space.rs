//! The vector space the database holds, and [`Vectors`], the one way the
//! adapters reach an embedder (`docs/proposals/providers.md` §4.3).
//!
//! `embedding_space` is a one-row table naming the provider, model and width
//! of every stored vector. It is written once by `ingest embed` (first use),
//! rewritten only by `ingest reembed` ([`switch_space`], which also changes
//! the columns' `vector(N)` and clears them, in one transaction), and read
//! by everything else. The comparison is [`Space::check`], pure and shared,
//! so the retriever, the call store and ingest agree on what a mismatch is.
//!
//! [`Vectors`] wraps a [`WithSpace`] embedder with that check: it embeds
//! nothing until the stored space equals its own. On a mismatch the
//! vector legs are off with an error-level log naming both spaces — a wrong
//! answer from a mixed space would be silent, a missing leg is not — and on
//! an absent row (nothing embedded yet) they are off with a warning until
//! `ingest embed` writes it. The row is re-read on every use (one primary-key
//! read of a one-row table, beside the seven legs a retrieval already runs),
//! so a running bot picks up the first `ingest embed`, and a `reembed` under
//! a running bot darkens its legs instead of erroring on the new width or
//! mixing spaces at the old one; the log line fires on the transition, not
//! on every request.
//!
//! A read can only be stale by one request; a *write* must not be stale at
//! all, or a `reembed` committing between the check and the `INSERT` would
//! store an old-space vector under the new row. So every writer of a vector
//! column — `PgCallStore::persist`, each `ingest embed` batch — takes the
//! shared side of [`CALLS_REWRITE_LOCK`] inside its transaction and reads
//! the row under it ([`hold_space`], [`Vectors::hold`]), while
//! [`switch_space`] takes the exclusive side first: a switch waits for the
//! in-flight writes, and a write that starts after it sees the new row. The
//! CR loader and the retirement pass hold the same lock, so the switch also
//! serialises with them instead of deadlocking on the `calls` table.

use std::{
    fmt,
    sync::{Arc, Mutex},
};

use judge_core::{InputKind, JudgeError};
use judge_embed::{Provider, Space, WithSpace};
use pgvector::Vector;
use sqlx::{PgConnection, PgPool};

use super::{CALLS_REWRITE_LOCK, bad_row, upstream};

/// One table with an `embedding` column, and its index exactly as the
/// migrations define it (`20260829000003_rules.sql`, `20260829000004_calls.sql`,
/// `20260829000005_rules_embedding_partial.sql`): [`switch_space`] recreates
/// these, so the definitions live here beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VectorTable {
    /// The table.
    pub table: &'static str,
    /// The HNSW index on its `embedding` column.
    pub index: &'static str,
    /// The `CREATE INDEX` statement, verbatim from the migration.
    pub create_index: &'static str,
}

/// Every table with an `embedding` column.
pub const VECTOR_TABLES: [VectorTable; 3] = [
    VectorTable {
        table: "rules",
        index: "rules_embedding_idx",
        create_index: "CREATE INDEX rules_embedding_idx ON rules USING hnsw (embedding vector_cosine_ops) WHERE parent_id IS NULL",
    },
    VectorTable {
        table: "glossary",
        index: "glossary_embedding_idx",
        create_index: "CREATE INDEX glossary_embedding_idx ON glossary USING hnsw (embedding vector_cosine_ops)",
    },
    VectorTable {
        table: "calls",
        index: "calls_embedding_idx",
        create_index: "CREATE INDEX calls_embedding_idx ON calls USING hnsw (embedding vector_cosine_ops)",
    },
];

/// The space the stored vectors belong to, or `None` when nothing has been
/// embedded (no `embedding_space` row).
///
/// # Errors
/// `Upstream` from sqlx, or a row naming a provider this binary does not know.
pub async fn stored_space(exec: impl sqlx::PgExecutor<'_>) -> Result<Option<Space>, JudgeError> {
    let row = sqlx::query!("SELECT provider, model, dimensions FROM embedding_space")
        .fetch_optional(exec)
        .await
        .map_err(upstream("embedding_space"))?;
    row.map(|r| {
        Ok(Space {
            provider: r.provider.parse::<Provider>().map_err(|e| bad_row(format!("embedding_space: {e}")))?,
            model: r.model,
            dimensions: usize::try_from(r.dimensions).map_err(|_| bad_row(format!("embedding_space: dimensions {} out of range", r.dimensions)))?,
        })
    })
    .transpose()
}

/// Record `space` as the stored one: the first `ingest embed`. Fails when a
/// row exists (the primary key), so only [`switch_space`] ever replaces one.
///
/// # Errors
/// `Upstream` from sqlx, including the existing-row conflict.
pub async fn record_space(exec: impl sqlx::PgExecutor<'_>, space: &Space) -> Result<(), JudgeError> {
    let dimensions = i32::try_from(space.dimensions).map_err(|_| bad_row(format!("dimensions {} out of range", space.dimensions)))?;
    sqlx::query!("INSERT INTO embedding_space (provider, model, dimensions) VALUES ($1, $2, $3)", space.provider.as_str(), space.model, dimensions)
        .execute(exec)
        .await
        .map_err(upstream("record embedding_space"))?;
    Ok(())
}

/// Take the shared side of [`CALLS_REWRITE_LOCK`] for the rest of the
/// transaction on `conn` and read the stored space under it. A vector may be
/// written in this transaction exactly when the result is `Some(space)`: no
/// [`switch_space`] is in flight (it holds the exclusive side) and none can
/// commit before this transaction does. The lock is shared, so writers never
/// wait for each other; the HTTP call that produced the vector belongs
/// *before* this, so the lock is not held across the network.
///
/// # Errors
/// `Upstream` from sqlx, or an unreadable row as [`stored_space`].
pub async fn hold_space(conn: &mut PgConnection) -> Result<Option<Space>, JudgeError> {
    sqlx::query!("SELECT pg_advisory_xact_lock_shared($1)", CALLS_REWRITE_LOCK)
        .execute(&mut *conn)
        .await
        .map_err(upstream("lock embedding_space for a write"))?;
    stored_space(&mut *conn).await
}

/// The width of `table.embedding` from the catalogue (its `vector(N)` typmod),
/// which is the truth the migrations and [`switch_space`] leave behind.
///
/// # Errors
/// `Upstream` from sqlx, or a column that is not a `vector(N)`.
pub async fn column_width(exec: impl sqlx::PgExecutor<'_>, table: &str) -> Result<usize, JudgeError> {
    let ty = sqlx::query_scalar!(
        r#"
        SELECT format_type(a.atttypid, a.atttypmod) AS "type!"
        FROM pg_attribute a
        WHERE a.attrelid = to_regclass($1) AND a.attname = 'embedding' AND NOT a.attisdropped
        "#,
        table
    )
    .fetch_optional(exec)
    .await
    .map_err(upstream("embedding column type"))?
    .ok_or_else(|| bad_row(format!("{table}.embedding: no such column")))?;
    // `format_type` qualifies the name (`public.vector(1024)`) when the
    // extension's schema is not on the search path.
    let unqualified = ty.rsplit_once('.').map_or(ty.as_str(), |(_, t)| t);
    unqualified
        .strip_prefix("vector(")
        .and_then(|s| s.strip_suffix(')'))
        .and_then(|n| n.parse::<usize>().ok())
        .ok_or_else(|| bad_row(format!("{table}.embedding is {ty}, not a vector(N)")))
}

/// How many vectors each table holds, by table name.
///
/// # Errors
/// `Upstream` from sqlx.
pub async fn stored_counts(exec: impl sqlx::PgExecutor<'_>) -> Result<Vec<(&'static str, i64)>, JudgeError> {
    let r = sqlx::query!(
        r#"
        SELECT (SELECT count(*) FROM rules WHERE embedding IS NOT NULL) AS "rules!",
               (SELECT count(*) FROM glossary WHERE embedding IS NOT NULL) AS "glossary!",
               (SELECT count(*) FROM calls WHERE embedding IS NOT NULL) AS "calls!"
        "#
    )
    .fetch_one(exec)
    .await
    .map_err(upstream("count embeddings"))?;
    Ok(vec![("rules", r.rules), ("glossary", r.glossary), ("calls", r.calls)])
}

/// Switch the database to `space`, in one transaction: for every table in
/// [`VECTOR_TABLES`], drop the HNSW index, clear the column, retype it to
/// `vector(N)` and recreate the index as the migration defines it; then
/// replace the `embedding_space` row. Nothing survives partially: a failing
/// step (a width HNSW cannot index, say) leaves the old width, the old
/// vectors and the old row. `ingest reembed` runs this, then the embed loop.
///
/// The transaction holds the exclusive side of [`CALLS_REWRITE_LOCK`] from
/// its first statement: every in-flight vector write ([`hold_space`]) has
/// committed before the columns change, and a CR load or retirement pass
/// (which lock the same key) runs before or after, never interleaved.
///
/// # Errors
/// `Upstream` from sqlx; the transaction is rolled back.
pub async fn switch_space(pool: &PgPool, space: &Space) -> Result<(), JudgeError> {
    let dimensions = i32::try_from(space.dimensions).map_err(|_| bad_row(format!("dimensions {} out of range", space.dimensions)))?;
    let mut tx = pool.begin().await.map_err(upstream("begin reembed"))?;
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", CALLS_REWRITE_LOCK)
        .execute(&mut *tx)
        .await
        .map_err(upstream("lock vector columns for reembed"))?;
    for t in VECTOR_TABLES {
        // Identifiers are the constants above, never input; the width is a checked integer.
        for sql in [
            format!("DROP INDEX {}", t.index),
            format!("UPDATE {} SET embedding = NULL", t.table),
            format!("ALTER TABLE {} ALTER COLUMN embedding TYPE vector({dimensions})", t.table),
            t.create_index.to_owned(),
        ] {
            // `AssertSqlSafe`: the strings are built from the constants above and an integer.
            sqlx::query(sqlx::AssertSqlSafe(sql.clone())).execute(&mut *tx).await.map_err(|e| JudgeError::Upstream(anyhow::Error::new(e).context(sql)))?;
        }
    }
    sqlx::query!("DELETE FROM embedding_space").execute(&mut *tx).await.map_err(upstream("clear embedding_space"))?;
    record_space(&mut *tx, space).await?;
    tx.commit().await.map_err(upstream("commit reembed"))?;
    Ok(())
}

/// What the last check found; kept only so the log line fires on a change.
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    /// Not looked yet.
    Unchecked,
    /// The stored space is the embedder's: embed away.
    Enabled,
    /// No row: nothing embedded yet.
    Absent,
    /// The stored space differs.
    Mismatch(Space),
}

/// An embedder that embeds only into the space the database holds. Shared
/// (`Arc`) by the retriever, the library and the call store so a change is
/// logged once per process; the adapters hold no bare embedder, so there is
/// no path from a configured model to a vector column that skips it.
pub struct Vectors {
    pool: PgPool,
    embedder: Arc<dyn WithSpace>,
    state: Mutex<State>,
}

impl fmt::Debug for Vectors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock().map_or_else(|_| "<poisoned>".to_owned(), |s| format!("{s:?}"));
        f.debug_struct("Vectors").field("space", self.embedder.space()).field("state", &state).finish_non_exhaustive()
    }
}

impl Vectors {
    /// Guard `embedder` with the stored-space check over `pool`.
    #[must_use]
    pub fn new(pool: PgPool, embedder: Arc<dyn WithSpace>) -> Self {
        Self { pool, embedder, state: Mutex::new(State::Unchecked) }
    }

    /// The space the embedder writes into.
    #[must_use]
    pub fn space(&self) -> &Space {
        self.embedder.space()
    }

    fn state(&self) -> State {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    fn set_state(&self, s: State) {
        *self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = s;
    }

    /// Fold what a read of `embedding_space` found into the state, logging
    /// when it changed, and say whether the embedder may be used.
    fn observe(&self, stored: Result<Option<Space>, JudgeError>) -> bool {
        let configured = self.embedder.space();
        let stored = match stored {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, %configured, "could not read embedding_space; vector legs off for this request");
                return false;
            }
        };
        let after = match stored {
            None => State::Absent,
            Some(stored) => match configured.check(&stored) {
                Ok(()) => State::Enabled,
                Err(_) => State::Mismatch(stored),
            },
        };
        if after != self.state() {
            match &after {
                State::Enabled => tracing::info!(space = %configured, "embedding space matches the database; vector legs on"),
                State::Absent => tracing::warn!(space = %configured, "no embedding_space row: nothing embedded yet; vector legs off until `ingest embed` runs"),
                State::Mismatch(stored) => tracing::error!(%configured, %stored, "embedding space mismatch; vector legs off, nothing is mixed (run `ingest reembed --yes` to switch)"),
                State::Unchecked => {}
            }
            self.set_state(after.clone());
        }
        after == State::Enabled
    }

    /// Whether the embedder may be used: the stored space equals its own,
    /// as of now (one small query). The binaries call this once at startup
    /// so the verdict lands in the startup log beside the config summary.
    pub async fn enabled(&self) -> bool {
        self.observe(stored_space(&self.pool).await)
    }

    /// [`Self::enabled`] for a transaction that is about to write a vector:
    /// takes the shared lock and reads the row on `conn` ([`hold_space`]),
    /// so the answer cannot change before the transaction commits.
    ///
    /// # Errors
    /// `Upstream` from sqlx (the lock or the read).
    pub async fn hold(&self, conn: &mut PgConnection) -> Result<bool, JudgeError> {
        let stored = hold_space(conn).await?;
        Ok(self.observe(Ok(stored)))
    }

    /// Embed one text, or `None` (logged) when the space check fails, the
    /// embedder fails, or it returns something other than one vector of the
    /// space's width.
    pub async fn embed(&self, text: &str, kind: InputKind) -> Option<Vector> {
        if !self.enabled().await {
            return None;
        }
        let vectors = match self.embedder.embed(&[text], kind).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "embedding failed; skipping the vector leg");
                return None;
            }
        };
        let want = self.space().dimensions;
        match vectors.into_iter().next() {
            Some(v) if v.len() == want => Some(Vector::from(v)),
            Some(v) => {
                tracing::warn!(got = v.len(), want, "embedder returned a vector of the wrong width; skipping the vector leg");
                None
            }
            None => {
                tracing::warn!("embedder returned no vector; skipping the vector leg");
                None
            }
        }
    }
}
