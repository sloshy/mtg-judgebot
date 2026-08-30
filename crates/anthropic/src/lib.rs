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

pub use client::{Client, ClientError, DEFAULT_MAX_SPEND_USD, PRICES, Pricing, pricing_for};
pub use schema::{anthropic_schema, AnthropicSubset};
pub use synth::{Final, Fresh, LOOKUP_RULES, SendOutcome, Step, Synth, SynthConfig, ToolRequested, Truncated, classify};

/// Longest raw model text logged at debug level before parsing.
pub const LOG_TEXT_CHARS: usize = 2000;

/// The first `max` characters of `s` (char-safe), with `…` appended if cut.
/// For log lines: model output is logged before parsing so a schema failure
/// can be diagnosed.
#[must_use]
pub fn truncate_for_log(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    match s.char_indices().nth(max) {
        None => std::borrow::Cow::Borrowed(s),
        Some((end, _)) => std::borrow::Cow::Owned(format!("{}…", s.get(..end).unwrap_or_default())),
    }
}

/// Default model. Opus 5 with adaptive thinking is the project baseline.
pub const DEFAULT_MODEL: &str = "claude-opus-5";
/// Messages API version header.
pub const API_VERSION: &str = "2023-06-01";
