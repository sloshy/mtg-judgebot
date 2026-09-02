//! `Embedder` over an OpenAI-compatible `POST {base_url}/embeddings`
//! (`docs/proposals/providers.md` §4.3): `OpenAI` itself, `LiteLLM`, Ollama,
//! vLLM, llama.cpp, Azure. The request is `input`, `model` and, unless the
//! server is known to reject it, `dimensions`; the response's `data[].embedding`
//! in `index` order. The API has no query/document distinction, so
//! `InputKind` is ignored.
//!
//! The width is checked on every reply: a server that ignores `dimensions`
//! (or one told not to receive it) must still produce vectors of the
//! configured width, because that is the width of the columns.

use std::{fmt, time::Duration};

use async_trait::async_trait;
use judge_core::{Embedder, InputKind, JudgeError};
use serde::{Deserialize, Serialize};

use crate::{Provider, Space, WithSpace};

/// Attempts per call on 429/5xx (1 initial + 2 retries), as the chat backends do.
const MAX_ATTEMPTS: u32 = 3;
/// Backoff for a retryable failure that carries no `retry-after`.
const BASE_BACKOFF: Duration = Duration::from_millis(500);
/// Upper bound honoured for `retry-after`.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How the key is sent. A local server (Ollama, llama.cpp) needs none.
/// Keys are held as plain strings here (this crate does not depend on
/// `judge-llm`); the `Debug` impls redact them.
#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    /// No credential.
    None,
    /// `Authorization: Bearer <key>` (`OpenAI`, `LiteLLM`, `OpenRouter`, vLLM).
    Bearer(String),
    /// `api-key: <key>` (Azure `OpenAI`).
    ApiKeyHeader(String),
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::None => f.write_str("None"),
            Auth::Bearer(_) => f.write_str("Bearer(<redacted>)"),
            Auth::ApiKeyHeader(_) => f.write_str("ApiKeyHeader(<redacted>)"),
        }
    }
}

/// OpenAI-compatible embedder. Cheap to clone (the connection pool is shared).
#[derive(Clone, Debug)]
pub struct OpenAiEmbedder {
    http: reqwest::Client,
    /// `{base_url}/embeddings`, with a query string in the base kept at the
    /// end for Azure's `?api-version=`.
    url: String,
    auth: Auth,
    space: Space,
    /// Whether `dimensions` goes on the wire. Off for a server that rejects
    /// the field (vLLM with a model that has no matryoshka training).
    send_dimensions: bool,
}

#[derive(Serialize)]
struct Req<'a> {
    input: &'a [&'a str],
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

#[derive(Deserialize)]
struct Resp {
    data: Vec<Datum>,
}

#[derive(Deserialize)]
struct Datum {
    embedding: Vec<f32>,
    #[serde(default)]
    index: Option<usize>,
}

/// `{"error": {"message": ...}}`, the shape every compatible server uses.
#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    message: String,
}

impl OpenAiEmbedder {
    /// Against `base_url` (the origin plus the API prefix, typically ending
    /// in `/v1`), with `auth`, for `model` at `dimensions` wide. With
    /// `send_dimensions` the width is asked for on every request; without it
    /// the width is only checked on the reply.
    ///
    /// # Errors
    /// If the underlying HTTP client cannot be built.
    pub fn new(base_url: &str, auth: Auth, model: impl Into<String>, dimensions: usize, send_dimensions: bool) -> Result<Self, JudgeError> {
        let http = reqwest::Client::builder().timeout(Duration::from_mins(5)).build().map_err(anyhow::Error::from)?;
        Ok(Self {
            http,
            url: embeddings_url(base_url),
            auth,
            space: Space { provider: Provider::OpenAi, model: model.into(), dimensions },
            send_dimensions,
        })
    }

    /// The URL requests go to.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    fn authorize(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            Auth::None => builder,
            Auth::Bearer(key) => builder.bearer_auth(key),
            Auth::ApiKeyHeader(key) => builder.header("api-key", key),
        }
    }
}

/// `{base_url}/embeddings`, keeping a query string the base carries after the path.
fn embeddings_url(base_url: &str) -> String {
    let (path, query) = base_url.split_once('?').map_or((base_url, None), |(p, q)| (p, Some(q)));
    let path = format!("{}/embeddings", path.trim_end_matches('/'));
    match query {
        Some(q) => format!("{path}?{q}"),
        None => path,
    }
}

