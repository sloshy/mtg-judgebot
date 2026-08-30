//! Hand-written Anthropic Messages API client (no official Rust SDK exists).
//!
//! * [`wire`]   — serde types for `POST /v1/messages` and its response.
//! * [`schema`] — schemars → Anthropic structured-output schema subset.
//! * [`client`] — thin `reqwest` client.
//! * [`synth`]  — `Synth<Fresh | ToolRequested | Final>`: the single `lookup_rules` round.

pub mod client;
pub mod schema;
pub mod synth;
pub mod wire;

pub use client::{Client, ClientError};
pub use schema::{anthropic_schema, AnthropicSubset};
pub use synth::{Final, Fresh, LOOKUP_RULES, SendOutcome, Step, Synth, SynthConfig, ToolRequested, classify};

/// Default model. Opus 5 with adaptive thinking is the project baseline.
pub const DEFAULT_MODEL: &str = "claude-opus-5";
/// Messages API version header.
pub const API_VERSION: &str = "2023-06-01";
