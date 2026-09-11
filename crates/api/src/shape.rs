//! Pure shaping: HTTP request validation and pipeline outcome → JSON reply.
//!
//! The Discord layer's pure helpers are reused where they exist
//! (`render::{rule_url, scryfall_url, error}`, `question::pin_card`); this
//! module only decides the wire shape the web client sees.

use judge_bot::discord::{question, render};
use judge_core::{Ambiguous, Citation, Confidence, Context, JudgeError, Source, Validated, Verdict};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Longest accepted question, in characters.
pub const MAX_QUESTION_CHARS: usize = 1000;
/// Most pins accepted on one request (each pin is one resolved ambiguity).
pub const MAX_PINS: usize = 8;
/// Longest accepted pin span or card name, in characters.
pub const MAX_PIN_CHARS: usize = 200;
/// Candidate names returned per ambiguous span (mirrors the Discord button row).
pub const MAX_CHOICES: usize = render::MAX_CHOICES;

/// Reply when an IP has used up its request window.
pub const RATE_LIMITED: &str =
    "You've asked several questions in a short time. Please wait a few minutes and ask again.";

/// Body of `POST /api/judge`.
#[derive(Clone, Debug, Deserialize)]
pub struct JudgeRequest {
    /// The rules question (write a card as `[[Full Name]]` to pin it).
    pub question: String,
    /// Client-generated session id; questions sharing one share history.
    #[serde(default)]
    pub session_id: Option<Uuid>,
    /// Resolved ambiguities from earlier `ambiguous` replies, oldest first.
    #[serde(default)]
    pub pins: Vec<Pin>,
}

/// One resolved "did you mean…?": the span as written and the full card name
/// the user picked.
#[derive(Clone, Debug, Deserialize)]
pub struct Pin {
    /// The ambiguous span exactly as the `ambiguous` reply reported it.
    pub span: String,
    /// The full card name chosen from that reply's candidates.
    pub name: String,
}

/// Every reply body of `POST /api/judge`, tagged by `kind`.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApiReply {
    /// A validated verdict.
    Answer {
        /// The answer text.
        answer: String,
        /// Model self-reported confidence (`low` / `medium` / `high`).
        confidence: Confidence,
        /// Rules body the answer draws on (`cr` / `commander`).
        source: Source,
        /// CR effective date as `YYYYMMDD`.
        cr_version: String,
        /// Citations in the model's order.
        citations: Vec<CitationView>,
    },
    /// Card spans that matched several cards; re-ask with `pins`.
    Ambiguous {
        /// One entry per still-ambiguous span, first span first.
        spans: Vec<AmbiguousView>,
    },
    /// Card spans that matched nothing.
    NotFound {
        /// The spans as written.
        names: Vec<String>,
    },
    /// Anything else the caller can't act on beyond rephrasing.
    Error {
        /// Human-readable explanation.
        message: String,
    },
    /// Every judge slot is taken.
    Busy {
        /// Human-readable explanation.
        message: String,
    },
    /// The caller's IP used up its request window.
    RateLimited {
        /// Human-readable explanation.
        message: String,
    },
}

/// One citation, pre-linked the way the Discord embed links them.
#[derive(Clone, Debug, Serialize)]
pub struct CitationView {
    /// Display label, e.g. `702.19b` or `Ruling (2018-07-13) — Blood Moon`.
    pub label: String,
    /// Where the source can be read, when it is public (rules and Scryfall
    /// material; prior calls have no public page).
    pub url: Option<String>,
    /// The verbatim quote, whitespace collapsed.
    pub quote: String,
}

/// One ambiguous span with its candidate full names.
#[derive(Clone, Debug, Serialize)]
pub struct AmbiguousView {
    /// The span as the user wrote it.
    pub query: String,
    /// Candidate full card names, at most [`MAX_CHOICES`].
    pub choices: Vec<String>,
    /// True if more candidates existed than [`MAX_CHOICES`].
    pub truncated: bool,
}

