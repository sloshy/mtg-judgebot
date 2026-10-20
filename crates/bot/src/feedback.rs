//! The feedback queue (#27, D38) and the sweep of web raters' addresses.
//!
//! Every rating given with a reason (Partially correct or Incorrect, from
//! Discord's form or, later, the web's) opens an item in `feedback_queue`,
//! keyed on the rating. The operator works it from the admin page,
//! `judge-cli` and the admin MCP tools: [`list`] the items, [`show`] one with
//! the call's full material, and [`close`] it as resolved or dismissed with a
//! note. A rater who replaces their reason reopens the item; one who re-rates
//! as Correct drops an item nobody has written on, and leaves one with a note
//! or a "fixed by" in place (`db::calls`).
//!
//! A web rating keeps the address and user agent it came from for
//! [`IpDays`] (`FEEDBACK_IP_DAYS`, default 90): [`sweep`] clears both on
//! older ratings and keeps the feedback. It runs on every rating written and
//! as the refresh's `sweep` step, so an idle instance still clears them.

use std::num::NonZeroU16;

use judge_core::{Close, FeedbackCategory, JudgeError, QueueStatus, RaterKind};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

pub use crate::feed::Cursor;
use crate::{
    admin::CallDetail,
    db::{bad_row, upstream},
    feed::{Asked, Names},
};

/// `FEEDBACK_IP_DAYS`: how long a web rating keeps its address and user agent.
pub const IP_DAYS_ENV: &str = "FEEDBACK_IP_DAYS";
/// [`IP_DAYS_ENV`]'s default.
pub const DEFAULT_IP_DAYS: u16 = 90;
/// The longest window the variable takes: ten years.
pub const MAX_IP_DAYS: u16 = 3650;
/// Items per page of the queue.
pub const PAGE_SIZE: usize = 50;
/// Characters of a question shown in a queue row.
const QUESTION_CHARS: usize = 200;

/// How many days a web rating keeps its address and user agent: 1 to
/// [`MAX_IP_DAYS`], [`DEFAULT_IP_DAYS`] when unset. There is no "keep
/// forever".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpDays(NonZeroU16);

impl Default for IpDays {
    fn default() -> Self {
        Self(NonZeroU16::new(DEFAULT_IP_DAYS).unwrap_or(NonZeroU16::MIN))
    }
}

impl IpDays {
    /// `days` if it is in range.
    #[must_use]
    pub const fn new(days: u16) -> Option<Self> {
        match NonZeroU16::new(days) {
            Some(d) if days <= MAX_IP_DAYS => Some(Self(d)),
            _ => None,
        }
    }

    /// Parse `FEEDBACK_IP_DAYS`: blank is the default.
    ///
    /// # Errors
    /// The value, when it is not a whole number from 1 to [`MAX_IP_DAYS`].
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        let Some(v) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(Self::default());
        };
        v.parse::<u16>()
            .ok()
            .and_then(Self::new)
            .ok_or_else(|| v.to_owned())
    }

    /// The window this process's environment sets. `Config::load` refuses a
    /// bad value at startup, so a process that got this far has none; one
    /// that did not load a `Config` is told, and gets the default.
    #[must_use]
    pub fn from_env() -> Self {
        Self::parse(std::env::var(IP_DAYS_ENV).ok().as_deref()).unwrap_or_else(|value| {
            tracing::warn!(
                %value,
                "{IP_DAYS_ENV} is not a whole number of days from 1 to {MAX_IP_DAYS}; using {DEFAULT_IP_DAYS}"
            );
            Self::default()
        })
    }

    /// The number of days.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

/// Clear the address and user agent of every rating written more than
/// `days` ago, keeping the rating and its reason. Returns how many ratings
/// it cleared. A row another writer holds is skipped, never waited for: two
/// ratings written at once each sweep, and neither can deadlock the other;
/// what one skips, the next sweep clears.
///
/// # Errors
/// The database.
pub async fn sweep<'e>(e: impl PgExecutor<'e>, days: IpDays) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        r#"
        UPDATE ratings SET ip = NULL, user_agent = NULL
        WHERE (call_id, user_id) IN (
            SELECT call_id, user_id FROM ratings
            WHERE (ip IS NOT NULL OR user_agent IS NOT NULL)
              AND ts < now() - make_interval(days => $1)
            FOR UPDATE SKIP LOCKED
        )
        "#,
        i32::from(days.get()),
    )
    .execute(e)
    .await?
    .rows_affected())
}

/// What a sweep cleared. Stored in a refresh run's record (`ingest::runs`),
/// so its field names are a stored shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Swept {
    /// The window it applied, in days.
    pub days: u16,
    /// Ratings whose address and user agent it cleared.
    pub ratings: u64,
}

