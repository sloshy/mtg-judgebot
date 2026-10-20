//! [`PgCallStore`]: pipeline step 6 — persist validated verdicts and ratings.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use judge_core::{
    CallId, CallStore, Context, Earlier, FailedCall, FailedCallId, FeedbackCategory, Forgotten,
    HistoryScope, InputKind, JudgeError, Left, Origin, Qa, Question, Rated, Rater, RaterId, Rating,
    RunRecord, Score, Tally, Validated, Verdict, oracle_fingerprint,
};
use pgvector::Vector;
use sqlx::PgPool;

use super::{CALLS_REWRITE_LOCK, Vectors, bad_row, bad_row_from, upstream};
use crate::{
    feedback::{self, IpDays},
    session::{PersistCall, SessionId},
};

/// Calls + ratings. Only `Verdict<Validated>` can be persisted (invariant I3).
#[derive(Clone)]
pub struct PgCallStore {
    pool: PgPool,
    vectors: Option<Arc<Vectors>>,
    /// How long a web rating keeps its address (`FEEDBACK_IP_DAYS`): every
    /// rating written sweeps the older ones.
    ip_days: IpDays,
}

impl fmt::Debug for PgCallStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgCallStore")
            .field("vectors", &self.vectors.as_ref().map(|v| v.space()))
            .finish_non_exhaustive()
    }
}

impl PgCallStore {
    /// A store that leaves `calls.embedding` NULL (no embedder), sweeping
    /// web raters' addresses after the environment's `FEEDBACK_IP_DAYS`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            vectors: None,
            ip_days: IpDays::from_env(),
        }
    }

    /// The same store, sweeping web raters' addresses after `days`.
    #[must_use]
    pub const fn with_ip_days(mut self, days: IpDays) -> Self {
        self.ip_days = days;
        self
    }

    /// Embed each persisted public question so later calls can be found by
    /// similarity (a private call is never a prior-call example, so it is
    /// not embedded), subject to the space check in [`Vectors`]: a call is
    /// stored without a vector rather than with one of another space
    /// (`ingest embed` fills it in later).
    #[must_use]
    pub fn with_vectors(mut self, vectors: Arc<Vectors>) -> Self {
        self.vectors = Some(vectors);
        self
    }

    async fn embed(&self, text: &str, tally: &Tally) -> Option<Vector> {
        self.vectors
            .as_ref()?
            .embed(text, InputKind::Document, tally)
            .await
    }
}

/// Characters kept of a failed call's question, of each attempt's answer and
/// of the error text, and how long and how many rows `failed_calls` holds.
const FAILED_QUESTION_CHARS: usize = 2_000;
const FAILED_ATTEMPT_CHARS: usize = 8_000;
const FAILED_ERROR_CHARS: usize = 4_000;
const FAILED_KEEP_DAYS: i32 = 30;
const FAILED_KEEP_ROWS: i64 = 500;

/// USD as whole micro-dollars (the spend ledger's unit), rounded, never negative.
fn micro_usd(usd: f64) -> i64 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "rounded and clamped first; a run's cost is far below i64::MAX micro-dollars"
    )]
    let micro = (usd * 1_000_000.0).round().clamp(0.0, 9.0e15) as i64;
    micro
}

/// `s` cut to `max` characters, marked when cut.
fn cut(s: &str, max: usize) -> String {
    // Postgres `text` cannot hold a NUL, and a model's text may.
    let mut out: String = s.chars().filter(|&c| c != '\0').take(max).collect();
    if s.chars().filter(|&c| c != '\0').count() > max {
        out.push('…');
    }
    out
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
    /// agent session's persist step idempotent in the database. The
    /// question's embedding is billed to `tally`.
    async fn insert(
        &self,
        q: &Question,
        origin: &Origin,
        v: &Verdict<Validated>,
        ctx: &Context,
        session: Option<SessionId>,
        tally: &Tally,
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
        let mut embedding = if origin.is_private() {
            None
        } else {
            self.embed(&q.text, tally).await
        };
        let asker = origin.asker();
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
                               citations, context_ids, cr_version, embedding, session_id,
                               interface, asker_id, asker_name, private)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
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
            session.map(|s| s.0),
            origin.interface().id(),
            asker.map(judge_core::Asker::id_string),
            asker
                .and_then(|a| a.name.as_ref())
                .map(AsRef::<str>::as_ref),
            origin.is_private(),
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(upstream("insert call"))?;
        tx.commit().await.map_err(upstream("commit persist"))?;
        tracing::info!(
            call = %id,
            thread = %q.thread_id,
            interface = %origin.interface(),
            private = origin.is_private(),
            "call persisted"
        );
        Ok(CallId::new(id))
    }
}

