//! Wire types for `POST /v1/messages`. Field names match the JSON exactly.
//! Unknown response fields are ignored; unknown enum values in responses map
//! to an explicit `Unknown` variant so drift is visible rather than fatal.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------- request ----------

/// Request body of `POST /v1/messages`.
#[derive(Clone, Debug, Serialize)]
pub struct MessagesRequest {
    /// Which model, spelled the way the door wants it (first, as the
    /// first-party body has it).
    #[serde(flatten)]
    pub model: ModelField,
    /// Hard output ceiling.
    pub max_tokens: u32,
    /// System prompt blocks.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<SystemBlock>,
    /// Conversation so far.
    pub messages: Vec<Message>,
    /// Client tools the model may call.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// How the model may pick tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Thinking mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    /// Effort and structured-output format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    /// Server-side refusal fallback (beta `server-side-fallback-2026-07-01`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallbacks: Option<Fallbacks>,
}

/// How the body names the model. Every door takes `"model": "<id>"`
/// except Vertex, which puts the model in the URL path and takes
/// `anthropic_version` in the body instead (on the other doors that is the
/// `anthropic-version` header). An enum so a body can carry neither or
/// both only by construction, never by a stray `Option`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ModelField {
    /// `"model": "<id>"`.
    InBody {
        /// Model id, e.g. `claude-opus-5` (`anthropic.claude-opus-5` on Bedrock).
        model: String,
    },
    /// `"anthropic_version": "vertex-2023-10-16"`; the model is in the URL.
    InUrl {
        /// The Vertex API version string ([`VERTEX_API_VERSION`]).
        anthropic_version: String,
    },
}

/// The `anthropic_version` Vertex takes in the body.
pub const VERTEX_API_VERSION: &str = "vertex-2023-10-16";

impl ModelField {
    /// The Vertex form.
    #[must_use]
    pub fn in_url() -> Self {
        Self::InUrl {
            anthropic_version: VERTEX_API_VERSION.to_owned(),
        }
    }
}

impl From<&str> for ModelField {
    fn from(model: &str) -> Self {
        Self::InBody {
            model: model.to_owned(),
        }
    }
}

impl From<String> for ModelField {
    fn from(model: String) -> Self {
        Self::InBody { model }
    }
}

/// Server-side fallback when the safety classifier refuses.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Fallbacks {
    /// Anthropic's recommended fallback, routed by refusal category.
    Default(DefaultFallback),
    /// Explicit list of fallback models, tried in order.
    Models(Vec<FallbackModel>),
}

impl Fallbacks {
    /// The `"default"` mode.
    #[must_use]
    pub const fn default_mode() -> Self {
        Self::Default(DefaultFallback::Default)
    }
    /// The beta header this parameter needs.
    pub const BETA: &'static str = "server-side-fallback-2026-07-01";
}

/// The literal string `"default"`.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DefaultFallback {
    /// `"default"`.
    Default,
}

/// One entry in the array form of `fallbacks`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FallbackModel {
    /// Model id to fall back to.
    pub model: String,
}

/// A system prompt block; `cache_control` marks a prompt-cache breakpoint.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemBlock {
    /// Plain text.
    Text {
        /// The text.
        text: String,
        /// Prompt-cache breakpoint.
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

impl SystemBlock {
    /// An uncached text block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            cache_control: None,
        }
    }
    /// A text block ending a 5-minute cache prefix.
    #[must_use]
    pub fn cached(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            cache_control: Some(CacheControl::ephemeral()),
        }
    }
}

/// `{"type":"ephemeral","ttl":"5m"|"1h"}`; `ttl` omitted means 5 minutes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheControl {
    /// Always `ephemeral`.
    #[serde(rename = "type")]
    pub kind: CacheKind,
    /// Cache lifetime; `None` is the 5-minute default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<CacheTtl>,
}

impl CacheControl {
    /// Default (5-minute) breakpoint.
    #[must_use]
    pub const fn ephemeral() -> Self {
        Self {
            kind: CacheKind::Ephemeral,
            ttl: None,
        }
    }
    /// Breakpoint with an explicit TTL.
    #[must_use]
    pub const fn with_ttl(ttl: CacheTtl) -> Self {
        Self {
            kind: CacheKind::Ephemeral,
            ttl: Some(ttl),
        }
    }
}

/// The only cache type the API has.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheKind {
    /// `ephemeral`.
    Ephemeral,
}

/// Prompt-cache lifetime.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum CacheTtl {
    /// Five minutes (the default).
    #[serde(rename = "5m")]
    FiveMinutes,
    /// One hour.
    #[serde(rename = "1h")]
    OneHour,
}

/// Message author.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The caller.
    User,
    /// The model.
    Assistant,
}

/// One conversation turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    /// Author.
    pub role: Role,
    /// Content blocks.
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// A user turn holding one text block.
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }
}

