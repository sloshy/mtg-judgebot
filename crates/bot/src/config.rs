//! `judge.toml` — which models the judge runs on, and through which
//! providers (`docs/proposals/providers.md` §5). The pipeline, the prompts
//! and the validation do not change with it; only who is on the other end
//! of the HTTP connection.
//!
//! **Zero config keeps working.** [`Config::load`] reads the file named by
//! `JUDGE_CONFIG`, else `./judge.toml` if it exists, else builds today's
//! setup from the environment exactly as before: Anthropic's first-party API
//! with `ANTHROPIC_API_KEY`, `claude-opus-5` for both stages, Voyage if
//! `VOYAGE_API_KEY` is set. So an existing deployment, the eval numbers and
//! the pinned prompt digest are untouched by upgrading.
//!
//! The file is typed on the way in: unknown keys are rejected
//! (`deny_unknown_fields`, so a typo is an error naming the key), model ids
//! and variable names must be non-empty, prices finite and non-negative,
//! dimensions positive. Secrets are named by environment variable
//! (`api_key_env`) and read at load — the variable must be present and
//! non-blank — into an [`ApiKey`], whose `Debug` is redacted; the value is
//! never in the file and never in a log line. What the loader cannot
//! express in types it checks by hand, naming the key: a stage naming a
//! provider that is not there or of the wrong kind, a model on an
//! `openai` provider with no price (the cap cannot estimate it: the built-in
//! table knows Anthropic's models and errs high for unknown ones there, but
//! an unknown model on an OpenAI-compatible server could be anything, so the
//! operator must say, or mark the provider `pricing = "free"`), an Anthropic
//! door that is not built into this binary, and a key that contradicts
//! another (`auth` with no `api_key_env`, a stage price on a free provider,
//! `effort` on a provider that will not send it) — a knob that would be
//! silently ignored is an error naming both keys instead.
//!
//! The loader is the one place a `judge.toml` is read; every binary calls
//! it, logs [`Config::summary`] at startup, and takes its models and
//! embedder from it.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use judge_anthropic::{Anthropic, Endpoint, ProxyAuth};
use judge_core::Embedder;
use judge_embed::VoyageEmbedder;
use judge_llm::{ApiKey, Backend, Capabilities, ChatRequest, ChatResponse, Effort, LlmError, Price, Pricing, SpendMeter, pricing_for};
use judge_openai::{Auth, Dialect, MaxTokensParam, OpenAi, StructuredOutputMode};
use nutype::nutype;
use serde::Deserialize;

use crate::{DepsConfig, Models};

/// The environment variable naming the config file.
pub const CONFIG_ENV: &str = "JUDGE_CONFIG";
/// The file used when `JUDGE_CONFIG` is unset and it exists.
pub const DEFAULT_PATH: &str = "judge.toml";
/// The provider name the environment setup uses for Anthropic, and the one
/// implied for embeddings when `[models.embed]` names none.
pub const ANTHROPIC_PROVIDER: &str = "anthropic";
/// The implied embeddings provider.
pub const VOYAGE_PROVIDER: &str = "voyage";
/// Where the environment setup reads its Anthropic key.
pub const ANTHROPIC_KEY_ENV: &str = "ANTHROPIC_API_KEY";
/// Where the environment setup reads its Voyage key.
pub const VOYAGE_KEY_ENV: &str = "VOYAGE_API_KEY";
/// The spend cap, read by both setups.
pub const MAX_SPEND_ENV: &str = "JUDGE_MAX_USD";
/// Voyage defaults, as `VoyageEmbedder::from_env` has them.
const VOYAGE_DEFAULT_MODEL: &str = "voyage-3.5";
const VOYAGE_DEFAULT_DIMENSIONS: usize = 1024;
const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

// ---------- the file, typed ----------

/// A model id: non-empty after trimming.
#[nutype(sanitize(trim), validate(not_empty), derive(Clone, Debug, Display, Deserialize, PartialEq, Eq, AsRef))]
pub struct ModelId(String);

/// An environment variable name: non-empty after trimming.
#[nutype(sanitize(trim), validate(not_empty), derive(Clone, Debug, Display, Deserialize, PartialEq, Eq, AsRef))]
pub struct EnvVar(String);

/// A provider name (the key under `[providers]`): non-empty after trimming.
#[nutype(sanitize(trim), validate(not_empty), derive(Clone, Debug, Display, Deserialize, PartialEq, Eq, PartialOrd, Ord, AsRef))]
pub struct ProviderName(String);

/// A base URL: an absolute `http`/`https` URL with a host (the backends
/// strip a trailing slash). Checked at load so `ollama:11434/v1` fails
/// naming the key, not on the first question in reqwest.
#[nutype(sanitize(trim), validate(with = validate_http_url, error = BadBaseUrl), derive(Clone, Debug, Display, Deserialize, PartialEq, Eq, AsRef))]
pub struct BaseUrl(String);

/// A `base_url` that is not an absolute `http`/`https` URL with a host.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("base_url must be an absolute http(s) URL with a host, like \"http://ollama:11434/v1\"")]
pub struct BadBaseUrl;

fn validate_http_url(s: &str) -> Result<(), BadBaseUrl> {
    match url::Url::parse(s) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.has_host() => Ok(()),
        _ => Err(BadBaseUrl),
    }
}

/// USD per million tokens: finite and non-negative.
#[nutype(validate(finite, greater_or_equal = 0.0), derive(Clone, Copy, Debug, Deserialize, PartialEq, AsRef))]
pub struct Usd(f64);

/// An embedding width: positive.
#[nutype(validate(greater = 0), derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, AsRef))]
pub struct Dimensions(usize);

/// An output ceiling: positive.
#[nutype(validate(greater = 0), derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, AsRef))]
pub struct MaxTokens(u32);

/// The whole file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    providers: BTreeMap<ProviderName, ProviderEntry>,
    models: ModelsEntry,
}

/// One `[providers.<name>]` table, by `kind`.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum ProviderEntry {
    /// The Messages API, through one of its doors.
    Anthropic {
        #[serde(default)]
        endpoint: Door,
        /// Origin override (`direct`) or the proxy's origin (`proxy`).
        #[serde(default)]
        base_url: Option<BaseUrl>,
        api_key_env: EnvVar,
        /// Which header a proxy wants the key in; `proxy` only.
        #[serde(default)]
        auth: Option<ProxyHeader>,
        #[serde(default)]
        pricing: Option<FreePricing>,
    },
    /// An OpenAI-compatible chat completions server.
    Openai {
        base_url: BaseUrl,
        /// Absent for a local server that needs no key.
        #[serde(default)]
        api_key_env: Option<EnvVar>,
        /// Which header the key travels in; `bearer` unless said otherwise,
        /// and an error without `api_key_env` (it would be ignored).
        #[serde(default)]
        auth: Option<OpenAiHeader>,
        #[serde(default)]
        structured_output: StructuredOutputKnob,
        #[serde(default = "yes")]
        strict_tools: bool,
        #[serde(default)]
        reasoning_effort: bool,
        #[serde(default)]
        max_tokens_param: MaxTokensKnob,
        #[serde(default)]
        cache_hints: bool,
        #[serde(default)]
        pricing: Option<FreePricing>,
    },
    /// Voyage AI embeddings.
    Voyage {
        /// Defaults to `VOYAGE_API_KEY`.
        #[serde(default)]
        api_key_env: Option<EnvVar>,
    },
}

fn yes() -> bool {
    true
}

/// `endpoint` on an `anthropic` provider. The full vocabulary is accepted so
/// the file reads the same across releases; the doors this binary does not
/// have are refused at load with [`ConfigError::NotBuilt`], not mistaken for
/// a typo.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Door {
    #[default]
    Direct,
    Proxy,
    ClaudePlatformOnAws,
    Bedrock,
    Vertex,
}

/// `auth` on an `anthropic` proxy.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ProxyHeader {
    XApiKey,
    Bearer,
}

/// `auth` on an `openai` provider.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum OpenAiHeader {
    Bearer,
    /// Azure's `api-key` header.
    ApiKey,
}

