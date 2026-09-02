//! `Extractor` adapter: one low-effort model call with the `Extraction`
//! schema as structured output (pipeline steps 1 + 3), over any
//! `judge_llm::ChatModel`.
//!
//! The system prompt is stable (taxonomy + source definitions) and carries a
//! cache hint; thread history and the question go in the user turn. The pure
//! pieces (`build_request`, `parse_extraction`) are unit-tested without a
//! network; `extract` itself is tested against a `wiremock` Anthropic server.

use std::{fmt::Write as _, sync::Arc};

use anyhow::Context as _;
use async_trait::async_trait;
use judge_core::{Category, Extraction, Extractor, JudgeError, Qa, Question};
use judge_llm::{
    ChatModel, ChatRequest, ChatResponse, Effort, LOG_TEXT_CHARS, OutputSchema, Stop, TextBlock, ToolChoice, Turn,
    truncate_for_log,
};

/// Knobs for the extraction request. The model itself is the backend's.
#[derive(Clone, Debug)]
pub struct ExtractConfig {
    /// Effort; extraction is cheap and needs little thinking.
    pub effort: Effort,
    /// Output ceiling; the JSON is a few hundred tokens at most.
    pub max_tokens: u32,
    /// How many of the most recent thread Q&As to include.
    pub history_turns: usize,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            effort: Effort::Low,
            max_tokens: 2000,
            history_turns: 5,
        }
    }
}

/// `Extractor` over a `ChatModel`.
pub struct LlmExtractor {
    model: Arc<dyn ChatModel>,
    cfg: ExtractConfig,
}

impl LlmExtractor {
    /// Over `model` with explicit request knobs.
    #[must_use]
    pub fn new(model: Arc<dyn ChatModel>, cfg: ExtractConfig) -> Self {
        Self { model, cfg }
    }
}

#[async_trait]
impl Extractor for LlmExtractor {
    async fn extract(&self, q: &Question, history: &[Qa]) -> Result<Extraction, JudgeError> {
        let req = build_request(&self.cfg, q, history);
        let resp = self.model.complete(&req).await.map_err(anyhow::Error::from)?;
        let e = parse_extraction(&resp)?;
        if e.categories().all(|g| g.category == Category::Other) {
            tracing::warn!(question = %q.text, "extractor returned no category other than `other`; the category-map leg will be empty");
        }
        tracing::info!(
            spans = ?e.card_spans,
            concepts = ?e.concepts,
            categories = ?e.categories().map(|g| (g.category.id(), g.confidence)).collect::<Vec<_>>(),
            source = ?e.source,
            "extraction"
        );
        Ok(e)
    }
}

