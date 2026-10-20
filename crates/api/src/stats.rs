//! The public dashboard's endpoint (#28), served beside the feed: under
//! `--web` unless `JUDGE_FEED=false`, when it is not routed at all (the
//! JSON 404 of any unserved `/api/` path).
//!
//! * `GET /api/stats?from&to&days&interface&user&model&locale`: counts of
//!   the public calls and runs in a range of UTC days (`judge_bot::db::stats`,
//!   [`Dashboard`]). Cost appears only with `JUDGE_PUBLIC_COST=true`, models
//!   only with `JUDGE_PUBLIC_MODELS=true`; a `model` filter where models are
//!   not shown is refused (400), never silently ignored.
//!
//! What is counted is decided by `judge_bot::db::stats::public`, in SQL;
//! this module validates the query, rate-limits it per address (its own
//! window: an aggregate is dearer than a feed page), and sends the report.

use std::{net::IpAddr, sync::Arc, time::Duration};

use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse as _, Response},
};
use judge_bot::admin::{FilterText, MAX_FILTER_CHARS};
use judge_bot::db::stats::{
    self, BadRange, Dashboard, Date, Disclosure, ModelFilter, PublicQuery, Span, StatsError,
};
use judge_bot::feed::Asked;
use judge_bot::ingest::prune::Days;
use judge_bot::maintenance;
use judge_core::DiscordUsername;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::{
    catalog::{self, Messages},
    http::{App, PeerAddr, client_ip},
    shape::{self, ApiReply},
};

/// Dashboard reads allowed per address per [`READ_WINDOW`]: each is a few
/// aggregate queries over up to a year of calls, so fewer than the feed's
/// pages; a person changing filters stays far under it.
pub const READ_LIMIT: u32 = 30;
/// The read limit's window.
pub const READ_WINDOW: Duration = Duration::from_mins(1);
/// Dashboard reads running at once in one process, whoever asks: each is a
/// few aggregate queries, so this bounds the database's work however many
/// addresses ask.
pub const CONCURRENCY: usize = 2;
/// How long a read waits for a slot before the busy reply.
pub const SLOT_WAIT: Duration = Duration::from_secs(5);

/// The public dashboard, behind a trait so the routes are tested without a
/// database.
#[async_trait::async_trait]
pub trait PublicStats: Send + Sync {
    /// What it shows beyond the counts.
    fn disclosure(&self) -> Disclosure;
    /// The report for `q`.
    async fn dashboard(&self, q: &PublicQuery) -> Result<Dashboard, StatsError>;
}

/// [`PublicStats`] over the database: public calls within the feed window.
#[derive(Clone, Debug)]
pub struct PgPublicStats {
    pool: PgPool,
    window: Option<Days>,
    disclosure: Disclosure,
}

impl PgPublicStats {
    /// Reports over `pool` that count no call older than `window`, with cost
    /// and models as `disclosure` allows.
    #[must_use]
    pub const fn new(pool: PgPool, window: Option<Days>, disclosure: Disclosure) -> Self {
        Self {
            pool,
            window,
            disclosure,
        }
    }
}

#[async_trait::async_trait]
impl PublicStats for PgPublicStats {
    fn disclosure(&self) -> Disclosure {
        self.disclosure
    }

    async fn dashboard(&self, q: &PublicQuery) -> Result<Dashboard, StatsError> {
        stats::public(&self.pool, self.window, self.disclosure, q).await
    }
}

/// `GET /api/stats`'s query.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct StatsQuery {
    /// The first day, `YYYY-MM-DD` (UTC).
    pub from: Option<String>,
    /// The last day (default today).
    pub to: Option<String>,
    /// Days ending on the last, when there is no first (default 30).
    pub days: Option<String>,
    /// Only calls through this interface.
    pub interface: Option<String>,
    /// Only calls by this Discord username.
    pub user: Option<String>,
    /// Only calls whose run billed this model: only where models are shown.
    pub model: Option<String>,
    /// The language for the messages, ahead of `Accept-Language`.
    pub locale: Option<String>,
}

/// What `GET /api/about` says of the dashboard, when it is served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct StatsAbout {
    /// Whether it carries cost figures (`JUDGE_PUBLIC_COST`).
    pub cost: bool,
    /// Whether it carries model figures and takes a `model` filter
    /// (`JUDGE_PUBLIC_MODELS`).
    pub models: bool,
}

impl StatsAbout {
    /// What `disclosure` shows.
    #[must_use]
    pub const fn of(disclosure: Disclosure) -> Self {
        Self {
            cost: disclosure.cost().is_some(),
            models: disclosure.models().is_some(),
        }
    }
}

