//! `Synth<Fresh | ToolRequested | Final>` — invariant I4: the `lookup_rules`
//! tool round happens at most once. `Synth<Final>` has no method to request
//! tools again, and `finish` rejects a second tool call. After a tool round
//! `tool_choice` deliberately stays `auto`: changing it would invalidate the
//! prompt cache of the (large) user turn, and the system prompt already tells
//! the model it may call the tool once. `Synth::new_final` (used for the
//! citation retry) starts with `tool_choice: none`.
//!
//! Stage-dependent data lives *in the stage type* (`ToolRequested` owns the
//! tool-call ids and requested rule ids), so a `Synth<ToolRequested>` cannot
//! exist without them. The pure response-classification logic is `classify`,
//! unit-tested against [`ChatResponse`] fixtures without a live API. The
//! typestate is about one tool round, not about any provider: it runs over
//! whatever [`ChatModel`] it is given.

use std::sync::Arc;

use anyhow::Context as _;
use judge_core::{JudgeError, RuleChunk, RuleId, Unvalidated, Verdict};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AssistantTurn, ChatModel, ChatRequest, ChatResponse, Effort, OutputSchema, RefusalFallback, Stop, TextBlock,
    ToolChoice, ToolResult, ToolSpec, Turn, schema_of,
};

/// Name of the one tool the synthesizer may call.
pub const LOOKUP_RULES: &str = "lookup_rules";

/// Input schema of `lookup_rules`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LookupRulesInput {
    /// CR rule or subsection ids to fetch, e.g. `613` or `702.19b`.
    pub ids: Vec<RuleId>,
}

mod sealed {
    pub trait Sealed {}
}
/// Typestate marker for `Synth`. Sealed.
pub trait Stage: sealed::Sealed + core::fmt::Debug {}

/// Nothing sent yet.
#[derive(Debug)]
pub struct Fresh;
/// The model asked for rules; exactly one `answer_tool` is allowed.
#[derive(Debug)]
pub struct ToolRequested {
    /// Tool-call ids to answer (one each; several only if the model ignored
    /// the single-call request).
    call_ids: Vec<String>,
    /// Union of the ids the model asked for.
    requested: Vec<RuleId>,
}
/// Tool result attached; only `finish` remains.
#[derive(Debug)]
pub struct Final;
impl sealed::Sealed for Fresh {}
impl sealed::Sealed for ToolRequested {}
impl sealed::Sealed for Final {}
impl Stage for Fresh {}
impl Stage for ToolRequested {}
impl Stage for Final {}

/// Knobs for the synthesis request. The model itself is the backend's.
#[derive(Clone, Debug)]
pub struct SynthConfig {
    /// Output ceiling; keep ≤ 16k while the backends are non-streaming.
    pub max_tokens: u32,
    /// How hard the model thinks.
    pub effort: Effort,
    /// Server-side refusal fallback, where the backend supports it.
    pub fallbacks: Option<RefusalFallback>,
    /// Bytes of rule text inlined into a `lookup_rules` tool result; a whole
    /// subsection such as `702` would otherwise inject tens of thousands of
    /// tokens. Chunks past the cap are replaced by an "omitted" line.
    pub max_tool_result_chars: usize,
}

impl Default for SynthConfig {
    fn default() -> Self {
        Self { max_tokens: 16_000, effort: Effort::High, fallbacks: Some(RefusalFallback::Default), max_tool_result_chars: 30_000 }
    }
}

/// The response hit `max_tokens` (reasoning tokens count against it).
/// Carried inside `JudgeError::Upstream` so a caller can downcast and retry
/// at a lower effort.
#[derive(Debug, thiserror::Error)]
#[error("response truncated at max_tokens ({output_tokens} output tokens)")]
pub struct Truncated {
    /// `usage.output` of the truncated response.
    pub output_tokens: u64,
}

/// Result of the first send.
// One-shot value matched immediately by the caller; boxing the Synth buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum SendOutcome {
    /// The model wants rules; answer with `answer_tool`.
    ToolRequested(Synth<ToolRequested>),
    /// The model answered directly.
    Done(Verdict<Unvalidated>),
}

