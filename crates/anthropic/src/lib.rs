//! The Anthropic Messages API as a `judge-llm` backend (no official Rust SDK
//! exists, so the wire types are ours).
//!
//! * [`wire`]    — serde types for `POST /v1/messages` and its response.
//! * [`schema`]  — schemars → Anthropic structured-output schema subset.
//! * [`convert`] — neutral [`judge_llm::ChatRequest`] ↔ Messages API.
//! * [`client`]  — [`Endpoint`] (which door, which auth) and [`Anthropic`],
//!   the [`judge_llm::Backend`] implementation (metered by `judge-llm`
//!   before the pipeline sees it).
//! * [`aws`] (feature `aws`) — `SigV4` and the AWS credential chain for
//!   Claude Platform on AWS and Bedrock; [`gcp`] (feature `gcp`) — bearer
//!   tokens from Application Default Credentials for Vertex AI. Both
//!   features are on by default; a lean build turns them off and loses
//!   those [`Endpoint`] variants entirely.
//!
//! The spend cap ([`judge_llm::Metered`]), the retry loop
//! ([`judge_llm::http`]) and the one-tool-round typestate
//! ([`judge_llm::Synth`]) are provider-neutral and live in `judge-llm`.

#[cfg(feature = "aws")]
pub mod aws;
pub mod client;
pub mod convert;
#[cfg(feature = "gcp")]
pub mod gcp;
pub mod schema;
pub mod wire;

pub use client::{
    Anthropic, Endpoint, ProxyAuth, WORKSPACE_HEADER, bedrock_origin, claude_platform_on_aws_origin, vertex_origin,
};
pub use convert::BACKEND;
pub use judge_llm::ApiKey;
pub use schema::{AnthropicSubset, anthropic_schema, to_anthropic};
pub use wire::{ModelField, VERTEX_API_VERSION};

/// Default model. Opus 5 with adaptive thinking is the project baseline.
pub const DEFAULT_MODEL: &str = "claude-opus-5";
/// Messages API version header.
pub const API_VERSION: &str = "2023-06-01";
