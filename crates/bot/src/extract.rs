//! `Extractor` adapter: one low-effort Anthropic call with the `Extraction`
//! schema as structured output (pipeline steps 1 + 3).
//!
//! The system prompt is stable (taxonomy + source definitions) and carries a
//! `cache_control` breakpoint; thread history and the question go in the user
//! turn. The pure pieces (`build_request`, `parse_extraction`) are unit-tested
//! without a network; `extract` itself is tested against a `wiremock` server.

use std::fmt::Write as _;

use anyhow::Context as _;
use async_trait::async_trait;
use judge_anthropic::{
    Client, DEFAULT_MODEL, LOG_TEXT_CHARS, anthropic_schema, truncate_for_log,
    wire::{
        Effort, Message, MessagesRequest, MessagesResponse, OutputConfig, OutputFormat, StopReason,
        SystemBlock,
    },
};
use judge_core::{Category, Extraction, Extractor, JudgeError, Qa, Question};

/// Knobs for the extraction request.
#[derive(Clone, Debug)]
pub struct ExtractConfig {
    /// Model id.
    pub model: String,
    /// `output_config.effort`; extraction is cheap and needs little thinking.
    pub effort: Effort,
    /// Output ceiling; the JSON is a few hundred tokens at most.
    pub max_tokens: u32,
    /// How many of the most recent thread Q&As to include.
    pub history_turns: usize,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_owned(),
            effort: Effort::Low,
            max_tokens: 2000,
            history_turns: 5,
        }
    }
}

/// Anthropic-backed `Extractor`.
pub struct AnthropicExtractor {
    client: Client,
    cfg: ExtractConfig,
}

impl AnthropicExtractor {
    /// Over a shared client with explicit request knobs.
    #[must_use]
    pub fn new(client: Client, cfg: ExtractConfig) -> Self {
        Self { client, cfg }
    }
}

