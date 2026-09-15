//! Neutral ↔ Messages API. The only place that knows how a
//! [`ChatRequest`] is spelled on Anthropic's wire, and how a
//! [`MessagesResponse`] reads as a [`ChatResponse`]. Pure: unit-tested
//! against JSON shapes, no network.

use judge_llm::{
    AssistantTurn, Billed, CacheHint, ChatRequest, ChatResponse, Effort, LlmError, Refusal,
    RefusalFallback, Stop, TextBlock, ToolCall, ToolChoice, Turn, Usage,
};

use crate::{
    schema::to_anthropic,
    wire::{
        self, CacheControl, CacheTtl, ContentBlock, FallbackModel, Fallbacks, Message,
        MessagesRequest, MessagesResponse, ModelField, OutputConfig, OutputFormat, Role,
        StopReason, SystemBlock, Thinking, Tool,
    },
};

/// The [`AssistantTurn::backend`] tag of turns this backend produces.
pub const BACKEND: &str = "anthropic";

/// The Messages API body for `req` against `model` (a bare `&str` is the
/// first-party `"model"` field; [`ModelField::in_url`] is Vertex's shape).
///
/// # Errors
/// `ForeignTurn` when a replayed assistant turn came from another backend;
/// `Request` when its payload is not Messages API content.
pub fn to_wire(
    model: impl Into<ModelField>,
    req: &ChatRequest,
) -> Result<MessagesRequest, LlmError> {
    let messages = req
        .turns
        .iter()
        .map(message)
        .collect::<Result<Vec<_>, _>>()?;
    let output_config = (req.effort.is_some() || req.output.is_some()).then(|| OutputConfig {
        effort: req.effort.map(effort),
        format: req.output.as_ref().map(|o| OutputFormat::JsonSchema {
            schema: to_anthropic(&o.schema),
        }),
    });
    Ok(MessagesRequest {
        model: model.into(),
        max_tokens: req.max_tokens,
        system: req.system.iter().map(system_block).collect(),
        messages,
        tools: req
            .tools
            .iter()
            .map(|t| Tool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: to_anthropic(&t.input_schema),
                strict: t.strict.then_some(true),
                cache_control: None,
            })
            .collect(),
        // Meaningless without tools; the API rejects it there.
        tool_choice: (!req.tools.is_empty()).then(|| tool_choice(req.tool_choice)),
        thinking: req.thinking.then(Thinking::adaptive),
        output_config,
        fallbacks: req.fallbacks.as_ref().map(fallbacks),
    })
}

/// The beta flags `req` needs on top of the client's own.
#[must_use]
pub fn betas_for(req: &ChatRequest) -> Vec<&'static str> {
    req.fallbacks.iter().map(|_| Fallbacks::BETA).collect()
}

fn system_block(b: &TextBlock) -> SystemBlock {
    SystemBlock::Text {
        text: b.text.clone(),
        cache_control: b.cache.map(cache_control),
    }
}

fn cache_control(hint: CacheHint) -> CacheControl {
    match hint {
        CacheHint::Short => CacheControl::ephemeral(),
        CacheHint::Long => CacheControl::with_ttl(CacheTtl::OneHour),
    }
}

fn message(turn: &Turn) -> Result<Message, LlmError> {
    Ok(match turn {
        Turn::User(blocks) => Message {
            role: Role::User,
            content: blocks
                .iter()
                .map(|b| ContentBlock::Text {
                    text: b.text.clone(),
                    cache_control: b.cache.map(cache_control),
                })
                .collect(),
        },
        Turn::Assistant(AssistantTurn { backend, raw }) => {
            if *backend != BACKEND {
                return Err(LlmError::ForeignTurn {
                    expected: BACKEND,
                    found: backend,
                });
            }
            let content: Vec<ContentBlock> = serde_json::from_value(raw.clone()).map_err(|e| {
                LlmError::Request(format!("assistant turn is not Messages API content: {e}"))
            })?;
            Message {
                role: Role::Assistant,
                content,
            }
        }
        Turn::ToolResults(results) => Message {
            role: Role::User,
            content: results
                .iter()
                .map(|r| ContentBlock::ToolResult {
                    tool_use_id: r.call_id.clone(),
                    content: r.content.clone(),
                    is_error: r.is_error,
                })
                .collect(),
        },
    })
}

fn tool_choice(choice: ToolChoice) -> wire::ToolChoice {
    match choice {
        ToolChoice::Auto { parallel: false } => wire::ToolChoice::auto_single(),
        ToolChoice::Auto { parallel: true } => wire::ToolChoice::Auto {
            disable_parallel_tool_use: None,
        },
        ToolChoice::None => wire::ToolChoice::None,
    }
}

fn effort(e: Effort) -> wire::Effort {
    match e {
        Effort::Low => wire::Effort::Low,
        Effort::Medium => wire::Effort::Medium,
        Effort::High => wire::Effort::High,
        Effort::XHigh => wire::Effort::Xhigh,
        Effort::Max => wire::Effort::Max,
    }
}

