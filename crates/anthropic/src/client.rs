//! The Messages API backend: [`Endpoint`] (which door, with what auth) and
//! [`Anthropic`], the [`Backend`] over it. No spend cap here — that is
//! [`judge_llm::Metered`], the only way a backend becomes the pipeline's
//! `ChatModel` — and no retry loop of its own: the request goes through
//! [`judge_llm::http::post_with_retries`].
//!
//! The body is the Messages API on every door; what differs is the URL,
//! the auth, whether the model is named in the body or the URL, and what
//! the door can honour server-side. That last part is [`Endpoint::capabilities`],
//! and [`Anthropic::mask`] removes from a request what its door would reject
//! (the fallbacks beta, `output_config.format`, `strict` tools) rather than
//! sending it: the adapters have already put the schema in the prompt when
//! the door says so, and validation is client-side regardless.

use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use judge_llm::{
    ApiKey, Backend, Capabilities, ChatRequest, ChatResponse, LlmError, StructuredOutput,
    http::{Reply, post_with_retries},
};

#[cfg(feature = "aws")]
use crate::aws::{AwsCredentials, AwsDoor, DefaultChain, sign_request};
#[cfg(feature = "gcp")]
use crate::gcp::{Adc, TokenSource};
use crate::{
    API_VERSION, DEFAULT_MODEL,
    convert::{BACKEND, betas_for, from_wire, to_wire, usage_of},
    wire::{ApiErrorBody, MessagesResponse, ModelField},
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The header Claude Platform on AWS routes a request to its workspace by.
pub const WORKSPACE_HEADER: &str = "anthropic-workspace-id";

/// Which header a proxy wants the key in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyAuth {
    /// `x-api-key: <key>`, as the first-party API.
    XApiKey,
    /// `Authorization: Bearer <key>` (`LiteLLM`'s virtual keys, most gateways).
    Bearer,
}

/// Which door the Messages API is reached through. The body is the same
/// everywhere; the URL, the auth and what the door supports differ. An enum
/// so that a new door is an exhaustive-match compile error, not a config
/// typo. The cloud doors exist only when their Cargo feature is on (`aws`,
/// `gcp`; both default), so a binary built without one cannot even name it.
///
/// Every door has a `base_url`: the origin the messages path is appended
/// to. The cloud constructors derive it from the region as the platform
/// documents (verified 2026-09-02, cited on each variant); it is a field so
/// a test can point the door at a mock, and an operator can correct it
/// without a release.
#[derive(Clone, Debug)]
pub enum Endpoint {
    /// Anthropic's first-party API: `x-api-key` against `{base_url}/v1/messages`.
    Direct {
        /// API origin, without the `/v1/messages` path.
        base_url: String,
        /// `x-api-key`.
        api_key: ApiKey,
    },
    /// A gateway speaking the Messages API (`LiteLLM`'s `/v1/messages`, a
    /// corporate proxy): same body against `{base_url}/v1/messages`, the
    /// key in whichever header the proxy wants, and no server-side refusal
    /// fallbacks — a proxy will not know the beta, so the request's
    /// `fallbacks` is masked off with a warning rather than sent.
    Proxy {
        /// Proxy origin, without the `/v1/messages` path.
        base_url: String,
        /// The proxy's key.
        api_key: ApiKey,
        /// Which header carries it.
        header: ProxyAuth,
    },
    /// Claude Platform on AWS: Anthropic's own platform reached through an
    /// AWS account. `SigV4` with service `aws-external-anthropic` against
    /// `https://aws-external-anthropic.{region}.api.aws/v1/messages`, the
    /// workspace in the required [`WORKSPACE_HEADER`], bare model ids, and
    /// first-party feature parity (`anthropic-beta` passes through, the
    /// fallbacks beta included). Workspaces are bound to one region; the
    /// signing region and the host must agree.
    #[cfg(feature = "aws")]
    ClaudePlatformOnAws {
        /// `https://aws-external-anthropic.{region}.api.aws` from [`Endpoint::claude_platform_on_aws`].
        base_url: String,
        /// The workspace's AWS region: the `SigV4` credential scope.
        region: String,
        /// `wrkspc_…`, sent as [`WORKSPACE_HEADER`].
        workspace_id: String,
        /// `SigV4` credentials: the default chain in production.
        credentials: Arc<dyn AwsCredentials>,
    },
    /// Claude in Amazon Bedrock through its Messages-shaped endpoint: `SigV4`
    /// with service `bedrock-mantle` against
    /// `https://bedrock-mantle.{region}.api.aws/anthropic/v1/messages`,
    /// `anthropic.`-prefixed model ids (`anthropic.claude-opus-5`, or an
    /// inference profile such as `global.anthropic.…`). Bedrock documents
    /// structured outputs and server-side fallbacks as unsupported, so
    /// `output_config.format`, `strict` tools and `fallbacks` are masked
    /// off: the schema goes into the prompt and is enforced client-side as
    /// always.
    #[cfg(feature = "aws")]
    Bedrock {
        /// `https://bedrock-mantle.{region}.api.aws` from [`Endpoint::bedrock`].
        base_url: String,
        /// The AWS region: host and `SigV4` credential scope.
        region: String,
        /// `SigV4` credentials: the default chain in production.
        credentials: Arc<dyn AwsCredentials>,
    },
    /// Claude on Google Cloud (Vertex AI): a bearer token from Application
    /// Default Credentials against
    /// `{base_url}/v1/projects/{project}/locations/{region}/publishers/anthropic/models/{model}:rawPredict`.
    /// The model is in the URL and `anthropic_version: vertex-2023-10-16`
    /// in the body ([`ModelField::InUrl`]); bare model ids; no server-side
    /// fallbacks (masked off). The origin depends on the region kind —
    /// `global`, the `us`/`eu` multi-regions, or a specific region — see
    /// [`vertex_origin`].
    #[cfg(feature = "gcp")]
    Vertex {
        /// From [`vertex_origin`] via [`Endpoint::vertex`].
        base_url: String,
        /// GCP project id.
        project: String,
        /// `global`, `us`, `eu`, or a region such as `us-east5`.
        region: String,
        /// The bearer token: ADC in production.
        token: Arc<dyn TokenSource>,
    },
}

