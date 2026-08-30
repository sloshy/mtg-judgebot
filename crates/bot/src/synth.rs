//! `Synthesizer` adapter over `judge_anthropic::Synth` (pipeline step 5).
//!
//! The typestate client owns the wire protocol; this module owns the prompt
//! and the bookkeeping around the single `lookup_rules` round:
//!
//! * the system prompt (`prompts/synth_system.md`, stable, so it carries a
//!   prompt-cache breakpoint) and the user turn rendered from `Context` under
//!   a [`Budget`] as two text blocks: the material (with its own cache
//!   breakpoint, so the tool-round continuation rereads it at the cache
//!   price) and the question;
//! * chunks fetched by the tool round go into `Context` (`rules` for
//!   validation, `tool_round` so that the one retry `judge()` may make
//!   renders them regardless of the budget, with the tool disabled);
//! * a response truncated at `max_tokens` is retried once at `Effort::Medium`;
//! * a citation of a lettered sub-rule (`702.19b`) whose rule-level parent
//!   (`702.19`, whose body folds the sub-rule text in) was shown is
//!   hydrated from the store, because `Verdict::validate` looks the cited id
//!   up in `Context` and the retriever legs only return rule-level rows.

use std::{borrow::Cow, fmt::Write as _, sync::Arc};

use async_trait::async_trait;
use judge_anthropic::{
    Client, SendOutcome, Synth, SynthConfig, Truncated,
    wire::{CacheControl, ContentBlock, Effort},
};
use judge_core::{
    Card, CardId, Citation, Context, EmptyVerdict, JudgeError, Question, Rejection, Retriever, RuleChunk, RuleId,
    Synthesizer, Unvalidated, Verdict,
};

/// The system prompt. Stable across requests, so `Synth::new` puts the
/// cache breakpoint on it.
pub const SYSTEM_PROMPT: &str = include_str!("prompts/synth_system.md");

/// Size caps for the rendered user turn. Chunks pinned by the tool round
/// are exempt from the CR caps but count towards them, so the rest of the
/// material shrinks to fit.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    /// Rule chunks rendered in the CR section.
    pub max_rule_chunks: usize,
    /// Bytes of rule text (body + examples) rendered in the CR section.
    pub max_rule_chars: usize,
    /// Scryfall rulings rendered per card.
    pub max_rulings_per_card: usize,
    /// Earlier Q&A pairs of the thread rendered (the most recent ones).
    pub max_history: usize,
    /// Bytes kept of each earlier answer.
    pub max_history_answer_chars: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_rule_chunks: 25,
            max_rule_chars: 30_000,
            max_rulings_per_card: 20,
            max_history: 4,
            max_history_answer_chars: 600,
        }
    }
}

/// `Synthesizer` over the Anthropic Messages API.
pub struct AnthropicSynthesizer {
    client: Client,
    cfg: SynthConfig,
    retriever: Arc<dyn Retriever>,
    system_prompt: String,
    budget: Budget,
}

impl AnthropicSynthesizer {
    /// With [`SYSTEM_PROMPT`] and the default [`Budget`].
    #[must_use]
    pub fn new(client: Client, cfg: SynthConfig, retriever: Arc<dyn Retriever>) -> Self {
        Self {
            client,
            cfg,
            retriever,
            system_prompt: SYSTEM_PROMPT.to_owned(),
            budget: Budget::default(),
        }
    }

    /// Override the user-turn budget.
    #[must_use]
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// Fetch cited sub-rules (`702.19b`) whose rule-level parent (`702.19`) is
    /// in Context, so `validate` can check the quote against the leaf's own
    /// row. Leaves whose parent was never shown are left alone: the model
    /// cannot have read them, and validation should reject the citation.
    async fn hydrate_leaf_citations(&self, v: &Verdict<Unvalidated>, ctx: &mut Context) -> Result<(), JudgeError> {
        let mut wanted: Vec<RuleId> = Vec::new();
        for c in v.citations() {
            if let Citation::Rule { id, .. } = c
                && ctx.rule(id).is_none()
                && !wanted.contains(id)
                && parent_of(id).is_some_and(|p| ctx.rule(&p).is_some())
            {
                wanted.push(id.clone());
            }
        }
        if wanted.is_empty() {
            return Ok(());
        }
        tracing::debug!(ids = ?wanted, "hydrating cited sub-rules");
        let chunks = self.retriever.lookup_rules(&wanted).await?;
        ctx.extend_rules(chunks);
        Ok(())
    }

