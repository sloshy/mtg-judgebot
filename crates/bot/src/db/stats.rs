//! What an operator asks of a running instance: how much is it used, what
//! does it cost, which answers did people think were wrong (and how much of
//! the feedback queue is still open), and how did the last data refreshes go. Read-only, over `calls`, `ratings`, the
//! `spend_days` ledger (`crate::budget`), the run record (`judge_runs`,
//! `judge_run_charges`) and `refresh_runs` (`crate::ingest::runs`).
//!
//! Counts are of *stored* calls: answered ones, private ones included
//! (`private` says how many). A question that was not answered (a "did you
//! mean?", an unknown card, a failure) is in the spend and not in the
//! questions.

use judge_core::JudgeError;
use schemars::JsonSchema;
use serde::Serialize;
use sqlx::PgPool;

use super::upstream;
use crate::ingest::runs::{self, RECENT_RUNS, RecentRun};

/// Most days [`usage`] looks back.
pub const MAX_DAYS: u32 = 366;
/// Calls listed under [`Usage::lowest_rated`].
pub const LOWEST_RATED: i64 = 10;
/// Characters of a question shown in [`RatedCall`].
const QUESTION_CHARS: usize = 200;

/// Most failed calls [`failures`] returns.
pub const MAX_FAILURES: u32 = 100;

/// A call that failed, as `failed_calls` kept it (already cut to size).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct FailedRow {
    /// Its row id (`judge_runs.failed_call_id` names it).
    pub id: i64,
    /// When it failed, RFC 3339 UTC.
    pub at: String,
    /// The thread it was asked in.
    pub thread: String,
    /// The interface: `discord`, `web`, `mcp`, `cli`, `agent_session`, or
    /// `agent` for a row from before 2.0.0 (MCP or `judge-cli`).
    pub interface: String,
    /// The Discord username of the asker, when known and not forgotten.
    pub asker: Option<String>,
    /// Asked private.
    pub private: bool,
    /// The question.
    pub question: String,
    /// The error.
    pub error: String,
    /// Why the first synthesis attempt was rejected, if it was.
    pub first_rejection: Option<String>,
    /// What the model sent on each synthesis attempt that returned an answer, in order.
    pub attempts: Vec<String>,
}

/// The newest `limit` (at most [`MAX_FAILURES`]) failed calls, newest first.
///
/// # Errors
/// `Upstream` from sqlx.
pub async fn failures(pool: &PgPool, limit: u32) -> Result<Vec<FailedRow>, JudgeError> {
    let rows = sqlx::query!(
        r#"
        SELECT id, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "at!",
               thread_id, interface, asker_name, private, question, error, first_rejection, attempts
        FROM failed_calls ORDER BY id DESC LIMIT $1
        "#,
        i64::from(limit.min(MAX_FAILURES)),
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("read failed calls"))?;
    Ok(rows
        .into_iter()
        .map(|r| FailedRow {
            id: r.id,
            at: r.at,
            thread: r.thread_id,
            interface: r.interface,
            asker: r.asker_name,
            private: r.private,
            question: r.question,
            error: r.error,
            first_rejection: r.first_rejection,
            attempts: r.attempts,
        })
        .collect())
}

/// One UTC day.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Day {
    /// `YYYY-MM-DD`.
    pub day: String,
    /// Calls stored that day, by interface: `/judge` on Discord.
    pub discord: i64,
    /// Asked through the web app or `POST /api/judge`.
    pub web: i64,
    /// The MCP `judge` tool.
    pub mcp: i64,
    /// `judge-cli judge`.
    pub cli: i64,
    /// Persisted by an agent session.
    pub agent_session: i64,
    /// Stored before 2.0.0 by the MCP `judge` tool or `judge-cli judge`,
    /// which were not told apart then.
    pub agent: i64,
    /// Of all the above, the calls asked private.
    pub private: i64,
    /// Estimated model spend of the `judgebot` processes and `judgebot ingest`
    /// that day, in USD: chat calls and embeddings together.
    pub usd: f64,
    /// Chat model calls behind that spend (embedding requests are not counted).
    pub llm_calls: i64,
}

/// A call people rated badly.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct RatedCall {
    /// Call id.
    pub call: uuid::Uuid,
    /// The question, cut to a line.
    pub question: String,
    /// The score retrieval uses: the judge override, else the smoothed mean.
    pub effective_score: f32,
    /// Ratings behind it.
    pub ratings: i32,
    /// Whether the retirement pass has already taken it out of retrieval.
    pub retired: bool,
}

/// What one model was billed for by the runs in the window.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct ModelUse {
    /// The provider table the requests went to.
    pub provider: String,
    /// The model each response named.
    pub model: String,
    /// Billed requests.
    pub requests: i64,
    /// Tokens in, over the requests that reported usage.
    pub input_tokens: i64,
    /// Tokens out.
    pub output_tokens: i64,
    /// Cached tokens read.
    pub cache_read_tokens: i64,
    /// Cached tokens written.
    pub cache_write_tokens: i64,
    /// Their cost in USD.
    pub usd: f64,
    /// Requests that reported no usage, charged at the worst case.
    pub estimated: i64,
}

