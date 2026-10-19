//! The public feed (#26): every answered call not asked private, newest
//! first, from every interface, and one call by id. The web role serves it
//! (`GET /api/feed`, `GET /api/calls/{id}`); this module is the settings and
//! the reads.
//!
//! What a public read may return is decided here, in SQL, and nowhere else:
//! a private call is never read (`NOT c.private` is in every query's
//! `WHERE`, and a test asks each read for one), a failed call is in another
//! table, and a call older than the feed window
//! (`JUDGE_FEED_DAYS`) is not read either. A missing call, a private one and
//! one outside the window are the same `None`. The asker's Discord id is
//! never selected; only the username snapshotted at asking time is, and
//! `/forget` has already cleared it on the forgotten user's calls.

use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use judge_core::{
    CallId, CardId, CardRef, Citation, Confidence, Interface, JudgeError, RulingKey, Score, Source,
};
use regex::Regex;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
}

/// How a call has been rated: counts, and the judge override when a member
/// with the judge role rated (the latest such rating). Who rated is never
/// shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Ratings {
    /// Rated correct.
    pub correct: u32,
    /// Rated partially correct.
    pub partial: u32,
    /// Rated incorrect.
    pub incorrect: u32,
    /// The judge override's score, when there is one.
    #[serde(serialize_with = "score_name")]
    pub judge: Option<Score>,
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
    correct: i64,
    partial: i64,
    incorrect: i64,
    judge: Option<i16>,
    retired_reason: Option<String>,
}

/// The window as the queries take it: days, or `NULL` for none.
fn window_days(window: Option<Days>) -> Option<i32> {
    window.map(|d| i32::from(d.get()))
}

/// The newest [`PAGE_SIZE`] public calls matching `filter` before `after`
/// (from the newest when `None`), within `window`.
///
/// # Errors
/// `Upstream` from sqlx, or a row that does not read.
pub async fn page(
    pool: &PgPool,
    window: Option<Days>,
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
               rt.correct AS "correct!", rt.partial AS "partial!", rt.incorrect AS "incorrect!",
               rt.judge, c.retired_reason
        FROM calls c
        CROSS JOIN LATERAL (
            SELECT count(*) FILTER (WHERE r.score = 3) AS correct,
                   count(*) FILTER (WHERE r.score = 2) AS partial,
                   count(*) FILTER (WHERE r.score = 1) AS incorrect,
                   (array_agg(r.score ORDER BY r.ts DESC) FILTER (WHERE r.is_judge))[1] AS judge
            FROM ratings r WHERE r.call_id = c.id
        ) rt
        WHERE NOT c.private
          AND ($1::int IS NULL OR c.created_at >= now() - make_interval(days => $1))
          AND ($2::text IS NULL OR c.interface = $2)
          AND ($3::text IS NULL OR c.asker_name = $3)
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
    let entries = rows.into_iter().map(entry).collect::<Result<Vec<_>, _>>()?;
    let names = names(pool, &entries).await?;
    let entries = entries.into_iter().map(|e| named(e, &names)).collect();
    Ok(Page {
        entries,
        next,
        names,
    })
}

/// The public call `id` within `window`; `None` for a call that is missing,
/// private or older than the window alike.
///
/// # Errors
/// `Upstream` from sqlx, or a row that does not read.
pub async fn call(
    pool: &PgPool,
    window: Option<Days>,
    id: CallId,
) -> Result<Option<(Entry, Names)>, JudgeError> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT c.id,
               (extract(epoch FROM c.created_at) * 1000000)::bigint AS "at_micros!",
               to_char(c.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "asked_at!",
               c.interface, c.asker_name, c.question, c.answer, c.confidence, c.source,
               c.cr_version, c.citations, coalesce(c.context_ids->'cards', '[]') AS "cards!",
               rt.correct AS "correct!", rt.partial AS "partial!", rt.incorrect AS "incorrect!",
               rt.judge, c.retired_reason
        FROM calls c
        CROSS JOIN LATERAL (
            SELECT count(*) FILTER (WHERE r.score = 3) AS correct,
                   count(*) FILTER (WHERE r.score = 2) AS partial,
                   count(*) FILTER (WHERE r.score = 1) AS incorrect,
                   (array_agg(r.score ORDER BY r.ts DESC) FILTER (WHERE r.is_judge))[1] AS judge
            FROM ratings r WHERE r.call_id = c.id
        ) rt
        WHERE NOT c.private
          AND ($1::int IS NULL OR c.created_at >= now() - make_interval(days => $1))
          AND c.id = $2
        "#,
        window_days(window),
        id.into_inner(),
    )
    .fetch_optional(pool)
    .await
    .map_err(upstream("read a public call"))?;
    let Some(row) = row else { return Ok(None) };
    let entry = entry(row)?;
    let names = names(pool, std::slice::from_ref(&entry)).await?;
    Ok(Some((named(entry, &names), names)))
}

/// A unit-variant enum from its stored serde name.
fn stored<T: serde::de::DeserializeOwned>(value: &str, what: &str) -> Result<T, JudgeError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|e| bad_row_from(e, format!("calls.{what} {value:?}")))
}

fn count(n: i64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
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
            correct: count(r.correct),
            partial: count(r.partial),
            incorrect: count(r.incorrect),
            judge,
        },
        retired: r.retired_reason,
    })
}

/// Read the card names and ruling dates `entries` need.
async fn names(pool: &PgPool, entries: &[Entry]) -> Result<Names, JudgeError> {
    let mut cards = BTreeSet::new();
    let mut rulings = BTreeSet::new();
    for e in entries {
        cards.extend(e.cards.iter().map(|c| c.id.into_inner()));
        for c in &e.citations {
            match c {
                Citation::ScryfallRuling { card, ruling, .. } => {
                    cards.insert(card.into_inner());
                    rulings.insert((card.into_inner(), ruling.to_string()));
                }
                Citation::OracleText { card, .. } => {
                    cards.insert(card.into_inner());
                }
                Citation::Rule { .. } | Citation::PriorCall { .. } => {}
            }
        }
    }
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

        let all = page(&pool, None, &Filter::default(), None).await?;
        assert_eq!(
            ids(&all),
            [web, alice, old],
            "newest first, private left out"
        );
        let thirty = Days::new(30);
        let windowed = page(&pool, thirty, &Filter::default(), None).await?;
        assert_eq!(ids(&windowed), [web, alice]);
        for private in [hidden, mcp_hidden] {
            assert!(call(&pool, None, CallId::new(private)).await?.is_none());
        }
        assert!(call(&pool, thirty, CallId::new(old)).await?.is_none());
        assert!(call(&pool, None, CallId::new(old)).await?.is_some());
        let by_alice = Filter {
            user: judge_core::DiscordUsername::try_new("alice".to_owned()).ok(),
            ..Filter::default()
        };
        assert_eq!(ids(&page(&pool, None, &by_alice, None).await?), [alice]);
        for asked in [Asked::Discord, Asked::Mcp] {
            let only = Filter {
                asked: Some(asked),
                ..Filter::default()
            };
            let got = ids(&page(&pool, None, &only, None).await?);
            assert!(
                !got.contains(&hidden) && !got.contains(&mcp_hidden),
                "{got:?}"
            );
        }

        let Some((entry, names)) = call(&pool, None, CallId::new(alice)).await? else {
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
                correct: 1,
                partial: 1,
                incorrect: 1,
                judge: Some(Score::Partial),
            }
        );
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
            let p = page(&pool, None, &Filter::default(), after).await?;
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