/// `structured_output` on an `openai` provider.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StructuredOutputKnob {
    #[default]
    JsonSchema,
    JsonObject,
    Prompt,
}

/// `max_tokens_param` on an `openai` provider.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MaxTokensKnob {
    #[default]
    MaxTokens,
    MaxCompletionTokens,
}

/// `pricing = "free"` on a provider: the only value the key takes.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum FreePricing {
    Free,
}

/// `[models]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsEntry {
    extract: StageEntry,
    synth: StageEntry,
    #[serde(default)]
    embed: Option<EmbedEntry>,
}

/// `[models.extract]` / `[models.synth]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StageEntry {
    provider: ProviderName,
    model: ModelId,
    #[serde(default)]
    max_tokens: Option<MaxTokens>,
    #[serde(default)]
    effort: Option<EffortKnob>,
    #[serde(default)]
    pricing: Option<PricingEntry>,
}

/// `effort` on a stage.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum EffortKnob {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl From<EffortKnob> for Effort {
    fn from(e: EffortKnob) -> Self {
        match e {
            EffortKnob::Low => Effort::Low,
            EffortKnob::Medium => Effort::Medium,
            EffortKnob::High => Effort::High,
            EffortKnob::Xhigh => Effort::XHigh,
            EffortKnob::Max => Effort::Max,
        }
    }
}

/// `[models.<stage>.pricing]`, USD per million tokens. The cache prices
/// default from the input price so as to err high: a cache read is never
/// dearer than an uncached read, and a cache write is billed at 1.25x the
/// input price by Anthropic, the dearest write premium of the providers the
/// judge knows.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PricingEntry {
    input: Usd,
    output: Usd,
    #[serde(default)]
    cache_read: Option<Usd>,
    #[serde(default)]
    cache_write: Option<Usd>,
}

impl PricingEntry {
    fn pricing(&self) -> Pricing {
        Pricing {
            input: self.input.into_inner(),
            output: self.output.into_inner(),
            cache_read: self.cache_read.unwrap_or(self.input).into_inner(),
            cache_write: self.cache_write.map_or(self.input.into_inner() * 1.25, Usd::into_inner),
        }
    }
}

/// `[models.embed]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbedEntry {
    /// Defaults to `voyage`.
    #[serde(default)]
    provider: Option<ProviderName>,
    model: ModelId,
    #[serde(default)]
    dimensions: Option<Dimensions>,
}

// ---------- errors ----------

/// Why a configuration could not be loaded. Every variant names the key or
/// variable to fix.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `JUDGE_CONFIG` names a file that is not there.
    #[error("{CONFIG_ENV}={path}: file not found")]
    Missing {
        /// The path.
        path: PathBuf,
    },
    /// The file could not be read. The cause is in the message rather than
    /// the error chain so `{:#}` does not print it twice.
    #[error("reading {path}: {cause}")]
    Read {
        /// The path.
        path: PathBuf,
        /// The failure.
        cause: std::io::Error,
    },
    /// The file is not a valid `judge.toml` (a typo names the key here).
    #[error("{path}: {cause}")]
    Parse {
        /// The path.
        path: PathBuf,
        /// The failure, with line and column.
        cause: toml::de::Error,
    },
    /// An `api_key_env` variable is unset or blank.
    #[error("providers.{provider}: {var} (api_key_env) is not set")]
    MissingEnv {
        /// The provider.
        provider: String,
        /// The variable.
        var: String,
    },
    /// A stage names a provider with no `[providers.<name>]` table.
    #[error("models.{stage}.provider = {provider:?} names no [providers.{provider}] table")]
    UnknownProvider {
        /// `extract`, `synth` or `embed`.
        stage: &'static str,
        /// The name.
        provider: String,
    },
    /// A stage names a provider of a kind that cannot serve it.
    #[error("models.{stage}: providers.{provider} is kind = {kind:?}, which cannot serve {stage} ({expected})")]
    WrongKind {
        /// The stage.
        stage: &'static str,
        /// The provider.
        provider: String,
        /// Its kind.
        kind: &'static str,
        /// What the stage needs.
        expected: &'static str,
    },
    /// A model on an `openai` provider has no price and the provider is not free.
    #[error(
        "models.{stage}: no price for {provider}/{model}; add [models.{stage}.pricing] (input, output per million tokens) or pricing = \"free\" on [providers.{provider}]"
    )]
    Unpriced {
        /// The stage.
        stage: &'static str,
        /// The provider.
        provider: String,
        /// The model.
        model: String,
    },
    /// The file asks for something this binary does not have.
    #[error("providers.{provider}: {what} is not built in this binary")]
    NotBuilt {
        /// The provider.
        provider: String,
        /// What was asked for.
        what: String,
    },
    /// A key that does not apply to the provider as configured.
    #[error("providers.{provider}: {key} {reason}")]
    Misplaced {
        /// The provider.
        provider: String,
        /// The key.
        key: &'static str,
        /// Why it does not apply.
        reason: &'static str,
    },
    /// A stage prices a model on a provider marked free: one of them is wrong.
    #[error("models.{stage}.pricing: providers.{provider} is pricing = \"free\"; remove one or the other")]
    PricedFree {
        /// The stage.
        stage: &'static str,
        /// The provider.
        provider: String,
    },
    /// A stage sets `effort` on an `openai` provider that would not send it.
    #[error("models.{stage}.effort: providers.{provider} has reasoning_effort = false, so the model would never see it; set reasoning_effort = true or drop effort")]
    EffortNotSent {
        /// The stage.
        stage: &'static str,
        /// The provider.
        provider: String,
    },
    /// No chat model at all: no file and no `ANTHROPIC_API_KEY`.
    #[error("no model configured: set {ANTHROPIC_KEY_ENV}, or write a {DEFAULT_PATH} (or point {CONFIG_ENV} at one)")]
    NoChatModel,
    /// `VOYAGE_DIMENSIONS` in the environment setup is not a positive integer.
    #[error("VOYAGE_DIMENSIONS must be a positive integer, got {value:?}")]
    BadDimensions {
        /// The value.
        value: String,
    },
    /// The spend cap, or a backend's HTTP client.
    #[error(transparent)]
    Llm(#[from] LlmError),
}

// ---------- the resolved configuration ----------

/// Where a [`Config`] came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A `judge.toml`.
    File(PathBuf),
    /// The environment (no file).
    Environment,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::File(p) => write!(f, "{}", p.display()),
            Source::Environment => f.write_str("env"),
        }
    }
}

/// A chat provider, resolved: the door and its credential.
#[derive(Clone, Debug)]
pub enum ChatProvider {
    /// The Messages API through `endpoint`.
    Anthropic {
        /// The door.
        endpoint: Endpoint,
    },
    /// An OpenAI-compatible server.
    OpenAi {
        /// Origin plus API prefix.
        base_url: String,
        /// How the key travels.
        auth: Auth,
        /// The server's departures from `OpenAI`.
        dialect: Dialect,
    },
}

impl ChatProvider {
    /// The provider key of the built-in price table and of `Backend::provider`.
    fn kind(&self) -> &'static str {
        match self {
            ChatProvider::Anthropic { .. } => judge_anthropic::BACKEND,
            ChatProvider::OpenAi { .. } => judge_openai::BACKEND,
        }
    }

    /// A description for the report: the door, never the key.
    fn describe(&self) -> serde_json::Value {
        match self {
            ChatProvider::Anthropic { endpoint: Endpoint::Direct { base_url, .. } } => {
                serde_json::json!({"kind": "anthropic", "endpoint": "direct", "base_url": base_url})
            }
            ChatProvider::Anthropic { endpoint: Endpoint::Proxy { base_url, header, .. } } => {
                let auth = match header {
                    ProxyAuth::XApiKey => "x-api-key",
                    ProxyAuth::Bearer => "bearer",
                };
                serde_json::json!({"kind": "anthropic", "endpoint": "proxy", "base_url": base_url, "auth": auth})
            }
            ChatProvider::OpenAi { base_url, auth, dialect } => serde_json::json!({
                "kind": "openai",
                "base_url": base_url,
                "auth": match auth { Auth::None => "none", Auth::Bearer(_) => "bearer", Auth::ApiKeyHeader(_) => "api-key" },
                "structured_output": match dialect.structured_output {
                    StructuredOutputMode::JsonSchema => "json_schema",
                    StructuredOutputMode::JsonObject => "json_object",
                    StructuredOutputMode::Prompt => "prompt",
                },
                "strict_tools": dialect.strict_tools,
                "reasoning_effort": dialect.reasoning_effort,
                "max_tokens_param": match dialect.max_tokens_param {
                    MaxTokensParam::MaxTokens => "max_tokens",
                    MaxTokensParam::MaxCompletionTokens => "max_completion_tokens",
                },
                "cache_hints": dialect.cache_hints,
            }),
        }
    }
}