    /// One synthesis conversation at `effort`. The first attempt may run the
    /// tool round; the retry after a rejected citation (which already has that
    /// round's chunks in `ctx`) may not.
    async fn converse(
        &self,
        q: &Question,
        ctx: &mut Context,
        rejected: Option<&Rejection>,
        effort: Effort,
    ) -> Result<Verdict<Unvalidated>, JudgeError> {
        let cfg = SynthConfig { effort, ..self.cfg.clone() };
        let user = user_turn(q, ctx, rejected, &self.budget);
        if rejected.is_some() {
            return Synth::new_final(self.client.clone(), &cfg, self.system_prompt.clone(), user).finish().await;
        }
        match Synth::new(self.client.clone(), &cfg, self.system_prompt.clone(), user).send().await? {
            SendOutcome::Done(v) => Ok(v),
            SendOutcome::ToolRequested(t) => {
                tracing::info!(requested = ?t.requested(), "lookup_rules tool round");
                let chunks = self.retriever.lookup_rules(t.requested()).await?;
                // Borrow for the tool result first, then move the chunks into Context.
                let f = t.answer_tool(&chunks);
                for c in &chunks {
                    if !ctx.tool_round.contains(&c.id) {
                        ctx.tool_round.push(c.id.clone());
                    }
                }
                ctx.extend_rules(chunks);
                f.finish().await
            }
        }
    }
}

#[async_trait]
impl Synthesizer for AnthropicSynthesizer {
    async fn answer(
        &self,
        q: &Question,
        ctx: &mut Context,
        rejected: Option<&Rejection>,
    ) -> Result<Verdict<Unvalidated>, JudgeError> {
        let first = self.converse(q, ctx, rejected, self.cfg.effort).await;
        let truncated = match &first {
            Err(JudgeError::Upstream(e)) if self.cfg.effort > Effort::Medium => e.downcast_ref::<Truncated>().map(|t| t.output_tokens),
            _ => None,
        };
        let verdict = match truncated {
            Some(output_tokens) => {
                tracing::warn!(output_tokens, max_tokens = self.cfg.max_tokens, "synthesis truncated; retrying at medium effort");
                self.converse(q, ctx, rejected, Effort::Medium).await?
            }
            None => first?,
        };
        self.hydrate_leaf_citations(&verdict, ctx).await?;
        Ok(verdict)
    }
}

/// `702.19b` → `702.19`; `None` for rule-level (`702.19`) and section (`702`) ids.
fn parent_of(id: &RuleId) -> Option<RuleId> {
    let s = id.as_ref();
    let stem = s.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    if stem == s { None } else { RuleId::try_new(stem.to_owned()).ok() }
}

/// Bytes a chunk contributes to the CR section.
fn chunk_size(r: &RuleChunk) -> usize {
    r.body.len() + r.examples.iter().map(String::len).sum::<usize>()
}

/// The chunks the CR section shows, in `ctx.rules` order (category map,
/// full-text, vector, tool round). A sub-rule whose parent is present is
/// folded into the parent and not shown. Pinned chunks (and the parents of
/// pinned sub-rules) are always shown and charged to the budget first; the
/// rest fill what remains, except that an over-sized first chunk is kept so
/// the section is never empty.
fn select_rules<'a>(ctx: &'a Context, pinned: &[RuleId], budget: &Budget) -> Vec<&'a RuleChunk> {
    let folded = |r: &RuleChunk| r.parent_id.as_ref().is_some_and(|p| ctx.rule(p).is_some());
    let is_pinned = |r: &RuleChunk| {
        pinned.contains(&r.id)
            || ctx.rules.iter().any(|leaf| leaf.parent_id.as_ref() == Some(&r.id) && pinned.contains(&leaf.id))
    };
    let candidates: Vec<&RuleChunk> = ctx.rules.iter().filter(|r| !folded(r)).collect();
    let (mut count, mut bytes) = (0usize, 0usize);
    for r in candidates.iter().filter(|r| is_pinned(r)) {
        count += 1;
        bytes += chunk_size(r);
    }
    let mut shown = Vec::with_capacity(candidates.len());
    let mut dropped: Vec<&str> = Vec::new();
    for r in candidates {
        if is_pinned(r) {
            shown.push(r);
            continue;
        }
        let size = chunk_size(r);
        let fits = count < budget.max_rule_chunks && (bytes + size <= budget.max_rule_chars || count == 0);
        if fits {
            count += 1;
            bytes += size;
            shown.push(r);
        } else {
            dropped.push(r.id.as_ref());
        }
    }
    if !dropped.is_empty() {
        tracing::debug!(kept = shown.len(), bytes, ?dropped, "CR section over budget");
    }
    shown
}

/// Cut `s` to at most `max` bytes on a char boundary, marking the cut.
fn truncate(s: &str, max: usize) -> Cow<'_, str> {
    if s.len() <= max {
        return Cow::Borrowed(s);
    }
    let end = s.char_indices().map(|(i, _)| i).take_while(|&i| i <= max).last().unwrap_or(0);
    Cow::Owned(format!("{}…", s.get(..end).unwrap_or_default()))
}

fn render_cards(s: &mut String, cards: &[Card]) {
    if cards.is_empty() {
        return;
    }
    s.push_str("## Cards (current Oracle text)\n");
    for c in cards {
        let _ = writeln!(s, "### {} — card {} — layout {:?}", c.name, c.id, c.layout);
        for f in &c.faces {
            let cost = if f.mana_cost.is_empty() { String::new() } else { format!(" {}", f.mana_cost) };
            let _ = writeln!(s, "**{}**{cost} — {}\n{}", f.name, f.type_line, f.oracle_text);
        }
    }
}