/// Claude Platform on AWS's gateway origin for `region`.
#[must_use]
pub fn claude_platform_on_aws_origin(region: &str) -> String {
    format!("https://aws-external-anthropic.{region}.api.aws")
}

/// The Bedrock Messages endpoint's origin for `region`.
#[must_use]
pub fn bedrock_origin(region: &str) -> String {
    format!("https://bedrock-mantle.{region}.api.aws")
}

/// Vertex AI's origin for `region`: the global endpoint, a multi-region
/// endpoint (`us`, `eu`) or a regional one.
#[must_use]
pub fn vertex_origin(region: &str) -> String {
    match region {
        "global" => "https://aiplatform.googleapis.com".to_owned(),
        "us" | "eu" => format!("https://aiplatform.{region}.rep.googleapis.com"),
        _ => format!("https://{region}-aiplatform.googleapis.com"),
    }
}

impl Endpoint {
    /// [`Endpoint::Direct`] at the public origin.
    #[must_use]
    pub fn direct(api_key: impl Into<ApiKey>) -> Self {
        Self::Direct {
            base_url: DEFAULT_BASE_URL.to_owned(),
            api_key: api_key.into(),
        }
    }

    /// [`Endpoint::Direct`] from `ANTHROPIC_API_KEY` and optional
    /// `ANTHROPIC_BASE_URL`. A blank value counts as unset for both (a copied
    /// `.env.example` ships `ANTHROPIC_API_KEY=`), as the other binaries
    /// treat their keys, so a missing key fails at startup and not per request.
    ///
    /// # Errors
    /// `MissingApiKey` when the key is unset or blank.
    pub fn from_env() -> Result<Self, LlmError> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// [`Self::from_env`] over any source of variables (tests pass a map).
    ///
    /// # Errors
    /// `MissingApiKey` when the key is unset or blank.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, LlmError> {
        let set = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        let api_key = set("ANTHROPIC_API_KEY").ok_or(LlmError::MissingApiKey {
            var: "ANTHROPIC_API_KEY",
        })?;
        let base_url = set("ANTHROPIC_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        Ok(Self::Direct {
            base_url,
            api_key: api_key.into(),
        })
    }

    /// [`Endpoint::ClaudePlatformOnAws`] on the default credential chain.
    #[cfg(feature = "aws")]
    #[must_use]
    pub fn claude_platform_on_aws(
        region: impl Into<String>,
        workspace_id: impl Into<String>,
    ) -> Self {
        let region = region.into();
        Self::ClaudePlatformOnAws {
            base_url: claude_platform_on_aws_origin(&region),
            credentials: Arc::new(DefaultChain::new(region.clone())),
            region,
            workspace_id: workspace_id.into(),
        }
    }

    /// [`Endpoint::Bedrock`] on the default credential chain.
    #[cfg(feature = "aws")]
    #[must_use]
    pub fn bedrock(region: impl Into<String>) -> Self {
        let region = region.into();
        Self::Bedrock {
            base_url: bedrock_origin(&region),
            credentials: Arc::new(DefaultChain::new(region.clone())),
            region,
        }
    }

    /// [`Endpoint::Vertex`] on Application Default Credentials.
    #[cfg(feature = "gcp")]
    #[must_use]
    pub fn vertex(project: impl Into<String>, region: impl Into<String>) -> Self {
        let region = region.into();
        Self::Vertex {
            base_url: vertex_origin(&region),
            project: project.into(),
            region,
            token: Arc::new(Adc::new()),
        }
    }

    /// The same door at another origin (an operator's override, a test's mock).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        let origin = base_url.into();
        match &mut self {
            Endpoint::Direct { base_url, .. } | Endpoint::Proxy { base_url, .. } => {
                *base_url = origin;
            }
            #[cfg(feature = "aws")]
            Endpoint::ClaudePlatformOnAws { base_url, .. } | Endpoint::Bedrock { base_url, .. } => {
                *base_url = origin;
            }
            #[cfg(feature = "gcp")]
            Endpoint::Vertex { base_url, .. } => *base_url = origin,
        }
        self
    }

    /// The messages URL for `model` (only Vertex puts the model there).
    #[cfg_attr(
        not(feature = "gcp"),
        expect(unused_variables, reason = "only Vertex names the model in the URL")
    )]
    fn url(&self, model: &str) -> String {
        match self {
            Endpoint::Direct { base_url, .. } | Endpoint::Proxy { base_url, .. } => {
                format!("{}/v1/messages", base_url.trim_end_matches('/'))
            }
            #[cfg(feature = "aws")]
            Endpoint::ClaudePlatformOnAws { base_url, .. } => {
                format!("{}/v1/messages", base_url.trim_end_matches('/'))
            }
            #[cfg(feature = "aws")]
            Endpoint::Bedrock { base_url, .. } => {
                format!("{}/anthropic/v1/messages", base_url.trim_end_matches('/'))
            }
            #[cfg(feature = "gcp")]
            Endpoint::Vertex {
                base_url,
                project,
                region,
                ..
            } => format!(
                "{}/v1/projects/{project}/locations/{region}/publishers/anthropic/models/{model}:rawPredict",
                base_url.trim_end_matches('/')
            ),
        }
    }

    /// How the body names `model` on this door.
    fn model_field(&self, model: &str) -> ModelField {
        match self {
            Endpoint::Direct { .. } | Endpoint::Proxy { .. } => ModelField::from(model),
            #[cfg(feature = "aws")]
            Endpoint::ClaudePlatformOnAws { .. } | Endpoint::Bedrock { .. } => {
                ModelField::from(model)
            }
            #[cfg(feature = "gcp")]
            Endpoint::Vertex { .. } => ModelField::in_url(),
        }
    }

    /// Headers the door needs beyond auth and the API version.
    fn door_headers(&self) -> Vec<(&'static str, String)> {
        match self {
            Endpoint::Direct { .. } | Endpoint::Proxy { .. } => Vec::new(),
            #[cfg(feature = "aws")]
            Endpoint::ClaudePlatformOnAws { workspace_id, .. } => {
                vec![(WORKSPACE_HEADER, workspace_id.clone())]
            }
            #[cfg(feature = "aws")]
            Endpoint::Bedrock { .. } => Vec::new(),
            #[cfg(feature = "gcp")]
            Endpoint::Vertex { .. } => Vec::new(),
        }
    }

    /// Whether the door takes the `anthropic-beta` header at all. Bedrock
    /// documents it as unsupported, so no beta — a request's own or one
    /// added with [`Anthropic::with_beta`] — is sent there.
    fn accepts_betas(&self) -> bool {
        match self {
            Endpoint::Direct { .. } | Endpoint::Proxy { .. } => true,
            #[cfg(feature = "aws")]
            Endpoint::ClaudePlatformOnAws { .. } => true,
            #[cfg(feature = "aws")]
            Endpoint::Bedrock { .. } => false,
            #[cfg(feature = "gcp")]
            Endpoint::Vertex { .. } => true,
        }
    }

    /// Whether this door's credentials are discovered at first use rather
    /// than held: the cloud doors resolve a platform chain (environment,
    /// profile, instance role, ADC), the key doors were given their key.
    #[must_use]
    pub fn lazy_credentials(&self) -> bool {
        match self {
            Endpoint::Direct { .. } | Endpoint::Proxy { .. } => false,
            #[cfg(feature = "aws")]
            Endpoint::ClaudePlatformOnAws { .. } | Endpoint::Bedrock { .. } => true,
            #[cfg(feature = "gcp")]
            Endpoint::Vertex { .. } => true,
        }
    }

    /// Resolve this door's credentials once without sending anything. The
    /// constructors are lazy so that building a client never touches the
    /// network or the disk; a binary about to serve questions calls this at
    /// startup so a host with no credentials fails there, naming the door,
    /// the way a missing `api_key_env` fails at load — not per question,
    /// after the spend reservation. A key door returns `Ok` without I/O.
    ///
    /// # Errors
    /// [`LlmError::Auth`] when the platform chain has no credentials.
    pub async fn probe(&self) -> Result<(), LlmError> {
        self.auth().await.map(drop)
    }

    /// This door's auth for one call: the key it holds, or the credentials
    /// its platform chain hands out now.
    #[cfg_attr(
        not(any(feature = "aws", feature = "gcp")),
        expect(
            clippy::unused_async,
            reason = "only the cloud doors fetch credentials"
        )
    )]
    async fn auth(&self) -> Result<Auth, LlmError> {
        Ok(match self {
            Endpoint::Direct { api_key, .. }
            | Endpoint::Proxy {
                api_key,
                header: ProxyAuth::XApiKey,
                ..
            } => Auth::XApiKey(api_key.clone()),
            Endpoint::Proxy {
                api_key,
                header: ProxyAuth::Bearer,
                ..
            } => Auth::Bearer(api_key.clone()),
            #[cfg(feature = "aws")]
            Endpoint::ClaudePlatformOnAws {
                region,
                credentials,
                ..
            } => Auth::SigV4 {
                door: AwsDoor::ClaudePlatform,
                region: region.clone(),
                credentials: credentials.credentials().await?,
            },
            #[cfg(feature = "aws")]
            Endpoint::Bedrock {
                region,
                credentials,
                ..
            } => Auth::SigV4 {
                door: AwsDoor::Bedrock,
                region: region.clone(),
                credentials: credentials.credentials().await?,
            },
            #[cfg(feature = "gcp")]
            Endpoint::Vertex { token, .. } => Auth::Bearer(token.token().await?),
        })
    }

    /// What this door supports server-side.
    #[must_use]
    pub fn capabilities(&self) -> Capabilities {
        let first_party = Capabilities {
            structured_output: StructuredOutput::Enforced,
            strict_tools: true,
            effort: true,
            cache_hints: true,
            refusal_fallbacks: true,
        };
        match self {
            Endpoint::Direct { .. } => first_party,
            Endpoint::Proxy { .. } => Capabilities {
                refusal_fallbacks: false,
                ..first_party
            },
            #[cfg(feature = "aws")]
            Endpoint::ClaudePlatformOnAws { .. } => first_party,
            #[cfg(feature = "aws")]
            Endpoint::Bedrock { .. } => Capabilities {
                structured_output: StructuredOutput::PromptOnly,
                strict_tools: false,
                refusal_fallbacks: false,
                ..first_party
            },
            #[cfg(feature = "gcp")]
            Endpoint::Vertex { .. } => Capabilities {
                refusal_fallbacks: false,
                ..first_party
            },
        }
    }

    /// A name for logs: the door and where it points, never a credential.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Endpoint::Direct { base_url, .. } => format!("direct {base_url}"),
            Endpoint::Proxy {
                base_url, header, ..
            } => format!("proxy {base_url} ({header:?})"),
            #[cfg(feature = "aws")]
            Endpoint::ClaudePlatformOnAws {
                region,
                workspace_id,
                ..
            } => format!("claude-platform-on-aws {region} {workspace_id}"),
            #[cfg(feature = "aws")]
            Endpoint::Bedrock {
                base_url, region, ..
            } => format!("bedrock {region} {base_url}"),
            #[cfg(feature = "gcp")]
            Endpoint::Vertex {
                project, region, ..
            } => format!("vertex {project}/{region}"),
        }
    }
}