/// One synthesis conversation. `S` is the stage.
pub struct Synth<S: Stage> {
    model: Arc<dyn ChatModel>,
    cfg: SynthConfig,
    req: ChatRequest,
    stage: S,
}

impl<S: Stage> std::fmt::Debug for Synth<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Synth")
            .field("model", &self.model.model())
            .field("cfg", &self.cfg)
            .field("req", &self.req)
            .field("stage", &self.stage)
            .finish()
    }
}

/// The first request: cached system prompt, the caller's user-turn blocks
/// (the caller puts a cache hint on the material block), the `lookup_rules`
/// tool (single call per turn), extended reasoning, and the `Verdict` schema
/// as structured output.
fn first_request(cfg: &SynthConfig, system: String, user: Vec<TextBlock>, tool_choice: ToolChoice) -> ChatRequest {
    ChatRequest {
        max_tokens: cfg.max_tokens,
        system: vec![TextBlock::cached(system)],
        turns: vec![Turn::User(user)],
        tools: vec![ToolSpec {
            name: LOOKUP_RULES.to_owned(),
            description: "Fetch additional Comprehensive Rules text by id: rule ids such as \"613.7\" or \"702.19b\", \
                          or a whole subsection such as \"613\". You may call this at most once, before answering, if \
                          the provided rules are insufficient. Prefer specific rule ids: the result is truncated after \
                          a fixed amount of text, so a large subsection (e.g. \"702\", \"701\", \"800\") comes back \
                          incomplete."
                .to_owned(),
            input_schema: schema_of::<LookupRulesInput>(),
            strict: true,
        }],
        tool_choice,
        output: Some(OutputSchema::of::<Verdict>()),
        effort: Some(cfg.effort),
        thinking: true,
        fallbacks: cfg.fallbacks.clone(),
    }
}

impl<S: Stage> Synth<S> {
    /// The request as it stands (tests and diagnostics).
    #[must_use]
    pub fn request(&self) -> &ChatRequest {
        &self.req
    }

    /// One round trip, logging output tokens against `max_tokens` so the
    /// truncation margin is observable.
    async fn round_trip(&self) -> Result<ChatResponse, JudgeError> {
        let resp = self.model.complete(&self.req).await.map_err(anyhow::Error::from)?;
        tracing::info!(output_tokens = resp.usage.output, max_tokens = self.req.max_tokens, stop = ?resp.stop, "synthesis response");
        Ok(resp)
    }
}

impl Synth<Fresh> {
    /// Build the first request (see [`first_request`]) with the tool allowed.
    #[must_use]
    pub fn new(model: Arc<dyn ChatModel>, cfg: &SynthConfig, system: impl Into<String>, user: Vec<TextBlock>) -> Self {
        let req = first_request(cfg, system.into(), user, ToolChoice::Auto { parallel: false });
        Self { model, cfg: cfg.clone(), req, stage: Fresh }
    }

    /// # Errors
    /// `LlmRefused` on a refusal, `Upstream` on transport/parse problems or
    /// an unexpected stop reason.
    pub async fn send(mut self) -> Result<SendOutcome, JudgeError> {
        let resp = self.round_trip().await?;
        match classify(resp)? {
            Step::Verdict(v) => Ok(SendOutcome::Done(v)),
            Step::Tool { call_ids, requested, assistant } => {
                self.req.turns.push(Turn::Assistant(assistant));
                Ok(SendOutcome::ToolRequested(Synth {
                    model: self.model,
                    cfg: self.cfg,
                    req: self.req,
                    stage: ToolRequested { call_ids, requested },
                }))
            }
        }
    }
}

impl Synth<ToolRequested> {
    /// Ids the model asked for.
    #[must_use]
    pub fn requested(&self) -> &[RuleId] {
        &self.stage.requested
    }