fn retryable(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error()
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    async fn embed(&self, texts: &[&str], _kind: InputKind) -> Result<Vec<Vec<f32>>, JudgeError> {
        if texts.is_empty() {
            return Err(anyhow::anyhow!("embed: no texts given (the embeddings API rejects an empty input)").into());
        }
        let req = Req { input: texts, model: &self.space.model, dimensions: self.send_dimensions.then_some(self.space.dimensions) };
        let mut attempt = 1;
        let body = loop {
            let resp = self.authorize(self.http.post(&self.url)).json(&req).send().await.map_err(anyhow::Error::from)?;
            let status = resp.status();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let body = resp.bytes().await.map_err(anyhow::Error::from)?;
            if status.is_success() {
                break body;
            }
            let message = serde_json::from_slice::<ErrorBody>(&body)
                .map_or_else(|_| String::from_utf8_lossy(&body).into_owned(), |b| b.error.message);
            if retryable(status) && attempt < MAX_ATTEMPTS {
                let delay = retry_after.unwrap_or_else(|| BASE_BACKOFF * 2u32.pow(attempt - 1)).min(MAX_BACKOFF);
                tracing::warn!(attempt, ?delay, %status, message, "retrying embeddings request");
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }
            return Err(anyhow::anyhow!("embeddings {status}: {message}").into());
        };
        let parsed: Resp = serde_json::from_slice(&body).map_err(anyhow::Error::from)?;
        if parsed.data.len() != texts.len() {
            return Err(anyhow::anyhow!("embeddings returned {} vectors for {} texts", parsed.data.len(), texts.len()).into());
        }
        // Servers answer in input order; `index` is honoured when present in case one
        // does not. Indices that are not exactly 0..n (a proxy answering `[1, 1]`) would
        // attach a vector to the wrong text after sorting, so they are an error.
        let mut data = parsed.data;
        if data.iter().all(|d| d.index.is_some()) {
            data.sort_by_key(|d| d.index);
            if let Some((i, d)) = data.iter().enumerate().find(|(i, d)| d.index != Some(*i)) {
                return Err(anyhow::anyhow!("embeddings returned index {:?} where {i} was expected: not a permutation of the input", d.index).into());
            }
        }
        let want = self.space.dimensions;
        data.into_iter()
            .map(|d| {
                if d.embedding.len() == want {
                    Ok(d.embedding)
                } else {
                    Err(anyhow::anyhow!(
                        "embeddings returned a {}-dimensional vector but the configured space is {want}; set models.embed.dimensions to what the model produces",
                        d.embedding.len()
                    )
                    .into())
                }
            })
            .collect()
    }

    fn dimensions(&self) -> usize {
        self.space.dimensions
    }
}