/// One stage's model, resolved: provider, model, request knobs and price.
#[derive(Clone, Debug)]
pub struct Stage {
    /// The `[providers]` key (for logs and the report).
    pub provider: String,
    /// The provider.
    pub backend: ChatProvider,
    /// Model id, as the provider names it.
    pub model: String,
    /// Output ceiling.
    pub max_tokens: u32,
    /// How hard the model thinks.
    pub effort: Effort,
    /// What the cap bills it at.
    pub price: Price,
}

impl Stage {
    /// `provider/model`, as the summary line writes it.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }

    /// Build the backend.
    fn backend(&self) -> Result<ChatBackend, LlmError> {
        Ok(match &self.backend {
            ChatProvider::Anthropic { endpoint } => ChatBackend::Anthropic(Anthropic::new(endpoint.clone())?.with_model(&self.model)),
            ChatProvider::OpenAi { base_url, auth, dialect } => ChatBackend::OpenAi(OpenAi::new(base_url, auth.clone(), &self.model, *dialect)?),
        })
    }

    fn report(&self) -> serde_json::Value {
        serde_json::json!({
            "provider": self.provider,
            "model": self.model,
            "max_tokens": self.max_tokens,
            "effort": format!("{:?}", self.effort).to_lowercase(),
            "pricing": match self.price {
                Price::Free => serde_json::json!("free"),
                Price::Table(p) | Price::PerToken(p) => serde_json::json!({"input": p.input, "output": p.output, "cache_read": p.cache_read, "cache_write": p.cache_write}),
            },
        })
    }
}

/// The closed set of chat backends this binary can build, so a stage's
/// backend is chosen at load and the choice is exhaustive.
#[derive(Clone, Debug)]
enum ChatBackend {
    Anthropic(Anthropic),
    OpenAi(OpenAi),
}

#[async_trait::async_trait]
impl Backend for ChatBackend {
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        match self {
            ChatBackend::Anthropic(b) => b.complete(req).await,
            ChatBackend::OpenAi(b) => b.complete(req).await,
        }
    }
    fn capabilities(&self) -> Capabilities {
        match self {
            ChatBackend::Anthropic(b) => b.capabilities(),
            ChatBackend::OpenAi(b) => b.capabilities(),
        }
    }
    fn provider(&self) -> &'static str {
        match self {
            ChatBackend::Anthropic(b) => b.provider(),
            ChatBackend::OpenAi(b) => b.provider(),
        }
    }
    fn model(&self) -> &str {
        match self {
            ChatBackend::Anthropic(b) => b.model(),
            ChatBackend::OpenAi(b) => b.model(),
        }
    }
}

/// The embedder, resolved. Voyage is the only kind built so far
/// (OpenAI-compatible embeddings are phase 3).
#[derive(Clone, Debug)]
pub struct Embed {
    /// The `[providers]` key.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Vector width.
    pub dimensions: usize,
    api_key: ApiKey,
}

/// Both chat stages.
#[derive(Clone, Debug)]
struct Chat {
    extract: Stage,
    synth: Stage,
}

/// The resolved configuration: what every binary builds its models from.
#[derive(Clone, Debug)]
pub struct Config {
    source: Source,
    chat: Option<Chat>,
    embed: Option<Embed>,
    /// The process's one spend cap; every model built from this config bills to it.
    meter: SpendMeter,
}

