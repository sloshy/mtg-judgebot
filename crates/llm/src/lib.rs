//! The provider-neutral seam between the pipeline and whichever model serves
//! it. The judge asks a model for four things (extraction, a synthesis turn
//! with one optional `lookup_rules` round, and a citation retry) and needs
//! four answers back: text, tool calls, a stop reason and usage. Everything
//! here is about those, and nothing here knows a wire format.
//!
//! * [`ChatRequest`] / [`ChatResponse`] — the neutral request and response;
//!   [`AssistantTurn`] is the model's own previous turn, replayed opaquely.
//! * [`Backend`] — what a provider crate implements (`judge-anthropic` today).
//! * [`ChatModel`] — the port the pipeline calls. Sealed: only [`Metered`]
//!   implements it, so a backend cannot reach the wire uncapped.
//! * [`Metered`] — the spend cap around any backend: reserve a worst case
//!   before sending, settle to the real usage after; [`SpendMeter`] is the
//!   read handle the front doors log from, [`Price`] what a model is billed at.
//! * [`http`] — the retry loop every HTTP backend shares.
//! * [`synth`] — `Synth<Fresh | ToolRequested | Final>`, the typestate that
//!   bounds the tool round to one, and `classify`, the pure response reader.
//!
//! What makes the judge trustworthy is client-side (citation validation, the
//! typestate, schema-enforcing decode), so a backend that cannot enforce a
//! schema server-side degrades to more retries, not to weaker guarantees.
//! [`Capabilities`] says what a backend can enforce so the adapters can log
//! what they rely on.

pub mod error;
pub mod http;
pub mod spend;
pub mod synth;
mod types;

pub use error::LlmError;
pub use spend::{DEFAULT_MAX_SPEND_USD, Metered, PRICES, Price, Pricing, SpendMeter, pricing_for};
pub use synth::{
    Final, Fresh, LOOKUP_RULES, LookupRulesInput, SendOutcome, Step, Synth, SynthConfig, ToolRequested, Truncated,
    classify,
};
pub use types::{
    AssistantTurn, Backend, Billed, CacheHint, Capabilities, ChatModel, ChatRequest, ChatResponse, Effort, OutputSchema,
    Refusal, RefusalFallback, Stop, StructuredOutput, TextBlock, ToolCall, ToolChoice, ToolResult, ToolSpec, Turn,
    Usage, schema_of,
};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_for_log_cuts_on_char_boundaries() {
        assert_eq!(truncate_for_log("abc", 3), "abc");
        assert_eq!(truncate_for_log("abcd", 3), "abc…");
        assert_eq!(truncate_for_log("ééé", 2), "éé…");
    }
}
