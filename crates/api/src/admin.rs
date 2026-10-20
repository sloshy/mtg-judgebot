//! The `--admin` role (#24, D37): the operator's view of the instance on a
//! listener of its own, `ADMIN_ADDR` (default `127.0.0.1:8789`).
//!
//! * `/admin/api/*`: the reads, behind `Authorization: Bearer <ADMIN_TOKEN>`
//!   compared in constant time. They exist on this listener only: the
//!   network roles' listener answers the same paths with a 404.
//!   - `GET /admin/api/calls?cursor&interface&asker&model&private&q`: every
//!     stored call, private ones included, newest first.
//!   - `GET /admin/api/calls/{id}`: one call with its runs, charges and
//!     raters.
//!   - `GET /admin/api/failures?limit`: the failed calls.
//!   - `GET /admin/api/usage?days`: usage and spend.
//!   - `GET /admin/api/queue?cursor&status&category&interface&rater`: the
//!     feedback queue (#27, D38), newest first.
//!   - `GET /admin/api/queue/{id}`: one item with its call, whole.
//!   - `POST /admin/api/queue/{id}/resolve` and `…/dismiss`, body
//!     `{"note", "fixed_by"}`: close an item. A write, behind the same
//!     bearer: the token travels in a header, never a cookie, so another
//!     site's page cannot send it.
//! * `/admin` and the pages under it: the web app (`WEB_DIST`), which reads
//!   the token from the address's `#token=` fragment. `/` redirects there;
//!   the page's assets are served, nothing else.
//!
//! The listener takes the network roles' handling: reserved at startup (a
//! taken address fails before anything else starts), listening once the
//! instance is open (or after [`crate::network::LISTEN_WAIT`]), so during a
//! major upgrade's migration the proxy's dial goes on to the previous
//! container, and drained on the process's shutdown like the main one.

use std::{
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path as UrlPath, Query, Request, State, rejection::QueryRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse as _, Redirect, Response},
    routing::{any, get, post},
};
use judge_bot::{
    admin::{AdminSettings, AdminToken, CallDetail, CallFilter, CallsPage, FilterText, RunRow},
    db::stats::{self, FailedRow, Usage},
    feed::{self, Asked, Cursor, Names},
    feedback::{QueueDetail, QueueFilter, QueueItem, QueuePage},
    maintenance::{self, Availability},
    shutdown::Shutdown,
};
use judge_core::{
    CallId, Close, Closing, Confidence, FeedbackCategory, FixedBy, JudgeError, Locale, QueueNote,
    QueueStatus, RaterKind, Source,
};
use nonempty::NonEmpty;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

use crate::{
    ApiConfig,
    auth::{constant_time_eq, presented_bearer},
    catalog::{self, Messages},
    http,
    network::LISTEN_WAIT,
    serve_on,
    shape::{self, ApiReply, CardView, CitationView},
};

/// What the `--admin` role reads from the environment.
#[derive(Clone, Debug)]
pub struct AdminConfig {
    /// `ADMIN_ADDR` and `ADMIN_TOKEN`.
    pub settings: AdminSettings,
    /// `WEB_DIST`: the built page, which holds the admin pages too.
    pub web_dist: PathBuf,
}

impl AdminConfig {
    /// Read `ADMIN_ADDR`, `ADMIN_TOKEN` and `WEB_DIST` (blank counts as
    /// unset).
    ///
    /// # Errors
    /// As [`AdminSettings::from_vars`].
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let settings = AdminSettings::from_vars(&get)?;
        let web_dist = get("WEB_DIST")
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| ApiConfig::DEFAULT_WEB_DIST.to_owned());
        Ok(Self {
            settings,
            web_dist: PathBuf::from(web_dist),
        })
    }
}

/// One requirement of `--admin` the environment does not meet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdminUnmet {
    /// No `ADMIN_TOKEN`.
    Token,
    /// No built page where `WEB_DIST` points.
    WebBuild {
        /// The `index.html` that is missing.
        index: PathBuf,
    },
}

impl fmt::Display for AdminUnmet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Token => write!(
                f,
                "--admin was given but ADMIN_TOKEN is not set. The admin pages read every stored \
                 call, private ones included, so there is no anonymous mode: set ADMIN_TOKEN \
                 (`openssl rand -base64 32`, at least {} characters, not MCP_TOKEN) or drop \
                 --admin.",
                judge_bot::admin::MIN_TOKEN_BYTES
            ),
            Self::WebBuild { index } => write!(
                f,
                "--admin was given but {} does not exist. Build the page (`npm --prefix web run \
                 build`) or point WEB_DIST at the built directory.",
                index.display()
            ),
        }
    }
}

/// The `--admin` role with everything it requires: a token and a built
/// page. Made only by [`Admin::new`].
#[derive(Debug)]
pub struct Admin {
    addr: SocketAddr,
    token: AdminToken,
    web_dist: PathBuf,
}