    /// Attach the fetched chunks as the tool result, capped at
    /// `SynthConfig::max_tool_result_chars`. `tool_choice` is left as is so
    /// the cached user turn stays valid; `finish` rejects a second tool call.
    #[must_use]
    pub fn answer_tool(mut self, chunks: &[RuleChunk]) -> Synth<Final> {
        let content = render_tool_result(chunks, self.cfg.max_tool_result_chars);
        // Every call id must be answered in the same turn.
        let results = self
            .stage
            .call_ids
            .into_iter()
            .map(|call_id| ToolResult { call_id, content: content.clone(), is_error: false })
            .collect();
        self.req.turns.push(Turn::ToolResults(results));
        Synth { model: self.model, cfg: self.cfg, req: self.req, stage: Final }
    }
}

/// The tool result text: chunks in order until `max_chars` of rule text has
/// been inlined, then one line naming how many were left out.
#[must_use]
pub fn render_tool_result(chunks: &[RuleChunk], max_chars: usize) -> String {
    if chunks.is_empty() {
        return "No rules found for the requested ids.".to_owned();
    }
    let mut parts = Vec::with_capacity(chunks.len());
    let mut used = 0usize;
    let mut omitted = 0usize;
    for c in chunks {
        let size = c.body.len() + c.examples.iter().map(String::len).sum::<usize>();
        // A prefix in id order, so the "omitted" count is honest and the model can ask for the rest by id.
        if omitted == 0 && (parts.is_empty() || used + size <= max_chars) {
            used += size;
            parts.push(format!("[{}] {}\n{}\n{}", c.id, c.heading, c.body, c.examples.join("\n")));
        } else {
            omitted += 1;
        }
    }
    if omitted > 0 {
        tracing::warn!(shown = parts.len(), omitted, "lookup_rules result truncated");
        parts.push(format!("({omitted} more rules omitted: the request was too broad; cite only what is shown)"));
    }
    parts.join("\n\n")
}

impl Synth<Final> {
    /// A conversation that may not call the tool at all (`tool_choice: none`):
    /// the citation retry, whose Context already holds the earlier tool round.
    #[must_use]
    pub fn new_final(model: Arc<dyn ChatModel>, cfg: &SynthConfig, system: impl Into<String>, user: Vec<TextBlock>) -> Self {
        let req = first_request(cfg, system.into(), user, ToolChoice::None);
        Self { model, cfg: cfg.clone(), req, stage: Final }
    }

    /// # Errors
    /// `LlmRefused`, or `Upstream` if the model tries to call a tool again or returns bad JSON.
    pub async fn finish(self) -> Result<Verdict<Unvalidated>, JudgeError> {
        let resp = self.round_trip().await?;
        match classify(resp)? {
            Step::Verdict(v) => Ok(v),
            Step::Tool { .. } => Err(anyhow::anyhow!("model requested a second tool round; not allowed").into()),
        }
    }
}

/// What a response asks the caller to do next.
#[derive(Debug)]
pub enum Step {
    /// Answer these tool calls with rules for `requested`; `assistant` is
    /// the model's turn to replay.
    Tool {
        /// Tool-call ids.
        call_ids: Vec<String>,
        /// Union of requested rule ids, deduplicated.
        requested: Vec<RuleId>,
        /// The full assistant turn (reasoning included), for the continuation.
        assistant: AssistantTurn,
    },
    /// The model's final structured answer.
    Verdict(Verdict<Unvalidated>),
}

/// Pure decision logic over stop reason × content. Exhaustive so that a new
/// [`Stop`] variant is a compile error here rather than a silent fall-through.
///
/// # Errors
/// `LlmRefused` for a refusal; `Upstream` for truncation, unknown tools,
/// unexpected stop reasons and JSON that does not match the schema.
pub fn classify(resp: ChatResponse) -> Result<Step, JudgeError> {
    match resp.stop {
        Stop::Refusal(details) => {
            tracing::warn!(?details, "model refused");
            Err(JudgeError::LlmRefused)
        }
        Stop::MaxTokens => Err(anyhow::Error::from(Truncated { output_tokens: resp.usage.output }).into()),
        Stop::ToolUse => {
            let mut call_ids = Vec::new();
            let mut requested: Vec<RuleId> = Vec::new();
            for call in &resp.tool_calls {
                if call.name != LOOKUP_RULES {
                    return Err(anyhow::anyhow!("model requested an unknown tool: {}", call.name).into());
                }
                let parsed: LookupRulesInput =
                    serde_json::from_value(call.input.clone()).context("lookup_rules input did not match schema")?;
                call_ids.push(call.id.clone());
                for r in parsed.ids {
                    if !requested.contains(&r) {
                        requested.push(r);
                    }
                }
            }
            if call_ids.is_empty() {
                return Err(anyhow::anyhow!("tool-use stop without a tool call").into());
            }
            Ok(Step::Tool { call_ids, requested, assistant: resp.assistant })
        }
        Stop::EndTurn => {
            if !resp.tool_calls.is_empty() {
                return Err(anyhow::anyhow!("tool call with an end-of-turn stop").into());
            }
            parse_verdict(&resp).map(Step::Verdict)
        }
        Stop::Other(reason) => Err(anyhow::anyhow!("unexpected stop reason {reason:?}").into()),
    }
}