fn render_rules(s: &mut String, ctx: &Context, pinned: &[RuleId], budget: &Budget) {
    match ctx.cr_version() {
        Some(v) => {
            let _ = writeln!(s, "\n## Comprehensive Rules (effective {v})");
        }
        None => s.push_str("\n## Comprehensive Rules\n(no excerpts retrieved)\n"),
    }
    for r in select_rules(ctx, pinned, budget) {
        let _ = writeln!(s, "### [{}] {}\n{}", r.id, r.heading, r.body);
        for e in &r.examples {
            s.push_str(e);
            s.push('\n');
        }
    }
}

fn render_rulings(s: &mut String, ctx: &Context, budget: &Budget) {
    if ctx.rulings.is_empty() {
        return;
    }
    s.push_str("\n## Scryfall rulings (cite as scryfall_ruling with the card uuid from the heading and the idx)\n");
    for c in &ctx.cards {
        let mut shown = 0usize;
        let mut total = 0usize;
        for r in ctx.rulings.iter().filter(|r| r.card == c.id) {
            total += 1;
            if shown < budget.max_rulings_per_card {
                if shown == 0 {
                    let _ = writeln!(s, "### {} — card {}", c.name, c.id);
                }
                let _ = writeln!(s, "[ruling #{}] ({}) {}", r.idx, r.published_at, r.text);
                shown += 1;
            }
        }
        if total > shown {
            let _ = writeln!(s, "({} more rulings omitted)", total - shown);
        }
    }
    // Rulings for cards not in `ctx.cards` (a retriever quirk) are still citable.
    let mut orphan: Option<CardId> = None;
    for r in ctx.rulings.iter().filter(|r| !ctx.cards.iter().any(|c| c.id == r.card)) {
        if orphan != Some(r.card) {
            let _ = writeln!(s, "### card {}", r.card);
            orphan = Some(r.card);
        }
        let _ = writeln!(s, "[ruling #{}] ({}) {}", r.idx, r.published_at, r.text);
    }
}

fn render_aids(s: &mut String, ctx: &Context) {
    if !ctx.glossary.is_empty() {
        s.push_str("\n## Glossary (context only; not citable)\n");
        for g in &ctx.glossary {
            let _ = writeln!(s, "**{}**: {}", g.term, g.text);
        }
    }
    if !ctx.notes.is_empty() {
        s.push_str("\n## Notes on tricky cards (context only; not citable)\n");
        for n in &ctx.notes {
            let name = ctx.cards.iter().find(|c| c.id == n.card).map_or_else(|| n.card.to_string(), |c| c.name.clone());
            let _ = writeln!(s, "### {name}\n{}", n.note);
        }
    }
    if !ctx.prior.is_empty() {
        s.push_str("\n## Prior calls (examples only; the CR always outranks these)\n");
        for p in &ctx.prior {
            let _ = writeln!(
                s,
                "[call {}] rating {:.1}/3 ({} votes), CR {}\nQ: {}\nA: {}",
                p.id, p.rating, p.rating_count, p.cr_version, p.question, p.answer
            );
        }
    }
}

fn render_history(s: &mut String, ctx: &Context, budget: &Budget) {
    if ctx.history.is_empty() || budget.max_history == 0 {
        return;
    }
    s.push_str("\n## Earlier in this thread (context only; not citable)\n");
    let skip = ctx.history.len().saturating_sub(budget.max_history);
    for h in ctx.history.iter().skip(skip) {
        let _ = writeln!(s, "Q: {}\nA: {}", h.question, truncate(&h.answer, budget.max_history_answer_chars));
    }
}

fn render_rejection(s: &mut String, ctx: &Context, rejected: &Rejection) {
    s.push_str("\n## Previous attempt rejected\n");
    match rejected {
        Rejection::BadCitation(c) => {
            let why = match c {
                Citation::Rule { id, .. } if ctx.rule(id).is_none() => {
                    format!("rule {id} is not among the excerpts; cite an id exactly as shown in the excerpts")
                }
                Citation::ScryfallRuling { card, idx, .. } if ctx.ruling(*card, *idx).is_none() => {
                    format!("there is no ruling [ruling #{idx}] under a card heading with uuid {card} in the material")
                }
                Citation::PriorCall { id, .. } if ctx.prior_call(*id).is_none() => {
                    format!("there is no prior call labelled [call {id}] in the material")
                }
                _ => "the quote is not a verbatim substring of that source; copy the text exactly, within one line".to_owned(),
            };
            let _ = writeln!(s, "Your earlier answer cited {c}, which failed validation: {why}.");
        }
        Rejection::Empty(EmptyVerdict::NoCitations) => {
            s.push_str(
                "Your earlier answer had no citations, so it was rejected. Every answer must cite the rule(s) \
                 or ruling(s) from the material that decide the question; answer fully and cite them now.\n",
            );
        }
        Rejection::Empty(EmptyVerdict::ShortAnswer { .. }) => {
            s.push_str(
                "Your earlier answer was empty or too short to be an answer (a placeholder such as \
                 \"pending\" is not acceptable), so it was rejected. Answer the question fully, in plain \
                 prose, with citations.\n",
            );
        }
    }
}