impl WithSpace for OpenAiEmbedder {
    fn space(&self) -> &Space {
        &self.space
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, header_exists, method, path, query_param},
    };

    fn vec_of(n: usize, v: f32) -> Vec<f32> {
        vec![v; n]
    }

    #[test]
    fn debug_redacts_the_key() -> Result<(), JudgeError> {
        let e = OpenAiEmbedder::new("http://x/v1", Auth::Bearer("sk-secret".into()), "m", 4, true)?;
        let s = format!("{e:?}");
        assert!(!s.contains("sk-secret") && s.contains("<redacted>"), "{s}");
        let s = format!("{:?}", Auth::ApiKeyHeader("sk-secret".into()));
        assert!(!s.contains("sk-secret") && s.contains("<redacted>"), "{s}");
        Ok(())
    }

    #[test]
    fn urls_keep_a_query_string_after_the_path() {
        assert_eq!(embeddings_url("http://ollama:11434/v1"), "http://ollama:11434/v1/embeddings");
        assert_eq!(embeddings_url("http://litellm:4000/v1/"), "http://litellm:4000/v1/embeddings");
        assert_eq!(
            embeddings_url("https://x.openai.azure.com/openai/deployments/d?api-version=2024-10-21"),
            "https://x.openai.azure.com/openai/deployments/d/embeddings?api-version=2024-10-21"
        );
    }

    #[tokio::test]
    async fn request_shape_bearer_auth_and_index_order() -> Result<(), JudgeError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(header("authorization", "Bearer sk-test"))
            .and(body_json(json!({"input": ["a", "b"], "model": "text-embedding-3-small", "dimensions": 3})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list", "model": "text-embedding-3-small",
                "data": [
                    {"object": "embedding", "index": 1, "embedding": [2.0, 2.0, 2.0]},
                    {"object": "embedding", "index": 0, "embedding": [1.0, 1.0, 1.0]}
                ],
                "usage": {"prompt_tokens": 2, "total_tokens": 2}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let e = OpenAiEmbedder::new(&format!("{}/v1", server.uri()), Auth::Bearer("sk-test".into()), "text-embedding-3-small", 3, true)?;
        assert_eq!(e.space(), &Space { provider: Provider::OpenAi, model: "text-embedding-3-small".into(), dimensions: 3 });
        assert_eq!(e.dimensions(), 3);
        // `InputKind` is ignored: both kinds send the same body.
        let docs = e.embed(&["a", "b"], InputKind::Document).await?;
        assert_eq!(docs, vec![vec_of(3, 1.0), vec_of(3, 2.0)]);
        Ok(())
    }

    #[tokio::test]
    async fn dimensions_can_be_left_off_the_wire_and_azure_gets_its_header_and_query() -> Result<(), JudgeError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/d/embeddings"))
            .and(query_param("api-version", "2024-10-21"))
            .and(header("api-key", "az-key"))
            .and(body_json(json!({"input": ["q"], "model": "nomic-embed-text"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"embedding": [0.5, 0.5]}]})))
            .expect(1)
            .mount(&server)
            .await;
        let base = format!("{}/openai/deployments/d?api-version=2024-10-21", server.uri());
        let e = OpenAiEmbedder::new(&base, Auth::ApiKeyHeader("az-key".into()), "nomic-embed-text", 2, false)?;
        assert_eq!(e.embed(&["q"], InputKind::Query).await?, vec![vec_of(2, 0.5)]);
        Ok(())
    }

    #[tokio::test]
    async fn no_auth_sends_no_credential() -> Result<(), JudgeError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"embedding": [1.0]}]})))
            .mount(&server)
            .await;
        let e = OpenAiEmbedder::new(&format!("{}/v1", server.uri()), Auth::None, "m", 1, true)?;
        assert_eq!(e.embed(&["q"], InputKind::Query).await?, vec![vec![1.0]]);
        Ok(())
    }

    #[tokio::test]
    async fn a_count_mismatch_and_a_width_mismatch_are_errors() -> Result<(), JudgeError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(body_json(json!({"input": ["a", "b"], "model": "m", "dimensions": 2})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"embedding": [1.0, 1.0]}]})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(body_json(json!({"input": ["a"], "model": "m", "dimensions": 2})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"embedding": [1.0, 1.0, 1.0]}]})))
            .mount(&server)
            .await;
        let e = OpenAiEmbedder::new(&format!("{}/v1", server.uri()), Auth::None, "m", 2, true)?;
        let err = e.embed(&["a", "b"], InputKind::Document).await.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("returned 1 vectors for 2 texts"), "{err}");
        let err = e.embed(&["a"], InputKind::Document).await.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("3-dimensional") && err.contains("configured space is 2"), "{err}");
        let err = e.embed(&[], InputKind::Document).await.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("no texts"), "{err}");
        Ok(())
    }

    #[tokio::test]
    async fn indices_must_be_a_permutation_of_the_input() -> Result<(), JudgeError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
                {"index": 1, "embedding": [1.0, 1.0]},
                {"index": 1, "embedding": [2.0, 2.0]}
            ]})))
            .mount(&server)
            .await;
        let e = OpenAiEmbedder::new(&format!("{}/v1", server.uri()), Auth::None, "m", 2, true)?;
        let err = e.embed(&["a", "b"], InputKind::Document).await.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("index Some(1) where 0 was expected"), "{err}");
        Ok(())
    }

    #[tokio::test]
    async fn error_bodies_are_reported_and_a_429_is_retried() -> Result<(), JudgeError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": {"message": "Incorrect API key provided", "type": "invalid_request_error"}})))
            .expect(1)
            .mount(&server)
            .await;
        let e = OpenAiEmbedder::new(&format!("{}/v1", server.uri()), Auth::Bearer("bad".into()), "m", 2, true)?;
        let err = e.embed(&["a"], InputKind::Document).await.err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, "embeddings 401 Unauthorized: Incorrect API key provided");

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0").set_body_string("slow down"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"embedding": [1.0, 2.0]}]})))
            .mount(&server)
            .await;
        let e = OpenAiEmbedder::new(&format!("{}/v1", server.uri()), Auth::None, "m", 2, true)?;
        assert_eq!(e.embed(&["a"], InputKind::Document).await?, vec![vec![1.0, 2.0]]);
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), 2);

        // Retries are bounded: a server that keeps failing fails the call after MAX_ATTEMPTS.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0").set_body_json(json!({"error": {"message": "overloaded"}})))
            .mount(&server)
            .await;
        let e = OpenAiEmbedder::new(&format!("{}/v1", server.uri()), Auth::None, "m", 2, true)?;
        let err = e.embed(&["a"], InputKind::Document).await.err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, "embeddings 503 Service Unavailable: overloaded");
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), MAX_ATTEMPTS as usize);
        Ok(())
    }
}
