//! The public feed (#26): every answered call not asked private, newest
//! first, from every interface, and one call by id. The web role serves it
//! (`GET /api/feed`, `GET /api/calls/{id}`); this module is the settings and
//! the reads.
//!
//! What a public read may return is decided here, in SQL, and nowhere else:
//! a private call is never read and a call older than the feed window
//! (`JUDGE_FEED_DAYS`) is not read either (`publicly_visible`, a SQL function
//! the migrations define once and the public dashboard's counts use too, is
//! in every query's `WHERE`, and a test asks each read for one), and a
//! failed call is in another table. A missing call, a private one and
//! one outside the window are the same `None`. The asker's Discord id is
//! never selected; only the username snapshotted at asking time is, and
//! `/forget` has already cleared it on the forgotten user's calls.

use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use judge_core::{
    CallId, CardId, CardRef, CardStatus, Citation, Confidence, FeedbackCategory, Interface,
    JudgeError, RaterKind, RulingKey, Score, Source,
};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::{bad_row, bad_row_from, upstream};
use crate::ingest::prune::{Days, MAX_RETENTION_DAYS};

/// `JUDGE_FEED`: `false` turns the feed off. On by default.
pub const FEED_ENV: &str = "JUDGE_FEED";
/// `JUDGE_FEED_DAYS`: hide calls older than this many days from every
/// public read. Unset or `0`: no limit.
pub const FEED_DAYS_ENV: &str = "JUDGE_FEED_DAYS";
/// Calls per page of the feed.
pub const PAGE_SIZE: usize = 20;

/// Whether the feed is served, and how far back it reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    /// `JUDGE_FEED=false`: no page and no endpoints.
    Off,
    /// Served, showing calls younger than `window` (every call when `None`).
    On {
        /// `JUDGE_FEED_DAYS`.
        window: Option<Days>,
    },
}

impl Default for Feed {
    fn default() -> Self {
        Self::On { window: None }
    }
}

/// A `JUDGE_FEED` or `JUDGE_FEED_DAYS` value the feed cannot take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BadFeed {
    /// `JUDGE_FEED` is not a boolean.
    Flag(String),
    /// `JUDGE_FEED_DAYS` is not a whole number of days in range.
    Days(String),
}

impl Feed {
    /// Parse `JUDGE_FEED` (blank: on) and `JUDGE_FEED_DAYS` (blank or `0`:
    /// no window). Both are checked even when the feed is off, so a typo is
    /// found before the feed is turned on.
    ///
    /// # Errors
    /// The first value that does not parse.
    pub fn parse(flag: Option<&str>, days: Option<&str>) -> Result<Self, BadFeed> {
        let on = match flag.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            None | Some("" | "true" | "1" | "yes" | "on") => true,
            Some("false" | "0" | "no" | "off") => false,
            Some(_) => return Err(BadFeed::Flag(flag.unwrap_or_default().to_owned())),
        };
        let window = match days.map(str::trim).filter(|v| !v.is_empty()) {
            None => None,
            Some(v) => match v.parse::<u16>() {
                Ok(0) => None,
                Ok(n) => Some(Days::new(n).ok_or_else(|| BadFeed::Days(v.to_owned()))?),
                Err(_) => return Err(BadFeed::Days(v.to_owned())),
            },
        };
        Ok(if on { Self::On { window } } else { Self::Off })
    }

    /// The longest `JUDGE_FEED_DAYS` takes, as the call retention's.
    pub const MAX_DAYS: u16 = MAX_RETENTION_DAYS;

    /// Whether the feed is served.
    #[must_use]
    pub const fn is_on(self) -> bool {
        matches!(self, Self::On { .. })
    }
}

/// The interface a call came in through, as the feed labels it: the
/// [`Interface`]s, and the one value stored before 2.0.0 told them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Asked {
    /// `/judge` on Discord.
    Discord,
    /// The web app or `POST /api/judge`.
    Web,
    /// The MCP `judge` tool.
    Mcp,
    /// `judge-cli judge`.
    Cli,
    /// An agent-driven session.
    AgentSession,
    /// Before 2.0.0: the MCP `judge` tool or `judge-cli judge`.
    Agent,
}

impl Asked {
    /// Every value, in declaration order.
    pub const ALL: [Self; 6] = [
        Self::Discord,
        Self::Web,
        Self::Mcp,
        Self::Cli,
        Self::AgentSession,
        Self::Agent,
    ];

    /// The stored id (`calls.interface`).
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Discord => Interface::Discord.id(),
            Self::Web => Interface::Web.id(),
            Self::Mcp => Interface::Mcp.id(),
            Self::Cli => Interface::Cli.id(),
            Self::AgentSession => Interface::AgentSession.id(),
            Self::Agent => "agent",
        }
    }

    /// The value whose id is `id`.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == id)
    }
}

impl From<Interface> for Asked {
    fn from(i: Interface) -> Self {
        match i {
            Interface::Discord => Self::Discord,
            Interface::Web => Self::Web,
            Interface::Mcp => Self::Mcp,
            Interface::Cli => Self::Cli,
            Interface::AgentSession => Self::AgentSession,
        }
    }
}

/// A Discord mention as a message stores it: `<@123>`, `<@!123>` (a
/// user), `<@&123>` (a role), `<#123>` (a channel). Discord writes a typed
/// `@name` into a command's text as one, so a stored question can hold
/// another member's id or a channel's.
static MENTION: LazyLock<Regex> = LazyLock::new(|| {
    #[expect(
        clippy::expect_used,
        reason = "the pattern is a constant: an invalid one panics in every test that uses it"
    )]
    Regex::new(r"<(@[!&]?|#)[0-9]+>").expect("MENTION is a valid regex")
});

/// `text` with every Discord mention reduced to its sigil (`@…`, `#…`):
/// the feed shows no Discord id, a member's, a role's or a channel's.
#[must_use]
pub fn without_mentions(text: &str) -> String {
    MENTION
        .replace_all(text, |c: &regex::Captures<'_>| {
            if c.get(1).is_some_and(|s| s.as_str() == "#") {
                "#…"
            } else {
                "@…"
            }
        })
        .into_owned()
}

/// Above any cursor: 2^53 microseconds, where float8 stops being exact.
const MAX_CURSOR_MICROS: i64 = 1 << 53;