impl Admin {
    /// Check `cfg`: every requirement unmet comes back, not the first.
    ///
    /// # Errors
    /// No `ADMIN_TOKEN`, no `index.html` in `WEB_DIST`, or both.
    pub fn new(cfg: AdminConfig) -> Result<Self, NonEmpty<AdminUnmet>> {
        let mut unmet = vec![];
        let index = cfg.web_dist.join("index.html");
        if !index.is_file() {
            unmet.push(AdminUnmet::WebBuild { index });
        }
        match (cfg.settings.token, NonEmpty::from_vec(unmet)) {
            (Some(token), None) => Ok(Self {
                addr: cfg.settings.addr,
                token,
                web_dist: cfg.web_dist,
            }),
            (Some(_), Some(unmet)) => Err(unmet),
            (None, None) => Err(NonEmpty::new(AdminUnmet::Token)),
            (None, Some(unmet)) => {
                let mut all = NonEmpty::new(AdminUnmet::Token);
                all.extend(unmet);
                Err(all)
            }
        }
    }

    /// Where it listens.
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Build the routes over `pool` and reserve `ADMIN_ADDR`. Answers in
    /// `locale` when a caller's own language has no catalog.
    ///
    /// # Errors
    /// Binding the address.
    pub fn reserve(
        self,
        pool: &PgPool,
        availability: Availability,
        shutdown: Shutdown,
        locale: Locale,
    ) -> Result<AdminListening> {
        let reads = Arc::new(PgAdminReads(pool.clone()));
        let routes = router(
            reads,
            &self.token,
            &self.web_dist,
            availability.clone(),
            locale,
        );
        let socket = http::reserve(self.addr)?;
        tracing::info!(
            addr = %self.addr,
            web_dist = %self.web_dist.display(),
            "admin listener bound; it listens once the instance is open"
        );
        Ok(AdminListening {
            socket,
            routes,
            shutdown,
            availability,
        })
    }
}

/// The admin listener, its address reserved, ready to serve.
pub struct AdminListening {
    socket: tokio::net::TcpSocket,
    routes: Router,
    shutdown: Shutdown,
    availability: Availability,
}

impl AdminListening {
    /// Listen once the instance is open (or after [`LISTEN_WAIT`]), then
    /// serve until the accept loop fails, or until the process's shutdown is
    /// requested and the requests in flight are answered ([`serve_on`]).
    ///
    /// # Errors
    /// A fatal accept-loop error.
    pub async fn serve(self) -> Result<()> {
        let Self {
            socket,
            routes,
            shutdown,
            availability,
        } = self;
        tokio::select! {
            () = availability.opened() => {}
            () = tokio::time::sleep(LISTEN_WAIT) => tracing::info!(
                wait_secs = LISTEN_WAIT.as_secs(),
                "the instance is still migrating; the admin listener listens anyway"
            ),
            // Nothing was accepted, so nothing to drain.
            () = shutdown.requested() => return Ok(()),
        }
        let listener = http::listen(socket)?;
        serve_on(listener, routes, &shutdown).await
    }
}

/// The operator's reads, and the feedback queue's one write (closing an
/// item), behind a trait so the routes are tested without a database.
#[async_trait::async_trait]
pub trait AdminReads: Send + Sync {
    /// A page of every stored call.
    async fn calls(
        &self,
        filter: &CallFilter,
        after: Option<Cursor>,
    ) -> Result<CallsPage, JudgeError>;
    /// One call, whole; `None` when there is none with that id.
    async fn call(&self, id: CallId) -> Result<Option<(CallDetail, Names)>, JudgeError>;
    /// The newest failed calls.
    async fn failures(&self, limit: u32) -> Result<Vec<FailedRow>, JudgeError>;
    /// Usage over the last `days`.
    async fn usage(&self, days: u32) -> Result<Usage, JudgeError>;
    /// A page of the feedback queue.
    async fn queue(
        &self,
        filter: QueueFilter,
        after: Option<Cursor>,
    ) -> Result<QueuePage, JudgeError>;
    /// One feedback item with its call; `None` when there is none.
    async fn item(&self, id: Uuid) -> Result<Option<(QueueDetail, Names)>, JudgeError>;
    /// Close a feedback item; `None` when there is none. The one write.
    async fn close(&self, id: Uuid, close: &Close) -> Result<Option<QueueItem>, JudgeError>;
}

/// [`AdminReads`] over the database.
#[derive(Clone, Debug)]
pub struct PgAdminReads(pub PgPool);

#[async_trait::async_trait]
impl AdminReads for PgAdminReads {
    async fn calls(
        &self,
        filter: &CallFilter,
        after: Option<Cursor>,
    ) -> Result<CallsPage, JudgeError> {
        judge_bot::admin::calls(&self.0, filter, after).await
    }

    async fn call(&self, id: CallId) -> Result<Option<(CallDetail, Names)>, JudgeError> {
        judge_bot::admin::call(&self.0, id).await
    }

    async fn failures(&self, limit: u32) -> Result<Vec<FailedRow>, JudgeError> {
        stats::failures(&self.0, limit).await
    }

    async fn usage(&self, days: u32) -> Result<Usage, JudgeError> {
        stats::usage(&self.0, days).await
    }

    async fn queue(
        &self,
        filter: QueueFilter,
        after: Option<Cursor>,
    ) -> Result<QueuePage, JudgeError> {
        judge_bot::feedback::list(&self.0, filter, after).await
    }

    async fn item(&self, id: Uuid) -> Result<Option<(QueueDetail, Names)>, JudgeError> {
        judge_bot::feedback::show(&self.0, id).await
    }

    async fn close(&self, id: Uuid, close: &Close) -> Result<Option<QueueItem>, JudgeError> {
        judge_bot::feedback::close(&self.0, id, close).await
    }
}

