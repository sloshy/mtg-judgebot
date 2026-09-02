//! The Messages API backend: [`Endpoint`] (which door, with what auth) and
//! [`Anthropic`], the [`Backend`] over it. No spend cap here — that is
//! [`judge_llm::Metered`], the only way a backend becomes the pipeline's
//! `ChatModel` — and no retry loop of its own: the request goes through
//! [`judge_llm::http::post_with_retries`].

use std::{fmt, time::Duration};

use async_trait::async_trait;
use judge_llm::{
    Backend, Capabilities, ChatRequest, ChatResponse, LlmError, StructuredOutput,
    http::{Reply, post_with_retries},
};

use crate::{
    API_VERSION, DEFAULT_MODEL,
    convert::{BACKEND, betas_for, from_wire, to_wire, usage_of},
    wire::{ApiErrorBody, MessagesResponse},
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// An API key. Its `Debug` is redacted so it can never reach a `{:?}` log line.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// The key as sent on the wire.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for ApiKey {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ApiKey {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Which door the Messages API is reached through. The body is the same
/// everywhere; the URL and the auth differ. An enum so that a new door is an
/// exhaustive-match compile error, not a config typo.
#[derive(Clone, Debug)]
pub enum Endpoint {
    /// Anthropic's first-party API: `x-api-key` against `{base_url}/v1/messages`.
    Direct {
        /// API origin, without the `/v1/messages` path.
        base_url: String,
        /// `x-api-key`.
        api_key: ApiKey,
    },
}

impl Endpoint {
    /// [`Endpoint::Direct`] at the public origin.
    #[must_use]
    pub fn direct(api_key: impl Into<ApiKey>) -> Self {
        Self::Direct { base_url: DEFAULT_BASE_URL.to_owned(), api_key: api_key.into() }
    }

    /// [`Endpoint::Direct`] from `ANTHROPIC_API_KEY` and optional
    /// `ANTHROPIC_BASE_URL`. A blank value counts as unset for both (a copied
    /// `.env.example` ships `ANTHROPIC_API_KEY=`), as the other binaries
    /// treat their keys, so a missing key fails at startup and not per request.
    ///
    /// # Errors
    /// `MissingApiKey` when the key is unset or blank.
    pub fn from_env() -> Result<Self, LlmError> {
        let set = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let api_key = set("ANTHROPIC_API_KEY").ok_or(LlmError::MissingApiKey { var: "ANTHROPIC_API_KEY" })?;
        let base_url = set("ANTHROPIC_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        Ok(Self::Direct { base_url, api_key: api_key.into() })
    }

    /// The messages URL.
    fn url(&self) -> String {
        match self {
            Endpoint::Direct { base_url, .. } => format!("{}/v1/messages", base_url.trim_end_matches('/')),
        }
    }

    /// Add this door's auth to a request.
    fn authorize(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Endpoint::Direct { api_key, .. } => builder.header("x-api-key", api_key.expose()),
        }
    }

    /// What this door supports server-side.
    fn capabilities(&self) -> Capabilities {
        match self {
            Endpoint::Direct { .. } => Capabilities {
                structured_output: StructuredOutput::Enforced,
                strict_tools: true,
                effort: true,
                cache_hints: true,
                refusal_fallbacks: true,
            },
        }
    }
}

/// The Messages API as a [`Backend`]. Cheap to clone (the connection pool is shared).
#[derive(Clone, Debug)]
pub struct Anthropic {
    http: reqwest::Client,
    endpoint: Endpoint,
    model: String,
    betas: Vec<String>,
}

impl Anthropic {
    /// Over `endpoint`, at [`DEFAULT_MODEL`].
    ///
    /// # Errors
    /// If the underlying HTTP client cannot be built.
    pub fn new(endpoint: Endpoint) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder().timeout(Duration::from_mins(10)).build()?;
        Ok(Self { http, endpoint, model: DEFAULT_MODEL.to_owned(), betas: Vec::new() })
    }

    /// [`Endpoint::from_env`] at [`DEFAULT_MODEL`].
    ///
    /// # Errors
    /// `MissingApiKey`, or if the client cannot be built.
    pub fn from_env() -> Result<Self, LlmError> {
        Self::new(Endpoint::from_env()?)
    }

    /// Use another model id.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Add a beta flag sent as `anthropic-beta` on every request (on top of
    /// the ones a request itself needs, such as the fallbacks beta).
    #[must_use]
    pub fn with_beta(mut self, beta: impl Into<String>) -> Self {
        let beta = beta.into();
        if !self.betas.contains(&beta) {
            self.betas.push(beta);
        }
        self
    }

    /// Beta flags sent on every request.
    #[must_use]
    pub fn betas(&self) -> &[String] {
        &self.betas
    }

    /// The door in use.
    #[must_use]
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Decode one reply: the API's error body on a non-2xx status, the
    /// response otherwise; a 2xx that does not decode (or does not read as
    /// a neutral response) is reported with whatever usage it carried so the
    /// spend cap can bill it.
    fn decode(reply: &Reply) -> Result<ChatResponse, LlmError> {
        if !reply.status.is_success() {
            let (kind, message) = match serde_json::from_slice::<ApiErrorBody>(&reply.body) {
                Ok(b) => (b.error.kind, b.error.message),
                Err(_) => ("unknown".to_owned(), String::from_utf8_lossy(&reply.body).into_owned()),
            };
            return Err(LlmError::Api { status: reply.status, kind, message });
        }
        let parsed: MessagesResponse = serde_json::from_slice(&reply.body)
            .map_err(|source| LlmError::Decode { source, billed: usage_of(&reply.body) })?;
        tracing::debug!(
            input = parsed.usage.input_tokens,
            output = parsed.usage.output_tokens,
            cache_read = ?parsed.usage.cache_read_input_tokens,
            stop = ?parsed.stop_reason,
            "messages ok"
        );
        from_wire(&parsed).map_err(|source| LlmError::Decode { source, billed: usage_of(&reply.body) })
    }
}