/// Check the request's size limits.
///
/// # Errors
/// A message suitable for a 400 body.
pub fn validate(req: &JudgeRequest) -> Result<(), String> {
    if req.question.trim().is_empty() {
        return Err("question must not be empty".to_owned());
    }
    if req.question.chars().count() > MAX_QUESTION_CHARS {
        return Err(format!("question must be at most {MAX_QUESTION_CHARS} characters"));
    }
    if req.pins.len() > MAX_PINS {
        return Err(format!("at most {MAX_PINS} pins are accepted"));
    }
    for p in &req.pins {
        if p.span.trim().is_empty() || p.name.trim().is_empty() {
            return Err("pins must have a non-empty span and name".to_owned());
        }
        if p.span.chars().count() > MAX_PIN_CHARS || p.name.chars().count() > MAX_PIN_CHARS {
            return Err(format!("pin spans and names must be at most {MAX_PIN_CHARS} characters"));
        }
    }
    Ok(())
}

/// The question with every pin rewritten to `[[Full Name]]`, exactly as a
/// Discord "did you mean…?" pick would rewrite it.
#[must_use]
pub fn question_text(req: &JudgeRequest) -> String {
    req.pins
        .iter()
        .fold(req.question.clone(), |text, p| question::pin_card(&text, &p.span, &p.name))
}

/// Shape a validated verdict. `ctx` (the captured retrieval context) supplies
/// card names for ruling / Oracle-text labels; the links work without it.
#[must_use]
pub fn answer(v: &Verdict<Validated>, ctx: Option<&Context>) -> ApiReply {
    ApiReply::Answer {
        answer: v.answer().to_owned(),
        confidence: v.confidence(),
        source: v.source(),
        cr_version: v.cr_version().as_ref().to_owned(),
        citations: v.citations().iter().map(|c| citation_view(c, ctx)).collect(),
    }
}

/// Shape a failed `judge()`. Exhaustive over [`JudgeError`], so a new variant
/// must be shaped before this compiles.
#[must_use]
pub fn error(e: &JudgeError) -> ApiReply {
    match e {
        JudgeError::AmbiguousCards(spans) => ApiReply::Ambiguous {
            spans: spans.iter().map(ambiguous_view).collect(),
        },
        JudgeError::CardsNotFound(names) => ApiReply::NotFound { names: names.iter().cloned().collect() },
        JudgeError::OutOfScope(_)
        | JudgeError::BadCitation(_)
        | JudgeError::MalformedCitation(_)
        | JudgeError::EmptyVerdict(_)
        | JudgeError::LlmRefused
        | JudgeError::Upstream(_) => ApiReply::Error { message: render::error(e) },
    }
}

fn ambiguous_view(a: &Ambiguous) -> AmbiguousView {
    AmbiguousView {
        query: a.query.clone(),
        choices: a.candidates.iter().map(|c| c.name.clone()).take(MAX_CHOICES).collect(),
        truncated: a.candidates.len() > MAX_CHOICES,
    }
}