/// What every admin route shares.
struct AdminState {
    reads: Arc<dyn AdminReads>,
    availability: Availability,
    locale: Locale,
}

/// The token, kept as bytes for the comparison.
#[derive(Clone)]
struct Token(Arc<[u8]>);

/// The admin listener's routes over `reads`, the API behind `token`.
pub fn router(
    reads: Arc<dyn AdminReads>,
    token: &AdminToken,
    web_dist: &Path,
    availability: Availability,
    locale: Locale,
) -> Router {
    let state = Arc::new(AdminState {
        reads,
        availability,
        locale,
    });
    let token = Token(token.expose().as_bytes().into());
    let api = Router::new()
        .route("/admin/api/calls", get(calls_route))
        .route("/admin/api/calls/{id}", get(call_route))
        .route("/admin/api/failures", get(failures_route))
        .route("/admin/api/usage", get(usage_route))
        .route("/admin/api/queue", get(queue_route))
        .route("/admin/api/queue/{id}", get(item_route))
        .route("/admin/api/queue/{id}/resolve", post(resolve_route))
        .route("/admin/api/queue/{id}/dismiss", post(dismiss_route))
        .route("/admin/api/{*rest}", any(not_routed))
        .route("/admin/api", any(not_routed))
        .route("/admin/api/", any(not_routed))
        // `route_layer`: the gate runs where a route matched (every path
        // under `/admin/api`), never as the fallback.
        .route_layer(middleware::from_fn_with_state(
            (token, Arc::clone(&state)),
            require_admin,
        ))
        .with_state(state);
    let page = ServeFile::new(web_dist.join("index.html"));
    Router::new()
        .merge(api)
        .route(
            "/",
            get(|| async { Redirect::to(judge_bot::admin::PAGE_PATH) }),
        )
        .route_service("/admin", page.clone())
        .route_service("/admin/", page.clone())
        .route_service("/admin/{*rest}", page)
        // The page's own files (scripts, styles, icons); no index fallback,
        // so any other path is a 404.
        .fallback_service(ServeDir::new(web_dist))
}

/// The bearer gate, and `Cache-Control: no-store` on what it lets through:
/// the replies hold private material a shared cache must never keep.
async fn require_admin(
    State((token, state)): State<(Token, Arc<AdminState>)>,
    req: Request,
    next: Next,
) -> Response {
    let ok =
        presented_bearer(req.headers()).is_some_and(|p| constant_time_eq(p.as_bytes(), &token.0));
    if !ok {
        let m = messages(&state, req.headers());
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"admin\"")],
            Json(ApiReply::Error {
                message: m.admin_unauthorized.to_owned(),
            }),
        )
            .into_response();
    }
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// The caller's catalog: `Accept-Language`, then the instance's.
fn messages(state: &AdminState, headers: &HeaderMap) -> &'static Messages {
    catalog::messages(shape::request_locale(
        None,
        headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
        state.locale,
    ))
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiReply::Error {
            message: message.into(),
        }),
    )
        .into_response()
}

/// The maintenance reply while a major upgrade migrates: the schema may be
/// the old major's.
fn closed(state: &AdminState, m: &Messages) -> Option<Response> {
    (!state.availability.is_open()).then(|| {
        (
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
            .into_response()
    })
}

fn failed(m: &Messages, what: &str, e: &JudgeError) -> Response {
    tracing::warn!(error = format_args!("{e:#}"), "{what} failed");
    error(StatusCode::INTERNAL_SERVER_ERROR, m.admin_failed)
}

async fn not_routed(State(state): State<Arc<AdminState>>, headers: HeaderMap) -> Response {
    error(StatusCode::NOT_FOUND, messages(&state, &headers).not_routed)
}

/// `GET /admin/api/calls`'s query.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CallsQuery {
    /// The `next` of the previous page.
    pub cursor: Option<String>,
    /// Only calls asked through this interface.
    pub interface: Option<String>,
    /// Only calls whose asker has this Discord username or user id.
    pub asker: Option<String>,
    /// Only calls whose run billed this model.
    pub model: Option<String>,
    /// `true`: only private calls; `false`: only public ones.
    pub private: Option<String>,
    /// Text in the question, the answer or the asker's username.
    pub q: Option<String>,
}

/// The filter and cursor a query names.
///
/// # Errors
/// The message for a 400.
pub fn parse_calls_query(
    m: &Messages,
    q: &CallsQuery,
) -> Result<(CallFilter, Option<Cursor>), String> {
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
    let text = |v: &Option<String>, name: &str| match blank(v) {
        None => Ok(None),
        Some(t) => FilterText::parse(&t)
            .map(Some)
            .ok_or_else(|| (m.bad_filter)(name, judge_bot::admin::MAX_FILTER_CHARS)),
    };
    let private = match blank(&q.private).as_deref() {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(_) => return Err(m.bad_private.to_owned()),
    };
    let search = match blank(&q.q) {
        None => None,
        Some(t) => {
            Some(feed::Search::parse(&t).ok_or_else(|| (m.bad_search)(feed::MAX_SEARCH_CHARS))?)
        }
    };
    Ok((
        CallFilter {
            asked,
            asker: text(&q.asker, "asker")?,
            model: text(&q.model, "model")?,
            private,
            search,
        },
        after,
    ))
}

