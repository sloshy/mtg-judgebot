//! The axum layer: shared state, routes and handlers. Everything that can be
//! pure lives in [`crate::shape`]; this file is the glue.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::Context as _;
use axum::{
    Json, Router,
    extract::{ConnectInfo, FromRequest, FromRequestParts, Request, State},
    http::{HeaderMap, StatusCode, request::Parts},
    response::{IntoResponse as _, Response},
    routing::{any, get, post},
};
use judge_bot::ingest::runs::FreshnessReader;
use judge_bot::maintenance::{self, Availability};
use judge_bot::run;
use judge_bot::shutdown::Shutdown;
use judge_core::{
    About, CallStore, Deps, Freshness, HistoryScope, Locale, NetworkOperator, Origin, Question,
    SourceOffer,
};
use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

use crate::{
    catalog,
    config::{ApiConfig, ClientIpSource},
    feed::{self, PublicCalls},
    interfaces::Interfaces,
    limit::RateLimiter,
    shape::{self, ApiReply, JudgeRequest},
};

/// How long a request waits for a free judge slot before replying "busy".
/// Web clients have no 3 s deadline, so this is more patient than Discord.
pub const ACQUIRE_WAIT: Duration = Duration::from_secs(10);

/// A readiness check `GET /api/health` runs: the database, in production.
///
/// A trait rather than a `PgPool` so the routes stay testable without one and
/// so the probe is whatever the binary wires (a container healthcheck wants
/// "can this process serve a question", which for the API means "can it
/// reach Postgres").
#[async_trait::async_trait]
pub trait Probe: Send + Sync {
    /// `Ok` when the dependency answers; the error text is returned to the
    /// caller, so it should name the dependency and not leak credentials.
    async fn probe(&self) -> Result<(), String>;
}

/// How long the database probe waits, pool acquisition included. Shorter than
/// a container healthcheck's timeout, so a saturated pool reads as "slow", not
/// as a killed check.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Fails while the process is closed for a major upgrade's migration
/// ([`judge_bot::maintenance`]), so a deploy does not count it as up.
pub struct Maintenance(pub Availability);

#[async_trait::async_trait]
impl Probe for Maintenance {
    async fn probe(&self) -> Result<(), String> {
        if self.0.is_open() {
            Ok(())
        } else {
            Err("maintenance: migrating the schema for a major upgrade".to_owned())
        }
    }
}

#[async_trait::async_trait]
impl Probe for sqlx::PgPool {
    async fn probe(&self) -> Result<(), String> {
        let query = sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(self);
        match tokio::time::timeout(PROBE_TIMEOUT, query).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(format!("database: {e}")),
            Err(_) => Err(format!(
                "database: no answer within {}s",
                PROBE_TIMEOUT.as_secs()
            )),
        }
    }
}

/// What `GET /api/about` reports about the data: the CR release loaded and
/// the last refresh. A trait for the same reason as [`Probe`]: the routes are
/// tested without a database.
#[async_trait::async_trait]
pub trait DataStatus: Send + Sync {
    /// The data's freshness, or `None` when it cannot be read; never an
    /// error, so the source offer is served whatever the database does.
    async fn freshness(&self) -> Option<Freshness>;
}

#[async_trait::async_trait]
impl DataStatus for FreshnessReader {
    async fn freshness(&self) -> Option<Freshness> {
        self.read().await
    }
}

/// Shared state behind every route.
pub struct App {
    deps: Deps,
    store: Arc<dyn CallStore>,
    permits: Arc<Semaphore>,
    limiter: RateLimiter,
    history_len: usize,
    client_ip: ClientIpSource,
    probe: Option<Arc<dyn Probe>>,
    /// What `GET /api/about` says about where this instance's source is.
    offer: SourceOffer,
    /// Whom `GET /api/about` (and so the page) tells users to write to.
    operator: NetworkOperator,
    /// What `GET /api/about` says about the data; `None` serves it as null.
    data: Option<Arc<dyn DataStatus>>,
    /// Open, or closed for a major upgrade's migration.
    availability: Availability,
    /// The instance's language (`JUDGE_LOCALE`), for a caller whose own has
    /// no catalog.
    locale: Locale,
    /// The public feed's reads, when the feed is served.
    public_calls: Option<Arc<dyn PublicCalls>>,
    /// The feed reads' own per-address window ([`feed::READ_LIMIT`]).
    reads: RateLimiter,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("permits", &self.permits.available_permits())
            .field("limiter", &self.limiter)
            .finish_non_exhaustive()
    }
}

impl App {
    /// Wire the shared state.
    #[must_use]
    pub fn new(
        deps: Deps,
        store: Arc<dyn CallStore>,
        cfg: &ApiConfig,
        offer: SourceOffer,
        operator: NetworkOperator,
    ) -> Self {
        Self {
            deps,
            store,
            permits: Arc::new(Semaphore::new(cfg.max_concurrent)),
            limiter: RateLimiter::new(cfg.rate_limit, cfg.rate_window),
            history_len: cfg.history_len,
            client_ip: cfg.client_ip,
            probe: None,
            offer,
            operator,
            data: None,
            availability: Availability::open(),
            locale: Locale::default(),
            public_calls: None,
            reads: RateLimiter::new(feed::READ_LIMIT, feed::READ_WINDOW),
        }
    }

    /// Serve the public feed (`GET /api/feed`, `GET /api/calls/{id}`) from
    /// `calls`, where the web page is served.
    #[must_use]
    pub fn with_public_calls(mut self, calls: Arc<dyn PublicCalls>) -> Self {
        self.public_calls = Some(calls);
        self
    }

    /// The instance's language.
    pub(crate) const fn locale(&self) -> Locale {
        self.locale
    }

    /// The feed's reads, when it is served.
    pub(crate) fn public_calls(&self) -> Option<Arc<dyn PublicCalls>> {
        self.public_calls.clone()
    }

    /// Whether the instance is open (not migrating a major upgrade).
    pub(crate) fn is_open(&self) -> bool {
        self.availability.is_open()
    }

    /// What the rate limiters bucket on.
    pub(crate) const fn client_ip_source(&self) -> ClientIpSource {
        self.client_ip
    }

    /// Count a feed read from `ip`; `false` when its window is used up.
    pub(crate) fn allow_read(&self, ip: IpAddr) -> bool {
        self.reads.allow(ip)
    }

    /// Answer a caller whose language has no catalog in `locale`
    /// (`JUDGE_LOCALE`).
    #[must_use]
    pub const fn with_locale(mut self, locale: Locale) -> Self {
        self.locale = locale;
        self
    }

    /// Answer questions only while `availability` is open, and the
    /// maintenance reply otherwise.
    #[must_use]
    pub fn with_availability(mut self, availability: Availability) -> Self {
        self.availability = availability;
        self
    }