/// Where the next page starts: the last entry's time and id. Written as
/// `<microseconds since the epoch>.<id as 32 hex digits>`, which a client
/// passes back unread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    at_micros: i64,
    id: Uuid,
}

impl Cursor {
    /// The cursor after the row asked at `at_micros` with id `id`.
    pub(crate) const fn new(at_micros: i64, id: Uuid) -> Self {
        Self { at_micros, id }
    }

    /// Its time, in microseconds since the epoch.
    pub(crate) const fn at_micros(self) -> i64 {
        self.at_micros
    }

    /// Its call's id.
    pub(crate) const fn id(self) -> Uuid {
        self.id
    }
}

impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.at_micros, self.id.simple())
    }
}

impl std::str::FromStr for Cursor {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        let (at, id) = s.split_once('.').ok_or(())?;
        if at.is_empty() || !at.bytes().all(|b| b.is_ascii_digit()) || id.len() != 32 {
            return Err(());
        }
        let at_micros: i64 = at.parse().map_err(|_| ())?;
        // Exact as the float8 the SQL multiplies through, and far past any
        // call (the year 2255); larger is no cursor the feed wrote.
        if at_micros >= MAX_CURSOR_MICROS {
            return Err(());
        }
        Ok(Self {
            at_micros,
            id: Uuid::try_parse(id).map_err(|_| ())?,
        })
    }
}

/// Which calls a page holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// Only calls asked through this interface.
    pub asked: Option<Asked>,
    /// Only calls asked by this Discord username.
    pub user: Option<judge_core::DiscordUsername>,
    /// Only calls whose question, answer or asker's username contains this.
    pub search: Option<Search>,
}

/// The longest search the feed takes, in characters.
pub const MAX_SEARCH_CHARS: usize = 100;

/// Text a feed search looks for: trimmed, 1 to [`MAX_SEARCH_CHARS`]
/// characters, matched case-insensitively as a substring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Search(String);

impl Search {
    /// `text` as a search, or `None` when it is blank, too long, or holds a
    /// NUL (which Postgres text cannot).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty() && text.chars().count() <= MAX_SEARCH_CHARS && !text.contains('\0'))
            .then(|| Self(text.to_owned()))
    }

    /// The `ILIKE` pattern: the text with `\`, `%` and `_` escaped (the
    /// default escape character), between two `%`.
    #[must_use]
    pub fn pattern(&self) -> String {
        let mut out = String::with_capacity(self.0.len() + 2);
        out.push('%');
        for c in self.0.chars() {
            if matches!(c, '\\' | '%' | '_') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('%');
        out
    }
}

/// How a call has been rated: counts per kind of rater, and the judge
/// override when a member with the judge role rated (the latest such
/// rating). Who rated is never shown. Discord's and the web's are apart: only
/// Discord's count toward the examples (D10), and a page shows the web's
/// only when its reader asks to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Ratings {
    /// Discord ratings.
    pub discord: Counts,
    /// Ratings from the web app's anonymous sessions.
    pub web: Counts,
    /// The judge override's score, when there is one.
    #[serde(serialize_with = "score_name")]
    pub judge: Option<Score>,
}

/// Ratings by score.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Rated correct.
    pub correct: u32,
    /// Rated partially correct.
    pub partial: u32,
    /// Rated incorrect.
    pub incorrect: u32,
}

/// One rater's reason, as the public may read it when the operator sets
/// `JUDGE_PUBLIC_FEEDBACK=true`: what kind of rater (so a reader who hides
/// web ratings hides their reasons too), the score and the reason, and
/// nothing that names the rater: no user id, no name, no address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PublicReason {
    /// Discord or the web.
    pub rater_kind: RaterKind,
    /// `partial` or `incorrect`.
    pub score: &'static str,
    /// What kind of problem.
    pub category: FeedbackCategory,
    /// What is wrong, in the rater's words (mentions reduced as the feed's).
    pub wrong: Option<String>,
    /// The right answer, likewise.
    pub right: Option<String>,
}

/// A reason as the query reads it.
#[derive(Deserialize)]
struct StoredReason {
    kind: String,
    score: i16,
    category: String,
    wrong: Option<String>,
    right: Option<String>,
}

impl StoredReason {
    fn public(self) -> Option<PublicReason> {
        Some(PublicReason {
            rater_kind: RaterKind::parse(&self.kind)?,
            score: match self.score {
                1 => "incorrect",
                2 => "partial",
                _ => return None,
            },
            category: FeedbackCategory::parse(&self.category)?,
            wrong: self.wrong.as_deref().map(public_text),
            right: self.right.as_deref().map(public_text),
        })
    }
}

/// A rater's words as the public reads them: user, role and channel
/// mentions reduced to `@…` and `#…` ([`without_mentions`]), and `@everyone`
/// and `@here` broken with a zero-width space so no client that renders
/// them can ping. Nothing else is changed.
fn public_text(text: &str) -> String {
    without_mentions(text)
        .replace("@everyone", "@\u{200b}everyone")
        .replace("@here", "@\u{200b}here")
}

/// Whether the public reads carry raters' reasons
/// (`JUDGE_PUBLIC_FEEDBACK`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reasons {
    /// Admin-only: the default.
    #[default]
    Hidden,
    /// Shown, without the raters' identity.
    Shown,
}

/// `JUDGE_PUBLIC_FEEDBACK`: `true` shows raters' reasons on the public feed.
pub const PUBLIC_FEEDBACK_ENV: &str = "JUDGE_PUBLIC_FEEDBACK";

impl Reasons {
    /// Parse `JUDGE_PUBLIC_FEEDBACK` (blank: hidden).
    ///
    /// # Errors
    /// The value, when it is not a boolean.
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            None | Some("" | "false" | "0" | "no" | "off") => Ok(Self::Hidden),
            Some("true" | "1" | "yes" | "on") => Ok(Self::Shown),
            Some(_) => Err(raw.unwrap_or_default().to_owned()),
        }
    }

    const fn shown(self) -> bool {
        matches!(self, Self::Shown)
    }
}

#[expect(
    clippy::ref_option,
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's serialize_with hands the field by reference"
)]
fn score_name<S: serde::Serializer>(s: &Option<Score>, out: S) -> Result<S::Ok, S::Error> {
    match s {
        None => out.serialize_none(),
        Some(Score::Correct) => out.serialize_str("correct"),
        Some(Score::Partial) => out.serialize_str("partial"),
        Some(Score::Incorrect) => out.serialize_str("incorrect"),
    }
}