async fn calls_route(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    query: Result<Query<CallsQuery>, QueryRejection>,
) -> Response {
    let m = messages(&state, &headers);
    if let Some(reply) = closed(&state, m) {
        return reply;
    }
    let q = match query {
        Ok(Query(q)) => q,
        Err(e) => return error(e.status(), e.body_text()),
    };
    let (filter, after) = match parse_calls_query(m, &q) {
        Ok(ok) => ok,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    match state.reads.calls(&filter, after).await {
        Ok(page) => Json(page).into_response(),
        Err(e) => failed(m, "GET /admin/api/calls", &e),
    }
}

/// One call as the admin page shows it: [`CallDetail`] with its citations
/// labelled and linked as an answer's are.
#[derive(Clone, Debug, Serialize)]
pub struct CallView {
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
    /// The thread it was asked in.
    pub thread: String,
    /// The language it was asked in.
    pub locale: String,
    /// The agent session that stored it.
    pub session: Option<Uuid>,
    /// The question, verbatim.
    pub question: String,
    /// The answer, verbatim.
    pub answer: String,
    /// The model's confidence.
    pub confidence: Confidence,
    /// The rules body.
    pub source: Source,
    /// The category id.
    pub category: String,
    /// The CR it was answered under.
    pub cr_version: String,
    /// Its citations.
    pub citations: Vec<CitationView>,
    /// The cards it was about.
    pub cards: Vec<CardView>,
    /// Why it was retired, when it was.
    pub retired: Option<String>,
    /// Every rating with its rater.
    pub ratings: Vec<judge_bot::admin::RatingRow>,
    /// The runs that left it, with their charges.
    pub runs: Vec<RunRow>,
}

/// Shape `call`, its labels in `m`.
#[must_use]
pub fn call_view(m: &Messages, call: CallDetail, names: &Names) -> CallView {
    CallView {
        citations: call
            .citations
            .iter()
            .map(|c| shape::citation_view(m, c, names))
            .collect(),
        cards: call.cards.iter().map(shape::card_view).collect(),
        id: call.id,
        asked_at: call.asked_at,
        interface: call.interface,
        asker: call.asker,
        asker_id: call.asker_id,
        private: call.private,
        thread: call.thread,
        locale: call.locale,
        session: call.session,
        question: call.question,
        answer: call.answer,
        confidence: call.confidence,
        source: call.source,
        category: call.category,
        cr_version: call.cr_version,
        retired: call.retired,
        ratings: call.ratings,
        runs: call.runs,
    }
}

async fn call_route(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let m = messages(&state, &headers);
    if let Some(reply) = closed(&state, m) {
        return reply;
    }
    let Ok(id) = Uuid::try_parse(&id) else {
        return error(StatusCode::NOT_FOUND, m.admin_call_not_found);
    };
    match state.reads.call(CallId::new(id)).await {
        Ok(Some((call, names))) => Json(call_view(m, call, &names)).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, m.admin_call_not_found),
        Err(e) => failed(m, "GET /admin/api/calls/{id}", &e),
    }
}

/// `GET /admin/api/failures`'s query.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct FailuresQuery {
    /// The newest this many (default 50, at most `stats::MAX_FAILURES`).
    pub limit: Option<u32>,
}

/// `GET /admin/api/failures`'s reply.
#[derive(Clone, Debug, Serialize)]
pub struct FailuresView {
    /// Newest first.
    pub failures: Vec<FailedRow>,
}

async fn failures_route(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    query: Result<Query<FailuresQuery>, QueryRejection>,
) -> Response {
    let m = messages(&state, &headers);
    if let Some(reply) = closed(&state, m) {
        return reply;
    }
    let limit = match query {
        Ok(Query(q)) => q.limit.unwrap_or(50),
        Err(e) => return error(e.status(), e.body_text()),
    };
    match state.reads.failures(limit).await {
        Ok(failures) => Json(FailuresView { failures }).into_response(),
        Err(e) => failed(m, "GET /admin/api/failures", &e),
    }
}

/// `GET /admin/api/usage`'s query.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct UsageQuery {
    /// UTC days ending today (default 30, at most `stats::MAX_DAYS`).
    pub days: Option<u32>,
}

async fn usage_route(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    query: Result<Query<UsageQuery>, QueryRejection>,
) -> Response {
    let m = messages(&state, &headers);
    if let Some(reply) = closed(&state, m) {
        return reply;
    }
    let days = match query {
        Ok(Query(q)) => q.days.unwrap_or(30),
        Err(e) => return error(e.status(), e.body_text()),
    };
    match state.reads.usage(days).await {
        Ok(usage) => Json(usage).into_response(),
        Err(e) => failed(m, "GET /admin/api/usage", &e),
    }
}

/// `GET /admin/api/queue`'s query.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct QueueQuery {
    /// The `next` of the previous page.
    pub cursor: Option<String>,
    /// `open`, `resolved` or `dismissed`.
    pub status: Option<String>,
    /// A feedback category id.
    pub category: Option<String>,
    /// The call's interface.
    pub interface: Option<String>,
    /// `discord` or `web`.
    pub rater: Option<String>,
}

