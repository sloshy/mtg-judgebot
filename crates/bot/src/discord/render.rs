//! Pure rendering: a validated verdict or a [`JudgeError`] → the text of a
//! Discord reply. No serenity types here, so every branch is unit-testable
//! and Discord's size limits are enforced by construction (`fit`).

use std::fmt::Write as _;

use judge_core::{Ambiguous, Citation, Confidence, JudgeError, Score, Validated, Verdict};
use nonempty::NonEmpty;

/// Discord's limit on message `content`, in characters.
pub const CONTENT_LIMIT: usize = 2000;
/// Discord's limit on an embed description, in characters.
pub const EMBED_DESCRIPTION_LIMIT: usize = 4096;
/// Discord's limit on a button label, in characters.
pub const BUTTON_LABEL_LIMIT: usize = 80;
/// Most "did you mean…?" choices offered (one action row holds five buttons).
pub const MAX_CHOICES: usize = 5;
/// Appended to text that had to be cut to fit.
pub const TRUNCATION_MARKER: &str = " […]";
/// Longest quote shown per citation line.
pub const QUOTE_LIMIT: usize = 240;

/// Reply when every judge slot is taken.
pub const BUSY: &str =
    "I'm answering as many questions as I can right now; please try again in a minute.";
/// Fixed reply for tournament-policy and non-rules questions (ARCHITECTURE.md §3 step 3).
pub const OUT_OF_SCOPE: &str = "I only answer Comprehensive Rules and Commander rules questions. Tournament policy \
                                (MTR/IPG) and questions that aren't about the rules are out of my scope: for \
                                policy, see the Magic Tournament Rules or ask a tournament judge.";
/// Generic failure: the pipeline errored or could not produce a verified answer.
pub const FAILED: &str = "Sorry, I couldn't produce a verified answer this time. Please try again, or rephrase the question.";
/// The operator's Anthropic spend cap has been reached.
pub const SPEND_CAP: &str =
    "I've hit my spending cap for now, so I can't answer until the operator raises it.";
/// A "did you mean…?" was answered too late (or twice).
pub const EXPIRED: &str =
    "That question has expired (choices are kept for ten minutes). Please ask it again.";
/// Someone other than the asker pressed a "did you mean…?" button.
pub const NOT_YOURS: &str = "Only the person who asked can pick a card for that question.";
/// A button whose `custom_id` did not parse.
pub const UNKNOWN_BUTTON: &str = "I don't recognise that button any more.";
/// The rating could not be stored.
pub const RATE_FAILED: &str = "Sorry, I couldn't record that rating. Please try again.";
/// A card pick was processed but the message could not be updated with the result.
pub const EDIT_FAILED: &str =
    "I worked out an answer but couldn't update the message. Please ask the question again.";

/// A rendered answer: message content plus the embed holding the citations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// Message content: the answer text, at most [`CONTENT_LIMIT`] chars.
    pub content: String,
    /// Embed description: one `[702.19b] “…”` line per citation, at most
    /// [`EMBED_DESCRIPTION_LIMIT`] chars.
    pub citations: String,
    /// Embed footer: confidence and CR version.
    pub footer: String,
}

/// A rendered "did you mean…?" for the first ambiguous span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DidYouMean {
    /// Message content listing the choices.
    pub content: String,
    /// Full card names, at most [`MAX_CHOICES`], in button order.
    pub choices: Vec<String>,
}

/// Render a validated verdict.
#[must_use]
pub fn answer(v: &Verdict<Validated>) -> Answer {
    let lines: Vec<String> = v.citations().iter().map(citation_line).collect();
    Answer {
        content: fit(v.answer(), CONTENT_LIMIT),
        citations: fit_lines(&lines, EMBED_DESCRIPTION_LIMIT),
        footer: footer(v),
    }
}

/// `Confidence: High · CR 2026-08-19`.
#[must_use]
pub fn footer(v: &Verdict<Validated>) -> String {
    format!(
        "Confidence: {} · CR {}",
        confidence_label(v.confidence()),
        cr_date(v.cr_version().as_ref())
    )
}

