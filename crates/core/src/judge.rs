//! The pipeline: extract → resolve → retrieve → synthesize → validate.

use std::sync::Arc;

use futures::future::try_join_all;
use nonempty::NonEmpty;

use crate::{
    Ambiguous, Card, Extractor, JudgeError, MatchedVia, Qa, Question, RejectedAttempt, Rejection, Resolution, Resolver,
    Retriever, Synthesizer, Validated, Verdict,
};

/// The ports `judge()` needs. `CallStore` is deliberately absent: persisting
/// is the caller's decision after rendering.
pub struct Deps {
    /// Steps 1 + 3.
    pub extractor: Arc<dyn Extractor>,
    /// Step 2.
    pub resolver: Arc<dyn Resolver>,
    /// Step 4 (and the `lookup_rules` tool).
    pub retriever: Arc<dyn Retriever>,
    /// Step 5.
    pub synthesizer: Arc<dyn Synthesizer>,
}

/// Answer `q`. Returns a verdict whose citations were checked against Context.
///
/// # Errors
/// See [`JudgeError`]. Ambiguity and not-found are reported for *all* spans at once.
pub async fn judge(deps: &Deps, q: &Question, history: &[Qa]) -> Result<Verdict<Validated>, JudgeError> {
    let e = deps.extractor.extract(q, history).await?;
    // The verdict's source is stamped from here, not reported by the synthesis
    // model: `validate` takes an `AnswerableSource`, so this is the only way in.
    let source = e.source.answerable().ok_or(JudgeError::OutOfScope(e.source))?;
    let resolutions = try_join_all(e.card_spans.iter().map(|s| deps.resolver.resolve(s))).await?;
    let cards = collect_resolved(resolutions)?;
    let mut ctx = deps.retriever.retrieve(q, &cards, &e).await?;
    ctx.history = history.to_vec();

    // ARCHITECTURE §3 step 5: BadCitation (or an empty verdict) ⇒ retry once,
    // telling the model what was wrong, then error. Retries live here, not in
    // the Discord layer.
    let first = deps.synthesizer.answer(q, &mut ctx, None).await?;
    // Kept before `validate` consumes the verdict: the retry is a fresh
    // conversation, and it is shown what it is asked to correct.
    let answer = first.answer().to_owned();
    let rejection = match first.validate(&ctx, source) {
        Err(JudgeError::BadCitation(c)) => Rejection::BadCitation(c),
        Err(JudgeError::MalformedCitation(m)) => Rejection::Malformed(m),
        Err(JudgeError::EmptyVerdict(e)) => Rejection::Empty(e),
        done => return done,
    };
    let rejected = RejectedAttempt::new(rejection, &answer);
    // At INFO, not DEBUG: when the retry also fails, the *first* rejection is
    // usually what explains the second, and production runs at INFO.
    tracing::info!(%rejected, "verdict rejected; retrying synthesis once");
    deps.synthesizer.answer(q, &mut ctx, Some(&rejected)).await?.validate(&ctx, source)
}

