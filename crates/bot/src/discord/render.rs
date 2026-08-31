//! Pure rendering: a validated verdict or a [`JudgeError`] → the text of a
//! Discord reply. No serenity types here, so every branch is unit-testable
//! and Discord's size limits are enforced by construction (`fit`).

use std::fmt::Write as _;

use judge_core::{
    Ambiguous, CardId, Citation, Confidence, Context, JudgeError, RuleId, Score, Validated,
    Verdict,
};
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
/// Longest question restated in the reply header.
pub const QUESTION_LIMIT: usize = 300;
/// HTML mirror of the Comprehensive Rules with one anchor per rule
/// (verified 2026-08-30: ids look like `R70219b`, see [`rule_anchor`]).
pub const CR_MIRROR_URL: &str = "https://yawgatog.com/resources/magic-rules/";

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

/// Render a validated verdict. The content restates the question (see
/// [`with_header`]); citation lines link into `ctx`'s cards where it has them.
#[must_use]
pub fn answer(v: &Verdict<Validated>, ctx: Option<&Context>, asker: u64, question: &str) -> Answer {
    let lines: Vec<String> = v.citations().iter().map(|c| citation_line(c, ctx)).collect();
    Answer {
        content: with_header(asker, question, v.answer()),
        citations: fit_lines(&lines, EMBED_DESCRIPTION_LIMIT),
        footer: footer(v),
    }
}

/// `<@asker> asked: <question>`, the question collapsed to one line and cut to
/// [`QUESTION_LIMIT`] chars. The `<@…>` mention renders in Discord; whether it
/// *pings* is decided by the message's allowed-mentions, not here.
#[must_use]
pub fn header(asker: u64, question: &str) -> String {
    format!(
        "<@{asker}> asked: {}",
        fit(&collapse_whitespace(question), QUESTION_LIMIT)
    )
}

/// [`header`], a blank line, then `body`, the whole fit to [`CONTENT_LIMIT`]
/// (so an over-long body is cut, never the header).
#[must_use]
pub fn with_header(asker: u64, question: &str, body: &str) -> String {
    fit(&format!("{}\n\n{body}", header(asker, question)), CONTENT_LIMIT)
}

/// Anchor of a rule on [`CR_MIRROR_URL`]: `R` plus the id with its dots
/// removed. Observed on the live page 2026-08-30: `100.1` → `id=R1001`,
/// `702.19b` → `id=R70219b`, `613.1a` → `id=R6131a`, `702` → `id=R702`,
/// `704.5aa` → `id=R7045aa` (3318 such ids, one per rule).
#[must_use]
pub fn rule_anchor(id: &RuleId) -> String {
    format!("R{}", id.as_ref().replace('.', ""))
}

/// Deep link to a rule: [`CR_MIRROR_URL`] plus `#` and [`rule_anchor`].
#[must_use]
pub fn rule_url(id: &RuleId) -> String {
    format!("{CR_MIRROR_URL}#{}", rule_anchor(id))
}

