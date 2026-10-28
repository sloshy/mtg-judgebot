//! The operator's own view of the instance (#24, D37): every stored call,
//! private ones included, one call with its runs, charges and raters, and the
//! settings of the `--admin` role's listener.
//!
//! Where the public feed (`crate::feed`) decides in SQL what a stranger may
//! read, nothing here hides anything: these reads are for whoever holds the
//! instance's database or an admin token. They are reached three ways, each
//! behind its own credential: the admin listener (`ADMIN_TOKEN`, loopback),
//! the agent tools of a process that has the database itself (`judge-cli`,
//! stdio `judge-mcp`), and `/mcp` with `MCP_ADMIN_TOKEN`. `MCP_TOKEN` reaches
//! none of them.
//!
//! The failed calls and the usage are `crate::db::stats`'s, which `judge-cli
//! stats` and `failures` read too.

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::Context as _;
use judge_core::{
    CallId, CardId, CardRef, Citation, Confidence, FeedbackCategory, JudgeError, RaterKind, Source,
};
use judge_llm::ApiKey;
use schemars::JsonSchema;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::{bad_row, upstream};
use crate::feed::{Asked, Cursor, Names, Search, names_for, stored};

/// `ADMIN_ADDR`: where the `--admin` role listens.
pub const ADMIN_ADDR_ENV: &str = "ADMIN_ADDR";
/// `ADMIN_PORT`: the host port compose publishes the admin listener on
/// (loopback), default 8789.
pub const ADMIN_PORT_ENV: &str = "ADMIN_PORT";
/// What the compose `judgebot` service sets `ADMIN_ADDR` to, whatever
/// `.env` says (`docker-compose.yml` holds the same string, and a test holds
/// it to this): every address of the container, so the proxy can reach it.
pub const COMPOSE_ADDR: &str = "0.0.0.0:8789";
/// `ADMIN_TOKEN`: the admin listener's bearer token.
pub const ADMIN_TOKEN_ENV: &str = "ADMIN_TOKEN";
/// `MCP_TOKEN`: `/mcp`'s token for the judge and lookup tools.
pub const MCP_TOKEN_ENV: &str = "MCP_TOKEN";
/// `MCP_ADMIN_TOKEN`: `/mcp`'s token for the judge, lookup and admin tools.
pub const MCP_ADMIN_TOKEN_ENV: &str = "MCP_ADMIN_TOKEN";
/// `ADMIN_ADDR`'s default: loopback, for a process outside a container.
/// The compose service sets `0.0.0.0:8789`, and the proxy publishes it on
/// the host's loopback only.
pub const DEFAULT_ADDR: &str = "127.0.0.1:8789";
/// The shortest token accepted, in bytes, for every bearer token the
/// instance takes: a guessable token is worse than none.
pub const MIN_TOKEN_BYTES: usize = 24;
/// Where the admin pages are, under the listener's address.
pub const PAGE_PATH: &str = "/admin";

/// Check a bearer token's shape: at least [`MIN_TOKEN_BYTES`] of printable
/// ASCII without spaces (it travels in an HTTP header, and anything a client
/// cannot send would make an endpoint that is 401 forever).
///
/// # Errors
/// What is wrong, naming `var` and never quoting the value.
pub fn bearer_token(var: &str, value: &str) -> anyhow::Result<ApiKey> {
    anyhow::ensure!(
        value.len() >= MIN_TOKEN_BYTES,
        "{var} must be at least {MIN_TOKEN_BYTES} bytes (try `openssl rand -base64 32`)"
    );
    anyhow::ensure!(
        value.bytes().all(|b| b.is_ascii_graphic()),
        "{var} must be printable ASCII without spaces (try `openssl rand -base64 32`)"
    );
    Ok(ApiKey::from(value))
}

/// Refuse a token equal to another one the instance takes: each opens a
/// different set of things, so one value for two would hand the holder of
/// the weaker both.
///
/// # Errors
/// They are equal, naming both variables.
pub fn distinct(
    var: &str,
    value: &str,
    other_var: &str,
    other: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        other != Some(value),
        "{var} must differ from {other_var}: the two open different things, and one value for \
         both would let whoever holds either use both (`openssl rand -base64 32` makes another)"
    );
    Ok(())
}

/// The admin listener's bearer token, checked: well formed, and equal to
/// neither `MCP_TOKEN` nor `MCP_ADMIN_TOKEN`. Redacted in `Debug`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminToken(ApiKey);