    /// Make `GET /api/health` check `probe` (the database) instead of only
    /// answering that the process is up.
    #[must_use]
    pub fn with_probe(mut self, probe: Arc<dyn Probe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// Make `GET /api/about` report the data's freshness from `data` (the
    /// run record, in production) instead of null.
    #[must_use]
    pub fn with_data_status(mut self, data: Arc<dyn DataStatus>) -> Self {
        self.data = Some(data);
        self
    }

    /// The judge slots, to share with another interface in this process
    /// (the MCP transport), so that both together stay under `JUDGE_CONCURRENCY`.
    #[must_use]
    pub fn permits(&self) -> Arc<Semaphore> {
        Arc::clone(&self.permits)
    }

    /// A judge slot, waiting up to [`ACQUIRE_WAIT`] for one; `None` means "busy".
    /// Owned, so the run can hold it past a request that goes away.
    async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        match tokio::time::timeout(ACQUIRE_WAIT, Arc::clone(&self.permits).acquire_owned()).await {
            Ok(Ok(p)) => Some(p),
            // The semaphore is never closed; a timeout is the only real `None`.
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// Run the pipeline for `q` and shape the reply. Mirrors the Discord
    /// adapter's flow (history → judge → persist) minus rating buttons.
    async fn answer(
        &self,
        locale: Locale,
        q: &Question,
        ip: IpAddr,
        slot: OwnedSemaphorePermit,
    ) -> ApiReply {
        let history = match self
            .store
            .history(&q.thread_id, self.history_len, HistoryScope::Public)
            .await
        {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(
                    error = format_args!("{e:#}"),
                    "session history unavailable; judging without it"
                );
                vec![]
            }
        };
        // The stored call feeds session history and the prior-call retrieval
        // source; web calls arrive unrated, exactly like an unrated Discord
        // call. A store failure only costs those.
        let ran = run::run(&self.deps, &self.store, q, &Origin::Web, history, slot).await;
        tracing::info!(
            %ip,
            thread = %q.thread_id,
            elapsed_ms = ran.elapsed.as_millis(),
            usd = format_args!("{:.4}", ran.cost.usd()),
            worst_case = ran.cost.is_estimate(),
            llm_calls = ran.model_calls,
            outcome = %ran.outcome,
            "POST /api/judge"
        );
        match ran.result {
            Ok(v) => shape::answer(locale, &v, ran.context.as_ref()),
            Err(e) => {
                // The line above classifies the outcome; a genuine failure also
                // needs its cause, or the operator sees `outcome=upstream` and
                // nothing else. `thread` correlates this with that line — the
                // IP is deliberately not repeated here, because unlike the
                // metadata above, `{e:#}` can carry fragments of the question.
                if e.is_operator_failure() {
                    tracing::warn!(thread = %q.thread_id, error = format_args!("{e:#}"), "judge failed");
                }
                shape::error(locale, &e)
            }
        }
    }
}

/// The routes `interfaces` asks for: `POST /api/judge` for
/// [`Interface::Api`](crate::Interface::Api), and the built web client as the
/// router's fallback for [`Web`](crate::Interface::Web) (unknown paths get
/// `index.html`, so a client-side route refresh still loads the app). An
/// interface left off is simply not mounted: its paths fall through to
/// whatever is left, which is a 404 with no web client and the page's own SPA
/// fallback with one. The startup log is where an operator reads why.
///
/// `GET /api/health` is unconditional: it reports on the process, not on a
/// interface, and the container healthcheck has to reach it whatever else is
/// switched off.
///
/// The MCP router ([`crate::mcp::router`]) is merged *into* this one, so the
/// web fallback wins and the token gate stays on `/mcp` alone.
pub fn router(app: Arc<App>, interfaces: &Interfaces, web_dist: &Path) -> Router {
    // `/about` sits beside `/health`, outside the opt-in set: the source
    // offer is owed on every interface, and the page reads it from here.
    let mut router = Router::new()
        .route("/api/health", get(health))
        .route("/api/about", get(about));
    if interfaces.api() {
        router = router.route("/api/judge", post(judge_route));
    }
    if interfaces.web() {
        if app.public_calls.is_some() {
            router = router
                .route("/api/feed", get(feed::feed_route))
                .route("/api/calls/{id}", get(feed::call_route));
        }
        // An endpoint that is not served is a JSON 404, not the page: the
        // page's fallback below would answer it with `index.html`. The
        // admin pages and their API exist on the admin listener only
        // (`--admin`, D37), so here they are a 404 too, never the page.
        router = router
            .route("/api/{*rest}", any(api_not_found))
            .route("/admin", any(api_not_found))
            .route("/admin/", any(api_not_found))
            .route("/admin/{*rest}", any(api_not_found));
        // `fallback`, not `not_found_service`: the page's own routes
        // (`/feed`, `/calls/<id>`) are pages, served 200, so a link to one
        // unfurls and is crawled as a page rather than a missing one.
        let files = ServeDir::new(web_dist).fallback(ServeFile::new(web_dist.join("index.html")));
        router = router.fallback_service(files);
    }
    router.with_state(app)
}

/// Bind `cfg.addr`. Separate from [`serve_on`] so a process binds before it
/// starts anything else (the Discord gateway), and a taken address fails
/// first.
///
/// # Errors
/// Binding the address.
pub async fn bind(cfg: &ApiConfig) -> anyhow::Result<tokio::net::TcpListener> {
    let listener = tokio::net::TcpListener::bind(cfg.addr)
        .await
        .with_context(|| format!("bind {}", cfg.addr))?;
    tracing::info!(addr = %cfg.addr, "HTTP adapter listening");
    Ok(listener)
}

/// Reserve `addr` without listening on it: a connection to it is refused
/// until [`listen`], but an address another process listens on fails here,
/// as it does in [`bind`].
///
/// # Errors
/// Binding the address.
pub fn reserve(addr: SocketAddr) -> anyhow::Result<tokio::net::TcpSocket> {
    let socket = if addr.is_ipv4() {
        tokio::net::TcpSocket::new_v4()
    } else {
        tokio::net::TcpSocket::new_v6()
    }
    .context("open a socket")?;
    // As `TcpListener::bind` does on Unix, so a restart can rebind at once.
    socket.set_reuseaddr(true).context("set SO_REUSEADDR")?;
    socket.bind(addr).with_context(|| format!("bind {addr}"))?;
    Ok(socket)
}

/// Start listening on a socket from [`reserve`].
///
/// # Errors
/// The kernel refused to listen on it.
pub fn listen(socket: tokio::net::TcpSocket) -> anyhow::Result<tokio::net::TcpListener> {
    let listener = socket.listen(1024).context("listen")?;
    if let Ok(addr) = listener.local_addr() {
        tracing::info!(%addr, "HTTP adapter listening");
    }
    Ok(listener)
}

/// Serve `router` on `listener` until the accept loop fails, or until
/// `shutdown` is requested and the requests in flight have been answered.
///
/// On the request the listener closes, so a new connection is refused
/// (behind the `edge` proxy or the tunnel, the dial goes on to the other
/// instance's address), and each open connection is closed once its current
/// request is answered. A request still open at the drain's limit is cut
/// off with the process.
///
/// # Errors
/// A fatal accept-loop error.
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: &Shutdown,
) -> anyhow::Result<()> {
    let stopping = shutdown.clone();
    let serving = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { stopping.requested().await });
    tokio::select! {
        served = serving => {
            served.context("serve HTTP")?;
            if shutdown.is_requested() {
                tracing::info!("HTTP: every request in flight was answered");
            }
            Ok(())
        }
        () = shutdown.limit_passed() => {
            tracing::warn!(
                limit_secs = shutdown.limit().as_secs(),
                "HTTP: requests still open at the drain's limit; they end with the process"
            );
            Ok(())
        }
    }
}

