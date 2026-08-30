//! `Synth<Fresh | ToolRequested | Final>` — invariant I4: the `lookup_rules`
//! tool round happens at most once. `Synth<Final>` has no method to request
//! tools again, and `finish` rejects a second `tool_use`. After a tool round
//! `tool_choice` deliberately stays `auto`: changing it would invalidate the
//! prompt cache of the (large) user turn, and the system prompt already tells
//! the model it may call the tool once. `Synth::new_final` (used for the
//! citation retry) starts with `tool_choice: none`.
//!
//! Stage-dependent data lives *in the stage type* (`ToolRequested` owns the
//! `tool_use` ids and requested rule ids), so a `Synth<ToolRequested>` cannot
//! exist without them. The pure response-classification logic is `classify`,
//! unit-tested against `MessagesResponse` fixtures without a live API.

use anyhow::Context as _;
use judge_core::{JudgeError, RuleChunk, RuleId, Unvalidated, Verdict};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    Client, DEFAULT_MODEL, anthropic_schema,
    wire::{
        ContentBlock, Effort, Fallbacks, Message, MessagesRequest, MessagesResponse, OutputConfig, OutputFormat,
        Role, StopReason, SystemBlock, Thinking, Tool, ToolChoice,
    },
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
    /// `tool_use` block ids to answer (one each; several only if the model
    /// ignored `disable_parallel_tool_use`).
    tool_use_ids: Vec<String>,
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

/// Knobs for the synthesis request.
#[derive(Clone, Debug)]
pub struct SynthConfig {
    /// Model id.
    pub model: String,
    /// Output ceiling; keep ≤ 16k while the client is non-streaming.
    pub max_tokens: u32,
    /// `output_config.effort`.
    pub effort: Effort,
    /// Server-side refusal fallback. `Some` requires the client to send
    /// `Fallbacks::BETA` (see `Client::with_beta`); `Synth::new` adds it.
    pub fallbacks: Option<Fallbacks>,
    /// Bytes of rule text inlined into a `lookup_rules` tool result; a whole
    /// subsection such as `702` would otherwise inject tens of thousands of
    /// tokens. Chunks past the cap are replaced by an "omitted" line.
    pub max_tool_result_chars: usize,
}

impl Default for SynthConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_owned(),
            max_tokens: 16_000,
            effort: Effort::High,
            fallbacks: Some(Fallbacks::default_mode()),
            max_tool_result_chars: 30_000,
        }
    }
}

/// The response hit `max_tokens` (thinking tokens count against it). Carried
/// inside `JudgeError::Upstream` so a caller can downcast and retry at a
/// lower effort.
#[derive(Debug, thiserror::Error)]
#[error("response truncated at max_tokens ({output_tokens} output tokens)")]
pub struct Truncated {
    /// `usage.output_tokens` of the truncated response.
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
#[derive(Debug)]
pub struct Synth<S: Stage> {
    client: Client,
    cfg: SynthConfig,
    req: MessagesRequest,
    stage: S,
}

/// The first request: cached system prompt, the caller's user-turn blocks
/// (the caller puts a cache breakpoint on the material block), the
/// `lookup_rules` tool (single call per turn), adaptive thinking, and the
/// `Verdict` schema as structured output.
fn first_request(cfg: &SynthConfig, system: String, user: Vec<ContentBlock>, tool_choice: ToolChoice) -> MessagesRequest {
    MessagesRequest {
        model: cfg.model.clone(),
        max_tokens: cfg.max_tokens,
        system: vec![SystemBlock::cached(system)],
        messages: vec![Message { role: Role::User, content: user }],
        tools: vec![Tool {
            name: LOOKUP_RULES.to_owned(),
            description: "Fetch additional Comprehensive Rules text by id: rule ids such as \"613.7\" or \"702.19b\", \
                          or a whole subsection such as \"613\". You may call this at most once, before answering, if \
                          the provided rules are insufficient. Prefer specific rule ids: the result is truncated after \
                          a fixed amount of text, so a large subsection (e.g. \"702\", \"701\", \"800\") comes back \
                          incomplete."
                .to_owned(),
            input_schema: anthropic_schema::<LookupRulesInput>(),
            strict: Some(true),
            cache_control: None,
        }],
        tool_choice: Some(tool_choice),
        thinking: Some(Thinking::adaptive()),
        output_config: Some(OutputConfig {
            effort: Some(cfg.effort),
            format: Some(OutputFormat::JsonSchema { schema: anthropic_schema::<Verdict>() }),
        }),
        fallbacks: cfg.fallbacks.clone(),
    }
}

impl<S: Stage> Synth<S> {
    /// One round trip, logging output tokens against `max_tokens` so the
    /// truncation margin is observable.
    async fn round_trip(&self) -> Result<MessagesResponse, JudgeError> {
        let resp = self.client.messages(&self.req).await.map_err(anyhow::Error::from)?;
        tracing::info!(
            output_tokens = resp.usage.output_tokens,
            max_tokens = self.req.max_tokens,
            stop = ?resp.stop_reason,
            "synthesis response"
        );
        Ok(resp)
    }
}

impl Synth<Fresh> {
    /// Build the first request (see [`first_request`]) with the tool allowed.
    #[must_use]
    pub fn new(client: Client, cfg: &SynthConfig, system: impl Into<String>, user: Vec<ContentBlock>) -> Self {
        let client = if cfg.fallbacks.is_some() { client.with_beta(Fallbacks::BETA) } else { client };
        let req = first_request(cfg, system.into(), user, ToolChoice::auto_single());
        Self { client, cfg: cfg.clone(), req, stage: Fresh }
    }

