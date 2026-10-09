//! [`EmbedBackend`] over Voyage AI's HTTP API (`POST /v1/embeddings`): the
//! vectors and `usage.total_tokens`, which [`crate::MeteredEmbedder`] settles
//! the spend cap's reservation to.

use std::{fmt, time::Duration};

use async_trait::async_trait;
use judge_core::{InputKind, JudgeError};
use serde::{Deserialize, Serialize};

use crate::{EmbedBackend, EmbedError, EmbedUsage, Embedded, Provider, Space};

/// Voyage's key-only free tier allows 3 requests/min; 429s are retried with a
/// fixed pause instead of failing a long ingest run.
const RATE_LIMIT_TRIES: u32 = 8;
const RATE_LIMIT_PAUSE: std::time::Duration = std::time::Duration::from_secs(25);
const URL: &str = "https://api.voyageai.com/v1/embeddings";

/// Voyage AI embedder, built by the configuration loader
/// ([`VoyageEmbedder::new`]); it reaches the pipeline only inside a
/// [`crate::MeteredEmbedder`].
#[derive(Clone)]
pub struct VoyageEmbedder {
    http: reqwest::Client,
    /// [`URL`], except in tests.
    url: String,
    api_key: String,
    /// Model and width, sent on every request so the vectors are that wide
    /// whatever the model's default.
    space: Space,
}

// Manual impl: the API key must never reach a `{:?}` log line.
impl fmt::Debug for VoyageEmbedder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoyageEmbedder")
            .field("space", &self.space)
            .field("api_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct Req<'a> {
    input: &'a [&'a str],
    model: &'a str,
    input_type: &'static str,
    /// Sent explicitly so `dimensions()` and the model agree regardless of the model's default.
    output_dimension: usize,
}

#[derive(Deserialize)]
struct Resp {
    data: Vec<Datum>,
}

/// The billed part of a 2xx body, read on its own before [`Resp`] so a body
/// whose `data` does not decode is still billed.
#[derive(Deserialize)]
struct Billed {
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    total_tokens: u64,
}

/// What a 2xx `body` says it used.
fn usage(body: &[u8]) -> EmbedUsage {
    serde_json::from_slice::<Billed>(body)
        .ok()
        .and_then(|b| b.usage)
        .map_or(EmbedUsage::Unreported, |u| {
            EmbedUsage::Tokens(u.total_tokens)
        })
}

#[derive(Deserialize)]
struct Datum {
    embedding: Vec<f32>,
}

impl VoyageEmbedder {
    /// With an explicit key, model and width. `dimensions` is sent on every
    /// request, so the vectors are that wide whatever the model's default.
    ///
    /// # Errors
    /// If the underlying HTTP client cannot be built (no system CA certificates).
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        dimensions: usize,
    ) -> Result<Self, JudgeError> {
        // The same bound as the OpenAI-compatible embedder: a stalled request
        // must not hold a refresh (and its lease) forever.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_mins(5))
            .build()
            .map_err(anyhow::Error::from)?;
        Ok(Self {
            http,
            url: URL.to_owned(),
            api_key: api_key.into(),
            space: Space {
                provider: Provider::Voyage,
                model: model.into(),
                dimensions,
            },
        })
    }
}

#[async_trait]
impl EmbedBackend for VoyageEmbedder {
    async fn embed(&self, texts: &[&str], kind: InputKind) -> Result<Embedded, EmbedError> {
        if texts.is_empty() {
            return Err(
                anyhow::anyhow!("embed: no texts given (Voyage rejects an empty input)").into(),
            );
        }
        let input_type = match kind {
            InputKind::Document => "document",
            InputKind::Query => "query",
        };
        let req = Req {
            input: texts,
            model: &self.space.model,
            input_type,
            output_dimension: self.space.dimensions,
        };
        let mut tries = 0;
        let body = loop {
            let resp = self
                .http
                .post(&self.url)
                .bearer_auth(&self.api_key)
                .json(&req)
                .send()
                .await
                .map_err(anyhow::Error::from)?;
            let status = resp.status();
            let body = resp.bytes().await.map_err(anyhow::Error::from)?;
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && tries < RATE_LIMIT_TRIES {
                tries += 1;
                tracing::warn!(
                    tries,
                    "voyage rate limited (429); pausing {}s",
                    RATE_LIMIT_PAUSE.as_secs()
                );
                tokio::time::sleep(RATE_LIMIT_PAUSE).await;
                continue;
            }
            if !status.is_success() {
                // Keep Voyage's JSON error body: the status alone says nothing useful.
                return Err(
                    anyhow::anyhow!("voyage {status}: {}", String::from_utf8_lossy(&body)).into(),
                );
            }
            break body;
        };
        // A 2xx was billed whatever its body holds.
        let usage = usage(&body);
        let billed = |error: anyhow::Error| EmbedError {
            error: JudgeError::from(error),
            billed: Some(usage),
        };
        let parsed: Resp = serde_json::from_slice(&body).map_err(|e| billed(e.into()))?;
        if parsed.data.len() != texts.len() {
            return Err(billed(anyhow::anyhow!(
                "voyage returned {} embeddings for {} texts",
                parsed.data.len(),
                texts.len()
            )));
        }
        Ok(Embedded {
            vectors: parsed.data.into_iter().map(|d| d.embedding).collect(),
            usage,
        })
    }