impl AdminToken {
    /// The token as a client sends it.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

/// `/mcp`'s admin token, checked: well formed, and equal to neither
/// `MCP_TOKEN` nor `ADMIN_TOKEN`. Only [`McpAdminToken::from_vars`] makes
/// one, so the admin tools can be mounted on `/mcp` only behind a token that
/// passed those checks (`judge_agent::mcp::admin_http_service` takes one).
/// Redacted in `Debug`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpAdminToken(ApiKey);

impl McpAdminToken {
    /// Read `MCP_ADMIN_TOKEN` (blank counts as unset).
    ///
    /// # Errors
    /// A malformed token, or one equal to `MCP_TOKEN` or `ADMIN_TOKEN`.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Option<Self>> {
        let var = |k: &str| {
            get(k)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let Some(t) = var(MCP_ADMIN_TOKEN_ENV) else {
            return Ok(None);
        };
        let key = bearer_token(MCP_ADMIN_TOKEN_ENV, &t)?;
        distinct(
            MCP_ADMIN_TOKEN_ENV,
            &t,
            MCP_TOKEN_ENV,
            var(MCP_TOKEN_ENV).as_deref(),
        )?;
        distinct(
            MCP_ADMIN_TOKEN_ENV,
            &t,
            ADMIN_TOKEN_ENV,
            var(ADMIN_TOKEN_ENV).as_deref(),
        )?;
        Ok(Some(Self(key)))
    }

    /// The token as a client sends it.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

/// The `--admin` role's settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminSettings {
    /// `ADMIN_ADDR`.
    pub addr: SocketAddr,
    /// `ADMIN_TOKEN`, when it is set.
    pub token: Option<AdminToken>,
    /// `ADMIN_PORT`: the host port the proxy publishes it on, when set.
    pub published: Option<u16>,
}

impl AdminSettings {
    /// Read `ADMIN_ADDR` and `ADMIN_TOKEN` (blank counts as unset), the
    /// token checked against `MCP_TOKEN` and `MCP_ADMIN_TOKEN`.
    ///
    /// # Errors
    /// A malformed address or token, or a token equal to another.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let var = |k: &str| {
            get(k)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let addr = var(ADMIN_ADDR_ENV)
            .unwrap_or_else(|| DEFAULT_ADDR.to_owned())
            .parse::<SocketAddr>()
            .with_context(|| {
                format!("{ADMIN_ADDR_ENV} must be a socket address such as {DEFAULT_ADDR}")
            })?;
        let published = var(ADMIN_PORT_ENV)
            .map(|p| {
                p.parse::<u16>()
                    .ok()
                    .filter(|p| *p > 0)
                    .with_context(|| format!("{ADMIN_PORT_ENV} must be a port number 1..=65535"))
            })
            .transpose()?;
        let token = match var(ADMIN_TOKEN_ENV) {
            None => None,
            Some(t) => {
                let key = bearer_token(ADMIN_TOKEN_ENV, &t)?;
                distinct(
                    ADMIN_TOKEN_ENV,
                    &t,
                    MCP_TOKEN_ENV,
                    var(MCP_TOKEN_ENV).as_deref(),
                )?;
                distinct(
                    ADMIN_TOKEN_ENV,
                    &t,
                    MCP_ADMIN_TOKEN_ENV,
                    var(MCP_ADMIN_TOKEN_ENV).as_deref(),
                )?;
                Some(AdminToken(key))
            }
        };
        Ok(Self {
            addr,
            token,
            published,
        })
    }

    /// The address a browser on this machine opens: the listen address, or,
    /// for an unspecified host (`0.0.0.0`, `::`: inside a container whose
    /// port the proxy publishes on the host's loopback), loopback on
    /// `ADMIN_PORT` (else the same port).
    #[must_use]
    pub fn browser_addr(&self) -> SocketAddr {
        let ip = match self.addr.ip() {
            IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            ip => return SocketAddr::new(ip, self.addr.port()),
        };
        SocketAddr::new(ip, self.published.unwrap_or(self.addr.port()))
    }

    /// The admin page's address carrying `token` in its fragment, which a
    /// browser never sends: `http://127.0.0.1:8789/admin#token=…`.
    #[must_use]
    pub fn page_url(&self, token: &AdminToken) -> String {
        format!(
            "http://{}{PAGE_PATH}#token={}",
            self.browser_addr(),
            fragment_escape(token.expose())
        )
    }
}

/// `text` with every byte but the URL-unreserved ones percent-encoded, so a
/// base64 token's `+`, `/` and `=` reach `URLSearchParams` unchanged (a bare
/// `+` would read as a space).
fn fragment_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Calls per page of the admin list.
pub const PAGE_SIZE: usize = 50;
/// The longest asker or model filter, in characters.
pub const MAX_FILTER_CHARS: usize = 100;

/// A filter value: trimmed, 1 to [`MAX_FILTER_CHARS`] characters, no NUL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterText(String);

impl FilterText {
    /// `text` as a filter, or `None` when it is blank, too long or holds a NUL.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty() && text.chars().count() <= MAX_FILTER_CHARS && !text.contains('\0'))
            .then(|| Self(text.to_owned()))
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Which calls an admin page holds. Every field narrows; none hides a
/// private call unless asked to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallFilter {
    /// Only calls asked through this interface.
    pub asked: Option<Asked>,
    /// Only calls whose asker has this Discord username or user id.
    pub asker: Option<FilterText>,
    /// Only calls whose run billed this model (the name a response gave).
    pub model: Option<FilterText>,
    /// Only private calls (`true`) or only public ones (`false`).
    pub private: Option<bool>,
    /// Only calls whose question, answer or asker's username contains this.
    pub search: Option<Search>,
}