impl Config {
    /// `JUDGE_CONFIG` if set (the file must exist), else `./judge.toml` if
    /// it exists, else the environment setup.
    ///
    /// # Errors
    /// See [`ConfigError`].
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(None)
    }

    /// [`Self::load`] with an explicit path taking precedence over
    /// `JUDGE_CONFIG` (`eval answer --config`).
    ///
    /// # Errors
    /// See [`ConfigError`].
    pub fn load_from(path: Option<&Path>) -> Result<Self, ConfigError> {
        let env_path = std::env::var(CONFIG_ENV).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()).map(PathBuf::from);
        let explicit = path.map(Path::to_path_buf).or(env_path);
        let path = match explicit {
            Some(p) if p.is_file() => p,
            Some(p) => return Err(ConfigError::Missing { path: p }),
            None if Path::new(DEFAULT_PATH).is_file() => PathBuf::from(DEFAULT_PATH),
            None => return Self::from_env(),
        };
        let text = std::fs::read_to_string(&path).map_err(|cause| ConfigError::Read { path: path.clone(), cause })?;
        Self::from_toml(&text, &path, |k| std::env::var(k).ok())
    }

    /// Parse and resolve `text` (the contents of `path`, named in errors),
    /// reading secrets with `env`.
    ///
    /// # Errors
    /// See [`ConfigError`].
    pub fn from_toml(text: &str, path: &Path, env: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let file: File = toml::from_str(text).map_err(|cause| ConfigError::Parse { path: path.to_path_buf(), cause })?;
        let resolver = Resolver { file: &file, env: &env };
        let extract = resolver.stage("extract", &file.models.extract)?;
        let synth = resolver.stage("synth", &file.models.synth)?;
        let embed = file.models.embed.as_ref().map(|e| resolver.embed(e)).transpose()?;
        let meter = SpendMeter::from_var(env(MAX_SPEND_ENV).as_deref())?;
        Ok(Self { source: Source::File(path.to_path_buf()), chat: Some(Chat { extract, synth }), embed, meter })
    }

    /// The environment setup: Anthropic direct with `ANTHROPIC_API_KEY`
    /// (and `ANTHROPIC_BASE_URL`) for both stages if the key is set, Voyage
    /// with `VOYAGE_API_KEY` (`VOYAGE_MODEL`, `VOYAGE_DIMENSIONS`) if that
    /// one is. Neither key is an error here: a binary that needs a chat
    /// model gets [`ConfigError::NoChatModel`] from [`Self::models`].
    ///
    /// # Errors
    /// A bad `JUDGE_MAX_USD` or `VOYAGE_DIMENSIONS`.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// [`Self::from_env`] over a variable lookup (tests). Nothing here reads
    /// the process environment: `JUDGE_MAX_USD` comes through `env` too.
    ///
    /// # Errors
    /// As [`Self::from_env`].
    pub fn from_vars(env: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let set = |k: &str| env(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let chat = set(ANTHROPIC_KEY_ENV)
            .map(|key| {
                let endpoint = Endpoint::Direct {
                    base_url: set("ANTHROPIC_BASE_URL").unwrap_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.to_owned()),
                    api_key: key.into(),
                };
                let model = judge_anthropic::DEFAULT_MODEL;
                let stage = |stage, max_tokens, effort| {
                    Ok::<_, ConfigError>(Stage {
                        provider: ANTHROPIC_PROVIDER.to_owned(),
                        backend: ChatProvider::Anthropic { endpoint: endpoint.clone() },
                        model: model.to_owned(),
                        max_tokens,
                        effort,
                        price: Price::Table(pricing_for(judge_anthropic::BACKEND, model).ok_or_else(|| ConfigError::Unpriced {
                            stage,
                            provider: ANTHROPIC_PROVIDER.to_owned(),
                            model: model.to_owned(),
                        })?),
                    })
                };
                let (x, s) = (crate::extract::ExtractConfig::default(), judge_llm::SynthConfig::default());
                Ok::<_, ConfigError>(Chat { extract: stage("extract", x.max_tokens, x.effort)?, synth: stage("synth", s.max_tokens, s.effort)? })
            })
            .transpose()?;
        let embed = set(VOYAGE_KEY_ENV)
            .map(|key| {
                let dimensions = match set("VOYAGE_DIMENSIONS") {
                    Some(s) => s.parse::<usize>().ok().filter(|d| *d > 0).ok_or(ConfigError::BadDimensions { value: s })?,
                    None => VOYAGE_DEFAULT_DIMENSIONS,
                };
                Ok::<_, ConfigError>(Embed {
                    provider: VOYAGE_PROVIDER.to_owned(),
                    model: set("VOYAGE_MODEL").unwrap_or_else(|| VOYAGE_DEFAULT_MODEL.to_owned()),
                    dimensions,
                    api_key: key.into(),
                })
            })
            .transpose()?;
        Ok(Self { source: Source::Environment, chat, embed, meter: SpendMeter::from_var(set(MAX_SPEND_ENV).as_deref())? })
    }

    /// Where this came from.
    #[must_use]
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The extraction stage, if a chat model is configured.
    #[must_use]
    pub fn extract(&self) -> Option<&Stage> {
        self.chat.as_ref().map(|c| &c.extract)
    }

    /// The synthesis stage, if a chat model is configured.
    #[must_use]
    pub fn synth(&self) -> Option<&Stage> {
        self.chat.as_ref().map(|c| &c.synth)
    }

    /// The embedder, if one is configured.
    #[must_use]
    pub fn embed(&self) -> Option<&Embed> {
        self.embed.as_ref()
    }

    /// The process's spend meter: what the models bill to and the front
    /// doors read.
    #[must_use]
    pub fn meter(&self) -> &SpendMeter {
        &self.meter
    }

    /// The metered models for both stages, on this config's meter.
    ///
    /// # Errors
    /// [`ConfigError::NoChatModel`] when the environment setup has no key;
    /// a backend whose HTTP client cannot be built.
    pub fn models(&self) -> Result<Models, ConfigError> {
        self.models_if_configured()?.ok_or(ConfigError::NoChatModel)
    }

    /// [`Self::models`], or `None` when no chat model is configured (the
    /// agent surface runs without one).
    ///
    /// # Errors
    /// A backend whose HTTP client cannot be built.
    pub fn models_if_configured(&self) -> Result<Option<Models>, ConfigError> {
        let Some(chat) = &self.chat else {
            return Ok(None);
        };
        Ok(Some(Models::priced(
            self.meter.clone(),
            chat.extract.backend()?,
            chat.extract.price,
            chat.synth.backend()?,
            chat.synth.price,
        )))
    }

    /// The request knobs each stage was configured with, for
    /// [`crate::build_deps_with`]. Defaults when no chat model is configured.
    #[must_use]
    pub fn deps_config(&self) -> DepsConfig {
        let mut cfg = DepsConfig::default();
        if let Some(chat) = &self.chat {
            cfg.extract.effort = chat.extract.effort;
            cfg.extract.max_tokens = chat.extract.max_tokens;
            cfg.synth.effort = chat.synth.effort;
            cfg.synth.max_tokens = chat.synth.max_tokens;
        }
        cfg
    }

    /// The embedder, if one is configured.
    ///
    /// # Errors
    /// None today; the signature is for the `OpenAI` embedder of phase 3.
    pub fn embedder(&self) -> Result<Option<Arc<dyn Embedder>>, ConfigError> {
        Ok(self.embed.as_ref().map(|e| {
            Arc::new(VoyageEmbedder::new(e.api_key.expose(), &e.model, e.dimensions)) as Arc<dyn Embedder>
        }))
    }

    /// The one-line summary every binary logs at startup:
    /// `config=judge.toml extract=ollama/qwen3:8b synth=anthropic/claude-opus-5 embed=voyage/voyage-3.5 cap=$5.00`.
    #[must_use]
    pub fn summary(&self) -> String {
        let stage = |s: Option<&Stage>| s.map_or_else(|| "none".to_owned(), Stage::label);
        let embed = self.embed.as_ref().map_or_else(|| "none".to_owned(), |e| format!("{}/{}", e.provider, e.model));
        format!(
            "config={} extract={} synth={} embed={} cap=${:.2}",
            self.source,
            stage(self.extract()),
            stage(self.synth()),
            embed,
            self.meter.max_spend_usd()
        )
    }

    /// The resolved configuration as JSON, secrets redacted (`judge-cli config`).
    #[must_use]
    pub fn report(&self) -> serde_json::Value {
        let mut providers = serde_json::Map::new();
        for s in [self.extract(), self.synth()].into_iter().flatten() {
            providers.entry(s.provider.clone()).or_insert_with(|| s.backend.describe());
        }
        if let Some(e) = &self.embed {
            providers.entry(e.provider.clone()).or_insert_with(|| serde_json::json!({"kind": "voyage"}));
        }
        serde_json::json!({
            "source": self.source.to_string(),
            "spend_cap_usd": self.meter.max_spend_usd(),
            "providers": providers,
            "models": {
                "extract": self.extract().map(Stage::report),
                "synth": self.synth().map(Stage::report),
                "embed": self.embed.as_ref().map(|e| serde_json::json!({"provider": e.provider, "model": e.model, "dimensions": e.dimensions})),
            },
        })
    }
}

/// Resolves the typed file into [`Stage`]s and [`Embed`]: looks providers
/// up, reads secrets, checks kinds and prices.
struct Resolver<'a, E: Fn(&str) -> Option<String>> {
    file: &'a File,
    env: &'a E,
}