/// Content blocks appear in both requests and responses.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text {
        /// The text.
        text: String,
        /// Prompt-cache breakpoint (requests only; never set by responses).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// The model calling a client tool.
    ToolUse {
        /// Id to echo back in `tool_result`.
        id: String,
        /// Tool name.
        name: String,
        /// Arguments (validated against `input_schema` when `strict`).
        input: Value,
    },
    /// The caller answering a `tool_use`.
    ToolResult {
        /// The `tool_use.id` this answers.
        tool_use_id: String,
        /// Result text.
        content: String,
        /// Whether the tool failed.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    /// Model reasoning (empty text unless `display: summarized`).
    Thinking {
        /// Summary text, possibly empty.
        #[serde(default)]
        thinking: String,
        /// Opaque signature; echo back unchanged.
        #[serde(default)]
        signature: String,
    },
    /// Reasoning withheld by the API.
    RedactedThinking {
        /// Opaque payload; echo back unchanged.
        data: String,
    },
    /// Any block type we do not model; kept verbatim so it can be echoed back.
    #[serde(untagged)]
    Other(Value),
}

impl ContentBlock {
    /// An uncached text block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            cache_control: None,
        }
    }
}

/// A client tool definition.
#[derive(Clone, Debug, Serialize)]
pub struct Tool {
    /// Tool name.
    pub name: String,
    /// What the tool does and when to call it.
    pub description: String,
    /// JSON Schema of `input`.
    pub input_schema: Value,
    /// Guarantee `input` validates against `input_schema`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// Prompt-cache breakpoint after this tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// How the model may pick tools. `disable_parallel_tool_use: true` limits
/// the model to at most one `tool_use` block per turn.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides.
    Auto {
        /// At most one tool call per turn.
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    /// Model must call some tool.
    Any {
        /// At most one tool call per turn.
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    /// Model must call this tool.
    Tool {
        /// Tool name.
        name: String,
        /// At most one tool call per turn.
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    /// Model may not call tools.
    None,
}

impl ToolChoice {
    /// `auto` with parallel tool use disabled: at most one `tool_use` per turn.
    #[must_use]
    pub const fn auto_single() -> Self {
        Self::Auto {
            disable_parallel_tool_use: Some(true),
        }
    }
}

/// Adaptive thinking is the only mode used by this project (Opus 5 rejects `budget_tokens`).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Thinking {
    /// Model decides how much to think.
    Adaptive {
        /// Whether thinking text is returned.
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
}

impl Thinking {
    /// Adaptive thinking with the default (omitted) display.
    #[must_use]
    pub const fn adaptive() -> Self {
        Self::Adaptive { display: None }
    }
}

/// Whether `thinking` blocks carry text.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    /// Readable summary.
    Summarized,
    /// Empty text (default).
    Omitted,
}

/// `output_config.effort`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// `output_config`.
#[derive(Clone, Debug, Serialize, Default)]
pub struct OutputConfig {
    /// Thinking depth / token spend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Structured output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
}

/// Structured output: the final text block is JSON conforming to `schema`.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputFormat {
    /// JSON constrained by `schema`.
    JsonSchema {
        /// The schema (Anthropic subset; see `crate::schema`).
        schema: Value,
    },
}

// ---------- response ----------

/// Response body of `POST /v1/messages`.
#[derive(Clone, Debug, Deserialize)]
pub struct MessagesResponse {
    /// Message id.
    pub id: String,
    /// Model that answered (may differ from the request under `fallbacks`).
    pub model: String,
    /// Always `assistant`.
    pub role: Role,
    /// Content blocks.
    pub content: Vec<ContentBlock>,
    /// Why generation stopped.
    pub stop_reason: Option<StopReason>,
    /// Populated only for `refusal`.
    #[serde(default)]
    pub stop_details: Option<StopDetails>,
    /// Token accounting.
    pub usage: Usage,
}

impl MessagesResponse {
    /// Concatenated text of all `text` blocks.
    #[must_use]
    pub fn text(&self) -> String {
        self.text_blocks().collect()
    }

    /// Text of every `text` block, in order.
    pub fn text_blocks(&self) -> impl Iterator<Item = &str> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }

    /// All tool-use blocks as `(id, name, input)`.
    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

/// `stop_reason`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Natural end.
    EndTurn,
    /// Hit `max_tokens`; output is truncated.
    MaxTokens,
    /// Hit a stop sequence.
    StopSequence,
    /// The model wants tool results.
    ToolUse,
    /// A server tool needs the caller to continue the turn.
    PauseTurn,
    /// Safety classifier declined (HTTP 200). See `stop_details`.
    Refusal,
    /// A value this crate does not know yet.
    #[serde(other)]
    Unknown,
}

/// Populated only when `stop_reason == "refusal"`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct StopDetails {
    /// Always `refusal`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Refusal category (open set), if any.
    #[serde(default)]
    pub category: Option<String>,
    /// Human-readable explanation, if any.
    #[serde(default)]
    pub explanation: Option<String>,
}