/// One call's auth, resolved before the retry loop so the credential fetch
/// happens once and the per-attempt work is only what must be per attempt
/// (a `SigV4` signature carries its own timestamp).
enum Auth {
    XApiKey(ApiKey),
    Bearer(ApiKey),
    #[cfg(feature = "aws")]
    SigV4 {
        door: AwsDoor,
        region: String,
        credentials: aws_credential_types::Credentials,
    },
}

impl Auth {
    /// `builder` with this auth added; signing needs the finished request,
    /// so `SigV4` builds it, signs it and re-wraps it.
    #[cfg_attr(
        not(feature = "aws"),
        expect(
            unused_variables,
            clippy::unnecessary_wraps,
            reason = "only SigV4 re-wraps the request, and only signing can fail"
        )
    )]
    fn apply(
        &self,
        http: &reqwest::Client,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, LlmError> {
        match self {
            Auth::XApiKey(key) => Ok(builder.header("x-api-key", key.expose())),
            Auth::Bearer(key) => Ok(builder.bearer_auth(key.expose())),
            #[cfg(feature = "aws")]
            Auth::SigV4 {
                door,
                region,
                credentials,
            } => {
                let mut request = builder.build()?;
                sign_request(
                    &mut request,
                    *door,
                    region,
                    credentials,
                    std::time::SystemTime::now(),
                )?;
                Ok(reqwest::RequestBuilder::from_parts(http.clone(), request))
            }
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
    /// Whether the "masked off" warning has been logged; shared by clones so
    /// a door that cannot honour something says so once per backend (each
    /// stage builds its own, so at most once per stage), not per request.
    warned_mask: Arc<AtomicBool>,
}

impl Anthropic {
    /// Over `endpoint`, at [`DEFAULT_MODEL`].
    ///
    /// # Errors
    /// If the underlying HTTP client cannot be built.
    pub fn new(endpoint: Endpoint) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_mins(10))
            .build()?;
        Ok(Self {
            http,
            endpoint,
            model: DEFAULT_MODEL.to_owned(),
            betas: Vec::new(),
            warned_mask: Arc::new(AtomicBool::new(false)),
        })
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
    /// the ones a request itself needs, such as the fallbacks beta) — on the
    /// doors that take the header; Bedrock does not, and drops it with the
    /// mask warning.
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

    /// `req` with what this door cannot honour removed, per
    /// [`Endpoint::capabilities`]: `fallbacks` (and the beta header it would
    /// have needed) where there are no server-side fallbacks, the output
    /// schema where structured outputs are not enforced (the adapter has put
    /// it in the prompt), `strict` on tools where strict tools are not
    /// supported. The process-wide betas ([`Anthropic::with_beta`]) are not
    /// in the request, but a door with no `anthropic-beta` header drops them
    /// too ([`Endpoint::accepts_betas`]), and this is where that is said.
    /// Warns the first time.
    fn mask<'a>(&self, req: &'a ChatRequest) -> Cow<'a, ChatRequest> {
        let caps = self.endpoint.capabilities();
        let fallbacks = req.fallbacks.is_some() && !caps.refusal_fallbacks;
        let output = req.output.is_some() && caps.structured_output != StructuredOutput::Enforced;
        let strict = !caps.strict_tools && req.tools.iter().any(|t| t.strict);
        let betas = !self.betas.is_empty() && !self.endpoint.accepts_betas();
        if !(fallbacks || output || strict || betas) {
            return Cow::Borrowed(req);
        }
        if !self.warned_mask.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                endpoint = %self.endpoint.describe(),
                fallbacks,
                output_format = output,
                strict_tools = strict,
                betas,
                "this door cannot honour part of the request server-side; masked off (the schema is in the prompt, validation is client-side)"
            );
        }
        if !(fallbacks || output || strict) {
            return Cow::Borrowed(req);
        }
        let mut masked = req.clone();
        if fallbacks {
            masked.fallbacks = None;
        }
        if output {
            masked.output = None;
        }
        if strict {
            for tool in &mut masked.tools {
                tool.strict = false;
            }
        }
        Cow::Owned(masked)
    }

    /// Decode one reply: the API's error body on a non-2xx status, the
    /// response otherwise; a 2xx that does not decode (or does not read as
    /// a neutral response) is reported with whatever usage it carried so the
    /// spend cap can bill it.
    fn decode(reply: &Reply) -> Result<ChatResponse, LlmError> {
        if !reply.status.is_success() {
            let (kind, message) = match serde_json::from_slice::<ApiErrorBody>(&reply.body) {
                Ok(b) => (b.error.kind, b.error.message),
                Err(_) => (
                    "unknown".to_owned(),
                    String::from_utf8_lossy(&reply.body).into_owned(),
                ),
            };
            return Err(LlmError::Api {
                status: reply.status,
                kind,
                message,
            });
        }
        let parsed: MessagesResponse =
            serde_json::from_slice(&reply.body).map_err(|source| LlmError::Decode {
                source,
                billed: usage_of(&reply.body),
            })?;
        tracing::debug!(
            input = parsed.usage.input_tokens,
            output = parsed.usage.output_tokens,
            cache_read = ?parsed.usage.cache_read_input_tokens,
            stop = ?parsed.stop_reason,
            "messages ok"
        );
        from_wire(&parsed).map_err(|source| LlmError::Decode {
            source,
            billed: usage_of(&reply.body),
        })
    }
}