/// The material part of the user turn. `pinned` chunks bypass the CR budget.
fn render_material(ctx: &Context, pinned: &[RuleId], budget: &Budget) -> String {
    let mut s = String::from("# Material\n");
    render_cards(&mut s, &ctx.cards);
    render_rules(&mut s, ctx, pinned, budget);
    render_rulings(&mut s, ctx, budget);
    render_aids(&mut s, ctx);
    render_history(&mut s, ctx, budget);
    s
}

/// The question part of the user turn, with the rejection notice if any.
fn render_question(q: &Question, ctx: &Context, rejected: Option<&Rejection>) -> String {
    let mut s = String::new();
    if let Some(c) = rejected {
        render_rejection(&mut s, ctx, c);
    }
    let _ = write!(s, "\n# Question\n{}\n", q.text);
    s
}

/// The user turn as two text blocks: the material (with a cache breakpoint,
/// so a tool-round continuation rereads it at the cache price) and the
/// question. On the retry after a rejected citation the tool-round chunks
/// in `ctx.tool_round` are pinned past the budget.
fn user_turn(q: &Question, ctx: &Context, rejected: Option<&Rejection>, budget: &Budget) -> Vec<ContentBlock> {
    let pinned: &[RuleId] = if rejected.is_some() { &ctx.tool_round } else { &[] };
    vec![
        ContentBlock::Text { text: render_material(ctx, pinned, budget), cache_control: Some(CacheControl::ephemeral()) },
        ContentBlock::text(render_question(q, ctx, rejected)),
    ]
}