/// How many runs in the window ended one way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct OutcomeCount {
    /// `answered`, `ambiguous`, …: `judge_core::Outcome`'s ids.
    pub outcome: String,
    /// Runs.
    pub runs: i64,
}

/// `judge-cli stats`.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Usage {
    /// Days covered, ending today (UTC).
    pub days: u32,
    /// Each day with a stored call or any spend, newest first.
    pub by_day: Vec<Day>,
    /// Stored calls in the window.
    pub questions: i64,
    /// Estimated spend in the window, in USD.
    pub usd: f64,
    /// Runs of the built-in pipeline that finished in the window, whatever
    /// their outcome (`judge_runs`).
    pub runs: i64,
    /// What those runs cost, in USD: their charges summed. Less than `usd`,
    /// which also counts what no run made (the refresh's embeddings, a
    /// search's).
    pub run_usd: f64,
    /// Those runs' charges by model, dearest first.
    pub by_model: Vec<ModelUse>,
    /// Those runs by outcome, commonest first.
    pub outcomes: Vec<OutcomeCount>,
    /// Discord ratings recorded in the window: incorrect, partially
    /// correct, correct. The ones that count toward the examples.
    pub ratings: [i64; 3],
    /// Web ratings recorded in the window, likewise: anonymous, so one
    /// person can add many; counted apart so they cannot skew the above.
    pub web_ratings: [i64; 3],
    /// Calls in the whole table the retirement pass has retired.
    pub retired_calls: i64,
    /// The rated calls with the lowest effective score, worst first.
    pub lowest_rated: Vec<RatedCall>,
    /// The feedback queue, whatever the window: items per category by
    /// status, every category listed ([`crate::feedback::counts`]).
    pub feedback: Vec<crate::feedback::QueueCount>,
    /// The latest data refresh runs ([`RECENT_RUNS`]), newest first,
    /// whatever the window.
    pub refresh_runs: Vec<RecentRun>,
    /// Why `refresh_runs` is empty when it could not be read: the schema
    /// predates the table (migrations pending). `None` when it was read.
    pub refresh_runs_note: Option<String>,
}