/// Turn per-span resolutions into cards, or the first blocking error.
///
/// The extractor is told to emit both a nickname and the full name it stands
/// for ("the tron lands" + the three Urza's lands, "Ragavan" + "Ragavan,
/// Nimble Pilferer"), so a span that failed to resolve on its own is often a
/// second reference to a card that *did* resolve from another span. Two
/// duplicate rules drop those before anything is reported:
///
/// * an `Ambiguous` span from a non-fuzzy rung (alias, short name, printed
///   name…) whose candidates include a card resolved from another span is a
///   duplicate reference. Fuzzy candidates are trigram neighbours, not names
///   the user could have meant ("Urza" beside "Urza's Saga" is a different
///   card), so a fuzzy-ambiguous span is never dropped this way;
/// * a `NotFound` span whose words occur, as a contiguous run of whole words,
///   in the name or a face name of a resolved card is a duplicate reference
///   ("moon" in "Blood Moon", but not in "Moonmist").
///
/// Only spans still ambiguous after that become `AmbiguousCards`. Ambiguity
/// wins over not-found because it is actionable via buttons.
///
/// # Errors
/// `AmbiguousCards` or `CardsNotFound`.
pub fn collect_resolved(resolutions: Vec<Resolution>) -> Result<Vec<Card>, JudgeError> {
    let mut cards = Vec::new();
    let mut ambiguous = Vec::new();
    let mut missing = Vec::new();
    for r in resolutions {
        match r {
            Resolution::Resolved { card, .. } => {
                if !cards.iter().any(|c: &Card| c.id == card.id) {
                    cards.push(card);
                }
            }
            Resolution::Ambiguous { query, candidates, via } => ambiguous.push((Ambiguous { query, candidates }, via)),
            Resolution::NotFound { query } => missing.push(query),
        }
    }
    ambiguous.retain(|(a, via)| {
        let dup = *via != MatchedVia::Fuzzy && a.candidates.iter().any(|cand| cards.iter().any(|c| c.id == cand.id));
        if dup {
            tracing::debug!(span = %a.query, "ambiguous span duplicates a resolved card; dropped");
        }
        !dup
    });
    missing.retain(|m| {
        let dup = cards.iter().any(|c| card_mentions(c, m));
        if dup {
            tracing::debug!(span = %m, "unresolved span is part of a resolved card's name; dropped");
        }
        !dup
    });
    if let Some(a) = NonEmpty::from_vec(ambiguous.into_iter().map(|(a, _)| a).collect()) {
        return Err(JudgeError::AmbiguousCards(a));
    }
    if let Some(m) = NonEmpty::from_vec(missing) {
        return Err(JudgeError::CardsNotFound(m));
    }
    Ok(cards)
}

/// True if the words of `span` occur, case-insensitively and as a contiguous
/// run of whole words, in the card's name or in any face name. Words are runs
/// of alphanumerics, so `urza` matches `Urza's Tower` but `moon` does not
/// match `Moonmist`. An empty span never matches.
fn card_mentions(card: &Card, span: &str) -> bool {
    let needle = words(span);
    if needle.is_empty() {
        return false;
    }
    std::iter::once(card.name.as_str())
        .chain(card.faces.iter().map(|f| f.name.as_str()))
        .any(|n| words(n).windows(needle.len()).any(|w| w == needle.as_slice()))
}

