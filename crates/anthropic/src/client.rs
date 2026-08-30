//! Minimal `POST /v1/messages` client with bounded retries.
//!
//! Non-streaming only: every request in this project keeps `max_tokens`
//! at or below 16k, which stays within the non-streaming HTTP timeout
//! guidance, so `eventsource-stream` is not needed yet.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use reqwest::StatusCode;

use crate::{
    API_VERSION,
    wire::{ApiErrorBody, MessagesRequest, MessagesResponse, Usage},
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// Attempts per `messages` call (1 initial + 2 retries), matching the official SDKs' `max_retries = 2`.
const MAX_ATTEMPTS: u32 = 3;
/// Backoff for a retryable failure that carries no `retry-after`.
const BASE_BACKOFF: Duration = Duration::from_millis(500);
/// Upper bound honoured for `retry-after`.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// `JUDGE_MAX_USD` fallback, and the cap every [`Client::new`] starts with:
/// deliberately conservative for a prototype on a small credit balance.
///
/// Enforcement is by *reservation*: before a request is sent, a worst-case
/// estimate of its cost (`max_tokens` at the output price plus the request
/// body at the input price) is added to the shared counter and the request
/// is refused if that would exceed the cap; after the response the
/// reservation is replaced by the actual usage. Concurrent callers therefore
/// cannot collectively overshoot by more than the estimation error of one
/// call, and a 2xx body that fails to decode is still billed from its
/// `usage` field when that field parses.
pub const DEFAULT_MAX_SPEND_USD: f64 = 5.00;
/// Micro-dollars per dollar (spend is tracked as an integer so `AtomicU64` can hold it).
const MICRO_PER_USD: f64 = 1_000_000.0;

/// USD per million tokens for one model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pricing {
    /// Uncached input tokens.
    pub input: f64,
    /// Output tokens (thinking included).
    pub output: f64,
    /// Prompt-cache reads.
    pub cache_read: f64,
    /// Prompt-cache writes (5-minute TTL).
    pub cache_write: f64,
}

impl Pricing {
    /// Estimated cost of `usage` in USD.
    #[must_use]
    pub fn usd(&self, usage: &Usage) -> f64 {
        // u64 → f64 loses nothing below 2^53 tokens; billing estimates do not need more.
        #[allow(clippy::cast_precision_loss)]
        let tok = |n: u64| n as f64 / 1_000_000.0;
        tok(usage.input_tokens) * self.input
            + tok(usage.output_tokens) * self.output
            + tok(usage.cache_read_input_tokens.unwrap_or(0)) * self.cache_read
            + tok(usage.cache_creation_input_tokens.unwrap_or(0)) * self.cache_write
    }
}

/// Price table, USD per million tokens. Verified 2026-08-29 against the Anthropic
/// pricing page; re-check when a model is added or a price changes.
pub const PRICES: &[(&str, Pricing)] = &[
    ("claude-opus-5", Pricing { input: 5.0, output: 25.0, cache_read: 0.50, cache_write: 6.25 }),
];

/// Pricing for `model`. Unknown models (including fallbacks the server may
/// route to) are priced as Opus 5 so the estimate errs high rather than low.
#[must_use]
pub fn pricing_for(model: &str) -> Pricing {
    PRICES
        .iter()
        .find(|(m, _)| *m == model)
        .or_else(|| PRICES.first())
        .map_or(Pricing { input: 5.0, output: 25.0, cache_read: 0.50, cache_write: 6.25 }, |(_, p)| *p)
}

/// Spend counters and cap shared by every clone of a [`Client`].
#[derive(Debug)]
struct Spend {
    /// Recorded spend plus in-flight reservations, in micro-dollars.
    micro_usd: AtomicU64,
    calls: AtomicU64,
    /// Cap in micro-dollars; shared, so `with_max_spend_usd` on any clone applies to all.
    cap_micro_usd: AtomicU64,
}

impl Spend {
    fn new(cap_micro_usd: u64) -> Self {
        Self { micro_usd: AtomicU64::new(0), calls: AtomicU64::new(0), cap_micro_usd: AtomicU64::new(cap_micro_usd) }
    }

    /// Reserve `estimate` micro-dollars atomically, or report the cap.
    fn reserve(&self, estimate: u64) -> Result<(), ClientError> {
        let cap = self.cap_micro_usd.load(Ordering::Relaxed);
        let mut spent = self.micro_usd.load(Ordering::Relaxed);
        loop {
            if spent >= cap || spent.saturating_add(estimate) > cap {
                return Err(ClientError::SpendCapExceeded { spent: from_micro(spent), cap: from_micro(cap) });
            }
            match self.micro_usd.compare_exchange_weak(spent, spent + estimate, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return Ok(()),
                Err(actual) => spent = actual,
            }
        }
    }