/// Token usage.
#[derive(Clone, Debug, Deserialize, Default, PartialEq, Eq)]
pub struct Usage {
    /// Uncached input tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// Output tokens (thinking included).
    #[serde(default)]
    pub output_tokens: u64,
    /// Tokens written to the prompt cache.
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    /// Tokens read from the prompt cache.
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
}

/// Error body returned with a non-2xx status.
#[derive(Clone, Debug, Deserialize)]
pub struct ApiErrorBody {
    /// The error.
    pub error: ApiError,
}

/// `{"type": ..., "message": ...}`.
#[derive(Clone, Debug, Deserialize)]
pub struct ApiError {
    /// Error type, e.g. `invalid_request_error`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Human-readable message.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at<'a>(v: &'a Value, p: &str) -> &'a Value {
        v.pointer(p).unwrap_or(&Value::Null)
    }

    #[test]
    fn request_serializes_expected_shape() -> Result<(), serde_json::Error> {
        let req = MessagesRequest {
            model: "claude-opus-5".into(),
            max_tokens: 16000,
            system: vec![SystemBlock::cached("sys")],
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            tool_choice: Some(ToolChoice::auto_single()),
            thinking: Some(Thinking::adaptive()),
            output_config: Some(OutputConfig {
                effort: Some(Effort::High),
                format: None,
            }),
            fallbacks: Some(Fallbacks::default_mode()),
        };
        let v = serde_json::to_value(&req)?;
        assert_eq!(
            at(&v, "/thinking"),
            &serde_json::json!({"type": "adaptive"})
        );
        assert_eq!(
            at(&v, "/output_config"),
            &serde_json::json!({"effort": "high"})
        );
        assert_eq!(
            at(&v, "/system/0/cache_control"),
            &serde_json::json!({"type": "ephemeral"})
        );
        assert_eq!(
            at(&v, "/messages/0/content/0"),
            &serde_json::json!({"type": "text", "text": "hi"})
        );
        assert_eq!(
            at(&v, "/tool_choice"),
            &serde_json::json!({"type": "auto", "disable_parallel_tool_use": true})
        );
        assert_eq!(at(&v, "/fallbacks"), "default");
        assert!(v.get("tools").is_none());
        assert_eq!(at(&v, "/model"), "claude-opus-5");
        assert!(v.get("anthropic_version").is_none());
        // The model field comes first, as the first-party body has it.
        assert!(
            serde_json::to_string(&req)?
                .starts_with(r#"{"model":"claude-opus-5","max_tokens":16000,"#)
        );

        let vertex = MessagesRequest {
            model: ModelField::in_url(),
            ..req
        };
        let v = serde_json::to_value(&vertex)?;
        assert!(
            v.get("model").is_none(),
            "Vertex names the model in the URL: {v}"
        );
        assert_eq!(at(&v, "/anthropic_version"), VERTEX_API_VERSION);
        assert!(
            serde_json::to_string(&vertex)?
                .starts_with(r#"{"anthropic_version":"vertex-2023-10-16","max_tokens":16000,"#)
        );
        Ok(())
    }

    #[test]
    fn cache_control_ttl_and_placement() -> Result<(), serde_json::Error> {
        let cc = serde_json::to_value(CacheControl::with_ttl(CacheTtl::OneHour))?;
        assert_eq!(cc, serde_json::json!({"type": "ephemeral", "ttl": "1h"}));
        let tool = Tool {
            name: "t".into(),
            description: "d".into(),
            input_schema: serde_json::json!({"type": "object"}),
            strict: None,
            cache_control: Some(CacheControl::ephemeral()),
        };
        assert_eq!(
            at(&serde_json::to_value(&tool)?, "/cache_control/type"),
            "ephemeral"
        );
        let block = ContentBlock::Text {
            text: "x".into(),
            cache_control: Some(CacheControl::ephemeral()),
        };
        assert_eq!(
            at(&serde_json::to_value(&block)?, "/cache_control/type"),
            "ephemeral"
        );
        let models = serde_json::to_value(Fallbacks::Models(vec![FallbackModel {
            model: "claude-opus-4-8".into(),
        }]))?;
        assert_eq!(models, serde_json::json!([{"model": "claude-opus-4-8"}]));
        Ok(())
    }

    #[test]
    fn response_parses_refusal_and_unknown_stop() -> Result<(), serde_json::Error> {
        let raw = r#"{"id":"m","model":"claude-opus-5","role":"assistant","content":[{"type":"text","text":"t"}],
            "stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber"},
            "usage":{"input_tokens":1,"output_tokens":0}}"#;
        let r: MessagesResponse = serde_json::from_str(raw)?;
        assert_eq!(r.stop_reason, Some(StopReason::Refusal));
        assert_eq!(r.text(), "t");
        assert_eq!(
            r.stop_details.and_then(|d| d.category).as_deref(),
            Some("cyber")
        );
        let r: MessagesResponse =
            serde_json::from_str(&raw.replace("\"refusal\",", "\"brand_new\","))?;
        assert_eq!(r.stop_reason, Some(StopReason::Unknown));
        Ok(())
    }
}
