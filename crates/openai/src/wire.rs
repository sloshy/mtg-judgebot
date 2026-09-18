//! Wire types for `POST /chat/completions`. Field names match the JSON
//! exactly. Unknown response fields are ignored; `finish_reason` is kept as
//! the server's own string, because compatible servers invent values and
//! the neutral [`judge_llm::Stop::Other`] carries the name through.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------- request ----------

/// Request body of `POST /chat/completions`.
#[derive(Clone, Debug, Serialize)]
pub struct ChatRequest {
    /// Model id, as the server names it (`gpt-5`, `qwen3:8b`, a `LiteLLM` alias).
    pub model: String,
    /// Conversation so far, system message first.
    pub messages: Vec<Message>,
    /// Output ceiling, by its older name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Output ceiling, by its `OpenAI` name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    /// Client tools the model may call.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// How the model may pick tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Whether several calls may come back in one turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Structured output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    /// Reasoning depth (reasoning models only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// One conversation message. The assistant's own turn is replayed as the
/// `choices[0].message` object it came from, verbatim, so `tool_calls`,
/// `reasoning_content` and anything else the server put there go back.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// The system prompt.
    System {
        /// Text, or parts when cache hints are forwarded.
        content: Content,
    },
    /// The caller.
    User {
        /// Text, or parts when cache hints are forwarded.
        content: Content,
    },
    /// The caller answering one tool call.
    Tool {
        /// The `tool_calls[].id` this answers.
        tool_call_id: String,
        /// Result text.
        content: String,
    },
    /// The model's previous turn, verbatim (carries its own `"role":"assistant"`).
    #[serde(untagged)]
    Assistant(Value),
}

/// Message content: a plain string, or content parts (the form that can
/// carry `cache_control`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Content {
    /// Plain text.
    Text(String),
    /// Content parts.
    Parts(Vec<Part>),
}

impl Content {
    /// The text of every text part, in order.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        let (single, parts) = match self {
            Content::Text(t) => (Some(t.as_str()), None),
            Content::Parts(p) => (None, Some(p)),
        };
        single
            .into_iter()
            .chain(parts.into_iter().flatten().filter_map(|p| match p {
                Part::Text { text, .. } => Some(text.as_str()),
                Part::Other(_) => None,
            }))
    }
}

/// One content part.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    /// Text.
    Text {
        /// The text.
        text: String,
        /// Prompt-cache breakpoint, forwarded by gateways to Anthropic upstreams.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// A part type this crate does not model (images, audio); kept verbatim.
    #[serde(untagged)]
    Other(Value),
}

/// `{"type":"ephemeral"}` with an optional `ttl`, as Anthropic spells it.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheControl {
    /// Always `ephemeral`.
    #[serde(rename = "type")]
    pub kind: CacheKind,
    /// `"1h"` for the long lifetime; omitted for the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<CacheTtl>,
}

/// The only cache type there is.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheKind {
    /// `ephemeral`.
    Ephemeral,
}

/// Prompt-cache lifetime.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum CacheTtl {
    /// One hour.
    #[serde(rename = "1h")]
    OneHour,
}

/// A client tool: `{"type":"function","function":{...}}`.
#[derive(Clone, Debug, Serialize)]
pub struct Tool {
    /// Always `function`.
    #[serde(rename = "type")]
    pub kind: FunctionKind,
    /// The function.
    pub function: Function,
}

/// The literal `"function"`.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FunctionKind {
    /// `function`.
    Function,
}

/// A function definition.
#[derive(Clone, Debug, Serialize)]
pub struct Function {
    /// Tool name.
    pub name: String,
    /// What the tool does and when to call it.
    pub description: String,
    /// JSON Schema of the arguments (strict subset when `strict`).
    pub parameters: Value,
    /// Guarantee the arguments validate against `parameters`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// How the model may pick tools.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    /// The model decides.
    Auto,
    /// The model may not call tools.
    None,
}

/// `response_format`.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// The output is constrained to a schema.
    JsonSchema {
        /// The schema envelope.
        json_schema: JsonSchemaFormat,
    },
    /// The output is valid JSON of no particular shape.
    JsonObject,
}