/// One public call.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The call.
    pub id: CallId,
    /// When it was asked, RFC 3339 UTC.
    pub asked_at: String,
    /// Through which interface.
    pub asked: Asked,
    /// The Discord username of the asker; `None` is anonymous (not asked on
    /// Discord, asked before names were kept, or forgotten).
    pub asker: Option<String>,
    /// The question as asked.
    pub question: String,
    /// The answer.
    pub answer: String,
    /// The model's confidence.
    pub confidence: Confidence,
    /// The rules body it was answered under.
    pub source: Source,
    /// The CR it was answered under, `YYYYMMDD`.
    pub cr_version: String,
    /// Its citations, in the answer's order. One stored in a shape that no
    /// longer reads is left out (its call is retired, by the same rule).
    pub citations: Vec<Citation>,
    /// The cards the question was taken to be about.
    pub cards: Vec<CardRef>,
    /// Its ratings.
    pub ratings: Ratings,
    /// Raters' reasons, oldest first, when `JUDGE_PUBLIC_FEEDBACK` shows
    /// them; `None` when it does not.
    pub reasons: Option<Vec<PublicReason>>,
    /// Why the retirement pass took it out of the examples, when it has.
    pub retired: Option<String>,
}

/// Card names and ruling dates the entries' citations name, read with them
/// so a label says which card and which ruling.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    cards: HashMap<CardId, String>,
    rulings: HashMap<(CardId, RulingKey), String>,
}

impl Names {
    /// A card's current name.
    #[must_use]
    pub fn card(&self, id: CardId) -> Option<&str> {
        self.cards.get(&id).map(String::as_str)
    }

    /// A ruling's date, `YYYY-MM-DD`.
    #[must_use]
    pub fn ruling(&self, card: CardId, key: &RulingKey) -> Option<&str> {
        self.rulings.get(&(card, *key)).map(String::as_str)
    }
}

/// A page of the feed.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    /// Newest first.
    pub entries: Vec<Entry>,
    /// Where the next page starts; `None` on the last.
    pub next: Option<Cursor>,
    /// What the citations name.
    pub names: Names,
}

/// One call as the queries read it.
struct Row {
    id: Uuid,
    at_micros: i64,
    asked_at: String,
    interface: String,
    asker_name: Option<String>,
    question: String,
    answer: String,
    confidence: String,
    source: String,
    cr_version: String,
    citations: serde_json::Value,
    cards: serde_json::Value,
    /// The cards the reply noted as unreleased (D42).
    unreleased_cards: Vec<Uuid>,
    d_correct: i64,
    d_partial: i64,
    d_incorrect: i64,
    w_correct: i64,
    w_partial: i64,
    w_incorrect: i64,
    judge: Option<i16>,
    reasons: Option<serde_json::Value>,
    retired_reason: Option<String>,
}

/// The window as the queries take it: days, or `NULL` for none.
pub(crate) fn window_days(window: Option<Days>) -> Option<i32> {
    window.map(|d| i32::from(d.get()))
}

