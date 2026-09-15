//! Neutral ↔ chat completions. The only place that knows how a
//! [`ChatRequest`] is spelled on an OpenAI-compatible wire under a
//! [`Dialect`], and how a [`wire::ChatResponse`] reads as a
//! [`ChatResponse`]. Pure: unit-tested against JSON shapes, no network.
//!
//! The mapping (`docs/PROVIDERS.md` §4.2):
//!
//! | neutral | chat completions |
//! |---|---|
//! | `system` blocks | one `system` message: the texts joined, or parts with `cache_control` when `cache_hints` |
//! | `Turn::User` | one `user` message, likewise |
//! | `Turn::Assistant` | `choices[0].message` replayed verbatim |
//! | `Turn::ToolResults` | one `tool` message per result, with `tool_call_id` |
//! | `ToolSpec` | `tools: [{type: function, function: {name, description, parameters, strict}}]` |
//! | `ToolChoice::Auto { parallel }` | `tool_choice: "auto"`, `parallel_tool_calls: parallel` |
//! | `ToolChoice::None` | `tool_choice: "none"` |
//! | `OutputSchema` | `response_format` per `structured_output`; the prompt carries the schema otherwise |
//! | `Effort` | `reasoning_effort` (`xhigh`/`max` → `high`) when `reasoning_effort` |
//! | `max_tokens` | `max_tokens` or `max_completion_tokens` per `max_tokens_param` |
//! | `finish_reason` | `stop` → `EndTurn`, `length` → `MaxTokens`, `tool_calls` → `ToolUse`, `content_filter` → `Refusal`; a non-null `message.refusal` → `Refusal` |
//! | `usage` | `prompt_tokens − cached_tokens` → input, `cached_tokens` → cache read, `completion_tokens` → output |
//! | `function.arguments` | a JSON **string**, parsed with serde |
//!
//! `thinking` and `fallbacks` have no counterpart and are ignored: a
//! reasoning model reasons on its own, and refusal fallbacks are an Anthropic
//! feature the capabilities already report as absent.

use judge_llm::{
    AssistantTurn, Billed, CacheHint, ChatRequest, ChatResponse, Effort, LlmError, Refusal, Stop,
    TextBlock, ToolCall, ToolChoice, Turn, Usage,
};
use serde_json::Value;

use crate::{
    Dialect, MaxTokensParam, StructuredOutputMode,
    schema::to_openai_strict,
    wire::{
        self, AssistantMessage, CacheControl, CacheKind, CacheTtl, Content, Function, FunctionKind,
        JsonSchemaFormat, Message, Part, ReasoningEffort, ResponseFormat, Tool,
    },
};

/// The [`AssistantTurn::backend`] tag of turns this backend produces.
pub const BACKEND: &str = "openai";