/// `200 ok` when the process can serve, `503` naming the failed dependency
/// when it cannot; a container healthcheck or a load balancer reads the
/// status, a human reads the body. Without a probe it only says the process
/// is up.
async fn health(State(app): State<Arc<App>>) -> (StatusCode, String) {
    match &app.probe {
        None => (StatusCode::OK, "ok".to_owned()),
        Some(p) => match p.probe().await {
            Ok(()) => (StatusCode::OK, "ok".to_owned()),
            Err(why) => {
                tracing::warn!(%why, "health probe failed");
                (StatusCode::SERVICE_UNAVAILABLE, why)
            }
        },
    }
}

/// `GET /api/about`: the source offer and what this listener serves.
#[derive(Debug, Serialize)]
struct AboutReply {
    #[serde(flatten)]
    about: About,
    /// Whether the public feed is served here (`/feed`, `GET /api/feed`).
    feed: bool,
}

/// The source offer as JSON (`judge_core::About`): repository, commit,
/// licence, copyright, the operator's support address, the notice in one
/// string, and the data's freshness (null when it cannot be read: the offer
/// is served whatever the database does); and whether the feed is served.
async fn about(State(app): State<Arc<App>>) -> Json<AboutReply> {
    let freshness = match &app.data {
        Some(d) => d.freshness().await,
        None => None,
    };
    Json(AboutReply {
        about: app
            .offer
            .about(app.operator.operator(), freshness, app.locale),
        feed: app.public_calls.is_some(),
    })
}

/// An `/api/` path nothing here serves.
async fn api_not_found(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let locale = shape::request_locale(
        None,
        headers
            .get(axum::http::header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
        app.locale,
    );
    (
        StatusCode::NOT_FOUND,
        Json(ApiReply::Error {
            message: catalog::messages(locale).not_routed.to_owned(),
        }),
    )
        .into_response()
}

/// The peer address, when the server was started with connect info (tests
/// drive the router directly and have none). Infallible by construction.
pub(crate) struct PeerAddr(pub(crate) Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0),
        )))
    }
}

/// The `POST /api/judge` body. A body axum's `Json` cannot read (not JSON,
/// the wrong `Content-Type`, the wrong shape, too large) keeps axum's status
/// and message but is answered as an [`ApiReply::Error`], so every reply from
/// the route is JSON tagged by `kind`.
struct JudgeBody(JudgeRequest);

impl<S: Send + Sync> FromRequest<S> for JudgeBody {
    type Rejection = (StatusCode, Json<ApiReply>);

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<JudgeRequest>::from_request(req, state).await {
            Ok(Json(body)) => Ok(Self(body)),
            Err(e) => Err((
                e.status(),
                Json(ApiReply::Error {
                    message: e.body_text(),
                }),
            )),
        }
    }
}

async fn judge_route(
    State(app): State<Arc<App>>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    JudgeBody(req): JudgeBody,
) -> Response {
    let locale = shape::request_locale(
        req.locale.as_deref(),
        headers
            .get(axum::http::header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
        app.locale,
    );
    let m = catalog::messages(locale);
    if let Err(message) = shape::validate(locale, &req) {
        return (StatusCode::BAD_REQUEST, Json(ApiReply::Error { message })).into_response();
    }
    // Before the rate limit: a maintenance reply costs the caller nothing.
    if !app.availability.is_open() {
        return (
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
            .into_response();
    }
    let ip = client_ip(&headers, peer, app.client_ip);
    if !app.limiter.allow(ip) {
        tracing::info!(%ip, "rate limited");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ApiReply::RateLimited {
                message: m.rate_limited.to_owned(),
            }),
        )
            .into_response();
    }
    let Some(slot) = app.acquire().await else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ApiReply::Busy {
                message: m.busy.to_owned(),
            }),
        )
            .into_response();
    };
    // No session id still gets an answer, just no follow-up history.
    let session = req.session_id.unwrap_or_else(Uuid::new_v4);
    let q = Question {
        thread_id: format!("web:{session}"),
        text: shape::question_text(&req),
    };
    (StatusCode::OK, Json(app.answer(locale, &q, ip, slot).await)).into_response()
}