#[async_trait]
impl Backend for Anthropic {
    #[tracing::instrument(skip_all, fields(model = %self.model))]
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let req = self.mask(req);
        let body = serde_json::to_vec(&to_wire(self.endpoint.model_field(&self.model), &req)?)
            .map_err(|e| LlmError::Request(format!("serialize Messages API body: {e}")))?;
        let betas: Vec<&str> = if self.endpoint.accepts_betas() {
            self.betas
                .iter()
                .map(String::as_str)
                .chain(betas_for(&req))
                .collect()
        } else {
            Vec::new()
        };
        let url = self.endpoint.url(&self.model);
        let door_headers = self.endpoint.door_headers();
        let auth = self.endpoint.auth().await?;
        let build = || {
            let mut builder = self
                .http
                .post(&url)
                .header("anthropic-version", API_VERSION)
                .header("content-type", "application/json");
            for (name, value) in &door_headers {
                builder = builder.header(*name, value);
            }
            if !betas.is_empty() {
                builder = builder.header("anthropic-beta", betas.join(","));
            }
            auth.apply(&self.http, builder.body(body.clone()))
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
    use judge_llm::{
        ChatModel as _, Metered, RefusalFallback, SpendMeter, TextBlock, ToolChoice, Turn,
    };
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

    pub(super) fn req() -> ChatRequest {
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
        Anthropic::new(Endpoint::Direct {
            base_url: server.uri(),
            api_key: "k".into(),
        })
    }

    #[tokio::test]
    async fn usage_accumulates_and_cap_blocks_without_sending() -> Result<(), LlmError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "k"))
            .and(header("anthropic-version", API_VERSION))
            // 1M input + 200k output = $5 + $5 = $10 per call.
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(1_000_000, 200_000, 0, 0)),
            )
            .mount(&server)
            .await;
        let client = Metered::new(
            against(&server)?,
            SpendMeter::new().with_max_spend_usd(15.0)?,
        )?;
        let clone = client.clone();
        client.complete(&req()).await?;
        assert!(
            (client.meter().spent_usd() - 10.0).abs() < 1e-6,
            "{}",
            client.meter().spent_usd()
        );
        clone.complete(&req()).await?;
        assert!(
            (client.meter().spent_usd() - 20.0).abs() < 1e-6,
            "{}",
            client.meter().spent_usd()
        );
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
        assert!(
            matches!(&r, Err(LlmError::Api { kind, message, .. }) if kind == "invalid_request_error" && message == "nope"),
            "{r:?}"
        );
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
        assert!(matches!(
            client.complete(&req()).await,
            Err(LlmError::Decode {
                billed: Some(_),
                ..
            })
        ));
        assert!(
            (client.meter().spent_usd() - 5.0).abs() < 1e-6,
            "{}",
            client.meter().spent_usd()
        );
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
        let client = Metered::new(
            against(&server)?,
            SpendMeter::new().with_max_spend_usd(0.10)?,
        )?;
        let big = ChatRequest {
            max_tokens: 16_000,
            ..req()
        };
        assert!(matches!(
            client.complete(&big).await,
            Err(LlmError::SpendCapExceeded { .. })
        ));
        // The small request fits, and afterwards the reservation is gone.
        client.complete(&req()).await?;
        assert!(
            client.meter().spent_usd() < 0.001,
            "{}",
            client.meter().spent_usd()
        );
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
        client
            .complete(&ChatRequest {
                fallbacks: Some(RefusalFallback::Default),
                ..req()
            })
            .await?;
        client
            .with_beta("x-beta")
            .complete(&ChatRequest {
                fallbacks: Some(RefusalFallback::Default),
                ..req()
            })
            .await?;
        let reqs = server.received_requests().await.unwrap_or_default();
        let beta = |i: usize| {
            reqs.get(i)
                .and_then(|r| r.headers.get("anthropic-beta"))
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        assert_eq!(beta(0), None);
        assert_eq!(beta(1).as_deref(), Some(crate::wire::Fallbacks::BETA));
        assert_eq!(
            beta(2),
            Some(format!("x-beta,{}", crate::wire::Fallbacks::BETA))
        );
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
    fn from_vars_without_key_is_typed_and_blank_counts_as_unset() {
        for vars in [
            &[][..],
            &[("ANTHROPIC_API_KEY", "  ")][..],
            &[("ANTHROPIC_BASE_URL", "https://x.example")][..],
        ] {
            let get = |k: &str| {
                vars.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| (*v).to_owned())
            };
            assert!(
                matches!(
                    Endpoint::from_vars(get),
                    Err(LlmError::MissingApiKey {
                        var: "ANTHROPIC_API_KEY"
                    })
                ),
                "{vars:?}"
            );
        }
        let get = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "sk-ant-x".to_owned());
        assert!(matches!(
            Endpoint::from_vars(get),
            Ok(Endpoint::Direct { .. })
        ));
    }

    #[tokio::test]
    async fn a_proxy_uses_its_header_and_masks_the_fallbacks_beta() -> Result<(), LlmError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("authorization", "Bearer sk-proxy"))
            .and(header("anthropic-version", API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(1, 1, 0, 0)))
            .mount(&server)
            .await;
        let proxy = Endpoint::Proxy {
            base_url: format!("{}/", server.uri()),
            api_key: "sk-proxy".into(),
            header: ProxyAuth::Bearer,
        };
        assert!(!proxy.capabilities().refusal_fallbacks);
        assert_eq!(
            proxy.capabilities().structured_output,
            StructuredOutput::Enforced
        );
        let client = Anthropic::new(proxy)?;
        client
            .complete(&ChatRequest {
                fallbacks: Some(RefusalFallback::Default),
                ..req()
            })
            .await?;
        client
            .clone()
            .complete(&ChatRequest {
                fallbacks: Some(RefusalFallback::Default),
                ..req()
            })
            .await?;
        let reqs = server.received_requests().await.unwrap_or_default();
        assert_eq!(reqs.len(), 2);
        for r in &reqs {
            assert!(
                r.headers.get("anthropic-beta").is_none(),
                "no beta on a proxy"
            );
            assert!(
                r.headers.get("x-api-key").is_none(),
                "bearer, not x-api-key"
            );
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap_or_default();
            assert!(
                body.get("fallbacks").is_none(),
                "fallbacks masked off: {body}"
            );
        }
        assert!(
            client.warned_mask.load(Ordering::Relaxed),
            "warned (once; the clone shares the flag)"
        );

        // The x-api-key flavour, and no mask without fallbacks in the request.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "k2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(1, 1, 0, 0)))
            .expect(1)
            .mount(&server)
            .await;
        let client = Anthropic::new(Endpoint::Proxy {
            base_url: server.uri(),
            api_key: "k2".into(),
            header: ProxyAuth::XApiKey,
        })?;
        client.complete(&req()).await?;
        assert!(
            !client.warned_mask.load(Ordering::Relaxed),
            "nothing to mask, nothing to warn about"
        );
        Ok(())
    }

    /// A key door already holds its credential: the probe is `Ok` and does no I/O
    /// (the mock is never reached, and the key doors are not lazy).
    #[tokio::test]
    async fn key_doors_probe_without_io() -> Result<(), LlmError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        for endpoint in [
            Endpoint::Direct {
                base_url: server.uri(),
                api_key: "k".into(),
            },
            Endpoint::Proxy {
                base_url: server.uri(),
                api_key: "k".into(),
                header: ProxyAuth::Bearer,
            },
        ] {
            assert!(!endpoint.lazy_credentials());
            endpoint.probe().await?;
        }
        Ok(())
    }

    #[test]
    fn direct_endpoint_url_and_capabilities() -> Result<(), LlmError> {
        let e = Endpoint::Direct {
            base_url: "http://x/".into(),
            api_key: "k".into(),
        };
        assert_eq!(e.url("m"), "http://x/v1/messages");
        assert_eq!(e.model_field("m"), ModelField::from("m"));
        assert_eq!(
            e.clone().with_base_url("http://y").url("m"),
            "http://y/v1/messages"
        );
        assert_eq!(
            e.capabilities().structured_output,
            StructuredOutput::Enforced
        );
        assert!(e.capabilities().refusal_fallbacks);
        let a = Anthropic::new(e)?.with_model("claude-something");
        assert_eq!(a.model(), "claude-something");
        assert_eq!(a.provider(), "anthropic");
        Ok(())
    }
}

