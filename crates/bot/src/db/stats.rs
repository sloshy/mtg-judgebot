//! The dashboard (#28): what the instance has been asked and how it went, by
//! interface, by user, by model and over time; and for the operator, what it
//! cost and what the public never sees. One module of queries, read by every
//! surface that shows them, so they cannot disagree:
//!
//! * [`public`]: `GET /api/stats` and the web app's `/dashboard`;
//! * [`usage`]: the admin page's usage view (`/admin/api/usage`), the admin
//!   MCP tool `admin_usage` and `judge-cli stats`.
//!
//! Both build the same [`Dashboard`] from the same SQL. The scope decides
//! what it counts, and is set by the entry point, never by a caller:
//!
//! * The public one counts only calls and runs not asked private and within
//!   the feed window (`JUDGE_FEED_DAYS`): `publicly_visible`, the SQL
//!   function the feed's reads use, is in the `WHERE` of every public count.
//!   A private call or run is never counted, not in a total, an outcome, a
//!   rating, a category, a card or a user's figures.
//! * Its cost figures exist only through a [`CostAccess`], and its model
//!   figures and model filter only through a [`ModelAccess`]: the public
//!   scope holds one only when `JUDGE_PUBLIC_COST` / `JUDGE_PUBLIC_MODELS` is
//!   true ([`Disclosure`]), so a forgotten check cannot fill the field.
//! * The operator's counts everything, private calls and runs included, and
//!   adds the spend ledger, the budget, the run costs and latency, the failed
//!   calls, the feedback queue and the data refreshes ([`Usage`]).
//!
//! Runs record no asker, so under a user filter the run figures (outcomes,
//! run costs, latency) are `None` rather than a guess. Every report covers a
//! bounded range of UTC days ([`MAX_DAYS`]), so every query reads a bounded
//! slice of `calls` and `judge_runs` through their time indexes.

use std::{collections::BTreeMap, fmt, str::FromStr as _};

use judge_core::{Category, DiscordUsername, JudgeError, Outcome, OutcomeGroup};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{bad_row, bad_row_from, upstream};
use crate::admin::FilterText;
use crate::budget::Period;
use crate::feed::{Asked, window_days};
use crate::ingest::prune::Days;
use crate::ingest::runs::{self, RECENT_RUNS, RecentRun};

#[cfg(test)]
mod tests;

/// Most UTC days one report covers.
pub const MAX_DAYS: u32 = 366;
/// Days a report covers when it names no start.
pub const DEFAULT_DAYS: u32 = 30;
/// Entries in each "most" list: categories, cards, askers, expensive runs.
pub const TOP: i64 = 10;
/// Calls listed under [`Usage::lowest_rated`].
pub const LOWEST_RATED: i64 = 10;
/// Characters of a question shown in a list.
const QUESTION_CHARS: usize = 200;
/// Most failed calls [`failures`] returns.
pub const MAX_FAILURES: u32 = 100;

/// `JUDGE_PUBLIC_COST`: `true` shows cost figures on the public dashboard.
pub const PUBLIC_COST_ENV: &str = "JUDGE_PUBLIC_COST";
/// `JUDGE_PUBLIC_MODELS`: `true` shows which models answered on the public
/// dashboard, and lets it filter by model.
pub const PUBLIC_MODELS_ENV: &str = "JUDGE_PUBLIC_MODELS";

// --- What the public may see -------------------------------------------------

/// What the public dashboard shows beyond the counts: cost
/// (`JUDGE_PUBLIC_COST`) and models (`JUDGE_PUBLIC_MODELS`), both off by
/// default. The only way to a [`CostAccess`] or [`ModelAccess`] for the
/// public scope.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Disclosure {
    cost: bool,
    models: bool,
}

/// A `JUDGE_PUBLIC_COST` or `JUDGE_PUBLIC_MODELS` value that is not a
/// boolean: the variable and the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadDisclosure {
    /// The variable.
    pub var: &'static str,
    /// The value as set.
    pub value: String,
}

impl Disclosure {
    /// Parse `JUDGE_PUBLIC_COST` and `JUDGE_PUBLIC_MODELS` (blank: off).
    ///
    /// # Errors
    /// The first value that is not a boolean.
    pub fn parse(cost: Option<&str>, models: Option<&str>) -> Result<Self, BadDisclosure> {
        let flag = |var: &'static str, raw: Option<&str>| match raw
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("" | "false" | "0" | "no" | "off") => Ok(false),
            Some("true" | "1" | "yes" | "on") => Ok(true),
            Some(_) => Err(BadDisclosure {
                var,
                value: raw.unwrap_or_default().to_owned(),
            }),
        };
        Ok(Self {
            cost: flag(PUBLIC_COST_ENV, cost)?,
            models: flag(PUBLIC_MODELS_ENV, models)?,
        })
    }

    /// Cost figures, when `JUDGE_PUBLIC_COST` shows them.
    #[must_use]
    pub const fn cost(self) -> Option<CostAccess> {
        if self.cost {
            Some(CostAccess(()))
        } else {
            None
        }
    }

    /// Model figures and the model filter, when `JUDGE_PUBLIC_MODELS` shows
    /// them.
    #[must_use]
    pub const fn models(self) -> Option<ModelAccess> {
        if self.models {
            Some(ModelAccess(()))
        } else {
            None
        }
    }
}

/// Proof that this report may carry cost figures: from
/// [`Disclosure::cost`], or the operator's scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CostAccess(());

/// Proof that this report may carry model figures and filter by model: from
/// [`Disclosure::models`], or the operator's scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelAccess(());

/// A public model filter: made only with a [`ModelAccess`], so the public
/// dashboard can filter by model only when it shows models.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelFilter {
    name: FilterText,
}

impl ModelFilter {
    /// Filter by `name` (as a response named the model).
    #[must_use]
    pub const fn new(_access: ModelAccess, name: FilterText) -> Self {
        Self { name }
    }
}

// --- Days and ranges ---------------------------------------------------------

/// A UTC calendar day, `YYYY-MM-DD`, years 2000 to 9999.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(i32);