/// The days a query names, as the operator's report takes them too.
///
/// # Errors
/// The message for a 400.
pub fn parse_span(
    m: &Messages,
    from: Option<&str>,
    to: Option<&str>,
    days: Option<&str>,
) -> Result<Span, String> {
    let day = |name: &str, v: Option<&str>| {
        v.map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| Date::parse(v).ok_or_else(|| (m.bad_date)(name)))
            .transpose()
    };
    let days = days
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| {
            v.parse::<u32>()
                .ok()
                .filter(|d| (1..=stats::MAX_DAYS).contains(d))
                .ok_or_else(|| (m.bad_range)(stats::MAX_DAYS))
        })
        .transpose()?;
    Ok(Span {
        from: day("from", from)?,
        to: day("to", to)?,
        days,
    })
}

/// The report a query names, `disclosure` deciding whether it may filter
/// by model.
///
/// # Errors
/// The message for a 400.
pub fn parse_query(
    m: &Messages,
    q: &StatsQuery,
    disclosure: Disclosure,
) -> Result<PublicQuery, String> {
    let blank = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
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
            // The type drops one leading `@` itself.
            DiscordUsername::try_new(u).map_err(|_| m.bad_user.to_owned())?,
        ),
    };
    let model = match blank(&q.model) {
        None => None,
        Some(name) => {
            // Refused rather than ignored: an ignored filter would answer
            // with every model's figures under the name of one.
            let access = disclosure
                .models()
                .ok_or_else(|| m.model_not_public.to_owned())?;
            let name = FilterText::parse(&name)
                .ok_or_else(|| (m.bad_filter)("model", MAX_FILTER_CHARS))?;
            Some(ModelFilter::new(access, name))
        }
    };
    Ok(PublicQuery {
        span: parse_span(m, q.from.as_deref(), q.to.as_deref(), q.days.as_deref())?,
        asked,
        user,
        model,
    })
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

/// `GET /api/stats`.
pub(crate) async fn stats_route(
    State(app): State<Arc<App>>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    query: Result<Query<StatsQuery>, QueryRejection>,
) -> Response {
    let q = query.map(|Query(q)| q);
    let locale = shape::request_locale(
        q.as_ref().ok().and_then(|q| q.locale.as_deref()),
        headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
        app.locale(),
    );
    let m = catalog::messages(locale);
    let Some(reads) = app.public_stats() else {
        return error(StatusCode::NOT_FOUND, m.not_routed);
    };
    // The schema may be the old major's while a major upgrade migrates.
    if !app.is_open() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(
                header::RETRY_AFTER,
                maintenance::RETRY_AFTER.as_secs().to_string(),
            )],
            Json(ApiReply::Maintenance {
                message: m.maintenance.to_owned(),
                retry_after_secs: maintenance::RETRY_AFTER.as_secs(),
            }),
        )
            .into_response();
    }
    let ip: IpAddr = client_ip(&headers, peer, app.client_ip_source());
    if !app.allow_stats(ip) {
        tracing::info!(%ip, "dashboard reads rate limited");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ApiReply::RateLimited {
                message: m.rate_limited.to_owned(),
            }),
        )
            .into_response();
    }
    let q = match q {
        Ok(q) => q,
        Err(e) => return error(e.status(), e.body_text()),
    };
    let query = match parse_query(m, &q, reads.disclosure()) {
        Ok(q) => q,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let Some(_slot) = app.stats_slot().await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, SLOT_WAIT.as_secs().to_string())],
            Json(ApiReply::Busy {
                message: m.stats_busy.to_owned(),
            }),
        )
            .into_response();
    };
    match reads.dashboard(&query).await {
        Ok(d) => Json(d).into_response(),
        Err(StatsError::Range(BadRange::BeforeWindow { days })) => {
            error(StatusCode::BAD_REQUEST, (m.before_window)(days))
        }
        Err(StatsError::Range(_)) => error(StatusCode::BAD_REQUEST, (m.bad_range)(stats::MAX_DAYS)),
        // Not a database failure: the report is too large for the public
        // limit, so it says to ask for less.
        Err(StatsError::TimedOut) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiReply::Busy {
                message: m.stats_too_slow.to_owned(),
            }),
        )
            .into_response(),
        Err(StatsError::Store(e)) => {
            tracing::warn!(error = format_args!("{e:#}"), "GET /api/stats failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, m.stats_failed)
        }
    }
}