/// The address rate limiting buckets on: `CF-Connecting-IP` behind Cloudflare,
/// otherwise the peer address (loopback if the connect info is missing, as in
/// tests).
///
/// `X-Forwarded-For` is deliberately never consulted. Cloudflare *appends* the
/// connecting address to a client-supplied header instead of replacing it, so
/// its first hop is chosen by the caller; bucketing on that would let anyone
/// mint a fresh allowance per request against a paid endpoint. A missing
/// `CF-Connecting-IP` falls back to the peer, which over-counts (everything
/// behind the proxy shares one bucket) rather than under-counting.
pub(crate) fn client_ip(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    source: ClientIpSource,
) -> IpAddr {
    let trusted = match source {
        ClientIpSource::PeerAddr => None,
        ClientIpSource::CloudflareConnectingIp => headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<IpAddr>().ok()),
    };
    match (trusted, peer) {
        (Some(ip), _) => ip,
        (None, Some(p)) => p.ip(),
        (None, None) => IpAddr::V4(Ipv4Addr::LOCALHOST),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interfaces::Interface;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use judge_core::{
        CallId, Card, CardId, Category, CategoryGuess, Confidence, Context, CrVersion, Extraction,
        Extractor, Face, Interface as Asked, JudgeError, Layout, MatchedVia, Outcome, Qa,
        Resolution, Resolver, Retriever, RuleChunk, RuleId, RunRecord, Score, Source, Synthesizer,
        Tally, Unvalidated, Validated, Verdict,
    };
    use nonempty::{NonEmpty, nonempty};
    use std::sync::Mutex;
    use tower::ServiceExt as _;

    const RULE_BODY: &str =
        "Damage dealt by a source with lifelink causes its controller to gain that much life.";

    fn card(n: u128, name: &str) -> Card {
        Card {
            id: CardId::new(Uuid::from_u128(n)),
            name: name.into(),
            layout: Layout::Normal,
            faces: NonEmpty::new(Face {
                name: name.into(),
                oracle_text: String::new(),
                mana_cost: String::new(),
                type_line: "Creature".into(),
            }),
        }
    }

    /// Emits one span per `[[…]]` in the text plus "urza" if it appears bare.
    struct StubExtractor;
    #[async_trait]
    impl Extractor for StubExtractor {
        async fn extract(
            &self,
            q: &Question,
            _h: &[Qa],
            _t: &Tally,
        ) -> Result<Extraction, JudgeError> {
            let mut spans = vec![];
            if let Some(start) = q.text.find("[[")
                && let Some(len) = q.text.get(start..).and_then(|s| s.find("]]"))
                && let Some(s) = q.text.get(start..start + len + 2)
            {
                spans.push(s.to_owned());
            }
            if q.text.contains("urza") {
                spans.push("urza".to_owned());
            }
            Ok(Extraction {
                card_spans: spans,
                concepts: vec![],
                primary: CategoryGuess {
                    category: Category::KeywordAbilities,
                    confidence: Confidence::High,
                },
                secondary: vec![],
                source: Source::Cr,
            })
        }
    }

    /// `[[Name]]` resolves; the bare span "urza" is ambiguous.
    struct StubResolver;
    #[async_trait]
    impl Resolver for StubResolver {
        async fn resolve(&self, span: &str) -> Result<Resolution, JudgeError> {
            if let Some(name) = span.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")) {
                return Ok(Resolution::Resolved {
                    card: card(1, name),
                    via: MatchedVia::Bracket,
                });
            }
            Ok(Resolution::Ambiguous {
                query: span.to_owned(),
                candidates: NonEmpty::from((card(2, "Urza's Tower"), vec![card(3, "Urza's Mine")])),
                via: MatchedVia::ShortName,
            })
        }
    }

    struct StubRetriever;
    #[async_trait]
    impl Retriever for StubRetriever {
        async fn retrieve(
            &self,
            _q: &Question,
            _c: &[Card],
            _e: &Extraction,
            _tally: &judge_core::Tally,
        ) -> Result<Context, JudgeError> {
            Ok(Context {
                rules: vec![RuleChunk {
                    id: RuleId::try_new("702.15b".to_owned()).map_err(anyhow::Error::from)?,
                    parent_id: None,
                    subsection: RuleId::try_new("702".to_owned()).map_err(anyhow::Error::from)?,
                    heading: "Lifelink".into(),
                    body: RULE_BODY.into(),
                    examples: vec![],
                    cr_version: CrVersion::try_new("20260819".to_owned())
                        .map_err(anyhow::Error::from)?,
                }],
                ..Context::default()
            })
        }
        async fn lookup_rules(&self, _ids: &[RuleId]) -> Result<Vec<RuleChunk>, JudgeError> {
            Ok(vec![])
        }
    }

    struct StubSynth;
    #[async_trait]
    impl Synthesizer for StubSynth {
        async fn answer(
            &self,
            _q: &Question,
            _ctx: &mut Context,
            _rejected: Option<&judge_core::RejectedAttempt>,
            _t: &Tally,
        ) -> Result<Verdict<Unvalidated>, JudgeError> {
            let id = RuleId::try_new("702.15b".to_owned()).map_err(anyhow::Error::from)?;
            Ok(Verdict::new(
                "Two instances of lifelink are redundant: you gain the life once.".into(),
                Confidence::High,
                vec![judge_core::Citation::Rule {
                    id,
                    quote: judge_core::Quote::try_new("gain that much life")
                        .map_err(anyhow::Error::from)?,
                }],
                Category::KeywordAbilities,
            ))
        }
    }

    /// Records persisted questions and runs; returns no history.
    #[derive(Default)]
    struct StubStore {
        persisted: Mutex<Vec<String>>,
        runs: Mutex<Vec<RunRecord>>,
    }
    #[async_trait]
    impl CallStore for StubStore {
        async fn persist(
            &self,
            q: &Question,
            _origin: &Origin,
            _v: &Verdict<Validated>,
            _ctx: &Context,
            _t: &Tally,
        ) -> Result<CallId, JudgeError> {
            if let Ok(mut p) = self.persisted.lock() {
                p.push(q.text.clone());
            }
            Ok(CallId::new(Uuid::from_u128(9)))
        }
        async fn rate(
            &self,
            _call: CallId,
            _user: &str,
            _score: Score,
            _judge: bool,
        ) -> Result<judge_core::Rated, JudgeError> {
            Ok(judge_core::Rated::Recorded)
        }
        async fn history(
            &self,
            _thread: &str,
            _n: usize,
            _scope: HistoryScope,
        ) -> Result<Vec<Qa>, JudgeError> {
            Ok(vec![])
        }
        async fn forget_user(&self, _user: &str) -> Result<judge_core::Forgotten, JudgeError> {
            Ok(judge_core::Forgotten::default())
        }
        async fn record_failure(
            &self,
            _failed: &judge_core::FailedCall,
        ) -> Result<judge_core::FailedCallId, JudgeError> {
            Ok(judge_core::FailedCallId(1))
        }
        async fn record_run(&self, run: &RunRecord) -> Result<(), JudgeError> {
            if let Ok(mut r) = self.runs.lock() {
                r.push(run.clone());
            }
            Ok(())
        }
    }

    type Res = Result<(), Box<dyn std::error::Error>>;

    fn test_app(rate_limit: u32) -> (Router, Arc<StubStore>) {
        test_app_with_dist(rate_limit, Path::new("does-not-exist"))
    }

    fn stub_deps() -> Deps {
        Deps {
            extractor: Arc::new(StubExtractor),
            resolver: Arc::new(StubResolver),
            retriever: Arc::new(StubRetriever),
            synthesizer: Arc::new(StubSynth),
        }
    }

    fn test_app_with_dist(rate_limit: u32, dist: &Path) -> (Router, Arc<StubStore>) {
        test_app_serving(
            rate_limit,
            dist,
            &Interfaces::of(&nonempty![Interface::Api, Interface::Web]),
        )
    }

    fn test_app_serving(
        rate_limit: u32,
        dist: &Path,
        interfaces: &Interfaces,
    ) -> (Router, Arc<StubStore>) {
        let store = Arc::new(StubStore::default());
        let cfg = test_cfg(rate_limit, dist);
        let app = Arc::new(App::new(
            stub_deps(),
            Arc::clone(&store) as Arc<dyn CallStore>,
            &cfg,
            SourceOffer::upstream(judge_core::Commit::Unknown),
            test_operator(),
        ));
        (router(app, interfaces, &cfg.web_dist), store)
    }

    /// A support address and no Discord username: all the network roles demand.
    fn test_operator() -> NetworkOperator {
        #[expect(
            clippy::expect_used,
            reason = "a fixture built from a constant in a test"
        )]
        judge_core::Operator::new(
            None,
            judge_core::SupportEmail::try_new("judge@example.org").ok(),
        )
        .for_network()
        .expect("the address is set")
    }

    fn test_cfg(rate_limit: u32, dist: &Path) -> ApiConfig {
        ApiConfig {
            addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            web_dist: dist.to_path_buf(),
            max_concurrent: 2,
            history_len: 5,
            rate_limit,
            rate_window: Duration::from_mins(5),
            client_ip: ClientIpSource::PeerAddr,
            mcp_token: None,
            mcp_admin_token: None,
            mcp_hosts: vec![],
            mcp_judge_limit: ApiConfig::DEFAULT_MCP_JUDGE_LIMIT,
            mcp_judge_window: ApiConfig::DEFAULT_MCP_JUDGE_WINDOW,
        }
    }

    async fn post_judge(
        router: Router,
        body: &str,
    ) -> Result<(StatusCode, serde_json::Value), Box<dyn std::error::Error>> {
        let req = Request::post("/api/judge")
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))?;
        let res = router.oneshot(req).await?;
        let status = res.status();
        let bytes = res.into_body().collect().await?.to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        Ok((status, json))
    }

    /// The runs recorded, once `n` have been: a run is recorded after its
    /// reply is sent, so a test waits for it (up to a second).
    async fn runs(store: &StubStore, n: usize) -> Vec<RunRecord> {
        let read = || {
            store
                .runs
                .lock()
                .map_or_else(|e| e.into_inner().clone(), |r| r.clone())
        };
        for _ in 0..100 {
            if read().len() >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        read()
    }

    fn persisted(store: &StubStore) -> Vec<String> {
        store
            .persisted
            .lock()
            .map_or_else(|e| e.into_inner().clone(), |p| p.clone())
    }

    #[tokio::test]
    async fn a_plain_question_is_answered_and_persisted() -> Res {
        let (router, store) = test_app(10);
        let (status, j) = post_judge(router, r#"{"question":"does lifelink stack?"}"#).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(j.get("kind").and_then(|k| k.as_str()), Some("answer"));
        assert_eq!(
            j.get("cr_version").and_then(|k| k.as_str()),
            Some("20260819")
        );
        assert!(
            j.pointer("/citations/0/url")
                .and_then(|u| u.as_str())
                .is_some_and(|u| u.contains("yawgatog")),
            "{j}"
        );
        assert_eq!(persisted(&store), vec!["does lifelink stack?".to_owned()]);
        let runs = runs(&store, 1).await;
        let [run] = runs.as_slice() else {
            return Err(format!("one run recorded, got {runs:?}").into());
        };
        assert_eq!((run.interface, run.private), (Asked::Web, false));
        assert_eq!(run.outcome, Outcome::Answered);
        assert_eq!(
            run.left,
            judge_core::Left::Call(CallId::new(Uuid::from_u128(9)))
        );
        Ok(())
    }

    /// A question that leaves no call (an ambiguous card) is still a run.
    #[tokio::test]
    async fn a_run_is_recorded_whatever_its_outcome() -> Res {
        let (router, store) = test_app(10);
        let (status, _) = post_judge(router, r#"{"question":"can urza block?"}"#).await?;
        assert_eq!(status, StatusCode::OK);
        let runs = runs(&store, 1).await;
        let [run] = runs.as_slice() else {
            return Err(format!("one run recorded, got {runs:?}").into());
        };
        assert_eq!(run.outcome, Outcome::Ambiguous);
        assert_eq!(run.left, judge_core::Left::Nothing);
        assert!(persisted(&store).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn an_ambiguous_span_offers_choices_and_a_pin_resolves_it() -> Res {
        let (router, store) = test_app(10);
        let (status, j) = post_judge(router.clone(), r#"{"question":"can urza block?"}"#).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(j.get("kind").and_then(|k| k.as_str()), Some("ambiguous"));
        let choices = j
            .pointer("/spans/0/choices")
            .and_then(|c| c.as_array())
            .map(|c| c.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();
        assert_eq!(choices, vec!["Urza's Tower", "Urza's Mine"]);
        // Nothing is persisted for an ambiguous outcome.
        assert!(persisted(&store).is_empty());

        // The client re-asks with the pick pinned.
        let body =
            r#"{"question":"can urza block?","pins":[{"span":"urza","name":"Urza's Tower"}]}"#;
        let (status, j) = post_judge(router, body).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            j.get("kind").and_then(|k| k.as_str()),
            Some("answer"),
            "{j}"
        );
        assert_eq!(
            persisted(&store),
            vec!["can [[Urza's Tower]] block?".to_owned()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn requests_beyond_the_rate_limit_get_429() -> Res {
        let (router, _) = test_app(1);
        let (status, _) =
            post_judge(router.clone(), r#"{"question":"does lifelink stack?"}"#).await?;
        assert_eq!(status, StatusCode::OK);
        let (status, j) = post_judge(router, r#"{"question":"does lifelink stack?"}"#).await?;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(j.get("kind").and_then(|k| k.as_str()), Some("rate_limited"));
        Ok(())
    }

    #[tokio::test]
    async fn invalid_requests_get_400_without_using_the_rate_window() -> Res {
        let (router, _) = test_app(1);
        let (status, j) = post_judge(router.clone(), r#"{"question":"  "}"#).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(j.get("kind").and_then(|k| k.as_str()), Some("error"));
        // The invalid request did not consume the single slot in the window.
        let (status, _) = post_judge(router, r#"{"question":"does lifelink stack?"}"#).await?;
        assert_eq!(status, StatusCode::OK);
        Ok(())
    }

    /// A body axum cannot read keeps axum's status but is a JSON `error`
    /// reply, and like any invalid request spends none of the rate window.
    #[tokio::test]
    async fn unreadable_bodies_get_json_errors_with_axums_status() -> Res {
        let (router, _) = test_app(1);
        let too_large = format!(r#"{{"question":"{}"}}"#, "a".repeat(3 * 1024 * 1024));
        for (content_type, body, status, says) in [
            (
                "application/json",
                "{not json",
                StatusCode::BAD_REQUEST,
                "parse",
            ),
            (
                "application/json",
                "{}",
                StatusCode::UNPROCESSABLE_ENTITY,
                "missing field `question`",
            ),
            (
                "application/json",
                r#"{"question":"q","session_id":"not-a-uuid"}"#,
                StatusCode::UNPROCESSABLE_ENTITY,
                "session_id",
            ),
            (
                "text/plain",
                r#"{"question":"q"}"#,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type",
            ),
            (
                "application/json",
                too_large.as_str(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "length limit",
            ),
        ] {
            let req = Request::post("/api/judge")
                .header("content-type", content_type)
                .body(Body::from(body.to_owned()))?;
            let res = router.clone().oneshot(req).await?;
            assert_eq!(res.status(), status, "{content_type} {body:.40}");
            let bytes = res.into_body().collect().await?.to_bytes();
            let j: serde_json::Value = serde_json::from_slice(&bytes)?;
            assert_eq!(j.get("kind").and_then(|k| k.as_str()), Some("error"), "{j}");
            assert!(
                j.get("message")
                    .and_then(|m| m.as_str())
                    .is_some_and(|m| m.contains(says)),
                "{j}"
            );
        }
        let (status, _) = post_judge(router, r#"{"question":"does lifelink stack?"}"#).await?;
        assert_eq!(status, StatusCode::OK);
        Ok(())
    }

    /// The token gate is scoped to `/mcp` by merge order: the web routes and
    /// the SPA fallback stay open, every method on `/mcp` is gated, and
    /// without the MCP router `/mcp` is just another SPA path.
    #[tokio::test]
    async fn the_mcp_gate_covers_only_mcp() -> Res {
        use axum::http::Method;
        const TOKEN: &str = "0123456789abcdef0123456789abcdef";
        // A real dist dir, so the SPA fallback answers 200 and proves it survived the merge.
        let dist = std::env::temp_dir().join(format!("judge-api-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dist)?;
        std::fs::write(
            dist.join("index.html"),
            "<!doctype html><title>judge</title>",
        )?;
        let (web, _) = test_app_with_dist(10, &dist);
        let (bare_web, _) = test_app_with_dist(10, &dist);
        let router = web.merge(crate::mcp::gated(crate::mcp::tests::Echo, TOKEN, None));
        let send = |router: Router, method: Method, path: &str, auth: Option<&str>| {
            let mut req = Request::builder().method(method).uri(path);
            if let Some(a) = auth {
                req = req.header("authorization", a);
            }
            let req = req.body(Body::empty());
            async move { Ok::<_, Box<dyn std::error::Error>>(router.oneshot(req?).await?.status()) }
        };
        // `/` is answered by the fallback (`ServeDir`) alone: a router that lost
        // it in the merge would 404 here.
        assert_eq!(
            send(bare_web, Method::GET, "/", None).await?,
            StatusCode::OK,
            "unmerged web fallback"
        );
        assert_eq!(
            send(router.clone(), Method::GET, "/api/health", None).await?,
            StatusCode::OK
        );
        assert_eq!(
            send(router.clone(), Method::GET, "/", None).await?,
            StatusCode::OK,
            "web fallback survived the merge"
        );
        for method in [Method::POST, Method::GET, Method::DELETE, Method::OPTIONS] {
            assert_eq!(
                send(router.clone(), method.clone(), "/mcp", None).await?,
                StatusCode::UNAUTHORIZED,
                "{method}"
            );
        }
        let bearer = format!("Bearer {TOKEN}");
        assert_eq!(
            send(router.clone(), Method::POST, "/mcp", Some(&bearer)).await?,
            StatusCode::OK
        );
        assert_eq!(
            send(router, Method::DELETE, "/mcp", Some(&bearer)).await?,
            StatusCode::OK
        );
        let (bare, _) = test_app(10);
        assert_ne!(
            send(bare, Method::POST, "/mcp", None).await?,
            StatusCode::UNAUTHORIZED,
            "no token configured: no gate"
        );
        Ok(())
    }

    /// The point of the opt-in: an interface nobody named is not mounted, and the
    /// paths behind it are not answered by something else standing in.
    #[tokio::test]
    async fn an_interface_left_off_is_not_mounted() -> Res {
        let dist = std::env::temp_dir().join(format!("judge-api-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dist)?;
        std::fs::write(
            dist.join("index.html"),
            "<!doctype html><title>judge</title>",
        )?;

        // --api alone: the question route answers and the page is absent, even
        // though WEB_DIST points at a real build.
        let (api_only, _) =
            test_app_serving(10, &dist, &Interfaces::of(&nonempty![Interface::Api]));
        let (status, j) =
            post_judge(api_only.clone(), r#"{"question":"does lifelink stack?"}"#).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(j.get("kind").and_then(|k| k.as_str()), Some("answer"));
        let res = api_only
            .oneshot(Request::get("/").body(Body::empty())?)
            .await?;
        assert_eq!(
            res.status(),
            StatusCode::NOT_FOUND,
            "a built page must not be served without --web"
        );

        // --web alone: the page is served and the route that spends money is
        // not routed at all.
        let (web_only, store) =
            test_app_serving(10, &dist, &Interfaces::of(&nonempty![Interface::Web]));
        let res = web_only
            .clone()
            .oneshot(Request::get("/").body(Body::empty())?)
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let (status, _) = post_judge(web_only, r#"{"question":"does lifelink stack?"}"#).await?;
        assert_ne!(
            status,
            StatusCode::OK,
            "POST /api/judge must not be answered without --api"
        );
        assert!(persisted(&store).is_empty());
        std::fs::remove_dir_all(&dist)?;
        Ok(())
    }

    /// The admin pages and their API exist on the admin listener only: the
    /// network roles' listener answers them 404 (JSON, never the page),
    /// whatever token a request carries and whatever interfaces are on.
    #[tokio::test]
    async fn the_admin_routes_are_not_on_the_main_listener() -> Res {
        let dist = std::env::temp_dir().join(format!("judge-api-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dist)?;
        std::fs::write(
            dist.join("index.html"),
            "<!doctype html><title>judge</title>",
        )?;
        for set in [
            nonempty![Interface::Api, Interface::Web],
            nonempty![Interface::Api],
            nonempty![Interface::Web, Interface::Mcp],
        ] {
            let (router, _) = test_app_serving(10, &dist, &Interfaces::of(&set));
            for path in [
                "/admin",
                "/admin/",
                "/admin/calls/x",
                "/admin/api/calls",
                "/admin/api/usage",
            ] {
                let req = Request::get(path)
                    .header("authorization", "Bearer 0123456789abcdef0123456789abcdef")
                    .body(Body::empty())?;
                let res = router.clone().oneshot(req).await?;
                assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path} on {set:?}");
                let bytes = res.into_body().collect().await?.to_bytes();
                assert!(
                    !String::from_utf8_lossy(&bytes).contains("<title>"),
                    "{path} served the page"
                );
            }
        }
        std::fs::remove_dir_all(&dist)?;
        Ok(())
    }

    /// `/api/health` is not an interface: the compose healthcheck and the
    /// tunnel's readiness read it whatever the operator switched off.
    #[tokio::test]
    async fn health_is_served_whatever_is_switched_off() -> Res {
        for set in [
            nonempty![Interface::Api],
            nonempty![Interface::Web],
            nonempty![Interface::Mcp],
        ] {
            let (router, _) =
                test_app_serving(10, Path::new("does-not-exist"), &Interfaces::of(&set));
            let res = router
                .oneshot(Request::get("/api/health").body(Body::empty())?)
                .await?;
            assert_eq!(res.status(), StatusCode::OK);
        }
        Ok(())
    }

    /// The source offer is owed on every interface, so `/api/about` sits beside
    /// `/api/health` outside the opt-in set, and carries what the offer
    /// holds: the repository, the commit and its link, and the notice.
    #[tokio::test]
    async fn about_is_served_whatever_is_switched_off_and_carries_the_offer() -> Res {
        for set in [
            nonempty![Interface::Api],
            nonempty![Interface::Web],
            nonempty![Interface::Mcp],
        ] {
            let (router, _) =
                test_app_serving(10, Path::new("does-not-exist"), &Interfaces::of(&set));
            let res = router
                .oneshot(Request::get("/api/about").body(Body::empty())?)
                .await?;
            assert_eq!(res.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(res.into_body(), 4096).await?;
            let j: serde_json::Value = serde_json::from_slice(&bytes)?;
            assert_eq!(
                j.get("repository"),
                Some(&serde_json::json!(judge_core::source::DEFAULT_REPOSITORY))
            );
            assert_eq!(j.get("commit"), Some(&serde_json::Value::Null));
            assert_eq!(
                j.get("license"),
                Some(&serde_json::json!("AGPL-3.0-or-later"))
            );
            assert_eq!(
                j.get("operator_email"),
                Some(&serde_json::json!("judge@example.org"))
            );
            assert_eq!(j.get("operator_discord"), Some(&serde_json::Value::Null));
            assert!(
                j.get("notice")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|n| n.contains(judge_core::source::COPYRIGHT)
                        && n.contains("judge@example.org")),
                "{j}"
            );
        }
        Ok(())
    }

    /// Serves one public call (id 1) and a page holding it.
    struct StubFeed;

    fn stub_entry() -> judge_bot::feed::Entry {
        judge_bot::feed::Entry {
            id: CallId::new(Uuid::from_u128(1)),
            asked_at: "2026-10-18T12:00:00Z".into(),
            asked: judge_bot::feed::Asked::Discord,
            asker: Some("alice".into()),
            question: "does trample stack?".into(),
            answer: "No.".into(),
            confidence: Confidence::High,
            source: Source::Cr,
            cr_version: "20260819".into(),
            citations: vec![],
            cards: vec![],
            ratings: judge_bot::feed::Ratings {
                judge: Some(Score::Partial),
                ..judge_bot::feed::Ratings::default()
            },
            retired: None,
        }
    }

    #[async_trait]
    impl PublicCalls for StubFeed {
        async fn page(
            &self,
            _filter: &judge_bot::feed::Filter,
            after: Option<judge_bot::feed::Cursor>,
        ) -> Result<judge_bot::feed::Page, JudgeError> {
            Ok(judge_bot::feed::Page {
                entries: if after.is_none() {
                    vec![stub_entry()]
                } else {
                    vec![]
                },
                next: after
                    .is_none()
                    .then(|| "1.00000000000000000000000000000001".parse().ok())
                    .flatten(),
                names: judge_bot::feed::Names::default(),
            })
        }

        async fn call(
            &self,
            id: CallId,
        ) -> Result<Option<(judge_bot::feed::Entry, judge_bot::feed::Names)>, JudgeError> {
            Ok((id == CallId::new(Uuid::from_u128(1)))
                .then(|| (stub_entry(), judge_bot::feed::Names::default())))
        }
    }

    /// The web page with the feed served from [`StubFeed`] (or not), open
    /// or closed for an upgrade.
    fn feed_app(feed: bool, availability: Availability) -> Router {
        let store = Arc::new(StubStore::default());
        let cfg = test_cfg(10, Path::new("does-not-exist"));
        let app = App::new(
            stub_deps(),
            store as Arc<dyn CallStore>,
            &cfg,
            SourceOffer::upstream(judge_core::Commit::Unknown),
            test_operator(),
        )
        .with_availability(availability);
        let app = if feed {
            app.with_public_calls(Arc::new(StubFeed))
        } else {
            app
        };
        router(
            Arc::new(app),
            &Interfaces::of(&nonempty![Interface::Web]),
            Path::new("does-not-exist"),
        )
    }

    async fn get_json(
        router: &Router,
        path: &str,
    ) -> Result<(StatusCode, serde_json::Value), Box<dyn std::error::Error>> {
        let res = router
            .clone()
            .oneshot(Request::get(path).body(Body::empty())?)
            .await?;
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 16).await?;
        Ok((status, serde_json::from_slice(&bytes)?))
    }

    fn kind(j: &serde_json::Value) -> Option<&str> {
        j.get("kind").and_then(serde_json::Value::as_str)
    }

    #[tokio::test]
    async fn the_feed_pages_and_a_call_has_its_own_address() -> Res {
        let app = feed_app(true, Availability::open());
        let (status, j) = get_json(&app, "/api/feed").await?;
        assert_eq!(status, StatusCode::OK, "{j}");
        let first = j.pointer("/entries/0").ok_or("an entry")?;
        assert_eq!(first.get("interface"), Some(&serde_json::json!("discord")));
        assert_eq!(first.get("asker"), Some(&serde_json::json!("alice")));
        assert_eq!(
            first.pointer("/ratings/judge"),
            Some(&serde_json::json!("partial"))
        );
        assert!(first.get("asker_id").is_none(), "{first}");
        let next = j
            .get("next")
            .and_then(serde_json::Value::as_str)
            .ok_or("next")?;
        let (status, j) = get_json(
            &app,
            &format!("/api/feed?cursor={next}&interface=discord&user=@alice"),
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{j}");
        assert_eq!(j.get("next"), Some(&serde_json::Value::Null));

        let one = Uuid::from_u128(1);
        let (status, j) = get_json(&app, &format!("/api/calls/{one}")).await?;
        assert_eq!(status, StatusCode::OK, "{j}");
        assert_eq!(j.get("id"), Some(&serde_json::json!(one)));
        for missing in [Uuid::from_u128(2).to_string(), "not-a-call".to_owned()] {
            let (status, j) = get_json(&app, &format!("/api/calls/{missing}")).await?;
            assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
            assert_eq!(kind(&j), Some("error"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_feed_query_that_does_not_read_is_a_400() -> Res {
        let app = feed_app(true, Availability::open());
        let long = format!("q={}", "x".repeat(judge_bot::feed::MAX_SEARCH_CHARS + 1));
        for q in [
            "cursor=nope",
            "interface=telegram",
            "user=not%20a%20name",
            "cursor=1.2.3",
            long.as_str(),
        ] {
            let (status, j) = get_json(&app, &format!("/api/feed?{q}")).await?;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{q}: {j}");
            assert_eq!(kind(&j), Some("error"), "{q}");
        }
        Ok(())
    }

    /// `JUDGE_FEED=false` (no reader): no endpoints, a JSON 404 rather than
    /// the page, and `/api/about` says so.
    #[tokio::test]
    async fn a_feed_turned_off_is_not_served_and_about_says_so() -> Res {
        for on in [true, false] {
            let app = feed_app(on, Availability::open());
            let (_, about) = get_json(&app, "/api/about").await?;
            assert_eq!(about.get("feed"), Some(&serde_json::json!(on)));
            assert!(
                about.get("repository").is_some(),
                "the offer is flattened in"
            );
            if !on {
                for path in [
                    "/api/feed",
                    "/api/calls/00000000-0000-0000-0000-000000000001",
                ] {
                    let (status, j) = get_json(&app, path).await?;
                    assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
                    assert_eq!(kind(&j), Some("error"), "{path}");
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn feed_reads_wait_out_an_upgrade_and_have_their_own_limit() -> Res {
        let closed = feed_app(true, Availability::upgrading());
        let (status, j) = get_json(&closed, "/api/feed").await?;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(kind(&j), Some("maintenance"));

        // The question limit is 10 here; reads are counted apart.
        let app = feed_app(true, Availability::open());
        for _ in 0..crate::feed::READ_LIMIT {
            assert_eq!(get_json(&app, "/api/feed").await?.0, StatusCode::OK);
        }
        let (status, j) = get_json(&app, "/api/calls/00000000-0000-0000-0000-000000000001").await?;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(kind(&j), Some("rate_limited"));
        Ok(())
    }

    /// `freshness` is what the data status read, and null when it read
    /// nothing; the offer around it is served either way.
    #[tokio::test]
    async fn about_carries_the_freshness_or_null() -> Res {
        struct Fixed(Option<Freshness>);
        #[async_trait]
        impl DataStatus for Fixed {
            async fn freshness(&self) -> Option<Freshness> {
                self.0.clone()
            }
        }
        let fresh = Freshness {
            cr_version: Some(judge_core::CrVersion::try_new("20260925".to_owned())?),
            refreshed_secs_ago: Some(10_800),
            last_refresh_failed: true,
        };
        for (status, want) in [
            (
                Some(fresh),
                serde_json::json!({
                    "cr_version": "20260925",
                    "refreshed_secs_ago": 10_800,
                    "last_refresh_failed": true,
                }),
            ),
            (None, serde_json::Value::Null),
        ] {
            let app = Arc::new(
                App::new(
                    stub_deps(),
                    Arc::new(StubStore::default()),
                    &test_cfg(10, Path::new("does-not-exist")),
                    SourceOffer::upstream(judge_core::Commit::Unknown),
                    test_operator(),
                )
                .with_data_status(Arc::new(Fixed(status))),
            );
            let router = router(
                app,
                &Interfaces::of(&nonempty![Interface::Api]),
                Path::new("does-not-exist"),
            );
            let res = router
                .oneshot(Request::get("/api/about").body(Body::empty())?)
                .await?;
            assert_eq!(res.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(res.into_body(), 4096).await?;
            let j: serde_json::Value = serde_json::from_slice(&bytes)?;
            assert_eq!(j.get("freshness"), Some(&want), "{j}");
            assert!(j.get("notice").is_some(), "{j}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn health_answers_ok() -> Res {
        let (router, _) = test_app(10);
        let req = Request::get("/api/health").body(Body::empty())?;
        let res = router.oneshot(req).await?;
        assert_eq!(res.status(), StatusCode::OK);
        Ok(())
    }

    /// A probe decides the status: a failing dependency is 503 with its name
    /// in the body, so a healthcheck flips and an operator can read why.
    #[tokio::test]
    async fn health_reports_the_probe() -> Res {
        struct Fixed(Result<(), String>);
        #[async_trait]
        impl Probe for Fixed {
            async fn probe(&self) -> Result<(), String> {
                self.0.clone()
            }
        }
        for (probe, status, body) in [
            (Ok(()), StatusCode::OK, "ok"),
            (
                Err("database: connection refused".to_owned()),
                StatusCode::SERVICE_UNAVAILABLE,
                "database: connection refused",
            ),
        ] {
            let app = Arc::new(
                App::new(
                    stub_deps(),
                    Arc::new(StubStore::default()),
                    &test_cfg(10, Path::new("does-not-exist")),
                    SourceOffer::upstream(judge_core::Commit::Unknown),
                    test_operator(),
                )
                .with_probe(Arc::new(Fixed(probe))),
            );
            let router = router(
                app,
                &Interfaces::of(&nonempty![Interface::Api]),
                Path::new("does-not-exist"),
            );
            let res = router
                .oneshot(Request::get("/api/health").body(Body::empty())?)
                .await?;
            assert_eq!(res.status(), status);
            let bytes = axum::body::to_bytes(res.into_body(), 1024).await?;
            assert_eq!(std::str::from_utf8(&bytes)?, body);
        }
        Ok(())
    }

    /// While a major upgrade migrates, a question gets the maintenance reply
    /// (503, `Retry-After`), runs nothing, is stored nowhere and costs no
    /// rate allowance, and `/api/health` fails; once open, all is as before.
    #[tokio::test]
    async fn a_closed_instance_answers_with_maintenance_and_fails_health() -> Res {
        let store = Arc::new(StubStore::default());
        let availability = judge_bot::maintenance::Availability::upgrading();
        let app = Arc::new(
            App::new(
                stub_deps(),
                Arc::clone(&store) as Arc<dyn CallStore>,
                &test_cfg(1, Path::new("does-not-exist")),
                SourceOffer::upstream(judge_core::Commit::Unknown),
                test_operator(),
            )
            .with_probe(Arc::new(Maintenance(availability.clone())))
            .with_availability(availability.clone()),
        );
        let router = router(
            app,
            &Interfaces::of(&nonempty![Interface::Api]),
            Path::new("does-not-exist"),
        );
        let ask = || {
            Request::post("/api/judge")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"question":"does bolt kill bob?"}"#))
        };
        for _ in 0..3 {
            let res = router.clone().oneshot(ask()?).await?;
            assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(
                res.headers()
                    .get("retry-after")
                    .map(axum::http::HeaderValue::as_bytes),
                Some(&b"30"[..])
            );
            let bytes = axum::body::to_bytes(res.into_body(), 4096).await?;
            let body: serde_json::Value = serde_json::from_slice(&bytes)?;
            assert_eq!(body.get("kind"), Some(&"maintenance".into()), "{body}");
            assert_eq!(body.get("retry_after_secs"), Some(&30.into()));
        }
        assert!(persisted(&store).is_empty());
        let health = router
            .clone()
            .oneshot(Request::get("/api/health").body(Body::empty())?)
            .await?;
        assert_eq!(health.status(), StatusCode::SERVICE_UNAVAILABLE);
        availability.set_open();
        let health = router
            .clone()
            .oneshot(Request::get("/api/health").body(Body::empty())?)
            .await?;
        assert_eq!(health.status(), StatusCode::OK);
        // Open, and the maintenance replies took none of the one-a-window allowance.
        let res = router.oneshot(ask()?).await?;
        assert_eq!(res.status(), StatusCode::OK);
        Ok(())
    }

    #[test]
    fn client_ip_prefers_the_peer_unless_cloudflare_is_the_ingress() -> Res {
        use ClientIpSource::{CloudflareConnectingIp as Cf, PeerAddr};
        let mut headers = HeaderMap::new();
        let peer: SocketAddr = "203.0.113.9:44210".parse()?;
        assert_eq!(client_ip(&headers, Some(peer), PeerAddr), peer.ip());
        headers.insert("cf-connecting-ip", "198.51.100.7".parse()?);
        // The header only counts when the operator opted in.
        assert_eq!(client_ip(&headers, Some(peer), PeerAddr), peer.ip());
        assert_eq!(
            client_ip(&headers, Some(peer), Cf),
            "198.51.100.7".parse::<IpAddr>()?
        );
        // A garbage header falls back to the peer; no peer falls back to loopback.
        headers.insert("cf-connecting-ip", "not-an-ip".parse()?);
        assert_eq!(client_ip(&headers, Some(peer), Cf), peer.ip());
        assert_eq!(
            client_ip(&HeaderMap::new(), None, Cf),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        Ok(())
    }

    #[test]
    fn a_forged_x_forwarded_for_cannot_pick_the_rate_limit_bucket() -> Res {
        // Cloudflare appends to a caller-supplied X-Forwarded-For instead of
        // replacing it, so its first hop is whatever the caller wrote. Honouring
        // it would hand every request a fresh bucket against a paid endpoint.
        use ClientIpSource::{CloudflareConnectingIp as Cf, PeerAddr};
        let peer: SocketAddr = "203.0.113.9:44210".parse()?;
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 203.0.113.9".parse()?);
        assert_eq!(client_ip(&headers, Some(peer), PeerAddr), peer.ip());
        assert_eq!(client_ip(&headers, Some(peer), Cf), peer.ip());
        // Even alongside a genuine CF-Connecting-IP, the forged header loses.
        headers.insert("cf-connecting-ip", "198.51.100.7".parse()?);
        assert_eq!(
            client_ip(&headers, Some(peer), Cf),
            "198.51.100.7".parse::<IpAddr>()?
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_shutdown_answers_the_request_in_flight_and_refuses_new_ones() -> Res {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (began, in_handler) = tokio::sync::oneshot::channel::<()>();
        let began = Arc::new(Mutex::new(Some(began)));
        let slow = get(move || {
            let began = Arc::clone(&began);
            async move {
                if let Some(tx) = began.lock().ok().and_then(|mut b| b.take()) {
                    let _ = tx.send(());
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                "done"
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let shutdown = Shutdown::new(Duration::from_secs(10));
        let serving = tokio::spawn({
            let s = shutdown.clone();
            async move { serve_on(listener, Router::new().route("/slow", slow), &s).await }
        });
        let mut conn = tokio::net::TcpStream::connect(addr).await?;
        conn.write_all(b"GET /slow HTTP/1.1\r\nHost: test\r\n\r\n")
            .await?;
        in_handler.await?;
        shutdown.request("test");
        // The request in flight is answered, and its connection closed after it.
        let mut reply = String::new();
        tokio::time::timeout(Duration::from_secs(5), conn.read_to_string(&mut reply)).await??;
        assert!(
            reply.starts_with("HTTP/1.1 200") && reply.ends_with("done"),
            "{reply}"
        );
        tokio::time::timeout(Duration::from_secs(5), serving).await???;
        assert!(
            tokio::net::TcpStream::connect(addr).await.is_err(),
            "the listener is closed"
        );
        Ok(())
    }
}
