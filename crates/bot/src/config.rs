//! `judge.toml` — which models the judge runs on, and through which
//! providers (`docs/PROVIDERS.md` §5). The pipeline, the prompts
//! and the validation do not change with it; only who is on the other end
//! of the HTTP connection.
//!
//! **Zero config works.** [`Config::load`] reads the file named by
//! `JUDGE_CONFIG`, else `./judge.toml` if it exists, else builds the default
//! setup from the environment: Anthropic's first-party API with
//! `ANTHROPIC_API_KEY`, `claude-opus-5-5` for both stages, Voyage if
//! `VOYAGE_API_KEY` is set. The eval numbers were measured on that setup;
//! the prompts, and so the pinned prompt digest, were tuned on Opus 5.
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
//! The cloud doors (`claude-platform-on-aws`, `bedrock`, `vertex`) hold no
//! key: their credentials come from the platform's chain, resolved lazily so
//! loading a config never touches the network. A binary about to serve
//! questions calls [`Config::probe_auth`] once after loading, so a host with
//! no credentials fails at startup naming the provider and the door, the
//! way a missing `api_key_env` does — not on the first question. Each
//! provider entry resolves to one [`Endpoint`], shared by the stages that
//! name it, so two stages on one cloud provider share one credential chain.
//!
//! The loader is the one place a `judge.toml` is read; every binary calls
//! it, logs [`Config::summary`] at startup, and takes its models and
//! embedder from it. The embedder comes with its vector space
//! ([`judge_embed::WithSpace`]) and, through [`Config::vectors`], behind the
//! stored-space check, so no binary can write a vector of the wrong model.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use judge_anthropic::{Anthropic, Endpoint, ProxyAuth};
use judge_core::{
    Commit, CommitHash, DiscordOperator, DiscordUsername, JudgeError, MissingContact,
    NetworkOperator, Operator, RepositoryUrl, SourceOffer, SupportEmail,
    operator::{OPERATOR_DISCORD_ENV, OPERATOR_EMAIL_ENV},
    source::SOURCE_URL_ENV,
};
use judge_embed::{OpenAiEmbedder, Provider, Space, VoyageEmbedder, WithSpace};
use judge_llm::{
    ApiKey, Backend, Capabilities, ChatRequest, ChatResponse, Effort, LlmError, Price, Pricing,
    SpendMeter, pricing_for,
};
use judge_openai::{Auth, Dialect, MaxTokensParam, OpenAi, StructuredOutputMode};
use nutype::nutype;
use serde::Deserialize;
use sqlx::PgPool;

use crate::{
    DepsConfig, Models,
    budget::{self, AlertWebhook, Budget, Period},
    db::Vectors,
};

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
/// The commit this binary was built from, stamped by `build.rs` (`JUDGE_COMMIT`
/// in the build environment, else `git rev-parse HEAD`); `None` when the
/// build had neither.
pub const BUILD_COMMIT: Option<&str> = option_env!("JUDGE_BUILD_COMMIT");
/// `"1"` when the build's working tree had uncommitted changes.
const BUILD_DIRTY: Option<&str> = option_env!("JUDGE_BUILD_DIRTY");
/// Voyage defaults, as `VoyageEmbedder::from_env` has them.
const VOYAGE_DEFAULT_MODEL: &str = "voyage-3.5";
const VOYAGE_DEFAULT_DIMENSIONS: usize = 1024;
const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

// ---------- the file, typed ----------

/// A model id: non-empty after trimming.
#[nutype(
    sanitize(trim),
    validate(not_empty),
    derive(Clone, Debug, Display, Deserialize, PartialEq, Eq, AsRef)
)]
pub struct ModelId(String);

/// An environment variable name: non-empty after trimming.
#[nutype(
    sanitize(trim),
    validate(not_empty),
    derive(Clone, Debug, Display, Deserialize, PartialEq, Eq, AsRef)
)]
pub struct EnvVar(String);

/// A provider name (the key under `[providers]`): non-empty after trimming.
#[nutype(
    sanitize(trim),
    validate(not_empty),
    derive(
        Clone,
        Debug,
        Display,
        Deserialize,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        AsRef
    )
)]
pub struct ProviderName(String);

/// A cloud region (`us-west-2`, `europe-west1`, `us-east5`, Vertex's
/// `global`/`us`/`eu`): lowercase letters, digits and hyphens. It is
/// interpolated into a hostname, so anything else fails at load naming the
/// key rather than as a transport error on the first question.
#[nutype(sanitize(trim), validate(with = validate_region, error = BadRegion), derive(Clone, Debug, Display, Deserialize, PartialEq, Eq, AsRef))]
pub struct Region(String);

/// A `region` that is not lowercase letters, digits and hyphens.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "region must be the cloud region's id in lowercase letters, digits and hyphens (us-west-2, europe-west1, global)"
)]
pub struct BadRegion;

/// A GCP project id (or number): lowercase letters, digits and hyphens. It
/// is interpolated into the Vertex URL path, so the same rule as
/// [`Region`]; a legacy domain-scoped id (`example.com:proj`) is refused.
#[nutype(sanitize(trim), validate(with = validate_project, error = BadProject), derive(Clone, Debug, Display, Deserialize, PartialEq, Eq, AsRef))]
pub struct Project(String);

/// A `project` that is not lowercase letters, digits and hyphens.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "project must be the GCP project id (or number) in lowercase letters, digits and hyphens, like \"my-proj-123456\""
)]
pub struct BadProject;

/// Non-empty, lowercase ASCII letters, digits and hyphens, and neither
/// starts nor ends with a hyphen: the DNS-label shape both platforms use.
fn is_label(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

fn validate_region(s: &str) -> Result<(), BadRegion> {
    is_label(s).then_some(()).ok_or(BadRegion)
}

fn validate_project(s: &str) -> Result<(), BadProject> {
    is_label(s).then_some(()).ok_or(BadProject)
}

/// A Claude Platform on AWS workspace id: `wrkspc_` and an alphanumeric
/// identifier, the form the platform documents. Checked at load so an ARN
/// or a name pasted in its place fails naming the key, not as a 403 on the
/// first question.
#[nutype(sanitize(trim), validate(with = validate_workspace_id, error = BadWorkspaceId), derive(Clone, Debug, Display, Deserialize, PartialEq, Eq, AsRef))]
pub struct WorkspaceId(String);

/// A `workspace_id` that is not `wrkspc_` plus an alphanumeric identifier.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "workspace_id must be the workspace's id, \"wrkspc_\" followed by letters and digits (AWS Console > Claude Platform on AWS > Workspaces)"
)]
pub struct BadWorkspaceId;

fn validate_workspace_id(s: &str) -> Result<(), BadWorkspaceId> {
    match s.strip_prefix("wrkspc_") {
        Some(rest) if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric()) => Ok(()),
        _ => Err(BadWorkspaceId),
    }
}

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
#[nutype(
    validate(finite, greater_or_equal = 0.0),
    derive(Clone, Copy, Debug, Deserialize, PartialEq, AsRef)
)]
pub struct Usd(f64);

/// An embedding width: `1..=`[`MAX_DIMENSIONS`]. Bounded at load so a
/// `dimensions = 3072` fails naming the key, not after `reembed --yes` has
/// retyped the columns and `CREATE INDEX ... USING hnsw` rolls it back.
#[nutype(validate(with = validate_dimensions, error = BadDimensions), derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, AsRef))]
pub struct Dimensions(usize);

/// The widest `vector(N)` pgvector's HNSW index accepts. Every `embedding`
/// column is HNSW-indexed, so a wider model must be asked for a narrower
/// (matryoshka) width, or is unusable here.
pub const MAX_DIMENSIONS: usize = 2000;

/// A width outside `1..=`[`MAX_DIMENSIONS`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "dimensions must be 1..={MAX_DIMENSIONS} (pgvector's HNSW index limit); a wider model must be asked for a narrower width (dimensions = 1024 with send_dimensions = true)"
)]
pub struct BadDimensions;

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "nutype's validator signature"
)]
fn validate_dimensions(n: &usize) -> Result<(), BadDimensions> {
    if (1..=MAX_DIMENSIONS).contains(n) {
        Ok(())
    } else {
        Err(BadDimensions)
    }
}

/// An output ceiling: positive.
#[nutype(
    validate(greater = 0),
    derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, AsRef)
)]
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
        /// Origin override: optional on every door (the cloud doors derive
        /// theirs from `region`), required for `proxy`.
        #[serde(default)]
        base_url: Option<BaseUrl>,
        /// The key's variable: `direct` and `proxy` only. The cloud doors
        /// take credentials from their platform's chain, never from here.
        #[serde(default)]
        api_key_env: Option<EnvVar>,
        /// Which header a proxy wants the key in; `proxy` only.
        #[serde(default)]
        auth: Option<ProxyHeader>,
        /// The cloud doors' region (`claude-platform-on-aws`, `bedrock`, `vertex`).
        #[serde(default)]
        region: Option<Region>,
        /// `claude-platform-on-aws` only.
        #[serde(default)]
        workspace_id: Option<WorkspaceId>,
        /// `vertex` only.
        #[serde(default)]
        project: Option<Project>,
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
        /// Embeddings: whether `dimensions` goes on the wire. Off for a
        /// server that rejects the field (vLLM with a model that has no
        /// matryoshka training); the width is then only checked on the reply.
        #[serde(default = "yes")]
        send_dimensions: bool,
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
/// the file reads the same across releases; a door this binary was built
/// without (Cargo features `aws`, `gcp`) is refused at load with
/// [`ConfigError::NotBuilt`] naming the feature, not mistaken for a typo.
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

impl Door {
    /// The value as the file spells it.
    const fn name(self) -> &'static str {
        match self {
            Door::Direct => "direct",
            Door::Proxy => "proxy",
            Door::ClaudePlatformOnAws => "claude-platform-on-aws",
            Door::Bedrock => "bedrock",
            Door::Vertex => "vertex",
        }
    }

    /// Whether the door takes a key (`api_key_env`) rather than a platform
    /// credential chain.
    const fn takes_key(self) -> bool {
        matches!(self, Door::Direct | Door::Proxy)
    }
}

/// `value`, or [`ConfigError::Required`] naming the key and the door.
fn required<T>(
    provider: &str,
    key: &'static str,
    door: Door,
    value: Option<T>,
) -> Result<T, ConfigError> {
    value.ok_or_else(|| ConfigError::Required {
        provider: provider.to_owned(),
        key,
        door: door.name(),
    })
}

/// Whether `model` is one of Bedrock's documented forms, `anthropic.<model>`
/// or `<profile>.anthropic.<model>` (an inference profile such as
/// `global.anthropic.claude-opus-5-5`) — a whole `anthropic` segment, not a
/// substring, so `claude-opus-5-anthropic.x` is as wrong as a bare id.
fn bedrock_model_id(model: &str) -> bool {
    model.starts_with("anthropic.") || model.contains(".anthropic.")
}

/// A cloud door's origin: the configured override, or the constructor's.
#[cfg(any(feature = "aws", feature = "gcp"))]
fn at_origin(endpoint: Endpoint, base_url: Option<&BaseUrl>) -> Endpoint {
    match base_url {
        Some(b) => endpoint.with_base_url(b.to_string()),
        None => endpoint,
    }
}

// The cloud doors. Each exists only when its feature is compiled in; the
// stub otherwise names the feature. The unused-parameter shape of the stubs
// is deliberate: both signatures must agree so the caller does not care.

#[cfg(feature = "aws")]
fn claude_platform_on_aws(
    provider: &str,
    region: Option<&Region>,
    workspace_id: Option<&WorkspaceId>,
    base_url: Option<&BaseUrl>,
) -> Result<Endpoint, ConfigError> {
    let region = required(provider, "region", Door::ClaudePlatformOnAws, region)?;
    let workspace_id = required(
        provider,
        "workspace_id",
        Door::ClaudePlatformOnAws,
        workspace_id,
    )?;
    Ok(at_origin(
        Endpoint::claude_platform_on_aws(region.to_string(), workspace_id.to_string()),
        base_url,
    ))
}

