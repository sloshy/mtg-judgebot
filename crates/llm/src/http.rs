//! The HTTP retry loop every backend shares: one request, retried up to
//! twice on 408/409/429/529/5xx and connection-level transport failures,
//! honouring `retry-after` when the server sends one.
//!
//! Non-streaming only: every request in this project keeps `max_tokens` at
//! or below 16k, which stays within the non-streaming HTTP timeout guidance.

use std::time::Duration;

use reqwest::StatusCode;

use crate::{ChatResponse, LlmError};

/// Attempts per call (1 initial + 2 retries), matching the official SDKs' `max_retries = 2`.
pub const MAX_ATTEMPTS: u32 = 3;
/// Backoff for a retryable failure that carries no `retry-after`.
const BASE_BACKOFF: Duration = Duration::from_millis(500);
/// Upper bound honoured for `retry-after`.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// What one HTTP round trip returned, for the backend to decode.
#[derive(Debug)]
pub struct Reply {
    /// HTTP status.
    pub status: StatusCode,
    /// The whole body.
    pub body: Vec<u8>,
}

/// Send `build()` and decode the reply with `decode`, retrying a retryable
/// [`LlmError`] (see [`LlmError::is_retryable`]) up to [`MAX_ATTEMPTS`]
/// times. `build` is called once per attempt; `decode` maps the status and
/// body to a response or to the error the backend wants reported (a decode
/// failure is not retried, a non-2xx status may be).
///
/// # Errors
/// The last attempt's error.
pub async fn post_with_retries<B, D>(build: B, decode: D) -> Result<ChatResponse, LlmError>
where
    B: Fn() -> reqwest::RequestBuilder,
    D: Fn(&Reply) -> Result<ChatResponse, LlmError>,
{
    let mut attempt = 1;
    loop {
        match once(&build, &decode).await {
            Ok(resp) => return Ok(resp),
            Err(Failure { err, retry_after }) if attempt < MAX_ATTEMPTS && err.is_retryable() => {
                let delay = retry_after.unwrap_or_else(|| BASE_BACKOFF * 2u32.pow(attempt - 1)).min(MAX_BACKOFF);
                tracing::warn!(attempt, ?delay, error = %err, "retrying chat request");
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(Failure { err, .. }) => return Err(err),
        }
    }
}

/// One failed attempt.
struct Failure {
    err: LlmError,
    /// Parsed `retry-after`, if the server sent one.
    retry_after: Option<Duration>,
}

async fn once<B, D>(build: &B, decode: &D) -> Result<ChatResponse, Failure>
where
    B: Fn() -> reqwest::RequestBuilder,
    D: Fn(&Reply) -> Result<ChatResponse, LlmError>,
{
    let resp = build().send().await.map_err(|e| Failure { err: LlmError::Transport(e), retry_after: None })?;
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let body = resp.bytes().await.map_err(|e| Failure { err: LlmError::Transport(e), retry_after })?;
    decode(&Reply { status, body: body.to_vec() }).map_err(|err| Failure { err, retry_after })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AssistantTurn, Stop, Usage};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn decode(reply: &Reply) -> Result<ChatResponse, LlmError> {
        if !reply.status.is_success() {
            return Err(LlmError::Api { status: reply.status, kind: "k".into(), message: String::from_utf8_lossy(&reply.body).into_owned() });
        }
        Ok(ChatResponse {
            text: vec![String::from_utf8_lossy(&reply.body).into_owned()],
            tool_calls: vec![],
            stop: Stop::EndTurn,
            usage: Usage::default(),
            model: "m".into(),
            assistant: AssistantTurn { backend: "test", raw: serde_json::Value::Null },
        })
    }

    #[tokio::test]
    async fn a_429_with_retry_after_is_retried_and_a_400_is_not() -> Result<(), LlmError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0").set_body_string("slow down"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/x")).respond_with(ResponseTemplate::new(200).set_body_string("ok")).mount(&server).await;
        let http = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let resp = post_with_retries(|| http.post(&url), decode).await?;
        assert_eq!(resp.text, ["ok"]);
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), 2);

        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/x")).respond_with(ResponseTemplate::new(400).set_body_string("nope")).mount(&server).await;
        let url = format!("{}/x", server.uri());
        let err = post_with_retries(|| http.post(&url), decode).await;
        assert!(matches!(err, Err(LlmError::Api { status, .. }) if status == StatusCode::BAD_REQUEST), "{err:?}");
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), 1);
        Ok(())
    }

    #[tokio::test]
    async fn retries_are_bounded() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
            .mount(&server)
            .await;
        let http = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let err = post_with_retries(|| http.post(&url), decode).await;
        assert!(matches!(err, Err(LlmError::Api { status, .. }) if status == StatusCode::SERVICE_UNAVAILABLE), "{err:?}");
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), MAX_ATTEMPTS as usize);
    }
}