/// The whole user turn as one string (rendering tests).
#[cfg(test)]
fn render_user_turn(q: &Question, ctx: &Context, rejected: Option<&Rejection>, pinned: &[RuleId], budget: &Budget) -> String {
    render_material(ctx, pinned, budget) + &render_question(q, ctx, rejected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_core::{AnswerableSource, Category, Confidence, CrVersion, Extraction, Face, Layout, Qa, Ruling, Source};
    use std::sync::{Mutex, PoisonError};
    use nonempty::NonEmpty;
    use serde_json::{Value, json};
    use uuid::Uuid;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, method, path},
    };

    type R = Result<(), Box<dyn std::error::Error>>;

    fn rid(id: &str) -> Result<RuleId, Box<dyn std::error::Error>> {
        Ok(RuleId::try_new(id.to_owned())?)
    }

    fn chunk(id: &str, parent: Option<&str>, body: &str) -> Result<RuleChunk, Box<dyn std::error::Error>> {
        let subsection = id.get(..3).unwrap_or("702");
        Ok(RuleChunk {
            id: rid(id)?,
            parent_id: parent.map(rid).transpose()?,
            subsection: rid(subsection)?,
            heading: "Heading".into(),
            body: body.into(),
            examples: vec![],
            cr_version: CrVersion::try_new("20250801".to_owned())?,
        })
    }

    fn card(n: u128, name: &str, oracle: &str) -> Card {
        Card {
            id: CardId::new(Uuid::from_u128(n)),
            name: name.into(),
            layout: Layout::Normal,
            faces: NonEmpty::new(Face {
                name: name.into(),
                oracle_text: oracle.into(),
                mana_cost: "{1}{B}".into(),
                type_line: "Creature — Human Wizard".into(),
            }),
        }
    }

    fn q() -> Question {
        Question { thread_id: "thread-1".into(), text: "does trample work with deathtouch?".into() }
    }

    fn at<'a>(v: &'a Value, p: &str) -> &'a Value {
        v.pointer(p).unwrap_or(&Value::Null)
    }

    // ---- rendering ----

    #[test]
    fn renders_cards_version_rulings_history_and_rejection() -> R {
        let bob = card(7, "Dark Confidant", "At the beginning of your upkeep, reveal the top card of your library.");
        let ctx = Context {
            cards: vec![bob],
            rules: vec![chunk("702.15", None, "702.15. Lifelink\n702.15b Damage dealt by a source with lifelink causes that source's controller to gain that much life.")?],
            rulings: vec![Ruling { card: CardId::new(Uuid::from_u128(7)), idx: 2, published_at: "2020-01-01".into(), text: "Bob is sad.".into() }],
            history: vec![Qa { question: "earlier?".into(), answer: "yes".into() }],
            ..Context::default()
        };
        let bad = Rejection::BadCitation(Citation::Rule { id: rid("702.15")?, quote: "nope".into() });
        let s = render_user_turn(&q(), &ctx, Some(&bad), &[], &Budget::default());
        assert!(s.starts_with("# Material\n## Cards (current Oracle text)\n### Dark Confidant — card 00000000-0000-0000-0000-000000000007"), "{s}");
        assert!(s.contains("**Dark Confidant** {1}{B} — Creature — Human Wizard\nAt the beginning"), "{s}");
        assert!(s.contains("## Comprehensive Rules (effective 20250801)\n### [702.15] Heading\n702.15. Lifelink\n702.15b"), "{s}");
        assert!(s.contains("### Dark Confidant — card 00000000-0000-0000-0000-000000000007\n[ruling #2] (2020-01-01) Bob is sad."), "{s}");
        assert!(s.contains("## Earlier in this thread (context only; not citable)\nQ: earlier?\nA: yes"), "{s}");
        assert!(s.contains("## Previous attempt rejected\nYour earlier answer cited rule 702.15: \"nope\", which failed validation: the quote is not a verbatim substring"), "{s}");
        assert!(s.ends_with("\n# Question\ndoes trample work with deathtouch?\n"), "{s}");

        let plain = render_user_turn(&q(), &ctx, None, &[], &Budget::default());
        assert!(!plain.contains("rejected"), "{plain}");

        let missing = Rejection::BadCitation(Citation::Rule { id: rid("999.1")?, quote: "x".into() });
        let s = render_user_turn(&q(), &ctx, Some(&missing), &[], &Budget::default());
        assert!(s.contains("rule 999.1 is not among the excerpts"), "{s}");

        // Empty verdicts get their own notice, explaining what "empty" meant.
        let s = render_user_turn(&q(), &ctx, Some(&Rejection::Empty(EmptyVerdict::NoCitations)), &[], &Budget::default());
        assert!(s.contains("## Previous attempt rejected\nYour earlier answer had no citations"), "{s}");
        assert!(s.contains("answer fully"), "{s}");
        let s = render_user_turn(&q(), &ctx, Some(&Rejection::Empty(EmptyVerdict::ShortAnswer { chars: 7 })), &[], &Budget::default());
        assert!(s.contains("## Previous attempt rejected\nYour earlier answer was empty or too short"), "{s}");
        assert!(s.contains("Answer the question fully"), "{s}");
        Ok(())
    }

    #[test]
    fn budget_caps_chunk_count_and_bytes_in_retrieval_order() -> R {
        let rules: Vec<RuleChunk> = (1..=30).map(|i| chunk(&format!("702.{i}"), None, &"x".repeat(100))).collect::<Result<_, _>>()?;
        let ctx = Context { rules, ..Context::default() };
        let shown = select_rules(&ctx, &[], &Budget::default());
        let ids: Vec<&str> = shown.iter().map(|r| r.id.as_ref()).collect();
        assert_eq!(ids.len(), 25);
        assert_eq!(ids.first().copied(), Some("702.1"));
        assert_eq!(ids.last().copied(), Some("702.25"));
        let s = render_user_turn(&q(), &ctx, None, &[], &Budget::default());
        assert!(s.contains("### [702.25] ") && !s.contains("### [702.26] "), "{s}");

        // Byte cap: 5 000-byte chunks, 30 000-byte budget ⇒ 6 chunks.
        let big: Vec<RuleChunk> = (1..=10).map(|i| chunk(&format!("613.{i}"), None, &"y".repeat(5_000))).collect::<Result<_, _>>()?;
        let ctx = Context { rules: big, ..Context::default() };
        assert_eq!(select_rules(&ctx, &[], &Budget::default()).len(), 6);

        // An over-sized first chunk is still shown; nothing else fits after it.
        let huge = vec![chunk("613.1", None, &"z".repeat(40_000))?, chunk("613.2", None, "small")?];
        let ctx = Context { rules: huge, ..Context::default() };
        let shown = select_rules(&ctx, &[], &Budget::default());
        assert_eq!(shown.len(), 1);
        assert_eq!(shown.first().map(|r| r.id.as_ref()), Some("613.1"));
        Ok(())
    }

    #[test]
    fn sub_rules_fold_into_a_present_parent() -> R {
        let ctx = Context {
            rules: vec![
                chunk("702.19", None, "702.19. Trample\n702.19b The controller of an attacking creature with trample")?,
                chunk("702.19b", Some("702.19"), "702.19b The controller of an attacking creature with trample")?,
                chunk("704.5q", Some("704.5"), "704.5q orphan leaf")?,
            ],
            ..Context::default()
        };
        let ids: Vec<&str> = select_rules(&ctx, &[], &Budget::default()).iter().map(|r| r.id.as_ref()).collect();
        assert_eq!(ids, ["702.19", "704.5q"]);
        Ok(())
    }

    #[test]
    fn pinned_tool_round_chunks_survive_the_budget() -> R {
        // 30 retrieved chunks, then the tool round appended 613.7 and a leaf with its parent.
        let mut rules: Vec<RuleChunk> = (1..=30).map(|i| chunk(&format!("702.{i}"), None, &"x".repeat(100))).collect::<Result<_, _>>()?;
        let mut ctx = Context { rules: std::mem::take(&mut rules), ..Context::default() };
        ctx.extend_rules([
            chunk("613.7", None, "613.7. Within a layer or sublayer")?,
            chunk("704.5", None, "704.5. The state-based actions\n704.5q orphan")?,
            chunk("704.5q", Some("704.5"), "704.5q orphan")?,
        ]);
        let pinned = [rid("613.7")?, rid("704.5q")?];
        let shown = select_rules(&ctx, &pinned, &Budget::default());
        let ids: Vec<&str> = shown.iter().map(|r| r.id.as_ref()).collect();
        // Two pinned chunks (the leaf's parent is pinned in its place) reserve two of the 25 slots.
        assert_eq!(ids.len(), 25, "{ids:?}");
        assert!(ids.contains(&"613.7") && ids.contains(&"704.5") && !ids.contains(&"704.5q"), "{ids:?}");
        assert!(ids.contains(&"702.23") && !ids.contains(&"702.24"), "{ids:?}");
        assert_eq!(ids.last().copied(), Some("704.5"));

        let s = render_user_turn(&q(), &ctx, None, &pinned, &Budget::default());
        assert!(s.contains("### [613.7] Heading\n613.7. Within a layer or sublayer"), "{s}");
        // Without pinning the tool-round chunk would be dropped.
        let unpinned = render_user_turn(&q(), &ctx, None, &[], &Budget::default());
        assert!(!unpinned.contains("[613.7]"), "{unpinned}");
        Ok(())
    }

    #[test]
    fn rulings_capped_per_card_and_history_truncated() {
        let bob = CardId::new(Uuid::from_u128(7));
        let rulings = (0..25)
            .map(|i| Ruling { card: bob, idx: i, published_at: "2020-01-01".into(), text: format!("ruling {i}") })
            .collect();
        let long = "é".repeat(700);
        let ctx = Context {
            cards: vec![card(7, "Dark Confidant", "")],
            rulings,
            history: (0..6).map(|i| Qa { question: format!("q{i}"), answer: if i == 5 { long.clone() } else { format!("a{i}") } }).collect(),
            ..Context::default()
        };
        let s = render_user_turn(&q(), &ctx, None, &[], &Budget::default());
        assert!(s.contains("#19] (2020-01-01) ruling 19\n(5 more rulings omitted)"), "{s}");
        assert!(!s.contains("ruling 20"), "{s}");
        // Only the last four Q&A, and the long answer cut on a char boundary.
        assert!(!s.contains("Q: q1\n") && s.contains("Q: q2\n"), "{s}");
        assert!(s.contains(&format!("A: {}…", "é".repeat(300))), "{s}");
    }

    #[test]
    fn parent_of_ids() -> R {
        assert_eq!(parent_of(&rid("702.19b")?).as_ref().map(AsRef::as_ref), Some("702.19"));
        assert_eq!(parent_of(&rid("704.5aa")?).as_ref().map(AsRef::as_ref), Some("704.5"));
        assert!(parent_of(&rid("702.19")?).is_none());
        assert!(parent_of(&rid("702")?).is_none());
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "abc…");
        Ok(())
    }

    #[test]
    fn user_turn_is_cached_material_then_question() -> R {
        let ctx = Context { rules: vec![chunk("613.7", None, TIMESTAMP_RULE)?], ..Context::default() };
        let blocks = user_turn(&q(), &ctx, None, &Budget::default());
        let v = serde_json::to_value(&blocks)?;
        assert_eq!(at(&v, "/0/cache_control/type"), "ephemeral");
        assert!(at(&v, "/0/text").as_str().is_some_and(|t| t.starts_with("# Material\n") && !t.contains("# Question")));
        assert!(at(&v, "/1/cache_control").is_null());
        assert_eq!(at(&v, "/1/text"), "\n# Question\ndoes trample work with deathtouch?\n");
        Ok(())
    }

    // ---- wiremock: the tool round end to end ----

    /// `lookup_rules` answers from a fixed table and records every call.
    struct StubRetriever {
        table: Vec<RuleChunk>,
        calls: Mutex<Vec<Vec<RuleId>>>,
    }

    #[async_trait]
    impl Retriever for StubRetriever {
        async fn retrieve(&self, _q: &Question, _c: &[Card], _e: &Extraction) -> Result<Context, JudgeError> {
            Err(anyhow::anyhow!("not used").into())
        }
        async fn lookup_rules(&self, ids: &[RuleId]) -> Result<Vec<RuleChunk>, JudgeError> {
            self.calls.lock().unwrap_or_else(PoisonError::into_inner).push(ids.to_vec());
            Ok(self.table.iter().filter(|c| ids.contains(&c.id)).cloned().collect())
        }
    }

    const TIMESTAMP_RULE: &str = "613.7. Within a layer or sublayer, determining which order effects are applied in is usually done using a timestamp system.";

    fn message(stop: &str, content: &Value) -> Value {
        json!({
            "id": "msg_1", "model": "claude-opus-5", "role": "assistant",
            "content": content, "stop_reason": stop,
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
    }

    fn verdict_json(id: &str, quote: &str) -> String {
        json!({
            "answer": "Timestamps decide: the later effect wins within the same layer.", "confidence": "high",
            "citations": [{"kind": "rule", "id": id, "quote": quote}],
            "category": "layers"
        })
        .to_string()
    }

    fn synth_against(server: &MockServer, table: Vec<RuleChunk>) -> Result<(AnthropicSynthesizer, Arc<StubRetriever>), Box<dyn std::error::Error>> {
        let retriever = Arc::new(StubRetriever { table, calls: Mutex::new(Vec::new()) });
        let client = Client::new("test-key")?.with_base_url(server.uri());
        let synth = AnthropicSynthesizer::new(client, SynthConfig::default(), retriever.clone());
        Ok((synth, retriever))
    }

    async fn bodies(server: &MockServer) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
        let reqs = server.received_requests().await.unwrap_or_default();
        Ok(reqs.iter().map(|r| serde_json::from_slice::<Value>(&r.body)).collect::<Result<_, _>>()?)
    }

    /// Both text blocks of user message `i` concatenated.
    fn user_text(req: &Value, i: usize) -> String {
        [0, 1].iter().map(|b| at(req, &format!("/messages/{i}/content/{b}/text")).as_str().unwrap_or_default()).collect()
    }

    #[tokio::test]
    async fn tool_round_feeds_context_second_request_and_retry() -> R {
        let server = MockServer::start().await;
        // Mounted first: a request carrying a tool_result gets the verdict.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_string_contains(r#""type":"tool_result""#))
            .respond_with(ResponseTemplate::new(200).set_body_json(message(
                "end_turn",
                &json!([{"type": "text", "text": verdict_json("613.7", "usually done using a timestamp system")}]),
            )))
            .expect(1)
            .mount(&server)
            .await;
        // The first fresh request asks for 613.7; any later fresh request (the retry) answers directly.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(message(
                "tool_use",
                &json!([{"type": "tool_use", "id": "tu_1", "name": "lookup_rules", "input": {"ids": ["613.7"]}}]),
            )))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(message(
                "end_turn",
                &json!([{"type": "text", "text": verdict_json("613.7", "usually done using a timestamp system")}]),
            )))
            .expect(1)
            .mount(&server)
            .await;

        let (synth, retriever) = synth_against(&server, vec![chunk("613.7", None, TIMESTAMP_RULE)?])?;
        // 30 retrieved chunks: more than the budget shows, so pinning is observable on the retry.
        let rules: Vec<RuleChunk> = (1..=30).map(|i| chunk(&format!("702.{i}"), None, &"x".repeat(100))).collect::<Result<_, _>>()?;
        let mut ctx = Context { rules, ..Context::default() };

        let v = synth.answer(&q(), &mut ctx, None).await?;
        assert_eq!(v.confidence(), Confidence::High);
        assert!(ctx.rule(&rid("613.7")?).is_some(), "extend_rules received the tool-round chunk");
        assert_eq!(retriever.calls.lock().unwrap_or_else(PoisonError::into_inner).clone(), vec![vec![rid("613.7")?]]);
        let validated = v.validate(&ctx, AnswerableSource::Cr)?;
        assert_eq!(validated.category(), Category::Layers);
        assert_eq!(validated.source(), Source::Cr);

        let reqs = bodies(&server).await?;
        assert_eq!(reqs.len(), 2);
        let first = reqs.first().ok_or("no first request")?;
        assert_eq!(at(first, "/system/0/text").as_str(), Some(SYSTEM_PROMPT));
        assert_eq!(at(first, "/system/0/cache_control/type").as_str(), Some("ephemeral"));
        assert_eq!(at(first, "/tools/0/name").as_str(), Some("lookup_rules"));
        assert_eq!(at(first, "/messages/0/content/0/cache_control/type").as_str(), Some("ephemeral"));
        assert_eq!(at(first, "/tool_choice/type").as_str(), Some("auto"));
        assert!(user_text(first, 0).contains("### [702.25] ") && !user_text(first, 0).contains("[613.7]"));
        let second = reqs.get(1).ok_or("no second request")?;
        assert_eq!(at(second, "/messages/1/role").as_str(), Some("assistant"));
        assert_eq!(at(second, "/messages/1/content/0/type").as_str(), Some("tool_use"));
        assert_eq!(at(second, "/messages/2/role").as_str(), Some("user"));
        assert_eq!(at(second, "/messages/2/content/0/type").as_str(), Some("tool_result"));
        assert_eq!(at(second, "/messages/2/content/0/tool_use_id").as_str(), Some("tu_1"));
        assert!(at(second, "/messages/2/content/0/content").as_str().is_some_and(|c| c.contains(TIMESTAMP_RULE)));
        // The cached material block is unchanged and tool_choice stays auto, so the cache is reusable.
        assert_eq!(at(second, "/tool_choice/type").as_str(), Some("auto"));
        assert_eq!(at(second, "/messages/0/content/0"), at(first, "/messages/0/content/0"));
        assert_eq!(ctx.tool_round, vec![rid("613.7")?]);

        // The retry (as judge() would make it) pins the tool-round chunk past the budget and forbids the tool.
        let bad = Rejection::BadCitation(Citation::Rule { id: rid("613.7")?, quote: "not there".into() });
        let v2 = synth.answer(&q(), &mut ctx, Some(&bad)).await?;
        assert!(v2.validate(&ctx, AnswerableSource::Cr).is_ok());
        let reqs = bodies(&server).await?;
        assert_eq!(reqs.len(), 3);
        let third = reqs.get(2).ok_or("no third request")?;
        assert_eq!(at(third, "/tool_choice/type").as_str(), Some("none"));
        let text = user_text(third, 0);
        assert!(text.contains("### [613.7] Heading\n613.7. Within a layer"), "{text}");
        assert!(text.contains("## Previous attempt rejected") && text.contains("rule 613.7: \"not there\""), "{text}");
        assert!(text.contains("### [702.24] ") && !text.contains("### [702.25] "), "one slot went to the pinned chunk: {text}");
        Ok(())
    }

    #[tokio::test]
    async fn cited_sub_rule_of_a_shown_parent_is_hydrated() -> R {
        let server = MockServer::start().await;
        let leaf_line = "702.19b The controller of an attacking creature with trample first assigns damage to the creature(s) blocking it.";
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(message(
                "end_turn",
                &json!([{"type": "text", "text": verdict_json("702.19b", "first assigns damage to the creature(s) blocking it")}]),
            )))
            .expect(1)
            .mount(&server)
            .await;
        let parent = chunk("702.19", None, &format!("702.19. Trample\n{leaf_line}"))?;
        let leaf = chunk("702.19b", Some("702.19"), leaf_line)?;
        let (synth, retriever) = synth_against(&server, vec![parent.clone(), leaf])?;
        let mut ctx = Context { rules: vec![parent], ..Context::default() };

        let v = synth.answer(&q(), &mut ctx, None).await?;
        assert!(ctx.rule(&rid("702.19b")?).is_some(), "leaf hydrated");
        assert_eq!(retriever.calls.lock().unwrap_or_else(PoisonError::into_inner).clone(), vec![vec![rid("702.19b")?]]);
        assert!(v.validate(&ctx, AnswerableSource::Cr).is_ok());
        // The hydrated leaf folds back into its parent when rendered again.
        let s = render_user_turn(&q(), &ctx, None, &[], &Budget::default());
        assert_eq!(s.matches("702.19b The controller").count(), 1, "{s}");
        Ok(())
    }

    #[tokio::test]
    async fn sub_rule_of_an_unseen_parent_is_not_hydrated() -> R {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(message(
                "end_turn",
                &json!([{"type": "text", "text": verdict_json("702.19b", "whatever")}]),
            )))
            .mount(&server)
            .await;
        let leaf = chunk("702.19b", Some("702.19"), "702.19b whatever")?;
        let (synth, retriever) = synth_against(&server, vec![leaf])?;
        let mut ctx = Context { rules: vec![chunk("613.7", None, TIMESTAMP_RULE)?], ..Context::default() };
        let v = synth.answer(&q(), &mut ctx, None).await?;
        assert!(retriever.calls.lock().unwrap_or_else(PoisonError::into_inner).is_empty());
        assert!(matches!(v.validate(&ctx, AnswerableSource::Cr), Err(JudgeError::BadCitation(_))));
        Ok(())
    }

    #[tokio::test]
    async fn truncated_response_is_retried_once_at_medium_effort() -> R {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_string_contains(r#""effort":"medium""#))
            .respond_with(ResponseTemplate::new(200).set_body_json(message(
                "end_turn",
                &json!([{"type": "text", "text": verdict_json("613.7", "timestamp system")}]),
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(message("max_tokens", &json!([{"type": "text", "text": "{"}]))))
            .expect(1)
            .mount(&server)
            .await;
        let (synth, _) = synth_against(&server, vec![])?;
        let mut ctx = Context { rules: vec![chunk("613.7", None, TIMESTAMP_RULE)?], ..Context::default() };
        let v = synth.answer(&q(), &mut ctx, None).await?;
        assert!(v.validate(&ctx, AnswerableSource::Cr).is_ok());
        let reqs = bodies(&server).await?;
        assert_eq!(at(reqs.first().ok_or("first")?, "/output_config/effort"), "high");
        assert_eq!(at(reqs.get(1).ok_or("second")?, "/output_config/effort"), "medium");

        // A second truncation is surfaced.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(message("max_tokens", &json!([{"type": "text", "text": "{"}]))))
            .expect(2)
            .mount(&server)
            .await;
        let (synth, _) = synth_against(&server, vec![])?;
        let r = synth.answer(&q(), &mut ctx, None).await;
        assert!(matches!(r, Err(JudgeError::Upstream(e)) if e.downcast_ref::<Truncated>().is_some()));
        Ok(())
    }
}
