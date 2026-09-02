//! [`OpenAi`], the chat completions [`Backend`], and [`Auth`], how the key
//! travels. No spend cap here — that is [`judge_llm::Metered`], the only way
//! a backend becomes the pipeline's `ChatModel` — and no retry loop of its
//! own: the request goes through [`judge_llm::http::post_with_retries`].

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use judge_llm::{
    ApiKey, Backend, Capabilities, ChatRequest, ChatResponse, LlmError,
    http::{Reply, post_with_retries},
};

use crate::{
    Dialect,
    convert::{BACKEND, from_wire, to_wire, usage_of},
    wire::{self, ApiErrorBody},
};

/// How the key is sent. A local server (Ollama, llama.cpp) needs none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    /// No credential.
    None,
    /// `Authorization: Bearer <key>` (`OpenAI`, `LiteLLM`, `OpenRouter`, vLLM).
    Bearer(ApiKey),
    /// `api-key: <key>` (Azure `OpenAI`).
    ApiKeyHeader(ApiKey),
}

/// Chat completions as a [`Backend`]. Cheap to clone (the connection pool is shared).
#[derive(Clone, Debug)]
pub struct OpenAi {
    http: reqwest::Client,
    /// `POST` target, from the base URL (`{base_url}/chat/completions`,
    /// with a query string in the base kept at the end for Azure's
    /// `?api-version=`).
    url: String,
    auth: Auth,
    model: String,
    dialect: Dialect,
    /// Whether "effort asked for but the dialect cannot send it" has been
    /// logged; shared by clones so it is said once per process.
    warned_effort: Arc<AtomicBool>,
}

impl OpenAi {
    /// Against `base_url` (the origin plus the API prefix, typically ending
    /// in `/v1`), with `auth`, for `model`, under `dialect`.
    ///
    /// # Errors
    /// If the underlying HTTP client cannot be built.
    pub fn new(base_url: &str, auth: Auth, model: impl Into<String>, dialect: Dialect) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder().timeout(Duration::from_mins(10)).build()?;
        Ok(Self {
            http,
            url: chat_url(base_url),
            auth,
            model: model.into(),
            dialect,
            warned_effort: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The dialect in use.
    #[must_use]
    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// The URL requests go to.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Add the auth to a request.
    fn authorize(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            Auth::None => builder,
            Auth::Bearer(key) => builder.bearer_auth(key.expose()),
            Auth::ApiKeyHeader(key) => builder.header("api-key", key.expose()),
        }
    }

    /// Decode one reply: the API's error body on a non-2xx status, the
    /// response otherwise; a 2xx that does not decode (or does not read as
    /// a neutral response) is reported with whatever usage it carried so the
    /// spend cap can bill it.
    fn decode(&self, reply: &Reply) -> Result<ChatResponse, LlmError> {
        if !reply.status.is_success() {
            let (kind, message) = match serde_json::from_slice::<ApiErrorBody>(&reply.body) {
                Ok(b) => (b.error.kind.unwrap_or_else(|| "unknown".to_owned()), b.error.message),
                Err(_) => ("unknown".to_owned(), String::from_utf8_lossy(&reply.body).into_owned()),
            };
            return Err(LlmError::Api { status: reply.status, kind, message });
        }
        let parsed: wire::ChatResponse =
            serde_json::from_slice(&reply.body).map_err(|source| LlmError::Decode { source, billed: usage_of(&reply.body) })?;
        tracing::debug!(
            usage = ?parsed.usage,
            finish_reason = ?parsed.choices.first().and_then(|c| c.finish_reason.as_deref()),
            "chat completion ok"
        );
        from_wire(&parsed, &self.model).map_err(|source| LlmError::Decode { source, billed: usage_of(&reply.body) })
    }
}

/// `{base_url}/chat/completions`, keeping a query string the base carries
/// (Azure: `.../deployments/<d>?api-version=...`) after the path.
fn chat_url(base_url: &str) -> String {
    let (path, query) = base_url.split_once('?').map_or((base_url, None), |(p, q)| (p, Some(q)));
    let path = format!("{}/chat/completions", path.trim_end_matches('/'));
    match query {
        Some(q) => format!("{path}?{q}"),
        None => path,
    }
}

#[async_trait]
impl Backend for OpenAi {
    #[tracing::instrument(skip_all, fields(model = %self.model))]
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        if req.effort.is_some() && !self.dialect.reasoning_effort && !self.warned_effort.swap(true, Ordering::Relaxed) {
            tracing::warn!(model = %self.model, "effort requested but the provider is not configured for reasoning_effort; not sent");
        }
        let body = serde_json::to_vec(&to_wire(&self.model, self.dialect, req)?)
            .map_err(|e| LlmError::Request(format!("serialize chat completions body: {e}")))?;
        let build = || self.authorize(self.http.post(&self.url)).header("content-type", "application/json").body(body.clone());
        post_with_retries(build, |reply| self.decode(reply)).await
    }

