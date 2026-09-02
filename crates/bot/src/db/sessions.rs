//! [`PgSessionStore`]: agent-driven sessions (`crate::session`) as rows of
//! `agent_sessions`, one jsonb document each.
//!
//! Every step is load → transition → save. Two callers stepping the same
//! session at once would each load the same document and the second save
//! would silently discard the first's transition, so `save` is conditional on
//! the version the caller loaded: the loser gets a conflict, not a lost
//! update. Expired rows are invisible to `load` and swept opportunistically
//! on `insert`.

use std::time::Duration;

use judge_core::JudgeError;
use sqlx::PgPool;

use super::{bad_row_from, upstream};
use crate::session::{Session, SessionId};

/// Postgres-backed session store.
#[derive(Clone, Debug)]
pub struct PgSessionStore {
    pool: PgPool,
}

/// The row version a caller loaded, to be handed back to `save`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version(i32);

/// Whether `save` wrote the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Saved {
    /// Written; the version advanced.
    Yes,
    /// Not written: the row's version was not the one loaded (a concurrent
    /// step saved first), or the row expired or was removed.
    Conflict,
}

/// Longest TTL accepted; anything longer is clamped, so an over-long value
/// cannot turn into an out-of-range timestamp at the database.
pub const MAX_TTL: Duration = Duration::from_hours(24 * 30);

impl PgSessionStore {
    /// Over a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Store a new session, expiring `ttl` from now. Sweeps expired rows first.
    ///
    /// # Errors
    /// `Upstream` from sqlx or serde.
    pub async fn insert(&self, s: &Session, ttl: Duration) -> Result<(), JudgeError> {
        let swept = sqlx::query!("DELETE FROM agent_sessions WHERE expires_at < now()")
            .execute(&self.pool)
            .await
            .map_err(upstream("sweep expired sessions"))?
            .rows_affected();
        if swept > 0 {
            tracing::debug!(swept, "expired agent sessions removed");
        }
        let state = serde_json::to_value(&s.stage).map_err(|e| bad_row_from(e, "serialize session state"))?;
        sqlx::query!(
            r#"
            INSERT INTO agent_sessions (id, thread_id, question, state, expires_at)
            VALUES ($1, $2, $3, $4, now() + $5)
            "#,
            s.id.0,
            s.question.thread_id,
            s.question.text,
            state,
            ttl_interval(ttl)
        )
        .execute(&self.pool)
        .await
        .map_err(upstream("insert session"))?;
        Ok(())
    }

    /// The session, if it exists and has not expired.
    ///
    /// # Errors
    /// `Upstream` from sqlx or serde.
    pub async fn load(&self, id: SessionId) -> Result<Option<Session>, JudgeError> {
        Ok(self.load_versioned(id).await?.map(|(s, _)| s))
    }

    /// [`Self::load`] with the version to pass to [`Self::save`].
    ///
    /// # Errors
    /// `Upstream` from sqlx or serde.
    pub async fn load_versioned(&self, id: SessionId) -> Result<Option<(Session, Version)>, JudgeError> {
        let row = sqlx::query!(
            r#"
            SELECT thread_id, question, state, version
            FROM agent_sessions
            WHERE id = $1 AND expires_at >= now()
            "#,
            id.0
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(upstream("load session"))?;
        row.map(|r| {
            let stage = serde_json::from_value(r.state).map_err(|e| bad_row_from(e, format!("session {id}: state")))?;
            let session = Session {
                id,
                question: judge_core::Question { thread_id: r.thread_id, text: r.question },
                stage,
            };
            Ok((session, Version(r.version)))
        })
        .transpose()
    }

    /// Write the session back if nobody else has since the caller loaded
    /// `version`, bumping the version and pushing expiry out by `ttl`.
    ///
    /// # Errors
    /// `Upstream` from sqlx or serde. A lost race is [`Saved::Conflict`],
    /// not an error: the caller decides what it means for its step.
    pub async fn save(&self, s: &Session, version: Version, ttl: Duration) -> Result<Saved, JudgeError> {
        let state = serde_json::to_value(&s.stage).map_err(|e| bad_row_from(e, "serialize session state"))?;
        let updated = sqlx::query!(
            r#"
            UPDATE agent_sessions
               SET state = $2, version = version + 1, updated_at = now(), expires_at = now() + $4
             WHERE id = $1 AND version = $3 AND expires_at >= now()
            "#,
            s.id.0,
            state,
            version.0,
            ttl_interval(ttl)
        )
        .execute(&self.pool)
        .await
        .map_err(upstream("save session"))?
        .rows_affected();
        Ok(if updated == 0 { Saved::Conflict } else { Saved::Yes })
    }
}

/// `ttl` (clamped to [`MAX_TTL`]) as a Postgres interval.
fn ttl_interval(ttl: Duration) -> sqlx::postgres::types::PgInterval {
    sqlx::postgres::types::PgInterval {
        months: 0,
        days: 0,
        microseconds: i64::try_from(ttl.min(MAX_TTL).as_micros()).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{AgentThread, Stage};
    use judge_core::Qa;

    fn session() -> anyhow::Result<Session> {
        Ok(Session::begin(
            SessionId::new(),
            AgentThread::new(),
            "q".into(),
            vec![Qa { question: "a".into(), answer: "b".into() }],
        )?)
    }

    #[sqlx::test]
    async fn round_trips_and_detects_concurrent_steps(pool: PgPool) -> anyhow::Result<()> {
        let store = PgSessionStore::new(pool);
        let s = session()?;
        store.insert(&s, Duration::from_mins(1)).await?;
        let (loaded, v1) = store.load_versioned(s.id).await?.ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!(loaded, s);
        let mut moved = s.clone();
        moved.stage = Stage::Closed(crate::session::Outcome::OutOfScope { source: judge_core::Source::Tournament });
        assert_eq!(store.save(&moved, v1, Duration::from_mins(1)).await?, Saved::Yes);
        assert_eq!(store.load(s.id).await?, Some(moved.clone()));
        // A second save from the stale version loses.
        assert_eq!(store.save(&s, v1, Duration::from_mins(1)).await?, Saved::Conflict);
        let (_, v2) = store.load_versioned(s.id).await?.ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_ne!(v1, v2);
        assert_eq!(store.save(&moved, v2, Duration::from_mins(1)).await?, Saved::Yes);
        // An absurd TTL is clamped rather than rejected by Postgres.
        assert_eq!(store.save(&moved, Version(3), Duration::MAX).await?, Saved::Yes);
        Ok(())
    }

    #[sqlx::test]
    async fn expired_sessions_are_invisible_and_swept(pool: PgPool) -> anyhow::Result<()> {
        let store = PgSessionStore::new(pool.clone());
        let s = session()?;
        store.insert(&s, Duration::ZERO).await?;
        assert_eq!(store.load(s.id).await?, None, "expired at once");
        assert_eq!(store.save(&s, Version(1), Duration::from_mins(1)).await?, Saved::Conflict, "no resurrection through save");
        // The next insert sweeps it.
        store.insert(&session()?, Duration::from_mins(1)).await?;
        let n = sqlx::query_scalar!("SELECT count(*) AS \"n!\" FROM agent_sessions WHERE id = $1", s.id.0).fetch_one(&pool).await?;
        assert_eq!(n, 0);
        Ok(())
    }
}