/// One stored call as the admin list shows it.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct CallRow {
    /// The call.
    pub id: Uuid,
    /// When it was asked, RFC 3339 UTC.
    pub asked_at: String,
    /// Through which interface.
    pub interface: Asked,
    /// The asker's Discord username, when known and not forgotten.
    pub asker: Option<String>,
    /// The asker's Discord user id, likewise.
    pub asker_id: Option<String>,
    /// Asked private: kept off everything public.
    pub private: bool,
    /// The question, verbatim.
    pub question: String,
    /// The model's confidence.
    pub confidence: Confidence,
    /// Why the retirement pass took it out of the examples, when it has.
    pub retired: Option<String>,
    /// The chat models its run billed, by the name each response gave.
    pub models: Vec<String>,
    /// What its run cost, in USD; `None` without a recorded run (an agent
    /// session, whose model is the agent).
    pub usd: Option<f64>,
    /// Whether that cost includes a worst-case estimate.
    pub cost_estimated: bool,
}

/// A page of the admin list.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct CallsPage {
    /// Newest first.
    pub calls: Vec<CallRow>,
    /// Pass back as `cursor` for the next page; `None` on the last.
    pub next: Option<String>,
    /// Every chat model any run has billed, for the model filter.
    pub models: Vec<String>,
}

/// One rating, with who gave it and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RatingRow {
    /// The rater: a Discord user id, or `web:<session uuid>`.
    pub user: String,
    /// A Discord user or a web session.
    pub kind: RaterKind,
    /// The name a web rater gave.
    pub name: Option<String>,
    /// `correct`, `partial` or `incorrect`.
    pub score: String,
    /// Whether the rater held the judge role.
    pub judge: bool,
    /// When, RFC 3339 UTC.
    pub at: String,
    /// The reason's category, for a rating that finds fault and was given
    /// one.
    pub category: Option<FeedbackCategory>,
    /// What is wrong, in the rater's words.
    pub wrong: Option<String>,
    /// The right answer, in the rater's words.
    pub right: Option<String>,
}

/// One billed request of a run.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct ChargeRow {
    /// `extraction`, `synthesis`, `tool_round` or `embedding`.
    pub stage: String,
    /// The provider table it went to.
    pub provider: String,
    /// The model the response named.
    pub model: String,
    /// Tokens in, `None` when the response reported no usage.
    pub input_tokens: Option<i64>,
    /// Tokens out.
    pub output_tokens: Option<i64>,
    /// Cached tokens read.
    pub cache_read_tokens: Option<i64>,
    /// Cached tokens written.
    pub cache_write_tokens: Option<i64>,
    /// Its cost in USD.
    pub usd: f64,
    /// Whether that cost is the worst case the spend cap reserved.
    pub estimated: bool,
}

/// One run of the built-in pipeline that left a call.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct RunRow {
    /// The run.
    pub id: Uuid,
    /// When it started, RFC 3339 UTC.
    pub started_at: String,
    /// How long it took, in milliseconds.
    pub elapsed_ms: i64,
    /// `answered`, …: `judge_core::Outcome`'s ids.
    pub outcome: String,
    /// Whether the first synthesis attempt was rejected and retried.
    pub retried: bool,
    /// Whether a request carried the `lookup_rules` round.
    pub tool_round: bool,
    /// Its billed requests, in order.
    pub charges: Vec<ChargeRow>,
}

/// Everything stored about one call.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct CallDetail {
    /// The call.
    pub id: Uuid,
    /// When it was asked, RFC 3339 UTC.
    pub asked_at: String,
    /// Through which interface.
    pub interface: Asked,
    /// The asker's Discord username.
    pub asker: Option<String>,
    /// The asker's Discord user id.
    pub asker_id: Option<String>,
    /// Asked private.
    pub private: bool,
    /// The thread it was asked in (`web:<uuid>`, `agent:<uuid>`, a Discord
    /// channel or thread id).
    pub thread: String,
    /// The language it was asked in.
    pub locale: String,
    /// The agent session that stored it, if one did.
    pub session: Option<Uuid>,
    /// The question, verbatim.
    pub question: String,
    /// The answer, verbatim.
    pub answer: String,
    /// The model's confidence.
    pub confidence: Confidence,
    /// The rules body it was answered under.
    pub source: Source,
    /// The category id.
    pub category: String,
    /// The CR it was answered under, `YYYYMMDD`.
    pub cr_version: String,
    /// Its citations, in the answer's order (one stored in a shape that no
    /// longer reads is left out).
    pub citations: Vec<Citation>,
    /// The cards the question was taken to be about.
    pub cards: Vec<CardRef>,
    /// Why the retirement pass retired it, when it has.
    pub retired: Option<String>,
    /// Every rating, oldest first, with the rater.
    pub ratings: Vec<RatingRow>,
    /// The runs that left it (one, or none for an agent session).
    pub runs: Vec<RunRow>,
}