fn fallbacks(f: &RefusalFallback) -> Fallbacks {
    match f {
        RefusalFallback::Default => Fallbacks::default_mode(),
        RefusalFallback::Models(models) => Fallbacks::Models(
            models
                .iter()
                .map(|m| FallbackModel { model: m.clone() })
                .collect(),
        ),
    }
}

/// Read a Messages API response as the neutral response. The assistant turn
/// is the whole content array (thinking blocks and all), so a continuation
/// replays it verbatim.
///
/// # Errors
/// If the content array cannot be re-serialized for that replay (no
/// [`ContentBlock`] fails today; reported here, against the response it
/// came from, rather than one request later).
pub fn from_wire(resp: &MessagesResponse) -> Result<ChatResponse, serde_json::Error> {
    let stop = match resp.stop_reason {
        Some(StopReason::EndTurn | StopReason::StopSequence) => Stop::EndTurn,
        Some(StopReason::MaxTokens) => Stop::MaxTokens,
        Some(StopReason::ToolUse) => Stop::ToolUse,
        Some(StopReason::Refusal) => Stop::Refusal(
            resp.stop_details
                .as_ref()
                .map(|d| Refusal {
                    category: d.category.clone(),
                    explanation: d.explanation.clone(),
                })
                .unwrap_or_default(),
        ),
        Some(StopReason::PauseTurn) => Stop::Other("pause_turn".into()),
        Some(StopReason::Unknown) => Stop::Other("unknown".into()),
        None => Stop::Other("none".into()),
    };
    Ok(ChatResponse {
        text: resp.text_blocks().map(str::to_owned).collect(),
        tool_calls: resp
            .tool_uses()
            .map(|(id, name, input)| ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                input: input.clone(),
            })
            .collect(),
        stop,
        usage: usage(&resp.usage),
        model: resp.model.clone(),
        assistant: AssistantTurn {
            backend: BACKEND,
            raw: serde_json::to_value(&resp.content)?,
        },
    })
}

fn usage(u: &wire::Usage) -> Usage {
    Usage {
        input: u.input_tokens,
        output: u.output_tokens,
        cache_read: u.cache_read_input_tokens.unwrap_or(0),
        cache_write: u.cache_creation_input_tokens.unwrap_or(0),
    }
}