/// The chat completions body for `req` against `model` under `dialect`.
///
/// # Errors
/// `ForeignTurn` when a replayed assistant turn came from another backend;
/// `Request` when its payload is not a message object.
pub fn to_wire(
    model: &str,
    dialect: Dialect,
    req: &ChatRequest,
) -> Result<wire::ChatRequest, LlmError> {
    let mut messages = Vec::with_capacity(req.turns.len() + 1);
    if !req.system.is_empty() {
        messages.push(Message::System {
            content: content(&req.system, dialect.cache_hints),
        });
    }
    for turn in &req.turns {
        match turn {
            Turn::User(blocks) => messages.push(Message::User {
                content: content(blocks, dialect.cache_hints),
            }),
            Turn::Assistant(AssistantTurn { backend, raw }) => {
                if *backend != BACKEND {
                    return Err(LlmError::ForeignTurn {
                        expected: BACKEND,
                        found: backend,
                    });
                }
                if !raw.is_object() {
                    return Err(LlmError::Request(
                        "assistant turn is not a chat completions message object".into(),
                    ));
                }
                messages.push(Message::Assistant(raw.clone()));
            }
            Turn::ToolResults(results) => {
                for r in results {
                    messages.push(Message::Tool {
                        tool_call_id: r.call_id.clone(),
                        content: r.content.clone(),
                    });
                }
            }
        }
    }
    let (max_tokens, max_completion_tokens) = match dialect.max_tokens_param {
        MaxTokensParam::MaxTokens => (Some(req.max_tokens), None),
        MaxTokensParam::MaxCompletionTokens => (None, Some(req.max_tokens)),
    };
    let tools: Vec<Tool> = req
        .tools
        .iter()
        .map(|t| Tool {
            kind: FunctionKind::Function,
            function: Function {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: to_openai_strict(&t.input_schema),
                strict: (t.strict && dialect.strict_tools).then_some(true),
            },
        })
        .collect();
    // Meaningless without tools; some servers reject it there.
    let (tool_choice, parallel_tool_calls) = if tools.is_empty() {
        (None, None)
    } else {
        match req.tool_choice {
            ToolChoice::Auto { parallel } => (Some(wire::ToolChoice::Auto), Some(parallel)),
            ToolChoice::None => (Some(wire::ToolChoice::None), None),
        }
    };
    let response_format = req
        .output
        .as_ref()
        .and_then(|o| match dialect.structured_output {
            StructuredOutputMode::JsonSchema => Some(ResponseFormat::JsonSchema {
                json_schema: JsonSchemaFormat {
                    name: schema_name(&o.schema),
                    schema: to_openai_strict(&o.schema),
                    strict: true,
                },
            }),
            StructuredOutputMode::JsonObject => Some(ResponseFormat::JsonObject),
            StructuredOutputMode::Prompt => None,
        });
    Ok(wire::ChatRequest {
        model: model.to_owned(),
        messages,
        max_tokens,
        max_completion_tokens,
        tools,
        tool_choice,
        parallel_tool_calls,
        response_format,
        reasoning_effort: if dialect.reasoning_effort {
            req.effort.map(effort)
        } else {
            None
        },
    })
}

/// The name `response_format.json_schema` needs: the schema's title (the
/// type name, from schemars) or a placeholder.
fn schema_name(schema: &schemars::Schema) -> String {
    schema
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("output")
        .to_owned()
}

/// Blocks as one message content: joined text, or parts when cache hints are
/// forwarded (a part is the only place `cache_control` can go).
fn content(blocks: &[TextBlock], cache_hints: bool) -> Content {
    if cache_hints && blocks.iter().any(|b| b.cache.is_some()) {
        Content::Parts(
            blocks
                .iter()
                .map(|b| Part::Text {
                    text: b.text.clone(),
                    cache_control: b.cache.map(cache_control),
                })
                .collect(),
        )
    } else {
        Content::Text(blocks.iter().map(|b| b.text.as_str()).collect())
    }
}

fn cache_control(hint: CacheHint) -> CacheControl {
    match hint {
        CacheHint::Short => CacheControl {
            kind: CacheKind::Ephemeral,
            ttl: None,
        },
        CacheHint::Long => CacheControl {
            kind: CacheKind::Ephemeral,
            ttl: Some(CacheTtl::OneHour),
        },
    }
}

fn effort(e: Effort) -> ReasoningEffort {
    match e {
        Effort::Low => ReasoningEffort::Low,
        Effort::Medium => ReasoningEffort::Medium,
        Effort::High | Effort::XHigh | Effort::Max => ReasoningEffort::High,
    }
}

