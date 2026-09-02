//! The neutral chat types. Field names describe what the pipeline means, not
//! what any API calls it; each backend maps them onto its wire format and is
//! the only place that knows the mapping.

use async_trait::async_trait;
use schemars::{JsonSchema, Schema, SchemaGenerator, generate::SchemaSettings};
use serde::Serialize;
use serde_json::Value;

use crate::LlmError;

/// One non-streaming chat request.
///
/// `Serialize` is for the spend cap's worst-case estimate (the serialized
/// size stands in for the input token count) and for debugging; it is not a
/// wire format.
#[derive(Clone, Debug, Serialize)]
pub struct ChatRequest {
    /// Hard output ceiling.
    pub max_tokens: u32,
    /// System prompt blocks; stable content first, with a [`CacheHint`] on
    /// the last block that is worth caching.
    pub system: Vec<TextBlock>,
    /// The conversation so far.
    pub turns: Vec<Turn>,
    /// Client tools the model may call.
    pub tools: Vec<ToolSpec>,
    /// How the model may pick tools. Meaningless, and not sent, when `tools`
    /// is empty.
    pub tool_choice: ToolChoice,
    /// The JSON Schema of the type the caller decodes the answer into.
    pub output: Option<OutputSchema>,
    /// How hard the model should think.
    pub effort: Option<Effort>,
    /// Ask for extended reasoning where a backend offers it as an opt-in
    /// (Anthropic: adaptive thinking). A backend whose models reason on their
    /// own ignores it.
    pub thinking: bool,
    /// Let the server route a refused request to another model, where the
    /// backend supports that.
    pub fallbacks: Option<RefusalFallback>,
}

/// A text block with an optional prompt-cache hint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TextBlock {
    /// The text.
    pub text: String,
    /// Marks the prefix up to and including this block as worth caching.
    pub cache: Option<CacheHint>,
}

impl TextBlock {
    /// A block with no cache hint.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), cache: None }
    }

    /// A block ending a cache prefix with the backend's default lifetime.
    #[must_use]
    pub fn cached(text: impl Into<String>) -> Self {
        Self { text: text.into(), cache: Some(CacheHint::Short) }
    }
}

/// A prompt-cache breakpoint. A hint: Anthropic emits `cache_control`,
/// backends that cache by prefix on their own ignore it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum CacheHint {
    /// The backend's default lifetime (five minutes on Anthropic).
    Short,
    /// The backend's long lifetime (one hour on Anthropic).
    Long,
}

/// One conversation turn.
#[derive(Clone, Debug, Serialize)]
pub enum Turn {
    /// The caller's text.
    User(Vec<TextBlock>),
    /// The model's own previous turn, replayed byte-for-byte.
    Assistant(AssistantTurn),
    /// The caller answering the model's tool calls.
    ToolResults(Vec<ToolResult>),
}

/// The model's own previous turn, as the backend that produced it returned
/// it. Opaque on purpose: thinking signatures, reasoning content, tool calls —
/// whatever came back is what goes back. Only the backend named by `backend`
/// reads `raw`; any other returns [`LlmError::ForeignTurn`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AssistantTurn {
    /// The backend that produced it (`judge_anthropic::BACKEND`, …).
    pub backend: &'static str,
    /// The backend's own representation of the turn.
    pub raw: Value,
}

/// The caller's answer to one tool call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ToolResult {
    /// The [`ToolCall::id`] this answers.
    pub call_id: String,
    /// Result text.
    pub content: String,
    /// Whether the tool failed.
    pub is_error: bool,
}

/// A client tool the model may call.
#[derive(Clone, Debug, Serialize)]
pub struct ToolSpec {
    /// Tool name.
    pub name: String,
    /// What the tool does and when to call it.
    pub description: String,
    /// JSON Schema of the input, untransformed; each backend applies its own
    /// subset transform.
    pub input_schema: Schema,
    /// Ask the backend to guarantee the input validates against the schema.
    pub strict: bool,
}

/// How the model may pick tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum ToolChoice {
    /// The model decides.
    Auto {
        /// Whether several calls may come back in one turn.
        parallel: bool,
    },
    /// The model may not call tools.
    None,
}

/// The JSON Schema of the type the caller decodes the answer into. Carried
/// untransformed (the full schemars output); a backend applies its own subset
/// transform at conversion time, and decoding still enforces everything the
/// transform stripped.
#[derive(Clone, Debug, Serialize)]
pub struct OutputSchema {
    /// The schema.
    pub schema: Schema,
}

impl OutputSchema {
    /// The schema of `T`.
    #[must_use]
    pub fn of<T: JsonSchema>() -> Self {
        Self { schema: schema_of::<T>() }
    }
}

/// The full draft 2020-12 schema of `T`, before any backend's transform.
#[must_use]
pub fn schema_of<T: JsonSchema>() -> Schema {
    SchemaGenerator::new(SchemaSettings::draft2020_12()).into_root_schema_for::<T>()
}

/// How hard the model should think. Ordered so an adapter can compare
/// (the truncation retry lowers the effort).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[allow(missing_docs)]
pub enum Effort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

/// What the server may route a refused request to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum RefusalFallback {
    /// The backend's recommended fallback.
    Default,
    /// These models, tried in order.
    Models(Vec<String>),
}