fn citation_view(c: &Citation, ctx: Option<&Context>) -> CitationView {
    let quote = collapse_whitespace(c.quote());
    let card_name = |id| ctx.and_then(|x| x.card(id)).map(|card| card.name.clone());
    match c {
        Citation::Rule { id, .. } => CitationView {
            label: id.to_string(),
            url: Some(render::rule_url(id)),
            quote,
        },
        Citation::ScryfallRuling { card, ruling, .. } => {
            let date = ctx.and_then(|x| x.ruling(*card, ruling)).map(|r| format!(" ({})", r.published_at)).unwrap_or_default();
            let label = match card_name(*card) {
                Some(name) => format!("Ruling{date} — {name}"),
                None => format!("Scryfall ruling{date}"),
            };
            CitationView { label, url: Some(render::scryfall_url(*card)), quote }
        }
        Citation::PriorCall { .. } => CitationView { label: "Prior call".to_owned(), url: None, quote },
        Citation::OracleText { card, face, .. } => {
            let face_note = match face {
                0 => String::new(),
                n => format!(", face {}", n.saturating_add(1)),
            };
            let label = match card_name(*card) {
                Some(name) => format!("Oracle text{face_note} — {name}"),
                None => format!("Oracle text{face_note}"),
            };
            CitationView { label, url: Some(render::scryfall_url(*card)), quote }
        }
    }
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_core::{
        AnswerableSource, CallId, Card, CardId, Category, CrVersion, Face, Layout, RuleChunk, RuleId,
        Ruling, Unvalidated,
        ruling_key,
};
    use nonempty::NonEmpty;

    fn req(question: &str) -> JudgeRequest {
        JudgeRequest { question: question.to_owned(), session_id: None, pins: vec![] }
    }

    #[test]
    fn validation_enforces_the_size_limits() {
        assert!(validate(&req("does lifelink stack?")).is_ok());
        assert!(validate(&req("  ")).is_err());
        assert!(validate(&req(&"x".repeat(MAX_QUESTION_CHARS + 1))).is_err());
        let mut r = req("q?");
        r.pins = vec![Pin { span: "urza".into(), name: "Urza's Tower".into() }; MAX_PINS + 1];
        assert!(validate(&r).is_err());
        r.pins = vec![Pin { span: " ".into(), name: "Urza's Tower".into() }];
        assert!(validate(&r).is_err());
        r.pins = vec![Pin { span: "urza".into(), name: "n".repeat(MAX_PIN_CHARS + 1) }];
        assert!(validate(&r).is_err());
        r.pins = vec![Pin { span: "urza".into(), name: "Urza's Tower".into() }];
        assert!(validate(&r).is_ok());
    }

    #[test]
    fn pins_rewrite_spans_in_order() {
        let mut r = req("can urza and bob block?");
        r.pins = vec![
            Pin { span: "urza".into(), name: "Urza, Lord High Artificer".into() },
            Pin { span: "bob".into(), name: "Dark Confidant".into() },
        ];
        assert_eq!(
            question_text(&r),
            "can [[Urza, Lord High Artificer]] and [[Dark Confidant]] block?"
        );
        assert_eq!(question_text(&req("plain")), "plain");
    }

    fn card(n: u128, name: &str) -> Card {
        Card {
            id: CardId::new(Uuid::from_u128(n)),
            name: name.into(),
            layout: Layout::Normal,
            faces: NonEmpty::new(Face {
                name: name.into(),
                oracle_text: String::new(),
                mana_cost: String::new(),
                type_line: "Creature".into(),
            }),
        }
    }

    fn validated() -> Result<(Verdict<Validated>, Context), Box<dyn std::error::Error>> {
        let body = "Damage dealt by a source with lifelink causes its controller to gain that much life.";
        let card_id = CardId::new(Uuid::from_u128(7));
        let ctx = Context {
            cards: vec![card(7, "Dark Confidant")],
            rules: vec![RuleChunk {
                id: RuleId::try_new("702.15b".to_owned())?,
                parent_id: None,
                subsection: RuleId::try_new("702".to_owned())?,
                heading: "Lifelink".into(),
                body: body.into(),
                examples: vec![],
                cr_version: CrVersion::try_new("20260819".to_owned())?,
            }],
            rulings: vec![Ruling {
                card: card_id,
                key: ruling_key("2020-01-01", "Lifelink is  not\na triggered ability."),
                published_at: "2020-01-01".into(),
                text: "Lifelink is  not\na triggered ability.".into(),
            }],
            ..Context::default()
        };
        let v = Verdict::<Unvalidated>::new(
            "You gain the life at the same time the damage is dealt.".into(),
            Confidence::High,
            vec![
                Citation::Rule { id: RuleId::try_new("702.15b".to_owned())?, quote: judge_core::Quote::try_new("gain that much life")? },
                Citation::ScryfallRuling { card: card_id, ruling: ruling_key("2020-01-01", "Lifelink is  not\na triggered ability."), quote: judge_core::Quote::try_new("a triggered ability")? },
            ],
            Category::KeywordAbilities,
        );
        let v = v.validate(&ctx, AnswerableSource::Cr)?;
        Ok((v, ctx))
    }

    #[test]
    fn answer_carries_linked_citations() -> Result<(), Box<dyn std::error::Error>> {
        let (v, ctx) = validated()?;
        let reply = answer(&v, Some(&ctx));
        let j = serde_json::to_value(&reply)?;
        assert_eq!(j.get("kind").and_then(|k| k.as_str()), Some("answer"));
        assert_eq!(j.get("confidence").and_then(|k| k.as_str()), Some("high"));
        assert_eq!(j.get("source").and_then(|k| k.as_str()), Some("cr"));
        assert_eq!(j.get("cr_version").and_then(|k| k.as_str()), Some("20260819"));
        let cites = j.get("citations").and_then(|c| c.as_array()).cloned().unwrap_or_default();
        assert_eq!(cites.len(), 2);
        assert_eq!(
            cites.first().and_then(|c| c.get("url")).and_then(|u| u.as_str()),
            Some("https://yawgatog.com/resources/magic-rules/#R70215b")
        );
        assert_eq!(
            cites.get(1).and_then(|c| c.get("label")).and_then(|l| l.as_str()),
            Some("Ruling (2020-01-01) — Dark Confidant")
        );
        // Whitespace in quotes is collapsed for display.
        let without_ctx = answer(&v, None);
        let j = serde_json::to_value(&without_ctx)?;
        assert_eq!(
            j.get("citations")
                .and_then(|c| c.as_array())
                .and_then(|c| c.get(1))
                .and_then(|c| c.get("label"))
                .and_then(|l| l.as_str()),
            Some("Scryfall ruling")
        );
        Ok(())
    }

    #[test]
    fn prior_call_and_oracle_text_citations_shape() -> Result<(), Box<dyn std::error::Error>> {
        let quote = "some  prior\nanswer";
        let view = citation_view(
            &Citation::PriorCall { id: CallId::new(Uuid::from_u128(3)), quote: judge_core::Quote::try_new(quote)? },
            None,
        );
        assert_eq!(view.label, "Prior call");
        assert_eq!(view.url, None);
        assert_eq!(view.quote, "some prior answer");
        let view = citation_view(
            &Citation::OracleText { card: CardId::new(Uuid::from_u128(4)), face: 1, quote: judge_core::Quote::try_new("Flying")? },
            None,
        );
        assert_eq!(view.label, "Oracle text, face 2");
        assert!(view.url.is_some_and(|u| u.contains("oracleid")));
        Ok(())
    }

    #[test]
    fn errors_shape_by_kind() -> Result<(), Box<dyn std::error::Error>> {
        let spans = NonEmpty::new(Ambiguous {
            query: "urza".into(),
            candidates: NonEmpty::from((
                card(1, "Urza's Mine"),
                (2..8).map(|i| card(i, &format!("Urza {i}"))).collect(),
            )),
        });
        let j = serde_json::to_value(error(&JudgeError::AmbiguousCards(spans)))?;
        assert_eq!(j.get("kind").and_then(|k| k.as_str()), Some("ambiguous"));
        let span = j
            .get("spans")
            .and_then(|s| s.as_array())
            .and_then(|s| s.first())
            .cloned()
            .unwrap_or_default();
        assert_eq!(span.get("query").and_then(|q| q.as_str()), Some("urza"));
        assert_eq!(span.get("choices").and_then(|c| c.as_array()).map(Vec::len), Some(MAX_CHOICES));
        assert_eq!(span.get("truncated").and_then(serde_json::Value::as_bool), Some(true));

        let j = serde_json::to_value(error(&JudgeError::CardsNotFound(NonEmpty::new("Xyzzy".to_owned()))))?;
        assert_eq!(j.get("kind").and_then(|k| k.as_str()), Some("not_found"));
        assert_eq!(
            j.get("names").and_then(|n| n.as_array()).and_then(|n| n.first()).and_then(|n| n.as_str()),
            Some("Xyzzy")
        );

        let j = serde_json::to_value(error(&JudgeError::OutOfScope(Source::Tournament)))?;
        assert_eq!(j.get("kind").and_then(|k| k.as_str()), Some("error"));
        assert_eq!(j.get("message").and_then(|m| m.as_str()), Some(render::OUT_OF_SCOPE));

        // Internal details never leak.
        let j = serde_json::to_value(error(&JudgeError::Upstream(anyhow::anyhow!("secret detail"))))?;
        assert!(!j.to_string().contains("secret"));
        Ok(())
    }
}