/// `response_format.json_schema`.
#[derive(Clone, Debug, Serialize)]
pub struct JsonSchemaFormat {
    /// A name for the schema (required by the API; not meaningful).
    pub name: String,
    /// The schema (strict subset; see `crate::schema`).
    pub schema: Value,
    /// Constrain the output to the schema exactly.
    pub strict: bool,
}

/// `reasoning_effort`.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[expect(
    missing_docs,
    reason = "the variants are the wire's reasoning_effort values"
)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

// ---------- response ----------

/// Response body of `POST /chat/completions`.
#[derive(Clone, Debug, Deserialize)]
pub struct ChatResponse {
    /// The model that answered, when the server says.
    #[serde(default)]
    pub model: Option<String>,
    /// The choices; only the first is read.
    pub choices: Vec<Choice>,
    /// Token accounting, when the server reports it.
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// One choice. `message` is kept raw for verbatim replay and read through
/// [`AssistantMessage`] for what the pipeline needs.
#[derive(Clone, Debug, Deserialize)]
pub struct Choice {
    /// The assistant message, verbatim.
    pub message: Value,
    /// Why generation stopped, as the server names it.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The fields of `choices[].message` the pipeline reads.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AssistantMessage {
    /// The text, if any (`null` alongside tool calls).
    #[serde(default)]
    pub content: Option<Content>,
    /// A refusal from the model's safety training, when non-null.
    #[serde(default)]
    pub refusal: Option<String>,
    /// Tool calls.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

/// One tool call. `function.arguments` is a JSON *string*.
#[derive(Clone, Debug, Deserialize)]
pub struct ToolCall {
    /// Id to echo back as `tool_call_id`.
    pub id: String,
    /// The call.
    pub function: FunctionCall,
}

/// `tool_calls[].function`.
#[derive(Clone, Debug, Deserialize)]
pub struct FunctionCall {
    /// Tool name.
    pub name: String,
    /// The arguments as a JSON document in a string.
    #[serde(default)]
    pub arguments: String,
}

/// Token usage. `prompt_tokens` *includes* the cached ones (unlike
/// Anthropic's `input_tokens`); `convert::usage` separates them.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Usage {
    /// Prompt tokens, cached ones included.
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Output tokens, reasoning included.
    #[serde(default)]
    pub completion_tokens: u64,
    /// Cache accounting.
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

/// `usage.prompt_tokens_details`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct PromptTokensDetails {
    /// Prompt tokens served from the cache.
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

/// Error body returned with a non-2xx status.
#[derive(Clone, Debug, Deserialize)]
pub struct ApiErrorBody {
    /// The error.
    pub error: ApiError,
}

/// `{"message": ..., "type": ..., "code": ...}`; only `message` is reliable
/// across servers.
#[derive(Clone, Debug, Deserialize)]
pub struct ApiError {
    /// Human-readable message.
    pub message: String,
    /// Error type, e.g. `invalid_request_error`.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at<'a>(v: &'a Value, p: &str) -> &'a Value {
        v.pointer(p).unwrap_or(&Value::Null)
    }