#[async_trait]
impl Extractor for AnthropicExtractor {
    async fn extract(&self, q: &Question, history: &[Qa]) -> Result<Extraction, JudgeError> {
        let req = build_request(&self.cfg, q, history);
        let resp = self
            .client
            .messages(&req)
            .await
            .map_err(anyhow::Error::from)?;
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
fn system_prompt() -> String {
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
fn user_turn(q: &Question, history: &[Qa], history_turns: usize) -> String {
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

/// One `messages` request: cached system prompt, effort from `cfg`, the
/// `Extraction` schema as structured output, no thinking override, no temperature.
#[must_use]
pub fn build_request(cfg: &ExtractConfig, q: &Question, history: &[Qa]) -> MessagesRequest {
    MessagesRequest {
        model: cfg.model.clone(),
        max_tokens: cfg.max_tokens,
        system: vec![SystemBlock::cached(system_prompt())],
        messages: vec![Message::user_text(user_turn(q, history, cfg.history_turns))],
        tools: vec![],
        tool_choice: None,
        thinking: None,
        output_config: Some(OutputConfig {
            effort: Some(cfg.effort),
            format: Some(OutputFormat::JsonSchema {
                schema: anthropic_schema::<Extraction>(),
            }),
        }),
        fallbacks: None,
    }
}

/// Map `stop_reason` × content to an `Extraction`. The structured-output JSON
/// is the *last* text block; the model may not emit anything else.
///
/// # Errors
/// `LlmRefused` for `refusal`; `Upstream` for truncation, unexpected stop
/// reasons, a missing text block, or JSON that does not match the schema (the
/// raw text is included in the message).
pub fn parse_extraction(resp: &MessagesResponse) -> Result<Extraction, JudgeError> {
    match resp.stop_reason {
        Some(StopReason::Refusal) => {
            tracing::warn!(details = ?resp.stop_details, "extraction refused");
            Err(JudgeError::LlmRefused)
        }
        Some(StopReason::MaxTokens) => {
            Err(anyhow::anyhow!("extraction truncated at max_tokens").into())
        }
        Some(StopReason::EndTurn | StopReason::StopSequence) => {
            let text = resp
                .text_blocks()
                .last()
                .ok_or_else(|| anyhow::anyhow!("extraction response had no text block"))?;
            tracing::debug!(raw = %truncate_for_log(text, LOG_TEXT_CHARS), "extraction raw model text");
            let e: Extraction = serde_json::from_str(text)
                .with_context(|| format!("extraction JSON did not match schema: {text}"))?;
            if e.secondary.len() > Extraction::MAX_SECONDARY {
                // The schema cannot express maxItems (stripped by AnthropicSubset);
                // `Extraction::categories()` ignores the extras.
                tracing::debug!(n = e.secondary.len(), "model returned more than two secondary categories");
            }
            Ok(e)
        }
        Some(StopReason::ToolUse | StopReason::PauseTurn | StopReason::Unknown) | None => {
            Err(anyhow::anyhow!(
                "unexpected stop_reason {:?} from extraction",
                resp.stop_reason
            )
            .into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_core::{Confidence, Source};
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

    fn extractor(server: &MockServer) -> Result<AnthropicExtractor, judge_anthropic::ClientError> {
        Ok(AnthropicExtractor::new(
            Client::new("test-key")?.with_base_url(server.uri()),
            ExtractConfig::default(),
        ))
    }

    #[test]
    fn request_shape_and_prompt_contents() -> Result<(), serde_json::Error> {
        let cfg = ExtractConfig::default();
        let req = build_request(&cfg, &question(), &history());
        let v = serde_json::to_value(&req)?;
        assert_eq!(at(&v, "/model"), "claude-opus-5");
        assert_eq!(at(&v, "/max_tokens"), 2000);
        assert_eq!(at(&v, "/output_config/effort"), "low");
        assert_eq!(at(&v, "/output_config/format/type"), "json_schema");
        assert_eq!(
            at(&v, "/output_config/format/schema/additionalProperties"),
            &Value::Bool(false)
        );
        // The wire schema requires a primary category: an empty classification is an API-level error.
        let required = at(&v, "/output_config/format/schema/required");
        assert!(required.as_array().is_some_and(|r| r.iter().any(|x| x == "primary")), "{required}");
        assert!(v.get("temperature").is_none());
        assert!(v.get("thinking").is_none());
        assert!(v.get("tools").is_none());
        assert_eq!(
            at(&v, "/system/0/cache_control"),
            &json!({"type": "ephemeral"})
        );
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
        let user = at(&v, "/messages/0/content/0/text")
            .as_str()
            .unwrap_or_default();
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
        let resp = |b: Value| serde_json::from_value::<MessagesResponse>(b);
        let truncated = resp(body("max_tokens", json!([{"type": "text", "text": "{"}])))?;
        assert!(matches!(
            parse_extraction(&truncated),
            Err(JudgeError::Upstream(_))
        ));
        let no_text = resp(body("end_turn", json!([])))?;
        assert!(matches!(
            parse_extraction(&no_text),
            Err(JudgeError::Upstream(_))
        ));
        let odd = resp(body("pause_turn", json!([{"type": "text", "text": GOOD}])))?;
        assert!(matches!(
            parse_extraction(&odd),
            Err(JudgeError::Upstream(_))
        ));
        // Prose before the JSON is ignored; only the last text block is parsed.
        let prose = resp(body(
            "end_turn",
            json!([{"type": "text", "text": "Here:"}, {"type": "text", "text": GOOD}]),
        ))?;
        assert_eq!(parse_extraction(&prose)?.source, Source::Cr);
        // More than two secondary categories are accepted but only two are used.
        let many = GOOD.replace(
            "\"secondary\":[",
            "\"secondary\":[{\"category\":\"other\",\"confidence\":\"low\"},{\"category\":\"combat\",\"confidence\":\"low\"},",
        );
        let four = resp(body("end_turn", json!([{"type": "text", "text": many}])))?;
        assert_eq!(parse_extraction(&four)?.categories().count(), 3);
        // A missing secondary array is fine; a missing primary is a schema violation.
        let no_secondary = resp(body("end_turn", json!([{"type": "text", "text": GOOD.replace(
            r#","secondary":[{"category":"keyword_abilities","confidence":"medium"}]"#, "")}])))?;
        assert_eq!(parse_extraction(&no_secondary)?.categories().count(), 1);
        let no_primary = resp(body("end_turn", json!([{"type": "text", "text": GOOD.replace(
            r#""primary":{"category":"layers","confidence":"high"},"#, "")}])))?;
        assert!(matches!(parse_extraction(&no_primary), Err(JudgeError::Upstream(_))));
        Ok(())
    }

    #[tokio::test]
    async fn config_is_used_for_the_request() -> Result<(), Box<dyn std::error::Error>> {
        let server = server_with(body("end_turn", json!([{"type": "text", "text": GOOD}]))).await;
        let cfg = ExtractConfig { effort: Effort::Medium, max_tokens: 777, history_turns: 1, ..ExtractConfig::default() };
        let x = AnthropicExtractor::new(Client::new("test-key")?.with_base_url(server.uri()), cfg);
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