/// Scryfall page for an oracle id. The `/card/<uuid>` form 404s; this search
/// form 303-redirects to the card's search result (verified 2026-08-30).
#[must_use]
pub fn scryfall_url(card: CardId) -> String {
    format!("https://scryfall.com/search?q=oracleid%3A{card}")
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

/// One compact citation line: `[702.19b](…rule url…) “quote”`. Rule and card
/// references are markdown links (embed descriptions render those; plain
/// message content does not, so these lines must stay in the embed). Card
/// names come from `ctx` when it holds the card; the link works either way,
/// since the URL only needs the oracle id the citation itself carries.
#[must_use]
pub fn citation_line(c: &Citation, ctx: Option<&Context>) -> String {
    let quote = fit(&collapse_whitespace(c.quote()), QUOTE_LIMIT);
    let card_name = |id: CardId| ctx.and_then(|x| x.card(id)).map(|card| card.name.clone());
    match c {
        Citation::Rule { id, .. } => format!("[{id}]({}) “{quote}”", rule_url(id)),
        Citation::ScryfallRuling { card, idx, .. } => {
            let n = idx.saturating_add(1);
            let label = match card_name(*card) {
                Some(name) => format!("Ruling #{n} — {name}"),
                None => format!("Scryfall ruling #{n}"),
            };
            format!("[{label}]({}) “{quote}”", scryfall_url(*card))
        }
        Citation::PriorCall { .. } => format!("[prior call] “{quote}”"),
        Citation::OracleText { card, face, .. } => {
            let face_note = match face {
                0 => String::new(),
                n => format!(", face {}", n.saturating_add(1)),
            };
            let label = match card_name(*card) {
                Some(name) => format!("Oracle text{face_note} — {name}"),
                None => format!("Oracle text{face_note}"),
            };
            format!("[{label}]({}) “{quote}”", scryfall_url(*card))
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

    const ASKER: u64 = 110_372_470_472_613_888;

    #[test]
    fn header_mentions_the_asker_and_truncates_the_question() {
        let h = header(ASKER, "Does  trample\nwork here?");
        assert_eq!(
            h,
            "<@110372470472613888> asked: Does trample work here?"
        );
        let long = "why ".repeat(200);
        let h = header(ASKER, &long);
        assert!(h.starts_with("<@110372470472613888> asked: why why"));
        assert!(h.ends_with(TRUNCATION_MARKER), "{h}");
        assert!(
            h.chars().count() <= QUESTION_LIMIT + "<@110372470472613888> asked: ".len(),
            "{h}"
        );
        assert_eq!(h.lines().count(), 1, "the header is a single line");
    }

    #[test]
    fn with_header_prefixes_and_keeps_the_content_budget() {
        let c = with_header(ASKER, "short?", "The answer.");
        assert_eq!(c, "<@110372470472613888> asked: short?\n\nThe answer.");
        // An over-long body is cut from the end; the header survives intact.
        let c = with_header(ASKER, &"q ".repeat(400), &"body ".repeat(1000));
        assert!(c.chars().count() <= CONTENT_LIMIT, "{}", c.len());
        assert!(c.starts_with("<@110372470472613888> asked: q q"));
        assert!(c.contains("\n\nbody"));
        assert!(c.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn answer_respects_both_limits() -> Res {
        let v = validated(&"word ".repeat(1000), 60)?;
        let a = answer(&v, None, ASKER, "does a long answer still fit?");
        assert!(
            a.content.chars().count() <= CONTENT_LIMIT,
            "{}",
            a.content.len()
        );
        assert!(a.content.starts_with("<@110372470472613888> asked: does a long answer"));
        assert!(a.content.ends_with(TRUNCATION_MARKER));
        assert!(a.citations.chars().count() <= EMBED_DESCRIPTION_LIMIT);
        assert!(a.citations.contains("… and "), "{}", a.citations);
        assert!(a.citations.starts_with(
            "[702.19b](https://yawgatog.com/resources/magic-rules/#R70219b) “The controller"
        ));
        assert_eq!(a.footer, "Confidence: High · CR 2026-08-19");
        Ok(())
    }

    #[test]
    fn short_answer_follows_the_header_with_one_citation_per_line() -> Res {
        let v = validated(LONG_ENOUGH, 2)?;
        let a = answer(&v, None, ASKER, "trample?");
        assert_eq!(
            a.content,
            format!("<@110372470472613888> asked: trample?\n\n{LONG_ENOUGH}")
        );
        assert_eq!(a.citations.lines().count(), 2);
        assert!(a.citations.lines().all(|l| {
            l.starts_with("[702.19b](https://yawgatog.com/resources/magic-rules/#R70219b) “")
                && l.ends_with('”')
        }));
        Ok(())
    }

    #[test]
    fn rule_anchor_matches_the_mirror_ids() -> Res {
        // Formats observed in the live page's HTML on 2026-08-30.
        for (id, anchor) in [
            ("100.1", "R1001"),
            ("702.19b", "R70219b"),
            ("613.1a", "R6131a"),
            ("702", "R702"),
            ("704.5aa", "R7045aa"),
        ] {
            assert_eq!(rule_anchor(&RuleId::try_new(id.to_owned())?), anchor);
        }
        assert_eq!(
            rule_url(&RuleId::try_new("100.1".to_owned())?),
            "https://yawgatog.com/resources/magic-rules/#R1001"
        );
        Ok(())
    }

    #[test]
    fn citation_lines_for_every_kind() {
        let id = CardId::new(Uuid::from_u128(1));
        let url = "https://scryfall.com/search?q=oracleid%3A00000000-0000-0000-0000-000000000001";
        assert_eq!(scryfall_url(id), url);
        let ctx = Context {
            cards: vec![card(1, "Dark Confidant")],
            ..Context::default()
        };
        let r = Citation::ScryfallRuling {
            card: id,
            idx: 0,
            quote: "a  ruling\nwith   space".into(),
        };
        assert_eq!(
            citation_line(&r, Some(&ctx)),
            format!("[Ruling #1 — Dark Confidant]({url}) “a ruling with space”")
        );
        // Without a context the link still works; only the name is missing.
        assert_eq!(
            citation_line(&r, None),
            format!("[Scryfall ruling #1]({url}) “a ruling with space”")
        );
        let p = Citation::PriorCall {
            id: judge_core::CallId::new(Uuid::from_u128(2)),
            quote: "x".repeat(300),
        };
        let line = citation_line(&p, Some(&ctx));
        assert!(line.starts_with("[prior call] “"));
        assert!(line.chars().count() <= QUOTE_LIMIT + 20);
        assert!(line.contains(TRUNCATION_MARKER));
        let o = Citation::OracleText {
            card: id,
            face: 0,
            quote: "Flying".into(),
        };
        assert_eq!(
            citation_line(&o, Some(&ctx)),
            format!("[Oracle text — Dark Confidant]({url}) “Flying”")
        );
        assert_eq!(citation_line(&o, None), format!("[Oracle text]({url}) “Flying”"));
        let back = Citation::OracleText {
            card: id,
            face: 1,
            quote: "Insectile".into(),
        };
        assert_eq!(
            citation_line(&back, Some(&ctx)),
            format!("[Oracle text, face 2 — Dark Confidant]({url}) “Insectile”")
        );
    }

    #[test]
    fn linked_citations_still_fit_the_embed_budget() {
        // Worst case: every line carries a long name, a full URL and a full
        // quote; fit_lines must count the whole markdown, not just the label.
        let id = CardId::new(Uuid::from_u128(7));
        let ctx = Context {
            cards: vec![card(7, &"Asmoranomardicadaistinaculdacar ".repeat(3))],
            ..Context::default()
        };
        let lines: Vec<String> = (0..40)
            .map(|i| {
                citation_line(
                    &Citation::ScryfallRuling {
                        card: id,
                        idx: i,
                        quote: "q".repeat(300),
                    },
                    Some(&ctx),
                )
            })
            .collect();
        assert!(lines.iter().all(|l| l.chars().count() > 300));
        let out = fit_lines(&lines, EMBED_DESCRIPTION_LIMIT);
        assert!(out.chars().count() <= EMBED_DESCRIPTION_LIMIT, "{}", out.len());
        assert!(out.contains("… and "), "some lines must have been dropped");
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