/// One compact citation line: `[702.19b] “quote”`.
#[must_use]
pub fn citation_line(c: &Citation) -> String {
    let quote = fit(&collapse_whitespace(c.quote()), QUOTE_LIMIT);
    match c {
        Citation::Rule { id, .. } => format!("[{id}] “{quote}”"),
        Citation::ScryfallRuling { idx, .. } => {
            format!("[Scryfall ruling #{}] “{quote}”", idx.saturating_add(1))
        }
        Citation::PriorCall { .. } => format!("[prior call] “{quote}”"),
        Citation::OracleText { face: 0, .. } => format!("[Oracle text] “{quote}”"),
        Citation::OracleText { face, .. } => {
            format!("[Oracle text, face {}] “{quote}”", face.saturating_add(1))
        }
    }
}

/// Render the "did you mean…?" for the *first* ambiguous span; the buttons
/// carry `choices` in order.
#[must_use]
pub fn did_you_mean(spans: &NonEmpty<Ambiguous>) -> DidYouMean {
    let first = spans.first();
    let choices: Vec<String> = first
        .candidates
        .iter()
        .map(|c| c.name.clone())
        .take(MAX_CHOICES)
        .collect();
    let mut content = format!(
        "I'm not sure which card you mean by **{}**. Did you mean…?",
        first.query
    );
    for (i, name) in choices.iter().enumerate() {
        // Writing to a String cannot fail.
        let _ = write!(content, "\n{}. {name}", i + 1);
    }
    if first.candidates.len() > MAX_CHOICES {
        content.push_str(
            "\n(Showing the first five. If yours isn't here, write its full name as [[Card Name]].)",
        );
    }
    if spans.len() > 1 {
        let rest: Vec<String> = spans
            .iter()
            .skip(1)
            .map(|a| format!("**{}**", a.query))
            .collect();
        let _ = write!(content, "\nAfter that I'll ask about {}.", rest.join(", "));
    }
    DidYouMean {
        content: fit(&content, CONTENT_LIMIT),
        choices,
    }
}

/// The message for a failed `judge()`. Exhaustive over [`JudgeError`], so a
/// new variant must be rendered before this compiles. `AmbiguousCards` gives
/// the "did you mean…?" text; the buttons come from [`did_you_mean`].
#[must_use]
pub fn error(e: &JudgeError) -> String {
    match e {
        JudgeError::AmbiguousCards(spans) => did_you_mean(spans).content,
        JudgeError::CardsNotFound(names) => {
            let listed: Vec<String> = names.iter().map(|n| format!("**{n}**")).collect();
            fit(
                &format!(
                    "I couldn't find a card called {}. Check the spelling, or write the full name as [[Card Name]].",
                    listed.join(" or ")
                ),
                CONTENT_LIMIT,
            )
        }
        JudgeError::OutOfScope(_) => OUT_OF_SCOPE.to_owned(),
        JudgeError::Upstream(err) if is_spend_cap(err) => SPEND_CAP.to_owned(),
        JudgeError::BadCitation(_)
        | JudgeError::EmptyVerdict(_)
        | JudgeError::LlmRefused
        | JudgeError::Upstream(_) => FAILED.to_owned(),
    }
}

fn is_spend_cap(err: &anyhow::Error) -> bool {
    err.downcast_ref::<judge_anthropic::ClientError>()
        .is_some_and(|c| matches!(c, judge_anthropic::ClientError::SpendCapExceeded { .. }))
}

/// Label of a rating button.
#[must_use]
pub const fn rating_label(score: Score) -> &'static str {
    match score {
        Score::Incorrect => "Incorrect",
        Score::Partial => "Partially correct",
        Score::Correct => "Correct",
    }
}

/// Ephemeral acknowledgement of a rating.
#[must_use]
pub fn rated(score: Score, is_judge: bool) -> String {
    let who = if is_judge { " as a judge ruling" } else { "" };
    format!(
        "Recorded your rating{who}: {}. Rating again replaces it.",
        rating_label(score)
    )
}

/// Shown on the "did you mean…?" message while the pick is being judged.
#[must_use]
pub fn working(card: &str) -> String {
    fit(
        &format!("Got it: **{card}**. Working on it…"),
        CONTENT_LIMIT,
    )
}

