//! Minimal `POST /v1/messages` client with bounded retries.
//!
//! Non-streaming only: every request in this project keeps `max_tokens`
//! at or below 16k, which stays within the non-streaming HTTP timeout
//! guidance, so `eventsource-stream` is not needed yet.

use std::{fmt, time::Duration};

use reqwest::StatusCode;

use crate::{API_VERSION, wire::{ApiErrorBody, MessagesRequest, MessagesResponse}};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// Attempts per `messages` call (1 initial + 2 retries), matching the official SDKs' `max_retries = 2`.
const MAX_ATTEMPTS: u32 = 3;
/// Backoff for a retryable failure that carries no `retry-after`.
const BASE_BACKOFF: Duration = Duration::from_millis(500);
/// Upper bound honoured for `retry-after`.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

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
            ClientError::Decode(_) | ClientError::MissingApiKey => false,
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
}

// Manual impl: the API key must never reach a `{:?}` log line.
impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.base_url)
            .field("betas", &self.betas)
            .field("api_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Client {
    /// # Errors
    /// If the underlying HTTP client cannot be built.
    pub fn new(api_key: impl Into<String>) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder().timeout(Duration::from_mins(10)).build()?;
        Ok(Self { http, api_key: api_key.into(), base_url: DEFAULT_BASE_URL.to_owned(), betas: Vec::new() })
    }

    /// Read `ANTHROPIC_API_KEY` (and optional `ANTHROPIC_BASE_URL`).
    ///
    /// # Errors
    /// `MissingApiKey`, or if the client cannot be built.
    pub fn from_env() -> Result<Self, ClientError> {
        let key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| ClientError::MissingApiKey)?;
        let mut c = Self::new(key)?;
        if let Ok(url) = std::env::var("ANTHROPIC_BASE_URL") {
            c.base_url = url;
        }
        Ok(c)
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
        let mut attempt = 1;
        loop {
            match self.messages_once(req).await {
                Ok(resp) => return Ok(resp),
                Err((err, retry_after)) if attempt < MAX_ATTEMPTS && err.is_retryable() => {
                    let delay = retry_after.unwrap_or_else(|| BASE_BACKOFF * 2u32.pow(attempt - 1)).min(MAX_BACKOFF);
                    tracing::warn!(attempt, ?delay, error = %err, "retrying messages request");
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err((err, _)) => return Err(err),
            }
        }
    }

    /// Single attempt; the error carries the parsed `retry-after`, if any.
    async fn messages_once(&self, req: &MessagesRequest) -> Result<MessagesResponse, (ClientError, Option<Duration>)> {
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
        let resp = builder.json(req).send().await.map_err(|e| (ClientError::Transport(e), None))?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = resp.bytes().await.map_err(|e| (ClientError::Transport(e), retry_after))?;
        if !status.is_success() {
            let (kind, message) = match serde_json::from_slice::<ApiErrorBody>(&body) {
                Ok(b) => (b.error.kind, b.error.message),
                Err(_) => ("unknown".to_owned(), String::from_utf8_lossy(&body).into_owned()),
            };
            return Err((ClientError::Api { status, kind, message }, retry_after));
        }
        let parsed: MessagesResponse = serde_json::from_slice(&body).map_err(|e| (ClientError::Decode(e), None))?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn from_env_without_key_is_typed() {
        // Only meaningful when the variable is absent in the test environment.
        if std::env::var_os("ANTHROPIC_API_KEY").is_none() {
            assert!(matches!(Client::from_env(), Err(ClientError::MissingApiKey)));
        }
    }
}