impl Date {
    /// The day `text` names, or `None` when it is not a real `YYYY-MM-DD`
    /// day from 2000 on.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let b = text.as_bytes();
        if b.len() != 10 || b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
            return None;
        }
        let num = |r: std::ops::Range<usize>| -> Option<u32> {
            let s = text.get(r)?;
            s.bytes()
                .all(|c| c.is_ascii_digit())
                .then(|| s.parse().ok())
                .flatten()
        };
        let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
        if !(2000..=9999).contains(&y) || !(1..=12).contains(&m) || d == 0 {
            return None;
        }
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let last = match m {
            2 if leap => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        (d <= last).then(|| Self(days_from_civil(y, m, d)))
    }

    /// `n` days later (earlier when negative).
    #[must_use]
    pub const fn plus(self, n: i32) -> Self {
        Self(self.0.saturating_add(n))
    }

    /// Days from `earlier` to `self`.
    #[must_use]
    pub const fn since(self, earlier: Self) -> i32 {
        self.0.saturating_sub(earlier.0)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = civil_from_days(self.0);
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`), for years from 2000.
const fn days_from_civil(y: u32, m: u32, d: u32) -> i32 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    #[expect(
        clippy::cast_possible_wrap,
        reason = "years up to 9999 are under 3 million days"
    )]
    let days = (era * 146_097 + doe) as i32 - 719_468;
    days
}

/// The inverse of [`days_from_civil`].
const fn civil_from_days(days: i32) -> (u32, u32, u32) {
    #[expect(
        clippy::cast_sign_loss,
        reason = "a Date is from 2000 on, so the shifted count is positive"
    )]
    let z = (days + 719_468) as u32;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The UTC days a report covers: `from` to `to`, both included, at most
/// [`MAX_DAYS`] of them. Made only by [`Span::resolve`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    from: Date,
    to: Date,
}

impl Range {
    /// The first day.
    #[must_use]
    pub const fn from(self) -> Date {
        self.from
    }

    /// The last day.
    #[must_use]
    pub const fn to(self) -> Date {
        self.to
    }

    /// How many days.
    #[must_use]
    pub fn days(self) -> u32 {
        u32::try_from(self.to.since(self.from)).map_or(1, |n| n.saturating_add(1))
    }
}

/// Which days a report asks for, before today is known: any of a first day,
/// a last day (default today) and a number of days ending on the last
/// (default [`DEFAULT_DAYS`], used only without a first day).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    /// The first day.
    pub from: Option<Date>,
    /// The last day; today when `None`.
    pub to: Option<Date>,
    /// Days ending on the last, when there is no first day.
    pub days: Option<u32>,
}

/// Why a [`Span`] names no report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BadRange {
    /// The first day is after the last.
    #[error("the first day is after the last")]
    Reversed,
    /// More than [`MAX_DAYS`] days.
    #[error("a report covers at most {MAX_DAYS} days")]
    TooLong,
    /// Zero days.
    #[error("a report covers at least one day")]
    NoDays,
    /// The range ends before the public window's first day: nothing in it
    /// is shown (`JUDGE_FEED_DAYS`).
    #[error("the range ends before the last {days} days, which are all this instance shows")]
    BeforeWindow {
        /// `JUDGE_FEED_DAYS`.
        days: u16,
    },
}

impl Span {
    /// The days a report covers, `today` being the database's.
    ///
    /// # Errors
    /// A first day after the last, a range longer than [`MAX_DAYS`], or
    /// `days` of zero or over [`MAX_DAYS`].
    pub fn resolve(self, today: Date) -> Result<Range, BadRange> {
        let to = self.to.unwrap_or(today);
        let from = match (self.from, self.days) {
            (Some(from), _) => from,
            (None, days) => {
                let days = days.unwrap_or(DEFAULT_DAYS);
                if days == 0 {
                    return Err(BadRange::NoDays);
                }
                if days > MAX_DAYS {
                    return Err(BadRange::TooLong);
                }
                to.plus(1 - i32::try_from(days).unwrap_or(1))
            }
        };
        if from > to {
            return Err(BadRange::Reversed);
        }
        let range = Range { from, to };
        if range.days() > MAX_DAYS {
            return Err(BadRange::TooLong);
        }
        Ok(range)
    }
}

// --- Queries -------------------------------------------------------------------

/// The public dashboard's question (`GET /api/stats`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PublicQuery {
    /// The days.
    pub span: Span,
    /// Only calls (and runs) through this interface.
    pub asked: Option<Asked>,
    /// Only calls by this Discord username.
    pub user: Option<DiscordUsername>,
    /// Only calls (and runs) that billed this model.
    pub model: Option<ModelFilter>,
}

/// The operator's question (`judge-cli stats`, `/admin/api/usage`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperatorQuery {
    /// The days.
    pub span: Span,
    /// Only calls (and runs) through this interface.
    pub asked: Option<Asked>,
    /// Only calls by this Discord username or user id.
    pub asker: Option<FilterText>,
    /// Only calls (and runs) that billed this model.
    pub model: Option<FilterText>,
}

/// Why a report could not be made.
#[derive(Debug, thiserror::Error)]
#[expect(
    clippy::large_enum_variant,
    reason = "JudgeError is large everywhere it is returned (see result_large_err)"
)]
pub enum StatsError {
    /// The days asked for name no report.
    #[error(transparent)]
    Range(#[from] BadRange),
    /// A query ran past the report's statement timeout
    /// ([`PUBLIC_TIMEOUT`], [`OPERATOR_TIMEOUT`]).
    #[error("the report took longer than its limit: try a shorter range or a filter")]
    TimedOut,
    /// The database.
    #[error(transparent)]
    Store(#[from] JudgeError),
}

/// How long one query of a public report may run.
pub const PUBLIC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// How long one query of the operator's report may run.
pub const OPERATOR_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(1);

/// A sqlx error as a report's: a statement cancelled by the timeout is
/// [`StatsError::TimedOut`], anything else the store's failure to `what`.
fn read(what: &'static str) -> impl FnOnce(sqlx::Error) -> StatsError {
    move |e| match &e {
        // 57014 query_canceled: what `statement_timeout` raises.
        sqlx::Error::Database(d) if d.code().as_deref() == Some("57014") => StatsError::TimedOut,
        _ => StatsError::Store(upstream(what)(e)),
    }
}

/// A read-only transaction whose statements are cancelled past `limit`:
/// every query of a report runs in one.
async fn bounded(
    pool: &PgPool,
    limit: std::time::Duration,
) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, StatsError> {
    let mut tx = pool.begin().await.map_err(read("begin a report"))?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(read("begin a report"))?;
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind(format!("{}ms", limit.as_millis()))
        .execute(&mut *tx)
        .await
        .map_err(read("set the report's timeout"))?;
    Ok(tx)
}

/// What the queries filter on, set by [`public`] and [`usage`] alone.
struct Params<'a> {
    /// Count only what `publicly_visible` admits.
    public_only: bool,
    /// The public window in days, for `publicly_visible`.
    window: Option<i32>,
    range: Range,
    asked: Option<&'static str>,
    user: Option<&'a str>,
    /// Whether `user` may also match a Discord user id (the operator only).
    user_by_id: bool,
    model: Option<&'a str>,
}

impl Params<'_> {
    fn from(&self) -> String {
        self.range.from.to_string()
    }

    fn to(&self) -> String {
        self.range.to.to_string()
    }
}

// --- What a report holds ---------------------------------------------------------

/// Calls asked one UTC day, by interface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DayCount {
    /// `YYYY-MM-DD`.
    pub day: String,
    /// `/judge` on Discord.
    pub discord: i64,
    /// The web app or `POST /api/judge`.
    pub web: i64,
    /// The MCP `judge` tool.
    pub mcp: i64,
    /// `judge-cli judge`.
    pub cli: i64,
    /// Persisted by an agent session.
    pub agent_session: i64,
    /// Stored before 2.0.0 by the MCP `judge` tool or `judge-cli judge`.
    pub agent: i64,
    /// Of all the above, the calls asked private: the operator's report
    /// only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private: Option<i64>,
}

impl DayCount {
    /// The day's calls through `asked`.
    #[must_use]
    pub const fn of(&self, asked: Asked) -> i64 {
        match asked {
            Asked::Discord => self.discord,
            Asked::Web => self.web,
            Asked::Mcp => self.mcp,
            Asked::Cli => self.cli,
            Asked::AgentSession => self.agent_session,
            Asked::Agent => self.agent,
        }
    }
}

/// Calls through one interface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InterfaceCount {
    /// The interface.
    pub interface: Asked,
    /// Calls.
    pub questions: i64,
}

/// How the runs in view ended, as the dashboard groups them
/// (`judge_core::OutcomeGroup`); a run whose caller went away is in none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Outcomes {
    /// Answered.
    pub answered: i64,
    /// A "did you mean?".
    pub ambiguous: i64,
    /// A card that matched nothing.
    pub not_found: i64,
    /// Tournament policy, or not a rules question.
    pub out_of_scope: i64,
    /// The model declined.
    pub refused: i64,
    /// Every failure of the pipeline: citations that never validated, the
    /// spend cap, an upstream error.
    pub failed: i64,
}

impl Outcomes {
    fn add(&mut self, group: OutcomeGroup, n: i64) {
        let slot = match group {
            OutcomeGroup::Answered => &mut self.answered,
            OutcomeGroup::Ambiguous => &mut self.ambiguous,
            OutcomeGroup::NotFound => &mut self.not_found,
            OutcomeGroup::OutOfScope => &mut self.out_of_scope,
            OutcomeGroup::Refused => &mut self.refused,
            OutcomeGroup::Failed => &mut self.failed,
        };
        *slot = slot.saturating_add(n);
    }
}

/// Ratings by score.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RatingCounts {
    /// Rated correct.
    pub correct: i64,
    /// Rated partially correct.
    pub partial: i64,
    /// Rated incorrect.
    pub incorrect: i64,
}

/// The ratings of the calls in view: Discord's (which count toward the
/// examples) and the web's anonymous ones, apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RatingMix {
    /// Discord ratings.
    pub discord: RatingCounts,
    /// Web ratings.
    pub web: RatingCounts,
}

/// Calls in one category.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CategoryCount {
    /// The category id (`judge_core::Category`).
    pub category: String,
    /// Its label, from `data/categories.yaml`.
    pub label: String,
    /// Calls.
    pub questions: i64,
}

/// Calls about one card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CardCount {
    /// The card's Oracle id.
    pub id: Uuid,
    /// Its current name.
    pub name: String,
    /// Calls that were about it.
    pub questions: i64,
}

/// Calls by one Discord user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AskerCount {
    /// Their username, as snapshotted when they asked.
    pub name: String,
    /// Calls.
    pub questions: i64,
}

/// What the calls in view cost: their runs' charges. Present only with a
/// [`CostAccess`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, JsonSchema)]
pub struct CostSummary {
    /// USD, summed over the runs that left these calls.
    pub usd: f64,
    /// Calls with a recorded run (an agent session's has none).
    pub questions: i64,
    /// `usd` over `questions`; `None` with no costed call.
    pub per_question_usd: Option<f64>,
    /// Whether any of it is a worst-case estimate.
    pub estimated: bool,
}

/// Which model the calls in view were answered and read with. Present only
/// with a [`ModelAccess`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelCount {
    /// The model, as the response named it.
    pub model: String,
    /// Calls whose answer it wrote (a synthesis or tool-round request).
    pub answered: i64,
    /// Calls whose question it read (the extraction request).
    pub extracted: i64,
}

/// The dashboard: one report, the public's and the operator's alike.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Dashboard {
    /// The first day covered, `YYYY-MM-DD` (UTC). On the public dashboard
    /// never before the feed window's first day.
    pub from: String,
    /// The last day covered.
    pub to: String,
    /// The public window: calls older than this many days are not counted.
    /// Absent without one, and in the operator's report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_days: Option<u16>,
    /// Calls stored in the range.
    pub questions: i64,
    /// Distinct Discord users among their askers.
    pub askers: i64,
    /// Calls by interface, commonest first.
    pub interfaces: Vec<InterfaceCount>,
    /// Calls per day by interface, every day of the range, oldest first.
    pub by_day: Vec<DayCount>,
    /// How the runs in the range ended; `None` under a user filter (runs
    /// record no asker).
    pub outcomes: Option<Outcomes>,
    /// The ratings of these calls.
    pub ratings: RatingMix,
    /// The most-asked categories, commonest first.
    pub categories: Vec<CategoryCount>,
    /// The cards most asked about, commonest first.
    pub cards: Vec<CardCount>,
    /// The Discord users who asked most, commonest first.
    pub top_askers: Vec<AskerCount>,
    /// What these calls cost; absent unless cost is shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostSummary>,
    /// The models that answered them; absent unless models are shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<ModelCount>>,
}

// --- The public report -----------------------------------------------------------

/// The public dashboard for `q`: public calls and runs within `window` only,
/// cost and models as `disclosure` allows.
///
/// # Errors
/// A span that names no report, or the store.
pub async fn public(
    pool: &PgPool,
    window: Option<Days>,
    disclosure: Disclosure,
    q: &PublicQuery,
) -> Result<Dashboard, StatsError> {
    let mut tx = bounded(pool, PUBLIC_TIMEOUT).await?;
    let today = today(&mut tx).await?;
    let mut range = q.span.resolve(today)?;
    // Days before the window hold nothing public: the report starts at it,
    // and a range that ends before it is refused rather than moved.
    if let Some(w) = window {
        let first = today.plus(-i32::from(w.get()));
        if range.to < first {
            return Err(BadRange::BeforeWindow { days: w.get() }.into());
        }
        range.from = range.from.max(first);
    }
    let params = Params {
        public_only: true,
        window: window_days(window),
        range,
        asked: q.asked.map(Asked::id),
        user: q.user.as_ref().map(AsRef::<str>::as_ref),
        user_by_id: false,
        model: q.model.as_ref().map(|m| m.name.as_str()),
    };
    let mut dashboard = dashboard(&mut tx, &params, disclosure.cost(), disclosure.models()).await?;
    tx.commit().await.map_err(read("end a report"))?;
    dashboard.window_days = window.map(Days::get);
    Ok(dashboard)
}

/// The UTC day by the database's clock.
async fn today(conn: &mut PgConnection) -> Result<Date, StatsError> {
    let day = sqlx::query_scalar!(r#"SELECT (now() AT TIME ZONE 'utc')::date::text AS "day!""#)
        .fetch_one(&mut *conn)
        .await
        .map_err(read("read today's date"))?;
    Ok(Date::parse(&day).ok_or_else(|| bad_row(format!("today is {day:?}")))?)
}

/// One dashboard row's JSON parts.
#[derive(Deserialize)]
struct CategoryRow {
    category: String,
    questions: i64,
}

#[derive(Deserialize)]
struct CostRow {
    micro_usd: i64,
    questions: i64,
    estimated: bool,
}

fn json<T: serde::de::DeserializeOwned>(v: serde_json::Value, what: &str) -> Result<T, JudgeError> {
    serde_json::from_value(v).map_err(|e| bad_row_from(e, format!("dashboard {what}")))
}

/// The dashboard over `p`, with cost and model figures only for an access.
async fn dashboard(
    conn: &mut PgConnection,
    p: &Params<'_>,
    cost: Option<CostAccess>,
    models: Option<ModelAccess>,
) -> Result<Dashboard, StatsError> {
    let row = sqlx::query!(
        r#"
        WITH v AS MATERIALIZED (
            -- Two branches, one per scope, each switched on or off as a
            -- whole by $1: the public one says `publicly_visible` with no
            -- condition around it, so even a generic plan proves NOT private
            -- and reads the feed's partial indexes.
            SELECT c.id, c.created_at, c.interface, c.asker_name, c.private, c.category,
                   coalesce(c.context_ids->'cards', '[]'::jsonb) AS cards
            FROM calls c
            WHERE $1::boolean
              AND publicly_visible(c.private, c.created_at, $2::int)
              AND c.created_at >= $3::text::date::timestamp AT TIME ZONE 'utc'
              AND c.created_at < ($4::text::date + 1)::timestamp AT TIME ZONE 'utc'
              AND ($5::text IS NULL OR c.interface = $5)
              AND ($6::text IS NULL OR c.asker_name = $6)
              AND ($8::text IS NULL OR EXISTS (
                    SELECT 1 FROM judge_runs r JOIN judge_run_charges ch ON ch.run_id = r.id
                    WHERE r.call_id = c.id AND ch.model = $8 AND ch.stage <> 'embedding'))
            UNION ALL
            SELECT c.id, c.created_at, c.interface, c.asker_name, c.private, c.category,
                   coalesce(c.context_ids->'cards', '[]'::jsonb) AS cards
            FROM calls c
            WHERE NOT $1::boolean
              AND c.created_at >= $3::text::date::timestamp AT TIME ZONE 'utc'
              AND c.created_at < ($4::text::date + 1)::timestamp AT TIME ZONE 'utc'
              AND ($5::text IS NULL OR c.interface = $5)
              AND ($6::text IS NULL OR c.asker_name = $6 OR ($7::boolean AND c.asker_id = $6))
              AND ($8::text IS NULL OR EXISTS (
                    SELECT 1 FROM judge_runs r JOIN judge_run_charges ch ON ch.run_id = r.id
                    WHERE r.call_id = c.id AND ch.model = $8 AND ch.stage <> 'embedding'))
        ), charged AS MATERIALIZED (
            -- Each run's charges by their key (OFFSET 0 keeps the planner
            -- from flattening it into a scan of every charge), so the work
            -- grows with the range, not with the table.
            SELECT v.id AS call_id, ch.stage, ch.model, ch.micro_usd, ch.cost_estimated
            FROM v JOIN judge_runs r ON r.call_id = v.id
                   CROSS JOIN LATERAL (
                       SELECT c.stage, c.model, c.micro_usd, c.cost_estimated
                       FROM judge_run_charges c WHERE c.run_id = r.id OFFSET 0) ch
            WHERE ($9::boolean OR $10::boolean)
              -- A run is recorded after its call is stored: never earlier
              -- than the range's first call, which lets its index bound it.
              AND r.finished_at >= $3::text::date::timestamp AT TIME ZONE 'utc'
        )
        SELECT
            (SELECT count(*) FROM (SELECT DISTINCT asker_name FROM v WHERE asker_name IS NOT NULL) a
            ) AS "askers!",
            (SELECT coalesce(jsonb_agg(to_jsonb(d) ORDER BY d.day), '[]'::jsonb) FROM (
                SELECT g.day::date::text AS day,
                       coalesce(n.discord, 0) AS discord, coalesce(n.web, 0) AS web,
                       coalesce(n.mcp, 0) AS mcp, coalesce(n.cli, 0) AS cli,
                       coalesce(n.agent_session, 0) AS agent_session,
                       coalesce(n.agent, 0) AS agent, coalesce(n.private, 0) AS private
                FROM generate_series($3::text::date, $4::text::date, interval '1 day') AS g(day)
                LEFT JOIN (
                    SELECT (created_at AT TIME ZONE 'utc')::date AS day,
                           count(*) FILTER (WHERE interface = 'discord') AS discord,
                           count(*) FILTER (WHERE interface = 'web') AS web,
                           count(*) FILTER (WHERE interface = 'mcp') AS mcp,
                           count(*) FILTER (WHERE interface = 'cli') AS cli,
                           count(*) FILTER (WHERE interface = 'agent_session') AS agent_session,
                           count(*) FILTER (WHERE interface = 'agent') AS agent,
                           count(*) FILTER (WHERE private) AS private
                    FROM v GROUP BY 1
                ) n ON n.day = g.day::date
            ) d) AS "by_day!",
            (SELECT coalesce(jsonb_agg(to_jsonb(k) ORDER BY k.questions DESC, k.category), '[]'::jsonb)
             FROM (SELECT category, count(*) AS questions FROM v
                   GROUP BY category ORDER BY 2 DESC, 1 LIMIT $11) k
            ) AS "categories!",
            (SELECT coalesce(jsonb_agg(to_jsonb(k) ORDER BY k.questions DESC, k.name), '[]'::jsonb)
             FROM (SELECT ca.oracle_id AS id, ca.name, n.questions
                   FROM (SELECT x.card, count(DISTINCT v.id) AS questions
                         FROM v CROSS JOIN LATERAL jsonb_array_elements_text(
                             CASE WHEN jsonb_typeof(v.cards) = 'array' THEN v.cards
                                  ELSE '[]'::jsonb END) AS x(card)
                         GROUP BY x.card) n
                   JOIN cards ca ON ca.oracle_id::text = n.card
                   ORDER BY n.questions DESC, ca.name LIMIT $11) k
            ) AS "cards!",
            (SELECT coalesce(jsonb_agg(to_jsonb(k) ORDER BY k.questions DESC, k.name), '[]'::jsonb)
             FROM (SELECT asker_name AS name, count(*) AS questions FROM v
                   WHERE asker_name IS NOT NULL
                   GROUP BY asker_name ORDER BY 2 DESC, 1 LIMIT $11) k
            ) AS "top_askers!",
            (SELECT jsonb_build_object(
                 'discord', jsonb_build_object(
                     'correct', count(*) FILTER (WHERE rt.rater_kind = 'discord' AND rt.score = 3),
                     'partial', count(*) FILTER (WHERE rt.rater_kind = 'discord' AND rt.score = 2),
                     'incorrect', count(*) FILTER (WHERE rt.rater_kind = 'discord' AND rt.score = 1)),
                 'web', jsonb_build_object(
                     'correct', count(*) FILTER (WHERE rt.rater_kind = 'web' AND rt.score = 3),
                     'partial', count(*) FILTER (WHERE rt.rater_kind = 'web' AND rt.score = 2),
                     'incorrect', count(*) FILTER (WHERE rt.rater_kind = 'web' AND rt.score = 1)))
             FROM ratings rt JOIN v ON v.id = rt.call_id
            ) AS "ratings!",
            CASE WHEN $9::boolean THEN (
                SELECT jsonb_build_object(
                    'micro_usd', coalesce(sum(micro_usd), 0)::bigint,
                    'questions', count(DISTINCT call_id),
                    'estimated', coalesce(bool_or(cost_estimated), false))
                FROM charged
            ) END AS cost,
            CASE WHEN $10::boolean THEN (
                SELECT coalesce(jsonb_agg(to_jsonb(m) ORDER BY m.answered DESC, m.extracted DESC, m.model), '[]'::jsonb)
                FROM (SELECT model,
                             count(DISTINCT call_id) FILTER (WHERE stage IN ('synthesis', 'tool_round')) AS answered,
                             count(DISTINCT call_id) FILTER (WHERE stage = 'extraction') AS extracted
                      FROM charged WHERE stage <> 'embedding' GROUP BY model) m
            ) END AS models
        "#,
        p.public_only,
        p.window,
        p.from(),
        p.to(),
        p.asked,
        p.user,
        p.user_by_id,
        p.model,
        cost.is_some(),
        models.is_some(),
        TOP,
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(read("read the dashboard"))?;

    let by_day: Vec<DayCount> = json::<Vec<DayCount>>(row.by_day, "by_day")?
        .into_iter()
        // The private count is the operator's: in the public report it is
        // always 0, and left out.
        .map(|d| DayCount {
            private: d.private.filter(|_| !p.public_only),
            ..d
        })
        .collect();
    // The totals are the days': one pass over the calls, not three.
    let mut interfaces: Vec<InterfaceCount> = Asked::ALL
        .into_iter()
        .map(|asked| InterfaceCount {
            interface: asked,
            questions: by_day.iter().map(|d| d.of(asked)).sum(),
        })
        .filter(|i| i.questions > 0)
        .collect();
    interfaces.sort_by(|a, b| {
        b.questions
            .cmp(&a.questions)
            .then_with(|| a.interface.id().cmp(b.interface.id()))
    });
    let categories = json::<Vec<CategoryRow>>(row.categories, "categories")?
        .into_iter()
        .map(|c| CategoryCount {
            label: Category::from_str(&c.category)
                .map_or_else(|_| c.category.clone(), |k| k.label().to_owned()),
            category: c.category,
            questions: c.questions,
        })
        .collect();
    let outcomes = match p.user {
        Some(_) => None,
        None => Some(grouped(&run_outcomes(conn, p).await?)),
    };
    Ok(Dashboard {
        from: p.from(),
        to: p.to(),
        window_days: None,
        questions: interfaces.iter().map(|i| i.questions).sum(),
        askers: row.askers,
        interfaces,
        by_day,
        outcomes,
        ratings: json(row.ratings, "ratings")?,
        categories,
        cards: json(row.cards, "cards")?,
        top_askers: json(row.top_askers, "top_askers")?,
        cost: match (cost, row.cost) {
            (Some(_), Some(v)) => {
                let c: CostRow = json(v, "cost")?;
                Some(CostSummary {
                    usd: usd(c.micro_usd),
                    questions: c.questions,
                    per_question_usd: (c.questions > 0).then(|| {
                        #[expect(
                            clippy::cast_precision_loss,
                            reason = "a count of calls stays far below 2^53"
                        )]
                        let n = c.questions as f64;
                        usd(c.micro_usd) / n
                    }),
                    estimated: c.estimated,
                })
            }
            _ => None,
        },
        models: match (models, row.models) {
            (Some(_), Some(v)) => Some(json(v, "models")?),
            _ => None,
        },
    })
}

/// How many runs in the range ended one way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct OutcomeCount {
    /// `answered`, `ambiguous`, …: `judge_core::Outcome`'s ids.
    pub outcome: String,
    /// Runs.
    pub runs: i64,
}

/// The runs in the range by outcome, commonest first: public ones within
/// the window only when `p` is public. Not for a user filter: runs record
/// no asker.
async fn run_outcomes(
    conn: &mut PgConnection,
    p: &Params<'_>,
) -> Result<Vec<OutcomeCount>, StatsError> {
    Ok(sqlx::query!(
        r#"
        SELECT r.outcome AS "outcome!", count(*) AS "runs!"
        FROM (
            -- One branch per scope, as the dashboard's calls: the public one
            -- reads `judge_runs_public_finished_at` even in a generic plan.
            SELECT r.outcome FROM judge_runs r
            WHERE $1::boolean
              AND publicly_visible(r.private, r.finished_at, $2::int)
              AND r.finished_at >= $3::text::date::timestamp AT TIME ZONE 'utc'
              AND r.finished_at < ($4::text::date + 1)::timestamp AT TIME ZONE 'utc'
              AND ($5::text IS NULL OR r.interface = $5)
              AND ($6::text IS NULL OR EXISTS (
                    SELECT 1 FROM judge_run_charges ch
                    WHERE ch.run_id = r.id AND ch.model = $6 AND ch.stage <> 'embedding'))
            UNION ALL
            SELECT r.outcome FROM judge_runs r
            WHERE NOT $1::boolean
              AND r.finished_at >= $3::text::date::timestamp AT TIME ZONE 'utc'
              AND r.finished_at < ($4::text::date + 1)::timestamp AT TIME ZONE 'utc'
              AND ($5::text IS NULL OR r.interface = $5)
              AND ($6::text IS NULL OR EXISTS (
                    SELECT 1 FROM judge_run_charges ch
                    WHERE ch.run_id = r.id AND ch.model = $6 AND ch.stage <> 'embedding'))
        ) r
        GROUP BY r.outcome
        ORDER BY 2 DESC, 1
        "#,
        p.public_only,
        p.window,
        p.from(),
        p.to(),
        p.asked,
        p.model,
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(read("read the run outcomes"))?
    .into_iter()
    .map(|r| OutcomeCount {
        outcome: r.outcome,
        runs: r.runs,
    })
    .collect())
}

/// `counts` in the dashboard's groups. An outcome this build does not know
/// (a newer build of the same major wrote it) is left out with a WARN rather
/// than failing the report.
fn grouped(counts: &[OutcomeCount]) -> Outcomes {
    let mut out = Outcomes::default();
    for c in counts {
        let Some(outcome) = Outcome::parse(&c.outcome) else {
            tracing::warn!(outcome = %c.outcome, runs = c.runs, "judge_runs.outcome this build does not know; left out of the dashboard's outcomes");
            continue;
        };
        if let Some(group) = outcome.group() {
            out.add(group, c.runs);
        }
    }
    out
}

// --- The operator's report -------------------------------------------------------

/// The budget now: the period and the cap the instance's processes run
/// with, as the last of them recorded it (`budget_setting`, written by
/// every process that keeps the spend ledger), and what is left of it.
/// Whoever reads it (the admin page, `judge-cli stats`, MCP) gets the
/// instance's settings, never its own.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct BudgetStatus {
    /// `process`, `day` or `month` (`JUDGE_BUDGET_PERIOD`); `None` when no
    /// process has recorded its settings yet.
    pub period: Option<String>,
    /// `JUDGE_MAX_USD`, in USD; `None` likewise.
    pub cap_usd: Option<f64>,
    /// When the settings were last recorded, RFC 3339 UTC.
    pub recorded_at: Option<String>,
    /// The period's first day (UTC); `None` for `process`, where each
    /// process counts its own spend from its start, and when unknown.
    pub period_start: Option<String>,
    /// Spent in the period by every process (the `spend_days` ledger);
    /// `None` likewise.
    pub spent_usd: Option<f64>,
    /// The cap less that, never below 0; `None` likewise.
    pub left_usd: Option<f64>,
}

/// One UTC day of the spend ledger.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct SpendDay {
    /// `YYYY-MM-DD`.
    pub day: String,
    /// Model spend that day, chat calls and embeddings together, of the
    /// `judgebot` processes and `judgebot ingest`.
    pub usd: f64,
    /// Chat model calls behind it (embedding requests are not counted).
    pub llm_calls: i64,
}

/// One UTC month of the spend ledger, within the range.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct SpendMonth {
    /// `YYYY-MM`.
    pub month: String,
    /// Spend on the range's days of that month.
    pub usd: f64,
    /// Chat model calls behind it.
    pub llm_calls: i64,
}

/// What one model was billed for by the runs in the range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
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

/// What the runs through one interface cost.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InterfaceSpend {
    /// `discord`, `web`, `mcp` or `cli`.
    pub interface: String,
    /// Runs.
    pub runs: i64,
    /// Their cost in USD.
    pub usd: f64,
}

/// What one stage of the pipeline cost.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StageSpend {
    /// `extraction`, `synthesis`, `tool_round` or `embedding`.
    pub stage: String,
    /// Billed requests.
    pub requests: i64,
    /// Their cost in USD.
    pub usd: f64,
}

/// How long answered runs took, start to finish.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Latency {
    /// Answered runs measured.
    pub runs: i64,
    /// The median, in milliseconds.
    pub p50_ms: f64,
    /// The 90th percentile.
    pub p90_ms: f64,
    /// The slowest.
    pub max_ms: f64,
    /// The mean.
    pub mean_ms: f64,
}

/// One of the dearest runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExpensiveRun {
    /// The run.
    pub run: Uuid,
    /// The call it left, when it left one.
    pub call: Option<Uuid>,
    /// Its question (the call's, or the failed call's), cut to a line.
    pub question: Option<String>,
    /// The interface.
    pub interface: String,
    /// How it ended.
    pub outcome: String,
    /// When it finished, RFC 3339 UTC.
    pub at: String,
    /// Its cost in USD.
    pub usd: f64,
    /// Whether that includes a worst-case estimate.
    pub estimated: bool,
}

/// The runs of the built-in pipeline in the range: what they cost and how
/// long they took (an agent session has no run).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct RunStats {
    /// Runs that finished in the range, whatever their outcome.
    pub runs: i64,
    /// What they cost, in USD: their charges summed. Less than the ledger's
    /// spend, which also counts what no run made (the refresh's embeddings,
    /// a search's).
    pub usd: f64,
    /// Requests charged at the worst case (they reported no usage).
    pub estimated: i64,
    /// By outcome (`judge_core::Outcome`'s ids), commonest first.
    pub outcomes: Vec<OutcomeCount>,
    /// Runs the spend cap refused (`at_cap`).
    pub refused_at_cap: i64,
    /// Charges by model, dearest first.
    pub by_model: Vec<ModelUse>,
    /// Runs and cost by interface, dearest first.
    pub by_interface: Vec<InterfaceSpend>,
    /// Charges by stage, dearest first.
    pub by_stage: Vec<StageSpend>,
    /// The mean cost of an answered run, in USD.
    pub usd_per_answer: Option<f64>,
    /// How long answered runs took; `None` without one.
    pub latency: Option<Latency>,
    /// The dearest runs, dearest first.
    pub most_expensive: Vec<ExpensiveRun>,
}

/// A call people rated badly.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct RatedCall {
    /// Call id.
    pub call: Uuid,
    /// The question, cut to a line.
    pub question: String,
    /// The score retrieval uses: the judge override, else the smoothed mean.
    pub effective_score: f32,
    /// Ratings behind it.
    pub ratings: i32,
    /// Whether the retirement pass has already taken it out of retrieval.
    pub retired: bool,
}

/// The operator's report (`judge-cli stats`, `/admin/api/usage`, the MCP
/// tool `admin_usage`): the [`Dashboard`] over every call, private ones
/// included, then the spend, the budget, the runs, and the instance's
/// state.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Usage {
    /// The dashboard, private calls and runs counted.
    #[serde(flatten)]
    pub dashboard: Dashboard,
    /// Days covered.
    pub days: u32,
    /// Of the calls, the ones asked private.
    pub private: i64,
    /// Failed calls stored in the range (`failed_calls`, which keeps 30 days
    /// and 500 rows).
    pub failed_calls: i64,
    /// Model spend in the range, in USD, from the `spend_days` ledger: every
    /// long-running process and `judgebot ingest`, chat and embeddings.
    /// Not narrowed by a filter.
    pub usd: f64,
    /// That spend per day with any, oldest first.
    pub spend_by_day: Vec<SpendDay>,
    /// That spend per month, oldest first.
    pub spend_by_month: Vec<SpendMonth>,
    /// The budget now, whatever the range.
    pub budget: BudgetStatus,
    /// The runs in the range; `None` under a user filter (runs record no
    /// asker).
    pub run_stats: Option<RunStats>,
    /// Calls in the whole table the retirement pass has retired.
    pub retired_calls: i64,
    /// The rated calls with the lowest effective score, worst first,
    /// whatever the range.
    pub lowest_rated: Vec<RatedCall>,
    /// The feedback queue, whatever the range: items per category by
    /// status, every category listed ([`crate::feedback::counts`]).
    pub feedback: Vec<crate::feedback::QueueCount>,
    /// The latest data refresh runs ([`RECENT_RUNS`]), newest first,
    /// whatever the range.
    pub refresh_runs: Vec<RecentRun>,
    /// Why `refresh_runs` is empty when it could not be read: the schema
    /// predates the table (migrations pending). `None` when it was read.
    pub refresh_runs_note: Option<String>,
}

/// The operator's report for `q`, every call counted.
///
/// # Errors
/// A span that names no report, a query past [`OPERATOR_TIMEOUT`], or the
/// store.
pub async fn usage(pool: &PgPool, q: &OperatorQuery) -> Result<Usage, StatsError> {
    let mut tx = bounded(pool, OPERATOR_TIMEOUT).await?;
    let today = today(&mut tx).await?;
    let range = q.span.resolve(today)?;
    let params = Params {
        public_only: false,
        window: None,
        range,
        asked: q.asked.map(Asked::id),
        user: q.asker.as_ref().map(FilterText::as_str),
        user_by_id: true,
        model: q.model.as_ref().map(FilterText::as_str),
    };
    let dashboard = dashboard(
        &mut tx,
        &params,
        Some(CostAccess(())),
        Some(ModelAccess(())),
    )
    .await?;
    let private = dashboard
        .by_day
        .iter()
        .map(|d| d.private.unwrap_or(0))
        .sum();
    let spend_by_day = spend_days(&mut tx, range).await?;
    let mut months: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for d in &spend_by_day {
        let month = d.day.get(..7).unwrap_or(&d.day).to_owned();
        let m = months.entry(month).or_default();
        m.0 = m.0.saturating_add(micro(d.usd));
        m.1 = m.1.saturating_add(d.llm_calls);
    }
    let spend_by_month = months
        .into_iter()
        .map(|(month, (micro, calls))| SpendMonth {
            month,
            usd: usd(micro),
            llm_calls: calls,
        })
        .collect();
    let run_stats = match params.user {
        Some(_) => None,
        None => Some(run_stats(&mut tx, &params).await?),
    };
    let failed_calls = failed_count(&mut tx, &params).await?;
    let budget = budget_status(&mut tx).await?;
    let retired_calls = retired_calls(&mut tx).await?;
    let lowest_rated = lowest_rated(&mut tx).await?;
    tx.commit().await.map_err(read("end a report"))?;
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
        Err(e) => return Err(JudgeError::Upstream(e).into()),
    };
    Ok(Usage {
        days: range.days(),
        private,
        failed_calls,
        usd: spend_by_day.iter().map(|d| d.usd).sum(),
        spend_by_day,
        spend_by_month,
        budget,
        run_stats,
        retired_calls,
        lowest_rated,
        feedback: crate::feedback::counts(pool).await?,
        refresh_runs,
        refresh_runs_note,
        dashboard,
    })
}

/// The ledger's days in `range`, oldest first.
async fn spend_days(conn: &mut PgConnection, range: Range) -> Result<Vec<SpendDay>, StatsError> {
    Ok(sqlx::query!(
        r#"
        SELECT day::text AS "day!", micro_usd, calls
        FROM spend_days
        WHERE day BETWEEN $1::text::date AND $2::text::date
        ORDER BY day
        "#,
        range.from.to_string(),
        range.to.to_string(),
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(read("read the spend ledger"))?
    .into_iter()
    .map(|r| SpendDay {
        day: r.day,
        usd: usd(r.micro_usd),
        llm_calls: r.calls,
    })
    .collect())
}

/// The budget now, from the settings the instance's processes recorded.
async fn budget_status(conn: &mut PgConnection) -> Result<BudgetStatus, StatsError> {
    let row = sqlx::query!(
        r#"
        SELECT period, cap_micro,
               to_char(written_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "recorded_at!"
        FROM budget_setting
        "#
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(read("read the budget settings"))?;
    let Some(row) = row else {
        return Ok(BudgetStatus {
            period: None,
            cap_usd: None,
            recorded_at: None,
            period_start: None,
            spent_usd: None,
            left_usd: None,
        });
    };
    let period = Period::parse(Some(&row.period))
        .map_err(|v| StatsError::Store(bad_row(format!("budget_setting.period {v:?}"))))?;
    let cap = usd(row.cap_micro);
    let field = match period {
        Period::Process => None,
        Period::Day => Some("day"),
        Period::Month => Some("month"),
    };
    let (start, spent) = match field {
        None => (None, None),
        Some(field) => {
            let r = sqlx::query!(
                r#"
                SELECT date_trunc($1, now() AT TIME ZONE 'utc')::date::text AS "start!",
                       (SELECT coalesce(sum(micro_usd), 0)::bigint FROM spend_days
                        WHERE day >= date_trunc($1, now() AT TIME ZONE 'utc')::date) AS "spent!"
                "#,
                field
            )
            .fetch_one(&mut *conn)
            .await
            .map_err(read("read the budget period"))?;
            (Some(r.start), Some(usd(r.spent)))
        }
    };
    Ok(BudgetStatus {
        period: Some(period.as_str().to_owned()),
        cap_usd: Some(cap),
        recorded_at: Some(row.recorded_at),
        period_start: start,
        spent_usd: spent,
        left_usd: spent.map(|s| (cap - s).max(0.0)),
    })
}

/// Failed calls stored in the range, as the filters narrow them.
async fn failed_count(conn: &mut PgConnection, p: &Params<'_>) -> Result<i64, StatsError> {
    sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "n!" FROM failed_calls f
        WHERE f.created_at >= $1::text::date::timestamp AT TIME ZONE 'utc'
          AND f.created_at < ($2::text::date + 1)::timestamp AT TIME ZONE 'utc'
          AND ($3::text IS NULL OR f.interface = $3)
          AND ($4::text IS NULL OR f.asker_name = $4 OR f.asker_id = $4)
          AND ($5::text IS NULL OR EXISTS (
                SELECT 1 FROM judge_runs r JOIN judge_run_charges ch ON ch.run_id = r.id
                WHERE r.failed_call_id = f.id AND ch.model = $5 AND ch.stage <> 'embedding'))
        "#,
        p.from(),
        p.to(),
        p.asked,
        p.user,
        p.model,
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(read("count the failed calls"))
}

#[derive(Deserialize)]
struct LatencyRow {
    runs: i64,
    p50_ms: Option<f64>,
    p90_ms: Option<f64>,
    max_ms: Option<f64>,
    mean_ms: Option<f64>,
}

#[derive(Deserialize)]
struct MicroRow<T> {
    #[serde(flatten)]
    row: T,
    micro_usd: i64,
}

#[derive(Deserialize)]
struct ModelUseRow {
    provider: String,
    model: String,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    estimated: i64,
}

#[derive(Deserialize)]
struct InterfaceRow {
    interface: String,
    runs: i64,
}

#[derive(Deserialize)]
struct StageRow {
    stage: String,
    requests: i64,
}

#[derive(Deserialize)]
struct ExpensiveRow {
    run: Uuid,
    call: Option<Uuid>,
    question: Option<String>,
    interface: String,
    outcome: String,
    at: String,
    estimated: bool,
}

/// The operator's run figures over `p`, which is never public and never
/// filters by user.
async fn run_stats(conn: &mut PgConnection, p: &Params<'_>) -> Result<RunStats, StatsError> {
    let outcomes = run_outcomes(conn, p).await?;
    let row = sqlx::query!(
        r#"
        WITH rv AS MATERIALIZED (
            SELECT r.id, r.started_at, r.finished_at, r.interface, r.outcome, r.call_id,
                   r.failed_call_id
            FROM judge_runs r
            WHERE r.finished_at >= $1::text::date::timestamp AT TIME ZONE 'utc'
              AND r.finished_at < ($2::text::date + 1)::timestamp AT TIME ZONE 'utc'
              AND ($3::text IS NULL OR r.interface = $3)
              AND ($4::text IS NULL OR EXISTS (
                    SELECT 1 FROM judge_run_charges ch
                    WHERE ch.run_id = r.id AND ch.model = $4 AND ch.stage <> 'embedding'))
        ), ch AS MATERIALIZED (
            -- Each run's charges by their key (OFFSET 0 keeps the planner
            -- from flattening it into a scan of every charge), so the work
            -- grows with the range, not with the table.
            SELECT rv.id AS run_id, c.stage, c.provider, c.model, c.input_tokens, c.output_tokens,
                   c.cache_read_tokens, c.cache_write_tokens, c.micro_usd, c.cost_estimated
            FROM rv CROSS JOIN LATERAL (
                SELECT * FROM judge_run_charges x WHERE x.run_id = rv.id OFFSET 0) c
        ), cost AS (
            SELECT run_id, sum(micro_usd)::bigint AS micro_usd,
                   bool_or(cost_estimated) AS estimated
            FROM ch GROUP BY run_id
        )
        SELECT
            (SELECT count(*) FROM rv) AS "runs!",
            (SELECT coalesce(sum(micro_usd), 0)::bigint FROM ch) AS "micro_usd!",
            (SELECT count(*) FILTER (WHERE cost_estimated) FROM ch) AS "estimated!",
            (SELECT coalesce(jsonb_agg(to_jsonb(m) ORDER BY m.micro_usd DESC, m.provider, m.model), '[]'::jsonb)
             FROM (SELECT provider, model, count(*) AS requests,
                          coalesce(sum(input_tokens), 0)::bigint AS input_tokens,
                          coalesce(sum(output_tokens), 0)::bigint AS output_tokens,
                          coalesce(sum(cache_read_tokens), 0)::bigint AS cache_read_tokens,
                          coalesce(sum(cache_write_tokens), 0)::bigint AS cache_write_tokens,
                          coalesce(sum(micro_usd), 0)::bigint AS micro_usd,
                          count(*) FILTER (WHERE cost_estimated) AS estimated
                   FROM ch GROUP BY provider, model) m
            ) AS "by_model!",
            (SELECT coalesce(jsonb_agg(to_jsonb(i) ORDER BY i.micro_usd DESC, i.interface), '[]'::jsonb)
             FROM (SELECT rv.interface, count(*) AS runs,
                          coalesce(sum(cost.micro_usd), 0)::bigint AS micro_usd
                   FROM rv LEFT JOIN cost ON cost.run_id = rv.id GROUP BY rv.interface) i
            ) AS "by_interface!",
            (SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY s.micro_usd DESC, s.stage), '[]'::jsonb)
             FROM (SELECT stage, count(*) AS requests,
                          coalesce(sum(micro_usd), 0)::bigint AS micro_usd
                   FROM ch GROUP BY stage) s
            ) AS "by_stage!",
            (SELECT avg(cost.micro_usd)::float8 FROM rv JOIN cost ON cost.run_id = rv.id
             WHERE rv.outcome = 'answered') AS per_answer_micro,
            (SELECT jsonb_build_object(
                 'runs', count(*),
                 'p50_ms', percentile_cont(0.5) WITHIN GROUP (ORDER BY ms),
                 'p90_ms', percentile_cont(0.9) WITHIN GROUP (ORDER BY ms),
                 'max_ms', max(ms),
                 'mean_ms', avg(ms))
             FROM (SELECT (extract(epoch FROM finished_at - started_at) * 1000)::float8 AS ms
                   FROM rv WHERE outcome = 'answered') t
            ) AS "latency!",
            (SELECT coalesce(jsonb_agg(to_jsonb(e) ORDER BY e.micro_usd DESC, e.at DESC), '[]'::jsonb)
             FROM (SELECT t.run, t.call, coalesce(ca.question, f.question) AS question,
                          t.interface, t.outcome,
                          to_char(t.finished_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS at,
                          t.micro_usd, t.estimated
                   -- The dearest first, then their questions: ten lookups.
                   FROM (SELECT rv.id AS run, rv.call_id AS call, rv.failed_call_id,
                                rv.interface, rv.outcome, rv.finished_at, cost.micro_usd,
                                coalesce(cost.estimated, false) AS estimated
                         FROM rv JOIN cost ON cost.run_id = rv.id
                         ORDER BY cost.micro_usd DESC, rv.finished_at DESC
                         LIMIT $5) t
                   LEFT JOIN calls ca ON ca.id = t.call
                   LEFT JOIN failed_calls f ON f.id = t.failed_call_id) e
            ) AS "most_expensive!"
        "#,
        p.from(),
        p.to(),
        p.asked,
        p.model,
        TOP,
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(read("read the run figures"))?;

    let latency: LatencyRow = json(row.latency, "latency")?;
    Ok(RunStats {
        runs: row.runs,
        usd: usd(row.micro_usd),
        estimated: row.estimated,
        refused_at_cap: outcomes
            .iter()
            .filter(|o| o.outcome == Outcome::AtCap.id())
            .map(|o| o.runs)
            .sum(),
        outcomes,
        by_model: json::<Vec<MicroRow<ModelUseRow>>>(row.by_model, "by_model")?
            .into_iter()
            .map(|m| ModelUse {
                provider: m.row.provider,
                model: m.row.model,
                requests: m.row.requests,
                input_tokens: m.row.input_tokens,
                output_tokens: m.row.output_tokens,
                cache_read_tokens: m.row.cache_read_tokens,
                cache_write_tokens: m.row.cache_write_tokens,
                usd: usd(m.micro_usd),
                estimated: m.row.estimated,
            })
            .collect(),
        by_interface: json::<Vec<MicroRow<InterfaceRow>>>(row.by_interface, "by_interface")?
            .into_iter()
            .map(|i| InterfaceSpend {
                interface: i.row.interface,
                runs: i.row.runs,
                usd: usd(i.micro_usd),
            })
            .collect(),
        by_stage: json::<Vec<MicroRow<StageRow>>>(row.by_stage, "by_stage")?
            .into_iter()
            .map(|s| StageSpend {
                stage: s.row.stage,
                requests: s.row.requests,
                usd: usd(s.micro_usd),
            })
            .collect(),
        usd_per_answer: row.per_answer_micro.map(|m| m / 1_000_000.0),
        latency: match (
            latency.p50_ms,
            latency.p90_ms,
            latency.max_ms,
            latency.mean_ms,
        ) {
            (Some(p50_ms), Some(p90_ms), Some(max_ms), Some(mean_ms)) if latency.runs > 0 => {
                Some(Latency {
                    runs: latency.runs,
                    p50_ms,
                    p90_ms,
                    max_ms,
                    mean_ms,
                })
            }
            _ => None,
        },
        most_expensive: json::<Vec<MicroRow<ExpensiveRow>>>(row.most_expensive, "most_expensive")?
            .into_iter()
            .map(|e| ExpensiveRun {
                run: e.row.run,
                call: e.row.call,
                question: e.row.question.as_deref().map(one_line),
                interface: e.row.interface,
                outcome: e.row.outcome,
                at: e.row.at,
                usd: usd(e.micro_usd),
                estimated: e.row.estimated,
            })
            .collect(),
    })
}

async fn retired_calls(conn: &mut PgConnection) -> Result<i64, StatsError> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM calls WHERE retired_at IS NOT NULL"#)
        .fetch_one(&mut *conn)
        .await
        .map_err(read("count the retired calls"))
}

async fn lowest_rated(conn: &mut PgConnection) -> Result<Vec<RatedCall>, StatsError> {
    Ok(sqlx::query!(
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
    .fetch_all(&mut *conn)
    .await
    .map_err(read("read the lowest-rated calls"))?
    .into_iter()
    .map(|r| RatedCall {
        call: r.id,
        question: one_line(&r.question),
        effective_score: r.effective_score,
        ratings: r.n,
        retired: r.retired,
    })
    .collect())
}

// --- Failed calls ------------------------------------------------------------------

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

fn usd(micro: i64) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "micro-dollar amounts stay far below 2^53"
    )]
    let usd = micro as f64 / 1_000_000.0;
    usd
}

/// `usd` back in micro-dollars, for summing without float drift.
fn micro(usd: f64) -> i64 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a ledger amount in micro-dollars is far inside i64"
    )]
    let micro = (usd * 1_000_000.0).round() as i64;
    micro
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
