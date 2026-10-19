//! The public feed's endpoints (#26), served under `--web` unless
//! `JUDGE_FEED=false`:
//!
//! * `GET /api/feed?cursor&interface&user&locale`: a page of public calls,
//!   newest first, with the cursor of the next page.
//! * `GET /api/calls/{id}`: one public call. A private call, a call older
//!   than `JUDGE_FEED_DAYS` and a missing one are the same 404.
//!
//! What may be read is decided by `judge_bot::feed`, in SQL; this module
//! validates the query, rate-limits it per address (its own window, apart
//! from the questions'), and shapes the reply.

use std::{net::IpAddr, sync::Arc, time::Duration};

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse as _, Response},
};
use judge_bot::feed::{self, Asked, Cursor, Entry, Filter, Names, Page, Ratings};
use judge_bot::ingest::prune::Days;
use judge_bot::maintenance;
use judge_core::{CallId, Confidence, DiscordUsername, JudgeError, Locale, Source};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    catalog::{self, Messages},
    http::{App, PeerAddr, client_ip},
    shape::{self, ApiReply, CardView, CitationView},
};

/// Feed and call reads allowed per address per [`READ_WINDOW`]: a page
/// costs a database read, not a model call, so this is far looser than the
/// questions' limit, and only stops a scraper hammering the database.
pub const READ_LIMIT: u32 = 120;
/// The read limit's window.
pub const READ_WINDOW: Duration = Duration::from_mins(1);

/// The public reads, behind a trait so the routes are tested without a
/// database.
#[async_trait::async_trait]
pub trait PublicCalls: Send + Sync {
    /// A page of the feed.
    async fn page(&self, filter: &Filter, after: Option<Cursor>) -> Result<Page, JudgeError>;
    /// One public call, `None` when there is no public call `id`.
    async fn call(&self, id: CallId) -> Result<Option<(Entry, Names)>, JudgeError>;
}

/// [`PublicCalls`] over the database, within the feed window.
#[derive(Clone, Debug)]
pub struct PgPublicCalls {
    pool: PgPool,
    window: Option<Days>,
}

impl PgPublicCalls {
    /// Reads over `pool` that hide calls older than `window`.
    #[must_use]
    pub const fn new(pool: PgPool, window: Option<Days>) -> Self {
        Self { pool, window }
    }
}

#[async_trait::async_trait]
impl PublicCalls for PgPublicCalls {
    async fn page(&self, filter: &Filter, after: Option<Cursor>) -> Result<Page, JudgeError> {
        feed::page(&self.pool, self.window, filter, after).await
    }

    async fn call(&self, id: CallId) -> Result<Option<(Entry, Names)>, JudgeError> {
        feed::call(&self.pool, self.window, id).await
    }
}

/// `GET /api/feed`'s query.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FeedQuery {
    /// The `next` of the previous page.
    pub cursor: Option<String>,
    /// Only calls asked through this interface (`discord`, `web`, `mcp`,
    /// `cli`, `agent_session`, or `agent` for MCP and `judge-cli` before
    /// 2.0.0).
    pub interface: Option<String>,
    /// Only calls asked by this Discord username.
    pub user: Option<String>,
    /// The language for the labels and messages, ahead of `Accept-Language`.
    pub locale: Option<String>,
}

/// `GET /api/calls/{id}`'s query.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CallQuery {
    /// As [`FeedQuery::locale`].
    pub locale: Option<String>,
}

/// A page of the feed, as sent.
#[derive(Clone, Debug, Serialize)]
pub struct FeedView {
    /// Newest first.
    pub entries: Vec<EntryView>,
    /// Pass as `cursor` for the next page; `null` on the last.
    pub next: Option<String>,
}

/// One public call, as sent.
#[derive(Clone, Debug, Serialize)]
pub struct EntryView {
    /// The call's id: its page is `/calls/<id>`.
    pub id: Uuid,
    /// When it was asked, RFC 3339 UTC.
    pub asked_at: String,
    /// The interface it was asked through.
    pub interface: Asked,
    /// The asker's Discord username; `null` is anonymous.
    pub asker: Option<String>,
    /// The question as asked.
    pub question: String,
    /// The answer.
    pub answer: String,
    /// The model's confidence.
    pub confidence: Confidence,
    /// The rules body (`cr` / `commander`).
    pub source: Source,
    /// The CR it was answered under, `YYYYMMDD`.
    pub cr_version: String,
    /// Its citations, linked as an answer's are.
    pub citations: Vec<CitationView>,
    /// The cards the question was taken to be about.
    pub cards: Vec<CardView>,
    /// How it was rated.
    pub ratings: Ratings,
    /// Why it no longer serves as an example, when its citations stopped
    /// holding (the retirement pass); `null` while they hold.
    pub retired: Option<String>,
}