/// Read a chat completions response as the neutral response. The assistant
/// turn is `choices[0].message` verbatim, so a continuation replays
/// `tool_calls`, `reasoning_content` and whatever else came with it.
/// `model` is what was asked for, used when the server does not say.
///
/// A `stop` finish with tool calls present is read as `ToolUse`: some
/// compatible servers (Ollama, older vLLM) label a tool-calling turn that
/// way, and the calls are the response's own content, not a guess.
///
/// # Errors
/// No choices, a message that is not a message, or `function.arguments`
/// that is not a JSON document.
pub fn from_wire(
    resp: &wire::ChatResponse,
    model: &str,
) -> Result<ChatResponse, serde_json::Error> {
    let choice = resp
        .choices
        .first()
        .ok_or_else(|| serde::de::Error::custom("response has no choices"))?;
    let message: AssistantMessage = serde_json::from_value(choice.message.clone())?;
    let tool_calls = message
        .tool_calls
        .iter()
        .map(|c| {
            let input: Value = serde_json::from_str(&c.function.arguments)?;
            Ok(ToolCall {
                id: c.id.clone(),
                name: c.function.name.clone(),
                input,
            })
        })
        .collect::<Result<Vec<_>, serde_json::Error>>()?;
    let refusal = message.refusal.as_deref().filter(|r| !r.trim().is_empty());
    let stop = match (choice.finish_reason.as_deref(), refusal) {
        (_, Some(text)) => Stop::Refusal(Refusal {
            category: None,
            explanation: Some(text.to_owned()),
        }),
        (Some("content_filter"), None) => Stop::Refusal(Refusal {
            category: Some("content_filter".into()),
            explanation: None,
        }),
        (Some("length"), None) => Stop::MaxTokens,
        (Some("tool_calls" | "function_call"), None) => Stop::ToolUse,
        (Some("stop") | None, None) if !tool_calls.is_empty() => Stop::ToolUse,
        (Some("stop"), None) => Stop::EndTurn,
        (Some(other), None) => Stop::Other(other.to_owned()),
        (None, None) => Stop::Other("none".into()),
    };
    Ok(ChatResponse {
        text: message
            .content
            .iter()
            .flat_map(Content::texts)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect(),
        tool_calls,
        stop,
        usage: resp.usage.as_ref().map(usage).unwrap_or_default(),
        model: resp.model.clone().unwrap_or_else(|| model.to_owned()),
        assistant: AssistantTurn {
            backend: BACKEND,
            raw: choice.message.clone(),
        },
    })
}

/// Neutral usage: `prompt_tokens` includes the cached tokens, so the
/// uncached input is the difference (what the provider bills at the input
/// price).
fn usage(u: &wire::Usage) -> Usage {
    let cached = u
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens)
        .unwrap_or(0);
    Usage {
        input: u.prompt_tokens.saturating_sub(cached),
        output: u.completion_tokens,
        cache_read: cached,
        cache_write: 0,
    }
}

/// `usage` (and `model`) of a 2xx body, parsed leniently so that a response
/// which fails to decode as [`wire::ChatResponse`] is still billed.
#[derive(Debug, serde::Deserialize)]
struct UsageOnly {
    #[serde(default)]
    model: Option<String>,
    usage: wire::Usage,
}