/// Lower-cased alphanumeric words of `text`.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CardId, Category, CategoryGuess, Citation, Confidence, Context, CrVersion, EmptyVerdict, Extraction,
        Face, Layout, MatchedVia, RuleChunk, RuleId, Source, Unvalidated,
    };
    use async_trait::async_trait;
    use std::{
        collections::VecDeque,
        sync::{Mutex, PoisonError},
    };
    use uuid::Uuid;

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

    #[test]
    fn dedupes_resolved_cards() -> Result<(), JudgeError> {
        let cards = collect_resolved(vec![
            Resolution::Resolved { card: card(1, "Bob"), via: MatchedVia::Alias },
            Resolution::Resolved { card: card(1, "Bob"), via: MatchedVia::Exact },
        ])?;
        assert_eq!(cards.len(), 1);
        Ok(())
    }

    #[test]
    fn ambiguous_beats_not_found() {
        let r = collect_resolved(vec![
            Resolution::NotFound { query: "zzz".into() },
            ambiguous("Jace", NonEmpty::new(card(2, "Jace Beleren"))),
        ]);
        assert!(matches!(r, Err(JudgeError::AmbiguousCards(a)) if a.len() == 1));
    }

    /// An ambiguous span from a non-fuzzy rung (the nickname + full-name shape).
    fn ambiguous(query: &str, candidates: NonEmpty<Card>) -> Resolution {
        Resolution::Ambiguous { query: query.into(), candidates, via: MatchedVia::ShortName }
    }

    #[test]
    fn not_found_reported() {
        let r = collect_resolved(vec![Resolution::NotFound { query: "zzz".into() }]);
        assert!(matches!(r, Err(JudgeError::CardsNotFound(m)) if m.head == "zzz"));
    }

    fn resolved(n: u128, name: &str) -> Resolution {
        Resolution::Resolved { card: card(n, name), via: MatchedVia::Exact }
    }

    #[test]
    fn ambiguous_span_overlapping_a_resolved_card_is_a_duplicate() -> Result<(), JudgeError> {
        // "Ragavan" beside "Ragavan, Nimble Pilferer": the nickname's candidates include the resolved card.
        let cards = collect_resolved(vec![
            resolved(1, "Ragavan, Nimble Pilferer"),
            ambiguous("Ragavan", NonEmpty::from((card(1, "Ragavan, Nimble Pilferer"), vec![card(2, "Ragavan (token)")]))),
        ])?;
        assert_eq!(cards.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Ragavan, Nimble Pilferer"]);
        // Order does not matter: the resolved span may come after the ambiguous one.
        let cards = collect_resolved(vec![
            Resolution::Ambiguous {
                query: "tron lands".into(),
                candidates: NonEmpty::from((card(3, "Urza's Tower"), vec![card(4, "Urza's Mine")])),
                via: MatchedVia::Alias,
            },
            resolved(3, "Urza's Tower"),
            resolved(4, "Urza's Mine"),
            resolved(5, "Urza's Power Plant"),
        ])?;
        assert_eq!(cards.len(), 3);
        // Ambiguous between two cards that were BOTH resolved elsewhere: dropped, both kept.
        let cards = collect_resolved(vec![
            resolved(6, "Bruna, Light of Alabaster"),
            resolved(7, "Bruna, the Fading Light"),
            ambiguous("Bruna", NonEmpty::from((card(6, "Bruna, Light of Alabaster"), vec![card(7, "Bruna, the Fading Light")]))),
        ])?;
        assert_eq!(cards.len(), 2);
        Ok(())
    }

    #[test]
    fn fuzzy_ambiguous_span_is_never_a_duplicate() {
        // "Urza" beside "Urza's Saga": the fuzzy neighbours happen to include the
        // resolved Saga, but the user asked about a different card. Ask.
        let r = collect_resolved(vec![
            resolved(1, "Urza's Saga"),
            Resolution::Ambiguous {
                query: "Urza".into(),
                candidates: NonEmpty::from((
                    card(2, "Urza, Lord High Artificer"),
                    vec![card(3, "Urza, Academy Headmaster"), card(1, "Urza's Saga")],
                )),
                via: MatchedVia::Fuzzy,
            },
        ]);
        assert!(matches!(r, Err(JudgeError::AmbiguousCards(a)) if a.len() == 1 && a.head.query == "Urza"));
    }

    #[test]
    fn not_found_span_inside_a_resolved_name_is_a_duplicate() -> Result<(), JudgeError> {
        let cards = collect_resolved(vec![
            resolved(1, "Blood Moon"),
            Resolution::NotFound { query: "MOON".into() },
            Resolution::NotFound { query: "  moon ".into() },
            Resolution::NotFound { query: "blood moon".into() },
        ])?;
        assert_eq!(cards.len(), 1);
        // Apostrophes split words: "urza" and "urza's" both name Urza's Tower.
        let cards = collect_resolved(vec![
            resolved(3, "Urza's Tower"),
            Resolution::NotFound { query: "urza".into() },
            Resolution::NotFound { query: "Urza's".into() },
        ])?;
        assert_eq!(cards.len(), 1);
        // A face name counts too.
        let mut dfc = card(2, "Delver of Secrets // Insectile Aberration");
        dfc.faces.push(Face {
            name: "Insectile Aberration".into(),
            oracle_text: String::new(),
            mana_cost: String::new(),
            type_line: "Creature".into(),
        });
        let cards = collect_resolved(vec![
            Resolution::Resolved { card: dfc, via: MatchedVia::Exact },
            Resolution::NotFound { query: "insectile".into() },
        ])?;
        assert_eq!(cards.len(), 1);
        Ok(())
    }

    #[test]
    fn genuinely_ambiguous_or_missing_spans_still_error() {
        // Candidates disjoint from every resolved card.
        let r = collect_resolved(vec![
            resolved(1, "Blood Moon"),
            ambiguous("Jace", NonEmpty::new(card(2, "Jace Beleren"))),
        ]);
        assert!(matches!(r, Err(JudgeError::AmbiguousCards(a)) if a.len() == 1 && a.head.query == "Jace"));
        // A not-found span that is not part of any resolved name.
        let r = collect_resolved(vec![resolved(1, "Blood Moon"), Resolution::NotFound { query: "sun".into() }]);
        assert!(matches!(r, Err(JudgeError::CardsNotFound(m)) if m.head == "sun"));
        // A substring of a word is not a mention: the user's card must not vanish.
        for (name, span) in [("Moonmist", "moon"), ("Boltwing Marauder", "bolt"), ("Price of Progress", "ice"), ("Blood Moon", "lood")] {
            let r = collect_resolved(vec![resolved(1, name), Resolution::NotFound { query: span.into() }]);
            assert!(matches!(r, Err(JudgeError::CardsNotFound(ref m)) if m.head == span), "{name} / {span}: {r:?}");
        }
        // Words must be contiguous and in order.
        let r = collect_resolved(vec![resolved(1, "Blood Moon"), Resolution::NotFound { query: "moon blood".into() }]);
        assert!(matches!(r, Err(JudgeError::CardsNotFound(_))));
        // Only the duplicate is dropped; the other ambiguous span is reported.
        let r = collect_resolved(vec![
            resolved(1, "Blood Moon"),
            ambiguous("the moon", NonEmpty::new(card(1, "Blood Moon"))),
            ambiguous("Jace", NonEmpty::new(card(2, "Jace Beleren"))),
        ]);
        assert!(matches!(r, Err(JudgeError::AmbiguousCards(a)) if a.len() == 1 && a.head.query == "Jace"));
        // An empty span never matches by containment.
        let r = collect_resolved(vec![resolved(1, "Blood Moon"), Resolution::NotFound { query: "  ".into() }]);
        assert!(matches!(r, Err(JudgeError::CardsNotFound(_))));
    }

    // ---- judge() with stubs ----

    struct StubExtractor(Source);
    #[async_trait]
    impl Extractor for StubExtractor {
        async fn extract(&self, _q: &Question, _h: &[Qa]) -> Result<Extraction, JudgeError> {
            Ok(Extraction {
                card_spans: vec![],
                concepts: vec![],
                primary: CategoryGuess { category: Category::KeywordAbilities, confidence: Confidence::High },
                secondary: vec![],
                source: self.0,
            })
        }
    }

    struct StubResolver;
    #[async_trait]
    impl Resolver for StubResolver {
        async fn resolve(&self, span: &str) -> Result<Resolution, JudgeError> {
            Ok(Resolution::NotFound { query: span.to_owned() })
        }
    }

    struct StubRetriever;
    #[async_trait]
    impl Retriever for StubRetriever {
        async fn retrieve(&self, _q: &Question, _c: &[Card], _e: &Extraction) -> Result<Context, JudgeError> {
            Ok(Context {
                rules: vec![RuleChunk {
                    id: RuleId::try_new("702.15b".to_owned()).map_err(anyhow::Error::from)?,
                    parent_id: None,
                    subsection: RuleId::try_new("702".to_owned()).map_err(anyhow::Error::from)?,
                    heading: "Lifelink".into(),
                    body: "gain that much life".into(),
                    examples: vec![],
                    cr_version: CrVersion::try_new("20250801".to_owned()).map_err(anyhow::Error::from)?,
                }],
                ..Context::default()
            })
        }
        async fn lookup_rules(&self, _ids: &[RuleId]) -> Result<Vec<RuleChunk>, JudgeError> {
            Ok(vec![])
        }
    }

    const ANSWER: &str = "Lifelink does not stack: two instances gain life once.";

    /// Scripts a verdict carrying one citation the model wrote but that could
    /// not be parsed — the shape of the 2026-09-01 production failure. It has
    /// to come in as JSON: `Verdict::new` takes `Vec<Citation>`, so a malformed
    /// citation is not constructible by hand, which is the point.
    const MALFORMED: &str = "!malformed";

    /// Returns one scripted verdict per call and records what it was told.
    /// A quote of `""` scripts a verdict with no citations at all;
    /// [`MALFORMED`] scripts one whose only citation is unreadable.
    struct ScriptedSynth {
        quotes: Mutex<VecDeque<&'static str>>,
        seen: Mutex<Vec<(usize, Option<RejectedAttempt>)>>,
    }
    impl ScriptedSynth {
        /// Per call: the history length and the rejection it was shown.
        fn seen(&self) -> Vec<(usize, Option<Rejection>)> {
            let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            seen.iter().map(|(h, r)| (*h, r.as_ref().map(|r| r.rejection().clone()))).collect()
        }
        /// The rejected answers the retries were shown.
        fn retry_answers(&self) -> Vec<String> {
            let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            seen.iter().filter_map(|(_, r)| r.as_ref().and_then(RejectedAttempt::answer).map(ToOwned::to_owned)).collect()
        }
    }
    #[async_trait]
    impl Synthesizer for ScriptedSynth {
        async fn answer(
            &self,
            _q: &Question,
            ctx: &mut Context,
            rejected: Option<&RejectedAttempt>,
        ) -> Result<Verdict<Unvalidated>, JudgeError> {
            let quote = self
                .quotes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("scripted synthesizer called more times than scripted"))?;
            self.seen.lock().unwrap_or_else(PoisonError::into_inner).push((ctx.history.len(), rejected.cloned()));
            if quote == MALFORMED {
                let json = format!(
                    r#"{{"answer":"{ANSWER}","confidence":"high","category":"keyword_abilities",
                       "citations":[{{"kind":"rule","id":"","quote":""}}]}}"#
                );
                return serde_json::from_str(&json).map_err(|e| anyhow::Error::from(e).into());
            }
            let id = RuleId::try_new("702.15b".to_owned()).map_err(anyhow::Error::from)?;
            let citations = if quote.is_empty() { vec![] } else { vec![Citation::Rule { id, quote: crate::Quote::try_new(quote).map_err(anyhow::Error::from)? }] };
            Ok(Verdict::new(ANSWER.into(), Confidence::High, citations, Category::KeywordAbilities))
        }
    }

    fn deps(quotes: Vec<&'static str>) -> (Deps, Arc<ScriptedSynth>) {
        deps_from(Source::Cr, quotes)
    }

    fn deps_from(source: Source, quotes: Vec<&'static str>) -> (Deps, Arc<ScriptedSynth>) {
        let synth = Arc::new(ScriptedSynth { quotes: Mutex::new(quotes.into()), seen: Mutex::new(vec![]) });
        let d = Deps {
            extractor: Arc::new(StubExtractor(source)),
            resolver: Arc::new(StubResolver),
            retriever: Arc::new(StubRetriever),
            synthesizer: synth.clone(),
        };
        (d, synth)
    }

    fn q() -> Question {
        Question { thread_id: "t".into(), text: "does lifelink stack?".into() }
    }

    #[test]
    fn history_reaches_context_and_first_good_verdict_wins() -> Result<(), JudgeError> {
        let (d, synth) = deps(vec!["gain that much life"]);
        let history = vec![Qa { question: "q0".into(), answer: "a0".into() }];
        let v = futures::executor::block_on(judge(&d, &q(), &history))?;
        assert_eq!(v.cr_version().as_ref(), "20250801");
        assert_eq!(v.source(), Source::Cr);
        assert_eq!(synth.seen(), vec![(1, None)]);
        Ok(())
    }

    #[test]
    fn verdict_source_comes_from_the_extraction() -> Result<(), JudgeError> {
        let (d, _) = deps_from(Source::Commander, vec!["gain that much life"]);
        let v = futures::executor::block_on(judge(&d, &q(), &[]))?;
        assert_eq!(v.source(), Source::Commander);
        Ok(())
    }

    #[test]
    fn unanswerable_sources_stop_before_synthesis() {
        for source in [Source::OutOfScope, Source::Tournament] {
            let (d, synth) = deps_from(source, vec!["gain that much life"]);
            let r = futures::executor::block_on(judge(&d, &q(), &[]));
            assert!(matches!(r, Err(JudgeError::OutOfScope(s)) if s == source));
            assert!(synth.seen().is_empty(), "synthesis must not run for {source:?}");
        }
    }

    #[test]
    fn bad_citation_retries_once_with_the_rejected_citation() -> Result<(), JudgeError> {
        let (d, synth) = deps(vec!["not in the rule", "gain that much life"]);
        let v = futures::executor::block_on(judge(&d, &q(), &[]))?;
        assert_eq!(v.citations().len(), 1);
        let seen = synth.seen();
        assert_eq!(seen.len(), 2);
        assert!(matches!(&seen.get(1), Some((0, Some(Rejection::BadCitation(Citation::Rule { quote, .. })))) if quote.as_ref() == "not in the rule"));
        // The retry is a fresh conversation: it is handed the answer it is asked to correct.
        assert_eq!(synth.retry_answers(), vec![ANSWER.to_owned()]);
        Ok(())
    }

    #[test]
    fn empty_verdict_retries_once_like_a_bad_citation() -> Result<(), JudgeError> {
        let (d, synth) = deps(vec!["", "gain that much life"]);
        let v = futures::executor::block_on(judge(&d, &q(), &[]))?;
        assert_eq!(v.citations().len(), 1);
        let seen = synth.seen();
        assert_eq!(seen.len(), 2);
        assert!(matches!(&seen.get(1), Some((0, Some(Rejection::Empty(EmptyVerdict::NoCitations))))));

        let (d, _) = deps(vec!["", ""]);
        let r = futures::executor::block_on(judge(&d, &q(), &[]));
        assert!(matches!(r, Err(JudgeError::EmptyVerdict(EmptyVerdict::NoCitations))));
        Ok(())
    }

    /// The regression: an unreadable citation must behave like a bad one — one
    /// retry that tells the model what it wrote — rather than escaping as the
    /// un-retryable `Upstream` a whole-payload parse failure used to produce.
    #[test]
    fn malformed_citation_retries_once_like_a_bad_citation() -> Result<(), JudgeError> {
        let (d, synth) = deps(vec![MALFORMED, "gain that much life"]);
        let v = futures::executor::block_on(judge(&d, &q(), &[]))?;
        assert_eq!(v.citations().len(), 1);
        let seen = synth.seen();
        assert_eq!(seen.len(), 2, "exactly one retry");
        // The retry is told the raw element and why it could not be read.
        assert!(
            matches!(&seen.get(1), Some((0, Some(Rejection::Malformed(m))))
                if m.raw.contains(r#""kind":"rule""#) && m.error.contains("RuleId")),
            "{seen:?}"
        );
        Ok(())
    }

    /// A second unreadable response terminates rather than looping.
    #[test]
    fn malformed_citation_twice_is_an_error() {
        let (d, synth) = deps(vec![MALFORMED, MALFORMED]);
        let r = futures::executor::block_on(judge(&d, &q(), &[]));
        assert!(matches!(r, Err(JudgeError::MalformedCitation(_))), "{r:?}");
        assert_eq!(synth.seen().len(), 2, "capped at one retry, no loop");
    }

    #[test]
    fn bad_citation_twice_is_an_error() {
        let (d, synth) = deps(vec!["bad one", "bad two"]);
        let r = futures::executor::block_on(judge(&d, &q(), &[]));
        assert!(matches!(r, Err(JudgeError::BadCitation(Citation::Rule { quote, .. })) if quote.as_ref() == "bad two"));
        assert_eq!(synth.seen().len(), 2);
    }
}