#[async_trait]
impl PersistCall for PgCallStore {
    async fn persist_call(
        &self,
        session: SessionId,
        q: &Question,
        origin: &Origin,
        v: &Verdict<Validated>,
        ctx: &Context,
    ) -> Result<CallId, JudgeError> {
        // A session's run is not recorded (its model is the agent), so the
        // embedding's charge goes nowhere.
        self.insert(q, origin, v, ctx, Some(session), &Tally::new())
            .await
    }
}

#[async_trait]
impl CallStore for PgCallStore {
    async fn persist(
        &self,
        q: &Question,
        origin: &Origin,
        v: &Verdict<Validated>,
        ctx: &Context,
        tally: &Tally,
    ) -> Result<CallId, JudgeError> {
        self.insert(q, origin, v, ctx, None, tally).await
    }

    async fn rate(
        &self,
        call: CallId,
        rater: &Rater,
        rating: &Rating,
    ) -> Result<Rated, JudgeError> {
        let id = rater.id();
        let user_id = id.stored();
        let (name, ip, user_agent) = match rater {
            Rater::Discord { .. } => (None, None, None),
            Rater::Web {
                name,
                ip,
                user_agent,
                ..
            } => (
                name.as_ref().map(|n| AsRef::<str>::as_ref(n).to_owned()),
                ip.map(|i| i.to_string()),
                user_agent.as_ref().map(|u| u.as_str().to_owned()),
            ),
        };
        let reason = rating.reason();
        let text =
            |t: Option<&judge_core::FeedbackText>| t.map(|t| AsRef::<str>::as_ref(t).to_owned());
        let mut tx = self.pool.begin().await.map_err(upstream("begin rating"))?;
        // No row when the call is gone (the retention deleted it); a call
        // deleted between the check and the insert fails the foreign key.
        // Every field is written, so a replaced rating keeps nothing of the
        // reason, name or address it replaces.
        let written = sqlx::query!(
            r#"
            INSERT INTO ratings (call_id, user_id, score, is_judge, rater_kind, category,
                                 what_wrong, right_answer, web_name, ip, user_agent)
            SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10::text::inet, $11
            WHERE EXISTS (SELECT 1 FROM calls WHERE id = $1)
            ON CONFLICT (call_id, user_id)
            DO UPDATE SET score = EXCLUDED.score, is_judge = EXCLUDED.is_judge,
                          rater_kind = EXCLUDED.rater_kind, category = EXCLUDED.category,
                          what_wrong = EXCLUDED.what_wrong, right_answer = EXCLUDED.right_answer,
                          web_name = EXCLUDED.web_name, ip = EXCLUDED.ip,
                          user_agent = EXCLUDED.user_agent, ts = now()
            "#,
            call.into_inner(),
            user_id,
            i16::from(rating.score() as u8),
            rater.is_judge(),
            id.kind().id(),
            reason.map(|r| r.category.id()),
            text(reason.and_then(|r| r.wrong.as_ref())),
            text(reason.and_then(|r| r.right.as_ref())),
            name,
            ip,
            user_agent,
        )
        .execute(&mut *tx)
        .await;
        match written {
            Ok(done) if done.rows_affected() == 0 => return Ok(Rated::CallGone),
            Ok(_) => {}
            Err(sqlx::Error::Database(e)) if e.is_foreign_key_violation() => {
                return Ok(Rated::CallGone);
            }
            Err(e) => return Err(upstream("upsert rating")(e)),
        }
        // Lock order: the rating row (above), then its queue item (below),
        // as `forget_user` takes them; the sweep skips rows it cannot lock.
        if reason.is_some() {
            // A new reason is new work: open the item, or reopen it with the
            // operator's earlier note kept for reference.
            sqlx::query!(
                r#"
                INSERT INTO feedback_queue (call_id, user_id) VALUES ($1, $2)
                ON CONFLICT (call_id, user_id)
                DO UPDATE SET status = 'open', resolved_at = NULL, opened_at = now()
                "#,
                call.into_inner(),
                user_id,
            )
            .execute(&mut *tx)
            .await
            .map_err(upstream("open a queue item"))?;
        } else {
            // Re-rated Correct: nothing left to work on, but the operator's
            // own writing on an item is never deleted by a rater.
            sqlx::query!(
                r#"
                DELETE FROM feedback_queue
                WHERE call_id = $1 AND user_id = $2 AND note IS NULL AND fixed_by IS NULL
                "#,
                call.into_inner(),
                user_id,
            )
            .execute(&mut *tx)
            .await
            .map_err(upstream("drop a queue item"))?;
        }
        feedback::sweep(&mut *tx, self.ip_days)
            .await
            .map_err(upstream("sweep raters' addresses"))?;
        tx.commit().await.map_err(upstream("commit rating"))?;
        Ok(Rated::Recorded)
    }

