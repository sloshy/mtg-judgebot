//! `Embedder` over Voyage AI's HTTP API (`POST /v1/embeddings`).

use std::fmt;

use async_trait::async_trait;
use judge_core::{Embedder, InputKind, JudgeError};
use serde::{Deserialize, Serialize};

const URL: &str = "https://api.voyageai.com/v1/embeddings";
const DEFAULT_MODEL: &str = "voyage-3.5";
const DEFAULT_DIMENSIONS: usize = 1024;

#[derive(Clone)]
pub struct VoyageEmbedder {
    http: reqwest::Client,
    api_key: String,
    model: String,
    dimensions: usize,
}

// Manual impl: the API key must never reach a `{:?}` log line.
impl fmt::Debug for VoyageEmbedder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoyageEmbedder")
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
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

#[derive(Deserialize)]
struct Datum {
    embedding: Vec<f32>,
}

impl VoyageEmbedder {
    /// Reads `VOYAGE_API_KEY`, optional `VOYAGE_MODEL` (default `voyage-3.5`) and
    /// optional `VOYAGE_DIMENSIONS` (default 1024).
    ///
    /// # Errors
    /// If the key is unset or `VOYAGE_DIMENSIONS` is not a positive integer.
    pub fn from_env() -> anyhow::Result<Self> {
        let api_key = std::env::var("VOYAGE_API_KEY").map_err(|_| anyhow::anyhow!("VOYAGE_API_KEY is not set"))?;
        let model = std::env::var("VOYAGE_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());
        let dimensions = match std::env::var("VOYAGE_DIMENSIONS") {
            Ok(s) => s.parse::<usize>().ok().filter(|d| *d > 0).ok_or_else(|| anyhow::anyhow!("VOYAGE_DIMENSIONS must be a positive integer, got {s:?}"))?,
            Err(_) => DEFAULT_DIMENSIONS,
        };
        Ok(Self { http: reqwest::Client::new(), api_key, model, dimensions })
    }
}

#[async_trait]
impl Embedder for VoyageEmbedder {
    async fn embed(&self, texts: &[&str], kind: InputKind) -> Result<Vec<Vec<f32>>, JudgeError> {
        if texts.is_empty() {
            return Err(anyhow::anyhow!("embed: no texts given (Voyage rejects an empty input)").into());
        }
        let input_type = match kind {
            InputKind::Document => "document",
            InputKind::Query => "query",
        };
        let resp = self
            .http
            .post(URL)
            .bearer_auth(&self.api_key)
            .json(&Req { input: texts, model: &self.model, input_type, output_dimension: self.dimensions })
            .send()
            .await
            .map_err(anyhow::Error::from)?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(anyhow::Error::from)?;
        if !status.is_success() {
            // Keep Voyage's JSON error body: the status alone says nothing useful.
            return Err(anyhow::anyhow!("voyage {status}: {}", String::from_utf8_lossy(&body)).into());
        }
        let parsed: Resp = serde_json::from_slice(&body).map_err(anyhow::Error::from)?;
        if parsed.data.len() != texts.len() {
            return Err(anyhow::anyhow!("voyage returned {} embeddings for {} texts", parsed.data.len(), texts.len()).into());
        }
        Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_api_key() {
        let e = VoyageEmbedder { http: reqwest::Client::new(), api_key: "pa-secret".into(), model: DEFAULT_MODEL.into(), dimensions: 1024 };
        let s = format!("{e:?}");
        assert!(!s.contains("pa-secret") && s.contains("<redacted>"), "{s}");
    }

    #[test]
    fn request_shape() -> Result<(), serde_json::Error> {
        let v = serde_json::to_value(Req { input: &["a", "b"], model: "voyage-3.5", input_type: "query", output_dimension: 1024 })?;
        assert_eq!(v, serde_json::json!({"input": ["a", "b"], "model": "voyage-3.5", "input_type": "query", "output_dimension": 1024}));
        Ok(())
    }
}