#[cfg(not(feature = "aws"))]
fn claude_platform_on_aws(
    provider: &str,
    _: Option<&Region>,
    _: Option<&WorkspaceId>,
    _: Option<&BaseUrl>,
) -> Result<Endpoint, ConfigError> {
    Err(ConfigError::NotBuilt {
        provider: provider.to_owned(),
        what: format!("endpoint = {:?}", Door::ClaudePlatformOnAws.name()),
        feature: "aws",
    })
}

#[cfg(feature = "aws")]
fn bedrock(
    provider: &str,
    region: Option<&Region>,
    base_url: Option<&BaseUrl>,
) -> Result<Endpoint, ConfigError> {
    let region = required(provider, "region", Door::Bedrock, region)?;
    Ok(at_origin(Endpoint::bedrock(region.to_string()), base_url))
}

#[cfg(not(feature = "aws"))]
fn bedrock(
    provider: &str,
    _: Option<&Region>,
    _: Option<&BaseUrl>,
) -> Result<Endpoint, ConfigError> {
    Err(ConfigError::NotBuilt {
        provider: provider.to_owned(),
        what: format!("endpoint = {:?}", Door::Bedrock.name()),
        feature: "aws",
    })
}

#[cfg(feature = "gcp")]
fn vertex(
    provider: &str,
    project: Option<&Project>,
    region: Option<&Region>,
    base_url: Option<&BaseUrl>,
) -> Result<Endpoint, ConfigError> {
    let project = required(provider, "project", Door::Vertex, project)?;
    let region = required(provider, "region", Door::Vertex, region)?;
    Ok(at_origin(
        Endpoint::vertex(project.to_string(), region.to_string()),
        base_url,
    ))
}

