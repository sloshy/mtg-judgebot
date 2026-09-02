//! The Anthropic Messages API as a `judge-llm` backend (no official Rust SDK
//! exists, so the wire types are ours).
//!
//! * [`wire`]    — serde types for `POST /v1/messages` and its response.
//! * [`schema`]  — schemars → Anthropic structured-output schema subset.
//! * [`convert`] — neutral [`judge_llm::ChatRequest`] ↔ Messages API.
//! * [`client`]  — [`Endpoint`] (which door, which auth) and [`Anthropic`],
//!   the [`judge_llm::Backend`] implementation (metered by `judge-llm`
//!   before the pipeline sees it).
//!
//! The spend cap ([`judge_llm::Metered`]), the retry loop
//! ([`judge_llm::http`]) and the one-tool-round typestate
//! ([`judge_llm::Synth`]) are provider-neutral and live in `judge-llm`.

pub mod client;
pub mod convert;
pub mod schema;
pub mod wire;

pub use client::{Anthropic, Endpoint, ProxyAuth};
pub use judge_llm::ApiKey;
pub use convert::BACKEND;
pub use schema::{AnthropicSubset, anthropic_schema, to_anthropic};

/// Default model. Opus 5 with adaptive thinking is the project baseline.
pub const DEFAULT_MODEL: &str = "claude-opus-5";
/// Messages API version header.
pub const API_VERSION: &str = "2023-06-01";