/// The newest [`PAGE_SIZE`] public calls matching `filter` before `after`
/// (from the newest when `None`), within `window`. A call holding a value
/// this build does not read is left out, with a WARN, so a page can hold
/// fewer than [`PAGE_SIZE`] and still have a next one.
///
/// # Errors
/// `Upstream` from sqlx.
pub async fn page(
    pool: &PgPool,
    window: Option<Days>,
    reasons: Reasons,
    filter: &Filter,
    after: Option<Cursor>,
) -> Result<Page, JudgeError> {
    let limit = i64::try_from(PAGE_SIZE)
        .unwrap_or(i64::MAX)
        .saturating_add(1);
    let mut rows = sqlx::query_as!(
        Row,
        r#"
        SELECT c.id,
               (extract(epoch FROM c.created_at) * 1000000)::bigint AS "at_micros!",
               to_char(c.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "asked_at!",
               c.interface, c.asker_name, c.question, c.answer, c.confidence, c.source,
               c.cr_version, c.citations, coalesce(c.context_ids->'cards', '[]') AS "cards!",
               c.unreleased_cards,
               rt.d_correct AS "d_correct!", rt.d_partial AS "d_partial!",
               rt.d_incorrect AS "d_incorrect!", rt.w_correct AS "w_correct!",
               rt.w_partial AS "w_partial!", rt.w_incorrect AS "w_incorrect!",
               rt.judge, rt.reasons, c.retired_reason
        FROM calls c
        CROSS JOIN LATERAL (
            SELECT count(*) FILTER (WHERE r.score = 3 AND r.rater_kind = 'discord') AS d_correct,
                   count(*) FILTER (WHERE r.score = 2 AND r.rater_kind = 'discord') AS d_partial,
                   count(*) FILTER (WHERE r.score = 1 AND r.rater_kind = 'discord') AS d_incorrect,
                   count(*) FILTER (WHERE r.score = 3 AND r.rater_kind = 'web') AS w_correct,
                   count(*) FILTER (WHERE r.score = 2 AND r.rater_kind = 'web') AS w_partial,
                   count(*) FILTER (WHERE r.score = 1 AND r.rater_kind = 'web') AS w_incorrect,
                   (array_agg(r.score ORDER BY r.ts DESC)
                        FILTER (WHERE r.is_judge AND r.rater_kind = 'discord'))[1] AS judge,
                   -- Reasons only when the operator shows them, and never
                   -- who gave them: no user id, name, address or agent.
                   CASE WHEN $8::boolean THEN coalesce(
                       jsonb_agg(jsonb_build_object(
                           'kind', r.rater_kind, 'score', r.score, 'category', r.category,
                           'wrong', r.what_wrong, 'right', r.right_answer) ORDER BY r.ts)
                           -- A reason the operator dismissed is not shown.
                           FILTER (WHERE r.category IS NOT NULL
                                     AND q.status IS DISTINCT FROM 'dismissed'),
                       '[]'::jsonb) END AS reasons
            FROM ratings r
            LEFT JOIN feedback_queue q ON q.call_id = r.call_id AND q.user_id = r.user_id
            WHERE r.call_id = c.id
        ) rt
        WHERE publicly_visible(c.private, c.created_at, $1)
          AND ($2::text IS NULL OR c.interface = $2)
          -- A username is reassigned when its holder changes it, and each
          -- call keeps the name it was asked under: the name's calls are
          -- those of whoever asked under it last, as far as the public can
          -- see, so an earlier holder's are not listed as the new one's.
          AND ($3::text IS NULL OR (c.asker_name = $3 AND c.asker_id = (
                SELECT h.asker_id FROM calls h
                WHERE h.asker_name = $3 AND publicly_visible(h.private, h.created_at, $1)
                ORDER BY h.created_at DESC, h.id DESC LIMIT 1)))
          -- The text exactly as the feed shows it (`without_mentions`: each
          -- mention as `@…` or `#…`), so no part of a mention's id is ever
          -- searched and nobody can find who was mentioned by trying ids.
          AND ($7::text IS NULL
               OR regexp_replace(c.question, '<(@)[!&]?[0-9]+>|<(#)[0-9]+>', '\1\2…', 'g') ILIKE $7
               OR regexp_replace(c.answer, '<(@)[!&]?[0-9]+>|<(#)[0-9]+>', '\1\2…', 'g') ILIKE $7
               OR c.asker_name ILIKE $7)
          AND ($4::bigint IS NULL
               OR (c.created_at, c.id) < ('epoch'::timestamptz + $4 * interval '1 microsecond', $5))
        ORDER BY c.created_at DESC, c.id DESC
        LIMIT $6
        "#,
        window_days(window),
        filter.asked.map(Asked::id),
        filter.user.as_ref().map(AsRef::<str>::as_ref),
        after.map(|c| c.at_micros),
        after.map_or(Uuid::nil(), |c| c.id),
        limit,
        filter.search.as_ref().map(Search::pattern),
        reasons.shown(),
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("read the feed"))?;
    let next = if rows.len() > PAGE_SIZE {
        rows.truncate(PAGE_SIZE);
        rows.last().map(|r| Cursor {
            at_micros: r.at_micros,
            id: r.id,
        })
    } else {
        None
    };
    // The cursor above is the last row read, so a row left out here still
    // moves the next page past it.
    let entries: Vec<Entry> = rows.into_iter().filter_map(readable).collect();
    let names = names(pool, &entries).await?;
    let entries = entries.into_iter().map(|e| named(e, &names)).collect();
    Ok(Page {
        entries,
        next,
        names,
    })
}

/// The public call `id` within `window`; `None` for a call that is missing,
/// private or older than the window alike, and for one holding a value this
/// build does not read (with a WARN).
///
/// # Errors
/// `Upstream` from sqlx.
pub async fn call(
    pool: &PgPool,
    window: Option<Days>,
    reasons: Reasons,
    id: CallId,
) -> Result<Option<(Entry, Names)>, JudgeError> {
    let Some(entry) = read_call(pool, window, reasons, id).await? else {
        return Ok(None);
    };
    let names = names(pool, std::slice::from_ref(&entry)).await?;
    Ok(Some((named(entry, &names), names)))
}

/// [`call`]'s entry before its names are read.
async fn read_call(
    pool: &PgPool,
    window: Option<Days>,
    reasons: Reasons,
    id: CallId,
) -> Result<Option<Entry>, JudgeError> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT c.id,
               (extract(epoch FROM c.created_at) * 1000000)::bigint AS "at_micros!",
               to_char(c.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "asked_at!",
               c.interface, c.asker_name, c.question, c.answer, c.confidence, c.source,
               c.cr_version, c.citations, coalesce(c.context_ids->'cards', '[]') AS "cards!",
               c.unreleased_cards,
               rt.d_correct AS "d_correct!", rt.d_partial AS "d_partial!",
               rt.d_incorrect AS "d_incorrect!", rt.w_correct AS "w_correct!",
               rt.w_partial AS "w_partial!", rt.w_incorrect AS "w_incorrect!",
               rt.judge, rt.reasons, c.retired_reason
        FROM calls c
        CROSS JOIN LATERAL (
            SELECT count(*) FILTER (WHERE r.score = 3 AND r.rater_kind = 'discord') AS d_correct,
                   count(*) FILTER (WHERE r.score = 2 AND r.rater_kind = 'discord') AS d_partial,
                   count(*) FILTER (WHERE r.score = 1 AND r.rater_kind = 'discord') AS d_incorrect,
                   count(*) FILTER (WHERE r.score = 3 AND r.rater_kind = 'web') AS w_correct,
                   count(*) FILTER (WHERE r.score = 2 AND r.rater_kind = 'web') AS w_partial,
                   count(*) FILTER (WHERE r.score = 1 AND r.rater_kind = 'web') AS w_incorrect,
                   (array_agg(r.score ORDER BY r.ts DESC)
                        FILTER (WHERE r.is_judge AND r.rater_kind = 'discord'))[1] AS judge,
                   -- Reasons only when the operator shows them, and never
                   -- who gave them: no user id, name, address or agent.
                   CASE WHEN $3::boolean THEN coalesce(
                       jsonb_agg(jsonb_build_object(
                           'kind', r.rater_kind, 'score', r.score, 'category', r.category,
                           'wrong', r.what_wrong, 'right', r.right_answer) ORDER BY r.ts)
                           -- A reason the operator dismissed is not shown.
                           FILTER (WHERE r.category IS NOT NULL
                                     AND q.status IS DISTINCT FROM 'dismissed'),
                       '[]'::jsonb) END AS reasons
            FROM ratings r
            LEFT JOIN feedback_queue q ON q.call_id = r.call_id AND q.user_id = r.user_id
            WHERE r.call_id = c.id
        ) rt
        WHERE publicly_visible(c.private, c.created_at, $1)
          AND c.id = $2
        "#,
        window_days(window),
        id.into_inner(),
        reasons.shown(),
    )
    .fetch_optional(pool)
    .await
    .map_err(upstream("read a public call"))?;
    Ok(row.and_then(readable))
}

/// Whether the public may rate call `id` from the web: exactly when
/// [`call`] shows it (it exists, was not asked private, is within `window`
/// and reads in this build). A call the feed cannot show cannot be rated, so
/// a rating never lands on a call nobody can see.
///
/// # Errors
/// `Upstream` from sqlx.
pub async fn rateable(pool: &PgPool, window: Option<Days>, id: CallId) -> Result<bool, JudgeError> {
    Ok(read_call(pool, window, Reasons::Hidden, id)
        .await?
        .is_some())
}