    #[test]
    fn request_serializes_expected_shape() -> Result<(), serde_json::Error> {
        let req = ChatRequest {
            model: "gpt-5".into(),
            messages: vec![
                Message::System {
                    content: Content::Text("sys".into()),
                },
                Message::User {
                    content: Content::Parts(vec![Part::Text {
                        text: "hi".into(),
                        cache_control: Some(CacheControl {
                            kind: CacheKind::Ephemeral,
                            ttl: None,
                        }),
                    }]),
                },
                Message::Assistant(
                    json!({"role": "assistant", "content": null, "reasoning_content": "r", "tool_calls": [{"id": "c1"}]}),
                ),
                Message::Tool {
                    tool_call_id: "c1".into(),
                    content: "rules".into(),
                },
            ],
            max_tokens: None,
            max_completion_tokens: Some(16000),
            tools: vec![Tool {
                kind: FunctionKind::Function,
                function: Function {
                    name: "lookup_rules".into(),
                    description: "d".into(),
                    parameters: json!({"type": "object"}),
                    strict: Some(true),
                },
            }],
            tool_choice: Some(ToolChoice::Auto),
            parallel_tool_calls: Some(false),
            response_format: Some(ResponseFormat::JsonSchema {
                json_schema: JsonSchemaFormat {
                    name: "Verdict".into(),
                    schema: json!({"type": "object"}),
                    strict: true,
                },
            }),
            reasoning_effort: Some(ReasoningEffort::High),
        };
        let v = serde_json::to_value(&req)?;
        assert_eq!(
            at(&v, "/messages/0"),
            &json!({"role": "system", "content": "sys"})
        );
        assert_eq!(
            at(&v, "/messages/1"),
            &json!({"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]})
        );
        assert_eq!(
            at(&v, "/messages/2"),
            &json!({"role": "assistant", "content": null, "reasoning_content": "r", "tool_calls": [{"id": "c1"}]})
        );
        assert_eq!(
            at(&v, "/messages/3"),
            &json!({"role": "tool", "tool_call_id": "c1", "content": "rules"})
        );
        assert!(v.get("max_tokens").is_none());
        assert_eq!(at(&v, "/max_completion_tokens"), 16000);
        assert_eq!(
            at(&v, "/tools/0"),
            &json!({"type": "function", "function": {"name": "lookup_rules", "description": "d", "parameters": {"type": "object"}, "strict": true}})
        );
        assert_eq!(at(&v, "/tool_choice"), "auto");
        assert_eq!(at(&v, "/parallel_tool_calls"), false);
        assert_eq!(
            at(&v, "/response_format"),
            &json!({"type": "json_schema", "json_schema": {"name": "Verdict", "schema": {"type": "object"}, "strict": true}})
        );
        assert_eq!(at(&v, "/reasoning_effort"), "high");
        assert_eq!(serde_json::to_value(ToolChoice::None)?, "none");
        assert_eq!(
            serde_json::to_value(ResponseFormat::JsonObject)?,
            json!({"type": "json_object"})
        );
        Ok(())
    }

    #[test]
    fn response_parses_tool_calls_reasoning_and_refusal() -> Result<(), serde_json::Error> {
        let raw = json!({
            "id": "chatcmpl-1", "model": "deepseek-r1", "choices": [{
                "index": 0, "finish_reason": "tool_calls",
                "message": {"role": "assistant", "content": null, "reasoning_content": "think",
                            "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "lookup_rules", "arguments": "{\"ids\":[\"613\"]}"}}]}
            }],
            "usage": {"prompt_tokens": 100, "completion_tokens": 7, "prompt_tokens_details": {"cached_tokens": 60}}
        });
        let r: ChatResponse = serde_json::from_value(raw)?;
        assert_eq!(r.model.as_deref(), Some("deepseek-r1"));
        let c = r
            .choices
            .first()
            .ok_or_else(|| serde::de::Error::custom("no choice"))?;
        assert_eq!(c.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(at(&c.message, "/reasoning_content"), "think");
        let m: AssistantMessage = serde_json::from_value(c.message.clone())?;
        assert!(m.content.is_none() && m.refusal.is_none());
        assert_eq!(
            m.tool_calls.first().map(|t| (
                t.id.as_str(),
                t.function.name.as_str(),
                t.function.arguments.as_str()
            )),
            Some(("call_1", "lookup_rules", "{\"ids\":[\"613\"]}"))
        );
        assert_eq!(
            r.usage
                .and_then(|u| u.prompt_tokens_details)
                .and_then(|d| d.cached_tokens),
            Some(60)
        );

        let m: AssistantMessage = serde_json::from_value(
            json!({"role": "assistant", "content": [{"type": "text", "text": "a"}, {"type": "image_url", "image_url": {}}], "refusal": "no"}),
        )?;
        assert_eq!(
            m.content.as_ref().map(|c| c.texts().collect::<Vec<_>>()),
            Some(vec!["a"])
        );
        assert_eq!(m.refusal.as_deref(), Some("no"));
        Ok(())
    }
}