    /// Replace a reservation by the actual cost; returns the new total.
    fn settle(&self, reserved: u64, actual: u64) -> u64 {
        if actual >= reserved {
            self.micro_usd.fetch_add(actual - reserved, Ordering::Relaxed) + (actual - reserved)
        } else {
            self.micro_usd.fetch_sub(reserved - actual, Ordering::Relaxed) - (reserved - actual)
        }
    }
}

/// `usage` (and `model`) of a 2xx body, parsed leniently so that a response
/// which fails to decode as [`MessagesResponse`] is still billed.
#[derive(Debug, serde::Deserialize)]
struct UsageOnly {
    #[serde(default)]
    model: Option<String>,
    usage: Usage,
}

/// Everything `Client::messages` can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Connection / timeout / TLS.
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    /// Non-2xx status with the API's error body.
    #[error("api {status}: {kind}: {message}")]
    Api {
        /// HTTP status.
        status: StatusCode,
        /// `error.type`, e.g. `rate_limit_error`.
        kind: String,
        /// `error.message`.
        message: String,
    },
    /// 2xx body that does not parse as `MessagesResponse`.
    #[error("bad response body: {0}")]
    Decode(#[from] serde_json::Error),
    /// `ANTHROPIC_API_KEY` unset.
    #[error("ANTHROPIC_API_KEY is not set")]
    MissingApiKey,
    /// Cumulative estimated spend reached the cap; the request was not sent.
    #[error("spend cap exceeded: spent ${spent:.4} of ${cap:.2} cap; request not sent")]
    SpendCapExceeded {
        /// Estimated USD spent so far across all clones of the client.
        spent: f64,
        /// The cap in USD.
        cap: f64,
    },
    /// A spend cap (`JUDGE_MAX_USD`, `--max-usd`, [`Client::with_max_spend_usd`])
    /// is not a finite non-negative number.
    #[error("spend cap is not a finite non-negative number: {0:?}")]
    BadMaxSpend(String),
}

impl ClientError {
    /// 408, 409, 429, 529 and 5xx are retried, as in the official SDKs.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            ClientError::Transport(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            ClientError::Api { status, .. } => {
                matches!(status.as_u16(), 408 | 409 | 429 | 529) || status.is_server_error()
            }
            ClientError::Decode(_)
            | ClientError::MissingApiKey
            | ClientError::SpendCapExceeded { .. }
            | ClientError::BadMaxSpend(_) => false,
        }
    }
}

/// Thin `reqwest` wrapper. Cheap to clone (the connection pool is shared).
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    betas: Vec<String>,
    spend: Arc<Spend>,
}