/// A score as the admin surfaces name it.
pub(crate) fn score_id(score: i16) -> Result<&'static str, JudgeError> {
    match score {
        1 => Ok("incorrect"),
        2 => Ok("partial"),
        3 => Ok("correct"),
        n => Err(bad_row(format!("ratings.score {n}"))),
    }
}

fn stored<T>(raw: &str, column: &str, parse: impl Fn(&str) -> Option<T>) -> Result<T, JudgeError> {
    parse(raw).ok_or_else(|| bad_row(format!("{column} {raw:?}")))
}

/// Which items a page of the queue holds. Every field narrows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueFilter {
    /// Only items in this state.
    pub status: Option<QueueStatus>,
    /// Only items whose reason has this category.
    pub category: Option<FeedbackCategory>,
    /// Only items on calls asked through this interface.
    pub asked: Option<Asked>,
    /// Only items from this kind of rater.
    pub rater: Option<RaterKind>,
}

/// One queue item: the rating and its reason, the operator's state, and
/// enough of the call to recognise it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct QueueItem {
    /// The item.
    pub id: Uuid,
    /// The call it is about.
    pub call: Uuid,
    /// Where it stands.
    pub status: QueueStatus,
    /// When it was opened, or last reopened by a replaced reason, RFC 3339 UTC.
    pub opened_at: String,
    /// When it was resolved or dismissed.
    pub resolved_at: Option<String>,
    /// The operator's note.
    pub note: Option<String>,
    /// What fixed it.
    pub fixed_by: Option<String>,
    /// The rating as it stands now: `incorrect`, `partial`, or `correct`
    /// when the rater has since re-rated it so (an item with a note stays).
    pub score: String,
    /// The reason's category; `None` once re-rated Correct.
    pub category: Option<FeedbackCategory>,
    /// What is wrong, in the rater's words.
    pub wrong: Option<String>,
    /// The right answer, in the rater's words.
    pub right: Option<String>,
    /// A Discord user or a web session.
    pub rater_kind: RaterKind,
    /// The rater: a Discord user id, or `web:<session uuid>`.
    pub rater: String,
    /// The name a web rater gave.
    pub rater_name: Option<String>,
    /// Whether the rater held the judge role.
    pub judge: bool,
    /// The address a web rating came from, until the sweep clears it.
    pub ip: Option<String>,
    /// Its user agent, likewise.
    pub user_agent: Option<String>,
    /// When the rating was last written, RFC 3339 UTC.
    pub rated_at: String,
    /// The interface the call was asked through.
    pub interface: Asked,
    /// Whether the call was asked private.
    pub private: bool,
    /// The question, on one line and cut to 200 characters.
    pub question: String,
    /// Why the retirement pass retired the call, when it has.
    pub retired: Option<String>,
}

/// A page of the queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct QueuePage {
    /// Newest first (by when each was opened).
    pub items: Vec<QueueItem>,
    /// Pass back as `cursor` for the next page; `None` on the last.
    pub next: Option<String>,
}

/// One item with everything stored about its call.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct QueueDetail {
    /// The item.
    pub item: QueueItem,
    /// The call: question, answer, citations, cards, runs and models,
    /// retired state, every rating.
    pub call: CallDetail,
}

/// Items opened per category, by status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct QueueCount {
    /// The category; `None` for items whose rating was re-rated Correct.
    pub category: Option<FeedbackCategory>,
    /// Open.
    pub open: i64,
    /// Resolved.
    pub resolved: i64,
    /// Dismissed.
    pub dismissed: i64,
}

/// One row of the queue query, before it is read into a [`QueueItem`].
struct Row {
    at_micros: i64,
    id: Uuid,
    call_id: Uuid,
    status: String,
    opened_at: String,
    resolved_at: Option<String>,
    note: Option<String>,
    fixed_by: Option<String>,
    score: i16,
    category: Option<String>,
    what_wrong: Option<String>,
    right_answer: Option<String>,
    rater_kind: String,
    user_id: String,
    web_name: Option<String>,
    is_judge: bool,
    ip: Option<String>,
    user_agent: Option<String>,
    rated_at: String,
    interface: String,
    private: bool,
    question: String,
    retired_reason: Option<String>,
}