/// The filter and cursor a queue query names.
///
/// # Errors
/// The message for a 400.
pub fn parse_queue_query(
    m: &Messages,
    q: &QueueQuery,
) -> Result<(QueueFilter, Option<Cursor>), String> {
    fn one<T: Copy>(
        m: &Messages,
        name: &str,
        raw: Option<&String>,
        all: &[T],
        id: fn(T) -> &'static str,
    ) -> Result<Option<T>, String> {
        match raw.map(|v| v.trim()).filter(|v| !v.is_empty()) {
            None => Ok(None),
            Some(v) => all
                .iter()
                .copied()
                .find(|t| id(*t) == v)
                .map(Some)
                .ok_or_else(|| {
                    (m.bad_choice)(
                        name,
                        &all.iter().map(|t| id(*t)).collect::<Vec<_>>().join(", "),
                    )
                }),
        }
    }
    let after = match q.cursor.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        None => None,
        Some(c) => Some(c.parse::<Cursor>().map_err(|()| m.bad_cursor.to_owned())?),
    };
    Ok((
        QueueFilter {
            status: one(
                m,
                "status",
                q.status.as_ref(),
                &QueueStatus::ALL,
                QueueStatus::id,
            )?,
            category: one(
                m,
                "category",
                q.category.as_ref(),
                &FeedbackCategory::ALL,
                FeedbackCategory::id,
            )?,
            asked: one(m, "interface", q.interface.as_ref(), &Asked::ALL, Asked::id)?,
            rater: one(m, "rater", q.rater.as_ref(), &RaterKind::ALL, RaterKind::id)?,
        },
        after,
    ))
}

async fn queue_route(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    query: Result<Query<QueueQuery>, QueryRejection>,
) -> Response {
    let m = messages(&state, &headers);
    if let Some(reply) = closed(&state, m) {
        return reply;
    }
    let q = match query {
        Ok(Query(q)) => q,
        Err(e) => return error(e.status(), e.body_text()),
    };
    let (filter, after) = match parse_queue_query(m, &q) {
        Ok(ok) => ok,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    match state.reads.queue(filter, after).await {
        Ok(page) => Json(page).into_response(),
        Err(e) => failed(m, "GET /admin/api/queue", &e),
    }
}

/// One feedback item as the admin page shows it: the item, and its call as
/// [`call_view`] shapes one.
#[derive(Clone, Debug, Serialize)]
pub struct ItemView {
    /// The item.
    pub item: QueueItem,
    /// Its call, whole.
    pub call: CallView,
}

async fn item_route(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let m = messages(&state, &headers);
    if let Some(reply) = closed(&state, m) {
        return reply;
    }
    let Ok(id) = Uuid::try_parse(&id) else {
        return error(StatusCode::NOT_FOUND, m.admin_item_not_found);
    };
    match state.reads.item(id).await {
        Ok(Some((detail, names))) => Json(ItemView {
            item: detail.item,
            call: call_view(m, detail.call, &names),
        })
        .into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, m.admin_item_not_found),
        Err(e) => failed(m, "GET /admin/api/queue/{id}", &e),
    }
}

/// A closing's body.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseBody {
    /// What was found and done.
    pub note: String,
    /// What fixed it (a resolve only means something with one, but either
    /// takes it).
    #[serde(default)]
    pub fixed_by: Option<String>,
}

/// The closing `body` asks for.
///
/// # Errors
/// The message for a 400.
pub fn parse_close(m: &Messages, closing: Closing, body: &CloseBody) -> Result<Close, String> {
    let note = QueueNote::try_new(body.note.clone())
        .map_err(|_| (m.bad_note)(judge_core::feedback::QUEUE_NOTE_MAX_CHARS))?;
    let fixed_by = body
        .fixed_by
        .as_deref()
        .map_or(Ok(None), FixedBy::optional)
        .map_err(|_| (m.bad_fixed_by)(judge_core::feedback::FIXED_BY_MAX_CHARS))?;
    Ok(Close {
        closing,
        note,
        fixed_by,
    })
}