// Manual impl: the API key must never reach a `{:?}` log line.
impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.base_url)
            .field("betas", &self.betas)
            .field("api_key", &"<redacted>")
            .field("spent_usd", &self.spent_usd())
            .field("max_spend_usd", &self.max_spend_usd())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client capped at [`DEFAULT_MAX_SPEND_USD`]; see [`Client::with_max_spend_usd`].
    ///
    /// # Errors
    /// If the underlying HTTP client cannot be built.
    pub fn new(api_key: impl Into<String>) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder().timeout(Duration::from_mins(10)).build()?;
        Ok(Self {
            http,
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            betas: Vec::new(),
            spend: Arc::new(Spend::new(to_micro(DEFAULT_MAX_SPEND_USD))),
        })
    }

    /// Read `ANTHROPIC_API_KEY` (and optional `ANTHROPIC_BASE_URL`). The spend
    /// cap comes from `JUDGE_MAX_USD`, defaulting to [`DEFAULT_MAX_SPEND_USD`].
    ///
    /// # Errors
    /// `MissingApiKey`, `BadMaxSpend`, or if the client cannot be built.
    pub fn from_env() -> Result<Self, ClientError> {
        let key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| ClientError::MissingApiKey)?;
        let mut c = Self::new(key)?;
        if let Ok(url) = std::env::var("ANTHROPIC_BASE_URL") {
            c.base_url = url;
        }
        match std::env::var("JUDGE_MAX_USD") {
            Ok(raw) => c.with_max_spend_usd(raw.trim().parse::<f64>().map_err(|_| ClientError::BadMaxSpend(raw.clone()))?),
            Err(_) => Ok(c),
        }
    }

    /// Cap cumulative estimated spend. The cap is shared with every clone
    /// (made before or after this call), like the counter itself. A request
    /// whose worst-case cost estimate would take the total past the cap fails
    /// with [`ClientError::SpendCapExceeded`] without being sent; see
    /// [`DEFAULT_MAX_SPEND_USD`] for the reservation scheme.
    ///
    /// # Errors
    /// `BadMaxSpend` unless `cap_usd` is finite and non-negative.
    pub fn with_max_spend_usd(self, cap_usd: f64) -> Result<Self, ClientError> {
        if !(cap_usd.is_finite() && cap_usd >= 0.0) {
            return Err(ClientError::BadMaxSpend(cap_usd.to_string()));
        }
        self.spend.cap_micro_usd.store(to_micro(cap_usd), Ordering::Relaxed);
        Ok(self)
    }

    /// The cap in USD.
    #[must_use]
    pub fn max_spend_usd(&self) -> f64 {
        from_micro(self.spend.cap_micro_usd.load(Ordering::Relaxed))
    }

    /// Estimated USD spent by this client and all its clones.
    #[must_use]
    pub fn spent_usd(&self) -> f64 {
        from_micro(self.spend.micro_usd.load(Ordering::Relaxed))
    }

    /// Successful `messages` calls by this client and all its clones.
    #[must_use]
    pub fn calls(&self) -> u64 {
        self.spend.calls.load(Ordering::Relaxed)
    }

    /// Override the API origin (proxies, tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Add a beta flag sent as `anthropic-beta` on every request.
    #[must_use]
    pub fn with_beta(mut self, beta: impl Into<String>) -> Self {
        let beta = beta.into();
        if !self.betas.contains(&beta) {
            self.betas.push(beta);
        }
        self
    }

    /// Beta flags currently sent.
    #[must_use]
    pub fn betas(&self) -> &[String] {
        &self.betas
    }

    /// One non-streaming request, retried up to twice on 408/409/429/529/5xx
    /// and transport errors, honouring `retry-after` when present.
    ///
    /// # Errors
    /// Transport failures, non-2xx API errors, undecodable bodies.
    #[tracing::instrument(skip_all, fields(model = %req.model))]
    pub async fn messages(&self, req: &MessagesRequest) -> Result<MessagesResponse, ClientError> {
        self.send_guarded(req).await
    }

    /// The single choke point for outbound requests: reserves a worst-case
    /// cost against the spend cap before anything hits the wire, runs the
    /// retry loop, then replaces the reservation by the actual usage. Every
    /// public send path must go through here.
    async fn send_guarded(&self, req: &MessagesRequest) -> Result<MessagesResponse, ClientError> {
        let reserved = estimate_micro(req);
        self.spend.reserve(reserved)?;
        match self.send_with_retries(req).await {
            Ok(resp) => {
                self.record(reserved, &resp.model, &resp.usage);
                Ok(resp)
            }
            Err((err, billed)) => {
                // A body that was billed but did not decode still counts; anything else frees the reservation.
                match billed {
                    Some(u) => self.record(reserved, u.model.as_deref().unwrap_or_default(), &u.usage),
                    None => {
                        self.spend.settle(reserved, 0);
                    }
                }
                Err(err)
            }
        }
    }

    /// Replace `reserved` by the real cost of a billed response in the shared counters and log it.
    fn record(&self, reserved: u64, model: &str, usage: &Usage) {
        let usd = pricing_for(model).usd(usage);
        let total = self.spend.settle(reserved, to_micro(usd));
        let calls = self.spend.calls.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(
            model,
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            cache_read_tokens = usage.cache_read_input_tokens.unwrap_or(0),
            cache_write_tokens = usage.cache_creation_input_tokens.unwrap_or(0),
            usd = format_args!("{usd:.4}"),
            cumulative_usd = format_args!("{:.4}", from_micro(total)),
            calls,
            "anthropic call"
        );
    }

    /// The error carries the leniently parsed usage of a billed-but-undecodable body, if any.
    async fn send_with_retries(&self, req: &MessagesRequest) -> Result<MessagesResponse, (ClientError, Option<UsageOnly>)> {
        let mut attempt = 1;
        loop {
            match self.messages_once(req).await {
                Ok(resp) => return Ok(resp),
                Err(Failure { err, retry_after, billed: None }) if attempt < MAX_ATTEMPTS && err.is_retryable() => {
                    let delay = retry_after.unwrap_or_else(|| BASE_BACKOFF * 2u32.pow(attempt - 1)).min(MAX_BACKOFF);
                    tracing::warn!(attempt, ?delay, error = %err, "retrying messages request");
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(Failure { err, billed, .. }) => return Err((err, billed)),
            }
        }
    }

    /// Single attempt; the error carries the parsed `retry-after`, if any.
    async fn messages_once(&self, req: &MessagesRequest) -> Result<MessagesResponse, Failure> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let mut builder = self
            .http
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json");
        if !self.betas.is_empty() {
            builder = builder.header("anthropic-beta", self.betas.join(","));
        }
        let resp = builder.json(req).send().await.map_err(|e| Failure::plain(ClientError::Transport(e)))?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = resp.bytes().await.map_err(|e| Failure { err: ClientError::Transport(e), retry_after, billed: None })?;
        if !status.is_success() {
            let (kind, message) = match serde_json::from_slice::<ApiErrorBody>(&body) {
                Ok(b) => (b.error.kind, b.error.message),
                Err(_) => ("unknown".to_owned(), String::from_utf8_lossy(&body).into_owned()),
            };
            return Err(Failure { err: ClientError::Api { status, kind, message }, retry_after, billed: None });
        }
        let parsed: MessagesResponse = serde_json::from_slice(&body).map_err(|e| Failure {
            err: ClientError::Decode(e),
            retry_after: None,
            billed: serde_json::from_slice::<UsageOnly>(&body).ok(),
        })?;
        tracing::debug!(
            input = parsed.usage.input_tokens,
            output = parsed.usage.output_tokens,
            cache_read = ?parsed.usage.cache_read_input_tokens,
            stop = ?parsed.stop_reason,
            "messages ok"
        );
        Ok(parsed)
    }
}