impl<E: Fn(&str) -> Option<String>> Resolver<'_, E> {
    fn secret(&self, provider: &str, var: &str) -> Result<ApiKey, ConfigError> {
        (self.env)(var)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .map(ApiKey::from)
            .ok_or_else(|| ConfigError::MissingEnv { provider: provider.to_owned(), var: var.to_owned() })
    }

    fn provider<'b>(&'b self, stage: &'static str, name: &ProviderName) -> Result<&'b ProviderEntry, ConfigError> {
        self.file.providers.get(name).ok_or_else(|| ConfigError::UnknownProvider { stage, provider: name.to_string() })
    }

    fn stage(&self, stage: &'static str, entry: &StageEntry) -> Result<Stage, ConfigError> {
        let name = &entry.provider;
        let provider = self.provider(stage, name)?;
        let (backend, free) = match provider {
            ProviderEntry::Anthropic { endpoint, base_url, api_key_env, auth, pricing } => {
                let api_key = self.secret(name.as_ref(), api_key_env.as_ref())?;
                let endpoint = match endpoint {
                    Door::Direct => {
                        if auth.is_some() {
                            return Err(ConfigError::Misplaced { provider: name.to_string(), key: "auth", reason: "applies only to endpoint = \"proxy\"" });
                        }
                        Endpoint::Direct {
                            base_url: base_url.as_ref().map_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.to_owned(), ToString::to_string),
                            api_key,
                        }
                    }
                    Door::Proxy => {
                        let Some(base_url) = base_url else {
                            return Err(ConfigError::Misplaced { provider: name.to_string(), key: "base_url", reason: "is required for endpoint = \"proxy\"" });
                        };
                        let header = match auth.unwrap_or(ProxyHeader::XApiKey) {
                            ProxyHeader::XApiKey => ProxyAuth::XApiKey,
                            ProxyHeader::Bearer => ProxyAuth::Bearer,
                        };
                        Endpoint::Proxy { base_url: base_url.to_string(), api_key, header }
                    }
                    Door::ClaudePlatformOnAws | Door::Bedrock | Door::Vertex => {
                        let door = match endpoint {
                            Door::ClaudePlatformOnAws => "claude-platform-on-aws",
                            Door::Bedrock => "bedrock",
                            _ => "vertex",
                        };
                        return Err(ConfigError::NotBuilt { provider: name.to_string(), what: format!("endpoint = {door:?}") });
                    }
                };
                (ChatProvider::Anthropic { endpoint }, pricing.is_some())
            }
            ProviderEntry::Openai { base_url, api_key_env, auth, structured_output, strict_tools, reasoning_effort, max_tokens_param, cache_hints, pricing } => {
                let auth = match (api_key_env, auth) {
                    (None, None) => Auth::None,
                    (None, Some(_)) => {
                        return Err(ConfigError::Misplaced { provider: name.to_string(), key: "auth", reason: "needs api_key_env (there is no key to send)" });
                    }
                    (Some(var), None | Some(OpenAiHeader::Bearer)) => Auth::Bearer(self.secret(name.as_ref(), var.as_ref())?),
                    (Some(var), Some(OpenAiHeader::ApiKey)) => Auth::ApiKeyHeader(self.secret(name.as_ref(), var.as_ref())?),
                };
                if entry.effort.is_some() && !reasoning_effort {
                    return Err(ConfigError::EffortNotSent { stage, provider: name.to_string() });
                }
                let dialect = Dialect {
                    structured_output: match structured_output {
                        StructuredOutputKnob::JsonSchema => StructuredOutputMode::JsonSchema,
                        StructuredOutputKnob::JsonObject => StructuredOutputMode::JsonObject,
                        StructuredOutputKnob::Prompt => StructuredOutputMode::Prompt,
                    },
                    strict_tools: *strict_tools,
                    reasoning_effort: *reasoning_effort,
                    max_tokens_param: match max_tokens_param {
                        MaxTokensKnob::MaxTokens => MaxTokensParam::MaxTokens,
                        MaxTokensKnob::MaxCompletionTokens => MaxTokensParam::MaxCompletionTokens,
                    },
                    cache_hints: *cache_hints,
                };
                (ChatProvider::OpenAi { base_url: base_url.to_string(), auth, dialect }, pricing.is_some())
            }
            ProviderEntry::Voyage { .. } => {
                return Err(ConfigError::WrongKind { stage, provider: name.to_string(), kind: "voyage", expected: "a chat provider: kind = anthropic or openai" });
            }
        };
        let model = entry.model.to_string();
        // A free provider and a stage price contradict each other; an
        // explicit price beats the table (and is settled at, not merely
        // reserved at); the table errs high for Anthropic and knows nothing
        // else.
        let price = match (free, &entry.pricing) {
            (true, Some(_)) => return Err(ConfigError::PricedFree { stage, provider: name.to_string() }),
            (true, None) => Price::Free,
            (false, Some(p)) => Price::PerToken(p.pricing()),
            (false, None) => Price::Table(
                pricing_for(backend.kind(), &model)
                    .ok_or_else(|| ConfigError::Unpriced { stage, provider: name.to_string(), model: model.clone() })?,
            ),
        };
        let (default_max, default_effort) = if stage == "extract" {
            let x = crate::extract::ExtractConfig::default();
            (x.max_tokens, x.effort)
        } else {
            let s = judge_llm::SynthConfig::default();
            (s.max_tokens, s.effort)
        };
        Ok(Stage {
            provider: name.to_string(),
            backend,
            model,
            max_tokens: entry.max_tokens.map_or(default_max, MaxTokens::into_inner),
            effort: entry.effort.map_or(default_effort, Effort::from),
            price,
        })
    }

    fn embed(&self, entry: &EmbedEntry) -> Result<Embed, ConfigError> {
        let name = entry.provider.as_ref().map_or(VOYAGE_PROVIDER, AsRef::as_ref);
        // `voyage` is implied when the file has no table for it.
        let implied = ProviderEntry::Voyage { api_key_env: None };
        let provider = match self.file.providers.iter().find(|(k, _)| k.as_ref() == name).map(|(_, p)| p) {
            Some(p) => p,
            None if name == VOYAGE_PROVIDER => &implied,
            None => return Err(ConfigError::UnknownProvider { stage: "embed", provider: name.to_owned() }),
        };
        match provider {
            ProviderEntry::Voyage { api_key_env } => Ok(Embed {
                provider: name.to_owned(),
                model: entry.model.to_string(),
                dimensions: entry.dimensions.map_or(VOYAGE_DEFAULT_DIMENSIONS, Dimensions::into_inner),
                api_key: self.secret(name, api_key_env.as_ref().map_or(VOYAGE_KEY_ENV, AsRef::as_ref))?,
            }),
            ProviderEntry::Openai { .. } => {
                Err(ConfigError::NotBuilt { provider: name.to_owned(), what: "embeddings on an openai provider (phase 3)".into() })
            }
            ProviderEntry::Anthropic { .. } => {
                Err(ConfigError::WrongKind { stage: "embed", provider: name.to_owned(), kind: "anthropic", expected: "an embeddings provider: kind = voyage" })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_llm::StructuredOutput;
    use std::collections::HashMap;

    type R = Result<(), Box<dyn std::error::Error>>;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn load(text: &str, env: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let env = vars(env);
        Config::from_toml(text, Path::new("judge.toml"), |k| env.get(k).cloned())
    }

    const FULL: &str = r#"
[providers.anthropic]
kind = "anthropic"
endpoint = "direct"
api_key_env = "ANTHROPIC_API_KEY"

[providers.litellm]
kind = "openai"
base_url = "http://litellm:4000/v1"
api_key_env = "LITELLM_KEY"
structured_output = "json_object"
strict_tools = false
reasoning_effort = true
max_tokens_param = "max_completion_tokens"
cache_hints = true

[providers.ollama]
kind = "openai"
base_url = "http://ollama:11434/v1"
structured_output = "json_object"
pricing = "free"

[models.extract]
provider = "ollama"
model = "qwen3:8b"
max_tokens = 2000

[models.synth]
provider = "litellm"
model = "gpt-5"
effort = "high"
max_tokens = 12000
[models.synth.pricing]
input = 1.25
output = 10.0
cache_read = 0.125

[models.embed]
provider = "voyage"
model = "voyage-3.5"
dimensions = 1024
"#;

    #[test]
    fn a_full_file_resolves_every_knob() -> R {
        let c = load(FULL, &[("ANTHROPIC_API_KEY", "sk-ant"), ("LITELLM_KEY", "sk-lite"), ("VOYAGE_API_KEY", "pa-voy")])?;
        assert_eq!(c.source(), &Source::File(PathBuf::from("judge.toml")));
        let extract = c.extract().ok_or("extract")?;
        assert_eq!((extract.provider.as_str(), extract.model.as_str(), extract.max_tokens, extract.effort), ("ollama", "qwen3:8b", 2000, Effort::Low));
        assert_eq!(extract.price, Price::Free);
        let ChatProvider::OpenAi { base_url, auth, dialect } = &extract.backend else { return Err("openai".into()) };
        assert_eq!(base_url, "http://ollama:11434/v1");
        assert_eq!(auth, &Auth::None);
        assert_eq!(dialect, &Dialect { structured_output: StructuredOutputMode::JsonObject, ..Dialect::default() });

        let synth = c.synth().ok_or("synth")?;
        assert_eq!((synth.provider.as_str(), synth.model.as_str(), synth.max_tokens, synth.effort), ("litellm", "gpt-5", 12000, Effort::High));
        let Price::PerToken(rate) = synth.price else { return Err("priced".into()) };
        assert_eq!(rate, Pricing { input: 1.25, output: 10.0, cache_read: 0.125, cache_write: 1.5625 }, "cache_write defaults to 1.25x the input price");
        let ChatProvider::OpenAi { auth, dialect, .. } = &synth.backend else { return Err("openai".into()) };
        assert_eq!(auth, &Auth::Bearer("sk-lite".into()));
        assert_eq!(
            dialect,
            &Dialect {
                structured_output: StructuredOutputMode::JsonObject,
                strict_tools: false,
                reasoning_effort: true,
                max_tokens_param: MaxTokensParam::MaxCompletionTokens,
                cache_hints: true
            }
        );
        let embed = c.embed().ok_or("embed")?;
        assert_eq!((embed.provider.as_str(), embed.model.as_str(), embed.dimensions), ("voyage", "voyage-3.5", 1024));
        assert!(c.embedder()?.is_some_and(|e| e.dimensions() == 1024));

        let models = c.models()?;
        assert_eq!(models.extract().capabilities().structured_output, StructuredOutput::JsonMode);
        assert_eq!(models.synth().provider(), "openai");
        assert_eq!(models.synth().model(), "gpt-5");
        let deps = c.deps_config();
        assert_eq!((deps.extract.max_tokens, deps.synth.max_tokens, deps.synth.effort), (2000, 12000, Effort::High));
        assert_eq!(c.summary(), format!("config=judge.toml extract=ollama/qwen3:8b synth=litellm/gpt-5 embed=voyage/voyage-3.5 cap=${:.2}", c.meter().max_spend_usd()));
        Ok(())
    }

    #[test]
    fn secrets_never_appear_in_debug_or_the_report() -> R {
        let c = load(FULL, &[("ANTHROPIC_API_KEY", "sk-ant-secret"), ("LITELLM_KEY", "sk-lite-secret"), ("VOYAGE_API_KEY", "pa-voy-secret")])?;
        let dbg = format!("{c:?}");
        let report = c.report().to_string();
        for secret in ["sk-ant-secret", "sk-lite-secret", "pa-voy-secret"] {
            assert!(!dbg.contains(secret), "{dbg}");
            assert!(!report.contains(secret), "{report}");
        }
        assert!(dbg.contains("<redacted>"), "{dbg}");
        let r = c.report();
        assert_eq!(r.pointer("/providers/litellm/auth"), Some(&serde_json::json!("bearer")));
        assert_eq!(r.pointer("/providers/litellm/structured_output"), Some(&serde_json::json!("json_object")));
        assert_eq!(r.pointer("/models/extract/pricing"), Some(&serde_json::json!("free")));
        assert_eq!(r.pointer("/models/synth/pricing/output"), Some(&serde_json::json!(10.0)));
        assert_eq!(r.pointer("/models/embed/dimensions"), Some(&serde_json::json!(1024)));
        assert_eq!(r.pointer("/source"), Some(&serde_json::json!("judge.toml")));
        Ok(())
    }

    const MINIMAL: &str = r#"
[providers.anthropic]
kind = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"
[models.extract]
provider = "anthropic"
model = "claude-opus-5"
[models.synth]
provider = "anthropic"
model = "claude-opus-5"
"#;

    #[test]
    fn a_minimal_file_matches_the_environment_setup() -> R {
        let file = load(MINIMAL, &[("ANTHROPIC_API_KEY", "k")])?;
        let env = Config::from_vars(|k| (k == "ANTHROPIC_API_KEY").then(|| "k".to_owned()))?;
        for c in [&file, &env] {
            let (x, s) = (c.extract().ok_or("x")?, c.synth().ok_or("s")?);
            assert_eq!((x.max_tokens, x.effort), (2000, Effort::Low));
            assert_eq!((s.max_tokens, s.effort), (16000, Effort::High));
            assert_eq!(s.model, "claude-opus-5");
            assert_eq!(s.price, Price::Table(pricing_for("anthropic", "claude-opus-5").ok_or("table")?));
            let ChatProvider::Anthropic { endpoint: Endpoint::Direct { base_url, .. } } = &s.backend else { return Err("direct".into()) };
            assert_eq!(base_url, ANTHROPIC_DEFAULT_BASE_URL);
            assert!(c.embed().is_none());
            assert!(c.models()?.synth().capabilities().refusal_fallbacks);
        }
        assert_eq!(env.source(), &Source::Environment);
        assert!(env.summary().starts_with("config=env extract=anthropic/claude-opus-5 synth=anthropic/claude-opus-5 embed=none cap=$"), "{}", env.summary());
        Ok(())
    }

    #[test]
    fn the_environment_setup_without_a_key_has_no_chat_model() -> R {
        let c = Config::from_vars(|k| match k {
            "VOYAGE_API_KEY" => Some("pa".to_owned()),
            "VOYAGE_MODEL" => Some("voyage-3-large".to_owned()),
            "VOYAGE_DIMENSIONS" => Some("2048".to_owned()),
            "ANTHROPIC_API_KEY" => Some("   ".to_owned()),
            _ => None,
        })?;
        assert!(c.extract().is_none());
        assert!(matches!(c.models(), Err(ConfigError::NoChatModel)));
        assert!(c.models_if_configured()?.is_none());
        let e = c.embed().ok_or("embed")?;
        assert_eq!((e.model.as_str(), e.dimensions), ("voyage-3-large", 2048));
        assert!(c.summary().contains("extract=none synth=none embed=voyage/voyage-3-large"), "{}", c.summary());
        let bad = Config::from_vars(|k| match k {
            "VOYAGE_API_KEY" => Some("pa".to_owned()),
            "VOYAGE_DIMENSIONS" => Some("0".to_owned()),
            _ => None,
        });
        assert!(matches!(bad, Err(ConfigError::BadDimensions { .. })));
        let base = Config::from_vars(|k| match k {
            "ANTHROPIC_API_KEY" => Some("k".to_owned()),
            "ANTHROPIC_BASE_URL" => Some("http://proxy:8080".to_owned()),
            _ => None,
        })?;
        let ChatProvider::Anthropic { endpoint: Endpoint::Direct { base_url, .. } } = &base.synth().ok_or("s")?.backend else { return Err("direct".into()) };
        assert_eq!(base_url, "http://proxy:8080");
        Ok(())
    }

    #[test]
    fn a_typo_is_an_error_naming_the_key() {
        let typo = MINIMAL.replace("api_key_env", "api_key_evn");
        let err = load(&typo, &[("ANTHROPIC_API_KEY", "k")]).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("api_key_evn"), "{err}");
        assert!(err.starts_with("judge.toml:"), "{err}");
        let typo = FULL.replace("strict_tools = false", "strict_tool = false");
        let err = load(&typo, &[]).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("strict_tool"), "{err}");
        let typo = FULL.replace("[models.synth.pricing]", "[models.synth.pricng]");
        let err = load(&typo, &[]).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("pricng"), "{err}");
        let bad_value = FULL.replace("pricing = \"free\"", "pricing = \"cheap\"");
        let err = load(&bad_value, &[]).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("cheap") && err.contains("free"), "{err}");
        let bad_kind = MINIMAL.replace("kind = \"anthropic\"", "kind = \"antropic\"");
        let err = load(&bad_kind, &[]).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("antropic"), "{err}");
    }

    #[test]
    fn validators_reject_bad_values() {
        for (from, to) in [
            ("model = \"claude-opus-5\"\n[models.synth]", "model = \"  \"\n[models.synth]"),
            ("api_key_env = \"ANTHROPIC_API_KEY\"", "api_key_env = \"\""),
        ] {
            let text = MINIMAL.replacen(from, to, 1);
            assert!(text != MINIMAL, "replacement applied");
            let r = load(&text, &[("ANTHROPIC_API_KEY", "k")]);
            assert!(matches!(r, Err(ConfigError::Parse { .. })), "{from} -> {to}: {r:?}");
        }
        for (from, to) in [
            ("input = 1.25", "input = -1.0"),
            ("input = 1.25", "input = inf"),
            ("input = 1.25", "input = nan"),
            ("dimensions = 1024", "dimensions = 0"),
            ("max_tokens = 12000", "max_tokens = 0"),
            ("base_url = \"http://ollama:11434/v1\"", "base_url = \"ollama:11434/v1\""),
            ("base_url = \"http://ollama:11434/v1\"", "base_url = \"ftp://ollama:11434/v1\""),
            ("base_url = \"http://ollama:11434/v1\"", "base_url = \"http://\""),
            ("base_url = \"http://ollama:11434/v1\"", "base_url = \"\""),
        ] {
            let text = FULL.replacen(from, to, 1);
            assert!(text != FULL, "replacement applied");
            let r = load(&text, &[("ANTHROPIC_API_KEY", "k"), ("LITELLM_KEY", "k"), ("VOYAGE_API_KEY", "k")]);
            assert!(matches!(r, Err(ConfigError::Parse { .. })), "{from} -> {to}: {r:?}");
            let err = r.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(err.starts_with("judge.toml:"), "{err}");
            if from.starts_with("base_url") {
                assert!(err.contains("base_url must be an absolute http(s) URL"), "{err}");
            }
        }
        // A URL with a query (Azure's api-version) and a bare origin are both fine.
        for url in ["https://x.openai.azure.com/openai/deployments/d?api-version=2024-10-21", "http://localhost:11434", "http://127.0.0.1:1/v1/"] {
            let text = FULL.replacen("http://ollama:11434/v1", url, 1);
            assert!(load(&text, &[("ANTHROPIC_API_KEY", "k"), ("LITELLM_KEY", "k"), ("VOYAGE_API_KEY", "k")]).is_ok(), "{url}");
        }
    }

    #[test]
    fn contradictory_keys_are_errors_naming_both() {
        let env = [("ANTHROPIC_API_KEY", "k"), ("LITELLM_KEY", "k"), ("VOYAGE_API_KEY", "k"), ("AZURE_KEY", "k")];
        // A stage price on a free provider: which one did the operator mean?
        let priced_free = FULL.replace("provider = \"litellm\"\nmodel = \"gpt-5\"\neffort = \"high\"", "provider = \"ollama\"\nmodel = \"gpt-5\"");
        assert!(priced_free != FULL, "replacement applied");
        let err = load(&priced_free, &env).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, "models.synth.pricing: providers.ollama is pricing = \"free\"; remove one or the other");
        // `auth` with no key to send.
        let auth_no_key = FULL.replace("pricing = \"free\"", "pricing = \"free\"\nauth = \"api-key\"");
        let err = load(&auth_no_key, &env).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, "providers.ollama: auth needs api_key_env (there is no key to send)");
        // `effort` on an openai provider that will not send it (ollama has reasoning_effort = false).
        let effort_dropped = FULL.replace("model = \"qwen3:8b\"", "model = \"qwen3:8b\"\neffort = \"high\"");
        let err = load(&effort_dropped, &env).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.starts_with("models.extract.effort: providers.ollama has reasoning_effort = false"), "{err}");
        // ...while litellm (reasoning_effort = true) takes it, and an anthropic provider always does.
        assert!(load(FULL, &env).is_ok_and(|c| c.synth().is_some_and(|s| s.effort == Effort::High)));
        let on_anthropic = MINIMAL.replace("model = \"claude-opus-5\"\n", "model = \"claude-opus-5\"\neffort = \"max\"\n");
        assert!(load(&on_anthropic, &env).is_ok_and(|c| c.synth().is_some_and(|s| s.effort == Effort::Max)));
    }

    #[test]
    fn the_cap_is_read_through_the_injected_environment() -> R {
        let c = load(MINIMAL, &[("ANTHROPIC_API_KEY", "k"), ("JUDGE_MAX_USD", "1.25")])?;
        assert!((c.meter().max_spend_usd() - 1.25).abs() < 1e-9);
        let c = Config::from_vars(|k| match k {
            "ANTHROPIC_API_KEY" => Some("k".to_owned()),
            "JUDGE_MAX_USD" => Some("0.5".to_owned()),
            _ => None,
        })?;
        assert!((c.meter().max_spend_usd() - 0.5).abs() < 1e-9);
        assert!(c.summary().ends_with("cap=$0.50"), "{}", c.summary());
        let none = Config::from_vars(|_| None)?;
        assert!((none.meter().max_spend_usd() - judge_llm::DEFAULT_MAX_SPEND_USD).abs() < 1e-9, "hermetic: whatever the process has");
        let bad = load(MINIMAL, &[("ANTHROPIC_API_KEY", "k"), ("JUDGE_MAX_USD", "$5")]).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(bad.starts_with("JUDGE_MAX_USD"), "{bad}");
        Ok(())
    }

    #[tokio::test]
    async fn an_operator_price_on_an_anthropic_provider_is_what_the_meter_bills() -> R {
        use judge_llm::{TextBlock, ToolChoice, Turn};
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::{method, path}};
        // A proxy routing a model the table would price as Opus 5 ($5/$25); the operator says $1/$5.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-5",
                "content": [{"type": "text", "text": "hi"}], "stop_reason": "end_turn",
                "usage": {"input_tokens": 1_000_000, "output_tokens": 200_000}
            })))
            .mount(&server)
            .await;
        let text = format!(
            "[providers.gw]\nkind = \"anthropic\"\nendpoint = \"proxy\"\nbase_url = \"{}\"\napi_key_env = \"GW_KEY\"\n\
             [models.extract]\nprovider = \"gw\"\nmodel = \"claude-haiku-x\"\n[models.extract.pricing]\ninput = 1.0\noutput = 5.0\n\
             [models.synth]\nprovider = \"gw\"\nmodel = \"claude-haiku-x\"\n[models.synth.pricing]\ninput = 1.0\noutput = 5.0\n",
            server.uri()
        );
        let c = load(&text, &[("GW_KEY", "k"), ("JUDGE_MAX_USD", "100")])?;
        let models = c.models()?;
        let req = ChatRequest {
            max_tokens: 64,
            system: vec![],
            turns: vec![Turn::User(vec![TextBlock::plain("hi")])],
            tools: vec![],
            tool_choice: ToolChoice::None,
            effort: None,
            output: None,
            thinking: false,
            fallbacks: None,
        };
        models.synth().complete(&req).await?;
        assert!((c.meter().spent_usd() - 2.0).abs() < 1e-6, "billed at the operator's rate, not the table's $10: {}", c.meter().spent_usd());
        Ok(())
    }

    #[test]
    fn a_missing_or_blank_secret_is_an_error_naming_the_variable() {
        let err = load(MINIMAL, &[]).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, "providers.anthropic: ANTHROPIC_API_KEY (api_key_env) is not set");
        let err = load(MINIMAL, &[("ANTHROPIC_API_KEY", "  ")]);
        assert!(matches!(err, Err(ConfigError::MissingEnv { .. })));
        let err = load(FULL, &[("ANTHROPIC_API_KEY", "k"), ("VOYAGE_API_KEY", "k")]).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, "providers.litellm: LITELLM_KEY (api_key_env) is not set");
        // The implied voyage provider reads VOYAGE_API_KEY.
        let err = load(FULL, &[("ANTHROPIC_API_KEY", "k"), ("LITELLM_KEY", "k")]).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, "providers.voyage: VOYAGE_API_KEY (api_key_env) is not set");
    }

    #[test]
    fn an_unpriced_openai_model_is_refused_unless_the_provider_is_free() {
        let unpriced = FULL.replace("[models.synth.pricing]\ninput = 1.25\noutput = 10.0\ncache_read = 0.125\n", "");
        assert!(unpriced != FULL);
        let err = load(&unpriced, &[("ANTHROPIC_API_KEY", "k"), ("LITELLM_KEY", "k"), ("VOYAGE_API_KEY", "k")]).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(
            err,
            "models.synth: no price for litellm/gpt-5; add [models.synth.pricing] (input, output per million tokens) or pricing = \"free\" on [providers.litellm]"
        );
        // The same model on the free provider needs no price (ollama sends no effort, so that key goes too); on Anthropic the table errs high.
        let on_ollama = unpriced.replace("provider = \"litellm\"", "provider = \"ollama\"").replace("effort = \"high\"\n", "");
        let c = load(&on_ollama, &[("ANTHROPIC_API_KEY", "k"), ("VOYAGE_API_KEY", "k")]).ok();
        assert_eq!(c.as_ref().and_then(|c| c.synth()).map(|s| s.price), Some(Price::Free));
        let on_anthropic = unpriced.replace("provider = \"litellm\"\nmodel = \"gpt-5\"", "provider = \"anthropic\"\nmodel = \"claude-something-new\"");
        let c = load(&on_anthropic, &[("ANTHROPIC_API_KEY", "k"), ("VOYAGE_API_KEY", "k")]).ok();
        assert_eq!(c.as_ref().and_then(|c| c.synth()).map(|s| s.price), pricing_for("anthropic", "claude-opus-5").map(Price::Table));
    }

    #[test]
    fn provider_lookups_and_kinds_are_checked() {
        let env = [("ANTHROPIC_API_KEY", "k"), ("LITELLM_KEY", "k"), ("VOYAGE_API_KEY", "k")];
        let missing = FULL.replace("provider = \"ollama\"", "provider = \"olama\"");
        let err = load(&missing, &env).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, "models.extract.provider = \"olama\" names no [providers.olama] table");
        let wrong = FULL.replace("provider = \"ollama\"", "provider = \"voyage\"") + "\n[providers.voyage]\nkind = \"voyage\"\n";
        // A [providers.voyage] table with its own variable, and an embed entry naming no provider, both resolve.
        let own_var = FULL.replace("provider = \"voyage\"\n", "") + "\n[providers.voyage]\nkind = \"voyage\"\napi_key_env = \"MY_VOYAGE\"\n";
        let err = load(&own_var, &env).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, "providers.voyage: MY_VOYAGE (api_key_env) is not set");
        let c = load(&own_var, &[("ANTHROPIC_API_KEY", "k"), ("LITELLM_KEY", "k"), ("MY_VOYAGE", "k")]).ok();
        assert_eq!(c.as_ref().and_then(Config::embed).map(|e| e.provider.as_str()), Some("voyage"));
        let err = load(&wrong, &env).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.starts_with("models.extract: providers.voyage is kind = \"voyage\", which cannot serve extract"), "{err}");
        let embed_on_chat = FULL.replace("provider = \"voyage\"\nmodel = \"voyage-3.5\"", "provider = \"anthropic\"\nmodel = \"voyage-3.5\"");
        let err = load(&embed_on_chat, &env).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.starts_with("models.embed: providers.anthropic is kind = \"anthropic\""), "{err}");
        let embed_on_openai = FULL.replace("provider = \"voyage\"\nmodel = \"voyage-3.5\"", "provider = \"ollama\"\nmodel = \"nomic-embed-text\"");
        let err = load(&embed_on_openai, &env).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, "providers.ollama: embeddings on an openai provider (phase 3) is not built in this binary");
        let unknown_embed = FULL.replace("provider = \"voyage\"", "provider = \"voyag\"");
        let err = load(&unknown_embed, &env).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, "models.embed.provider = \"voyag\" names no [providers.voyag] table");
    }

    #[test]
    fn anthropic_doors() -> R {
        let proxy = MINIMAL.replace(
            "kind = \"anthropic\"\n",
            "kind = \"anthropic\"\nendpoint = \"proxy\"\nbase_url = \"http://litellm:4000/\"\nauth = \"bearer\"\n",
        );
        let c = load(&proxy, &[("ANTHROPIC_API_KEY", "k")])?;
        let ChatProvider::Anthropic { endpoint: Endpoint::Proxy { base_url, header, .. } } = &c.synth().ok_or("s")?.backend else {
            return Err("proxy".into());
        };
        assert_eq!((base_url.as_str(), *header), ("http://litellm:4000/", ProxyAuth::Bearer));
        assert!(!c.models()?.synth().capabilities().refusal_fallbacks);
        assert_eq!(c.report().pointer("/providers/anthropic/endpoint"), Some(&serde_json::json!("proxy")));

        let no_url = MINIMAL.replace("kind = \"anthropic\"\n", "kind = \"anthropic\"\nendpoint = \"proxy\"\n");
        let err = load(&no_url, &[("ANTHROPIC_API_KEY", "k")]).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, "providers.anthropic: base_url is required for endpoint = \"proxy\"");
        let auth_on_direct = MINIMAL.replace("kind = \"anthropic\"\n", "kind = \"anthropic\"\nauth = \"bearer\"\n");
        let err = load(&auth_on_direct, &[("ANTHROPIC_API_KEY", "k")]).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, "providers.anthropic: auth applies only to endpoint = \"proxy\"");
        for door in ["claude-platform-on-aws", "bedrock", "vertex"] {
            let cloud = MINIMAL.replace("kind = \"anthropic\"\n", &format!("kind = \"anthropic\"\nendpoint = \"{door}\"\n"));
            let err = load(&cloud, &[("ANTHROPIC_API_KEY", "k")]).err().map(|e| e.to_string()).unwrap_or_default();
            assert_eq!(err, format!("providers.anthropic: endpoint = \"{door}\" is not built in this binary"));
        }
        let unknown = MINIMAL.replace("kind = \"anthropic\"\n", "kind = \"anthropic\"\nendpoint = \"sideways\"\n");
        let err = load(&unknown, &[("ANTHROPIC_API_KEY", "k")]).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("sideways") && err.contains("bedrock"), "{err}");
        Ok(())
    }

    #[test]
    fn openai_auth_header_and_azure() -> R {
        let azure = MINIMAL.replace(
            "[providers.anthropic]\nkind = \"anthropic\"\napi_key_env = \"ANTHROPIC_API_KEY\"",
            "[providers.anthropic]\nkind = \"openai\"\nbase_url = \"https://x.openai.azure.com/openai/deployments/d?api-version=2024-10-21\"\napi_key_env = \"AZURE_KEY\"\nauth = \"api-key\"\npricing = \"free\"",
        );
        let c = load(&azure, &[("AZURE_KEY", "az")])?;
        let ChatProvider::OpenAi { auth, .. } = &c.synth().ok_or("s")?.backend else { return Err("openai".into()) };
        assert_eq!(auth, &Auth::ApiKeyHeader("az".into()));
        assert_eq!(c.report().pointer("/providers/anthropic/auth"), Some(&serde_json::json!("api-key")));
        Ok(())
    }

    #[tokio::test]
    async fn a_free_provider_never_trips_the_cap() -> R {
        use judge_llm::{TextBlock, ToolChoice, Turn};
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::{method, path}};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 5_000_000, "completion_tokens": 5_000_000}
            })))
            .expect(2)
            .mount(&server)
            .await;
        let text = format!(
            "[providers.ollama]\nkind = \"openai\"\nbase_url = \"{}/v1\"\npricing = \"free\"\n\
             [models.extract]\nprovider = \"ollama\"\nmodel = \"qwen3:8b\"\n\
             [models.synth]\nprovider = \"ollama\"\nmodel = \"qwen3:32b\"\n",
            server.uri()
        );
        let c = load(&text, &[])?;
        // A cap of zero: any per-token model would be refused before sending.
        c.meter().set_max_spend_usd(0.0)?;
        let models = c.models()?;
        let req = ChatRequest {
            max_tokens: 16_000,
            system: vec![],
            turns: vec![Turn::User(vec![TextBlock::plain("hi")])],
            tools: vec![],
            tool_choice: ToolChoice::None,
            output: None,
            effort: None,
            thinking: false,
            fallbacks: None,
        };
        models.extract().complete(&req).await?;
        models.synth().complete(&req).await?;
        assert_eq!(c.meter().calls(), 2, "counted");
        assert!(c.meter().spent_usd().abs() < f64::EPSILON, "never billed");
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), 2);
        Ok(())
    }

    #[test]
    fn load_from_prefers_the_argument_then_the_variable_and_reports_a_missing_file() -> R {
        let dir = std::env::temp_dir().join(format!("judge-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let good = dir.join("good.toml");
        std::fs::write(&good, MINIMAL)?;
        // The test environment may or may not carry ANTHROPIC_API_KEY; MINIMAL needs it.
        if std::env::var("ANTHROPIC_API_KEY").is_ok_and(|k| !k.trim().is_empty()) {
            let c = Config::load_from(Some(&good))?;
            assert_eq!(c.source(), &Source::File(good.clone()));
        } else {
            assert!(matches!(Config::load_from(Some(&good)), Err(ConfigError::MissingEnv { .. })));
        }
        let missing = dir.join("nope.toml");
        let err = Config::load_from(Some(&missing)).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("nope.toml") && err.contains("not found"), "{err}");
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, "[models\n")?;
        let err = Config::load_from(Some(&bad)).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.starts_with(&format!("{}:", bad.display())), "{err}");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
