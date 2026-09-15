//! [`PgCallStore`]: pipeline step 6 — persist validated verdicts and ratings.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use judge_core::{
    CallId, CallStore, Context, InputKind, JudgeError, Qa, Question, Score, Validated, Verdict,
    oracle_fingerprint,
};
use pgvector::Vector;
use sqlx::PgPool;

use super::{Vectors, bad_row, bad_row_from, upstream};
use crate::session::{PersistCall, SessionId};

/// Calls + ratings. Only `Verdict<Validated>` can be persisted (invariant I3).
#[derive(Clone)]
pub struct PgCallStore {
    pool: PgPool,
    vectors: Option<Arc<Vectors>>,
}

impl fmt::Debug for PgCallStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgCallStore")
            .field("vectors", &self.vectors.as_ref().map(|v| v.space()))
            .finish_non_exhaustive()
    }
}

impl PgCallStore {
    /// A store that leaves `calls.embedding` NULL (no embedder).
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            vectors: None,
        }
    }

    /// Embed each persisted question so later calls can be found by
    /// similarity, subject to the space check in [`Vectors`]: a call is
    /// stored without a vector rather than with one of another space
    /// (`ingest embed` fills it in later).
    #[must_use]
    pub fn with_vectors(mut self, vectors: Arc<Vectors>) -> Self {
        self.vectors = Some(vectors);
        self
    }

    async fn embed(&self, text: &str) -> Option<Vector> {
        self.vectors
            .as_ref()?
            .embed(text, InputKind::Document)
            .await
    }
}

/// The serde name of a unit-variant enum (`Source::Cr` → `cr`).
fn enum_id<T: serde::Serialize>(v: &T) -> Result<String, JudgeError> {
    match serde_json::to_value(v).map_err(|e| bad_row_from(e, "serialize enum"))? {
        serde_json::Value::String(s) => Ok(s),
        other => Err(bad_row(format!(
            "enum did not serialize to a string: {other}"
        ))),
    }
}

impl PgCallStore {
    /// The `INSERT` behind both [`CallStore::persist`] and
    /// [`PersistCall::persist_call`]. With a `session`, the row is keyed by it
    /// and a second insert for the same session returns the existing row (the
    /// partial unique index `calls_session_id_idx`), which is what makes an
    /// agent session's persist step idempotent in the database.
    async fn insert(
        &self,
        q: &Question,
        v: &Verdict<Validated>,
        ctx: &Context,
        session: Option<SessionId>,
    ) -> Result<CallId, JudgeError> {
        let citations = serde_json::to_value(v.citations())
            .map_err(|e| bad_row_from(e, "serialize citations"))?;
        let context_ids = serde_json::json!({
            "cards": ctx.cards.iter().map(|c| c.id).collect::<Vec<_>>(),
            "rules": ctx.rules.iter().map(|r| &r.id).collect::<Vec<_>>(),
            // `[[card, "<key>"]]`; rows written before migration 20260902000001 hold
            // `[[card, idx]]`. Nothing reads this member, it is a record of what the
            // model was shown.
            "rulings": ctx.rulings.iter().map(|r| (r.card, r.key)).collect::<Vec<_>>(),
            "prior": ctx.prior.iter().map(|p| p.id).collect::<Vec<_>>(),
            // `{ "<card uuid>": "<fingerprint>" }`: what each context card's Oracle
            // text was when this was answered. The retirement pass retires the
            // call when any of them has changed since (retire.rs); rows written
            // before this member existed carry no card dependency.
            "card_text": ctx.cards.iter().map(|c| (c.id.to_string(), oracle_fingerprint(c))).collect::<std::collections::BTreeMap<String, String>>(),
        });
        let source = enum_id(&v.source())?;
        let confidence = enum_id(&v.confidence())?;
        let cr_version: &str = v.cr_version().as_ref();
        // The HTTP call happens before the transaction; the space is then held
        // (shared lock, `db::space`) from the check to the insert, so a `reembed`
        // cannot commit in between and the vector, if kept, is of the row's space.
        let mut embedding = self.embed(&q.text).await;
        let mut tx = self.pool.begin().await.map_err(upstream("begin persist"))?;
        if embedding.is_some() {
            let held = match &self.vectors {
                Some(v) => v.hold(&mut tx).await?,
                None => false,
            };
            if !held {
                embedding = None;
            }
        }
        // `ON CONFLICT ... DO UPDATE` (a no-op set) rather than `DO NOTHING`
        // so that `RETURNING id` yields the existing row on conflict.
        let id = sqlx::query_scalar!(
            r#"
            INSERT INTO calls (thread_id, question, answer, category, source, confidence,
                               citations, context_ids, cr_version, embedding, session_id)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ON CONFLICT (session_id) WHERE session_id IS NOT NULL
            DO UPDATE SET session_id = EXCLUDED.session_id
            RETURNING id
            "#,
            q.thread_id,
            q.text,
            v.answer(),
            v.category().id(),
            source,
            confidence,
            citations,
            context_ids,
            cr_version,
            embedding as _,
            session.map(|s| s.0)
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(upstream("insert call"))?;
        tx.commit().await.map_err(upstream("commit persist"))?;
        tracing::info!(call = %id, thread = %q.thread_id, "call persisted");
        Ok(CallId::new(id))
    }
}

#[async_trait]
impl PersistCall for PgCallStore {
    async fn persist_call(
        &self,
        session: SessionId,
        q: &Question,
        v: &Verdict<Validated>,
        ctx: &Context,
    ) -> Result<CallId, JudgeError> {
        self.insert(q, v, ctx, Some(session)).await
    }
}

#[async_trait]
impl CallStore for PgCallStore {
    async fn persist(
        &self,
        q: &Question,
        v: &Verdict<Validated>,
        ctx: &Context,
    ) -> Result<CallId, JudgeError> {
        self.insert(q, v, ctx, None).await
    }

    async fn rate(
        &self,
        call: CallId,
        user_id: &str,
        score: Score,
        is_judge: bool,
    ) -> Result<(), JudgeError> {
        let score = i16::from(score as u8);
        sqlx::query!(
            r#"
            INSERT INTO ratings (call_id, user_id, score, is_judge)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (call_id, user_id)
            DO UPDATE SET score = EXCLUDED.score, is_judge = EXCLUDED.is_judge, ts = now()
            "#,
            call.into_inner(),
            user_id,
            score,
            is_judge
        )
        .execute(&self.pool)
        .await
        .map_err(upstream("upsert rating"))?;
        Ok(())
    }

    async fn forget_user(&self, user_id: &str) -> Result<u64, JudgeError> {
        let done = sqlx::query!("DELETE FROM ratings WHERE user_id = $1", user_id)
            .execute(&self.pool)
            .await
            .map_err(upstream("delete ratings"))?;
        Ok(done.rows_affected())
    }

    async fn history(&self, thread_id: &str, n: usize) -> Result<Vec<Qa>, JudgeError> {
        if n == 0 {
            return Ok(vec![]);
        }
        let limit = i64::try_from(n).unwrap_or(i64::MAX);
        // Newest `n` first, then reversed: the caller wants them oldest first.
        let rows = sqlx::query!(
            r#"
            SELECT question, answer
            FROM calls
            WHERE thread_id = $1
            ORDER BY created_at DESC
            LIMIT $2
            "#,
            thread_id,
            limit
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("thread history"))?;
        Ok(rows
            .into_iter()
            .rev()
            .map(|r| Qa {
                question: r.question,
                answer: r.answer,
            })
            .collect())
    }
}
