//! [`PgCallStore`]: pipeline step 6 — persist validated verdicts and ratings.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use judge_core::{
    CallId, CallStore, Context, Embedder, InputKind, JudgeError, Qa, Question, Score, Validated,
    Verdict,
};
use pgvector::Vector;
use sqlx::PgPool;

use super::{bad_row, bad_row_from, upstream};

/// Calls + ratings. Only `Verdict<Validated>` can be persisted (invariant I3).
#[derive(Clone)]
pub struct PgCallStore {
    pool: PgPool,
    embedder: Option<Arc<dyn Embedder>>,
}

impl fmt::Debug for PgCallStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgCallStore")
            .field("embedder", &self.embedder.is_some())
            .finish_non_exhaustive()
    }
}

impl PgCallStore {
    /// A store that leaves `calls.embedding` NULL (no embedder).
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            embedder: None,
        }
    }

    /// Embed each persisted question so later calls can be found by similarity.
    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    async fn embed(&self, text: &str) -> Option<Vector> {
        let embedder = self.embedder.as_ref()?;
        match embedder.embed(&[text], InputKind::Document).await {
            Ok(vectors) => vectors.into_iter().next().map(Vector::from),
            Err(e) => {
                tracing::warn!(error = %e, "embedding the call failed; storing it without an embedding");
                None
            }
        }
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

#[async_trait]
impl CallStore for PgCallStore {
    async fn persist(
        &self,
        q: &Question,
        v: &Verdict<Validated>,
        ctx: &Context,
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
        });
        let source = enum_id(&v.source())?;
        let confidence = enum_id(&v.confidence())?;
        let cr_version: &str = v.cr_version().as_ref();
        let embedding = self.embed(&q.text).await;
        let id = sqlx::query_scalar!(
            r#"
            INSERT INTO calls (thread_id, question, answer, category, source, confidence,
                               citations, context_ids, cr_version, embedding)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
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
            embedding as _
        )
        .fetch_one(&self.pool)
        .await
        .map_err(upstream("insert call"))?;
        tracing::info!(call = %id, thread = %q.thread_id, "call persisted");
        Ok(CallId::new(id))
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