/// A button label: the card name, cut to [`BUTTON_LABEL_LIMIT`] chars.
#[must_use]
pub fn button_label(name: &str) -> String {
    let name = name.trim();
    if name.chars().count() <= BUTTON_LABEL_LIMIT {
        return name.to_owned();
    }
    let mut s: String = name.chars().take(BUTTON_LABEL_LIMIT - 1).collect();
    s.push('…');
    s
}

/// `text`, cut to at most `limit` chars with [`TRUNCATION_MARKER`] at the end
/// if anything was dropped. Never panics; a `limit` smaller than the marker
/// simply hard-cuts.
#[must_use]
pub fn fit(text: &str, limit: usize) -> String {
    let text = text.trim_end();
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let marker = TRUNCATION_MARKER.chars().count();
    if limit < marker {
        return text.chars().take(limit).collect();
    }
    let mut out: String = text.chars().take(limit - marker).collect();
    let kept = out.trim_end().len();
    out.truncate(kept);
    out.push_str(TRUNCATION_MARKER);
    out
}

/// Join `lines` with newlines, dropping whole trailing lines (rather than
/// cutting one in half) so the result fits `limit`; says how many were dropped.
#[must_use]
pub fn fit_lines(lines: &[String], limit: usize) -> String {
    let mut out = String::new();
    let mut shown = 0usize;
    for line in lines {
        let candidate_len =
            out.chars().count() + usize::from(!out.is_empty()) + line.chars().count();
        // Leave room for the "… and N more" note if this is not the last line.
        let reserve = if shown + 1 < lines.len() { 24 } else { 0 };
        if candidate_len + reserve > limit {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        shown += 1;
    }
    let dropped = lines.len() - shown;
    if dropped > 0 {
        let note = format!("… and {dropped} more");
        if out.is_empty() {
            // Even the first line did not fit: cut it rather than show nothing.
            return fit(lines.first().map_or("", String::as_str), limit);
        }
        out.push('\n');
        out.push_str(&note);
    }
    fit(&out, limit)
}

const fn confidence_label(c: Confidence) -> &'static str {
    match c {
        Confidence::Low => "Low",
        Confidence::Medium => "Medium",
        Confidence::High => "High",
    }
}