/// `usage` (and `model`) of a 2xx body, parsed leniently so that a response
/// which fails to decode as [`MessagesResponse`] is still billed.
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
    use serde_json::{Value, json};

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
        let v = serde_json::to_value(to_wire("claude-opus-5", &req())?)?;
        assert_eq!(
            v,
            json!({
                "model": "claude-opus-5", "max_tokens": 64,
                "system": [{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}],
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "material", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "question"}
                ]}]
            })
        );
        assert!(betas_for(&req()).is_empty());
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
            fallbacks: Some(RefusalFallback::Default),
            ..req()
        };
        let v = serde_json::to_value(to_wire("m", &r)?)?;
        assert_eq!(at(&v, "/tools/0/strict"), &Value::Bool(true));
        assert_eq!(
            at(&v, "/tools/0/input_schema/additionalProperties"),
            &Value::Bool(false),
            "the subset transform ran"
        );
        assert!(
            !at(&v, "/tools/0/input_schema")
                .to_string()
                .contains("pattern"),
            "RuleId's regex is stripped from the tool schema"
        );
        assert_eq!(
            at(&v, "/tool_choice"),
            &json!({"type": "auto", "disable_parallel_tool_use": true})
        );
        assert_eq!(at(&v, "/thinking"), &json!({"type": "adaptive"}));
        assert_eq!(at(&v, "/output_config/effort"), "xhigh");
        assert_eq!(at(&v, "/output_config/format/type"), "json_schema");
        assert!(
            !at(&v, "/output_config/format/schema")
                .to_string()
                .contains("oneOf"),
            "the subset transform ran"
        );
        assert_eq!(at(&v, "/fallbacks"), "default");
        assert_eq!(betas_for(&r), [Fallbacks::BETA]);

        let none = ChatRequest {
            tool_choice: ToolChoice::None,
            ..r.clone()
        };
        assert_eq!(
            at(&serde_json::to_value(to_wire("m", &none)?)?, "/tool_choice"),
            &json!({"type": "none"})
        );
        let parallel = ChatRequest {
            tool_choice: ToolChoice::Auto { parallel: true },
            ..r.clone()
        };
        assert_eq!(
            at(
                &serde_json::to_value(to_wire("m", &parallel)?)?,
                "/tool_choice"
            ),
            &json!({"type": "auto"})
        );
        let models = ChatRequest {
            fallbacks: Some(RefusalFallback::Models(vec!["claude-opus-4-8".into()])),
            ..r
        };
        assert_eq!(
            at(&serde_json::to_value(to_wire("m", &models)?)?, "/fallbacks"),
            &json!([{"model": "claude-opus-4-8"}])
        );
        Ok(())
    }

    #[test]
    fn assistant_turns_replay_verbatim_and_foreign_ones_are_refused()
    -> Result<(), Box<dyn std::error::Error>> {
        let content = json!([
            {"type": "thinking", "thinking": "", "signature": "sig"},
            {"type": "redacted_thinking", "data": "opaque"},
            {"type": "future_block", "payload": 1},
            {"type": "tool_use", "id": "tu_1", "name": "lookup_rules", "input": {"ids": ["613"]}}
        ]);
        let r = ChatRequest {
            turns: vec![
                Turn::User(vec![TextBlock::plain("q")]),
                Turn::Assistant(AssistantTurn {
                    backend: BACKEND,
                    raw: content.clone(),
                }),
                Turn::ToolResults(vec![ToolResult {
                    call_id: "tu_1".into(),
                    content: "rules".into(),
                    is_error: false,
                }]),
            ],
            ..req()
        };
        let v = serde_json::to_value(to_wire("m", &r)?)?;
        assert_eq!(
            at(&v, "/messages/1"),
            &json!({"role": "assistant", "content": content})
        );
        assert_eq!(
            at(&v, "/messages/2"),
            &json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tu_1", "content": "rules"}]})
        );

        let foreign = ChatRequest {
            turns: vec![Turn::Assistant(AssistantTurn {
                backend: "openai",
                raw: json!({}),
            })],
            ..req()
        };
        assert!(matches!(
            to_wire("m", &foreign),
            Err(LlmError::ForeignTurn {
                expected: BACKEND,
                found: "openai"
            })
        ));
        let garbage = ChatRequest {
            turns: vec![Turn::Assistant(AssistantTurn {
                backend: BACKEND,
                raw: json!(7),
            })],
            ..req()
        };
        assert!(matches!(to_wire("m", &garbage), Err(LlmError::Request(_))));
        Ok(())
    }

    fn resp(stop: &str, content: &Value) -> Result<MessagesResponse, serde_json::Error> {
        serde_json::from_value(json!({
            "id": "m", "model": "claude-opus-5", "role": "assistant",
            "content": content, "stop_reason": stop,
            "stop_details": if stop == "refusal" { json!({"type": "refusal", "category": "cyber"}) } else { Value::Null },
            "usage": {"input_tokens": 1, "output_tokens": 2, "cache_read_input_tokens": 3}
        }))
    }

    #[test]
    fn responses_read_as_text_calls_stop_and_usage() -> Result<(), Box<dyn std::error::Error>> {
        let content = json!([
            {"type": "thinking", "thinking": "", "signature": "s"},
            {"type": "text", "text": "prose"},
            {"type": "tool_use", "id": "tu_1", "name": "lookup_rules", "input": {"ids": ["613"]}},
            {"type": "text", "text": "{}"}
        ]);
        let c = from_wire(&resp("tool_use", &content)?)?;
        assert_eq!(c.text, ["prose", "{}"]);
        assert_eq!(
            c.tool_calls,
            [ToolCall {
                id: "tu_1".into(),
                name: "lookup_rules".into(),
                input: json!({"ids": ["613"]})
            }]
        );
        assert_eq!(c.stop, Stop::ToolUse);
        assert_eq!(
            c.usage,
            Usage {
                input: 1,
                output: 2,
                cache_read: 3,
                cache_write: 0
            }
        );
        assert_eq!(c.model, "claude-opus-5");
        assert_eq!(c.assistant.backend, BACKEND);
        assert_eq!(
            c.assistant.raw, content,
            "the whole content array, thinking included"
        );

        for (wire, stop) in [
            ("end_turn", Stop::EndTurn),
            ("stop_sequence", Stop::EndTurn),
            ("max_tokens", Stop::MaxTokens),
            ("pause_turn", Stop::Other("pause_turn".into())),
            ("brand_new", Stop::Other("unknown".into())),
            (
                "refusal",
                Stop::Refusal(Refusal {
                    category: Some("cyber".into()),
                    explanation: None,
                }),
            ),
        ] {
            assert_eq!(from_wire(&resp(wire, &json!([]))?)?.stop, stop, "{wire}");
        }
        Ok(())
    }

    #[test]
    fn usage_of_reads_a_body_that_is_not_a_response() {
        let b = usage_of(
            br#"{"id": "m", "model": "x", "usage": {"input_tokens": 5, "output_tokens": 0}}"#,
        );
        assert_eq!(
            b,
            Some(Billed {
                model: Some("x".into()),
                usage: Usage {
                    input: 5,
                    ..Usage::default()
                }
            })
        );
        assert_eq!(usage_of(b"not json"), None);
        assert_eq!(usage_of(br#"{"id": "m"}"#), None);
    }
}