/// Shape `entry`, its labels in `m`.
#[must_use]
pub fn entry_view(m: &Messages, entry: Entry, names: &Names) -> EntryView {
    EntryView {
        id: entry.id.into_inner(),
        asked_at: entry.asked_at,
        interface: entry.asked,
        asker: entry.asker,
        question: entry.question,
        answer: entry.answer,
        confidence: entry.confidence,
        source: entry.source,
        cr_version: entry.cr_version,
        citations: entry
            .citations
            .iter()
            .map(|c| shape::citation_view(m, c, names))
            .collect(),
        cards: entry.cards.iter().map(shape::card_view).collect(),
        ratings: entry.ratings,
        retired: entry.retired,
    }
}

/// A JSON error reply.
fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiReply::Error {
            message: message.into(),
        }),
    )
        .into_response()
}

/// The language, the maintenance check and the read limit every public
/// read starts with: `Ok` is the reads and the language, `Err` the reply.
fn admit(
    app: &App,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    locale: Option<&str>,
) -> Result<(Arc<dyn PublicCalls>, Locale), Response> {
    let locale = shape::request_locale(
        locale,
        headers
            .get(axum::http::header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
        app.locale(),
    );
    let m = catalog::messages(locale);
    let Some(calls) = app.public_calls() else {
        // Unrouted when there is no reader; kept for completeness.
        return Err(error(StatusCode::NOT_FOUND, m.not_routed));
    };
    // The schema may be the old major's while a major upgrade migrates.
    if !app.is_open() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            [(
                axum::http::header::RETRY_AFTER,
                maintenance::RETRY_AFTER.as_secs().to_string(),
            )],
            Json(ApiReply::Maintenance {
                message: m.maintenance.to_owned(),
                retry_after_secs: maintenance::RETRY_AFTER.as_secs(),
            }),
        )
            .into_response());
    }
    let ip: IpAddr = client_ip(headers, peer, app.client_ip_source());
    if !app.allow_read(ip) {
        tracing::info!(%ip, "feed reads rate limited");
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(ApiReply::RateLimited {
                message: m.rate_limited.to_owned(),
            }),
        )
            .into_response());
    }
    Ok((calls, locale))
}

/// The filter and cursor a feed query names.
///
/// # Errors
/// The message for a 400.
pub fn parse_query(m: &Messages, q: &FeedQuery) -> Result<(Filter, Option<Cursor>), String> {
    let blank = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let after = match blank(&q.cursor) {
        None => None,
        Some(c) => Some(c.parse::<Cursor>().map_err(|()| m.bad_cursor.to_owned())?),
    };
    let asked = match blank(&q.interface) {
        None => None,
        Some(i) => Some(
            Asked::parse(&i)
                .ok_or_else(|| (m.bad_interface)(&Asked::ALL.map(Asked::id).join(", ")))?,
        ),
    };
    let user = match blank(&q.user) {
        None => None,
        Some(u) => Some(
            DiscordUsername::try_new(u.trim_start_matches('@').to_owned())
                .map_err(|_| m.bad_user.to_owned())?,
        ),
    };
    Ok((Filter { asked, user }, after))
}

/// `GET /api/feed`.
pub(crate) async fn feed_route(
    State(app): State<Arc<App>>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    query: Result<Query<FeedQuery>, QueryRejection>,
) -> Response {
    let q = query.map(|Query(q)| q);
    let (calls, locale) = match admit(
        &app,
        &headers,
        peer,
        q.as_ref().ok().and_then(|q| q.locale.as_deref()),
    ) {
        Ok(ok) => ok,
        Err(reply) => return reply,
    };
    let m = catalog::messages(locale);
    let q = match q {
        Ok(q) => q,
        Err(e) => return error(e.status(), e.body_text()),
    };
    let (filter, after) = match parse_query(m, &q) {
        Ok(ok) => ok,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    match calls.page(&filter, after).await {
        Ok(page) => {
            let Page {
                entries,
                next,
                names,
            } = page;
            Json(FeedView {
                entries: entries
                    .into_iter()
                    .map(|e| entry_view(m, e, &names))
                    .collect(),
                next: next.map(|c| c.to_string()),
            })
            .into_response()
        }
        Err(e) => {
            tracing::warn!(error = format_args!("{e:#}"), "GET /api/feed failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, m.feed_failed)
        }
    }
}

/// `GET /api/calls/{id}`.
pub(crate) async fn call_route(
    State(app): State<Arc<App>>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<CallQuery>, QueryRejection>,
) -> Response {
    let locale = query.as_ref().ok().and_then(|Query(q)| q.locale.clone());
    let (calls, locale) = match admit(&app, &headers, peer, locale.as_deref()) {
        Ok(ok) => ok,
        Err(reply) => return reply,
    };
    let m = catalog::messages(locale);
    // Not a call id: not a call, the same 404 as any other.
    let Ok(id) = Uuid::try_parse(&id) else {
        return error(StatusCode::NOT_FOUND, m.call_not_found);
    };
    match calls.call(CallId::new(id)).await {
        Ok(Some((entry, names))) => Json(entry_view(m, entry, &names)).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, m.call_not_found),
        Err(e) => {
            tracing::warn!(error = format_args!("{e:#}"), "GET /api/calls failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, m.feed_failed)
        }
    }
}