/// The cloud doors against wiremock: signed / bearer headers, paths, and the
/// body differences, with injected credentials so no cloud account is
/// touched. What went over the wire is what is asserted.
#[cfg(all(test, any(feature = "aws", feature = "gcp")))]
mod door_tests {
    use super::*;
    use judge_llm::{
        Effort, OutputSchema, RefusalFallback, TextBlock, ToolChoice, ToolSpec, Turn, schema_of,
    };
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{header, method, path},
    };

    fn ok_body() -> serde_json::Value {
        serde_json::json!({
            "id": "msg_1", "model": "claude-opus-5", "role": "assistant",
            "content": [{"type": "text", "text": "hi"}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
    }

    /// The synthesis shape: a strict tool, an output schema, effort, and
    /// the fallbacks beta — everything a door might have to mask.
    fn full_req() -> ChatRequest {
        ChatRequest {
            max_tokens: 64,
            system: vec![TextBlock::cached("sys")],
            turns: vec![Turn::User(vec![TextBlock::plain("hi")])],
            tools: vec![ToolSpec {
                name: "lookup_rules".into(),
                description: "d".into(),
                input_schema: schema_of::<judge_llm::LookupRulesInput>(),
                strict: true,
            }],
            tool_choice: ToolChoice::Auto { parallel: false },
            output: Some(OutputSchema::of::<judge_core::Verdict>()),
            effort: Some(Effort::Low),
            thinking: true,
            fallbacks: Some(RefusalFallback::Default),
        }
    }

    fn hdr(r: &Request, name: &str) -> Option<String> {
        r.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    fn body(r: &Request) -> serde_json::Value {
        serde_json::from_slice(&r.body).unwrap_or_default()
    }

    async fn only_request(server: &MockServer) -> Request {
        let mut reqs = server.received_requests().await.unwrap_or_default();
        assert_eq!(reqs.len(), 1);
        reqs.pop().unwrap_or_else(|| unreachable!("asserted above"))
    }

    /// `AWS4-HMAC-SHA256 Credential=<key>/<date>/<region>/<service>/aws4_request, SignedHeaders=…, Signature=<64 hex>`
    /// and an `x-amz-date` of the form `YYYYMMDDTHHMMSSZ`.
    #[cfg(feature = "aws")]
    fn assert_sigv4(r: &Request, access_key: &str, region: &str, service: &str) -> String {
        let auth = hdr(r, "authorization").unwrap_or_default();
        let date = hdr(r, "x-amz-date").unwrap_or_default();
        assert!(
            date.len() == 16
                && date.ends_with('Z')
                && date.chars().nth(8) == Some('T')
                && date.chars().filter(char::is_ascii_digit).count() == 14,
            "x-amz-date {date:?}"
        );
        let scope = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/{}/{region}/{service}/aws4_request, SignedHeaders=",
            date.get(..8).unwrap_or_default()
        );
        assert!(auth.starts_with(&scope), "{auth}\nexpected prefix {scope}");
        let signature = auth.split("Signature=").nth(1).unwrap_or_default();
        assert!(
            signature.len() == 64 && signature.chars().all(|c| c.is_ascii_hexdigit()),
            "{auth}"
        );
        assert!(hdr(r, "x-api-key").is_none(), "SigV4, not a key");
        auth.split("SignedHeaders=")
            .nth(1)
            .and_then(|s| s.split(',').next())
            .unwrap_or_default()
            .to_owned()
    }

    #[cfg(feature = "aws")]
    #[tokio::test]
    async fn claude_platform_on_aws_signs_with_its_service_and_sends_the_workspace()
    -> Result<(), LlmError> {
        use super::tests::req;
        use crate::aws::StaticCredentials;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header(WORKSPACE_HEADER, "wrkspc_01Test"))
            .and(header("anthropic-version", API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .mount(&server)
            .await;
        let endpoint = Endpoint::ClaudePlatformOnAws {
            base_url: server.uri(),
            region: "us-west-2".into(),
            workspace_id: "wrkspc_01Test".into(),
            credentials: Arc::new(StaticCredentials::new(
                "AKIDTEST",
                "secret",
                Some("session-token".into()),
            )),
        };
        assert_eq!(
            endpoint.capabilities(),
            Endpoint::direct("k").capabilities(),
            "first-party parity"
        );
        let client = Anthropic::new(endpoint)?;
        client.complete(&full_req()).await?;
        let r = only_request(&server).await;
        let signed = assert_sigv4(&r, "AKIDTEST", "us-west-2", "aws-external-anthropic");
        for name in [
            "anthropic-beta",
            "anthropic-version",
            WORKSPACE_HEADER,
            "content-type",
            "host",
            "x-amz-date",
            "x-amz-security-token",
        ] {
            assert!(
                signed.split(';').any(|h| h == name),
                "{name} not in SignedHeaders={signed}"
            );
        }
        assert_eq!(
            hdr(&r, "x-amz-security-token").as_deref(),
            Some("session-token")
        );
        assert_eq!(
            hdr(&r, "anthropic-beta").as_deref(),
            Some(crate::wire::Fallbacks::BETA),
            "no mask on this door"
        );
        let b = body(&r);
        assert_eq!(
            b.get("model"),
            Some(&serde_json::json!(DEFAULT_MODEL)),
            "bare model id, in the body: {b}"
        );
        assert_eq!(b.get("fallbacks"), Some(&serde_json::json!("default")));
        assert_eq!(
            b.pointer("/output_config/format/type"),
            Some(&serde_json::json!("json_schema"))
        );
        assert_eq!(b.pointer("/tools/0/strict"), Some(&serde_json::json!(true)));
        assert!(!client.warned_mask.load(Ordering::Relaxed));

        // Long-term keys carry no session token.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .mount(&server)
            .await;
        let endpoint = Endpoint::ClaudePlatformOnAws {
            base_url: server.uri(),
            region: "us-east-1".into(),
            workspace_id: "wrkspc_01Test".into(),
            credentials: Arc::new(StaticCredentials::new("AKIDLONG", "secret", None)),
        };
        Anthropic::new(endpoint)?.complete(&req()).await?;
        let r = only_request(&server).await;
        let signed = assert_sigv4(&r, "AKIDLONG", "us-east-1", "aws-external-anthropic");
        assert!(
            hdr(&r, "x-amz-security-token").is_none() && !signed.contains("x-amz-security-token"),
            "{signed}"
        );
        Ok(())
    }

    #[cfg(feature = "aws")]
    #[tokio::test]
    async fn bedrock_signs_as_bedrock_mantle_and_masks_what_it_does_not_support()
    -> Result<(), LlmError> {
        use crate::aws::StaticCredentials;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages"))
            .and(header("anthropic-version", API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .mount(&server)
            .await;
        let endpoint = Endpoint::Bedrock {
            base_url: server.uri(),
            region: "eu-central-1".into(),
            credentials: Arc::new(StaticCredentials::new("AKIDTEST", "secret", None)),
        };
        let caps = endpoint.capabilities();
        assert_eq!(caps.structured_output, StructuredOutput::PromptOnly);
        assert!(!caps.strict_tools && !caps.refusal_fallbacks && caps.effort && caps.cache_hints);
        let client = Anthropic::new(endpoint)?
            .with_model("anthropic.claude-opus-5")
            .with_beta("context-1m-2025-08-07");
        client.complete(&full_req()).await?;
        let r = only_request(&server).await;
        assert_sigv4(&r, "AKIDTEST", "eu-central-1", "bedrock-mantle");
        assert!(
            hdr(&r, "anthropic-beta").is_none(),
            "no beta header on this door: not the request's fallbacks beta, not the process-wide one"
        );
        assert!(hdr(&r, WORKSPACE_HEADER).is_none());
        let b = body(&r);
        assert_eq!(
            b.get("model"),
            Some(&serde_json::json!("anthropic.claude-opus-5"))
        );
        assert!(b.get("fallbacks").is_none(), "{b}");
        assert_eq!(
            b.get("output_config"),
            Some(&serde_json::json!({"effort": "low"})),
            "format masked, effort kept: {b}"
        );
        assert!(b.pointer("/tools/0/strict").is_none(), "strict masked: {b}");
        assert_eq!(
            b.pointer("/tools/0/name"),
            Some(&serde_json::json!("lookup_rules")),
            "the tool itself stays"
        );
        assert_eq!(
            b.pointer("/system/0/cache_control/type"),
            Some(&serde_json::json!("ephemeral")),
            "explicit breakpoints stay"
        );
        assert!(client.warned_mask.load(Ordering::Relaxed));
        Ok(())
    }

    /// A chain with nothing in it, as a host with no AWS credentials looks.
    #[cfg(feature = "aws")]
    #[derive(Debug)]
    struct NoCredentials;

    #[cfg(feature = "aws")]
    #[async_trait]
    impl AwsCredentials for NoCredentials {
        async fn credentials(&self) -> Result<aws_credential_types::Credentials, LlmError> {
            Err(LlmError::Auth {
                door: "aws",
                message: "no providers in chain".to_owned(),
            })
        }
    }

    /// The probe is the chain resolution and nothing else: a door whose
    /// chain is empty fails there, naming the door, and nothing is sent.
    #[cfg(feature = "aws")]
    #[tokio::test]
    async fn a_cloud_door_probes_its_chain_without_sending() -> Result<(), LlmError> {
        use super::tests::req;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .expect(0)
            .mount(&server)
            .await;
        let empty = Endpoint::Bedrock {
            base_url: server.uri(),
            region: "us-east-1".into(),
            credentials: Arc::new(NoCredentials),
        };
        assert!(empty.lazy_credentials());
        let Err(LlmError::Auth { door, message }) = empty.probe().await else {
            return Err(LlmError::Request("probe should fail".into()));
        };
        assert_eq!((door, message.as_str()), ("aws", "no providers in chain"));
        // The same failure per question, had nothing probed — and still nothing sent.
        let Err(LlmError::Auth { .. }) = Anthropic::new(empty)?.complete(&req()).await else {
            return Err(LlmError::Request("complete should fail".into()));
        };
        let full = Endpoint::Bedrock {
            base_url: server.uri(),
            region: "us-east-1".into(),
            credentials: Arc::new(crate::aws::StaticCredentials::new(
                "AKIDTEST", "secret", None,
            )),
        };
        full.probe().await?;
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "a probe sends nothing"
        );
        Ok(())
    }

    #[cfg(feature = "aws")]
    #[test]
    fn aws_constructors_derive_the_documented_urls() {
        let cpa = Endpoint::claude_platform_on_aws("us-west-2", "wrkspc_01X");
        assert_eq!(
            cpa.url("claude-opus-5"),
            "https://aws-external-anthropic.us-west-2.api.aws/v1/messages"
        );
        assert_eq!(
            cpa.door_headers(),
            [(WORKSPACE_HEADER, "wrkspc_01X".to_owned())]
        );
        assert_eq!(
            cpa.describe(),
            "claude-platform-on-aws us-west-2 wrkspc_01X"
        );
        let bedrock = Endpoint::bedrock("us-east-1");
        assert_eq!(
            bedrock.url("anthropic.claude-opus-5"),
            "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages"
        );
        assert!(bedrock.door_headers().is_empty());
        assert_eq!(
            bedrock.with_base_url("http://vpce.internal/").url("m"),
            "http://vpce.internal/anthropic/v1/messages"
        );
        let s = format!("{:?}", Endpoint::bedrock("us-east-1"));
        assert!(s.contains("DefaultChain") && s.contains("us-east-1"), "{s}");
    }

    #[cfg(feature = "gcp")]
    #[tokio::test]
    async fn vertex_uses_a_bearer_token_names_the_model_in_the_url_and_versions_the_body()
    -> Result<(), LlmError> {
        use crate::{gcp::StaticToken, wire::VERTEX_API_VERSION};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/my-proj/locations/global/publishers/anthropic/models/claude-opus-5:rawPredict"))
            .and(header("authorization", "Bearer ya29.test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .mount(&server)
            .await;
        let endpoint = Endpoint::Vertex {
            base_url: server.uri(),
            project: "my-proj".into(),
            region: "global".into(),
            token: Arc::new(StaticToken::new("ya29.test-token")),
        };
        let caps = endpoint.capabilities();
        assert!(!caps.refusal_fallbacks);
        assert_eq!(caps.structured_output, StructuredOutput::Enforced);
        assert!(caps.strict_tools);
        let client = Anthropic::new(endpoint)?;
        client.complete(&full_req()).await?;
        let r = only_request(&server).await;
        assert!(hdr(&r, "x-api-key").is_none() && hdr(&r, "x-amz-date").is_none());
        assert!(
            hdr(&r, "anthropic-beta").is_none(),
            "the fallbacks beta is masked off"
        );
        let b = body(&r);
        assert!(b.get("model").is_none(), "the model is in the URL: {b}");
        assert_eq!(
            b.get("anthropic_version"),
            Some(&serde_json::json!(VERTEX_API_VERSION))
        );
        assert!(b.get("fallbacks").is_none(), "{b}");
        assert_eq!(
            b.pointer("/output_config/format/type"),
            Some(&serde_json::json!("json_schema")),
            "structured outputs stay: {b}"
        );
        assert_eq!(b.pointer("/tools/0/strict"), Some(&serde_json::json!(true)));
        assert!(client.warned_mask.load(Ordering::Relaxed));
        let s = format!("{client:?}");
        assert!(!s.contains("test-token"), "{s}");
        Ok(())
    }

    #[cfg(feature = "gcp")]
    #[test]
    fn vertex_origins_follow_the_region_kind() {
        assert_eq!(vertex_origin("global"), "https://aiplatform.googleapis.com");
        assert_eq!(
            vertex_origin("us"),
            "https://aiplatform.us.rep.googleapis.com"
        );
        assert_eq!(
            vertex_origin("eu"),
            "https://aiplatform.eu.rep.googleapis.com"
        );
        assert_eq!(
            vertex_origin("us-east5"),
            "https://us-east5-aiplatform.googleapis.com"
        );
        let e = Endpoint::vertex("p", "us-east5");
        assert_eq!(
            e.url("claude-sonnet-4-6"),
            "https://us-east5-aiplatform.googleapis.com/v1/projects/p/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-6:rawPredict"
        );
        assert_eq!(e.model_field("claude-sonnet-4-6"), ModelField::in_url());
        assert_eq!(e.describe(), "vertex p/us-east5");
        assert!(format!("{e:?}").contains("Adc"));
    }
}
