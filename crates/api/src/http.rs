//! The axum layer: shared state, routes and handlers. Everything that can be
//! pure lives in [`crate::shape`]; this file is the glue.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use axum::{
    Json, Router,
    extract::{ConnectInfo, FromRequestParts, State},
    http::{HeaderMap, StatusCode, request::Parts},
    routing::{get, post},
};
use judge_bot::discord::{capture::CapturingRetriever, render};
use judge_core::{CallStore, Deps, JudgeError, Question, Retriever, Validated, Verdict, judge};
use judge_llm::SpendMeter;
use tokio::sync::{Semaphore, SemaphorePermit};
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

use crate::{
    config::{ApiConfig, ClientIpSource},
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

/// Shared state behind every route.
pub struct App {
    deps: Deps,
    capture: Arc<CapturingRetriever>,
    store: Arc<dyn CallStore>,
    meter: SpendMeter,
    permits: Arc<Semaphore>,
    limiter: RateLimiter,
    history_len: usize,
    client_ip: ClientIpSource,
    probe: Option<Arc<dyn Probe>>,
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
    /// Wire the shared state. `meter` must be the one the models inside
    /// `deps` bill to, so its counters reflect the judge runs.
    #[must_use]
    pub fn new(
        mut deps: Deps,
        store: Arc<dyn CallStore>,
        meter: SpendMeter,
        cfg: &ApiConfig,
    ) -> Self {
        let capture = Arc::new(CapturingRetriever::new(Arc::clone(&deps.retriever)));
        deps.retriever = Arc::clone(&capture) as Arc<dyn Retriever>;
        Self {
            deps,
            capture,
            store,
            meter,
            permits: Arc::new(Semaphore::new(cfg.max_concurrent)),
            limiter: RateLimiter::new(cfg.rate_limit, cfg.rate_window),
            history_len: cfg.history_len,
            client_ip: cfg.client_ip,
            probe: None,
        }
    }

    /// Make `GET /api/health` check `probe` (the database) instead of only
    /// answering that the process is up.
    #[must_use]
    pub fn with_probe(mut self, probe: Arc<dyn Probe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// The judge slots, to share with another front door in this process
    /// (the MCP transport), so that both together stay under `JUDGE_CONCURRENCY`.
    #[must_use]
    pub fn permits(&self) -> Arc<Semaphore> {
        Arc::clone(&self.permits)
    }

    /// A judge slot, waiting up to [`ACQUIRE_WAIT`] for one; `None` means "busy".
    async fn acquire(&self) -> Option<SemaphorePermit<'_>> {
        match tokio::time::timeout(ACQUIRE_WAIT, self.permits.acquire()).await {
            Ok(Ok(p)) => Some(p),
            // The semaphore is never closed; a timeout is the only real `None`.
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// Run the pipeline for `q` and shape the reply. Mirrors the Discord
    /// adapter's flow (history → judge → persist) minus rating buttons.
    async fn answer(&self, q: &Question, ip: IpAddr) -> ApiReply {
        let history = match self.store.history(&q.thread_id, self.history_len).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(
                    error = format_args!("{e:#}"),
                    "session history unavailable; judging without it"
                );
                vec![]
            }
        };
        let (t0, usd0, calls0) = (Instant::now(), self.meter.spent_usd(), self.meter.calls());
        let result = judge(&self.deps, q, &history).await;
        let captured = self.capture.take(q);
        tracing::info!(
            %ip,
            thread = %q.thread_id,
            elapsed_ms = t0.elapsed().as_millis(),
            usd = format_args!("{:.4}", self.meter.spent_usd() - usd0),
            llm_calls = self.meter.calls() - calls0,
            outcome = outcome(&result),
            "POST /api/judge"
        );
        match result {
            Ok(v) => {
                if let Some(ctx) = captured.as_ref() {
                    // Persisting feeds session history and the prior-call
                    // retrieval leg; web calls arrive unrated, exactly like an
                    // unrated Discord call. Failure only costs those.
                    if let Err(e) = self.store.persist(q, &v, ctx).await {
                        tracing::error!(
                            error = format_args!("{e:#}"),
                            "persist failed; answering anyway"
                        );
                    }
                } else {
                    tracing::warn!("no captured context for the question; call not persisted");
                }
                shape::answer(&v, captured.as_ref())
            }
            Err(e) => {
                // The line above classifies the outcome; a genuine failure also
                // needs its cause, or the operator sees `outcome=upstream` and
                // nothing else. `thread` correlates this with that line — the
                // IP is deliberately not repeated here, because unlike the
                // metadata above, `{e:#}` can carry fragments of the question.
                if e.is_operator_failure() {
                    tracing::warn!(thread = %q.thread_id, error = format_args!("{e:#}"), "judge failed");
                }
                shape::error(&e)
            }
        }
    }
}

const fn outcome(r: &Result<Verdict<Validated>, JudgeError>) -> &'static str {
    match r {
        Ok(_) => "answered",
        Err(JudgeError::AmbiguousCards(_)) => "ambiguous",
        Err(JudgeError::CardsNotFound(_)) => "not_found",
        Err(JudgeError::OutOfScope(_)) => "out_of_scope",
        Err(JudgeError::BadCitation(_)) => "bad_citation",
        Err(JudgeError::MalformedCitation(_)) => "malformed_citation",
        Err(JudgeError::EmptyVerdict(_)) => "empty_verdict",
        Err(JudgeError::LlmRefused) => "refused",
        Err(JudgeError::Upstream(_)) => "upstream",
    }
}