fn usd(micro: i64) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "micro-dollar amounts stay far below 2^53"
    )]
    let usd = micro as f64 / 1_000_000.0;
    usd
}

/// The newest [`PAGE_SIZE`] calls matching `filter` before `after` (from the
/// newest when `None`), private ones included unless the filter says
/// otherwise.
///
/// # Errors
/// `Upstream` from sqlx. A call that does not read is left out, with a WARN.
pub async fn calls(
    pool: &PgPool,
    filter: &CallFilter,
    after: Option<Cursor>,
) -> Result<CallsPage, JudgeError> {
    let limit = i64::try_from(PAGE_SIZE)
        .unwrap_or(i64::MAX)
        .saturating_add(1);
    let mut rows = sqlx::query!(
        r#"
        SELECT c.id,
               (extract(epoch FROM c.created_at) * 1000000)::bigint AS "at_micros!",
               to_char(c.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "asked_at!",
               c.interface, c.asker_id, c.asker_name, c.private, c.question, c.confidence,
               c.retired_reason,
               coalesce(run.models, '{}') AS "models!",
               run.micro_usd, coalesce(run.estimated, false) AS "estimated!"
        FROM calls c
        LEFT JOIN LATERAL (
            SELECT array_agg(DISTINCT ch.model ORDER BY ch.model)
                       FILTER (WHERE ch.stage <> 'embedding') AS models,
                   sum(ch.micro_usd)::bigint AS micro_usd,
                   bool_or(ch.cost_estimated) AS estimated
            FROM judge_runs r JOIN judge_run_charges ch ON ch.run_id = r.id
            WHERE r.call_id = c.id
        ) run ON true
        WHERE ($1::text IS NULL OR c.interface = $1)
          AND ($2::text IS NULL OR c.asker_name = $2 OR c.asker_id = $2)
          AND ($3::boolean IS NULL OR c.private = $3)
          AND ($4::text IS NULL OR EXISTS (
                SELECT 1 FROM judge_runs r JOIN judge_run_charges ch ON ch.run_id = r.id
                WHERE r.call_id = c.id AND ch.model = $4))
          AND ($5::text IS NULL
               OR c.question ILIKE $5 OR c.answer ILIKE $5 OR c.asker_name ILIKE $5)
          AND ($6::bigint IS NULL
               OR (c.created_at, c.id) < ('epoch'::timestamptz + $6 * interval '1 microsecond', $7))
        ORDER BY c.created_at DESC, c.id DESC
        LIMIT $8
        "#,
        filter.asked.map(Asked::id),
        filter.asker.as_ref().map(FilterText::as_str),
        filter.private,
        filter.model.as_ref().map(FilterText::as_str),
        filter.search.as_ref().map(Search::pattern),
        after.map(Cursor::at_micros),
        after.map_or(Uuid::nil(), Cursor::id),
        limit,
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("read the admin call list"))?;
    let next = if rows.len() > PAGE_SIZE {
        rows.truncate(PAGE_SIZE);
        rows.last()
            .map(|r| Cursor::new(r.at_micros, r.id).to_string())
    } else {
        None
    };
    // A call holding a value this build does not know (a newer build of the
    // same major wrote it) is left out with a WARN rather than failing the
    // list; the cursor above already moves past it.
    let calls = rows
        .into_iter()
        .filter_map(|r| {
            let id = r.id;
            let row = || -> Result<CallRow, JudgeError> {
                Ok(CallRow {
                    id: r.id,
                    asked_at: r.asked_at,
                    interface: asked(&r.interface)?,
                    asker: r.asker_name,
                    asker_id: r.asker_id,
                    private: r.private,
                    question: r.question,
                    confidence: stored(&r.confidence, "confidence")?,
                    retired: r.retired_reason,
                    models: r.models,
                    usd: r.micro_usd.map(usd),
                    cost_estimated: r.estimated,
                })
            };
            row()
                .inspect_err(|e| {
                    if crate::unread::first(crate::unread::Site::AdminList, id) {
                        tracing::warn!(call = %id, error = %format_args!("{e:#}"), "a stored call does not read; left out of the admin list (logged once)");
                    }
                })
                .ok()
        })
        .collect::<Vec<_>>();
    let models = sqlx::query_scalar!(
        "SELECT DISTINCT model FROM judge_run_charges WHERE stage <> 'embedding' ORDER BY model"
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("read the billed models"))?;
    Ok(CallsPage {
        calls,
        next,
        models,
    })
}

fn asked(id: &str) -> Result<Asked, JudgeError> {
    Asked::parse(id).ok_or_else(|| bad_row(format!("calls.interface {id:?}")))
}

/// Everything stored about call `id`, and the names its citations' labels
/// need; `None` when there is no such call, and for one holding a value this
/// build does not read (an interface, confidence or source a newer build of
/// the same major wrote), with a WARN naming it, as the list leaves it out.
/// A rating of the call that does not read is left out of it the same way.
///
/// # Errors
/// `Upstream` from sqlx.
pub async fn call(pool: &PgPool, id: CallId) -> Result<Option<(CallDetail, Names)>, JudgeError> {
    let Some(r) = sqlx::query!(
        r#"
        SELECT c.id,
               to_char(c.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "asked_at!",
               c.interface, c.asker_id, c.asker_name, c.private, c.thread_id, c.locale,
               c.session_id, c.question, c.answer, c.confidence, c.source, c.category,
               c.cr_version, c.citations, coalesce(c.context_ids->'cards', '[]') AS "cards!",
               c.unreleased_cards, c.retired_reason
        FROM calls c WHERE c.id = $1
        "#,
        id.into_inner(),
    )
    .fetch_optional(pool)
    .await
    .map_err(upstream("read a call"))?
    else {
        return Ok(None);
    };
    let decoded = || -> Result<_, JudgeError> {
        Ok((
            asked(&r.interface)?,
            stored::<Confidence>(&r.confidence, "confidence")?,
            stored::<Source>(&r.source, "source")?,
        ))
    };
    let (interface, confidence, source) = match decoded() {
        Ok(fields) => fields,
        Err(e) => {
            if crate::unread::first(crate::unread::Site::AdminCall, r.id) {
                tracing::warn!(call = %r.id, error = %format_args!("{e:#}"), "a stored call does not read; answered as not found (logged once)");
            }
            return Ok(None);
        }
    };
    // Each citation alone, as the feed reads them: a shape from an older
    // release that no longer reads is left out rather than failing the call.
    let citations: Vec<Citation> = match r.citations {
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter_map(|c| serde_json::from_value(c).ok())
            .collect(),
        _ => vec![],
    };
    let card_ids: Vec<CardId> = match r.cards {
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter_map(|c| serde_json::from_value(c).ok())
            .collect(),
        _ => vec![],
    };
    let cited: Vec<&Citation> = citations.iter().collect();
    let names = names_for(pool, &card_ids, &cited).await?;
    let cards = card_ids
        .into_iter()
        .filter_map(|id| {
            names.card(id).map(|n| CardRef {
                id,
                name: n.to_owned(),
                status: crate::feed::stored_status(id, &r.unreleased_cards),
            })
        })
        .collect();
    let ratings = sqlx::query!(
        r#"
        SELECT user_id, rater_kind, web_name, score, is_judge, category, what_wrong, right_answer,
               to_char(ts AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "at!"
        FROM ratings WHERE call_id = $1 ORDER BY ts, user_id
        "#,
        id.into_inner(),
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("read a call's ratings"))?
    .into_iter()
    .filter_map(|r| {
        let user = r.user_id.clone();
        let row = || -> Result<RatingRow, JudgeError> {
        Ok(RatingRow {
            user: r.user_id,
            kind: RaterKind::parse(&r.rater_kind)
                .ok_or_else(|| bad_row(format!("ratings.rater_kind {:?}", r.rater_kind)))?,
            name: r.web_name,
            score: crate::feedback::score_id(r.score)?.to_owned(),
            judge: r.is_judge,
            at: r.at,
            category: r
                .category
                .as_deref()
                .map(|c| {
                    FeedbackCategory::parse(c)
                        .ok_or_else(|| bad_row(format!("ratings.category {c:?}")))
                })
                .transpose()?,
            wrong: r.what_wrong,
            right: r.right_answer,
        })
        };
        row()
            .inspect_err(|e| {
                if crate::unread::first(crate::unread::Site::AdminRating, id.into_inner()) {
                    tracing::warn!(call = %id.into_inner(), rater = %user, error = %format_args!("{e:#}"), "a rating does not read; left out of the call (logged once per call)");
                }
            })
            .ok()
    })
    .collect::<Vec<_>>();
    let runs = runs(pool, id).await?;
    Ok(Some((
        CallDetail {
            id: r.id,
            asked_at: r.asked_at,
            interface,
            asker: r.asker_name,
            asker_id: r.asker_id,
            private: r.private,
            thread: r.thread_id,
            locale: r.locale,
            session: r.session_id,
            question: r.question,
            answer: r.answer,
            confidence,
            source,
            category: r.category,
            cr_version: r.cr_version,
            citations,
            cards,
            retired: r.retired_reason,
            ratings,
            runs,
        },
        names,
    )))
}

/// The runs that left call `id`, each with its charges.
async fn runs(pool: &PgPool, id: CallId) -> Result<Vec<RunRow>, JudgeError> {
    let runs = sqlx::query!(
        r#"
        SELECT id,
               to_char(started_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "started_at!",
               (extract(epoch FROM finished_at - started_at) * 1000)::bigint AS "elapsed_ms!",
               outcome, retried, tool_round
        FROM judge_runs WHERE call_id = $1 ORDER BY started_at, id
        "#,
        id.into_inner(),
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("read a call's runs"))?;
    if runs.is_empty() {
        return Ok(vec![]);
    }
    let ids: Vec<Uuid> = runs.iter().map(|r| r.id).collect();
    let charges = sqlx::query!(
        r#"
        SELECT run_id, stage, provider, model, input_tokens, output_tokens,
               cache_read_tokens, cache_write_tokens, micro_usd, cost_estimated
        FROM judge_run_charges WHERE run_id = ANY($1) ORDER BY run_id, seq
        "#,
        &ids,
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("read a call's charges"))?;
    Ok(runs
        .into_iter()
        .map(|r| RunRow {
            charges: charges
                .iter()
                .filter(|c| c.run_id == r.id)
                .map(|c| ChargeRow {
                    stage: c.stage.clone(),
                    provider: c.provider.clone(),
                    model: c.model.clone(),
                    input_tokens: c.input_tokens,
                    output_tokens: c.output_tokens,
                    cache_read_tokens: c.cache_read_tokens,
                    cache_write_tokens: c.cache_write_tokens,
                    usd: usd(c.micro_usd),
                    estimated: c.cost_estimated,
                })
                .collect(),
            id: r.id,
            started_at: r.started_at,
            elapsed_ms: r.elapsed_ms,
            outcome: r.outcome,
            retried: r.retried,
            tool_round: r.tool_round,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn error(pairs: &[(&str, &str)]) -> String {
        AdminSettings::from_vars(vars(pairs))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default()
    }

    #[test]
    fn the_settings_default_to_loopback_with_no_token() -> anyhow::Result<()> {
        let s = AdminSettings::from_vars(vars(&[]))?;
        assert_eq!(s.addr.to_string(), DEFAULT_ADDR);
        assert!(s.token.is_none());
        let s = AdminSettings::from_vars(vars(&[(ADMIN_TOKEN_ENV, TOKEN), (ADMIN_ADDR_ENV, " ")]))?;
        assert_eq!(s.token.as_ref().map(AdminToken::expose), Some(TOKEN));
        assert!(!format!("{s:?}").contains(TOKEN), "the token reached Debug");
        Ok(())
    }

    #[test]
    fn a_short_odd_or_shared_token_is_refused_by_name() {
        for (pairs, needle) in [
            (
                vec![(ADMIN_TOKEN_ENV, "short")],
                "ADMIN_TOKEN must be at least 24",
            ),
            (
                vec![(ADMIN_TOKEN_ENV, "0123456789abcdef 0123456789abcdef")],
                "ADMIN_TOKEN must be printable ASCII",
            ),
            (
                vec![(ADMIN_TOKEN_ENV, TOKEN), (MCP_TOKEN_ENV, TOKEN)],
                "ADMIN_TOKEN must differ from MCP_TOKEN",
            ),
            (
                vec![(ADMIN_TOKEN_ENV, TOKEN), (MCP_ADMIN_TOKEN_ENV, TOKEN)],
                "ADMIN_TOKEN must differ from MCP_ADMIN_TOKEN",
            ),
            (
                vec![(ADMIN_ADDR_ENV, "nowhere")],
                "ADMIN_ADDR must be a socket address",
            ),
        ] {
            let text = error(&pairs);
            assert!(text.contains(needle), "{pairs:?}: {text}");
            assert!(!text.contains(TOKEN), "the value was quoted: {text}");
        }
    }

    #[test]
    fn the_mcp_admin_token_is_its_own() -> anyhow::Result<()> {
        assert_eq!(
            McpAdminToken::from_vars(vars(&[(MCP_ADMIN_TOKEN_ENV, " ")]))?,
            None
        );
        let ok = McpAdminToken::from_vars(vars(&[(MCP_ADMIN_TOKEN_ENV, TOKEN)]))?;
        assert_eq!(ok.as_ref().map(McpAdminToken::expose), Some(TOKEN));
        for (other, needle) in [
            (MCP_TOKEN_ENV, "MCP_ADMIN_TOKEN must differ from MCP_TOKEN"),
            (
                ADMIN_TOKEN_ENV,
                "MCP_ADMIN_TOKEN must differ from ADMIN_TOKEN",
            ),
        ] {
            let text =
                McpAdminToken::from_vars(vars(&[(MCP_ADMIN_TOKEN_ENV, TOKEN), (other, TOKEN)]))
                    .err()
                    .map(|e| format!("{e:#}"))
                    .unwrap_or_default();
            assert!(text.contains(needle), "{text}");
        }
        let short = McpAdminToken::from_vars(vars(&[(MCP_ADMIN_TOKEN_ENV, "short")]));
        assert!(short.is_err());
        Ok(())
    }

    #[test]
    fn the_page_url_carries_the_token_in_its_fragment_on_a_browsable_address() -> anyhow::Result<()>
    {
        let token = "ab+cd/ef=0123456789abcdefgh";
        let s = AdminSettings::from_vars(vars(&[
            (ADMIN_TOKEN_ENV, token),
            (ADMIN_ADDR_ENV, "0.0.0.0:8789"),
        ]))?;
        let t = s.token.clone().ok_or_else(|| anyhow::anyhow!("token"))?;
        assert_eq!(
            s.page_url(&t),
            "http://127.0.0.1:8789/admin#token=ab%2Bcd%2Fef%3D0123456789abcdefgh"
        );
        let s = AdminSettings::from_vars(vars(&[(ADMIN_ADDR_ENV, "[::]:9000")]))?;
        assert_eq!(s.browser_addr().to_string(), "[::1]:9000");
        // In the container, the host port compose publishes.
        let s = AdminSettings::from_vars(vars(&[
            (ADMIN_ADDR_ENV, COMPOSE_ADDR),
            (ADMIN_PORT_ENV, "9789"),
        ]))?;
        assert_eq!(s.browser_addr().to_string(), "127.0.0.1:9789");
        // A process outside a container listens where it says.
        let s = AdminSettings::from_vars(vars(&[
            (ADMIN_ADDR_ENV, "127.0.0.1:8789"),
            (ADMIN_PORT_ENV, "9789"),
        ]))?;
        assert_eq!(s.browser_addr().to_string(), "127.0.0.1:8789");
        for bad in ["0", "65536", "http"] {
            assert!(
                AdminSettings::from_vars(vars(&[(ADMIN_PORT_ENV, bad)]))
                    .is_err_and(|e| format!("{e:#}").contains("ADMIN_PORT")),
                "{bad}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_filter_is_bounded() {
        assert_eq!(FilterText::parse("  "), None);
        assert_eq!(FilterText::parse(&"x".repeat(MAX_FILTER_CHARS + 1)), None);
        assert_eq!(FilterText::parse("a\0b"), None);
        assert_eq!(
            FilterText::parse(" alice ").map(|f| f.as_str().to_owned()),
            Some("alice".to_owned())
        );
    }

    /// Store a call; its id.
    async fn stored_call(
        pool: &PgPool,
        secs_ago: i32,
        interface: &str,
        asker: Option<(&str, &str)>,
        private: bool,
        question: &str,
    ) -> anyhow::Result<Uuid> {
        Ok(sqlx::query_scalar(
            "INSERT INTO calls (thread_id, interface, asker_id, asker_name, private, question, \
             answer, category, source, confidence, citations, context_ids, cr_version, created_at) \
             VALUES ('t', $1, $2, $3, $4, $5, 'a.', 'combat', 'cr', 'high', \
                     '[{\"kind\":\"rule\",\"id\":\"702.19b\",\"quote\":\"Trample.\"}]', '{}', \
                     '20260819', now() - make_interval(secs => $6)) RETURNING id",
        )
        .bind(interface)
        .bind(asker.map(|a| a.0))
        .bind(asker.map(|a| a.1))
        .bind(private)
        .bind(question)
        .bind(f64::from(secs_ago))
        .fetch_one(pool)
        .await?)
    }

    /// A run that left `call`, billed to `model`.
    async fn run_for(pool: &PgPool, call: Uuid, model: &str) -> anyhow::Result<()> {
        let run: Uuid = sqlx::query_scalar(
            "INSERT INTO judge_runs (started_at, interface, private, outcome, retried, tool_round, \
             call_id) VALUES (now() - interval '2 seconds', 'discord', false, 'answered', true, \
             false, $1) RETURNING id",
        )
        .bind(call)
        .fetch_one(pool)
        .await?;
        for (seq, stage, m, micro) in [
            (0_i16, "extraction", model, 1_000_i64),
            (1, "synthesis", model, 40_000),
            (2, "embedding", "voyage-3.5", 10),
        ] {
            sqlx::query(
                "INSERT INTO judge_run_charges (run_id, seq, stage, provider, model, input_tokens, \
                 output_tokens, cache_read_tokens, cache_write_tokens, micro_usd, cost_estimated) \
                 VALUES ($1, $2, $3, 'anthropic', $4, 10, 5, 0, 0, $5, false)",
            )
            .bind(run)
            .bind(seq)
            .bind(stage)
            .bind(m)
            .bind(micro)
            .execute(pool)
            .await?;
        }
        Ok(())
    }

    fn ids(page: &CallsPage) -> Vec<Uuid> {
        page.calls.iter().map(|c| c.id).collect()
    }

    /// A call holding a value this build does not read (a newer build of
    /// the same major may write one) is left out of the list and answered
    /// as not found by id, rather than failing either; a rating that does
    /// not read is left out of its call, which still reads.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_unreadable_call_or_rating_is_left_out_not_fatal(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        // What a newer build's migration could relax.
        sqlx::query("ALTER TABLE ratings DROP CONSTRAINT ratings_category_check")
            .execute(&pool)
            .await?;
        let good = stored_call(&pool, 30, "web", None, false, "good").await?;
        let bad = stored_call(&pool, 20, "web", None, false, "bad").await?;
        sqlx::query("UPDATE calls SET confidence = 'certain' WHERE id = $1")
            .bind(bad)
            .execute(&pool)
            .await?;
        for (user, category) in [("7", "wrong_card"), ("8", "a_newer_category")] {
            sqlx::query(
                "INSERT INTO ratings (call_id, user_id, score, category) VALUES ($1, $2, 1, $3)",
            )
            .bind(good)
            .bind(user)
            .bind(category)
            .execute(&pool)
            .await?;
        }

        let list = calls(&pool, &CallFilter::default(), None).await?;
        assert_eq!(ids(&list), [good]);
        assert!(call(&pool, CallId::new(bad)).await?.is_none());
        let (detail, _) = call(&pool, CallId::new(good))
            .await?
            .ok_or_else(|| anyhow::anyhow!("the good call reads"))?;
        let raters: Vec<&str> = detail.ratings.iter().map(|r| r.user.as_str()).collect();
        assert_eq!(raters, ["7"]);
        Ok(())
    }

    /// The admin list reads private calls, every filter narrows it, and a
    /// call comes back whole with its runs, charges and raters.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_admin_reads_see_everything_and_filter(pool: PgPool) -> anyhow::Result<()> {
        let public = stored_call(&pool, 30, "web", None, false, "does trample stack?").await?;
        let private = stored_call(
            &pool,
            20,
            "discord",
            Some(("42", "alice")),
            true,
            "a private one",
        )
        .await?;
        let mcp = stored_call(&pool, 10, "mcp", None, true, "via mcp").await?;
        run_for(&pool, private, "claude-opus-5-5").await?;
        run_for(&pool, mcp, "claude-haiku-5-5").await?;
        sqlx::query(
            "INSERT INTO ratings (call_id, user_id, score, is_judge) VALUES ($1, '7', 1, true)",
        )
        .bind(private)
        .execute(&pool)
        .await?;

        let all = calls(&pool, &CallFilter::default(), None).await?;
        assert_eq!(ids(&all), [mcp, private, public], "private calls included");
        assert_eq!(all.models, ["claude-haiku-5-5", "claude-opus-5-5"]);
        let row = all.calls.get(1).ok_or_else(|| anyhow::anyhow!("row"))?;
        assert_eq!(
            row.models,
            ["claude-opus-5-5"],
            "embeddings are not a chat model"
        );
        assert!(
            row.usd.is_some_and(|u| (u - 0.041_01).abs() < 1e-9),
            "{:?}",
            row.usd
        );
        assert_eq!(
            all.calls.get(2).map(|r| r.usd),
            Some(None),
            "no run, no cost"
        );

        let only = |f: CallFilter| {
            let pool = pool.clone();
            async move { Ok::<_, anyhow::Error>(ids(&calls(&pool, &f, None).await?)) }
        };
        assert_eq!(
            only(CallFilter {
                private: Some(false),
                ..CallFilter::default()
            })
            .await?,
            [public]
        );
        assert_eq!(
            only(CallFilter {
                private: Some(true),
                ..CallFilter::default()
            })
            .await?,
            [mcp, private]
        );
        for asker in ["alice", "42"] {
            assert_eq!(
                only(CallFilter {
                    asker: FilterText::parse(asker),
                    ..CallFilter::default()
                })
                .await?,
                [private],
                "{asker}"
            );
        }
        assert_eq!(
            only(CallFilter {
                model: FilterText::parse("claude-haiku-5-5"),
                ..CallFilter::default()
            })
            .await?,
            [mcp]
        );
        assert_eq!(
            only(CallFilter {
                asked: Some(Asked::Web),
                ..CallFilter::default()
            })
            .await?,
            [public]
        );
        assert_eq!(
            only(CallFilter {
                search: Search::parse("PRIVATE"),
                ..CallFilter::default()
            })
            .await?,
            [private]
        );

        let Some((detail, _)) = call(&pool, CallId::new(private)).await? else {
            anyhow::bail!("the private call reads");
        };
        assert_eq!(
            (
                detail.asker.as_deref(),
                detail.asker_id.as_deref(),
                detail.private
            ),
            (Some("alice"), Some("42"), true)
        );
        assert_eq!(detail.citations.len(), 1);
        assert_eq!(detail.ratings.len(), 1);
        assert_eq!(
            detail
                .ratings
                .first()
                .map(|r| (r.user.as_str(), r.score.as_str(), r.judge)),
            Some(("7", "incorrect", true))
        );
        let [run] = detail.runs.as_slice() else {
            anyhow::bail!("one run: {:?}", detail.runs);
        };
        assert_eq!((run.outcome.as_str(), run.retried), ("answered", true));
        assert_eq!(run.charges.len(), 3);
        assert!(
            call(&pool, CallId::new(Uuid::from_u128(1)))
                .await?
                .is_none()
        );
        Ok(())
    }

    /// Paging returns every call once, private ones included.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn admin_pages_cover_every_call_once(pool: PgPool) -> anyhow::Result<()> {
        for i in 0..(PAGE_SIZE + 5) {
            let secs = i32::try_from(i / 2)?;
            stored_call(&pool, secs, "discord", None, i % 3 == 0, "q").await?;
        }
        let mut seen = vec![];
        let mut after = None;
        loop {
            let p = calls(&pool, &CallFilter::default(), after).await?;
            seen.extend(ids(&p));
            match p.next {
                Some(c) => after = Some(c.parse().map_err(|()| anyhow::anyhow!("cursor"))?),
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
}