/// Usage over the last `days` UTC days (clamped to `1..=`[`MAX_DAYS`]).
///
/// # Errors
/// `Upstream` from sqlx.
pub async fn usage(pool: &PgPool, days: u32) -> Result<Usage, JudgeError> {
    let days = days.clamp(1, MAX_DAYS);
    let back = i32::try_from(days.saturating_sub(1)).unwrap_or(i32::MAX);
    let by_day = sqlx::query!(
        r#"
        WITH bounds AS (
            SELECT (now() AT TIME ZONE 'utc')::date - $1::int AS since
        ), asked AS (
            SELECT (created_at AT TIME ZONE 'utc')::date AS day,
                   count(*) FILTER (WHERE interface = 'discord') AS discord,
                   count(*) FILTER (WHERE interface = 'web') AS web,
                   count(*) FILTER (WHERE interface = 'mcp') AS mcp,
                   count(*) FILTER (WHERE interface = 'cli') AS cli,
                   count(*) FILTER (WHERE interface = 'agent_session') AS agent_session,
                   count(*) FILTER (WHERE interface = 'agent') AS agent,
                   count(*) FILTER (WHERE private) AS private
            FROM calls, bounds
            WHERE (created_at AT TIME ZONE 'utc')::date >= since
            GROUP BY 1
        ), spent AS (
            SELECT day, micro_usd, calls FROM spend_days, bounds WHERE day >= since
        )
        SELECT COALESCE(a.day, s.day)::text AS "day!",
               COALESCE(a.discord, 0) AS "discord!",
               COALESCE(a.web, 0) AS "web!",
               COALESCE(a.mcp, 0) AS "mcp!",
               COALESCE(a.cli, 0) AS "cli!",
               COALESCE(a.agent_session, 0) AS "agent_session!",
               COALESCE(a.agent, 0) AS "agent!",
               COALESCE(a.private, 0) AS "private!",
               COALESCE(s.micro_usd, 0) AS "micro_usd!",
               COALESCE(s.calls, 0) AS "llm_calls!"
        FROM asked a FULL JOIN spent s ON s.day = a.day
        ORDER BY 1 DESC
        "#,
        back
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("usage by day"))?
    .into_iter()
    .map(|r| Day {
        day: r.day,
        discord: r.discord,
        web: r.web,
        mcp: r.mcp,
        cli: r.cli,
        agent_session: r.agent_session,
        agent: r.agent,
        private: r.private,
        usd: usd(r.micro_usd),
        llm_calls: r.llm_calls,
    })
    .collect::<Vec<_>>();

    let ratings = sqlx::query!(
        r#"
        SELECT count(*) FILTER (WHERE score = 1 AND rater_kind = 'discord') AS "incorrect!",
               count(*) FILTER (WHERE score = 2 AND rater_kind = 'discord') AS "partial!",
               count(*) FILTER (WHERE score = 3 AND rater_kind = 'discord') AS "correct!",
               count(*) FILTER (WHERE score = 1 AND rater_kind = 'web') AS "web_incorrect!",
               count(*) FILTER (WHERE score = 2 AND rater_kind = 'web') AS "web_partial!",
               count(*) FILTER (WHERE score = 3 AND rater_kind = 'web') AS "web_correct!"
        FROM ratings
        WHERE (ts AT TIME ZONE 'utc')::date >= (now() AT TIME ZONE 'utc')::date - $1::int
        "#,
        back
    )
    .fetch_one(pool)
    .await
    .map_err(upstream("usage ratings"))?;

    let retired_calls =
        sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM calls WHERE retired_at IS NOT NULL"#)
            .fetch_one(pool)
            .await
            .map_err(upstream("usage retired"))?;

    let lowest_rated = sqlx::query!(
        r#"
        SELECT c.id, c.question, r.effective_score AS "effective_score!", r.n AS "n!",
               c.retired_at IS NOT NULL AS "retired!"
        FROM calls c JOIN calls_rated r ON r.call_id = c.id
        WHERE r.n > 0
        ORDER BY r.effective_score ASC, r.n DESC, c.created_at DESC
        LIMIT $1
        "#,
        LOWEST_RATED
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("usage lowest rated"))?
    .into_iter()
    .map(|r| RatedCall {
        call: r.id,
        question: one_line(&r.question),
        effective_score: r.effective_score,
        ratings: r.n,
        retired: r.retired,
    })
    .collect();

    let by_model: Vec<ModelUse> = sqlx::query!(
        r#"
        SELECT ch.provider, ch.model,
               count(*) AS "requests!",
               coalesce(sum(ch.input_tokens), 0)::bigint AS "input_tokens!",
               coalesce(sum(ch.output_tokens), 0)::bigint AS "output_tokens!",
               coalesce(sum(ch.cache_read_tokens), 0)::bigint AS "cache_read_tokens!",
               coalesce(sum(ch.cache_write_tokens), 0)::bigint AS "cache_write_tokens!",
               coalesce(sum(ch.micro_usd), 0)::bigint AS "micro_usd!",
               count(*) FILTER (WHERE ch.cost_estimated) AS "estimated!"
        FROM judge_run_charges ch JOIN judge_runs r ON r.id = ch.run_id
        WHERE (r.finished_at AT TIME ZONE 'utc')::date >= (now() AT TIME ZONE 'utc')::date - $1::int
        GROUP BY ch.provider, ch.model
        ORDER BY 8 DESC, 1, 2
        "#,
        back
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("usage by model"))?
    .into_iter()
    .map(|r| ModelUse {
        provider: r.provider,
        model: r.model,
        requests: r.requests,
        input_tokens: r.input_tokens,
        output_tokens: r.output_tokens,
        cache_read_tokens: r.cache_read_tokens,
        cache_write_tokens: r.cache_write_tokens,
        usd: usd(r.micro_usd),
        estimated: r.estimated,
    })
    .collect();

    let outcomes: Vec<OutcomeCount> = sqlx::query!(
        r#"
        SELECT outcome, count(*) AS "runs!"
        FROM judge_runs
        WHERE (finished_at AT TIME ZONE 'utc')::date >= (now() AT TIME ZONE 'utc')::date - $1::int
        GROUP BY outcome
        ORDER BY 2 DESC, 1
        "#,
        back
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("usage by outcome"))?
    .into_iter()
    .map(|r| OutcomeCount {
        outcome: r.outcome,
        runs: r.runs,
    })
    .collect();

    // A schema behind this binary (`JUDGE_AUTO_MIGRATE=false` with
    // migrations pending) has no run table; the rest of the view stands.
    let (refresh_runs, refresh_runs_note) = match runs::recent(pool, RECENT_RUNS).await {
        Ok(r) => (r, None),
        Err(e) if runs::is_missing_table(&e) => (
            Vec::new(),
            Some(
                "refresh_runs does not exist: migrations are pending (`judgebot ingest migrate`)"
                    .to_owned(),
            ),
        ),
        Err(e) => return Err(JudgeError::Upstream(e)),
    };

    Ok(Usage {
        days,
        questions: by_day
            .iter()
            .map(|d| d.discord + d.web + d.mcp + d.cli + d.agent_session + d.agent)
            .sum(),
        usd: by_day.iter().map(|d| d.usd).sum(),
        runs: outcomes.iter().map(|o| o.runs).sum(),
        run_usd: by_model.iter().map(|m| m.usd).sum(),
        by_model,
        outcomes,
        by_day,
        ratings: [ratings.incorrect, ratings.partial, ratings.correct],
        web_ratings: [
            ratings.web_incorrect,
            ratings.web_partial,
            ratings.web_correct,
        ],
        retired_calls,
        lowest_rated,
        feedback: crate::feedback::counts(pool).await?,
        refresh_runs,
        refresh_runs_note,
    })
}

fn usd(micro: i64) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "micro-dollar amounts stay far below 2^53"
    )]
    let usd = micro as f64 / 1_000_000.0;
    usd
}

fn one_line(q: &str) -> String {
    let flat = q.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= QUESTION_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(QUESTION_CHARS - 1).collect();
    cut.push('…');
    cut
}