/// The stable part of the prompt: task, taxonomy and source definitions.
/// Harness-neutral: the extraction answer is JSON whoever produces it.
#[must_use]
pub fn system_prompt() -> String {
    let mut s = String::from(
        "You are the entity-extraction and classification stage of a Magic: The Gathering rules \
         assistant. You do NOT answer the question. You read the user's message (and any earlier \
         Q&A from the same thread, for context only) and return a JSON object with five fields.\n\n\
         1. card_spans: every substring of the user's message that looks like a Magic card name or a \
         nickname for one, including anything written in [[double brackets]]. Copy each span EXACTLY as \
         written, character for character, keeping the brackets, capitalisation, typos and spacing; a \
         later stage matches the spans against the card database and strips brackets itself. Include \
         nicknames and abbreviations (e.g. \"Bob\", \"Tabernacle\", \"Rhystic\"). When a span is a nickname \
         for a specific card or for a fixed group of cards (\"the tron lands\", \"the Urza's lands\", \
         \"the Titans\"), ALSO add the full Oracle name of each card it stands for as extra spans (e.g. \
         \"Urza's Tower\", \"Urza's Mine\", \"Urza's Power Plant\"); exact copying matters only for spans \
         taken from the message. Do not include rules vocabulary, keyword abilities, card types, token \
         names or generic words like \"creature\" or \"token\". If nothing looks like a card name, return \
         an empty array. Three further rules:\n\
         - Drop set, printing, frame and finish qualifiers from a span; the qualifier is not part of the \
         name. \"mirage LED\" -> \"LED\"; \"Urza's Saga Waylay\" -> \"Waylay\"; \"my foil Bolt\" -> \"Bolt\"; \
         \"alpha Lotus\" -> \"Lotus\"; \"the promo one\", \"the borderless version\", \"the old frame\" add \
         nothing (this is the one case where a span is a trimmed substring rather than a full copy).\n\
         - Never emit a collective nickname (\"the tron lands\", \"the fetches\", \"my wraths\", \"the \
         Titans\", \"the swords\") as a span. When the message uses one and you know which cards it \
         stands for, emit the members' full Oracle names instead; when you do not know the members, \
         leave it to the concepts list. A collective is never itself a card name.\n\
         - Do not emit generic basic land words (\"is it just a Mountain now\", \"tap a Forest\", \"my \
         Islands\") unless the question is about that basic land itself (\"does Plains have a mana \
         ability?\").\n\n\
         2. concepts: short rules-vocabulary phrases a keyword search over the Comprehensive Rules should \
         see: keyword abilities, keyword actions, zone names, game actions, rule concepts (e.g. \
         \"lifelink\", \"state-based actions\", \"copy\", \"leaves-the-battlefield trigger\", \"layer 7b\"). \
         Normalise to the rules' own terminology. Do not include card names.\n\n\
         3. primary: the single category from the taxonomy below that best fits the question, as an \
         object with the category id and a confidence of low, medium or high. This field is required: \
         always give a best guess, and use \"other\" only when nothing else fits at all, never when a \
         low-confidence guess is possible.\n\n\
         4. secondary: up to two further categories that also apply, best first, in the same shape. \
         Do not repeat the primary category. An empty array is fine.\n\n\
         The taxonomy is the complete list; use the category id exactly as listed.\n\n\
         5. source: which rules body the question falls under:\n\
         - cr: a question about how the game works, answered by the Comprehensive Rules.\n\
         - commander: a question about Commander format rules (command zone, commander damage, commander \
         tax, colour identity, the Commander banned list or Rules Committee policy).\n\
         - tournament: tournament policy (Magic Tournament Rules, Infraction Procedure Guide, penalties, \
         judge calls at events, deck registration, time extensions).\n\
         - out_of_scope: not a Magic rules question at all (deck-building advice, card prices, lore, \
         digital-client bugs, chit-chat). A vague or ill-formed rules question is still cr, not out of scope.\n\n\
         Taxonomy (id: description):\n",
    );
    for c in Category::ALL {
        let _ = writeln!(s, "- {}: {}", c.id(), c.label());
    }
    s.push_str("\nRespond with the JSON object only; it must conform to the provided schema.");
    s
}

/// The user turn: a compact history block (most recent last) and the question.
#[must_use]
pub fn user_turn(q: &Question, history: &[Qa], history_turns: usize) -> String {
    let mut s = String::new();
    let start = history.len().saturating_sub(history_turns);
    let recent = history.get(start..).unwrap_or_default();
    if !recent.is_empty() {
        s.push_str("## Earlier in this thread (context only; extract from the question below)\n");
        for h in recent {
            let _ = writeln!(s, "Q: {}\nA: {}", h.question, h.answer);
        }
        s.push('\n');
    }
    let _ = writeln!(s, "## Question\n{}", q.text);
    s
}

/// The `Extraction` schema as an agent should see it: full JSON Schema, not
/// a backend's structured-output subset, which strips keywords an agent can
/// use. Deserialization enforces the same shape.
#[must_use]
pub fn schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Extraction)).unwrap_or_default()
}

/// One request: cached system prompt, effort from `cfg`, the `Extraction`
/// schema as structured output, no tools, no extended reasoning, no temperature.
#[must_use]
pub fn build_request(cfg: &ExtractConfig, q: &Question, history: &[Qa]) -> ChatRequest {
    ChatRequest {
        max_tokens: cfg.max_tokens,
        system: vec![TextBlock::cached(system_prompt())],
        turns: vec![Turn::User(vec![TextBlock::plain(user_turn(q, history, cfg.history_turns))])],
        tools: vec![],
        tool_choice: ToolChoice::None,
        output: Some(OutputSchema::of::<Extraction>()),
        effort: Some(cfg.effort),
        thinking: false,
        fallbacks: None,
    }
}