    async fn earlier_rating(
        &self,
        call: CallId,
        rater: RaterId,
    ) -> Result<Option<Earlier>, JudgeError> {
        let Some(row) = sqlx::query!(
            "SELECT score, category FROM ratings WHERE call_id = $1 AND user_id = $2",
            call.into_inner(),
            rater.stored(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(upstream("read a rating"))?
        else {
            return Ok(None);
        };
        let score = match row.score {
            1 => Score::Incorrect,
            2 => Score::Partial,
            3 => Score::Correct,
            n => return Err(bad_row(format!("ratings.score {n}"))),
        };
        let category = row
            .category
            .as_deref()
            .map(|c| {
                FeedbackCategory::parse(c).ok_or_else(|| bad_row(format!("ratings.category {c:?}")))
            })
            .transpose()?;
        Ok(Some(Earlier { score, category }))
    }

    async fn record_failure(&self, failed: &FailedCall) -> Result<FailedCallId, JudgeError> {
        let attempts: Vec<String> = failed
            .attempts
            .iter()
            .map(|a| cut(a, FAILED_ATTEMPT_CHARS))
            .collect();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(upstream("begin failure record"))?;
        let id = sqlx::query_scalar!(
            r#"
            INSERT INTO failed_calls (thread_id, private, asker_id, asker_name, interface,
                                      question, error, first_rejection, attempts)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            RETURNING id
            "#,
            failed.thread_id,
            failed.origin.is_private(),
            failed.origin.asker().map(judge_core::Asker::id_string),
            failed
                .origin
                .asker()
                .and_then(|a| a.name.as_ref())
                .map(AsRef::<str>::as_ref),
            failed.origin.interface().id(),
            cut(&failed.question, FAILED_QUESTION_CHARS),
            cut(&failed.error, FAILED_ERROR_CHARS),
            failed
                .first_rejection
                .as_deref()
                .map(|r| cut(r, FAILED_ERROR_CHARS)),
            &attempts,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(upstream("insert failed call"))?;
        sqlx::query!(
            r#"
            DELETE FROM failed_calls
            WHERE created_at < now() - make_interval(days => $1::int)
               OR id NOT IN (SELECT id FROM failed_calls ORDER BY id DESC LIMIT $2)
            "#,
            FAILED_KEEP_DAYS,
            FAILED_KEEP_ROWS,
        )
        .execute(&mut *tx)
        .await
        .map_err(upstream("prune failed calls"))?;
        tx.commit()
            .await
            .map_err(upstream("commit failure record"))?;
        Ok(FailedCallId(id))
    }

    async fn record_run(&self, run: &RunRecord) -> Result<(), JudgeError> {
        let (call, failed) = match run.left {
            Left::Call(id) => (Some(id.into_inner()), None),
            Left::Failure(id) => (None, Some(id.0)),
            Left::Nothing => (None, None),
        };
        let n = run.charges.len();
        let (mut seq, mut stage, mut provider, mut model) = (
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
        );
        let (mut input, mut output, mut cache_read, mut cache_write, mut micro, mut estimated) = (
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
        );
        for (i, c) in run.charges.iter().enumerate() {
            seq.push(i16::try_from(i).map_err(|e| bad_row_from(e, "charge count"))?);
            stage.push(c.stage.id().to_owned());
            provider.push(c.provider.clone());
            model.push(c.model.clone());
            let tokens = |f: fn(&judge_core::Tokens) -> u64| {
                c.tokens
                    .as_ref()
                    .map(|t| i64::try_from(f(t)).unwrap_or(i64::MAX))
            };
            input.push(tokens(|t| t.input));
            output.push(tokens(|t| t.output));
            cache_read.push(tokens(|t| t.cache_read));
            cache_write.push(tokens(|t| t.cache_write));
            micro.push(micro_usd(c.cost.usd()));
            estimated.push(c.cost.is_estimate());
        }
        // A data-modifying CTE always runs, so a run with no charges is still recorded.
        sqlx::query!(
            r#"
            WITH run AS (
                INSERT INTO judge_runs (started_at, interface, private, outcome, retried,
                                        tool_round, call_id, failed_call_id)
                VALUES (now() - make_interval(secs => $1), $2, $3, $4, $5, $6, $7, $8)
                RETURNING id
            )
            INSERT INTO judge_run_charges (run_id, seq, stage, provider, model, input_tokens,
                                           output_tokens, cache_read_tokens,
                                           cache_write_tokens, micro_usd, cost_estimated)
            SELECT run.id, c.seq, c.stage, c.provider, c.model, c.input_tokens,
                   c.output_tokens, c.cache_read_tokens, c.cache_write_tokens, c.micro_usd,
                   c.cost_estimated
            FROM run,
                 UNNEST($9::smallint[], $10::text[], $11::text[], $12::text[], $13::bigint[],
                        $14::bigint[], $15::bigint[], $16::bigint[], $17::bigint[],
                        $18::boolean[])
                     AS c(seq, stage, provider, model, input_tokens, output_tokens,
                          cache_read_tokens, cache_write_tokens, micro_usd, cost_estimated)
            "#,
            run.elapsed.as_secs_f64(),
            run.interface.id(),
            run.private,
            run.outcome.id(),
            run.retried,
            run.tool_round(),
            call,
            failed,
            &seq,
            &stage,
            &provider,
            &model,
            &input as &[Option<i64>],
            &output as &[Option<i64>],
            &cache_read as &[Option<i64>],
            &cache_write as &[Option<i64>],
            &micro,
            &estimated,
        )
        .execute(&self.pool)
        .await
        .map_err(upstream("insert judge run"))?;
        Ok(())
    }

    async fn forget_user(&self, user_id: &str) -> Result<Forgotten, JudgeError> {
        let mut tx = self.pool.begin().await.map_err(upstream("begin forget"))?;
        // The CR loader rewrites every call's answer from a read it made
        // earlier, under the exclusive side of this lock; holding the shared
        // side means it cannot write a private answer back after we remove it.
        sqlx::query!(
            "SELECT pg_advisory_xact_lock_shared($1)",
            CALLS_REWRITE_LOCK
        )
        .execute(&mut *tx)
        .await
        .map_err(upstream("lock calls for forget"))?;
        // Lock order: a user's `ratings` rows, then their `feedback_queue`
        // items, the order `rate` takes them in (it upserts the rating, then
        // the item). Taking the items first deadlocked against a re-rate.
        // So the ratings are locked first, then the items are deleted (to
        // count them: deleting the ratings would take them along uncounted,
        // the foreign key cascades), then the ratings.
        sqlx::query!(
            "SELECT 1 AS locked FROM ratings WHERE user_id = $1 FOR UPDATE",
            user_id
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(upstream("lock ratings for forget"))?;
        let feedback = sqlx::query!("DELETE FROM feedback_queue WHERE user_id = $1", user_id)
            .execute(&mut *tx)
            .await
            .map_err(upstream("delete feedback items"))?
            .rows_affected();
        let ratings = sqlx::query!("DELETE FROM ratings WHERE user_id = $1", user_id)
            .execute(&mut *tx)
            .await
            .map_err(upstream("delete ratings"))?
            .rows_affected();
        // A public call keeps its text, which the channel saw: only who asked
        // goes. A private one was the asker's alone, so its text goes too, and
        // with it what it cited and which cards it was about.
        let calls = sqlx::query!(
            r#"
            UPDATE calls
            SET asker_id = NULL, asker_name = NULL,
                question = CASE WHEN private THEN '[removed]' ELSE question END,
                answer = CASE WHEN private THEN '[removed]' ELSE answer END,
                embedding = CASE WHEN private THEN NULL ELSE embedding END,
                citations = CASE WHEN private THEN '[]'::jsonb ELSE citations END,
                context_ids = CASE WHEN private THEN '{}'::jsonb ELSE context_ids END
            WHERE asker_id = $1
            "#,
            user_id
        )
        .execute(&mut *tx)
        .await
        .map_err(upstream("anonymize calls"))?
        .rows_affected();
        let failures = sqlx::query!(
            r#"
            UPDATE failed_calls
            SET asker_id = NULL, asker_name = NULL, question = '[removed]', error = '[removed]',
                first_rejection = NULL, attempts = '{}'
            WHERE asker_id = $1
            "#,
            user_id
        )
        .execute(&mut *tx)
        .await
        .map_err(upstream("anonymize failed calls"))?
        .rows_affected();
        tx.commit().await.map_err(upstream("commit forget"))?;
        Ok(Forgotten {
            ratings,
            feedback,
            failures,
            calls,
        })
    }

    async fn history(
        &self,
        thread_id: &str,
        n: usize,
        scope: HistoryScope,
    ) -> Result<Vec<Qa>, JudgeError> {
        if n == 0 {
            return Ok(vec![]);
        }
        let limit = i64::try_from(n).unwrap_or(i64::MAX);
        // Newest `n` first, then reversed: the caller wants them oldest first.
        let rows = sqlx::query!(
            r#"
            SELECT question, answer
            FROM calls
            WHERE thread_id = $1 AND (NOT private OR $3)
            ORDER BY created_at DESC
            LIMIT $2
            "#,
            thread_id,
            limit,
            matches!(scope, HistoryScope::WithPrivate),
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