/// The routes: `POST /api/judge`, `GET /api/health`, and the built web client
/// as the fallback (unknown paths get `index.html`, so a client-side route
/// refresh still loads the app). The MCP router ([`crate::mcp::router`]) is
/// merged *into* this one, so this fallback wins and the token gate stays
/// on `/mcp` alone.
pub fn router(app: Arc<App>, web_dist: &Path) -> Router {
    let files =
        ServeDir::new(web_dist).not_found_service(ServeFile::new(web_dist.join("index.html")));
    Router::new()
        .route("/api/judge", post(judge_route))
        .route("/api/health", get(health))
        .fallback_service(files)
        .with_state(app)
}

/// Bind `cfg.addr` and serve until the listener fails.
///
/// # Errors
/// Binding the address, or a fatal accept-loop error.
/// Bind `cfg.addr` and serve `router` until the listener fails.
///
/// # Errors
/// Binding or accepting.
pub async fn serve(cfg: &ApiConfig, router: Router) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(cfg.addr)
        .await
        .with_context(|| format!("bind {}", cfg.addr))?;
    tracing::info!(addr = %cfg.addr, web_dist = %cfg.web_dist.display(), "HTTP adapter listening");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("serve HTTP")
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

/// The peer address, when the server was started with connect info (tests
/// drive the router directly and have none). Infallible by construction.
struct PeerAddr(Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0),
        ))
    }
}

async fn judge_route(
    State(app): State<Arc<App>>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    Json(req): Json<JudgeRequest>,
) -> (StatusCode, Json<ApiReply>) {
    if let Err(message) = shape::validate(&req) {
        return (StatusCode::BAD_REQUEST, Json(ApiReply::Error { message }));
    }
    let ip = client_ip(&headers, peer, app.client_ip);
    if !app.limiter.allow(ip) {
        tracing::info!(%ip, "rate limited");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ApiReply::RateLimited {
                message: shape::RATE_LIMITED.to_owned(),
            }),
        );
    }
    let Some(_permit) = app.acquire().await else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ApiReply::Busy {
                message: render::BUSY.to_owned(),
            }),
        );
    };
    // No session id still gets an answer, just no follow-up history.
    let session = req.session_id.unwrap_or_else(Uuid::new_v4);
    let q = Question {
        thread_id: format!("web:{session}"),
        text: shape::question_text(&req),
    };
    (StatusCode::OK, Json(app.answer(&q, ip).await))
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
fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>, source: ClientIpSource) -> IpAddr {
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
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use judge_core::{
        CallId, Card, CardId, Category, CategoryGuess, Confidence, Context, CrVersion, Extraction,
        Extractor, Face, Layout, MatchedVia, Qa, Resolution, Resolver, RuleChunk, RuleId, Score,
        Source, Synthesizer, Unvalidated,
    };
    use nonempty::NonEmpty;
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
        async fn extract(&self, q: &Question, _h: &[Qa]) -> Result<Extraction, JudgeError> {
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

    /// Records persisted questions; returns no history.
    #[derive(Default)]
    struct StubStore {
        persisted: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl CallStore for StubStore {
        async fn persist(
            &self,
            q: &Question,
            _v: &Verdict<Validated>,
            _ctx: &Context,
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
        ) -> Result<(), JudgeError> {
            Ok(())
        }
        async fn history(&self, _thread: &str, _n: usize) -> Result<Vec<Qa>, JudgeError> {
            Ok(vec![])
        }
        async fn forget_user(&self, _user: &str) -> Result<u64, JudgeError> {
            Ok(0)
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
        let store = Arc::new(StubStore::default());
        let cfg = test_cfg(rate_limit, dist);
        let app = Arc::new(App::new(
            stub_deps(),
            Arc::clone(&store) as Arc<dyn CallStore>,
            SpendMeter::new(),
            &cfg,
        ));
        (router(app, &cfg.web_dist), store)
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
        let router = web.merge(crate::mcp::router(crate::mcp::tests::Echo, TOKEN));
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
                    SpendMeter::new(),
                    &test_cfg(10, Path::new("does-not-exist")),
                )
                .with_probe(Arc::new(Fixed(probe))),
            );
            let router = router(app, Path::new("does-not-exist"));
            let res = router
                .oneshot(Request::get("/api/health").body(Body::empty())?)
                .await?;
            assert_eq!(res.status(), status);
            let bytes = axum::body::to_bytes(res.into_body(), 1024).await?;
            assert_eq!(std::str::from_utf8(&bytes)?, body);
        }
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
}