/// Map the stop reason × content to an `Extraction`. The structured-output
/// JSON is the *last* text block; the model may not emit anything else.
///
/// # Errors
/// `LlmRefused` for a refusal; `Upstream` for truncation, unexpected stop
/// reasons, a missing text block, or JSON that does not match the schema (the
/// raw text is included in the message).
pub fn parse_extraction(resp: &ChatResponse) -> Result<Extraction, JudgeError> {
    match &resp.stop {
        Stop::Refusal(details) => {
            tracing::warn!(?details, "extraction refused");
            Err(JudgeError::LlmRefused)
        }
        Stop::MaxTokens => Err(anyhow::anyhow!("extraction truncated at max_tokens").into()),
        Stop::EndTurn => {
            let text = resp.last_text().ok_or_else(|| anyhow::anyhow!("extraction response had no text block"))?;
            tracing::debug!(raw = %truncate_for_log(text, LOG_TEXT_CHARS), "extraction raw model text");
            let e: Extraction = serde_json::from_str(text)
                // Bounded for the same reason as the verdict parse: this text
                // reaches the logs through `Upstream`, and it carries the
                // asker's own words back into them.
                .with_context(|| {
                    let shown = truncate_for_log(text, LOG_TEXT_CHARS);
                    format!("extraction JSON did not match schema: {shown}")
                })?;
            if e.secondary.len() > Extraction::MAX_SECONDARY {
                // The schema subsets cannot express maxItems (stripped for
                // Anthropic); `Extraction::categories()` ignores the extras.
                tracing::debug!(n = e.secondary.len(), "model returned more than two secondary categories");
            }
            Ok(e)
        }
        Stop::ToolUse => Err(anyhow::anyhow!("unexpected tool call from extraction").into()),
        Stop::Other(reason) => Err(anyhow::anyhow!("unexpected stop reason {reason:?} from extraction").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_anthropic::{Anthropic, Endpoint};
    use judge_core::{Confidence, Source};
    use judge_llm::{AssistantTurn, Metered, Refusal, SpendMeter, Usage};
    use serde_json::{Value, json};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    fn at<'a>(v: &'a Value, p: &str) -> &'a Value {
        v.pointer(p).unwrap_or(&Value::Null)
    }

    fn question() -> Question {
        Question {
            thread_id: "t".into(),
            text: "Does [[Humility]] turn off Bob's lifelink?".into(),
        }
    }

    fn history() -> Vec<Qa> {
        (0..7)
            .map(|i| Qa {
                question: format!("q{i}"),
                answer: format!("a{i}"),
            })
            .collect()
    }

    const GOOD: &str = r#"{"card_spans":["[[Humility]]","Bob"],"concepts":["lifelink","layers"],"primary":{"category":"layers","confidence":"high"},"secondary":[{"category":"keyword_abilities","confidence":"medium"}],"source":"cr"}"#;

    fn body(stop: &str, content: Value) -> Value {
        let mut b = body_with(stop, &Value::Null, &Value::Null);
        if let Some(m) = b.as_object_mut() {
            m.insert("content".to_owned(), content);
        }
        b
    }

    fn body_with(stop: &str, content: &Value, stop_details: &Value) -> Value {
        json!({
            "id": "msg_1", "model": "claude-opus-5", "role": "assistant",
            "content": content, "stop_reason": stop, "stop_details": stop_details,
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
    }

    async fn server_with(resp: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("anthropic-version", judge_anthropic::API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(resp))
            .expect(1)
            .mount(&server)
            .await;
        server
    }

    /// The Anthropic backend against the mock server, metered as in production.
    fn model(server: &MockServer) -> Result<Arc<dyn ChatModel>, judge_llm::LlmError> {
        let backend = Anthropic::new(Endpoint::Direct { base_url: server.uri(), api_key: "test-key".into() })?;
        Ok(Arc::new(Metered::new(backend, SpendMeter::new())?))
    }

    fn extractor(server: &MockServer) -> Result<LlmExtractor, judge_llm::LlmError> {
        Ok(LlmExtractor::new(model(server)?, ExtractConfig::default()))
    }

    /// A neutral response as the backend would hand it over.
    fn resp(stop: Stop, text: &[&str]) -> ChatResponse {
        ChatResponse {
            text: text.iter().map(|t| (*t).to_owned()).collect(),
            tool_calls: vec![],
            stop,
            usage: Usage::default(),
            model: "claude-opus-5".into(),
            assistant: AssistantTurn { backend: "test", raw: Value::Null },
        }
    }

    #[test]
    fn request_shape_and_prompt_contents() -> Result<(), serde_json::Error> {
        let cfg = ExtractConfig::default();
        let req = build_request(&cfg, &question(), &history());
        assert_eq!(req.max_tokens, 2000);
        assert_eq!(req.effort, Some(Effort::Low));
        assert!(!req.thinking && req.tools.is_empty() && req.fallbacks.is_none());
        let schema = req.output.as_ref().map(|o| o.schema.clone().to_value()).unwrap_or_default();
        // The schema requires a primary category: an empty classification is an API-level error.
        let required = at(&schema, "/required");
        assert!(required.as_array().is_some_and(|r| r.iter().any(|x| x == "primary")), "{required}");
        let v = serde_json::to_value(&req)?;
        assert_eq!(at(&v, "/system/0/cache"), "Short");
        let sys = at(&v, "/system/0/text").as_str().unwrap_or_default();
        for c in Category::ALL {
            assert!(
                sys.contains(&format!("- {}: {}", c.id(), c.label())),
                "missing {c}"
            );
        }
        for src in ["cr:", "commander:", "tournament:", "out_of_scope:"] {
            assert!(sys.contains(src), "missing {src}");
        }
        assert!(sys.contains("3. primary:") && sys.contains("4. secondary:") && sys.contains("5. source:"), "{sys}");
        // Nickname-artefact rules: printing qualifiers, collectives beside their members, generic basics.
        assert!(sys.contains("\"mirage LED\" -> \"LED\"") && sys.contains("\"Urza's Saga Waylay\" -> \"Waylay\""), "{sys}");
        assert!(sys.contains("Never emit a collective nickname") && sys.contains("\"the tron lands\""), "{sys}");
        assert!(sys.contains("Do not emit generic basic land words"), "{sys}");
        let user = at(&v, "/turns/0/User/0/text").as_str().unwrap_or_default();
        assert!(at(&v, "/turns/0/User/0/cache").is_null());
        // Only the last `history_turns` Q&As, oldest first, then the question.
        assert!(!user.contains("Q: q1\n"), "{user}");
        assert!(
            user.contains("Q: q2\nA: a2") && user.contains("Q: q6\nA: a6"),
            "{user}"
        );
        assert!(
            user.ends_with("## Question\nDoes [[Humility]] turn off Bob's lifelink?\n"),
            "{user}"
        );
        // No history ⇒ no history block.
        let bare = user_turn(&question(), &[], 5);
        assert!(!bare.contains("Earlier in this thread"), "{bare}");
        Ok(())
    }

    #[tokio::test]
    async fn happy_path() -> Result<(), Box<dyn std::error::Error>> {
        let server = server_with(body("end_turn", json!([{"type": "text", "text": GOOD}]))).await;
        let e = extractor(&server)?.extract(&question(), &history()).await?;
        assert_eq!(e.card_spans, ["[[Humility]]", "Bob"]);
        assert_eq!(e.concepts, ["lifelink", "layers"]);
        assert_eq!(e.primary_category(), Category::Layers);
        assert_eq!(
            e.categories().nth(1).map(|g| g.confidence),
            Some(Confidence::Medium)
        );
        assert_eq!(e.source, Source::Cr);

        // Assert what actually went over the wire.
        let reqs = server.received_requests().await.unwrap_or_default();
        let sent: Value =
            serde_json::from_slice(&reqs.first().map(|r| r.body.clone()).unwrap_or_default())?;
        assert_eq!(at(&sent, "/model"), "claude-opus-5");
        assert_eq!(at(&sent, "/output_config/effort"), "low");
        assert_eq!(at(&sent, "/output_config/format/type"), "json_schema");
        assert!(sent.get("temperature").is_none());
        assert_eq!(at(&sent, "/system/0/cache_control/type"), "ephemeral");
        assert!(
            reqs.first()
                .is_some_and(|r| r.headers.get("x-api-key").is_some())
        );
        Ok(())
    }

    #[tokio::test]
    async fn refusal_maps_to_llm_refused() -> Result<(), Box<dyn std::error::Error>> {
        let b = body_with(
            "refusal",
            &json!([]),
            &json!({"type": "refusal", "category": "other"}),
        );
        let server = server_with(b).await;
        let r = extractor(&server)?.extract(&question(), &[]).await;
        assert!(matches!(r, Err(JudgeError::LlmRefused)), "{r:?}");
        Ok(())
    }

    #[tokio::test]
    async fn malformed_json_is_upstream_with_raw_text() -> Result<(), Box<dyn std::error::Error>> {
        let server = server_with(body(
            "end_turn",
            json!([{"type": "text", "text": "{\"card_spans\": 1"}]),
        ))
        .await;
        let r = extractor(&server)?.extract(&question(), &[]).await;
        match r {
            Err(JudgeError::Upstream(e)) => {
                assert!(format!("{e:#}").contains("{\"card_spans\": 1"), "{e:#}");
            }
            other => return Err(format!("expected Upstream, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn parse_edge_cases() -> Result<(), Box<dyn std::error::Error>> {
        let truncated = resp(Stop::MaxTokens, &["{"]);
        assert!(matches!(parse_extraction(&truncated), Err(JudgeError::Upstream(_))));
        let no_text = resp(Stop::EndTurn, &[]);
        assert!(matches!(parse_extraction(&no_text), Err(JudgeError::Upstream(_))));
        let odd = resp(Stop::Other("pause_turn".into()), &[GOOD]);
        assert!(matches!(parse_extraction(&odd), Err(JudgeError::Upstream(_))));
        let tool = resp(Stop::ToolUse, &[GOOD]);
        assert!(matches!(parse_extraction(&tool), Err(JudgeError::Upstream(_))));
        let refused = resp(Stop::Refusal(Refusal::default()), &[]);
        assert!(matches!(parse_extraction(&refused), Err(JudgeError::LlmRefused)));
        // Prose before the JSON is ignored; only the last text block is parsed.
        let prose = resp(Stop::EndTurn, &["Here:", GOOD]);
        assert_eq!(parse_extraction(&prose)?.source, Source::Cr);
        // More than two secondary categories are accepted but only two are used.
        let many = GOOD.replace(
            "\"secondary\":[",
            "\"secondary\":[{\"category\":\"other\",\"confidence\":\"low\"},{\"category\":\"combat\",\"confidence\":\"low\"},",
        );
        let four = resp(Stop::EndTurn, &[&many]);
        assert_eq!(parse_extraction(&four)?.categories().count(), 3);
        // A missing secondary array is fine; a missing primary is a schema violation.
        let no_secondary = GOOD.replace(r#","secondary":[{"category":"keyword_abilities","confidence":"medium"}]"#, "");
        assert_eq!(parse_extraction(&resp(Stop::EndTurn, &[&no_secondary]))?.categories().count(), 1);
        let no_primary = GOOD.replace(r#""primary":{"category":"layers","confidence":"high"},"#, "");
        assert!(matches!(parse_extraction(&resp(Stop::EndTurn, &[&no_primary])), Err(JudgeError::Upstream(_))));
        Ok(())
    }

    #[tokio::test]
    async fn config_is_used_for_the_request() -> Result<(), Box<dyn std::error::Error>> {
        let server = server_with(body("end_turn", json!([{"type": "text", "text": GOOD}]))).await;
        let cfg = ExtractConfig { effort: Effort::Medium, max_tokens: 777, history_turns: 1 };
        let x = LlmExtractor::new(model(&server)?, cfg);
        x.extract(&question(), &history()).await?;
        let reqs = server.received_requests().await.unwrap_or_default();
        let sent: Value = serde_json::from_slice(&reqs.first().map(|r| r.body.clone()).unwrap_or_default())?;
        assert_eq!(at(&sent, "/output_config/effort"), "medium");
        assert_eq!(at(&sent, "/max_tokens"), 777);
        let user = at(&sent, "/messages/0/content/0/text").as_str().unwrap_or_default();
        assert!(!user.contains("Q: q5\n") && user.contains("Q: q6\n"), "{user}");
        Ok(())
    }
}