    fn space(&self) -> &Space {
        &self.space
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_MODEL: &str = "voyage-3.5";
    use crate::{EmbedPrice, MeteredEmbedder, WithSpace, is_spend_cap};
    use judge_core::Embedder;
    use judge_llm::SpendMeter;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, method, path},
    };

    fn at(server: &MockServer) -> Result<VoyageEmbedder, JudgeError> {
        let mut e = VoyageEmbedder::new("pa-test", DEFAULT_MODEL, 2)?;
        e.url = format!("{}/v1/embeddings", server.uri());
        Ok(e)
    }

    #[tokio::test]
    async fn the_reported_usage_is_what_the_meter_settles_at()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(header("authorization", "Bearer pa-test"))
            .and(body_json(json!({"input": ["Sample text"], "model": "voyage-3.5", "input_type": "query", "output_dimension": 2})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [{"object": "embedding", "embedding": [0.5, 0.5], "index": 0}],
                "model": "voyage-3.5",
                "usage": {"total_tokens": 1_000_000}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let backend = at(&server)?;
        let meter = SpendMeter::new();
        let e = MeteredEmbedder::new(backend, meter.clone())?;
        assert_eq!(e.price(), EmbedPrice::Table(0.06));
        assert_eq!(
            e.embed(&["Sample text"], InputKind::Query).await?,
            vec![vec![0.5, 0.5]]
        );
        // 1M tokens of voyage-3.5 is $0.06: the usage, not the byte-count reservation.
        assert!(
            (meter.spent_usd() - 0.06).abs() < 1e-9,
            "{}",
            meter.spent_usd()
        );
        assert_eq!(meter.embedding_calls(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn a_capped_meter_refuses_before_any_request() -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let meter = SpendMeter::new().with_max_spend_usd(0.0)?;
        let e = MeteredEmbedder::new(at(&server)?, meter.clone())?;
        let err = e.embed(&["q"], InputKind::Query).await.err();
        assert!(err.as_ref().is_some_and(is_spend_cap), "{err:?}");
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), 0);
        assert_eq!(meter.embedding_calls(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn a_billed_body_that_does_not_decode_still_counts()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [], "usage": {"total_tokens": 2_000_000}
            })))
            .mount(&server)
            .await;
        let meter = SpendMeter::new();
        let e = MeteredEmbedder::new(at(&server)?, meter.clone())?;
        let err = e
            .embed(&["a"], InputKind::Document)
            .await
            .err()
            .map(|e| e.to_string());
        assert!(
            err.as_deref()
                .is_some_and(|e| e.contains("0 embeddings for 1 texts")),
            "{err:?}"
        );
        assert!(
            (meter.spent_usd() - 0.12).abs() < 1e-9,
            "{}",
            meter.spent_usd()
        );
        Ok(())
    }

    #[test]
    fn debug_redacts_api_key() -> Result<(), JudgeError> {
        let e = VoyageEmbedder::new("pa-secret", DEFAULT_MODEL, 1024)?;
        let s = format!("{e:?}");
        assert!(!s.contains("pa-secret") && s.contains("<redacted>"), "{s}");
        assert_eq!(
            e.space(),
            &Space {
                provider: Provider::Voyage,
                model: DEFAULT_MODEL.into(),
                dimensions: 1024
            }
        );
        Ok(())
    }

    #[test]
    fn request_shape() -> Result<(), serde_json::Error> {
        let v = serde_json::to_value(Req {
            input: &["a", "b"],
            model: "voyage-3.5",
            input_type: "query",
            output_dimension: 1024,
        })?;
        assert_eq!(
            v,
            serde_json::json!({"input": ["a", "b"], "model": "voyage-3.5", "input_type": "query", "output_dimension": 1024})
        );
        Ok(())
    }
}