/// `20260819` → `2026-08-19` (the `CrVersion` invariant guarantees eight ASCII digits).
fn cr_date(v: &str) -> String {
    match (v.get(0..4), v.get(4..6), v.get(6..8)) {
        (Some(y), Some(m), Some(d)) if v.len() == 8 => format!("{y}-{m}-{d}"),
        _ => v.to_owned(),
    }
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_core::{
        AnswerableSource, Card, CardId, Category, Context, CrVersion, EmptyVerdict, Face, Layout,
        RuleChunk, RuleId, Source,
    };
    use uuid::Uuid;

    type Res = Result<(), Box<dyn std::error::Error>>;

    fn rule(body: &str) -> Result<RuleChunk, Box<dyn std::error::Error>> {
        Ok(RuleChunk {
            id: RuleId::try_new("702.19b".to_owned())?,
            parent_id: None,
            subsection: RuleId::try_new("702".to_owned())?,
            heading: "Trample".into(),
            body: body.into(),
            examples: vec![],
            cr_version: CrVersion::try_new("20260819".to_owned())?,
        })
    }

    fn validated(
        answer: &str,
        n_citations: usize,
    ) -> Result<Verdict<Validated>, Box<dyn std::error::Error>> {
        let body = "The controller of an attacking creature with trample first assigns the combat damage to the \
                    creature(s) blocking it.";
        let ctx = Context {
            rules: vec![rule(body)?],
            ..Context::default()
        };
        let id = RuleId::try_new("702.19b".to_owned())?;
        let citations = vec![
            Citation::Rule {
                id,
                quote: body.into()
            };
            n_citations
        ];
        let v = Verdict::new(answer.into(), Confidence::High, citations, Category::Combat);
        Ok(v.validate(&ctx, AnswerableSource::Cr)?)
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
                type_line: "Land".into(),
            }),
        }
    }

    const LONG_ENOUGH: &str =
        "Trample assigns lethal damage to blockers first, and the rest to the player.";

    #[test]
    fn fit_keeps_short_text_and_marks_cut_text() {
        assert_eq!(fit("hello  ", 10), "hello");
        let long = "x".repeat(50);
        let cut = fit(&long, 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with(TRUNCATION_MARKER));
        // Multi-byte chars count as one each and never split.
        let uni = "é".repeat(30);
        let cut = fit(&uni, 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with(TRUNCATION_MARKER));
        // A limit smaller than the marker hard-cuts instead of overflowing.
        assert_eq!(fit("abcdef", 2), "ab");
        assert_eq!(fit("abcdef", 0), "");
    }

    #[test]
    fn answer_respects_both_limits() -> Res {
        let v = validated(&"word ".repeat(1000), 60)?;
        let a = answer(&v);
        assert!(
            a.content.chars().count() <= CONTENT_LIMIT,
            "{}",
            a.content.len()
        );
        assert!(a.content.ends_with(TRUNCATION_MARKER));
        assert!(a.citations.chars().count() <= EMBED_DESCRIPTION_LIMIT);
        assert!(a.citations.contains("… and "), "{}", a.citations);
        assert!(a.citations.starts_with("[702.19b] “The controller"));
        assert_eq!(a.footer, "Confidence: High · CR 2026-08-19");
        Ok(())
    }

    #[test]
    fn short_answer_is_verbatim_with_one_citation_per_line() -> Res {
        let v = validated(LONG_ENOUGH, 2)?;
        let a = answer(&v);
        assert_eq!(a.content, LONG_ENOUGH);
        assert_eq!(a.citations.lines().count(), 2);
        assert!(
            a.citations
                .lines()
                .all(|l| l.starts_with("[702.19b] “") && l.ends_with('”'))
        );
        Ok(())
    }

    #[test]
    fn citation_lines_for_every_kind() {
        let id = CardId::new(Uuid::from_u128(1));
        let r = Citation::ScryfallRuling {
            card: id,
            idx: 0,
            quote: "a  ruling\nwith   space".into(),
        };
        assert_eq!(
            citation_line(&r),
            "[Scryfall ruling #1] “a ruling with space”"
        );
        let p = Citation::PriorCall {
            id: judge_core::CallId::new(Uuid::from_u128(2)),
            quote: "x".repeat(300),
        };
        let line = citation_line(&p);
        assert!(line.starts_with("[prior call] “"));
        assert!(line.chars().count() <= QUOTE_LIMIT + 20);
        assert!(line.contains(TRUNCATION_MARKER));
        let o = Citation::OracleText {
            card: id,
            face: 0,
            quote: "Flying".into(),
        };
        assert_eq!(citation_line(&o), "[Oracle text] “Flying”");
        let back = Citation::OracleText {
            card: id,
            face: 1,
            quote: "Insectile".into(),
        };
        assert_eq!(citation_line(&back), "[Oracle text, face 2] “Insectile”");
    }

    #[test]
    fn fit_lines_drops_whole_lines() {
        let lines: Vec<String> = (0..10)
            .map(|i| format!("line {i} {}", "x".repeat(30)))
            .collect();
        let out = fit_lines(&lines, 120);
        assert!(out.chars().count() <= 120, "{out}");
        assert!(
            out.lines()
                .next()
                .is_some_and(|l| l == lines.first().map_or("", String::as_str))
        );
        assert!(out.ends_with(" more"), "{out}");
        assert_eq!(fit_lines(&[], 100), "");
        // One over-long line is cut rather than dropped.
        let one = vec!["y".repeat(500)];
        let out = fit_lines(&one, 50);
        assert_eq!(out.chars().count(), 50);
    }

    fn ambiguous(query: &str, names: &[&str]) -> Ambiguous {
        let cards: Vec<Card> = names
            .iter()
            .enumerate()
            .map(|(i, n)| card(i as u128 + 1, n))
            .collect();
        Ambiguous {
            query: query.into(),
            candidates: NonEmpty::from_vec(cards)
                .unwrap_or_else(|| NonEmpty::new(card(99, "fallback"))),
        }
    }

    #[test]
    fn ambiguous_cards_render_did_you_mean_with_at_most_five_choices() {
        let seven = [
            "Urza's Mine",
            "Urza's Tower",
            "Urza's Power Plant",
            "Urza's Saga",
            "Urza's Bauble",
            "Urza's Rage",
            "Urza, Lord High Artificer",
        ];
        let spans = NonEmpty::from((
            ambiguous("urza", &seven),
            vec![ambiguous("bob", &["Dark Confidant", "Bob's Burgers"])],
        ));
        let dym = did_you_mean(&spans);
        assert_eq!(dym.choices.len(), MAX_CHOICES);
        assert_eq!(dym.choices.first().map(String::as_str), Some("Urza's Mine"));
        assert!(dym.content.contains("**urza**") && dym.content.contains("Did you mean"));
        assert!(dym.content.contains("5. Urza's Bauble") && !dym.content.contains("Urza's Rage"));
        assert!(dym.content.contains("Showing the first five"));
        assert!(dym.content.contains("ask about **bob**"), "{}", dym.content);
        // error() renders the same text for the variant.
        assert_eq!(error(&JudgeError::AmbiguousCards(spans)), dym.content);
        // A single-span, two-candidate case has no trailer.
        let one = NonEmpty::new(ambiguous(
            "bruna",
            &["Bruna, the Fading Light", "Bruna, Light of Alabaster"],
        ));
        let dym = did_you_mean(&one);
        assert_eq!(dym.choices.len(), 2);
        assert!(!dym.content.contains("After that") && !dym.content.contains("Showing"));
    }

    #[test]
    fn every_error_variant_has_a_message() -> Res {
        let not_found = JudgeError::CardsNotFound(NonEmpty::from((
            "Xyzzy".to_owned(),
            vec!["Plugh".to_owned()],
        )));
        let m = error(&not_found);
        assert!(
            m.contains("**Xyzzy**") && m.contains("**Plugh**") && m.contains("[[Card Name]]"),
            "{m}"
        );

        for s in [Source::Tournament, Source::OutOfScope] {
            assert_eq!(error(&JudgeError::OutOfScope(s)), OUT_OF_SCOPE);
        }
        let bad = Citation::Rule {
            id: RuleId::try_new("702.19b".to_owned())?,
            quote: "nope".into(),
        };
        assert_eq!(error(&JudgeError::BadCitation(bad)), FAILED);
        assert_eq!(
            error(&JudgeError::EmptyVerdict(EmptyVerdict::NoCitations)),
            FAILED
        );
        assert_eq!(
            error(&JudgeError::EmptyVerdict(EmptyVerdict::ShortAnswer {
                chars: 3
            })),
            FAILED
        );
        assert_eq!(error(&JudgeError::LlmRefused), FAILED);
        assert_eq!(
            error(&JudgeError::Upstream(anyhow::anyhow!("db down"))),
            FAILED
        );
        let cap = judge_anthropic::ClientError::SpendCapExceeded {
            spent: 5.0,
            cap: 5.0,
        };
        assert_eq!(
            error(&JudgeError::Upstream(
                anyhow::Error::new(cap).context("extract")
            )),
            SPEND_CAP
        );
        // Nothing internal leaks into the generic message.
        assert!(!error(&JudgeError::Upstream(anyhow::anyhow!("secret detail"))).contains("secret"));
        Ok(())
    }

    #[test]
    fn labels() {
        assert_eq!(rating_label(Score::Incorrect), "Incorrect");
        assert_eq!(rating_label(Score::Partial), "Partially correct");
        assert_eq!(rating_label(Score::Correct), "Correct");
        assert_eq!(button_label("  Dark Confidant "), "Dark Confidant");
        let long = "Asmoranomardicadaistinaculdacar ".repeat(4);
        let l = button_label(&long);
        assert_eq!(l.chars().count(), BUTTON_LABEL_LIMIT);
        assert!(l.ends_with('…'));
        assert!(rated(Score::Correct, true).contains("judge"));
        assert!(!rated(Score::Correct, false).contains("judge"));
        assert!(working("Bob").contains("**Bob**"));
    }

    #[test]
    fn cr_date_formats_eight_digits_only() {
        assert_eq!(cr_date("20260819"), "2026-08-19");
        assert_eq!(cr_date("2026"), "2026");
    }
}
