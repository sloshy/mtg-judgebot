//! What an operator asks of a running instance: how much is it used, what
//! does it cost, and which answers did people think were wrong. Read-only,
//! over `calls`, `ratings` and the `spend_days` ledger (`crate::budget`).
//!
//! Counts are of *stored* calls, so a Discord question asked with
//! `private: True` is in the spend and not in the questions.

use judge_core::JudgeError;
use schemars::JsonSchema;
use serde::Serialize;
use sqlx::PgPool;

use super::upstream;

/// Most days [`usage`] looks back.
pub const MAX_DAYS: u32 = 366;
/// Calls listed under [`Usage::lowest_rated`].
pub const LOWEST_RATED: i64 = 10;
/// Characters of a question shown in [`RatedCall`].
const QUESTION_CHARS: usize = 200;

/// One UTC day.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Day {
    /// `YYYY-MM-DD`.
    pub day: String,
    /// Calls stored that day, by front door.
    pub discord: i64,
    /// Asked through the web page or `POST /api/judge`.
    pub web: i64,
    /// Persisted by an agent session or `judge-cli judge`.
    pub agent: i64,
    /// Estimated model spend of `bot` and `api` that day, in USD.
    pub usd: f64,
    /// Model calls behind that spend.
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
    /// Ratings recorded in the window: incorrect, partially correct, correct.
    pub ratings: [i64; 3],
    /// Calls in the whole table the retirement pass has retired.
    pub retired_calls: i64,
    /// The rated calls with the lowest effective score, worst first.
    pub lowest_rated: Vec<RatedCall>,
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
                   count(*) FILTER (WHERE thread_id LIKE 'web:%') AS web,
                   count(*) FILTER (WHERE thread_id LIKE 'agent:%') AS agent,
                   count(*) FILTER (WHERE thread_id NOT LIKE 'web:%'
                                      AND thread_id NOT LIKE 'agent:%') AS discord
            FROM calls, bounds
            WHERE (created_at AT TIME ZONE 'utc')::date >= since
            GROUP BY 1
        ), spent AS (
            SELECT day, micro_usd, calls FROM spend_days, bounds WHERE day >= since
        )
        SELECT COALESCE(a.day, s.day)::text AS "day!",
               COALESCE(a.discord, 0) AS "discord!",
               COALESCE(a.web, 0) AS "web!",
               COALESCE(a.agent, 0) AS "agent!",
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
        agent: r.agent,
        usd: usd(r.micro_usd),
        llm_calls: r.llm_calls,
    })
    .collect::<Vec<_>>();

    let ratings = sqlx::query!(
        r#"
        SELECT count(*) FILTER (WHERE score = 1) AS "incorrect!",
               count(*) FILTER (WHERE score = 2) AS "partial!",
               count(*) FILTER (WHERE score = 3) AS "correct!"
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

    Ok(Usage {
        days,
        questions: by_day.iter().map(|d| d.discord + d.web + d.agent).sum(),
        usd: by_day.iter().map(|d| d.usd).sum(),
        by_day,
        ratings: [ratings.incorrect, ratings.partial, ratings.correct],
        retired_calls,
        lowest_rated,
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