/// With structured outputs the JSON is the *last* text block; any earlier
/// text blocks are prose and are logged, not parsed.
fn parse_verdict(resp: &ChatResponse) -> Result<Verdict<Unvalidated>, JudgeError> {
    if let Some((_, prose)) = resp.text.split_last() {
        for t in prose {
            tracing::debug!(text = t, "ignoring prose text block before the structured output");
        }
    }
    let text = resp.last_text().ok_or_else(|| anyhow::anyhow!("response contained no text block"))?;
    tracing::debug!(raw = %crate::truncate_for_log(text, crate::LOG_TEXT_CHARS), "synthesis raw model text");
    serde_json::from_str(text)
        // Bounded: this string becomes the `Upstream` error chain, which the
        // adapters log at warn. `max_tokens` is 16k, so an unbounded `{text}`
        // could write ~64 KB to an unrotated container log per request.
        .with_context(|| format!("verdict JSON did not match schema: {}", crate::truncate_for_log(text, crate::LOG_TEXT_CHARS)))
        .map_err(JudgeError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Backend, Capabilities, LlmError, Metered, Price, Refusal, SpendMeter, StructuredOutput, ToolCall, Usage};
    use async_trait::async_trait;
    use serde_json::{Value, json};

    /// Never sends: these tests are about the requests the typestate builds.
    struct Unreachable;

    #[async_trait]
    impl Backend for Unreachable {
        async fn complete(&self, _req: &ChatRequest) -> Result<ChatResponse, LlmError> {
            Err(LlmError::Request("not sent".into()))
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities { structured_output: StructuredOutput::Enforced, strict_tools: true, effort: true, cache_hints: true, refusal_fallbacks: true }
        }
        fn provider(&self) -> &'static str {
            "test"
        }
        fn model(&self) -> &'static str {
            "m"
        }
    }

    fn model() -> Arc<dyn ChatModel> {
        Arc::new(Metered::priced(Unreachable, SpendMeter::new(), Price::Free))
    }

    fn resp(stop: Stop, text: &[&str], tool_calls: Vec<ToolCall>) -> ChatResponse {
        ChatResponse {
            text: text.iter().map(|t| (*t).to_owned()).collect(),
            tool_calls,
            stop,
            usage: Usage { input: 1, output: 1, cache_read: 0, cache_write: 0 },
            model: "m".into(),
            assistant: AssistantTurn { backend: "test", raw: json!(["replayed"]) },
        }
    }

    fn call(id: &str, name: &str, input: Value) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), input }
    }

    const VERDICT: &str = r#"{"answer":"a","confidence":"low","citations":[{"kind":"rule","id":"702.15b","quote":"q"}],"category":"layers"}"#;

    #[test]
    fn lookup_rules_schema_requires_ids() {
        let s = schema_of::<LookupRulesInput>().to_value();
        assert_eq!(s.pointer("/required"), Some(&json!(["ids"])));
        assert_eq!(s.pointer("/additionalProperties"), Some(&Value::Bool(false)), "deny_unknown_fields closes it");
    }

    #[test]
    fn fresh_request_has_tool_thinking_format_and_fallbacks() {
        let s = Synth::new(model(), &SynthConfig::default(), "sys", vec![TextBlock::plain("q")]);
        let r = s.request();
        assert_eq!(r.max_tokens, 16_000);
        assert_eq!(r.system, [TextBlock::cached("sys")]);
        assert_eq!(r.tools.first().map(|t| t.name.as_str()), Some(LOOKUP_RULES));
        assert!(r.tools.first().is_some_and(|t| t.strict));
        assert_eq!(r.tool_choice, ToolChoice::Auto { parallel: false });
        assert!(r.thinking);
        assert!(r.output.is_some());
        assert_eq!(r.effort, Some(Effort::High));
        assert_eq!(r.fallbacks, Some(RefusalFallback::Default));

        let cfg = SynthConfig { fallbacks: None, ..SynthConfig::default() };
        let s = Synth::new(model(), &cfg, "sys", vec![TextBlock::plain("q")]);
        assert!(s.request().fallbacks.is_none());

        let f = Synth::new_final(model(), &cfg, "sys", vec![TextBlock::plain("q")]);
        assert_eq!(f.request().tool_choice, ToolChoice::None);
        assert_eq!(f.request().tools.first().map(|t| t.name.as_str()), Some(LOOKUP_RULES), "the tool stays listed");
    }

    fn chunk(id: &str, body: &str) -> Result<RuleChunk, Box<dyn std::error::Error>> {
        Ok(RuleChunk {
            id: RuleId::try_new(id.to_owned())?,
            parent_id: None,
            subsection: RuleId::try_new("613".to_owned())?,
            heading: "H".into(),
            body: body.into(),
            examples: vec![],
            cr_version: judge_core::CrVersion::try_new("20250801".to_owned())?,
        })
    }

    #[test]
    fn answer_tool_answers_every_id_and_keeps_tool_choice() -> Result<(), Box<dyn std::error::Error>> {
        let s = Synth::new(model(), &SynthConfig::default(), "sys", vec![TextBlock::plain("q")]);
        let t: Synth<ToolRequested> = Synth {
            model: s.model,
            cfg: s.cfg,
            req: s.req,
            stage: ToolRequested { call_ids: vec!["tu_1".into(), "tu_2".into()], requested: vec![RuleId::try_new("613".to_owned())?] },
        };
        assert_eq!(t.requested().len(), 1);
        let f = t.answer_tool(&[]);
        assert_eq!(f.request().tool_choice, ToolChoice::Auto { parallel: false }, "cache-preserving");
        let Some(Turn::ToolResults(results)) = f.request().turns.last() else {
            return Err("expected a tool-results turn".into());
        };
        let ids: Vec<&str> = results.iter().map(|r| r.call_id.as_str()).collect();
        assert_eq!(ids, ["tu_1", "tu_2"]);
        assert!(results.iter().all(|r| r.content == "No rules found for the requested ids." && !r.is_error));
        Ok(())
    }

    #[test]
    fn tool_result_is_capped() -> Result<(), Box<dyn std::error::Error>> {
        let chunks = vec![chunk("613.1", &"a".repeat(60))?, chunk("613.2", &"b".repeat(60))?, chunk("613.3", "c")?];
        let full = render_tool_result(&chunks, 1_000);
        assert!(full.contains("[613.3]") && !full.contains("omitted"), "{full}");
        let capped = render_tool_result(&chunks, 100);
        assert!(capped.contains("[613.1]") && !capped.contains("[613.2]"), "{capped}");
        assert!(capped.ends_with("(2 more rules omitted: the request was too broad; cite only what is shown)"), "{capped}");
        // An over-sized first chunk is still shown.
        let one = render_tool_result(&chunks, 10);
        assert!(one.contains("[613.1]"), "{one}");
        Ok(())
    }

    #[test]
    fn classify_plain_verdict() -> Result<(), Box<dyn std::error::Error>> {
        let r = resp(Stop::EndTurn, &[VERDICT], vec![]);
        assert!(matches!(classify(r)?, Step::Verdict(v) if v.answer() == "a"));
        Ok(())
    }

    #[test]
    fn classify_uses_last_text_block() -> Result<(), Box<dyn std::error::Error>> {
        let r = resp(Stop::EndTurn, &["Sure, here it is:", VERDICT], vec![]);
        assert!(matches!(classify(r)?, Step::Verdict(_)));
        Ok(())
    }

    #[test]
    fn classify_tool_round_unions_ids_and_keeps_the_turn() -> Result<(), Box<dyn std::error::Error>> {
        let r = resp(
            Stop::ToolUse,
            &[],
            vec![
                call("tu_1", "lookup_rules", json!({"ids": ["613", "614"]})),
                call("tu_2", "lookup_rules", json!({"ids": ["614", "702.19b"]})),
            ],
        );
        let Step::Tool { call_ids, requested, assistant } = classify(r)? else {
            return Err("expected tool step".into());
        };
        assert_eq!(call_ids, ["tu_1", "tu_2"]);
        let ids: Vec<&str> = requested.iter().map(AsRef::as_ref).collect();
        assert_eq!(ids, ["613", "614", "702.19b"]);
        assert_eq!(assistant.raw, json!(["replayed"]));
        Ok(())
    }

    #[test]
    fn classify_errors() {
        let refusal = resp(Stop::Refusal(Refusal { category: Some("cyber".into()), explanation: None }), &[], vec![]);
        assert!(matches!(classify(refusal), Err(JudgeError::LlmRefused)));

        let truncated = resp(Stop::MaxTokens, &["{\"answer\": \"a"], vec![]);
        assert!(matches!(classify(truncated), Err(JudgeError::Upstream(e)) if e.downcast_ref::<Truncated>().is_some_and(|t| t.output_tokens == 1)));

        let unknown_tool = resp(Stop::ToolUse, &[], vec![call("t", "other", json!({}))]);
        assert!(matches!(classify(unknown_tool), Err(JudgeError::Upstream(_))));

        let bad_input = resp(Stop::ToolUse, &[], vec![call("t", "lookup_rules", json!({"ids": ["abc"]}))]);
        assert!(matches!(classify(bad_input), Err(JudgeError::Upstream(_))));

        let no_call = resp(Stop::ToolUse, &["x"], vec![]);
        assert!(matches!(classify(no_call), Err(JudgeError::Upstream(_))));

        let stray_tool = resp(Stop::EndTurn, &[VERDICT], vec![call("t", "lookup_rules", json!({"ids": []}))]);
        assert!(matches!(classify(stray_tool), Err(JudgeError::Upstream(_))));

        for stop in ["pause_turn", "something_new"] {
            let r = resp(Stop::Other(stop.into()), &[VERDICT], vec![]);
            assert!(matches!(classify(r), Err(JudgeError::Upstream(_))), "{stop}");
        }

        let bad_json = resp(Stop::EndTurn, &["{\"answer\":1}"], vec![]);
        assert!(matches!(classify(bad_json), Err(JudgeError::Upstream(_))));

        let no_text = resp(Stop::EndTurn, &[], vec![]);
        assert!(matches!(classify(no_text), Err(JudgeError::Upstream(_))));
    }

    /// The parse-failure context ends up in the `Upstream` chain, which the
    /// adapters log at warn. `max_tokens` is 16k, so an unbounded copy of the
    /// model text would write tens of KB to an unrotated container log per
    /// failed request.
    #[test]
    fn a_parse_failure_does_not_log_the_whole_model_response() -> Result<(), Box<dyn std::error::Error>> {
        let huge = format!(r#"{{"answer":"{}","citations":1}}"#, "はい".repeat(20_000));
        let r = resp(Stop::EndTurn, &[&huge], vec![]);
        let Err(JudgeError::Upstream(e)) = classify(r) else {
            return Err("expected an Upstream parse failure".into());
        };
        let logged = format!("{e:#}");
        assert!(logged.contains("verdict JSON did not match schema"), "{logged}");
        // Bounded by LOG_TEXT_CHARS, and cut on a char boundary (the text is
        // multi-byte, so a byte-wise cut would have panicked before this).
        assert!(logged.chars().count() < crate::LOG_TEXT_CHARS + 200, "{} chars", logged.chars().count());
        assert!(logged.contains('…'), "{logged}");
        Ok(())
    }
}