/// One failed attempt.
struct Failure {
    err: ClientError,
    /// Parsed `retry-after`, if the server sent one.
    retry_after: Option<Duration>,
    /// Usage of a 2xx body that did not decode as `MessagesResponse` (billed by Anthropic).
    billed: Option<UsageOnly>,
}

impl Failure {
    fn plain(err: ClientError) -> Self {
        Self { err, retry_after: None, billed: None }
    }
}

/// Worst-case cost of `req` in micro-dollars: `max_tokens` at the output
/// price plus the serialized body at roughly four bytes per token at the
/// uncached input price. Deliberately pessimistic; the reservation is
/// replaced by the real usage afterwards.
fn estimate_micro(req: &MessagesRequest) -> u64 {
    let p = pricing_for(&req.model);
    let body_bytes = serde_json::to_vec(req).map_or(0, |b| b.len());
    #[allow(clippy::cast_precision_loss)]
    let input_tokens = (body_bytes / 4) as f64;
    let usd = (f64::from(req.max_tokens) * p.output + input_tokens * p.input) / 1_000_000.0;
    to_micro(usd)
}

/// USD → micro-dollars, rounded to nearest; negative or NaN clamps to 0.
fn to_micro(usd: f64) -> u64 {
    // Rounded, clamped conversion; values are tiny relative to u64::MAX.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let m = (usd * MICRO_PER_USD).round().max(0.0) as u64;
    m
}