    /// # Errors
    /// `LlmRefused` on a refusal, `Upstream` on transport/parse problems or
    /// an unexpected stop reason.
    pub async fn send(mut self) -> Result<SendOutcome, JudgeError> {
        let resp = self.round_trip().await?;
        match classify(resp)? {
            Step::Verdict(v) => Ok(SendOutcome::Done(v)),
            Step::Tool { tool_use_ids, requested, content } => {
                self.req.messages.push(Message { role: Role::Assistant, content });
                Ok(SendOutcome::ToolRequested(Synth {
                    client: self.client,
                    cfg: self.cfg,
                    req: self.req,
                    stage: ToolRequested { tool_use_ids, requested },
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
        // Every tool_use id must be answered in the same user message.
        let results = self
            .stage
            .tool_use_ids
            .into_iter()
            .map(|tool_use_id| ContentBlock::ToolResult { tool_use_id, content: content.clone(), is_error: false })
            .collect();
        self.req.messages.push(Message { role: Role::User, content: results });
        Synth { client: self.client, cfg: self.cfg, req: self.req, stage: Final }
    }
}

/// The tool result text: chunks in order until `max_chars` of rule text has
/// been inlined, then one line naming how many were left out.
fn render_tool_result(chunks: &[RuleChunk], max_chars: usize) -> String {
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
    pub fn new_final(client: Client, cfg: &SynthConfig, system: impl Into<String>, user: Vec<ContentBlock>) -> Self {
        let client = if cfg.fallbacks.is_some() { client.with_beta(Fallbacks::BETA) } else { client };
        let req = first_request(cfg, system.into(), user, ToolChoice::None);
        Self { client, cfg: cfg.clone(), req, stage: Final }
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
    /// Answer these `tool_use` ids with rules for `requested`; `content` is the
    /// assistant turn to echo back.
    Tool {
        /// `tool_use` block ids.
        tool_use_ids: Vec<String>,
        /// Union of requested rule ids, deduplicated.
        requested: Vec<RuleId>,
        /// The full assistant content (thinking blocks included).
        content: Vec<ContentBlock>,
    },
    /// The model's final structured answer.
    Verdict(Verdict<Unvalidated>),
}

/// Pure decision logic over `stop_reason` × content. Exhaustive so that a new
/// `StopReason` variant is a compile error here rather than a silent fall-through.
///
/// # Errors
/// `LlmRefused` for `refusal`; `Upstream` for truncation, unknown tools,
/// unexpected stop reasons and JSON that does not match the schema.
pub fn classify(resp: MessagesResponse) -> Result<Step, JudgeError> {
    match resp.stop_reason {
        Some(StopReason::Refusal) => {
            tracing::warn!(details = ?resp.stop_details, "model refused");
            Err(JudgeError::LlmRefused)
        }
        Some(StopReason::MaxTokens) => {
            Err(anyhow::Error::from(Truncated { output_tokens: resp.usage.output_tokens }).into())
        }
        Some(StopReason::ToolUse) => {
            let mut tool_use_ids = Vec::new();
            let mut requested: Vec<RuleId> = Vec::new();
            for (id, name, input) in resp.tool_uses() {
                if name != LOOKUP_RULES {
                    return Err(anyhow::anyhow!("model requested an unknown tool: {name}").into());
                }
                let parsed: LookupRulesInput =
                    serde_json::from_value(input.clone()).context("lookup_rules input did not match schema")?;
                tool_use_ids.push(id.to_owned());
                for r in parsed.ids {
                    if !requested.contains(&r) {
                        requested.push(r);
                    }
                }
            }
            if tool_use_ids.is_empty() {
                return Err(anyhow::anyhow!("stop_reason tool_use without a tool_use block").into());
            }
            Ok(Step::Tool { tool_use_ids, requested, content: resp.content })
        }
        Some(StopReason::EndTurn | StopReason::StopSequence) => {
            if resp.tool_uses().next().is_some() {
                return Err(anyhow::anyhow!("tool_use block with stop_reason {:?}", resp.stop_reason).into());
            }
            parse_verdict(&resp).map(Step::Verdict)
        }
        Some(StopReason::PauseTurn | StopReason::Unknown) | None => {
            Err(anyhow::anyhow!("unexpected stop_reason {:?}", resp.stop_reason).into())
        }
    }
}

/// With structured outputs the JSON is the *last* text block; any earlier
/// text blocks are prose and are logged, not parsed.
fn parse_verdict(resp: &MessagesResponse) -> Result<Verdict<Unvalidated>, JudgeError> {
    let mut blocks = resp.text_blocks().peekable();
    let mut last = None;
    while let Some(t) = blocks.next() {
        if blocks.peek().is_some() {
            tracing::debug!(text = t, "ignoring prose text block before the structured output");
        }
        last = Some(t);
    }
    let text = last.ok_or_else(|| anyhow::anyhow!("response contained no text block"))?;
    tracing::debug!(raw = %crate::truncate_for_log(text, crate::LOG_TEXT_CHARS), "synthesis raw model text");
    serde_json::from_str(text)
        .with_context(|| format!("verdict JSON did not match schema: {text}"))
        .map_err(JudgeError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn at<'a>(v: &'a Value, p: &str) -> &'a Value {
        v.pointer(p).unwrap_or(&Value::Null)
    }

    fn resp(stop: &str, content: &Value) -> Result<MessagesResponse, serde_json::Error> {
        serde_json::from_value(json!({
            "id": "m", "model": "claude-opus-5", "role": "assistant",
            "content": content, "stop_reason": stop,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
    }

    const VERDICT: &str = r#"{"answer":"a","confidence":"low","citations":[{"kind":"rule","id":"702.15b","quote":"q"}],"category":"layers"}"#;

    #[test]
    fn lookup_rules_schema_is_strict() {
        let s = anthropic_schema::<LookupRulesInput>();
        assert_eq!(at(&s, "/additionalProperties"), &Value::Bool(false));
        assert_eq!(at(&s, "/required"), &json!(["ids"]));
        assert!(!s.to_string().contains("pattern"), "{s:#}");
    }

    #[test]
    fn fresh_request_has_tool_thinking_format_and_fallbacks() -> Result<(), Box<dyn std::error::Error>> {
        let s = Synth::new(Client::new("k")?, &SynthConfig::default(), "sys", vec![ContentBlock::text("q")]);
        let v = serde_json::to_value(&s.req)?;
        assert_eq!(at(&v, "/model"), "claude-opus-5");
        assert_eq!(at(&v, "/tools/0/name"), LOOKUP_RULES);
        assert_eq!(at(&v, "/tool_choice/disable_parallel_tool_use"), &Value::Bool(true));
        assert_eq!(at(&v, "/thinking/type"), "adaptive");
        assert_eq!(at(&v, "/output_config/format/type"), "json_schema");
        assert_eq!(at(&v, "/output_config/effort"), "high");
        assert_eq!(at(&v, "/fallbacks"), "default");
        assert_eq!(s.client.betas(), [Fallbacks::BETA]);

        let cfg = SynthConfig { fallbacks: None, ..SynthConfig::default() };
        let s = Synth::new(Client::new("k")?, &cfg, "sys", vec![ContentBlock::text("q")]);
        assert!(serde_json::to_value(&s.req)?.get("fallbacks").is_none());
        assert!(s.client.betas().is_empty());

        let f = Synth::new_final(Client::new("k")?, &cfg, "sys", vec![ContentBlock::text("q")]);
        assert_eq!(f.req.tool_choice, Some(ToolChoice::None));
        assert_eq!(at(&serde_json::to_value(&f.req)?, "/tools/0/name"), LOOKUP_RULES);
        Ok(())
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
        let s = Synth::new(Client::new("k")?, &SynthConfig::default(), "sys", vec![ContentBlock::text("q")]);
        let t: Synth<ToolRequested> = Synth {
            client: s.client,
            cfg: s.cfg,
            req: s.req,
            stage: ToolRequested {
                tool_use_ids: vec!["tu_1".into(), "tu_2".into()],
                requested: vec![RuleId::try_new("613".to_owned())?],
            },
        };
        assert_eq!(t.requested().len(), 1);
        let f = t.answer_tool(&[]);
        assert_eq!(f.req.tool_choice, Some(ToolChoice::auto_single()), "cache-preserving");
        let last = f.req.messages.last().map(serde_json::to_value).transpose()?.unwrap_or_default();
        assert_eq!(at(&last, "/role"), "user");
        assert_eq!(at(&last, "/content/0/tool_use_id"), "tu_1");
        assert_eq!(at(&last, "/content/1/tool_use_id"), "tu_2");
        assert_eq!(at(&last, "/content/1/content"), "No rules found for the requested ids.");
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
        let r = resp("end_turn", &json!([{"type": "text", "text": VERDICT}]))?;
        assert!(matches!(classify(r)?, Step::Verdict(v) if v.answer() == "a"));
        Ok(())
    }

    #[test]
    fn classify_uses_last_text_block() -> Result<(), Box<dyn std::error::Error>> {
        let r = resp(
            "end_turn",
            &json!([{"type": "thinking", "thinking": "", "signature": "s"}, {"type": "text", "text": "Sure, here it is:"}, {"type": "text", "text": VERDICT}]),
        )?;
        assert!(matches!(classify(r)?, Step::Verdict(_)));
        Ok(())
    }

    #[test]
    fn classify_tool_round_unions_ids() -> Result<(), Box<dyn std::error::Error>> {
        let r = resp(
            "tool_use",
            &json!([
                {"type": "tool_use", "id": "tu_1", "name": "lookup_rules", "input": {"ids": ["613", "614"]}},
                {"type": "tool_use", "id": "tu_2", "name": "lookup_rules", "input": {"ids": ["614", "702.19b"]}}
            ]),
        )?;
        let Step::Tool { tool_use_ids, requested, content } = classify(r)? else {
            return Err("expected tool step".into());
        };
        assert_eq!(tool_use_ids, ["tu_1", "tu_2"]);
        let ids: Vec<&str> = requested.iter().map(AsRef::as_ref).collect();
        assert_eq!(ids, ["613", "614", "702.19b"]);
        assert_eq!(content.len(), 2);
        Ok(())
    }

    #[test]
    fn classify_errors() -> Result<(), Box<dyn std::error::Error>> {
        let refusal = resp("refusal", &json!([]))?;
        assert!(matches!(classify(refusal), Err(JudgeError::LlmRefused)));

        let truncated = resp("max_tokens", &json!([{"type": "text", "text": "{\"answer\": \"a"}]))?;
        assert!(matches!(classify(truncated), Err(JudgeError::Upstream(e)) if e.downcast_ref::<Truncated>().is_some_and(|t| t.output_tokens == 1)));

        let unknown_tool = resp("tool_use", &json!([{"type": "tool_use", "id": "t", "name": "other", "input": {}}]))?;
        assert!(matches!(classify(unknown_tool), Err(JudgeError::Upstream(_))));

        let bad_input = resp("tool_use", &json!([{"type": "tool_use", "id": "t", "name": "lookup_rules", "input": {"ids": ["abc"]}}]))?;
        assert!(matches!(classify(bad_input), Err(JudgeError::Upstream(_))));

        let no_block = resp("tool_use", &json!([{"type": "text", "text": "x"}]))?;
        assert!(matches!(classify(no_block), Err(JudgeError::Upstream(_))));

        let stray_tool = resp("end_turn", &json!([{"type": "tool_use", "id": "t", "name": "lookup_rules", "input": {"ids": []}}, {"type": "text", "text": VERDICT}]))?;
        assert!(matches!(classify(stray_tool), Err(JudgeError::Upstream(_))));

        for stop in ["pause_turn", "something_new"] {
            let r = resp(stop, &json!([{"type": "text", "text": VERDICT}]))?;
            assert!(matches!(classify(r), Err(JudgeError::Upstream(_))), "{stop}");
        }

        let bad_json = resp("end_turn", &json!([{"type": "text", "text": "{\"answer\":1}"}]))?;
        assert!(matches!(classify(bad_json), Err(JudgeError::Upstream(_))));

        let no_text = resp("end_turn", &json!([]))?;
        assert!(matches!(classify(no_text), Err(JudgeError::Upstream(_))));
        Ok(())
    }
}