/// One non-streaming chat response.
#[derive(Clone, Debug)]
pub struct ChatResponse {
    /// Text blocks in order; with structured output the JSON is the last.
    pub text: Vec<String>,
    /// Tool calls the model made.
    pub tool_calls: Vec<ToolCall>,
    /// Why generation stopped.
    pub stop: Stop,
    /// Token accounting.
    pub usage: Usage,
    /// The model that answered, as billed (may differ from the one asked for
    /// under fallbacks).
    pub model: String,
    /// The turn to replay in a continuation.
    pub assistant: AssistantTurn,
}

impl ChatResponse {
    /// The last text block, where structured output puts the JSON.
    #[must_use]
    pub fn last_text(&self) -> Option<&str> {
        self.text.last().map(String::as_str)
    }
}

/// The model calling a client tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    /// Id to echo back in the [`ToolResult`].
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments.
    pub input: Value,
}

/// Why generation stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Natural end.
    EndTurn,
    /// Hit `max_tokens`; the output is truncated.
    MaxTokens,
    /// The model wants tool results.
    ToolUse,
    /// The model (or a classifier in front of it) declined.
    Refusal(Refusal),
    /// A reason the pipeline has no use for; the backend's own name for it.
    Other(String),
}

/// Details of a refusal, where the backend gives any.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refusal {
    /// Refusal category (open set).
    pub category: Option<String>,
    /// Human-readable explanation.
    pub explanation: Option<String>,
}

/// Token usage; a count the backend does not report is `0`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Uncached input tokens.
    pub input: u64,
    /// Output tokens, reasoning included.
    pub output: u64,
    /// Tokens read from the prompt cache.
    pub cache_read: u64,
    /// Tokens written to the prompt cache.
    pub cache_write: u64,
}

/// The usage of a 2xx body that did not decode as a response: billed by the
/// provider, so billed by the spend cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Billed {
    /// The model the body names, if it does.
    pub model: Option<String>,
    /// Its usage.
    pub usage: Usage,
}

/// What a backend can enforce server-side. The adapters use it to decide
/// whether the schema must be in the prompt and to log what they rely on.
// Independent yes/no facts about a backend, not a state machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// How the output schema is honoured.
    pub structured_output: StructuredOutput,
    /// Tool inputs are guaranteed to validate against their schema.
    pub strict_tools: bool,
    /// [`ChatRequest::effort`] is honoured.
    pub effort: bool,
    /// [`CacheHint`]s are honoured.
    pub cache_hints: bool,
    /// [`ChatRequest::fallbacks`] is honoured.
    pub refusal_fallbacks: bool,
}

/// How a backend honours [`OutputSchema`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructuredOutput {
    /// The output is constrained to the schema.
    Enforced,
    /// The output is valid JSON, shape not guaranteed.
    JsonMode,
    /// The schema can only be asked for in the prompt.
    PromptOnly,
}

/// What a provider crate implements: one uncapped round trip. Open to any
/// crate (`judge-anthropic` today), but never handed to the pipeline as is —
/// the pipeline's port is [`ChatModel`], which only [`crate::Metered`]
/// implements, so a backend reaches the wire only through the spend cap.
#[async_trait]
pub trait Backend: Send + Sync {
    /// One non-streaming round trip. Retries belong here (see
    /// [`crate::http`]); the spend cap belongs to [`crate::Metered`], not here.
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError>;
    /// What this backend can enforce server-side.
    fn capabilities(&self) -> Capabilities;
    /// The provider the price table is keyed by (`"anthropic"`, …).
    fn provider(&self) -> &'static str;
    /// Model id as the backend will bill it.
    fn model(&self) -> &str;
}

/// The port the pipeline calls. Sealed: the one implementation is
/// [`crate::Metered`] over a [`Backend`], so a `dyn ChatModel` *is* a capped
/// model — "every send goes through the spend cap" is a fact about the
/// types, not a discipline at the composition root.
#[async_trait]
pub trait ChatModel: sealed::Sealed + Send + Sync {
    /// One non-streaming round trip behind the spend cap: reserve, send, settle.
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError>;
    /// What the backend can enforce server-side.
    fn capabilities(&self) -> Capabilities;
    /// The provider the price table is keyed by (`"anthropic"`, …).
    fn provider(&self) -> &'static str;
    /// Model id as the backend will bill it.
    fn model(&self) -> &str;
}

pub(crate) mod sealed {
    /// Implemented for [`crate::Metered`] only.
    pub trait Sealed {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_orders_and_blocks_carry_hints() {
        assert!(Effort::Low < Effort::Medium && Effort::Medium < Effort::High && Effort::High < Effort::XHigh && Effort::XHigh < Effort::Max);
        assert_eq!(TextBlock::cached("s").cache, Some(CacheHint::Short));
        assert_eq!(TextBlock::plain("s").cache, None);
    }

    #[test]
    fn schema_of_is_the_untransformed_schemars_output() {
        let s = OutputSchema::of::<judge_core::Verdict>().schema.to_value();
        // The tagged enum is still a oneOf here; the backend transform turns it into anyOf.
        assert!(s.to_string().contains("oneOf"), "{s:#}");
        let last = ChatResponse {
            text: vec!["a".into(), "b".into()],
            tool_calls: vec![],
            stop: Stop::EndTurn,
            usage: Usage::default(),
            model: "m".into(),
            assistant: AssistantTurn { backend: "test", raw: Value::Null },
        };
        assert_eq!(last.last_text(), Some("b"));
    }
}