/// A unit-variant enum from its stored serde name.
pub(crate) fn stored<T: serde::de::DeserializeOwned>(
    value: &str,
    what: &str,
) -> Result<T, JudgeError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|e| bad_row_from(e, format!("calls.{what} {value:?}")))
}

fn count(n: i64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `r` as an entry, or `None` (and a WARN naming the call) when a value in it
/// is one this build does not know, such as an interface, confidence or
/// source a newer build of the same major wrote. One such call is left out
/// of the feed rather than failing the page it is on, and its own page is
/// the `None` of a call the feed does not show.
fn readable(r: Row) -> Option<Entry> {
    let id = r.id;
    entry(r)
        .inspect_err(|e| {
            if crate::unread::first(crate::unread::Site::Feed, id) {
                tracing::warn!(call = %id, error = %format_args!("{e:#}"), "a public call does not read; left out of the feed (logged once)");
            }
        })
        .ok()
}

/// A stored call's card status: `Unreleased` for the cards its reply noted
/// (D42), else `Released`. A stored call keeps only the note, so a card that
/// was previewed and covered reads as released.
pub(crate) fn stored_status(id: CardId, unreleased: &[Uuid]) -> CardStatus {
    if unreleased.contains(&id.into_inner()) {
        CardStatus::Unreleased
    } else {
        CardStatus::Released
    }
}

fn entry(r: Row) -> Result<Entry, JudgeError> {
    let asked = Asked::parse(&r.interface)
        .ok_or_else(|| bad_row(format!("calls.interface {:?}", r.interface)))?;
    // Each citation alone: a shape from an older release that no longer
    // reads is dropped rather than failing the page.
    let citations = match r.citations {
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter_map(|c| serde_json::from_value::<Citation>(c).ok())
            .collect(),
        _ => vec![],
    };
    let cards = match r.cards {
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter_map(|c| serde_json::from_value::<CardId>(c).ok())
            .map(|id| CardRef {
                id,
                name: String::new(),
                status: stored_status(id, &r.unreleased_cards),
            })
            .collect(),
        _ => vec![],
    };
    let judge = match r.judge {
        None => None,
        Some(1) => Some(Score::Incorrect),
        Some(2) => Some(Score::Partial),
        Some(3) => Some(Score::Correct),
        Some(n) => return Err(bad_row(format!("ratings.score {n}"))),
    };
    Ok(Entry {
        id: CallId::new(r.id),
        asked_at: r.asked_at,
        asked,
        asker: r.asker_name,
        question: without_mentions(&r.question),
        answer: without_mentions(&r.answer),
        confidence: stored(&r.confidence, "confidence")?,
        source: stored(&r.source, "source")?,
        cr_version: r.cr_version,
        citations,
        cards,
        ratings: Ratings {
            discord: Counts {
                correct: count(r.d_correct),
                partial: count(r.d_partial),
                incorrect: count(r.d_incorrect),
            },
            web: Counts {
                correct: count(r.w_correct),
                partial: count(r.w_partial),
                incorrect: count(r.w_incorrect),
            },
            judge,
        },
        // A reason in a shape this build does not read is left out.
        reasons: r.reasons.map(|v| match v {
            serde_json::Value::Array(items) => items
                .into_iter()
                .filter_map(|i| serde_json::from_value::<StoredReason>(i).ok())
                .filter_map(StoredReason::public)
                .collect(),
            _ => vec![],
        }),
        retired: r.retired_reason,
    })
}

/// Read the card names and ruling dates `entries` need.
async fn names(pool: &PgPool, entries: &[Entry]) -> Result<Names, JudgeError> {
    let cards: Vec<CardId> = entries
        .iter()
        .flat_map(|e| e.cards.iter().map(|c| c.id))
        .collect();
    let citations: Vec<&Citation> = entries.iter().flat_map(|e| e.citations.iter()).collect();
    names_for(pool, &cards, &citations).await
}

/// Read the names of `cards` and of the cards and rulings `citations` name.
pub(crate) async fn names_for(
    pool: &PgPool,
    cards: &[CardId],
    citations: &[&Citation],
) -> Result<Names, JudgeError> {
    let mut wanted: BTreeSet<Uuid> = cards.iter().map(|c| c.into_inner()).collect();
    let mut rulings = BTreeSet::new();
    for c in citations {
        match *c {
            Citation::ScryfallRuling { card, ruling, .. } => {
                wanted.insert(card.into_inner());
                rulings.insert((card.into_inner(), ruling.to_string()));
            }
            Citation::OracleText { card, .. } => {
                wanted.insert(card.into_inner());
            }
            Citation::Rule { .. } | Citation::PriorCall { .. } => {}
        }
    }
    let cards = wanted;
    let mut out = Names::default();
    if cards.is_empty() {
        return Ok(out);
    }
    let ids: Vec<Uuid> = cards.into_iter().collect();
    for r in sqlx::query!(
        "SELECT oracle_id, name FROM cards WHERE oracle_id = ANY($1)",
        &ids
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("read card names"))?
    {
        out.cards.insert(CardId::new(r.oracle_id), r.name);
    }
    if !rulings.is_empty() {
        let (on, keys): (Vec<Uuid>, Vec<String>) = rulings.into_iter().unzip();
        for r in sqlx::query!(
            r#"
            SELECT r.oracle_id, r.key AS "key!",
                   to_char(r.published_at, 'YYYY-MM-DD') AS "date!"
            FROM rulings r
            JOIN UNNEST($1::uuid[], $2::text[]) AS w(oracle_id, key)
              ON r.oracle_id = w.oracle_id AND r.key = w.key
            "#,
            &on,
            &keys,
        )
        .fetch_all(pool)
        .await
        .map_err(upstream("read ruling dates"))?
        {
            if let Ok(key) = r.key.parse::<RulingKey>() {
                out.rulings.insert((CardId::new(r.oracle_id), key), r.date);
            }
        }
    }
    Ok(out)
}

/// `entry` with each card's name filled in from `names`, and a card no
/// longer in the data dropped.
fn named(mut entry: Entry, names: &Names) -> Entry {
    entry.cards = entry
        .cards
        .into_iter()
        .filter_map(|c| {
            names.card(c.id).map(|n| CardRef {
                id: c.id,
                name: n.to_owned(),
                status: c.status,
            })
        })
        .collect();
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_default_on_with_no_window_and_refuse_typos() {
        assert_eq!(Feed::parse(None, None), Ok(Feed::On { window: None }));
        assert_eq!(Feed::parse(Some(" "), Some("0")), Ok(Feed::default()));
        assert_eq!(Feed::parse(Some("OFF"), None), Ok(Feed::Off));
        assert_eq!(
            Feed::parse(Some("true"), Some(" 90 ")),
            Ok(Feed::On {
                window: Days::new(90)
            })
        );
        assert_eq!(
            Feed::parse(Some("maybe"), None),
            Err(BadFeed::Flag("maybe".into()))
        );
        for bad in ["3651", "-1", "90d"] {
            assert_eq!(
                Feed::parse(Some("false"), Some(bad)),
                Err(BadFeed::Days(bad.into())),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_cursor_reads_back_what_it_wrote_and_nothing_else() {
        let c = Cursor {
            at_micros: 1_760_000_000_123_456,
            id: Uuid::from_u128(0xabc),
        };
        assert_eq!(c.to_string().parse(), Ok(c));
        for bad in [
            "",
            ".",
            "12",
            "-1.00000000000000000000000000000abc",
            "1.xyz",
            "1.00000000-0000-0000-0000-000000000abc",
            "99999999999999999999.00000000000000000000000000000abc",
            "9223372036854775807.00000000000000000000000000000abc",
            "9007199254740992.00000000000000000000000000000abc",
        ] {
            assert_eq!(bad.parse::<Cursor>(), Err(()), "{bad}");
        }
    }

    const CARD: Uuid = Uuid::from_u128(0xc0de);
    const KEY: &str = "0123456789abcdef";

    /// Store a call asked `days_ago` (plus `secs` seconds earlier) through
    /// `interface` by `asker`, private or not; its id.
    async fn call_at(
        pool: &PgPool,
        days_ago: i32,
        secs: i32,
        interface: &str,
        asker: Option<&str>,
        private: bool,
    ) -> anyhow::Result<Uuid> {
        let citations = serde_json::json!([
            {"kind": "rule", "id": "702.19b", "quote": "Trample lets damage through."},
            {"kind": "scryfall_ruling", "card": CARD, "ruling": KEY, "quote": "It does."},
            // A shape from an older release: left out, the rest still read.
            {"kind": "scryfall_ruling", "card": CARD, "idx": 3, "quote": "Old."},
        ]);
        let context = serde_json::json!({"cards": [CARD, Uuid::from_u128(0xdead)]});
        Ok(sqlx::query_scalar(
            "INSERT INTO calls (thread_id, interface, asker_id, asker_name, private, question, \
             answer, category, source, confidence, citations, context_ids, cr_version, created_at) \
             VALUES ('t', $1, $2, $3, $4, 'q?', 'a.', 'combat', 'cr', 'high', $5, $6, '20260819', \
                     now() - make_interval(days => $7, secs => $8)) RETURNING id",
        )
        .bind(interface)
        .bind(asker.map(|_| "42"))
        .bind(asker)
        .bind(private)
        .bind(citations)
        .bind(context)
        .bind(days_ago)
        .bind(f64::from(secs))
        .fetch_one(pool)
        .await?)
    }

    fn ids(page: &Page) -> Vec<Uuid> {
        page.entries.iter().map(|e| e.id.into_inner()).collect()
    }

    /// Private calls are never read, by any read or filter; failed calls
    /// are not calls; the window hides older calls from the page and the
    /// call alike; and the names a label needs come with the page.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn feed_reads_are_public_only(pool: PgPool) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO cards (oracle_id, name, layout) VALUES ($1, 'Blood Moon', 'normal')",
        )
        .bind(CARD)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO rulings (oracle_id, key, published_at, text) VALUES ($1, $2, '2019-10-04', 'It does.')",
        )
        .bind(CARD)
        .bind(KEY)
        .execute(&pool)
        .await?;
        let web = call_at(&pool, 0, 10, "web", None, false).await?;
        let alice = call_at(&pool, 0, 20, "discord", Some("alice"), false).await?;
        let hidden = call_at(&pool, 0, 5, "discord", Some("alice"), true).await?;
        let mcp_hidden = call_at(&pool, 0, 6, "mcp", None, true).await?;
        let old = call_at(&pool, 40, 0, "mcp", None, false).await?;
        sqlx::query(
            "INSERT INTO failed_calls (thread_id, private, interface, question, error) \
             VALUES ('t', false, 'web', 'failed?', 'boom')",
        )
        .execute(&pool)
        .await?;
        for (user, score, judge) in [("1", 3_i16, false), ("2", 1, false), ("3", 2, true)] {
            sqlx::query(
                "INSERT INTO ratings (call_id, user_id, score, is_judge) VALUES ($1, $2, $3, $4)",
            )
            .bind(alice)
            .bind(user)
            .bind(score)
            .bind(judge)
            .execute(&pool)
            .await?;
        }
        // Two web raters, one with a reason that names a Discord user, and a
        // Discord one whose name the reason must never carry.
        sqlx::query(
            "INSERT INTO ratings (call_id, user_id, score, rater_kind, category, what_wrong, \
             web_name, ip, user_agent) VALUES \
             ($1, 'web:a', 1, 'web', 'wrong_card', 'not <@123>', 'Jaya', '203.0.113.7', 'UA'), \
             ($1, 'web:b', 3, 'web', NULL, NULL, NULL, NULL, NULL)",
        )
        .bind(alice)
        .execute(&pool)
        .await?;
        sqlx::query(
            "UPDATE ratings SET category = 'outdated' WHERE call_id = $1 AND user_id = '2'",
        )
        .bind(alice)
        .execute(&pool)
        .await?;

        let all = page(&pool, None, Reasons::Hidden, &Filter::default(), None).await?;
        assert_eq!(
            ids(&all),
            [web, alice, old],
            "newest first, private left out"
        );
        let thirty = Days::new(30);
        let windowed = page(&pool, thirty, Reasons::Hidden, &Filter::default(), None).await?;
        assert_eq!(ids(&windowed), [web, alice]);
        for private in [hidden, mcp_hidden] {
            assert!(
                call(&pool, None, Reasons::Hidden, CallId::new(private))
                    .await?
                    .is_none()
            );
        }
        assert!(
            call(&pool, thirty, Reasons::Hidden, CallId::new(old))
                .await?
                .is_none()
        );
        assert!(
            call(&pool, None, Reasons::Hidden, CallId::new(old))
                .await?
                .is_some()
        );
        let by_alice = Filter {
            user: judge_core::DiscordUsername::try_new("alice".to_owned()).ok(),
            ..Filter::default()
        };
        assert_eq!(
            ids(&page(&pool, None, Reasons::Hidden, &by_alice, None).await?),
            [alice]
        );
        for asked in [Asked::Discord, Asked::Mcp] {
            let only = Filter {
                asked: Some(asked),
                ..Filter::default()
            };
            let got = ids(&page(&pool, None, Reasons::Hidden, &only, None).await?);
            assert!(
                !got.contains(&hidden) && !got.contains(&mcp_hidden),
                "{got:?}"
            );
        }

        // A search matches the text and the asker's username, never a
        // mention's id, and never a private call.
        sqlx::query("UPDATE calls SET question = 'see 5<@98765>5 about it' WHERE id = $1")
            .bind(web)
            .execute(&pool)
            .await?;
        sqlx::query("UPDATE calls SET question = 'see this too' WHERE id = $1")
            .bind(hidden)
            .execute(&pool)
            .await?;
        let found = |text: &str| Filter {
            search: Search::parse(text),
            ..Filter::default()
        };
        assert_eq!(
            ids(&page(&pool, None, Reasons::Hidden, &found("SEE"), None).await?),
            [web]
        );
        assert_eq!(
            ids(&page(&pool, None, Reasons::Hidden, &found("alic"), None).await?),
            [alice]
        );
        assert!(
            page(&pool, None, Reasons::Hidden, &found("98765"), None)
                .await?
                .entries
                .is_empty()
        );
        // Not across the mention either: its digits are never read.
        assert!(
            page(&pool, None, Reasons::Hidden, &found("55"), None)
                .await?
                .entries
                .is_empty()
        );
        assert_eq!(
            ids(&page(&pool, None, Reasons::Hidden, &found("5@…5 about"), None).await?),
            [web]
        );
        assert!(
            page(&pool, None, Reasons::Hidden, &found("%"), None)
                .await?
                .entries
                .is_empty()
        );

        let Some((entry, names)) = call(&pool, None, Reasons::Hidden, CallId::new(alice)).await?
        else {
            anyhow::bail!("alice's call is public");
        };
        assert_eq!(entry.asker.as_deref(), Some("alice"));
        assert_eq!(entry.asked, Asked::Discord);
        assert_eq!(
            entry.citations.len(),
            2,
            "the unreadable citation is left out"
        );
        assert_eq!(
            entry
                .cards
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["Blood Moon"],
            "a card no longer in the data is left out"
        );
        assert_eq!(
            names.ruling(CardId::new(CARD), &KEY.parse()?),
            Some("2019-10-04")
        );
        assert_eq!(
            entry.ratings,
            Ratings {
                discord: Counts {
                    correct: 1,
                    partial: 1,
                    incorrect: 1,
                },
                web: Counts {
                    correct: 1,
                    partial: 0,
                    incorrect: 1,
                },
                judge: Some(Score::Partial),
            },
            "the web's ratings counted apart from Discord's"
        );
        assert_eq!(entry.reasons, None, "reasons are the operator's by default");
        let Some((shown, _)) = call(&pool, None, Reasons::Shown, CallId::new(alice)).await? else {
            anyhow::bail!("alice's call is public");
        };
        let reasons = shown.reasons.ok_or_else(|| anyhow::anyhow!("no reasons"))?;
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        let web = reasons
            .iter()
            .find(|r| r.rater_kind == RaterKind::Web)
            .ok_or_else(|| anyhow::anyhow!("no web reason"))?;
        assert_eq!(web.wrong.as_deref(), Some("not @…"), "mentions reduced");
        assert_eq!(
            public_text("@everyone and @here"),
            "@\u{200b}everyone and @\u{200b}here"
        );
        // A reason the operator dismissed leaves the public reads.
        sqlx::query(
            "INSERT INTO feedback_queue (call_id, user_id, status, note, resolved_at) \
             VALUES ($1, 'web:a', 'dismissed', 'spam', now())",
        )
        .bind(alice)
        .execute(&pool)
        .await?;
        let Some((after, _)) = call(&pool, None, Reasons::Shown, CallId::new(alice)).await? else {
            anyhow::bail!("alice's call is public");
        };
        let left = after.reasons.ok_or_else(|| anyhow::anyhow!("no reasons"))?;
        assert!(
            left.iter().all(|r| r.rater_kind != RaterKind::Web),
            "the dismissed reason is gone: {left:?}"
        );
        assert_eq!(left.len(), 1);
        assert_eq!(
            after.ratings, entry.ratings,
            "the counts are not the queue's"
        );
        let text = serde_json::to_string(&reasons)?;
        for identity in ["Jaya", "203.0.113.7", "UA", "web:a", "\"2\""] {
            assert!(!text.contains(identity), "{identity} in {text}");
        }
        let page = page(&pool, None, Reasons::Shown, &Filter::default(), None).await?;
        assert!(page.entries.iter().all(|e| e.reasons.is_some()));
        Ok(())
    }

    /// The web may rate a public call within the window, and nothing else.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn only_a_public_call_in_the_window_is_rateable(pool: PgPool) -> anyhow::Result<()> {
        let public = call_at(&pool, 0, 0, "web", None, false).await?;
        let private = call_at(&pool, 0, 0, "discord", Some("alice"), true).await?;
        let old = call_at(&pool, 40, 0, "web", None, false).await?;
        let thirty = Days::new(30);
        assert!(rateable(&pool, thirty, CallId::new(public)).await?);
        assert!(!rateable(&pool, thirty, CallId::new(private)).await?);
        assert!(!rateable(&pool, thirty, CallId::new(old)).await?);
        assert!(rateable(&pool, None, CallId::new(old)).await?);
        assert!(!rateable(&pool, None, CallId::new(Uuid::nil())).await?);
        Ok(())
    }

    /// A call holding a value this build does not know (an interface,
    /// confidence or source a newer build of the same major may write) is
    /// left out of its page and has no page of its own; the rest of the
    /// feed reads, and a page whose last row is left out still has a next.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_reassigned_username_lists_only_its_current_holder(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        // Discord user 1 asked as "bob", renamed, and user 2 took "bob";
        // then user 3, also "bob" later, asked only in private.
        let asked = |id: &'static str, secs: i32, private: bool| {
            let pool = pool.clone();
            async move {
                let call = call_at(&pool, 0, secs, "discord", Some("bob"), private).await?;
                sqlx::query("UPDATE calls SET asker_id = $2 WHERE id = $1")
                    .bind(call)
                    .bind(id)
                    .execute(&pool)
                    .await?;
                anyhow::Ok(call)
            }
        };
        let earlier = asked("1", 30, false).await?;
        let current = asked("2", 20, false).await?;
        asked("3", 10, true).await?;
        let bob = Filter {
            user: judge_core::DiscordUsername::try_new("bob".to_owned()).ok(),
            ..Filter::default()
        };
        // The private call does not make user 3 the holder: the public cannot
        // see it, so it cannot tell it exists.
        assert_eq!(
            ids(&page(&pool, None, Reasons::Hidden, &bob, None).await?),
            vec![current]
        );
        // The earlier holder's call is still in the feed, under its name.
        let all = ids(&page(&pool, None, Reasons::Hidden, &Filter::default(), None).await?);
        assert!(all.contains(&earlier) && all.contains(&current));
        Ok(())
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_unreadable_call_is_left_out_not_fatal(pool: PgPool) -> anyhow::Result<()> {
        // What a newer build's migration could relax.
        sqlx::query("ALTER TABLE calls DROP CONSTRAINT calls_interface_check")
            .execute(&pool)
            .await?;
        let mut asked = vec![];
        for i in 0..=PAGE_SIZE {
            // Newest first: `asked[0]` heads the first page.
            asked.push(call_at(&pool, 0, i32::try_from(i)?, "web", None, false).await?);
        }
        let last_of_first = *asked
            .get(PAGE_SIZE - 1)
            .ok_or_else(|| anyhow::anyhow!("calls"))?;
        let bad = [
            (
                asked.first().copied(),
                "UPDATE calls SET confidence = 'certain' WHERE id = $1",
            ),
            (
                Some(last_of_first),
                "UPDATE calls SET source = 'oracle' WHERE id = $1",
            ),
            (
                asked.get(3).copied(),
                "UPDATE calls SET interface = 'telegram' WHERE id = $1",
            ),
        ];
        let mut unreadable = vec![];
        for (id, sql) in bad {
            let id = id.ok_or_else(|| anyhow::anyhow!("calls"))?;
            sqlx::query(sql).bind(id).execute(&pool).await?;
            unreadable.push(id);
        }
        let first = page(&pool, None, Reasons::Hidden, &Filter::default(), None).await?;
        assert_eq!(first.entries.len(), PAGE_SIZE - 3);
        let after = first.next.ok_or_else(|| anyhow::anyhow!("a next page"))?;
        assert_eq!(after.id(), last_of_first);
        let second = page(
            &pool,
            None,
            Reasons::Hidden,
            &Filter::default(),
            Some(after),
        )
        .await?;
        assert_eq!(second.next, None);
        let mut seen = ids(&first);
        seen.extend(ids(&second));
        let expected: Vec<Uuid> = asked
            .iter()
            .copied()
            .filter(|id| !unreadable.contains(id))
            .collect();
        assert_eq!(seen, expected);
        for id in unreadable {
            assert!(
                call(&pool, None, Reasons::Hidden, CallId::new(id))
                    .await?
                    .is_none()
            );
            // What the feed cannot show cannot be rated.
            assert!(!rateable(&pool, None, CallId::new(id)).await?);
        }
        let good = *asked.get(1).ok_or_else(|| anyhow::anyhow!("calls"))?;
        assert!(
            call(&pool, None, Reasons::Hidden, CallId::new(good))
                .await?
                .is_some()
        );
        assert!(rateable(&pool, None, CallId::new(good)).await?);
        Ok(())
    }

    /// Paging returns every public call once, in order, however many share
    /// a timestamp.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn pages_cover_every_call_once(pool: PgPool) -> anyhow::Result<()> {
        let n = PAGE_SIZE * 2 + 3;
        for i in 0..n {
            // Pairs share a time: the id breaks the tie.
            let secs = i32::try_from(i / 2)?;
            call_at(&pool, 0, secs, "web", None, false).await?;
        }
        let mut seen = vec![];
        let mut after = None;
        loop {
            let p = page(&pool, None, Reasons::Hidden, &Filter::default(), after).await?;
            assert!(p.entries.len() <= PAGE_SIZE);
            seen.extend(ids(&p));
            match p.next {
                Some(c) => {
                    after = Some(
                        c.to_string()
                            .parse()
                            .map_err(|()| anyhow::anyhow!("cursor"))?,
                    );
                }
                None => break,
            }
        }
        let all: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM calls ORDER BY created_at DESC, id DESC")
                .fetch_all(&pool)
                .await?;
        assert_eq!(seen, all);
        Ok(())
    }

    #[test]
    fn a_search_is_bounded_and_matches_its_text_literally() {
        assert_eq!(Search::parse("  "), None);
        assert_eq!(Search::parse(&"x".repeat(MAX_SEARCH_CHARS + 1)), None);
        assert_eq!(Search::parse("a\0b"), None);
        assert_eq!(
            Search::parse(" 100%_off\\ ").map(|s| s.pattern()),
            Some("%100\\%\\_off\\\\%".to_owned())
        );
    }

    #[test]
    fn mentions_lose_their_ids() {
        assert_eq!(
            without_mentions("ask <@123> or <@!45>, <@&6> in <#78>; not <@x> or <@>"),
            "ask @… or @…, @… in #…; not <@x> or <@>"
        );
    }

    #[test]
    fn every_interface_is_a_feed_label_with_its_stored_id() {
        for i in Interface::ALL {
            assert_eq!(Asked::from(i).id(), i.id());
            assert_eq!(Asked::parse(i.id()), Some(Asked::from(i)));
        }
        assert_eq!(Asked::parse("agent"), Some(Asked::Agent));
        assert_eq!(Asked::parse("Discord"), None);
    }
}