#[cfg(not(feature = "gcp"))]
fn vertex(
    provider: &str,
    _: Option<&Project>,
    _: Option<&Region>,
    _: Option<&BaseUrl>,
) -> Result<Endpoint, ConfigError> {
    Err(ConfigError::NotBuilt {
        provider: provider.to_owned(),
        what: format!("endpoint = {:?}", Door::Vertex.name()),
        feature: "gcp",
    })
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
            cache_write: self
                .cache_write
                .map_or(self.input.into_inner() * 1.25, Usd::into_inner),
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
    #[error(
        "models.{stage}: providers.{provider} is kind = {kind:?}, which cannot serve {stage} ({expected})"
    )]
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
    /// The file asks for something this binary was built without.
    #[error(
        "providers.{provider}: {what} is not built in this binary; it needs the {feature:?} feature of judge-anthropic (on by default)"
    )]
    NotBuilt {
        /// The provider.
        provider: String,
        /// What was asked for.
        what: String,
        /// The Cargo feature that would have built it.
        feature: &'static str,
    },
    /// A key the door needs is missing.
    #[error("providers.{provider}: {key} is required for endpoint = {door:?}")]
    Required {
        /// The provider.
        provider: String,
        /// The key.
        key: &'static str,
        /// The door.
        door: &'static str,
    },
    /// A stage on Bedrock names a model without Bedrock's `anthropic.`
    /// prefix; the door would answer every question with a 400.
    #[error(
        "models.{stage}.model = {model:?}: Claude in Amazon Bedrock names models with an `anthropic.` prefix (anthropic.claude-opus-5-5, or an inference profile such as global.anthropic.claude-opus-4-6-v1)"
    )]
    BedrockModelId {
        /// The stage.
        stage: &'static str,
        /// The model.
        model: String,
    },
    /// A cloud door's credential chain handed out nothing when
    /// [`Config::probe_auth`] asked at startup.
    #[error("providers.{provider} ({endpoint}): no credentials: {cause}")]
    Credentials {
        /// The provider.
        provider: String,
        /// The door, as [`Endpoint::describe`] names it.
        endpoint: String,
        /// The chain's answer.
        cause: LlmError,
    },
    /// `JUDGE_SOURCE_URL` is set but is not an http(s) URL.
    #[error("{SOURCE_URL_ENV}={value:?}: not an http(s) URL")]
    BadSourceUrl {
        /// The value as set.
        value: String,
    },
    /// An operator contact is set but is not what it claims to be.
    #[error("{var}={value:?}: not {expected}")]
    BadContact {
        /// The variable.
        var: &'static str,
        /// The value as set.
        value: String,
        /// What the variable holds when it is right.
        expected: &'static str,
    },
    /// A surface was started without the contact it must name.
    #[error(transparent)]
    MissingContact(#[from] MissingContact),
    /// A budget setting is set but is not one of its values.
    #[error("{var}={value:?}: not {expected}")]
    BadBudget {
        /// The variable.
        var: &'static str,
        /// The value as set (never the webhook URL, which is a credential).
        value: String,
        /// What the variable holds when it is right.
        expected: &'static str,
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
    #[error(
        "models.{stage}.pricing: providers.{provider} is pricing = \"free\"; remove one or the other"
    )]
    PricedFree {
        /// The stage.
        stage: &'static str,
        /// The provider.
        provider: String,
    },
    /// `[models.embed]` on an `openai` provider names no `dimensions`: the
    /// width is the columns' `vector(N)`, and an OpenAI-compatible model has
    /// no default this loader could know.
    #[error(
        "models.embed.dimensions is required on providers.{provider} (kind = \"openai\"): the vector width the model produces (text-embedding-3-small: 1536, nomic-embed-text: 768)"
    )]
    EmbedDimensions {
        /// The provider.
        provider: String,
    },
    /// The embedder's HTTP client could not be built.
    #[error("building the embedder: {0}")]
    Embedder(JudgeError),
    /// A stage sets `effort` on an `openai` provider that would not send it.
    #[error(
        "models.{stage}.effort: providers.{provider} has reasoning_effort = false, so the model would never see it; set reasoning_effort = true or drop effort"
    )]
    EffortNotSent {
        /// The stage.
        stage: &'static str,
        /// The provider.
        provider: String,
    },
    /// No chat model at all: no file and no `ANTHROPIC_API_KEY`.
    #[error(
        "no model configured: set {ANTHROPIC_KEY_ENV}, or write a {DEFAULT_PATH} (or point {CONFIG_ENV} at one)"
    )]
    NoChatModel,
    /// `VOYAGE_DIMENSIONS` in the environment setup is not an integer in
    /// `1..=`[`MAX_DIMENSIONS`].
    #[error(
        "VOYAGE_DIMENSIONS must be an integer in 1..={MAX_DIMENSIONS} (pgvector's HNSW index limit), got {value:?}"
    )]
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
        /// The embeddings knob, carried for the report (a provider table
        /// serves chat and embeddings alike).
        send_dimensions: bool,
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
            ChatProvider::Anthropic {
                endpoint: Endpoint::Direct { base_url, .. },
            } => {
                serde_json::json!({"kind": "anthropic", "endpoint": "direct", "base_url": base_url})
            }
            ChatProvider::Anthropic {
                endpoint:
                    Endpoint::Proxy {
                        base_url, header, ..
                    },
            } => {
                let auth = match header {
                    ProxyAuth::XApiKey => "x-api-key",
                    ProxyAuth::Bearer => "bearer",
                };
                serde_json::json!({"kind": "anthropic", "endpoint": "proxy", "base_url": base_url, "auth": auth})
            }
            #[cfg(feature = "aws")]
            ChatProvider::Anthropic {
                endpoint:
                    Endpoint::ClaudePlatformOnAws {
                        base_url,
                        region,
                        workspace_id,
                        ..
                    },
            } => {
                serde_json::json!({"kind": "anthropic", "endpoint": "claude-platform-on-aws", "region": region, "workspace_id": workspace_id, "base_url": base_url})
            }
            #[cfg(feature = "aws")]
            ChatProvider::Anthropic {
                endpoint:
                    Endpoint::Bedrock {
                        base_url, region, ..
                    },
            } => {
                serde_json::json!({"kind": "anthropic", "endpoint": "bedrock", "region": region, "base_url": base_url})
            }
            #[cfg(feature = "gcp")]
            ChatProvider::Anthropic {
                endpoint:
                    Endpoint::Vertex {
                        base_url,
                        project,
                        region,
                        ..
                    },
            } => {
                serde_json::json!({"kind": "anthropic", "endpoint": "vertex", "project": project, "region": region, "base_url": base_url})
            }
            ChatProvider::OpenAi {
                base_url,
                auth,
                dialect,
                send_dimensions,
            } => serde_json::json!({
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
                "send_dimensions": send_dimensions,
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
            ChatProvider::Anthropic { endpoint } => {
                ChatBackend::Anthropic(Anthropic::new(endpoint.clone())?.with_model(&self.model))
            }
            ChatProvider::OpenAi {
                base_url,
                auth,
                dialect,
                ..
            } => ChatBackend::OpenAi(OpenAi::new(base_url, auth.clone(), &self.model, *dialect)?),
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

/// The embedder, resolved: Voyage or an OpenAI-compatible server.
#[derive(Clone, Debug)]
pub struct Embed {
    /// The `[providers]` key.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Vector width.
    pub dimensions: usize,
    backend: EmbedProvider,
}

/// An embeddings provider, resolved: the door and its credential.
#[derive(Clone, Debug)]
enum EmbedProvider {
    Voyage {
        api_key: ApiKey,
    },
    OpenAi {
        base_url: String,
        auth: Auth,
        send_dimensions: bool,
    },
}

impl Embed {
    /// The space the embedder writes into: the provider *kind* (not the
    /// operator's name for it), the model and the width.
    #[must_use]
    pub fn space(&self) -> Space {
        let provider = match self.backend {
            EmbedProvider::Voyage { .. } => Provider::Voyage,
            EmbedProvider::OpenAi { .. } => Provider::OpenAi,
        };
        Space {
            provider,
            model: self.model.clone(),
            dimensions: self.dimensions,
        }
    }

    /// `provider/model`, as the summary line writes it.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }

    /// Build the embedder.
    fn embedder(&self) -> Result<Arc<dyn WithSpace>, ConfigError> {
        Ok(match &self.backend {
            EmbedProvider::Voyage { api_key } => Arc::new(
                VoyageEmbedder::new(api_key.expose(), &self.model, self.dimensions)
                    .map_err(ConfigError::Embedder)?,
            ),
            EmbedProvider::OpenAi {
                base_url,
                auth,
                send_dimensions,
            } => {
                let auth = match auth {
                    Auth::None => judge_embed::Auth::None,
                    Auth::Bearer(k) => judge_embed::Auth::Bearer(k.expose().to_owned()),
                    Auth::ApiKeyHeader(k) => judge_embed::Auth::ApiKeyHeader(k.expose().to_owned()),
                };
                Arc::new(
                    OpenAiEmbedder::new(
                        base_url,
                        auth,
                        &self.model,
                        self.dimensions,
                        *send_dimensions,
                    )
                    .map_err(ConfigError::Embedder)?,
                )
            }
        })
    }

    /// A description for the report: the door, never the key.
    fn describe(&self) -> serde_json::Value {
        match &self.backend {
            EmbedProvider::Voyage { .. } => serde_json::json!({"kind": "voyage"}),
            EmbedProvider::OpenAi {
                base_url,
                auth,
                send_dimensions,
            } => serde_json::json!({
                "kind": "openai",
                "base_url": base_url,
                "auth": match auth { Auth::None => "none", Auth::Bearer(_) => "bearer", Auth::ApiKeyHeader(_) => "api-key" },
                "send_dimensions": send_dimensions,
            }),
        }
    }
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
    /// The source offer every remote interface makes: `JUDGE_SOURCE_URL`
    /// (else the upstream repository) at the commit the binary was built from.
    offer: SourceOffer,
    /// Who runs this instance: `JUDGE_OPERATOR_DISCORD` and
    /// `JUDGE_OPERATOR_EMAIL`, each validated when set. Which one a process
    /// *needs* is its surface's business ([`Config::discord_operator`],
    /// [`Config::network_operator`]).
    operator: Operator,
    /// What the cap covers and where a tripped cap is reported
    /// (`JUDGE_BUDGET_PERIOD`, `JUDGE_ALERT_WEBHOOK`).
    budget: Budget,
}

/// The budget settings from `env`, blank meaning unset.
///
/// # Errors
/// [`ConfigError::BadBudget`] for a period that is not `process`, `day` or
/// `month`, or a webhook that is not an `https` URL. The webhook's value is
/// not echoed: it is a credential.
pub fn budget(env: impl Fn(&str) -> Option<String>) -> Result<Budget, ConfigError> {
    let period = Period::parse(env(budget::PERIOD_ENV).as_deref()).map_err(|value| {
        ConfigError::BadBudget {
            var: budget::PERIOD_ENV,
            value,
            expected: "process, day or month",
        }
    })?;
    let alert = env(budget::ALERT_WEBHOOK_ENV)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .map(|v| {
            AlertWebhook::parse(&v).map_err(|_| ConfigError::BadBudget {
                var: budget::ALERT_WEBHOOK_ENV,
                value: "<redacted>".to_owned(),
                expected: "an https:// webhook URL",
            })
        })
        .transpose()?;
    Ok(Budget { period, alert })
}

/// The commit stamped into this binary by `build.rs`.
#[must_use]
pub fn build_commit() -> Commit {
    match BUILD_COMMIT.and_then(|c| CommitHash::try_new(c).ok()) {
        Some(hash) => Commit::Known {
            hash,
            dirty: BUILD_DIRTY == Some("1"),
        },
        None => Commit::Unknown,
    }
}

/// The offer for this process: `JUDGE_SOURCE_URL` from `env` (blank means
/// the upstream repository) at [`build_commit`].
///
/// # Errors
/// [`ConfigError::BadSourceUrl`] when the variable is set to something that
/// is not an http(s) URL: a typo here would make every interface point users
/// at nothing, so it is refused at startup.
pub fn source_offer(env: impl Fn(&str) -> Option<String>) -> Result<SourceOffer, ConfigError> {
    let repository = match env(SOURCE_URL_ENV)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
    {
        Some(value) => {
            RepositoryUrl::try_new(&value).map_err(|_| ConfigError::BadSourceUrl { value })?
        }
        None => return Ok(SourceOffer::upstream(build_commit())),
    };
    Ok(SourceOffer::new(repository, build_commit()))
}

/// Who runs this process: `JUDGE_OPERATOR_DISCORD` and `JUDGE_OPERATOR_EMAIL`
/// from `env`, blank meaning unset.
///
/// # Errors
/// [`ConfigError::BadContact`] when either is set to something that is not a
/// Discord username or an email address. A typo here would send users with a
/// problem to nobody, so it is refused at startup, whichever surface this is.
pub fn operator(env: impl Fn(&str) -> Option<String>) -> Result<Operator, ConfigError> {
    let set = |k: &str| {
        env(k)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    let discord = set(OPERATOR_DISCORD_ENV)
        .map(|value| {
            DiscordUsername::try_new(&value).map_err(|_| ConfigError::BadContact {
                var: OPERATOR_DISCORD_ENV,
                value,
                expected: "a Discord username (2 to 32 of a-z, 0-9, '_' and '.'; not a display \
                           name or a name#1234 tag)",
            })
        })
        .transpose()?;
    let email = set(OPERATOR_EMAIL_ENV)
        .map(|value| {
            SupportEmail::try_new(&value).map_err(|_| ConfigError::BadContact {
                var: OPERATOR_EMAIL_ENV,
                value,
                expected: "an email address (name@host.tld, nothing else)",
            })
        })
        .transpose()?;
    Ok(Operator::new(discord, email))
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
        let env_path = std::env::var(CONFIG_ENV)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let explicit = path.map(Path::to_path_buf).or(env_path);
        let path = match explicit {
            Some(p) if p.is_file() => p,
            Some(p) => return Err(ConfigError::Missing { path: p }),
            None if Path::new(DEFAULT_PATH).is_file() => PathBuf::from(DEFAULT_PATH),
            None => return Self::from_env(),
        };
        let text = std::fs::read_to_string(&path).map_err(|cause| ConfigError::Read {
            path: path.clone(),
            cause,
        })?;
        Self::from_toml(&text, &path, |k| std::env::var(k).ok())
    }

    /// Parse and resolve `text` (the contents of `path`, named in errors),
    /// reading secrets with `env`.
    ///
    /// # Errors
    /// See [`ConfigError`].
    pub fn from_toml(
        text: &str,
        path: &Path,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let file: File = toml::from_str(text).map_err(|cause| ConfigError::Parse {
            path: path.to_path_buf(),
            cause,
        })?;
        let mut resolver = Resolver {
            file: &file,
            env: &env,
            chat: BTreeMap::new(),
        };
        let extract = resolver.stage("extract", &file.models.extract)?;
        let synth = resolver.stage("synth", &file.models.synth)?;
        let embed = file
            .models
            .embed
            .as_ref()
            .map(|e| resolver.embed(e))
            .transpose()?;
        let meter = SpendMeter::from_var(env(MAX_SPEND_ENV).as_deref())?;
        let offer = source_offer(&env)?;
        let operator = operator(&env)?;
        Ok(Self {
            source: Source::File(path.to_path_buf()),
            chat: Some(Chat { extract, synth }),
            embed,
            meter,
            offer,
            operator,
            budget: budget(&env)?,
        })
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
        let set = |k: &str| {
            env(k)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let chat = set(ANTHROPIC_KEY_ENV)
            .map(|key| {
                let endpoint = Endpoint::Direct {
                    base_url: set("ANTHROPIC_BASE_URL")
                        .unwrap_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.to_owned()),
                    api_key: key.into(),
                };
                let model = judge_anthropic::DEFAULT_MODEL;
                let stage = |stage, max_tokens, effort| {
                    Ok::<_, ConfigError>(Stage {
                        provider: ANTHROPIC_PROVIDER.to_owned(),
                        backend: ChatProvider::Anthropic {
                            endpoint: endpoint.clone(),
                        },
                        model: model.to_owned(),
                        max_tokens,
                        effort,
                        price: Price::Table(
                            pricing_for(judge_anthropic::BACKEND, model).ok_or_else(|| {
                                ConfigError::Unpriced {
                                    stage,
                                    provider: ANTHROPIC_PROVIDER.to_owned(),
                                    model: model.to_owned(),
                                }
                            })?,
                        ),
                    })
                };
                let (x, s) = (
                    crate::extract::ExtractConfig::default(),
                    judge_llm::SynthConfig::default(),
                );
                Ok::<_, ConfigError>(Chat {
                    extract: stage("extract", x.max_tokens, x.effort)?,
                    synth: stage("synth", s.max_tokens, s.effort)?,
                })
            })
            .transpose()?;
        let embed = set(VOYAGE_KEY_ENV)
            .map(|key| {
                let dimensions = match set("VOYAGE_DIMENSIONS") {
                    Some(s) => s
                        .parse::<usize>()
                        .ok()
                        .filter(|d| validate_dimensions(d).is_ok())
                        .ok_or(ConfigError::BadDimensions { value: s })?,
                    None => VOYAGE_DEFAULT_DIMENSIONS,
                };
                Ok::<_, ConfigError>(Embed {
                    provider: VOYAGE_PROVIDER.to_owned(),
                    model: set("VOYAGE_MODEL").unwrap_or_else(|| VOYAGE_DEFAULT_MODEL.to_owned()),
                    dimensions,
                    backend: EmbedProvider::Voyage {
                        api_key: key.into(),
                    },
                })
            })
            .transpose()?;
        Ok(Self {
            source: Source::Environment,
            chat,
            embed,
            meter: SpendMeter::from_var(set(MAX_SPEND_ENV).as_deref())?,
            offer: source_offer(&env)?,
            operator: operator(&env)?,
            budget: budget(&env)?,
        })
    }

    /// Where this came from.
    #[must_use]
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The source offer this process makes on every remote interface.
    #[must_use]
    pub fn source_offer(&self) -> &SourceOffer {
        &self.offer
    }

    /// What the spend cap covers and where a tripped cap is reported. The
    /// long-running services hand this to [`budget::start`].
    #[must_use]
    pub fn budget(&self) -> &Budget {
        &self.budget
    }

    /// Who runs this instance, as far as they said. Either contact may be
    /// absent: this is what a local process (`judge-cli`, `judge-mcp`) shows.
    #[must_use]
    pub fn operator(&self) -> &Operator {
        &self.operator
    }

    /// The operator as the Discord bot must know them.
    ///
    /// # Errors
    /// [`ConfigError::MissingContact`] without `JUDGE_OPERATOR_DISCORD`.
    pub fn discord_operator(&self) -> Result<DiscordOperator, ConfigError> {
        Ok(self.operator.clone().for_discord()?)
    }

    /// The operator as `judge-api` must know them, whichever doors it opens.
    ///
    /// # Errors
    /// [`ConfigError::MissingContact`] without `JUDGE_OPERATOR_EMAIL`.
    pub fn network_operator(&self) -> Result<NetworkOperator, ConfigError> {
        Ok(self.operator.clone().for_network()?)
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

    /// Resolve every cloud door's credentials once, without sending
    /// anything: the chains are lazy, so this is where a host with no AWS
    /// credentials or no ADC fails — at startup, naming the provider and the
    /// door, like a missing `api_key_env` fails at load. Logs each door
    /// resolved, so the startup log says which host a stage signs for (the
    /// summary line names the provider, not the door). Key doors and
    /// `openai` providers need nothing and log nothing; a provider shared by
    /// both stages is probed once.
    ///
    /// # Errors
    /// [`ConfigError::Credentials`] for the first door whose chain has none.
    pub async fn probe_auth(&self) -> Result<(), ConfigError> {
        let mut probed = std::collections::BTreeSet::new();
        for stage in [self.extract(), self.synth()].into_iter().flatten() {
            let ChatProvider::Anthropic { endpoint } = &stage.backend else {
                continue;
            };
            if !endpoint.lazy_credentials() || !probed.insert(stage.provider.as_str()) {
                continue;
            }
            endpoint
                .probe()
                .await
                .map_err(|cause| ConfigError::Credentials {
                    provider: stage.provider.clone(),
                    endpoint: endpoint.describe(),
                    cause,
                })?;
            tracing::info!(provider = %stage.provider, endpoint = %endpoint.describe(), "cloud credentials resolved");
        }
        Ok(())
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

    /// The embedder, if one is configured, with its space. `ingest embed`
    /// takes this; a binary that queries or writes vector columns takes
    /// [`Self::vectors`].
    ///
    /// # Errors
    /// [`ConfigError::Embedder`] when its HTTP client cannot be built.
    pub fn embedder(&self) -> Result<Option<Arc<dyn WithSpace>>, ConfigError> {
        self.embed.as_ref().map(Embed::embedder).transpose()
    }

    /// The embedder behind the stored-space check over `pool`, if one is
    /// configured: what the retriever, the library and the call store take.
    /// One `Arc` per process, so the check runs and logs once.
    ///
    /// # Errors
    /// As [`Self::embedder`].
    pub fn vectors(&self, pool: PgPool) -> Result<Option<Arc<Vectors>>, ConfigError> {
        Ok(self.embedder()?.map(|e| Arc::new(Vectors::new(pool, e))))
    }

    /// The one-line summary every binary logs at startup:
    /// `config=judge.toml extract=ollama/qwen3:8b synth=anthropic/claude-opus-5-5 embed=voyage/voyage-3.5 cap=$5.00`.
    /// What the cap covers is not in it: only `bot` and `api` run a budget
    /// period, and they log it themselves (`budget::start`).
    #[must_use]
    pub fn summary(&self) -> String {
        let stage = |s: Option<&Stage>| s.map_or_else(|| "none".to_owned(), Stage::label);
        let embed = self
            .embed
            .as_ref()
            .map_or_else(|| "none".to_owned(), Embed::label);
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
            providers
                .entry(s.provider.clone())
                .or_insert_with(|| s.backend.describe());
        }
        if let Some(e) = &self.embed {
            providers
                .entry(e.provider.clone())
                .or_insert_with(|| e.describe());
        }
        serde_json::json!({
            "source": self.source.to_string(),
            "source_offer": self.offer.about(&self.operator),
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
/// up, reads secrets, checks kinds and prices. A chat provider is resolved
/// once and remembered, so the stages that name it share one
/// [`ChatProvider`] — one [`Endpoint`], one credential chain.
struct Resolver<'a, E: Fn(&str) -> Option<String>> {
    file: &'a File,
    env: &'a E,
    chat: BTreeMap<ProviderName, (ChatProvider, bool)>,
}

impl<'a, E: Fn(&str) -> Option<String>> Resolver<'a, E> {
    fn secret(&self, provider: &str, var: &str) -> Result<ApiKey, ConfigError> {
        (self.env)(var)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .map(ApiKey::from)
            .ok_or_else(|| ConfigError::MissingEnv {
                provider: provider.to_owned(),
                var: var.to_owned(),
            })
    }

    /// How an `openai` provider's key travels: `auth` without `api_key_env`
    /// would be silently ignored, so it is an error.
    fn openai_auth(
        &self,
        provider: &str,
        api_key_env: Option<&EnvVar>,
        auth: Option<OpenAiHeader>,
    ) -> Result<Auth, ConfigError> {
        Ok(match (api_key_env, auth) {
            (None, None) => Auth::None,
            (None, Some(_)) => {
                return Err(ConfigError::Misplaced {
                    provider: provider.to_owned(),
                    key: "auth",
                    reason: "needs api_key_env (there is no key to send)",
                });
            }
            (Some(var), None | Some(OpenAiHeader::Bearer)) => {
                Auth::Bearer(self.secret(provider, var.as_ref())?)
            }
            (Some(var), Some(OpenAiHeader::ApiKey)) => {
                Auth::ApiKeyHeader(self.secret(provider, var.as_ref())?)
            }
        })
    }

    fn provider(
        &self,
        stage: &'static str,
        name: &ProviderName,
    ) -> Result<&'a ProviderEntry, ConfigError> {
        self.file
            .providers
            .get(name)
            .ok_or_else(|| ConfigError::UnknownProvider {
                stage,
                provider: name.to_string(),
            })
    }

    fn stage(&mut self, stage: &'static str, entry: &StageEntry) -> Result<Stage, ConfigError> {
        let name = &entry.provider;
        let provider = self.provider(stage, name)?;
        let (backend, free) = self.chat_provider(stage, name, provider)?;
        // The checks that pair a stage's knobs with its provider's.
        match provider {
            // A bare id on Bedrock would 400 on every question; the documented
            // forms are `anthropic.<model>` and `<profile>.anthropic.<model>`.
            ProviderEntry::Anthropic {
                endpoint: Door::Bedrock,
                ..
            } if !bedrock_model_id(entry.model.as_ref()) => {
                return Err(ConfigError::BedrockModelId {
                    stage,
                    model: entry.model.to_string(),
                });
            }
            ProviderEntry::Openai {
                reasoning_effort: false,
                ..
            } if entry.effort.is_some() => {
                return Err(ConfigError::EffortNotSent {
                    stage,
                    provider: name.to_string(),
                });
            }
            ProviderEntry::Anthropic { .. }
            | ProviderEntry::Openai { .. }
            | ProviderEntry::Voyage { .. } => {}
        }
        let model = entry.model.to_string();
        // A free provider and a stage price contradict each other; an
        // explicit price beats the table (and is settled at, not merely
        // reserved at); the table errs high for Anthropic and knows nothing
        // else.
        let price = match (free, &entry.pricing) {
            (true, Some(_)) => {
                return Err(ConfigError::PricedFree {
                    stage,
                    provider: name.to_string(),
                });
            }
            (true, None) => Price::Free,
            (false, Some(p)) => Price::PerToken(p.pricing()),
            (false, None) => {
                Price::Table(pricing_for(backend.kind(), &model).ok_or_else(|| {
                    ConfigError::Unpriced {
                        stage,
                        provider: name.to_string(),
                        model: model.clone(),
                    }
                })?)
            }
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

    /// `provider` (the `[providers.<name>]` table) as a chat provider and
    /// whether it is free — resolved once per name: the second stage on the
    /// same provider gets a clone that shares the first's [`Endpoint`].
    fn chat_provider(
        &mut self,
        stage: &'static str,
        name: &ProviderName,
        provider: &ProviderEntry,
    ) -> Result<(ChatProvider, bool), ConfigError> {
        if let Some(resolved) = self.chat.get(name) {
            return Ok(resolved.clone());
        }
        let resolved = match provider {
            ProviderEntry::Anthropic {
                endpoint,
                base_url,
                api_key_env,
                auth,
                region,
                workspace_id,
                project,
                pricing,
            } => {
                let door = *endpoint;
                let misplaced = |key: &'static str, reason: &'static str| ConfigError::Misplaced {
                    provider: name.to_string(),
                    key,
                    reason,
                };
                // Each key belongs to some doors only; on another it would be
                // silently ignored, so it is an error there.
                if auth.is_some() && door != Door::Proxy {
                    return Err(misplaced("auth", "applies only to endpoint = \"proxy\""));
                }
                if api_key_env.is_some() && !door.takes_key() {
                    return Err(misplaced(
                        "api_key_env",
                        "does not apply to a cloud door: this build signs with the platform's credential chain (SigV4, ADC); API-key auth for the cloud doors is not supported",
                    ));
                }
                if region.is_some() && door.takes_key() {
                    return Err(misplaced(
                        "region",
                        "applies only to the cloud doors (claude-platform-on-aws, bedrock, vertex)",
                    ));
                }
                if workspace_id.is_some() && door != Door::ClaudePlatformOnAws {
                    return Err(misplaced(
                        "workspace_id",
                        "applies only to endpoint = \"claude-platform-on-aws\"",
                    ));
                }
                if project.is_some() && door != Door::Vertex {
                    return Err(misplaced(
                        "project",
                        "applies only to endpoint = \"vertex\"",
                    ));
                }
                let key = |var: Option<&EnvVar>| {
                    self.secret(
                        name.as_ref(),
                        required(name.as_ref(), "api_key_env", door, var)?.as_ref(),
                    )
                };
                let endpoint = match door {
                    Door::Direct => Endpoint::Direct {
                        base_url: base_url.as_ref().map_or_else(
                            || ANTHROPIC_DEFAULT_BASE_URL.to_owned(),
                            ToString::to_string,
                        ),
                        api_key: key(api_key_env.as_ref())?,
                    },
                    Door::Proxy => {
                        let base_url =
                            required(name.as_ref(), "base_url", door, base_url.as_ref())?;
                        let header = match auth.unwrap_or(ProxyHeader::XApiKey) {
                            ProxyHeader::XApiKey => ProxyAuth::XApiKey,
                            ProxyHeader::Bearer => ProxyAuth::Bearer,
                        };
                        Endpoint::Proxy {
                            base_url: base_url.to_string(),
                            api_key: key(api_key_env.as_ref())?,
                            header,
                        }
                    }
                    Door::ClaudePlatformOnAws => claude_platform_on_aws(
                        name.as_ref(),
                        region.as_ref(),
                        workspace_id.as_ref(),
                        base_url.as_ref(),
                    )?,
                    Door::Bedrock => bedrock(name.as_ref(), region.as_ref(), base_url.as_ref())?,
                    Door::Vertex => vertex(
                        name.as_ref(),
                        project.as_ref(),
                        region.as_ref(),
                        base_url.as_ref(),
                    )?,
                };
                (ChatProvider::Anthropic { endpoint }, pricing.is_some())
            }
            ProviderEntry::Openai {
                base_url,
                api_key_env,
                auth,
                structured_output,
                strict_tools,
                reasoning_effort,
                max_tokens_param,
                cache_hints,
                send_dimensions,
                pricing,
            } => {
                let auth = self.openai_auth(name.as_ref(), api_key_env.as_ref(), *auth)?;
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
                (
                    ChatProvider::OpenAi {
                        base_url: base_url.to_string(),
                        auth,
                        dialect,
                        send_dimensions: *send_dimensions,
                    },
                    pricing.is_some(),
                )
            }
            ProviderEntry::Voyage { .. } => {
                return Err(ConfigError::WrongKind {
                    stage,
                    provider: name.to_string(),
                    kind: "voyage",
                    expected: "a chat provider: kind = anthropic or openai",
                });
            }
        };
        self.chat.insert(name.clone(), resolved.clone());
        Ok(resolved)
    }

    fn embed(&self, entry: &EmbedEntry) -> Result<Embed, ConfigError> {
        let name = entry
            .provider
            .as_ref()
            .map_or(VOYAGE_PROVIDER, AsRef::as_ref);
        // `voyage` is implied when the file has no table for it.
        let implied = ProviderEntry::Voyage { api_key_env: None };
        let provider = match self
            .file
            .providers
            .iter()
            .find(|(k, _)| k.as_ref() == name)
            .map(|(_, p)| p)
        {
            Some(p) => p,
            None if name == VOYAGE_PROVIDER => &implied,
            None => {
                return Err(ConfigError::UnknownProvider {
                    stage: "embed",
                    provider: name.to_owned(),
                });
            }
        };
        match provider {
            ProviderEntry::Voyage { api_key_env } => Ok(Embed {
                provider: name.to_owned(),
                model: entry.model.to_string(),
                dimensions: entry
                    .dimensions
                    .map_or(VOYAGE_DEFAULT_DIMENSIONS, Dimensions::into_inner),
                backend: EmbedProvider::Voyage {
                    api_key: self.secret(
                        name,
                        api_key_env.as_ref().map_or(VOYAGE_KEY_ENV, AsRef::as_ref),
                    )?,
                },
            }),
            ProviderEntry::Openai {
                base_url,
                api_key_env,
                auth,
                send_dimensions,
                ..
            } => {
                // The width is the columns' vector(N); nothing here can guess it for an arbitrary model.
                let Some(dimensions) = entry.dimensions else {
                    return Err(ConfigError::EmbedDimensions {
                        provider: name.to_owned(),
                    });
                };
                Ok(Embed {
                    provider: name.to_owned(),
                    model: entry.model.to_string(),
                    dimensions: dimensions.into_inner(),
                    backend: EmbedProvider::OpenAi {
                        base_url: base_url.to_string(),
                        auth: self.openai_auth(name, api_key_env.as_ref(), *auth)?,
                        send_dimensions: *send_dimensions,
                    },
                })
            }
            ProviderEntry::Anthropic { .. } => Err(ConfigError::WrongKind {
                stage: "embed",
                provider: name.to_owned(),
                kind: "anthropic",
                expected: "an embeddings provider: kind = voyage",
            }),
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
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
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
        let c = load(
            FULL,
            &[
                ("ANTHROPIC_API_KEY", "sk-ant"),
                ("LITELLM_KEY", "sk-lite"),
                ("VOYAGE_API_KEY", "pa-voy"),
            ],
        )?;
        assert_eq!(c.source(), &Source::File(PathBuf::from("judge.toml")));
        let extract = c.extract().ok_or("extract")?;
        assert_eq!(
            (
                extract.provider.as_str(),
                extract.model.as_str(),
                extract.max_tokens,
                extract.effort
            ),
            ("ollama", "qwen3:8b", 2000, Effort::Low)
        );
        assert_eq!(extract.price, Price::Free);
        let ChatProvider::OpenAi {
            base_url,
            auth,
            dialect,
            ..
        } = &extract.backend
        else {
            return Err("openai".into());
        };
        assert_eq!(base_url, "http://ollama:11434/v1");
        assert_eq!(auth, &Auth::None);
        assert_eq!(
            dialect,
            &Dialect {
                structured_output: StructuredOutputMode::JsonObject,
                ..Dialect::default()
            }
        );

        let synth = c.synth().ok_or("synth")?;
        assert_eq!(
            (
                synth.provider.as_str(),
                synth.model.as_str(),
                synth.max_tokens,
                synth.effort
            ),
            ("litellm", "gpt-5", 12000, Effort::High)
        );
        let Price::PerToken(rate) = synth.price else {
            return Err("priced".into());
        };
        assert_eq!(
            rate,
            Pricing {
                input: 1.25,
                output: 10.0,
                cache_read: 0.125,
                cache_write: 1.5625
            },
            "cache_write defaults to 1.25x the input price"
        );
        let ChatProvider::OpenAi { auth, dialect, .. } = &synth.backend else {
            return Err("openai".into());
        };
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
        assert_eq!(
            (
                embed.provider.as_str(),
                embed.model.as_str(),
                embed.dimensions
            ),
            ("voyage", "voyage-3.5", 1024)
        );
        assert_eq!(
            c.embedder()?.map(|e| e.space().clone()),
            Some(Space {
                provider: Provider::Voyage,
                model: "voyage-3.5".into(),
                dimensions: 1024
            })
        );

        let models = c.models()?;
        assert_eq!(
            models.extract().capabilities().structured_output,
            StructuredOutput::JsonMode
        );
        assert_eq!(models.synth().provider(), "openai");
        assert_eq!(models.synth().model(), "gpt-5");
        let deps = c.deps_config();
        assert_eq!(
            (
                deps.extract.max_tokens,
                deps.synth.max_tokens,
                deps.synth.effort
            ),
            (2000, 12000, Effort::High)
        );
        assert_eq!(
            c.summary(),
            format!(
                "config=judge.toml extract=ollama/qwen3:8b synth=litellm/gpt-5 embed=voyage/voyage-3.5 cap=${:.2}",
                c.meter().max_spend_usd()
            )
        );
        Ok(())
    }

    #[test]
    fn secrets_never_appear_in_debug_or_the_report() -> R {
        let c = load(
            FULL,
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-secret"),
                ("LITELLM_KEY", "sk-lite-secret"),
                ("VOYAGE_API_KEY", "pa-voy-secret"),
            ],
        )?;
        let dbg = format!("{c:?}");
        let report = c.report().to_string();
        for secret in ["sk-ant-secret", "sk-lite-secret", "pa-voy-secret"] {
            assert!(!dbg.contains(secret), "{dbg}");
            assert!(!report.contains(secret), "{report}");
        }
        assert!(dbg.contains("<redacted>"), "{dbg}");
        let r = c.report();
        assert_eq!(
            r.pointer("/providers/litellm/auth"),
            Some(&serde_json::json!("bearer"))
        );
        assert_eq!(
            r.pointer("/providers/litellm/structured_output"),
            Some(&serde_json::json!("json_object"))
        );
        assert_eq!(
            r.pointer("/models/extract/pricing"),
            Some(&serde_json::json!("free"))
        );
        assert_eq!(
            r.pointer("/models/synth/pricing/output"),
            Some(&serde_json::json!(10.0))
        );
        assert_eq!(
            r.pointer("/models/embed/dimensions"),
            Some(&serde_json::json!(1024))
        );
        assert_eq!(r.pointer("/source"), Some(&serde_json::json!("judge.toml")));
        Ok(())
    }

    const MINIMAL: &str = r#"
[providers.anthropic]
kind = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"
[models.extract]
provider = "anthropic"
model = "claude-opus-5-5"
[models.synth]
provider = "anthropic"
model = "claude-opus-5-5"
"#;

    /// A file that resolves with no environment at all: a free, keyless
    /// OpenAI-compatible server for both stages.
    const KEYLESS: &str = r#"
[providers.local]
kind = "openai"
base_url = "http://localhost:11434/v1"
structured_output = "json_object"
pricing = "free"
[models.extract]
provider = "local"
model = "qwen3:8b"
[models.synth]
provider = "local"
model = "qwen3:8b"
"#;

    #[test]
    fn a_minimal_file_matches_the_environment_setup() -> R {
        let file = load(MINIMAL, &[("ANTHROPIC_API_KEY", "k")])?;
        let env = Config::from_vars(|k| (k == "ANTHROPIC_API_KEY").then(|| "k".to_owned()))?;
        for c in [&file, &env] {
            let (x, s) = (c.extract().ok_or("x")?, c.synth().ok_or("s")?);
            assert_eq!((x.max_tokens, x.effort), (2000, Effort::Low));
            assert_eq!((s.max_tokens, s.effort), (16000, Effort::High));
            assert_eq!(s.model, "claude-opus-5-5");
            assert_eq!(
                s.price,
                Price::Table(pricing_for("anthropic", "claude-opus-5-5").ok_or("table")?)
            );
            let ChatProvider::Anthropic {
                endpoint: Endpoint::Direct { base_url, .. },
            } = &s.backend
            else {
                return Err("direct".into());
            };
            assert_eq!(base_url, ANTHROPIC_DEFAULT_BASE_URL);
            assert!(c.embed().is_none());
            assert!(c.models()?.synth().capabilities().refusal_fallbacks);
        }
        assert_eq!(env.source(), &Source::Environment);
        assert!(env.summary().starts_with("config=env extract=anthropic/claude-opus-5-5 synth=anthropic/claude-opus-5-5 embed=none cap=$"), "{}", env.summary());
        Ok(())
    }

    #[test]
    fn the_environment_setup_without_a_key_has_no_chat_model() -> R {
        let c = Config::from_vars(|k| match k {
            "VOYAGE_API_KEY" => Some("pa".to_owned()),
            "VOYAGE_MODEL" => Some("voyage-3-large".to_owned()),
            "VOYAGE_DIMENSIONS" => Some("2000".to_owned()),
            "ANTHROPIC_API_KEY" => Some("   ".to_owned()),
            _ => None,
        })?;
        assert!(c.extract().is_none());
        assert!(matches!(c.models(), Err(ConfigError::NoChatModel)));
        assert!(c.models_if_configured()?.is_none());
        let e = c.embed().ok_or("embed")?;
        assert_eq!((e.model.as_str(), e.dimensions), ("voyage-3-large", 2000));
        assert!(
            c.summary()
                .contains("extract=none synth=none embed=voyage/voyage-3-large"),
            "{}",
            c.summary()
        );
        // Zero and anything HNSW cannot index (2048: voyage-3-large's default) fail at load.
        for value in ["0", "2048", "x"] {
            let bad = Config::from_vars(|k| match k {
                "VOYAGE_API_KEY" => Some("pa".to_owned()),
                "VOYAGE_DIMENSIONS" => Some(value.to_owned()),
                _ => None,
            });
            assert!(
                matches!(bad, Err(ConfigError::BadDimensions { .. })),
                "{value}: {bad:?}"
            );
            let msg = bad.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(msg.contains("1..=2000") && msg.contains("HNSW"), "{msg}");
        }
        let base = Config::from_vars(|k| match k {
            "ANTHROPIC_API_KEY" => Some("k".to_owned()),
            "ANTHROPIC_BASE_URL" => Some("http://proxy:8080".to_owned()),
            _ => None,
        })?;
        let ChatProvider::Anthropic {
            endpoint: Endpoint::Direct { base_url, .. },
        } = &base.synth().ok_or("s")?.backend
        else {
            return Err("direct".into());
        };
        assert_eq!(base_url, "http://proxy:8080");
        Ok(())
    }

    #[test]
    fn each_surface_demands_its_own_contact_and_a_bad_one_fails_at_load() -> R {
        // Nothing set loads (a local process needs neither) and no surface starts.
        let nobody = Config::from_vars(|_| None)?;
        assert_eq!(nobody.operator(), &Operator::default());
        let msg = nobody
            .discord_operator()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(msg.contains("JUDGE_OPERATOR_DISCORD"), "{msg}");
        let msg = nobody
            .network_operator()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(msg.contains("JUDGE_OPERATOR_EMAIL"), "{msg}");
        // Blank is unset, not invalid.
        let blank = Config::from_vars(|k| match k {
            "JUDGE_OPERATOR_DISCORD" | "JUDGE_OPERATOR_EMAIL" => Some("  ".to_owned()),
            _ => None,
        })?;
        assert_eq!(blank.operator(), &Operator::default());

        // One contact starts its own surface and not the other.
        let discord_only = Config::from_vars(|k| match k {
            "JUDGE_OPERATOR_DISCORD" => Some("@SomeJudge".to_owned()),
            _ => None,
        })?;
        assert_eq!(
            discord_only.discord_operator()?.username().as_ref(),
            "somejudge"
        );
        assert!(matches!(
            discord_only.network_operator(),
            Err(ConfigError::MissingContact(MissingContact::Email))
        ));
        let both = Config::from_vars(|k| match k {
            "JUDGE_OPERATOR_DISCORD" => Some("somejudge".to_owned()),
            "JUDGE_OPERATOR_EMAIL" => Some("judge@example.org".to_owned()),
            _ => None,
        })?;
        assert_eq!(
            both.network_operator()?.email().as_ref(),
            "judge@example.org"
        );
        assert_eq!(
            both.report().pointer("/source_offer/operator_email"),
            Some(&serde_json::json!("judge@example.org"))
        );

        // A value that is set and wrong is refused whichever surface this is.
        for (var, value) in [
            ("JUDGE_OPERATOR_DISCORD", "Some Judge#1234"),
            ("JUDGE_OPERATOR_EMAIL", "judge at example dot org"),
        ] {
            let bad = Config::from_vars(|k| (k == var).then(|| value.to_owned()));
            let msg = bad.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(msg.contains(var) && msg.contains(value), "{msg}");
        }
        // The file branch reads the same variables.
        let filed = Config::from_toml(EXAMPLE, Path::new("judge.example.toml"), |k| match k {
            "JUDGE_OPERATOR_EMAIL" => Some("judge@example.org".to_owned()),
            "ANTHROPIC_API_KEY" | "VOYAGE_API_KEY" => Some("k".to_owned()),
            _ => None,
        })?;
        assert!(filed.network_operator().is_ok());
        Ok(())
    }

    #[test]
    fn the_budget_defaults_to_the_process_and_refuses_a_bad_value_without_echoing_the_webhook() -> R
    {
        let default = Config::from_vars(|_| None)?;
        assert_eq!(default.budget(), &Budget::default());
        let set = Config::from_vars(|k| match k {
            budget::PERIOD_ENV => Some("month".to_owned()),
            budget::ALERT_WEBHOOK_ENV => Some("https://discord.com/api/webhooks/1/tok".to_owned()),
            _ => None,
        })?;
        assert_eq!(set.budget().period, Period::Month);
        assert!(set.budget().alert.is_some());
        let bad = Config::from_vars(|k| (k == budget::PERIOD_ENV).then(|| "weekly".to_owned()));
        assert!(bad.is_err_and(|e| e.to_string().contains("JUDGE_BUDGET_PERIOD=\"weekly\"")));
        let bad = Config::from_vars(|k| {
            (k == budget::ALERT_WEBHOOK_ENV).then(|| "http://hooks.example/secret-token".to_owned())
        });
        assert!(bad.is_err_and(|e| {
            let m = e.to_string();
            m.contains("JUDGE_ALERT_WEBHOOK") && !m.contains("secret-token")
        }));
        Ok(())
    }

    #[test]
    fn the_source_offer_defaults_upstream_and_takes_an_override_or_refuses_it() -> R {
        let upstream = Config::from_vars(|_| None)?;
        assert_eq!(
            upstream.source_offer().repository().as_ref(),
            judge_core::source::DEFAULT_REPOSITORY
        );
        assert_eq!(upstream.source_offer().commit(), &build_commit());
        // Blank is unset (the .env template ships it blank).
        let blank = Config::from_vars(|k| (k == SOURCE_URL_ENV).then(|| "  ".to_owned()))?;
        assert_eq!(blank.source_offer(), upstream.source_offer());
        // The file path reads the same variable, so a judge.toml deployment
        // is not a different case.
        let forked = load(
            FULL,
            &[
                ("ANTHROPIC_API_KEY", "a"),
                ("LITELLM_KEY", "l"),
                ("VOYAGE_API_KEY", "v"),
                (SOURCE_URL_ENV, "https://codeberg.org/me/judge/"),
            ],
        )?;
        assert_eq!(
            forked.source_offer().repository().as_ref(),
            "https://codeberg.org/me/judge"
        );
        assert_eq!(
            forked.report().pointer("/source_offer/repository"),
            Some(&serde_json::json!("https://codeberg.org/me/judge"))
        );
        for value in ["codeberg.org/me/judge", "ftp://x/y", "https://a b"] {
            let bad = Config::from_vars(|k| (k == SOURCE_URL_ENV).then(|| value.to_owned()));
            assert!(
                matches!(bad, Err(ConfigError::BadSourceUrl { .. })),
                "{value}: {bad:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_typo_is_an_error_naming_the_key() {
        let typo = MINIMAL.replace("api_key_env", "api_key_evn");
        let err = load(&typo, &[("ANTHROPIC_API_KEY", "k")])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("api_key_evn"), "{err}");
        assert!(err.starts_with("judge.toml:"), "{err}");
        let typo = FULL.replace("strict_tools = false", "strict_tool = false");
        let err = load(&typo, &[])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("strict_tool"), "{err}");
        let typo = FULL.replace("[models.synth.pricing]", "[models.synth.pricng]");
        let err = load(&typo, &[])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("pricng"), "{err}");
        let bad_value = FULL.replace("pricing = \"free\"", "pricing = \"cheap\"");
        let err = load(&bad_value, &[])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("cheap") && err.contains("free"), "{err}");
        let bad_kind = MINIMAL.replace("kind = \"anthropic\"", "kind = \"antropic\"");
        let err = load(&bad_kind, &[])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("antropic"), "{err}");
    }

    #[test]
    fn validators_reject_bad_values() {
        for (from, to) in [
            (
                "model = \"claude-opus-5-5\"\n[models.synth]",
                "model = \"  \"\n[models.synth]",
            ),
            ("api_key_env = \"ANTHROPIC_API_KEY\"", "api_key_env = \"\""),
        ] {
            let text = MINIMAL.replacen(from, to, 1);
            assert!(text != MINIMAL, "replacement applied");
            let r = load(&text, &[("ANTHROPIC_API_KEY", "k")]);
            assert!(
                matches!(r, Err(ConfigError::Parse { .. })),
                "{from} -> {to}: {r:?}"
            );
        }
        for (from, to) in [
            ("input = 1.25", "input = -1.0"),
            ("input = 1.25", "input = inf"),
            ("input = 1.25", "input = nan"),
            ("dimensions = 1024", "dimensions = 0"),
            ("dimensions = 1024", "dimensions = 3072"),
            ("max_tokens = 12000", "max_tokens = 0"),
            (
                "base_url = \"http://ollama:11434/v1\"",
                "base_url = \"ollama:11434/v1\"",
            ),
            (
                "base_url = \"http://ollama:11434/v1\"",
                "base_url = \"ftp://ollama:11434/v1\"",
            ),
            (
                "base_url = \"http://ollama:11434/v1\"",
                "base_url = \"http://\"",
            ),
            ("base_url = \"http://ollama:11434/v1\"", "base_url = \"\""),
        ] {
            let text = FULL.replacen(from, to, 1);
            assert!(text != FULL, "replacement applied");
            let r = load(
                &text,
                &[
                    ("ANTHROPIC_API_KEY", "k"),
                    ("LITELLM_KEY", "k"),
                    ("VOYAGE_API_KEY", "k"),
                ],
            );
            assert!(
                matches!(r, Err(ConfigError::Parse { .. })),
                "{from} -> {to}: {r:?}"
            );
            let err = r.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(err.starts_with("judge.toml:"), "{err}");
            if from.starts_with("base_url") {
                assert!(
                    err.contains("base_url must be an absolute http(s) URL"),
                    "{err}"
                );
            }
            if to == "dimensions = 3072" {
                assert!(
                    err.contains("dimensions") && err.contains("1..=2000") && err.contains("HNSW"),
                    "{err}"
                );
            }
        }
        // A URL with a query (Azure's api-version) and a bare origin are both fine.
        for url in [
            "https://x.openai.azure.com/openai/deployments/d?api-version=2024-10-21",
            "http://localhost:11434",
            "http://127.0.0.1:1/v1/",
        ] {
            let text = FULL.replacen("http://ollama:11434/v1", url, 1);
            assert!(
                load(
                    &text,
                    &[
                        ("ANTHROPIC_API_KEY", "k"),
                        ("LITELLM_KEY", "k"),
                        ("VOYAGE_API_KEY", "k")
                    ]
                )
                .is_ok(),
                "{url}"
            );
        }
    }

    #[test]
    fn contradictory_keys_are_errors_naming_both() {
        let env = [
            ("ANTHROPIC_API_KEY", "k"),
            ("LITELLM_KEY", "k"),
            ("VOYAGE_API_KEY", "k"),
            ("AZURE_KEY", "k"),
        ];
        // A stage price on a free provider: which one did the operator mean?
        let priced_free = FULL.replace(
            "provider = \"litellm\"\nmodel = \"gpt-5\"\neffort = \"high\"",
            "provider = \"ollama\"\nmodel = \"gpt-5\"",
        );
        assert!(priced_free != FULL, "replacement applied");
        let err = load(&priced_free, &env)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "models.synth.pricing: providers.ollama is pricing = \"free\"; remove one or the other"
        );
        // `auth` with no key to send.
        let auth_no_key = FULL.replace(
            "pricing = \"free\"",
            "pricing = \"free\"\nauth = \"api-key\"",
        );
        let err = load(&auth_no_key, &env)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "providers.ollama: auth needs api_key_env (there is no key to send)"
        );
        // `effort` on an openai provider that will not send it (ollama has reasoning_effort = false).
        let effort_dropped = FULL.replace(
            "model = \"qwen3:8b\"",
            "model = \"qwen3:8b\"\neffort = \"high\"",
        );
        let err = load(&effort_dropped, &env)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.starts_with("models.extract.effort: providers.ollama has reasoning_effort = false"),
            "{err}"
        );
        // ...while litellm (reasoning_effort = true) takes it, and an anthropic provider always does.
        assert!(
            load(FULL, &env).is_ok_and(|c| c.synth().is_some_and(|s| s.effort == Effort::High))
        );
        let on_anthropic = MINIMAL.replace(
            "model = \"claude-opus-5-5\"\n",
            "model = \"claude-opus-5-5\"\neffort = \"max\"\n",
        );
        assert!(
            load(&on_anthropic, &env)
                .is_ok_and(|c| c.synth().is_some_and(|s| s.effort == Effort::Max))
        );
    }

    #[test]
    fn the_cap_is_read_through_the_injected_environment() -> R {
        let c = load(
            MINIMAL,
            &[("ANTHROPIC_API_KEY", "k"), ("JUDGE_MAX_USD", "1.25")],
        )?;
        assert!((c.meter().max_spend_usd() - 1.25).abs() < 1e-9);
        let c = Config::from_vars(|k| match k {
            "ANTHROPIC_API_KEY" => Some("k".to_owned()),
            "JUDGE_MAX_USD" => Some("0.5".to_owned()),
            _ => None,
        })?;
        assert!((c.meter().max_spend_usd() - 0.5).abs() < 1e-9);
        assert!(c.summary().ends_with("cap=$0.50"), "{}", c.summary());
        let none = Config::from_vars(|_| None)?;
        assert!(
            (none.meter().max_spend_usd() - judge_llm::DEFAULT_MAX_SPEND_USD).abs() < 1e-9,
            "hermetic: whatever the process has"
        );
        let bad = load(
            MINIMAL,
            &[("ANTHROPIC_API_KEY", "k"), ("JUDGE_MAX_USD", "$5")],
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(bad.starts_with("JUDGE_MAX_USD"), "{bad}");
        Ok(())
    }

    #[tokio::test]
    async fn an_operator_price_on_an_anthropic_provider_is_what_the_meter_bills() -> R {
        use judge_llm::{TextBlock, ToolChoice, Turn};
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        // A proxy routing a model the table would price as Opus 5 ($5/$25); the operator says $1/$5.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
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
        assert!(
            (c.meter().spent_usd() - 2.0).abs() < 1e-6,
            "billed at the operator's rate, not the table's $10: {}",
            c.meter().spent_usd()
        );
        Ok(())
    }

    #[test]
    fn a_missing_or_blank_secret_is_an_error_naming_the_variable() {
        let err = load(MINIMAL, &[])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "providers.anthropic: ANTHROPIC_API_KEY (api_key_env) is not set"
        );
        let err = load(MINIMAL, &[("ANTHROPIC_API_KEY", "  ")]);
        assert!(matches!(err, Err(ConfigError::MissingEnv { .. })));
        let err = load(FULL, &[("ANTHROPIC_API_KEY", "k"), ("VOYAGE_API_KEY", "k")])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "providers.litellm: LITELLM_KEY (api_key_env) is not set"
        );
        // The implied voyage provider reads VOYAGE_API_KEY.
        let err = load(FULL, &[("ANTHROPIC_API_KEY", "k"), ("LITELLM_KEY", "k")])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "providers.voyage: VOYAGE_API_KEY (api_key_env) is not set"
        );
    }

    #[test]
    fn an_unpriced_openai_model_is_refused_unless_the_provider_is_free() {
        let unpriced = FULL.replace(
            "[models.synth.pricing]\ninput = 1.25\noutput = 10.0\ncache_read = 0.125\n",
            "",
        );
        assert!(unpriced != FULL);
        let err = load(
            &unpriced,
            &[
                ("ANTHROPIC_API_KEY", "k"),
                ("LITELLM_KEY", "k"),
                ("VOYAGE_API_KEY", "k"),
            ],
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert_eq!(
            err,
            "models.synth: no price for litellm/gpt-5; add [models.synth.pricing] (input, output per million tokens) or pricing = \"free\" on [providers.litellm]"
        );
        // The same model on the free provider needs no price (ollama sends no effort, so that key goes too); on Anthropic the table errs high.
        let on_ollama = unpriced
            .replace("provider = \"litellm\"", "provider = \"ollama\"")
            .replace("effort = \"high\"\n", "");
        let c = load(
            &on_ollama,
            &[("ANTHROPIC_API_KEY", "k"), ("VOYAGE_API_KEY", "k")],
        )
        .ok();
        assert_eq!(
            c.as_ref().and_then(|c| c.synth()).map(|s| s.price),
            Some(Price::Free)
        );
        let on_anthropic = unpriced.replace(
            "provider = \"litellm\"\nmodel = \"gpt-5\"",
            "provider = \"anthropic\"\nmodel = \"claude-something-new\"",
        );
        let c = load(
            &on_anthropic,
            &[("ANTHROPIC_API_KEY", "k"), ("VOYAGE_API_KEY", "k")],
        )
        .ok();
        assert_eq!(
            c.as_ref().and_then(|c| c.synth()).map(|s| s.price),
            pricing_for("anthropic", "claude-opus-5").map(Price::Table)
        );
    }

    #[test]
    fn provider_lookups_and_kinds_are_checked() {
        let env = [
            ("ANTHROPIC_API_KEY", "k"),
            ("LITELLM_KEY", "k"),
            ("VOYAGE_API_KEY", "k"),
        ];
        let missing = FULL.replace("provider = \"ollama\"", "provider = \"olama\"");
        let err = load(&missing, &env)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "models.extract.provider = \"olama\" names no [providers.olama] table"
        );
        let wrong = FULL.replace("provider = \"ollama\"", "provider = \"voyage\"")
            + "\n[providers.voyage]\nkind = \"voyage\"\n";
        // A [providers.voyage] table with its own variable, and an embed entry naming no provider, both resolve.
        let own_var = FULL.replace("provider = \"voyage\"\n", "")
            + "\n[providers.voyage]\nkind = \"voyage\"\napi_key_env = \"MY_VOYAGE\"\n";
        let err = load(&own_var, &env)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(err, "providers.voyage: MY_VOYAGE (api_key_env) is not set");
        let c = load(
            &own_var,
            &[
                ("ANTHROPIC_API_KEY", "k"),
                ("LITELLM_KEY", "k"),
                ("MY_VOYAGE", "k"),
            ],
        )
        .ok();
        assert_eq!(
            c.as_ref()
                .and_then(Config::embed)
                .map(|e| e.provider.as_str()),
            Some("voyage")
        );
        let err = load(&wrong, &env)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.starts_with(
                "models.extract: providers.voyage is kind = \"voyage\", which cannot serve extract"
            ),
            "{err}"
        );
        let embed_on_chat = FULL.replace(
            "provider = \"voyage\"\nmodel = \"voyage-3.5\"",
            "provider = \"anthropic\"\nmodel = \"voyage-3.5\"",
        );
        let err = load(&embed_on_chat, &env)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.starts_with("models.embed: providers.anthropic is kind = \"anthropic\""),
            "{err}"
        );
        let embed_on_openai = FULL.replace(
            "provider = \"voyage\"\nmodel = \"voyage-3.5\"",
            "provider = \"ollama\"\nmodel = \"nomic-embed-text\"",
        );
        let c = load(&embed_on_openai, &env).ok();
        assert_eq!(
            c.as_ref().and_then(Config::embed).map(Embed::label),
            Some("ollama/nomic-embed-text".to_owned())
        );
        let unknown_embed = FULL.replace("provider = \"voyage\"", "provider = \"voyag\"");
        let err = load(&unknown_embed, &env)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "models.embed.provider = \"voyag\" names no [providers.voyag] table"
        );
    }

    #[test]
    fn embeddings_on_an_openai_provider_need_a_width_and_carry_the_wire_knob() -> R {
        let env = [
            ("ANTHROPIC_API_KEY", "k"),
            ("LITELLM_KEY", "sk-lite-secret"),
        ];
        let on_litellm = FULL.replace(
            "provider = \"voyage\"\nmodel = \"voyage-3.5\"\ndimensions = 1024",
            "provider = \"litellm\"\nmodel = \"text-embedding-3-small\"\ndimensions = 1536",
        );
        assert!(on_litellm != FULL);
        let c = load(&on_litellm, &env)?;
        let e = c.embed().ok_or("embed")?;
        assert_eq!(
            e.space(),
            Space {
                provider: Provider::OpenAi,
                model: "text-embedding-3-small".into(),
                dimensions: 1536
            }
        );
        let embedder = c.embedder()?.ok_or("embedder")?;
        assert_eq!(
            (embedder.space(), embedder.dimensions()),
            (&e.space(), 1536)
        );
        assert!(
            c.summary().contains("embed=litellm/text-embedding-3-small"),
            "{}",
            c.summary()
        );
        // The key never reaches Debug or the report; the report says how it travels and whether `dimensions` is sent.
        let (dbg, report) = (format!("{c:?}"), c.report());
        assert!(
            !dbg.contains("sk-lite-secret") && !report.to_string().contains("sk-lite-secret"),
            "{dbg}"
        );
        assert_eq!(
            report.pointer("/providers/litellm/send_dimensions"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            report.pointer("/models/embed/dimensions"),
            Some(&serde_json::json!(1536))
        );

        // An openai provider used only for embeddings is described too, with `send_dimensions = false` honoured.
        let only_embed = on_litellm.replace(
            "kind = \"openai\"\nbase_url = \"http://litellm:4000/v1\"",
            "kind = \"openai\"\nsend_dimensions = false\nbase_url = \"http://litellm:4000/v1\"",
        ) + "\n[providers.vllm]\nkind = \"openai\"\nbase_url = \"http://vllm:8000/v1\"\nsend_dimensions = false\npricing = \"free\"\n";
        let only_embed = only_embed.replace(
            "provider = \"litellm\"\nmodel = \"text-embedding-3-small\"",
            "provider = \"vllm\"\nmodel = \"bge-m3\"",
        );
        let c = load(&only_embed, &env)?;
        assert_eq!(
            c.report().pointer("/providers/vllm"),
            Some(
                &serde_json::json!({"kind": "openai", "base_url": "http://vllm:8000/v1", "auth": "none", "send_dimensions": false})
            )
        );
        assert_eq!(
            c.embed().map(Embed::space),
            Some(Space {
                provider: Provider::OpenAi,
                model: "bge-m3".into(),
                dimensions: 1536
            })
        );

        // No width: an error naming the key (voyage has a default, an arbitrary model does not).
        let no_width = on_litellm.replace("dimensions = 1536\n", "");
        let err = load(&no_width, &env)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.starts_with(
                "models.embed.dimensions is required on providers.litellm (kind = \"openai\")"
            ),
            "{err}"
        );
        Ok(())
    }

    #[test]
    fn anthropic_doors() -> R {
        let proxy = MINIMAL.replace(
            "kind = \"anthropic\"\n",
            "kind = \"anthropic\"\nendpoint = \"proxy\"\nbase_url = \"http://litellm:4000/\"\nauth = \"bearer\"\n",
        );
        let c = load(&proxy, &[("ANTHROPIC_API_KEY", "k")])?;
        let ChatProvider::Anthropic {
            endpoint: Endpoint::Proxy {
                base_url, header, ..
            },
        } = &c.synth().ok_or("s")?.backend
        else {
            return Err("proxy".into());
        };
        assert_eq!(
            (base_url.as_str(), *header),
            ("http://litellm:4000/", ProxyAuth::Bearer)
        );
        assert!(!c.models()?.synth().capabilities().refusal_fallbacks);
        assert_eq!(
            c.report().pointer("/providers/anthropic/endpoint"),
            Some(&serde_json::json!("proxy"))
        );

        let no_url = MINIMAL.replace(
            "kind = \"anthropic\"\n",
            "kind = \"anthropic\"\nendpoint = \"proxy\"\n",
        );
        let err = load(&no_url, &[("ANTHROPIC_API_KEY", "k")])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "providers.anthropic: base_url is required for endpoint = \"proxy\""
        );
        let auth_on_direct = MINIMAL.replace(
            "kind = \"anthropic\"\n",
            "kind = \"anthropic\"\nauth = \"bearer\"\n",
        );
        let err = load(&auth_on_direct, &[("ANTHROPIC_API_KEY", "k")])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "providers.anthropic: auth applies only to endpoint = \"proxy\""
        );
        let no_key = MINIMAL.replace("api_key_env = \"ANTHROPIC_API_KEY\"\n", "");
        let err = load(&no_key, &[])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "providers.anthropic: api_key_env is required for endpoint = \"direct\""
        );
        let unknown = MINIMAL.replace(
            "kind = \"anthropic\"\n",
            "kind = \"anthropic\"\nendpoint = \"sideways\"\n",
        );
        let err = load(&unknown, &[("ANTHROPIC_API_KEY", "k")])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("sideways") && err.contains("bedrock"), "{err}");
        Ok(())
    }

    /// `MINIMAL` with the anthropic provider's keys replaced by `extra`
    /// (no `api_key_env` unless `extra` has one).
    fn anthropic_with(extra: &str) -> String {
        MINIMAL.replace(
            "kind = \"anthropic\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n",
            &format!("kind = \"anthropic\"\n{extra}\n"),
        )
    }

    #[cfg(feature = "aws")]
    #[test]
    fn aws_doors_resolve_from_their_keys() -> R {
        use judge_llm::StructuredOutput;
        let c = load(
            &anthropic_with(
                "endpoint = \"claude-platform-on-aws\"\nregion = \"us-west-2\"\nworkspace_id = \" wrkspc_01AbC \"",
            ),
            &[],
        )?;
        let ChatProvider::Anthropic {
            endpoint:
                Endpoint::ClaudePlatformOnAws {
                    base_url,
                    region,
                    workspace_id,
                    ..
                },
        } = &c.synth().ok_or("s")?.backend
        else {
            return Err("claude-platform-on-aws".into());
        };
        assert_eq!(
            (base_url.as_str(), region.as_str(), workspace_id.as_str()),
            (
                "https://aws-external-anthropic.us-west-2.api.aws",
                "us-west-2",
                "wrkspc_01AbC"
            )
        );
        assert!(
            c.models()?.synth().capabilities().refusal_fallbacks,
            "first-party parity"
        );
        assert_eq!(
            c.report().pointer("/providers/anthropic/endpoint"),
            Some(&serde_json::json!("claude-platform-on-aws"))
        );
        assert_eq!(
            c.report().pointer("/providers/anthropic/workspace_id"),
            Some(&serde_json::json!("wrkspc_01AbC"))
        );

        let bedrock = anthropic_with("endpoint = \"bedrock\"\nregion = \"us-east-1\"\nbase_url = \"http://bedrock-proxy.internal/\"")
            .replace("model = \"claude-opus-5-5\"", "model = \"anthropic.claude-opus-5-5\"");
        let c = load(&bedrock, &[])?;
        let ChatProvider::Anthropic {
            endpoint:
                Endpoint::Bedrock {
                    base_url,
                    region,
                    credentials,
                    ..
                },
        } = &c.synth().ok_or("s")?.backend
        else {
            return Err("bedrock".into());
        };
        assert_eq!(
            (base_url.as_str(), region.as_str()),
            ("http://bedrock-proxy.internal/", "us-east-1"),
            "base_url overrides the derived origin"
        );
        // Both stages name the one provider: one endpoint, one credential chain between them.
        let ChatProvider::Anthropic {
            endpoint:
                Endpoint::Bedrock {
                    credentials: extract_credentials,
                    ..
                },
        } = &c.extract().ok_or("x")?.backend
        else {
            return Err("bedrock".into());
        };
        assert!(
            Arc::ptr_eq(credentials, extract_credentials),
            "the stages share the provider's chain"
        );
        let caps = c.models()?.synth().capabilities();
        assert_eq!(caps.structured_output, StructuredOutput::PromptOnly);
        assert!(!caps.strict_tools && !caps.refusal_fallbacks);
        assert_eq!(c.synth().ok_or("s")?.model, "anthropic.claude-opus-5-5");
        assert!(
            matches!(c.synth().ok_or("s")?.price, Price::Table(_)),
            "an unknown Anthropic id prices as Opus 5"
        );
        assert_eq!(
            c.report().pointer("/providers/anthropic/endpoint"),
            Some(&serde_json::json!("bedrock"))
        );
        // An inference profile is the other documented form.
        let profile = bedrock.replace(
            "anthropic.claude-opus-5-5",
            "global.anthropic.claude-opus-5-5",
        );
        assert_eq!(
            load(&profile, &[])?.synth().map(|s| s.model.clone()),
            Some("global.anthropic.claude-opus-5-5".to_owned())
        );
        // A bare id, or `anthropic.` anywhere but as a whole segment, is refused at load naming the stage.
        for model in ["claude-opus-5-5", "claude-opus-5-anthropic.x"] {
            let bare = bedrock.replace("anthropic.claude-opus-5-5", model);
            let err = load(&bare, &[])
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(
                err.starts_with(&format!("models.extract.model = {model:?}"))
                    && err.contains("anthropic."),
                "{model}: {err}"
            );
        }
        Ok(())
    }

    #[cfg(feature = "gcp")]
    #[test]
    fn vertex_resolves_from_its_keys() -> R {
        use judge_llm::StructuredOutput;
        let c = load(
            &anthropic_with("endpoint = \"vertex\"\nproject = \"my-proj\"\nregion = \"global\""),
            &[],
        )?;
        let ChatProvider::Anthropic {
            endpoint:
                Endpoint::Vertex {
                    base_url,
                    project,
                    region,
                    token,
                    ..
                },
        } = &c.synth().ok_or("s")?.backend
        else {
            return Err("vertex".into());
        };
        assert_eq!(
            (base_url.as_str(), project.as_str(), region.as_str()),
            ("https://aiplatform.googleapis.com", "my-proj", "global")
        );
        let ChatProvider::Anthropic {
            endpoint:
                Endpoint::Vertex {
                    token: extract_token,
                    ..
                },
        } = &c.extract().ok_or("x")?.backend
        else {
            return Err("vertex".into());
        };
        assert!(
            Arc::ptr_eq(token, extract_token),
            "the stages share the provider's ADC"
        );
        let caps = c.models()?.synth().capabilities();
        assert!(!caps.refusal_fallbacks && caps.strict_tools);
        assert_eq!(caps.structured_output, StructuredOutput::Enforced);
        assert_eq!(
            c.report().pointer("/providers/anthropic/project"),
            Some(&serde_json::json!("my-proj"))
        );
        Ok(())
    }

    /// `region` and `project` go into a hostname or a URL path: a value
    /// that would not survive that fails at load naming the key, on every
    /// build (the check is in the type, before any door is resolved).
    #[test]
    fn region_and_project_are_checked_as_labels() {
        let err = |extra: &str| {
            load(&anthropic_with(extra), &[])
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };
        for (extra, key) in [
            ("endpoint = \"bedrock\"\nregion = \"us-east-1/\"", "region"),
            ("endpoint = \"bedrock\"\nregion = \"US-EAST-1\"", "region"),
            ("endpoint = \"bedrock\"\nregion = \"-us\"", "region"),
            (
                "endpoint = \"vertex\"\nregion = \"global\"\nproject = \"My Proj\"",
                "project",
            ),
            (
                "endpoint = \"vertex\"\nregion = \"global\"\nproject = \"example.com:proj\"",
                "project",
            ),
        ] {
            let e = err(extra);
            assert!(
                e.contains(key) && e.contains("lowercase letters, digits and hyphens"),
                "{extra:?}: {e}"
            );
        }
        assert!(
            validate_region("us-east5").is_ok()
                && validate_region("global").is_ok()
                && validate_project("proj-123456").is_ok()
        );
    }

    /// The key doors hold their key: probing them is `Ok` without I/O, so
    /// the zero-config setup and a proxy start as they always did.
    #[tokio::test]
    async fn probe_auth_needs_nothing_from_a_key_door() -> R {
        let c = Config::from_vars(|k| (k == "ANTHROPIC_API_KEY").then(|| "k".to_owned()))?;
        c.probe_auth().await?;
        let c = load(
            FULL,
            &[
                ("ANTHROPIC_API_KEY", "k"),
                ("LITELLM_KEY", "k"),
                ("VOYAGE_API_KEY", "k"),
            ],
        )?;
        c.probe_auth().await?;
        Ok(())
    }

    #[test]
    fn cloud_door_keys_are_checked_per_door() {
        let err = |extra: &str| {
            load(&anthropic_with(extra), &[("ANTHROPIC_API_KEY", "k")])
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };
        // Misplaced keys are errors on every build; missing ones on a built door.
        for (extra, expect) in [
            (
                "endpoint = \"vertex\"\nregion = \"global\"\nproject = \"p\"\napi_key_env = \"ANTHROPIC_API_KEY\"",
                "providers.anthropic: api_key_env does not apply to a cloud door",
            ),
            (
                "api_key_env = \"ANTHROPIC_API_KEY\"\nregion = \"us-west-2\"",
                "providers.anthropic: region applies only to the cloud doors",
            ),
            (
                "api_key_env = \"ANTHROPIC_API_KEY\"\nworkspace_id = \"wrkspc_01X\"",
                "providers.anthropic: workspace_id applies only to endpoint = \"claude-platform-on-aws\"",
            ),
            (
                "endpoint = \"bedrock\"\nregion = \"us-east-1\"\nproject = \"p\"",
                "providers.anthropic: project applies only to endpoint = \"vertex\"",
            ),
            (
                "endpoint = \"bedrock\"\nregion = \"us-east-1\"\nauth = \"bearer\"",
                "providers.anthropic: auth applies only to endpoint = \"proxy\"",
            ),
        ] {
            let e = err(extra);
            assert!(e.starts_with(expect), "{extra:?}: {e}");
        }
        let e = err(
            "endpoint = \"claude-platform-on-aws\"\nregion = \"us-west-2\"\nworkspace_id = \"arn:aws:aws-external-anthropic:us-west-2:1:workspace/wrkspc_01X\"",
        );
        assert!(e.contains("workspace_id") && e.contains("wrkspc_"), "{e}");
        // A missing key is an error on a built door; a door this build lacks
        // names its feature first, whatever else is missing. Each door is
        // checked against its own feature, so the `aws`-only and `gcp`-only
        // builds are exercised too, not just both-on and both-off.
        for (extra, door, feature, built, missing) in [
            (
                "endpoint = \"bedrock\"",
                "bedrock",
                "aws",
                cfg!(feature = "aws"),
                "region",
            ),
            (
                "endpoint = \"claude-platform-on-aws\"\nregion = \"us-west-2\"",
                "claude-platform-on-aws",
                "aws",
                cfg!(feature = "aws"),
                "workspace_id",
            ),
            (
                "endpoint = \"vertex\"\nregion = \"global\"",
                "vertex",
                "gcp",
                cfg!(feature = "gcp"),
                "project",
            ),
        ] {
            let expect = if built {
                format!("providers.anthropic: {missing} is required for endpoint = \"{door}\"")
            } else {
                format!(
                    "providers.anthropic: endpoint = \"{door}\" is not built in this binary; it needs the \"{feature}\" feature of judge-anthropic (on by default)"
                )
            };
            assert_eq!(err(extra), expect, "{extra:?}");
        }
    }

    #[test]
    fn openai_auth_header_and_azure() -> R {
        let azure = MINIMAL.replace(
            "[providers.anthropic]\nkind = \"anthropic\"\napi_key_env = \"ANTHROPIC_API_KEY\"",
            "[providers.anthropic]\nkind = \"openai\"\nbase_url = \"https://x.openai.azure.com/openai/deployments/d?api-version=2024-10-21\"\napi_key_env = \"AZURE_KEY\"\nauth = \"api-key\"\npricing = \"free\"",
        );
        let c = load(&azure, &[("AZURE_KEY", "az")])?;
        let ChatProvider::OpenAi { auth, .. } = &c.synth().ok_or("s")?.backend else {
            return Err("openai".into());
        };
        assert_eq!(auth, &Auth::ApiKeyHeader("az".into()));
        assert_eq!(
            c.report().pointer("/providers/anthropic/auth"),
            Some(&serde_json::json!("api-key"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_free_provider_never_trips_the_cap() -> R {
        use judge_llm::{TextBlock, ToolChoice, Turn};
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
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
        // A keyless local provider, so the test reads nothing from the real
        // environment and passes with or without ANTHROPIC_API_KEY set.
        std::fs::write(&good, KEYLESS)?;
        let c = Config::load_from(Some(&good))?;
        assert_eq!(c.source(), &Source::File(good.clone()));
        let missing = dir.join("nope.toml");
        let err = Config::load_from(Some(&missing))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.contains("nope.toml") && err.contains("not found"),
            "{err}"
        );
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, "[models\n")?;
        let err = Config::load_from(Some(&bad))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.starts_with(&format!("{}:", bad.display())), "{err}");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// The tracked example: what the README and `docs/DEPLOYMENT.md` tell
    /// every operator to copy. `deny_unknown_fields` means a renamed knob
    /// would break it without touching any other test, so it is pinned here.
    const EXAMPLE: &str = include_str!("../../../judge.example.toml");

    #[test]
    fn the_example_file_loads_as_shipped() -> R {
        // Only the two keys the header says it needs: the litellm table is
        // uncommented but no stage names it, so LITELLM_KEY is never read.
        let env = vars(&[("ANTHROPIC_API_KEY", "a"), ("VOYAGE_API_KEY", "v")]);
        let c = Config::from_toml(EXAMPLE, Path::new("judge.example.toml"), |k| {
            env.get(k).cloned()
        })?;
        assert_eq!(
            c.summary(),
            "config=judge.example.toml extract=ollama/qwen3:8b synth=anthropic/claude-opus-5-5 embed=voyage/voyage-3.5 cap=$5.00"
        );
        assert_eq!(
            c.report().pointer("/providers/anthropic/endpoint"),
            Some(&serde_json::json!("direct"))
        );
        assert_eq!(
            c.report().pointer("/models/embed/dimensions"),
            Some(&serde_json::json!(1024))
        );
        Ok(())
    }

    /// The example with every commented table uncommented. A block starts
    /// at a `# [` line and runs to the next blank line; inside it the
    /// leading `# ` is stripped (a bare `#` separator stays a comment, and
    /// so does the prose above each header). The one placeholder that cannot
    /// pass a validator, `wrkspc_...`, is swapped for a well-formed id.
    fn example_with_every_table_uncommented() -> String {
        let mut out = String::new();
        let mut in_block = false;
        for line in EXAMPLE.lines() {
            if line.is_empty() {
                in_block = false;
            } else if line.starts_with("# [") {
                in_block = true;
            }
            out.push_str(if in_block {
                line.strip_prefix("# ").unwrap_or(line)
            } else {
                line
            });
            out.push('\n');
        }
        out.replace("wrkspc_...", "wrkspc_01AbC")
    }

    #[test]
    fn the_example_file_loads_with_every_door_uncommented() -> R {
        let text = example_with_every_table_uncommented();
        assert!(
            text.contains("\n[providers.claude-proxy]\n")
                && text.contains("\n[providers.vertex]\n")
                && text.contains("\n[models.synth.pricing]\n"),
            "{text}"
        );
        assert!(
            !text.contains("\npricing = \"free\"\nkind = \"anthropic\""),
            "a knob commented inside a live table stays commented"
        );
        let env = [
            ("ANTHROPIC_API_KEY", "a"),
            ("VOYAGE_API_KEY", "v"),
            ("LITELLM_KEY", "l"),
        ];
        // Every table parses and resolves; the stages still point where the shipped file does.
        let c = load(&text, &env)?;
        assert!(
            c.summary().contains(
                "extract=ollama/qwen3:8b synth=anthropic/claude-opus-5-5 embed=voyage/voyage-3.5"
            ),
            "{}",
            c.summary()
        );
        assert_eq!(
            c.report().pointer("/providers/voyage/kind"),
            Some(&serde_json::json!("voyage")),
            "the explicit voyage table, not the implied one"
        );
        // And each commented door resolves when synth names it (the pricing
        // table makes gpt-5 on litellm priceable; `effort` must go, since the
        // example's litellm has reasoning_effort = false and the loader
        // refuses a knob the model would never see).
        assert!(
            load(
                &text.replace("provider = \"anthropic\"", "provider = \"litellm\""),
                &env
            )
            .is_err_and(|e| e.to_string().contains("reasoning_effort = false"))
        );
        let doors: &[(&str, &str, &str)] = &[
            ("claude-proxy", "claude-opus-5-5", "proxy"),
            ("litellm", "gpt-5", "openai"),
            #[cfg(feature = "aws")]
            ("claude-aws", "claude-opus-5-5", "claude-platform-on-aws"),
            #[cfg(feature = "aws")]
            ("bedrock", "anthropic.claude-opus-5-5", "bedrock"),
            #[cfg(feature = "gcp")]
            ("vertex", "claude-opus-5-5", "vertex"),
        ];
        for (provider, model, door) in doors {
            let synth = text
                .replace(
                    "provider = \"anthropic\"",
                    &format!("provider = \"{provider}\""),
                )
                .replace(
                    "model = \"claude-opus-5-5\"",
                    &format!("model = \"{model}\""),
                )
                .replace("\neffort = \"high\"", "\n# effort = \"high\"");
            let c = load(&synth, &env).map_err(|e| format!("{provider}: {e}"))?;
            assert_eq!(
                c.synth().map(Stage::label).as_deref(),
                Some(format!("{provider}/{model}").as_str())
            );
            let report = c.report();
            let seen = report
                .pointer(&format!("/providers/{provider}/endpoint"))
                .or_else(|| report.pointer(&format!("/providers/{provider}/kind")));
            assert_eq!(seen, Some(&serde_json::json!(door)), "{provider}: {report}");
        }
        Ok(())
    }
}