impl Row {
    fn item(self) -> Result<QueueItem, JudgeError> {
        Ok(QueueItem {
            id: self.id,
            call: self.call_id,
            status: stored(&self.status, "feedback_queue.status", QueueStatus::parse)?,
            opened_at: self.opened_at,
            resolved_at: self.resolved_at,
            note: self.note,
            fixed_by: self.fixed_by,
            score: score_id(self.score)?.to_owned(),
            category: self
                .category
                .as_deref()
                .map(|c| stored(c, "ratings.category", FeedbackCategory::parse))
                .transpose()?,
            wrong: self.what_wrong,
            right: self.right_answer,
            rater_kind: stored(&self.rater_kind, "ratings.rater_kind", RaterKind::parse)?,
            rater: self.user_id,
            rater_name: self.web_name,
            judge: self.is_judge,
            ip: self.ip,
            user_agent: self.user_agent,
            rated_at: self.rated_at,
            interface: stored(&self.interface, "calls.interface", Asked::parse)?,
            private: self.private,
            question: one_line(&self.question),
            retired: self.retired_reason,
        })
    }
}

fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(QUESTION_CHARS) {
        Some((cut, _)) => format!("{}…", flat.get(..cut).unwrap_or(&flat)),
        None => flat,
    }
}

/// The newest [`PAGE_SIZE`] items matching `filter` before `after` (from the
/// newest when `None`).
///
/// # Errors
/// `Upstream` from sqlx, or a row that does not read.
pub async fn list(
    pool: &PgPool,
    filter: QueueFilter,
    after: Option<Cursor>,
) -> Result<QueuePage, JudgeError> {
    let limit = i64::try_from(PAGE_SIZE)
        .unwrap_or(i64::MAX)
        .saturating_add(1);
    let rows = sqlx::query_as!(
        Row,
        r#"
        SELECT (extract(epoch FROM q.opened_at) * 1000000)::bigint AS "at_micros!",
               q.id, q.call_id, q.status,
               to_char(q.opened_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "opened_at!",
               to_char(q.resolved_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS resolved_at,
               q.note, q.fixed_by, r.score, r.category, r.what_wrong, r.right_answer,
               r.rater_kind, r.user_id, r.web_name, r.is_judge, host(r.ip) AS ip, r.user_agent,
               to_char(r.ts AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "rated_at!",
               c.interface, c.private, c.question, c.retired_reason
        FROM feedback_queue q
        JOIN ratings r ON r.call_id = q.call_id AND r.user_id = q.user_id
        JOIN calls c ON c.id = q.call_id
        WHERE ($1::text IS NULL OR q.status = $1)
          AND ($2::text IS NULL OR r.category = $2)
          AND ($3::text IS NULL OR c.interface = $3)
          AND ($4::text IS NULL OR r.rater_kind = $4)
          AND ($5::bigint IS NULL
               OR (q.opened_at, q.id) < ('epoch'::timestamptz + $5 * interval '1 microsecond', $6))
        ORDER BY q.opened_at DESC, q.id DESC
        LIMIT $7
        "#,
        filter.status.map(QueueStatus::id),
        filter.category.map(FeedbackCategory::id),
        filter.asked.map(Asked::id),
        filter.rater.map(RaterKind::id),
        after.map(Cursor::at_micros),
        after.map_or(Uuid::nil(), Cursor::id),
        limit,
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("read the feedback queue"))?;
    let mut rows = rows;
    let next = if rows.len() > PAGE_SIZE {
        rows.truncate(PAGE_SIZE);
        rows.last()
            .map(|r| Cursor::new(r.at_micros, r.id).to_string())
    } else {
        None
    };
    let items = rows
        .into_iter()
        .map(Row::item)
        .collect::<Result<Vec<_>, JudgeError>>()?;
    Ok(QueuePage { items, next })
}

/// Item `id`, `None` when there is none.
///
/// # Errors
/// `Upstream` from sqlx, or a row that does not read.
pub async fn item(pool: &PgPool, id: Uuid) -> Result<Option<QueueItem>, JudgeError> {
    sqlx::query_as!(
        Row,
        r#"
        SELECT (extract(epoch FROM q.opened_at) * 1000000)::bigint AS "at_micros!",
               q.id, q.call_id, q.status,
               to_char(q.opened_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "opened_at!",
               to_char(q.resolved_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS resolved_at,
               q.note, q.fixed_by, r.score, r.category, r.what_wrong, r.right_answer,
               r.rater_kind, r.user_id, r.web_name, r.is_judge, host(r.ip) AS ip, r.user_agent,
               to_char(r.ts AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "rated_at!",
               c.interface, c.private, c.question, c.retired_reason
        FROM feedback_queue q
        JOIN ratings r ON r.call_id = q.call_id AND r.user_id = q.user_id
        JOIN calls c ON c.id = q.call_id
        WHERE q.id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
    .map_err(upstream("read a queue item"))?
    .map(Row::item)
    .transpose()
}

/// Item `id` with everything stored about its call, and the names the
/// call's citation labels need; `None` when there is no such item.
///
/// # Errors
/// `Upstream` from sqlx, or a row that does not read.
pub async fn show(pool: &PgPool, id: Uuid) -> Result<Option<(QueueDetail, Names)>, JudgeError> {
    let Some(item) = item(pool, id).await? else {
        return Ok(None);
    };
    let Some((call, names)) = crate::admin::call(pool, judge_core::CallId::new(item.call)).await?
    else {
        // The call went (the retention) between the two reads, and the item
        // with it.
        return Ok(None);
    };
    Ok(Some((QueueDetail { item, call }, names)))
}

/// Close item `id` as `close` says: resolved or dismissed, with its note
/// and what fixed it, now. Closing a closed item again replaces all three.
/// `None` when there is no such item.
///
/// # Errors
/// `Upstream` from sqlx, or a row that does not read.
pub async fn close(
    pool: &PgPool,
    id: Uuid,
    close: &Close,
) -> Result<Option<QueueItem>, JudgeError> {
    let closed = sqlx::query_scalar!(
        r#"
        UPDATE feedback_queue
        SET status = $2, note = $3, fixed_by = $4, resolved_at = now()
        WHERE id = $1
        RETURNING id
        "#,
        id,
        close.closing.status().id(),
        AsRef::<str>::as_ref(&close.note),
        close.fixed_by.as_ref().map(AsRef::<str>::as_ref),
    )
    .fetch_optional(pool)
    .await
    .map_err(upstream("close a queue item"))?;
    match closed {
        Some(id) => item(pool, id).await,
        None => Ok(None),
    }
}

/// How many items there are per category and status, every category
/// listed (a zero row included) in [`FeedbackCategory::ALL`]'s order, then
/// the items re-rated Correct when there are any.
///
/// # Errors
/// `Upstream` from sqlx, or a row that does not read.
pub async fn counts(pool: &PgPool) -> Result<Vec<QueueCount>, JudgeError> {
    let rows = sqlx::query!(
        r#"
        SELECT r.category,
               count(*) FILTER (WHERE q.status = 'open') AS "open!",
               count(*) FILTER (WHERE q.status = 'resolved') AS "resolved!",
               count(*) FILTER (WHERE q.status = 'dismissed') AS "dismissed!"
        FROM feedback_queue q
        JOIN ratings r ON r.call_id = q.call_id AND r.user_id = q.user_id
        GROUP BY r.category
        "#
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("count the feedback queue"))?;
    let mut read = rows
        .into_iter()
        .map(|r| {
            Ok(QueueCount {
                category: r
                    .category
                    .as_deref()
                    .map(|c| stored(c, "ratings.category", FeedbackCategory::parse))
                    .transpose()?,
                open: r.open,
                resolved: r.resolved,
                dismissed: r.dismissed,
            })
        })
        .collect::<Result<Vec<_>, JudgeError>>()?;
    let mut out: Vec<QueueCount> = FeedbackCategory::ALL
        .into_iter()
        .map(|c| {
            read.iter()
                .find(|r| r.category == Some(c))
                .copied()
                .unwrap_or(QueueCount {
                    category: Some(c),
                    open: 0,
                    resolved: 0,
                    dismissed: 0,
                })
        })
        .collect();
    read.retain(|r| r.category.is_none());
    out.extend(read);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_defaults_to_90_and_refuses_out_of_range() {
        assert_eq!(IpDays::parse(None).map(IpDays::get), Ok(90));
        assert_eq!(IpDays::parse(Some("  ")).map(IpDays::get), Ok(90));
        assert_eq!(IpDays::parse(Some(" 30 ")).map(IpDays::get), Ok(30));
        assert_eq!(IpDays::parse(Some("3650")).map(IpDays::get), Ok(3650));
        for bad in ["0", "3651", "-1", "90d", "1.5"] {
            assert_eq!(IpDays::parse(Some(bad)), Err(bad.to_owned()), "{bad}");
        }
    }

    use std::num::NonZeroU64;

    use judge_core::{
        CallId, CallStore, Closing, Earlier, Fault, FeedbackText, FixedBy, QueueNote, Rated,
        RatedNew, Rater, RaterId, Rating, Reason, Score, UserAgent, WebName,
    };

    use crate::db::PgCallStore;

    /// A stored call asked through `interface`, `secs_ago` seconds ago.
    async fn call(pool: &PgPool, interface: &str, secs_ago: u32) -> anyhow::Result<CallId> {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO calls (thread_id, interface, private, question, answer, category, source, \
             confidence, citations, context_ids, cr_version, created_at) \
             VALUES ('t', $1, false, 'does trample\nstack?', 'No.', 'combat', 'cr', 'high', \
                     '[{\"kind\":\"rule\",\"id\":\"702.19b\",\"quote\":\"Trample.\"}]', '{}', \
                     '20260819', now() - make_interval(secs => $2)) RETURNING id",
        )
        .bind(interface)
        .bind(f64::from(secs_ago))
        .fetch_one(pool)
        .await?;
        Ok(CallId::new(id))
    }

    fn discord(id: u64) -> Rater {
        Rater::Discord {
            user: NonZeroU64::new(id).unwrap_or(NonZeroU64::MIN),
            judge: false,
        }
    }

    fn web(n: u128) -> Rater {
        Rater::Web {
            session: Uuid::from_u128(n),
            name: WebName::try_new("Jaya").ok(),
            ip: "203.0.113.7".parse().ok(),
            user_agent: UserAgent::keep("Mozilla/5.0"),
        }
    }

    fn faulted(fault: Fault, category: FeedbackCategory, wrong: &str) -> Rating {
        Rating::faulted(
            fault,
            Reason {
                category,
                wrong: FeedbackText::optional(wrong).ok().flatten(),
                right: None,
            },
        )
    }

    fn resolve(note: &str) -> anyhow::Result<Close> {
        Ok(Close {
            closing: Closing::Resolved,
            note: QueueNote::try_new(note)?,
            fixed_by: FixedBy::optional("notes.yaml: Dark Confidant")?,
        })
    }

    async fn only_item(pool: &PgPool) -> anyhow::Result<QueueItem> {
        let page = list(pool, QueueFilter::default(), None).await?;
        anyhow::ensure!(page.items.len() == 1, "{} items", page.items.len());
        page.items
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no item"))
    }

    /// A reason opens an item; the operator closes it; a replaced reason
    /// reopens it with the operator's note kept, and the rating holds only
    /// the new reason.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_reason_opens_an_item_and_a_replaced_one_reopens_it(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let store = PgCallStore::new(pool.clone());
        let c = call(&pool, "discord", 0).await?;
        let rated = store
            .rate(
                c,
                &discord(42),
                &faulted(
                    Fault::Incorrect,
                    FeedbackCategory::WrongCard,
                    " Bob, not Bob Jr. ",
                ),
            )
            .await?;
        assert_eq!(rated, Rated::Recorded);
        let item = only_item(&pool).await?;
        assert_eq!(item.status, QueueStatus::Open);
        assert_eq!(item.score, "incorrect");
        assert_eq!(item.category, Some(FeedbackCategory::WrongCard));
        assert_eq!(item.wrong.as_deref(), Some("Bob, not Bob Jr."));
        assert_eq!(
            (item.rater_kind, item.rater.as_str()),
            (RaterKind::Discord, "42")
        );
        assert_eq!(item.question, "does trample stack?");
        assert_eq!(item.interface, Asked::Discord);
        assert_eq!(
            store
                .earlier_rating(
                    c,
                    RaterId::Discord(NonZeroU64::new(42).unwrap_or(NonZeroU64::MIN))
                )
                .await?,
            Some(Earlier {
                score: Score::Incorrect,
                category: Some(FeedbackCategory::WrongCard)
            })
        );

        let closed = close(&pool, item.id, &resolve("added a note")?)
            .await?
            .ok_or_else(|| anyhow::anyhow!("closed nothing"))?;
        assert_eq!(closed.status, QueueStatus::Resolved);
        assert!(closed.resolved_at.is_some());
        assert_eq!(
            closed.fixed_by.as_deref(),
            Some("notes.yaml: Dark Confidant")
        );

        store
            .rate(
                c,
                &discord(42),
                &faulted(Fault::Partial, FeedbackCategory::MissedPart, ""),
            )
            .await?;
        let reopened = only_item(&pool).await?;
        assert_eq!(reopened.id, item.id, "the same item");
        assert_eq!(reopened.status, QueueStatus::Open);
        assert_eq!(reopened.resolved_at, None);
        assert_eq!(
            reopened.note.as_deref(),
            Some("added a note"),
            "the note is kept"
        );
        assert_eq!(reopened.score, "partial");
        assert_eq!(reopened.category, Some(FeedbackCategory::MissedPart));
        assert_eq!(reopened.wrong, None, "nothing of the old reason is left");
        assert_eq!(close(&pool, Uuid::nil(), &resolve("x")?).await?, None);
        Ok(())
    }

    /// `/forget` locks a user's ratings before their items, the order a
    /// rating takes them in. With a rating held mid-write (its row written,
    /// its item next), `/forget` waits on the rating row and holds no item,
    /// so the writer's item write goes through and neither is aborted as a
    /// deadlock. (Items first deadlocked here.)
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn forget_waits_for_a_rating_in_flight_without_deadlock(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let store = PgCallStore::new(pool.clone());
        let c = call(&pool, "discord", 0).await?;
        store
            .rate(
                c,
                &discord(42),
                &faulted(Fault::Incorrect, FeedbackCategory::Other, ""),
            )
            .await?;
        let mut writer = pool.begin().await?;
        sqlx::query("UPDATE ratings SET score = 2 WHERE user_id = '42'")
            .execute(&mut *writer)
            .await?;
        let forget = tokio::spawn({
            let store = store.clone();
            async move { store.forget_user("42").await }
        });
        // Until `/forget` is blocked on a lock (the rating row).
        let mut blocked = false;
        for _ in 0..500 {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(&pool)
            .await?;
            if waiting > 0 {
                blocked = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        anyhow::ensure!(blocked, "/forget never waited on the rating in flight");
        sqlx::query("UPDATE feedback_queue SET opened_at = now() WHERE user_id = '42'")
            .execute(&mut *writer)
            .await?;
        writer.commit().await?;
        let gone = forget.await??;
        assert_eq!((gone.ratings, gone.feedback), (1, 1));
        Ok(())
    }

    /// A first-only rating never replaces: the rater's earlier one stays
    /// and comes back.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_first_rating_only_never_replaces(pool: PgPool) -> anyhow::Result<()> {
        let store = PgCallStore::new(pool.clone());
        let c = call(&pool, "web", 0).await?;
        let wrong = faulted(Fault::Incorrect, FeedbackCategory::WrongCard, "");
        assert_eq!(
            store.rate_if_new(c, &web(1), &wrong).await?,
            RatedNew::Recorded
        );
        assert_eq!(
            store.rate_if_new(c, &web(1), &Rating::Correct).await?,
            RatedNew::Taken(Earlier {
                score: Score::Incorrect,
                category: Some(FeedbackCategory::WrongCard)
            })
        );
        let left: i16 = sqlx::query_scalar("SELECT score FROM ratings WHERE call_id = $1")
            .bind(c.into_inner())
            .fetch_one(&pool)
            .await?;
        assert_eq!(left, 1, "the earlier rating stays");
        assert_eq!(
            store
                .rate_if_new(CallId::new(Uuid::nil()), &web(1), &Rating::Correct)
                .await?,
            RatedNew::CallGone
        );
        Ok(())
    }

    /// Re-rating as Correct leaves an item the operator wrote on, and drops
    /// one nobody did.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn re_rating_correct_keeps_a_noted_item_and_drops_a_bare_one(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let store = PgCallStore::new(pool.clone());
        let c = call(&pool, "discord", 0).await?;
        let reason = faulted(Fault::Incorrect, FeedbackCategory::Outdated, "");
        store.rate(c, &discord(1), &reason).await?;
        store.rate(c, &discord(2), &reason).await?;
        let noted = list(&pool, QueueFilter::default(), None)
            .await?
            .items
            .into_iter()
            .find(|i| i.rater == "1")
            .ok_or_else(|| anyhow::anyhow!("rater 1's item"))?;
        close(&pool, noted.id, &resolve("was right after all")?).await?;
        store.rate(c, &discord(1), &Rating::Correct).await?;
        store.rate(c, &discord(2), &Rating::Correct).await?;
        let left = only_item(&pool).await?;
        assert_eq!(left.id, noted.id);
        assert_eq!(left.score, "correct");
        assert_eq!(left.category, None);
        assert_eq!(left.status, QueueStatus::Resolved);
        let reasons: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ratings WHERE category IS NOT NULL")
                .fetch_one(&pool)
                .await?;
        assert_eq!(reasons, 0, "a Correct keeps no reason");
        Ok(())
    }

    /// The prior-call view counts Discord ratings only: a web rating never
    /// moves a call's standing.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn calls_rated_ignores_web_ratings(pool: PgPool) -> anyhow::Result<()> {
        let store = PgCallStore::new(pool.clone());
        let c = call(&pool, "web", 0).await?;
        let standing = |pool: PgPool| async move {
            sqlx::query_as::<_, (i32, f32)>(
                "SELECT n, effective_score FROM calls_rated WHERE call_id = $1",
            )
            .bind(c.into_inner())
            .fetch_one(&pool)
            .await
        };
        let before = standing(pool.clone()).await?;
        for n in 1..=4 {
            store
                .rate(
                    c,
                    &web(n),
                    &faulted(Fault::Incorrect, FeedbackCategory::Other, ""),
                )
                .await?;
        }
        assert_eq!(
            standing(pool.clone()).await?,
            before,
            "web ratings moved it"
        );
        store.rate(c, &discord(9), &Rating::Correct).await?;
        let (n, _) = standing(pool.clone()).await?;
        assert_eq!(n, 1);
        // The web items are in the queue all the same.
        let web_items = list(
            &pool,
            QueueFilter {
                rater: Some(RaterKind::Web),
                ..QueueFilter::default()
            },
            None,
        )
        .await?;
        assert_eq!(web_items.items.len(), 4);
        let first = web_items
            .items
            .first()
            .ok_or_else(|| anyhow::anyhow!("item"))?;
        assert!(first.rater.starts_with("web:"), "{}", first.rater);
        assert_eq!(first.rater_name.as_deref(), Some("Jaya"));
        assert_eq!(first.ip.as_deref(), Some("203.0.113.7"));
        Ok(())
    }

    /// A web rating as the sweep leaves it: rater, address, user agent,
    /// category.
    type Kept = (String, Option<String>, Option<String>, Option<String>);

    /// The sweep clears an old web rating's address and user agent and
    /// keeps its reason; every rating written runs it.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_sweep_clears_old_addresses_and_keeps_the_feedback(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let days = IpDays::new(90).ok_or_else(|| anyhow::anyhow!("90"))?;
        let store = PgCallStore::new(pool.clone()).with_ip_days(days);
        let c = call(&pool, "web", 0).await?;
        let reason = faulted(Fault::Incorrect, FeedbackCategory::WrongConclusion, "no");
        store.rate(c, &web(1), &reason).await?;
        store.rate(c, &web(2), &reason).await?;
        sqlx::query("UPDATE ratings SET ts = now() - interval '91 days' WHERE user_id = $1")
            .bind(RaterId::Web(Uuid::from_u128(1)).stored())
            .execute(&pool)
            .await?;
        // A Discord rating on another call is a feedback write too.
        let other = call(&pool, "discord", 0).await?;
        store.rate(other, &discord(5), &Rating::Correct).await?;
        let rows: Vec<Kept> = sqlx::query_as(
            "SELECT user_id, host(ip), user_agent, category FROM ratings \
             WHERE rater_kind = 'web' ORDER BY ts",
        )
        .fetch_all(&pool)
        .await?;
        let category = Some("wrong_conclusion".to_owned());
        assert_eq!(
            rows,
            vec![
                (
                    RaterId::Web(Uuid::from_u128(1)).stored(),
                    None,
                    None,
                    category.clone()
                ),
                (
                    RaterId::Web(Uuid::from_u128(2)).stored(),
                    Some("203.0.113.7".to_owned()),
                    Some("Mozilla/5.0".to_owned()),
                    category
                ),
            ]
        );
        // The refresh's step clears what is old by then, nothing more.
        sqlx::query("UPDATE ratings SET ts = now() - interval '31 days'")
            .execute(&pool)
            .await?;
        let short = IpDays::new(30).ok_or_else(|| anyhow::anyhow!("30"))?;
        assert_eq!(sweep(&pool, short).await?, 1);
        assert_eq!(sweep(&pool, short).await?, 0);
        Ok(())
    }

    /// A rating from before reasons has none and no item; the schema
    /// refuses a Correct with a reason and a web field on a Discord rating.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_old_rating_has_no_reason_and_the_schema_holds_the_shape(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let store = PgCallStore::new(pool.clone());
        let c = call(&pool, "discord", 0).await?;
        sqlx::query(
            "INSERT INTO ratings (call_id, user_id, score, is_judge) VALUES ($1, '7', 1, false)",
        )
        .bind(c.into_inner())
        .execute(&pool)
        .await?;
        assert!(
            list(&pool, QueueFilter::default(), None)
                .await?
                .items
                .is_empty()
        );
        let seven = RaterId::Discord(NonZeroU64::new(7).unwrap_or(NonZeroU64::MIN));
        assert_eq!(
            store.earlier_rating(c, seven).await?,
            Some(Earlier {
                score: Score::Incorrect,
                category: None
            })
        );
        for bad in [
            "INSERT INTO ratings (call_id, user_id, score, category) VALUES ($1, '8', 3, 'other')",
            "INSERT INTO ratings (call_id, user_id, score, what_wrong) VALUES ($1, '8', 1, 'x')",
            "INSERT INTO ratings (call_id, user_id, score, ip) VALUES ($1, '8', 1, '10.0.0.1')",
            "INSERT INTO ratings (call_id, user_id, score) VALUES ($1, 'web:x', 1)",
            "INSERT INTO ratings (call_id, user_id, score, rater_kind, is_judge) \
             VALUES ($1, 'web:x', 1, 'web', true)",
        ] {
            let refused = sqlx::query(bad).bind(c.into_inner()).execute(&pool).await;
            assert!(refused.is_err(), "{bad}");
        }
        Ok(())
    }

    /// The queue lists newest first, every filter narrows, a page ends with
    /// a cursor, an item shows with its call's material, and the counts
    /// cover every category.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_queue_filters_pages_shows_and_counts(pool: PgPool) -> anyhow::Result<()> {
        let store = PgCallStore::new(pool.clone());
        let on_discord = call(&pool, "discord", 60).await?;
        let on_web = call(&pool, "web", 30).await?;
        store
            .rate(
                on_discord,
                &discord(1),
                &faulted(Fault::Incorrect, FeedbackCategory::WrongCard, ""),
            )
            .await?;
        store
            .rate(
                on_web,
                &web(1),
                &faulted(Fault::Partial, FeedbackCategory::BadCitation, ""),
            )
            .await?;
        let all = list(&pool, QueueFilter::default(), None).await?;
        let calls: Vec<Uuid> = all.items.iter().map(|i| i.call).collect();
        assert_eq!(
            calls,
            [on_web.into_inner(), on_discord.into_inner()],
            "newest first"
        );
        assert_eq!(all.next, None);
        let only = |f: QueueFilter| {
            let pool = pool.clone();
            async move {
                Ok::<_, anyhow::Error>(
                    list(&pool, f, None)
                        .await?
                        .items
                        .iter()
                        .map(|i| i.call)
                        .collect::<Vec<_>>(),
                )
            }
        };
        for f in [
            QueueFilter {
                category: Some(FeedbackCategory::WrongCard),
                ..QueueFilter::default()
            },
            QueueFilter {
                asked: Some(Asked::Discord),
                ..QueueFilter::default()
            },
            QueueFilter {
                rater: Some(RaterKind::Discord),
                ..QueueFilter::default()
            },
        ] {
            assert_eq!(only(f).await?, [on_discord.into_inner()], "{f:?}");
        }
        assert!(
            only(QueueFilter {
                status: Some(QueueStatus::Dismissed),
                ..QueueFilter::default()
            })
            .await?
            .is_empty()
        );
        let first = all.items.first().ok_or_else(|| anyhow::anyhow!("item"))?;
        let dismissed = close(
            &pool,
            first.id,
            &Close {
                closing: Closing::Dismissed,
                note: QueueNote::try_new("spam")?,
                fixed_by: None,
            },
        )
        .await?;
        assert_eq!(dismissed.map(|i| i.status), Some(QueueStatus::Dismissed));

        let (detail, _) = show(&pool, first.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no detail"))?;
        assert_eq!(detail.call.answer, "No.");
        assert_eq!(detail.call.citations.len(), 1);
        let rating = detail
            .call
            .ratings
            .first()
            .ok_or_else(|| anyhow::anyhow!("rating"))?;
        assert_eq!(rating.kind, RaterKind::Web);
        assert_eq!(rating.category, Some(FeedbackCategory::BadCitation));
        assert!(show(&pool, Uuid::nil()).await?.is_none());

        let counted = counts(&pool).await?;
        assert_eq!(counted.len(), FeedbackCategory::ALL.len());
        let of = |c| counted.iter().find(|n| n.category == Some(c)).copied();
        assert_eq!(of(FeedbackCategory::WrongCard).map(|n| n.open), Some(1));
        assert_eq!(
            of(FeedbackCategory::BadCitation).map(|n| (n.open, n.dismissed)),
            Some((0, 1))
        );

        // Past a page: the cursor picks up where it ended.
        for n in 0..u64::try_from(PAGE_SIZE)? {
            store
                .rate(
                    on_discord,
                    &discord(100 + n),
                    &faulted(Fault::Partial, FeedbackCategory::Other, ""),
                )
                .await?;
        }
        let page = list(&pool, QueueFilter::default(), None).await?;
        assert_eq!(page.items.len(), PAGE_SIZE);
        let cursor: Cursor = page
            .next
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no cursor"))?
            .parse()
            .map_err(|()| anyhow::anyhow!("cursor"))?;
        let rest = list(&pool, QueueFilter::default(), Some(cursor)).await?;
        assert_eq!(rest.items.len(), 2);
        assert_eq!(rest.next, None);
        Ok(())
    }

    #[test]
    fn a_question_is_one_line_and_cut() {
        assert_eq!(one_line("a\n  b"), "a b");
        let long = "x".repeat(QUESTION_CHARS + 5);
        assert_eq!(one_line(&long).chars().count(), QUESTION_CHARS + 1);
    }
}