async fn close_route(
    state: &AdminState,
    headers: &HeaderMap,
    id: &str,
    body: Result<Json<CloseBody>, axum::extract::rejection::JsonRejection>,
    closing: Closing,
) -> Response {
    let m = messages(state, headers);
    if let Some(reply) = closed(state, m) {
        return reply;
    }
    let Ok(id) = Uuid::try_parse(id) else {
        return error(StatusCode::NOT_FOUND, m.admin_item_not_found);
    };
    let Ok(Json(body)) = body else {
        return error(StatusCode::BAD_REQUEST, m.bad_body);
    };
    let close = match parse_close(m, closing, &body) {
        Ok(close) => close,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    match state.reads.close(id, &close).await {
        Ok(Some(item)) => {
            tracing::info!(%id, status = %item.status, "feedback item closed");
            Json(item).into_response()
        }
        Ok(None) => error(StatusCode::NOT_FOUND, m.admin_item_not_found),
        Err(e) => failed(m, "POST /admin/api/queue/{id}", &e),
    }
}

async fn resolve_route(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: Result<Json<CloseBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    close_route(&state, &headers, &id, body, Closing::Resolved).await
}

async fn dismiss_route(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: Result<Json<CloseBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    close_route(&state, &headers, &id, body, Closing::Dismissed).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use judge_bot::admin::{ADMIN_TOKEN_ENV, CallRow};
    use tower::ServiceExt as _;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// One private call (id 1), and empty failures and usage.
    struct StubReads;

    fn row() -> CallRow {
        CallRow {
            id: Uuid::from_u128(1),
            asked_at: "2026-10-18T12:00:00Z".into(),
            interface: Asked::Discord,
            asker: Some("alice".into()),
            asker_id: Some("42".into()),
            private: true,
            question: "a private question".into(),
            confidence: Confidence::High,
            retired: None,
            models: vec!["claude-opus-5-5".into()],
            usd: Some(0.04),
            cost_estimated: false,
        }
    }

    #[async_trait::async_trait]
    impl AdminReads for StubReads {
        async fn calls(
            &self,
            filter: &CallFilter,
            _after: Option<Cursor>,
        ) -> Result<CallsPage, JudgeError> {
            Ok(CallsPage {
                calls: if filter.private == Some(false) {
                    vec![]
                } else {
                    vec![row()]
                },
                next: None,
                models: vec!["claude-opus-5-5".into()],
            })
        }
        async fn call(&self, id: CallId) -> Result<Option<(CallDetail, Names)>, JudgeError> {
            Ok((id == CallId::new(Uuid::from_u128(1))).then(|| {
                (
                    CallDetail {
                        id: Uuid::from_u128(1),
                        asked_at: "2026-10-18T12:00:00Z".into(),
                        interface: Asked::Discord,
                        asker: Some("alice".into()),
                        asker_id: Some("42".into()),
                        private: true,
                        thread: "123".into(),
                        locale: "en".into(),
                        session: None,
                        question: "a private question".into(),
                        answer: "No.".into(),
                        confidence: Confidence::High,
                        source: Source::Cr,
                        category: "combat".into(),
                        cr_version: "20260819".into(),
                        citations: vec![],
                        cards: vec![],
                        retired: None,
                        ratings: vec![],
                        runs: vec![],
                    },
                    Names::default(),
                )
            }))
        }
        async fn failures(&self, _limit: u32) -> Result<Vec<FailedRow>, JudgeError> {
            Ok(vec![])
        }
        async fn usage(&self, _days: u32) -> Result<Usage, JudgeError> {
            Err(JudgeError::Upstream(anyhow::anyhow!(
                "no usage in the stub"
            )))
        }
        async fn queue(
            &self,
            filter: QueueFilter,
            _after: Option<Cursor>,
        ) -> Result<QueuePage, JudgeError> {
            Ok(QueuePage {
                items: if filter.status == Some(QueueStatus::Dismissed) {
                    vec![]
                } else {
                    vec![item(QueueStatus::Open, None)]
                },
                next: None,
            })
        }
        async fn item(&self, id: Uuid) -> Result<Option<(QueueDetail, Names)>, JudgeError> {
            let call = self.call(CallId::new(Uuid::from_u128(1))).await?;
            Ok(call
                .filter(|_| id == Uuid::from_u128(5))
                .map(|(call, names)| {
                    (
                        QueueDetail {
                            item: item(QueueStatus::Open, None),
                            call,
                        },
                        names,
                    )
                }))
        }
        async fn close(&self, id: Uuid, close: &Close) -> Result<Option<QueueItem>, JudgeError> {
            Ok((id == Uuid::from_u128(5))
                .then(|| item(close.closing.status(), Some(close.note.to_string()))))
        }
    }

    fn item(status: QueueStatus, note: Option<String>) -> QueueItem {
        QueueItem {
            id: Uuid::from_u128(5),
            call: Uuid::from_u128(1),
            status,
            opened_at: "2026-10-20T12:00:00Z".into(),
            resolved_at: None,
            note,
            fixed_by: None,
            score: "incorrect".into(),
            category: Some(FeedbackCategory::WrongCard),
            wrong: Some("Bob".into()),
            right: None,
            rater_kind: RaterKind::Discord,
            rater: "42".into(),
            rater_name: None,
            judge: false,
            ip: None,
            user_agent: None,
            rated_at: "2026-10-20T12:00:00Z".into(),
            interface: Asked::Discord,
            private: false,
            question: "q".into(),
            retired: None,
        }
    }

    async fn post(
        router: &Router,
        path: &str,
        auth: Option<&str>,
        body: &str,
    ) -> Result<(StatusCode, serde_json::Value), Box<dyn std::error::Error>> {
        let mut req = Request::post(path).header(header::CONTENT_TYPE, "application/json");
        if let Some(a) = auth {
            req = req.header(header::AUTHORIZATION, a);
        }
        let res = router
            .clone()
            .oneshot(req.body(Body::from(body.to_owned()))?)
            .await?;
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 16).await?;
        Ok((status, serde_json::from_slice(&bytes)?))
    }

    /// The queue's reads and its one write: the write needs the token like
    /// every read, takes a note, and answers the item as it now stands.
    #[tokio::test]
    async fn the_queue_reads_and_closes_behind_the_token() -> Result<(), Box<dyn std::error::Error>>
    {
        let dist = dist()?;
        let app = app(&dist, Availability::open()).ok_or("app")?;
        let bearer = format!("Bearer {TOKEN}");
        let item = "/admin/api/queue/00000000-0000-0000-0000-000000000005";
        for path in ["/admin/api/queue", item] {
            let (status, _, _) = send(&app, path, None).await?;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        }
        let resolve = format!("{item}/resolve");
        let (status, _) = post(&app, &resolve, None, r#"{"note":"x"}"#).await?;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a write without the token"
        );
        let (status, _) = post(&app, &resolve, Some("Bearer nope"), r#"{"note":"x"}"#).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _, body) = send(&app, "/admin/api/queue?status=open", Some(&bearer)).await?;
        assert_eq!(status, StatusCode::OK);
        let j: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(
            j.pointer("/items/0/category"),
            Some(&serde_json::json!("wrong_card"))
        );
        let (status, _, body) = send(&app, item, Some(&bearer)).await?;
        assert_eq!(status, StatusCode::OK);
        let j: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(j.pointer("/call/answer"), Some(&serde_json::json!("No.")));
        assert_eq!(j.pointer("/item/status"), Some(&serde_json::json!("open")));

        let (status, j) = post(
            &app,
            &resolve,
            Some(&bearer),
            r#"{"note":" added a note ","fixed_by":"abc123"}"#,
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{j}");
        assert_eq!(j.get("status"), Some(&serde_json::json!("resolved")));
        assert_eq!(j.get("note"), Some(&serde_json::json!("added a note")));
        let (status, j) = post(
            &app,
            &format!("{item}/dismiss"),
            Some(&bearer),
            r#"{"note":"spam"}"#,
        )
        .await?;
        assert_eq!(
            (status, j.get("status")),
            (StatusCode::OK, Some(&serde_json::json!("dismissed")))
        );

        for (path, body, expected) in [
            (
                resolve.as_str(),
                r#"{"note":"  "}"#,
                StatusCode::BAD_REQUEST,
            ),
            (resolve.as_str(), r#"{"nope":1}"#, StatusCode::BAD_REQUEST),
            (
                resolve.as_str(),
                r#"{"note":"a\u0000b"}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                resolve.as_str(),
                r#"{"note":"ok","fixed_by":"x\u0000"}"#,
                StatusCode::BAD_REQUEST,
            ),
            (resolve.as_str(), "not json", StatusCode::BAD_REQUEST),
            (
                "/admin/api/queue/00000000-0000-0000-0000-000000000009/dismiss",
                r#"{"note":"x"}"#,
                StatusCode::NOT_FOUND,
            ),
            (
                "/admin/api/queue/nope/resolve",
                r#"{"note":"x"}"#,
                StatusCode::NOT_FOUND,
            ),
        ] {
            let (status, j) = post(&app, path, Some(&bearer), body).await?;
            assert_eq!(status, expected, "{path} {body}: {j}");
            assert_eq!(j.get("kind"), Some(&serde_json::json!("error")), "{path}");
        }
        for (path, expected) in [
            ("/admin/api/queue?status=closed", StatusCode::BAD_REQUEST),
            ("/admin/api/queue?category=typo", StatusCode::BAD_REQUEST),
            ("/admin/api/queue?rater=mcp", StatusCode::BAD_REQUEST),
            (
                "/admin/api/queue?interface=telegram",
                StatusCode::BAD_REQUEST,
            ),
            ("/admin/api/queue?cursor=nope", StatusCode::BAD_REQUEST),
            (
                "/admin/api/queue/00000000-0000-0000-0000-000000000009",
                StatusCode::NOT_FOUND,
            ),
        ] {
            let (status, _, body) = send(&app, path, Some(&bearer)).await?;
            assert_eq!(
                status,
                expected,
                "{path}: {}",
                String::from_utf8_lossy(&body)
            );
        }
        std::fs::remove_dir_all(&dist)?;
        Ok(())
    }

    fn admin_token() -> Option<AdminToken> {
        AdminSettings::from_vars(|k| (k == ADMIN_TOKEN_ENV).then(|| TOKEN.to_owned()))
            .ok()?
            .token
    }

    fn app(dist: &Path, availability: Availability) -> Option<Router> {
        Some(router(
            Arc::new(StubReads),
            &admin_token()?,
            dist,
            availability,
            Locale::En,
        ))
    }

    async fn send(
        router: &Router,
        path: &str,
        auth: Option<&str>,
    ) -> Result<(StatusCode, HeaderMap, Vec<u8>), Box<dyn std::error::Error>> {
        let mut req = Request::get(path);
        if let Some(a) = auth {
            req = req.header(header::AUTHORIZATION, a);
        }
        let res = router.clone().oneshot(req.body(Body::empty())?).await?;
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 16).await?;
        Ok((status, headers, bytes.to_vec()))
    }

    fn dist() -> Result<PathBuf, Box<dyn std::error::Error>> {
        let dist = std::env::temp_dir().join(format!("judge-admin-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(dist.join("assets"))?;
        std::fs::write(
            dist.join("index.html"),
            "<!doctype html><title>admin</title>",
        )?;
        std::fs::write(dist.join("assets/app.js"), "console.log(1)")?;
        Ok(dist)
    }

    /// Every API path is 401 without the token or with another one, and the
    /// token opens them, uncached.
    #[tokio::test]
    async fn the_api_answers_only_the_admin_token() -> Result<(), Box<dyn std::error::Error>> {
        let dist = dist()?;
        let app = app(&dist, Availability::open()).ok_or("app")?;
        let bearer = format!("Bearer {TOKEN}");
        for path in [
            "/admin/api/calls",
            "/admin/api/calls/00000000-0000-0000-0000-000000000001",
            "/admin/api/failures",
            "/admin/api/usage",
            "/admin/api/nothing",
            "/admin/api",
            "/admin/api/",
        ] {
            for auth in [
                None,
                Some("Bearer nope"),
                Some("Bearer fedcba9876543210fedcba9876543210"),
                Some(&format!("Basic {TOKEN}")),
                Some(&format!("Bearer {TOKEN}x")),
            ] {
                let (status, headers, _) = send(&app, path, auth).await?;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} {auth:?}");
                assert!(headers.contains_key(header::WWW_AUTHENTICATE));
            }
        }
        let (status, _, _) =
            send(&app, "/admin/api/calls", Some(&format!("bearer {TOKEN}"))).await?;
        assert_eq!(status, StatusCode::OK, "the scheme is case-insensitive");
        let (status, headers, body) = send(&app, "/admin/api/calls", Some(&bearer)).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        let j: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(
            j.pointer("/calls/0/private"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            j.pointer("/calls/0/asker_id"),
            Some(&serde_json::json!("42"))
        );
        let (status, _, body) = send(
            &app,
            "/admin/api/calls/00000000-0000-0000-0000-000000000001",
            Some(&bearer),
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        for (path, expected) in [
            (
                "/admin/api/calls/00000000-0000-0000-0000-000000000002",
                StatusCode::NOT_FOUND,
            ),
            ("/admin/api/calls/not-a-call", StatusCode::NOT_FOUND),
            ("/admin/api/calls?private=maybe", StatusCode::BAD_REQUEST),
            (
                "/admin/api/calls?interface=telegram",
                StatusCode::BAD_REQUEST,
            ),
            ("/admin/api/calls?cursor=nope", StatusCode::BAD_REQUEST),
            ("/admin/api/failures", StatusCode::OK),
            ("/admin/api/usage", StatusCode::INTERNAL_SERVER_ERROR),
            ("/admin/api/nothing", StatusCode::NOT_FOUND),
        ] {
            let (status, _, body) = send(&app, path, Some(&bearer)).await?;
            assert_eq!(
                status,
                expected,
                "{path}: {}",
                String::from_utf8_lossy(&body)
            );
            let j: serde_json::Value = serde_json::from_slice(&body)?;
            if expected != StatusCode::OK {
                assert_eq!(j.get("kind"), Some(&serde_json::json!("error")), "{path}");
            }
        }
        std::fs::remove_dir_all(&dist)?;
        Ok(())
    }

    /// The pages load without the token (it is in the fragment, which the
    /// browser keeps), the assets are served, and nothing else is.
    #[tokio::test]
    async fn the_pages_and_assets_are_served_and_nothing_else()
    -> Result<(), Box<dyn std::error::Error>> {
        let dist = dist()?;
        let app = app(&dist, Availability::open()).ok_or("app")?;
        for path in [
            "/admin",
            "/admin/",
            "/admin/calls/x",
            "/admin/usage",
            "/assets/app.js",
        ] {
            let (status, _, _) = send(&app, path, None).await?;
            assert_eq!(status, StatusCode::OK, "{path}");
        }
        let (status, headers, _) = send(&app, "/", None).await?;
        assert!(status.is_redirection());
        assert_eq!(
            headers.get(header::LOCATION).and_then(|v| v.to_str().ok()),
            Some("/admin")
        );
        for path in ["/feed", "/api/judge", "/api/health", "/mcp"] {
            let (status, _, _) = send(&app, path, None).await?;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
        std::fs::remove_dir_all(&dist)?;
        Ok(())
    }

    /// While a major upgrade migrates, the reads answer the maintenance
    /// reply (after the token check).
    #[tokio::test]
    async fn the_reads_wait_for_the_migration() -> Result<(), Box<dyn std::error::Error>> {
        let dist = dist()?;
        let app = app(&dist, Availability::upgrading()).ok_or("app")?;
        let (status, _, _) = send(&app, "/admin/api/calls", None).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, body) =
            send(&app, "/admin/api/calls", Some(&format!("Bearer {TOKEN}"))).await?;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let j: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(j.get("kind"), Some(&serde_json::json!("maintenance")));
        std::fs::remove_dir_all(&dist)?;
        Ok(())
    }

    #[test]
    fn the_role_needs_its_token_and_a_page() {
        let cfg = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            AdminConfig::from_vars(|k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()))
        };
        let unmet = |pairs: &[(&str, &str)]| {
            cfg(pairs).ok().map(|c| {
                Admin::new(c)
                    .err()
                    .map(|u| u.into_iter().collect::<Vec<_>>())
            })
        };
        assert_eq!(
            unmet(&[("WEB_DIST", "nowhere")]),
            Some(Some(vec![
                AdminUnmet::Token,
                AdminUnmet::WebBuild {
                    index: PathBuf::from("nowhere/index.html")
                }
            ]))
        );
        assert_eq!(
            unmet(&[(ADMIN_TOKEN_ENV, TOKEN), ("WEB_DIST", "nowhere")]),
            Some(Some(vec![AdminUnmet::WebBuild {
                index: PathBuf::from("nowhere/index.html")
            }]))
        );
        assert!(AdminUnmet::Token.to_string().contains("ADMIN_TOKEN"));
    }
}