    fn capabilities(&self) -> Capabilities {
        self.dialect.capabilities()
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
    use judge_llm::{ChatModel as _, Effort, Metered, Price, SpendMeter, TextBlock, ToolChoice, Turn};
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path, query_param},
    };

    fn ok_body(prompt: u64, completion: u64, cached: u64) -> serde_json::Value {
        json!({
            "id": "chatcmpl-1", "model": "qwen3:8b",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": prompt, "completion_tokens": completion, "prompt_tokens_details": {"cached_tokens": cached}}
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

    #[test]
    fn urls_keep_a_query_string_after_the_path() {
        assert_eq!(chat_url("http://ollama:11434/v1"), "http://ollama:11434/v1/chat/completions");
        assert_eq!(chat_url("http://litellm:4000/v1/"), "http://litellm:4000/v1/chat/completions");
        assert_eq!(
            chat_url("https://x.openai.azure.com/openai/deployments/d?api-version=2024-10-21"),
            "https://x.openai.azure.com/openai/deployments/d/chat/completions?api-version=2024-10-21"
        );
    }

    #[tokio::test]
    async fn bearer_auth_url_and_usage_reach_the_meter() -> Result<(), LlmError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-test"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(1_000, 10, 400)))
            .expect(1)
            .mount(&server)
            .await;
        let client = OpenAi::new(&format!("{}/v1", server.uri()), Auth::Bearer("sk-test".into()), "qwen3:8b", Dialect::default())?;
        let rate = judge_llm::Pricing { input: 1.0, output: 2.0, cache_read: 0.1, cache_write: 0.0 };
        let metered = Metered::priced(client, SpendMeter::new(), Price::PerToken(rate));
        let resp = metered.complete(&req()).await?;
        assert_eq!(resp.text, ["hi"]);
        assert_eq!(resp.usage, judge_llm::Usage { input: 600, output: 10, cache_read: 400, cache_write: 0 });
        // 600 in at $1 + 10 out at $2 + 400 cached at $0.10, per million.
        let expected = (600.0 + 20.0 + 40.0) / 1_000_000.0;
        assert!((metered.meter().spent_usd() - expected).abs() < 1e-9, "{}", metered.meter().spent_usd());
        assert_eq!(metered.meter().calls(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn azure_header_and_query_string() -> Result<(), LlmError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/d/chat/completions"))
            .and(query_param("api-version", "2024-10-21"))
            .and(header("api-key", "az"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(1, 1, 0)))
            .expect(1)
            .mount(&server)
            .await;
        let base = format!("{}/openai/deployments/d?api-version=2024-10-21", server.uri());
        let client = OpenAi::new(&base, Auth::ApiKeyHeader("az".into()), "d", Dialect::default())?;
        client.complete(&req()).await?;
        let reqs = server.received_requests().await.unwrap_or_default();
        assert!(reqs.first().is_some_and(|r| r.headers.get("authorization").is_none()));
        Ok(())
    }

    #[tokio::test]
    async fn api_errors_carry_the_message_and_undecodable_bodies_are_billed() -> Result<(), LlmError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": {"message": "Unsupported parameter: max_tokens", "type": "invalid_request_error", "code": "unsupported_parameter"}
            })))
            .mount(&server)
            .await;
        let client = OpenAi::new(&format!("{}/v1", server.uri()), Auth::None, "m", Dialect::default())?;
        let r = client.complete(&req()).await;
        assert!(
            matches!(&r, Err(LlmError::Api { status, kind, message }) if status.as_u16() == 400 && kind == "invalid_request_error" && message == "Unsupported parameter: max_tokens"),
            "{r:?}"
        );
        // A local server with no key and a bare-string error.
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/v1/chat/completions")).respond_with(ResponseTemplate::new(404).set_body_string("model not found")).mount(&server).await;
        let client = OpenAi::new(&format!("{}/v1", server.uri()), Auth::None, "m", Dialect::default())?;
        let r = client.complete(&req()).await;
        assert!(matches!(&r, Err(LlmError::Api { kind, message, .. }) if kind == "unknown" && message == "model not found"), "{r:?}");
        let reqs = server.received_requests().await.unwrap_or_default();
        assert!(reqs.first().is_some_and(|r| r.headers.get("authorization").is_none() && r.headers.get("api-key").is_none()));

        // `choices` missing: not a response, but `usage` parses, so the meter bills it.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "x", "usage": {"prompt_tokens": 1_000_000, "completion_tokens": 0}})))
            .mount(&server)
            .await;
        let client = OpenAi::new(&format!("{}/v1", server.uri()), Auth::None, "m", Dialect::default())?;
        let rate = judge_llm::Pricing { input: 5.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 };
        let metered = Metered::priced(client, SpendMeter::new(), Price::PerToken(rate));
        assert!(matches!(metered.complete(&req()).await, Err(LlmError::Decode { billed: Some(_), .. })));
        assert!((metered.meter().spent_usd() - 5.0).abs() < 1e-6, "{}", metered.meter().spent_usd());
        Ok(())
    }

    #[tokio::test]
    async fn effort_is_dropped_with_one_warning_when_the_dialect_cannot_send_it() -> Result<(), LlmError> {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/v1/chat/completions")).respond_with(ResponseTemplate::new(200).set_body_json(ok_body(1, 1, 0))).mount(&server).await;
        let client = OpenAi::new(&format!("{}/v1", server.uri()), Auth::None, "m", Dialect::default())?;
        client.complete(&ChatRequest { effort: Some(Effort::High), ..req() }).await?;
        client.clone().complete(&ChatRequest { effort: Some(Effort::High), ..req() }).await?;
        assert!(client.warned_effort.load(Ordering::Relaxed));
        let reqs = server.received_requests().await.unwrap_or_default();
        for r in &reqs {
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap_or_default();
            assert!(body.get("reasoning_effort").is_none(), "{body}");
        }
        Ok(())
    }

    #[test]
    fn debug_redacts_the_key() -> Result<(), LlmError> {
        let c = OpenAi::new("http://x/v1", Auth::Bearer("sk-super-secret".into()), "m", Dialect::default())?;
        let s = format!("{c:?}");
        assert!(!s.contains("super-secret") && s.contains("<redacted>"), "{s}");
        assert_eq!(c.model(), "m");
        assert_eq!(c.provider(), "openai");
        assert_eq!(c.url(), "http://x/v1/chat/completions");
        Ok(())
    }
}