fn from_micro(micro: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let usd = micro as f64 / MICRO_PER_USD;
    usd
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn ok_body(input: u64, output: u64, cache_read: u64, cache_write: u64) -> serde_json::Value {
        serde_json::json!({
            "id": "msg_1", "model": "claude-opus-5", "role": "assistant",
            "content": [{"type": "text", "text": "hi"}], "stop_reason": "end_turn",
            "usage": {
                "input_tokens": input, "output_tokens": output,
                "cache_read_input_tokens": cache_read, "cache_creation_input_tokens": cache_write
            }
        })
    }

    fn req() -> MessagesRequest {
        MessagesRequest {
            model: "claude-opus-5".into(),
            max_tokens: 64,
            system: vec![],
            messages: vec![crate::wire::Message::user_text("hi")],
            tools: vec![],
            tool_choice: None,
            thinking: None,
            output_config: None,
            fallbacks: None,
        }
    }

    #[test]
    fn opus_5_pricing_matches_table() {
        let u = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_input_tokens: Some(1_000_000),
            cache_creation_input_tokens: Some(1_000_000),
        };
        let usd = pricing_for("claude-opus-5").usd(&u);
        assert!((usd - (5.0 + 25.0 + 0.5 + 6.25)).abs() < 1e-9, "{usd}");
        // Unknown models price as Opus 5 (never under-estimate).
        assert_eq!(pricing_for("claude-something-new"), pricing_for("claude-opus-5"));
    }

    #[tokio::test]
    async fn usage_accumulates_and_cap_blocks_without_sending() -> Result<(), ClientError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            // 1M input + 200k output = $5 + $5 = $10 per call.
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(1_000_000, 200_000, 0, 0)))
            .mount(&server)
            .await;
        let client = Client::new("k")?.with_base_url(server.uri()).with_max_spend_usd(15.0)?;
        let clone = client.clone();
        client.messages(&req()).await?;
        assert!((client.spent_usd() - 10.0).abs() < 1e-6, "{}", client.spent_usd());
        clone.messages(&req()).await?;
        assert!((client.spent_usd() - 20.0).abs() < 1e-6, "{}", client.spent_usd());
        assert_eq!(client.calls(), 2);
        assert_eq!(clone.calls(), 2);
        let third = client.messages(&req()).await;
        assert!(
            matches!(third, Err(ClientError::SpendCapExceeded { spent, cap }) if (spent - 20.0).abs() < 1e-6 && (cap - 15.0).abs() < 1e-6),
            "{third:?}"
        );
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), 2);
        assert_eq!(client.calls(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn failed_calls_do_not_count() -> Result<(), ClientError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "type": "error", "error": {"type": "invalid_request_error", "message": "nope"}
            })))
            .mount(&server)
            .await;
        let client = Client::new("k")?.with_base_url(server.uri());
        assert!(matches!(client.messages(&req()).await, Err(ClientError::Api { .. })));
        assert_eq!(client.calls(), 0);
        assert!(client.spent_usd().abs() < f64::EPSILON);
        Ok(())
    }

    #[tokio::test]
    async fn undecodable_2xx_body_is_still_billed() -> Result<(), ClientError> {
        let server = MockServer::start().await;
        // `role` missing: not a MessagesResponse, but `usage` parses.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_1", "model": "claude-opus-5",
                "usage": {"input_tokens": 1_000_000, "output_tokens": 0}
            })))
            .mount(&server)
            .await;
        let client = Client::new("k")?.with_base_url(server.uri());
        assert!(matches!(client.messages(&req()).await, Err(ClientError::Decode(_))));
        assert!((client.spent_usd() - 5.0).abs() < 1e-6, "{}", client.spent_usd());
        assert_eq!(client.calls(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn reservation_refuses_a_request_that_cannot_fit() -> Result<(), ClientError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(1, 1, 0, 0)))
            .mount(&server)
            .await;
        // 16k output tokens at $25/MTok reserve $0.40; a $0.10 cap cannot fit it.
        let client = Client::new("k")?.with_base_url(server.uri()).with_max_spend_usd(0.10)?;
        let big = MessagesRequest { max_tokens: 16_000, ..req() };
        assert!(matches!(client.messages(&big).await, Err(ClientError::SpendCapExceeded { .. })));
        // The small request fits, and afterwards the reservation is gone.
        client.messages(&req()).await?;
        assert!(client.spent_usd() < 0.001, "{}", client.spent_usd());
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), 1);
        Ok(())
    }

    #[test]
    fn cap_is_shared_by_clones_and_validated() -> Result<(), ClientError> {
        let a = Client::new("k")?;
        assert!((a.max_spend_usd() - DEFAULT_MAX_SPEND_USD).abs() < 1e-9);
        let b = a.clone();
        let a = a.with_max_spend_usd(1.5)?;
        assert!((b.max_spend_usd() - 1.5).abs() < 1e-9, "clone made before the call shares the cap");
        for bad in [f64::NAN, -1.0, f64::INFINITY] {
            assert!(matches!(a.clone().with_max_spend_usd(bad), Err(ClientError::BadMaxSpend(_))), "{bad}");
        }
        Ok(())
    }

    #[test]
    fn debug_redacts_api_key() -> Result<(), ClientError> {
        let c = Client::new("sk-ant-super-secret")?.with_beta("x-beta");
        let s = format!("{c:?}");
        assert!(!s.contains("super-secret"), "{s}");
        assert!(s.contains("<redacted>") && s.contains("x-beta"), "{s}");
        Ok(())
    }

    #[test]
    fn retryable_statuses() {
        let api = |code: u16| ClientError::Api { status: StatusCode::from_u16(code).unwrap_or(StatusCode::OK), kind: String::new(), message: String::new() };
        for code in [408, 409, 429, 500, 502, 529] {
            assert!(api(code).is_retryable(), "{code}");
        }
        for code in [400, 401, 403, 404, 413] {
            assert!(!api(code).is_retryable(), "{code}");
        }
        assert!(!ClientError::MissingApiKey.is_retryable());
        assert!(!ClientError::SpendCapExceeded { spent: 1.0, cap: 1.0 }.is_retryable());
    }

    #[test]
    fn from_env_without_key_is_typed() {
        // Only meaningful when the variable is absent in the test environment.
        if std::env::var_os("ANTHROPIC_API_KEY").is_none() {
            assert!(matches!(Client::from_env(), Err(ClientError::MissingApiKey)));
        }
    }
}