#[async_trait]
impl Backend for Anthropic {
    #[tracing::instrument(skip_all, fields(model = %self.model))]
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let body = serde_json::to_vec(&to_wire(&self.model, req)?)
            .map_err(|e| LlmError::Request(format!("serialize Messages API body: {e}")))?;
        let betas: Vec<&str> = self.betas.iter().map(String::as_str).chain(betas_for(req)).collect();
        let url = self.endpoint.url();
        let build = || {
            let mut builder = self
                .endpoint
                .authorize(self.http.post(&url))
                .header("anthropic-version", API_VERSION)
                .header("content-type", "application/json");
            if !betas.is_empty() {
                builder = builder.header("anthropic-beta", betas.join(","));
            }
            builder.body(body.clone())
        };
        post_with_retries(build, Self::decode).await
    }

    fn capabilities(&self) -> Capabilities {
        self.endpoint.capabilities()
    }

    fn provider(&self) -> &'static str {
        BACKEND
    }

    fn model(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_llm::{ChatModel as _, Metered, RefusalFallback, SpendMeter, TextBlock, ToolChoice, Turn};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
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

    fn req() -> ChatRequest {
        ChatRequest {
            max_tokens: 64,
            system: vec![],
            turns: vec![Turn::User(vec![TextBlock::plain("hi")])],
            tools: vec![],
            tool_choice: ToolChoice::None,
            output: None,
            effort: None,
            thinking: false,
            fallbacks: None,
        }
    }

    fn against(server: &MockServer) -> Result<Anthropic, LlmError> {
        Anthropic::new(Endpoint::Direct { base_url: server.uri(), api_key: "k".into() })
    }

    #[tokio::test]
    async fn usage_accumulates_and_cap_blocks_without_sending() -> Result<(), LlmError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "k"))
            .and(header("anthropic-version", API_VERSION))
            // 1M input + 200k output = $5 + $5 = $10 per call.
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(1_000_000, 200_000, 0, 0)))
            .mount(&server)
            .await;
        let client = Metered::new(against(&server)?, SpendMeter::new().with_max_spend_usd(15.0)?)?;
        let clone = client.clone();
        client.complete(&req()).await?;
        assert!((client.meter().spent_usd() - 10.0).abs() < 1e-6, "{}", client.meter().spent_usd());
        clone.complete(&req()).await?;
        assert!((client.meter().spent_usd() - 20.0).abs() < 1e-6, "{}", client.meter().spent_usd());
        assert_eq!(client.meter().calls(), 2);
        assert_eq!(clone.meter().calls(), 2);
        let third = client.complete(&req()).await;
        assert!(
            matches!(third, Err(LlmError::SpendCapExceeded { spent, cap }) if (spent - 20.0).abs() < 1e-6 && (cap - 15.0).abs() < 1e-6),
            "{third:?}"
        );
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), 2);
        assert_eq!(client.meter().calls(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn failed_calls_do_not_count() -> Result<(), LlmError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "type": "error", "error": {"type": "invalid_request_error", "message": "nope"}
            })))
            .mount(&server)
            .await;
        let client = Metered::new(against(&server)?, SpendMeter::new())?;
        let r = client.complete(&req()).await;
        assert!(matches!(&r, Err(LlmError::Api { kind, message, .. }) if kind == "invalid_request_error" && message == "nope"), "{r:?}");
        assert_eq!(client.meter().calls(), 0);
        assert!(client.meter().spent_usd().abs() < f64::EPSILON);
        Ok(())
    }

    #[tokio::test]
    async fn undecodable_2xx_body_is_still_billed() -> Result<(), LlmError> {
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
        let client = Metered::new(against(&server)?, SpendMeter::new())?;
        assert!(matches!(client.complete(&req()).await, Err(LlmError::Decode { billed: Some(_), .. })));
        assert!((client.meter().spent_usd() - 5.0).abs() < 1e-6, "{}", client.meter().spent_usd());
        assert_eq!(client.meter().calls(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn reservation_refuses_a_request_that_cannot_fit() -> Result<(), LlmError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(1, 1, 0, 0)))
            .mount(&server)
            .await;
        // 16k output tokens at $25/MTok reserve $0.40; a $0.10 cap cannot fit it.
        let client = Metered::new(against(&server)?, SpendMeter::new().with_max_spend_usd(0.10)?)?;
        let big = ChatRequest { max_tokens: 16_000, ..req() };
        assert!(matches!(client.complete(&big).await, Err(LlmError::SpendCapExceeded { .. })));
        // The small request fits, and afterwards the reservation is gone.
        client.complete(&req()).await?;
        assert!(client.meter().spent_usd() < 0.001, "{}", client.meter().spent_usd());
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), 1);
        Ok(())
    }

    #[tokio::test]
    async fn the_fallbacks_beta_is_sent_only_when_the_request_asks() -> Result<(), LlmError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(1, 1, 0, 0)))
            .mount(&server)
            .await;
        let client = against(&server)?;
        client.complete(&req()).await?;
        client.complete(&ChatRequest { fallbacks: Some(RefusalFallback::Default), ..req() }).await?;
        client.with_beta("x-beta").complete(&ChatRequest { fallbacks: Some(RefusalFallback::Default), ..req() }).await?;
        let reqs = server.received_requests().await.unwrap_or_default();
        let beta = |i: usize| reqs.get(i).and_then(|r| r.headers.get("anthropic-beta")).and_then(|v| v.to_str().ok()).map(str::to_owned);
        assert_eq!(beta(0), None);
        assert_eq!(beta(1).as_deref(), Some(crate::wire::Fallbacks::BETA));
        assert_eq!(beta(2), Some(format!("x-beta,{}", crate::wire::Fallbacks::BETA)));
        Ok(())
    }

    #[test]
    fn debug_redacts_api_key() -> Result<(), LlmError> {
        let c = Anthropic::new(Endpoint::direct("sk-ant-super-secret"))?.with_beta("x-beta");
        let s = format!("{c:?}");
        assert!(!s.contains("super-secret"), "{s}");
        assert!(s.contains("<redacted>") && s.contains("x-beta"), "{s}");
        let m = format!("{:?}", Metered::new(c, SpendMeter::new())?);
        assert!(!m.contains("super-secret"), "{m}");
        Ok(())
    }

    #[test]
    fn from_env_without_key_is_typed() {
        // Only meaningful when the variable is absent or blank in the test environment.
        if std::env::var("ANTHROPIC_API_KEY").is_ok_and(|k| !k.trim().is_empty()) {
            return;
        }
        assert!(matches!(Anthropic::from_env(), Err(LlmError::MissingApiKey { var: "ANTHROPIC_API_KEY" })));
    }

    #[test]
    fn direct_endpoint_url_and_capabilities() -> Result<(), LlmError> {
        let e = Endpoint::Direct { base_url: "http://x/".into(), api_key: "k".into() };
        assert_eq!(e.url(), "http://x/v1/messages");
        assert_eq!(e.capabilities().structured_output, StructuredOutput::Enforced);
        assert!(e.capabilities().refusal_fallbacks);
        let a = Anthropic::new(e)?.with_model("claude-something");
        assert_eq!(a.model(), "claude-something");
        assert_eq!(a.provider(), "anthropic");
        Ok(())
    }
}