/// What a body that did not decode can still be billed as.
#[must_use]
pub fn usage_of(body: &[u8]) -> Option<Billed> {
    serde_json::from_slice::<UsageOnly>(body)
        .ok()
        .map(|u| Billed {
            model: u.model,
            usage: usage(&u.usage),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_llm::{OutputSchema, ToolResult, ToolSpec, schema_of};
    use serde_json::json;

    fn at<'a>(v: &'a Value, p: &str) -> &'a Value {
        v.pointer(p).unwrap_or(&Value::Null)
    }

    fn req() -> ChatRequest {
        ChatRequest {
            max_tokens: 64,
            system: vec![TextBlock::cached("sys")],
            turns: vec![Turn::User(vec![
                TextBlock::cached("material"),
                TextBlock::plain("question"),
            ])],
            tools: vec![],
            tool_choice: ToolChoice::None,
            output: None,
            effort: None,
            thinking: false,
            fallbacks: None,
        }
    }

    #[test]
    fn a_bare_request_sends_only_what_it_has() -> Result<(), Box<dyn std::error::Error>> {
        let v = serde_json::to_value(to_wire("qwen3:8b", Dialect::default(), &req())?)?;
        assert_eq!(
            v,
            json!({
                "model": "qwen3:8b", "max_tokens": 64,
                "messages": [
                    {"role": "system", "content": "sys"},
                    {"role": "user", "content": "materialquestion"}
                ]
            })
        );
        Ok(())
    }

    #[test]
    fn every_knob_maps_to_its_field() -> Result<(), Box<dyn std::error::Error>> {
        let r = ChatRequest {
            tools: vec![ToolSpec {
                name: "t".into(),
                description: "d".into(),
                input_schema: schema_of::<judge_llm::LookupRulesInput>(),
                strict: true,
            }],
            tool_choice: ToolChoice::Auto { parallel: false },
            output: Some(OutputSchema::of::<judge_core::Verdict>()),
            effort: Some(Effort::XHigh),
            thinking: true,
            fallbacks: Some(judge_llm::RefusalFallback::Default),
            ..req()
        };
        let d = Dialect {
            reasoning_effort: true,
            cache_hints: true,
            max_tokens_param: MaxTokensParam::MaxCompletionTokens,
            ..Dialect::default()
        };
        let v = serde_json::to_value(to_wire("gpt-5", d, &r)?)?;
        assert_eq!(
            at(&v, "/messages/0"),
            &json!({"role": "system", "content": [{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}]})
        );
        assert_eq!(
            at(&v, "/messages/1/content"),
            &json!([
                {"type": "text", "text": "material", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "question"}
            ])
        );
        assert_eq!(at(&v, "/tools/0/type"), "function");
        assert_eq!(at(&v, "/tools/0/function/name"), "t");
        assert_eq!(at(&v, "/tools/0/function/strict"), &Value::Bool(true));
        assert_eq!(
            at(&v, "/tools/0/function/parameters/additionalProperties"),
            &Value::Bool(false),
            "the strict transform ran"
        );
        assert_eq!(
            at(&v, "/tools/0/function/parameters/required"),
            &json!(["ids"])
        );
        assert_eq!(at(&v, "/tool_choice"), "auto");
        assert_eq!(at(&v, "/parallel_tool_calls"), &Value::Bool(false));
        assert_eq!(at(&v, "/response_format/type"), "json_schema");
        assert_eq!(at(&v, "/response_format/json_schema/name"), "Verdict");
        assert_eq!(
            at(&v, "/response_format/json_schema/strict"),
            &Value::Bool(true)
        );
        assert!(
            !at(&v, "/response_format/json_schema/schema")
                .to_string()
                .contains("oneOf"),
            "the strict transform ran"
        );
        assert_eq!(
            at(&v, "/reasoning_effort"),
            "high",
            "xhigh maps down to high"
        );
        assert!(v.get("max_tokens").is_none());
        assert_eq!(at(&v, "/max_completion_tokens"), 64);
        // Nothing Anthropic-shaped leaks.
        assert!(
            v.get("thinking").is_none()
                && v.get("fallbacks").is_none()
                && v.get("output_config").is_none()
        );

        // The other settings of each knob.
        let plain = Dialect {
            structured_output: StructuredOutputMode::JsonObject,
            strict_tools: false,
            ..Dialect::default()
        };
        let v = serde_json::to_value(to_wire("m", plain, &r)?)?;
        assert_eq!(at(&v, "/response_format"), &json!({"type": "json_object"}));
        assert!(
            at(&v, "/tools/0/function").get("strict").is_none(),
            "strict off"
        );
        assert!(
            v.get("reasoning_effort").is_none(),
            "effort not sent unless the dialect says so"
        );
        assert_eq!(
            at(&v, "/messages/0"),
            &json!({"role": "system", "content": "sys"}),
            "no cache parts without the knob"
        );
        let prompt = Dialect {
            structured_output: StructuredOutputMode::Prompt,
            ..Dialect::default()
        };
        assert!(
            serde_json::to_value(to_wire("m", prompt, &r)?)?
                .get("response_format")
                .is_none()
        );
        let none = ChatRequest {
            tool_choice: ToolChoice::None,
            ..r.clone()
        };
        let v = serde_json::to_value(to_wire("m", Dialect::default(), &none)?)?;
        assert_eq!(at(&v, "/tool_choice"), "none");
        assert!(v.get("parallel_tool_calls").is_none());
        let parallel = ChatRequest {
            tool_choice: ToolChoice::Auto { parallel: true },
            ..r
        };
        assert_eq!(
            at(
                &serde_json::to_value(to_wire("m", Dialect::default(), &parallel)?)?,
                "/parallel_tool_calls"
            ),
            &Value::Bool(true)
        );
        for (e, w) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::Max, "high"),
        ] {
            let r = ChatRequest {
                effort: Some(e),
                ..req()
            };
            assert_eq!(
                at(
                    &serde_json::to_value(to_wire("m", d, &r)?)?,
                    "/reasoning_effort"
                ),
                w
            );
        }
        Ok(())
    }

    #[test]
    fn assistant_turns_replay_verbatim_and_foreign_ones_are_refused()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = json!({
            "role": "assistant", "content": null, "reasoning_content": "let me think",
            "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "lookup_rules", "arguments": "{\"ids\":[\"613\"]}"}}],
            "future_field": {"x": 1}
        });
        let r = ChatRequest {
            turns: vec![
                Turn::User(vec![TextBlock::plain("q")]),
                Turn::Assistant(AssistantTurn {
                    backend: BACKEND,
                    raw: message.clone(),
                }),
                Turn::ToolResults(vec![
                    ToolResult {
                        call_id: "call_1".into(),
                        content: "rules".into(),
                        is_error: false,
                    },
                    ToolResult {
                        call_id: "call_2".into(),
                        content: "more".into(),
                        is_error: true,
                    },
                ]),
            ],
            ..req()
        };
        let v = serde_json::to_value(to_wire("m", Dialect::default(), &r)?)?;
        assert_eq!(
            at(&v, "/messages/2"),
            &message,
            "byte-for-byte, reasoning_content and tool_calls included"
        );
        assert_eq!(
            at(&v, "/messages/3"),
            &json!({"role": "tool", "tool_call_id": "call_1", "content": "rules"})
        );
        assert_eq!(
            at(&v, "/messages/4"),
            &json!({"role": "tool", "tool_call_id": "call_2", "content": "more"})
        );

        let foreign = ChatRequest {
            turns: vec![Turn::Assistant(AssistantTurn {
                backend: "anthropic",
                raw: json!([]),
            })],
            ..req()
        };
        assert!(matches!(
            to_wire("m", Dialect::default(), &foreign),
            Err(LlmError::ForeignTurn {
                expected: BACKEND,
                found: "anthropic"
            })
        ));
        let garbage = ChatRequest {
            turns: vec![Turn::Assistant(AssistantTurn {
                backend: BACKEND,
                raw: json!([1]),
            })],
            ..req()
        };
        assert!(matches!(
            to_wire("m", Dialect::default(), &garbage),
            Err(LlmError::Request(_))
        ));
        Ok(())
    }

    fn resp(
        finish: Option<&str>,
        message: &Value,
    ) -> Result<wire::ChatResponse, serde_json::Error> {
        serde_json::from_value(json!({
            "id": "chatcmpl-1", "model": "served-model",
            "choices": [{"index": 0, "message": message, "finish_reason": finish}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 7, "prompt_tokens_details": {"cached_tokens": 60}}
        }))
    }

    #[test]
    fn responses_read_as_text_calls_stop_and_usage() -> Result<(), Box<dyn std::error::Error>> {
        let message = json!({
            "role": "assistant", "content": null, "reasoning_content": "hmm",
            "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "lookup_rules", "arguments": "{\"ids\": [\"613\"]}"}}]
        });
        let c = from_wire(&resp(Some("tool_calls"), &message)?, "asked")?;
        assert!(c.text.is_empty());
        assert_eq!(
            c.tool_calls,
            [ToolCall {
                id: "call_1".into(),
                name: "lookup_rules".into(),
                input: json!({"ids": ["613"]})
            }]
        );
        assert_eq!(c.stop, Stop::ToolUse);
        assert_eq!(
            c.usage,
            Usage {
                input: 40,
                output: 7,
                cache_read: 60,
                cache_write: 0
            },
            "cached tokens come out of the input"
        );
        assert_eq!(c.model, "served-model");
        assert_eq!(c.assistant.backend, BACKEND);
        assert_eq!(
            c.assistant.raw, message,
            "the whole message object, reasoning included"
        );

        let text = json!({"role": "assistant", "content": "{}"});
        for (finish, stop) in [
            (Some("stop"), Stop::EndTurn),
            (Some("length"), Stop::MaxTokens),
            (
                Some("content_filter"),
                Stop::Refusal(Refusal {
                    category: Some("content_filter".into()),
                    explanation: None,
                }),
            ),
            (Some("brand_new"), Stop::Other("brand_new".into())),
            (None, Stop::Other("none".into())),
        ] {
            let c = from_wire(&resp(finish, &text)?, "asked")?;
            assert_eq!(c.stop, stop, "{finish:?}");
            assert_eq!(c.text, ["{}"]);
        }
        // A refusal in the message wins over the finish reason.
        let refused =
            json!({"role": "assistant", "content": null, "refusal": "I can't help with that."});
        let c = from_wire(&resp(Some("stop"), &refused)?, "asked")?;
        assert_eq!(
            c.stop,
            Stop::Refusal(Refusal {
                category: None,
                explanation: Some("I can't help with that.".into())
            })
        );
        assert!(c.text.is_empty());
        // The Ollama shape: `stop` with tool calls is a tool round.
        let c = from_wire(&resp(Some("stop"), &message)?, "asked")?;
        assert_eq!(c.stop, Stop::ToolUse);
        // Content parts, and a server that names no model.
        let parts: wire::ChatResponse = serde_json::from_value(json!({
            "choices": [{"message": {"role": "assistant", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": ""}, {"type": "text", "text": "b"}]}, "finish_reason": "stop"}]
        }))?;
        let c = from_wire(&parts, "asked")?;
        assert_eq!(c.text, ["a", "b"]);
        assert_eq!(c.model, "asked");
        assert_eq!(c.usage, Usage::default());
        Ok(())
    }

    #[test]
    fn bad_arguments_and_missing_choices_are_decode_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let bad = json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c", "type": "function", "function": {"name": "lookup_rules", "arguments": "{not json"}}]});
        assert!(from_wire(&resp(Some("tool_calls"), &bad)?, "m").is_err());
        let none: wire::ChatResponse = serde_json::from_value(json!({"choices": []}))?;
        assert!(from_wire(&none, "m").is_err());
        let not_a_message: wire::ChatResponse =
            serde_json::from_value(json!({"choices": [{"message": 7}]}))?;
        assert!(from_wire(&not_a_message, "m").is_err());
        Ok(())
    }

    #[test]
    fn usage_of_reads_a_body_that_is_not_a_response() {
        let b = usage_of(br#"{"id": "x", "model": "m", "usage": {"prompt_tokens": 5, "completion_tokens": 1, "prompt_tokens_details": {"cached_tokens": 2}}}"#);
        assert_eq!(
            b,
            Some(Billed {
                model: Some("m".into()),
                usage: Usage {
                    input: 3,
                    output: 1,
                    cache_read: 2,
                    cache_write: 0
                }
            })
        );
        assert_eq!(usage_of(b"not json"), None);
        assert_eq!(usage_of(br#"{"id": "x"}"#), None);
    }
}
