//! The pipeline: extract → resolve → retrieve → synthesize → validate.

use std::sync::Arc;

use futures::future::try_join_all;
use nonempty::NonEmpty;

use crate::{
    Ambiguous, Card, Extractor, JudgeError, Qa, Question, Resolution, Resolver, Retriever, Synthesizer,
    Validated, Verdict,
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
    if !e.source.is_answerable() {
        return Err(JudgeError::OutOfScope(e.source));
    }
    let resolutions = try_join_all(e.card_spans.iter().map(|s| deps.resolver.resolve(s))).await?;
    let cards = collect_resolved(resolutions)?;
    let mut ctx = deps.retriever.retrieve(q, &cards, &e).await?;
    ctx.history = history.to_vec();

    // ARCHITECTURE §3 step 5: BadCitation ⇒ retry once (telling the model which
    // citation failed), then error. Retries live here, not in the Discord layer.
    let rejected = match deps.synthesizer.answer(q, &mut ctx, None).await?.validate(&ctx) {
        Err(JudgeError::BadCitation(c)) => c,
        done => return done,
    };
    deps.synthesizer.answer(q, &mut ctx, Some(&rejected)).await?.validate(&ctx)
}

/// Turn per-span resolutions into cards, or the first blocking error.
/// Ambiguity wins over not-found because it is actionable via buttons.
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
            Resolution::Ambiguous { query, candidates } => ambiguous.push(Ambiguous { query, candidates }),
            Resolution::NotFound { query } => missing.push(query),
        }
    }
    if let Some(a) = NonEmpty::from_vec(ambiguous) {
        return Err(JudgeError::AmbiguousCards(a));
    }
    if let Some(m) = NonEmpty::from_vec(missing) {
        return Err(JudgeError::CardsNotFound(m));
    }
    Ok(cards)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CardId, Category, Citation, Confidence, Context, CrVersion, Extraction, Face, Layout, MatchedVia,
        RuleChunk, RuleId, Source, Unvalidated,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;
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
            Resolution::Ambiguous { query: "Jace".into(), candidates: NonEmpty::new(card(2, "Jace Beleren")) },
        ]);
        assert!(matches!(r, Err(JudgeError::AmbiguousCards(a)) if a.len() == 1));
    }

    #[test]
    fn not_found_reported() {
        let r = collect_resolved(vec![Resolution::NotFound { query: "zzz".into() }]);
        assert!(matches!(r, Err(JudgeError::CardsNotFound(m)) if m.head == "zzz"));
    }

    // ---- judge() with stubs ----

    struct StubExtractor;
    #[async_trait]
    impl Extractor for StubExtractor {
        async fn extract(&self, _q: &Question, _h: &[Qa]) -> Result<Extraction, JudgeError> {
            Ok(Extraction { card_spans: vec![], concepts: vec![], categories: vec![], source: Source::Cr })
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

    /// Returns one scripted verdict per call and records what it was told.
    struct ScriptedSynth {
        quotes: Mutex<Vec<&'static str>>,
        seen: Mutex<Vec<(usize, Option<Citation>)>>,
    }
    #[async_trait]
    impl Synthesizer for ScriptedSynth {
        async fn answer(
            &self,
            _q: &Question,
            ctx: &mut Context,
            rejected: Option<&Citation>,
        ) -> Result<Verdict<Unvalidated>, JudgeError> {
            #[allow(clippy::unwrap_used)]
            let quote = self.quotes.lock().unwrap().remove(0);
            #[allow(clippy::unwrap_used)]
            self.seen.lock().unwrap().push((ctx.history.len(), rejected.cloned()));
            let id = RuleId::try_new("702.15b".to_owned()).map_err(anyhow::Error::from)?;
            Ok(Verdict::new(
                "a".into(),
                Confidence::High,
                vec![Citation::Rule { id, quote: quote.into() }],
                Category::KeywordAbilities,
                Source::Cr,
            ))
        }
    }

    fn deps(quotes: Vec<&'static str>) -> (Deps, Arc<ScriptedSynth>) {
        let synth = Arc::new(ScriptedSynth { quotes: Mutex::new(quotes), seen: Mutex::new(vec![]) });
        let d = Deps {
            extractor: Arc::new(StubExtractor),
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
        #[allow(clippy::unwrap_used)]
        let seen = synth.seen.lock().unwrap().clone();
        assert_eq!(seen, vec![(1, None)]);
        Ok(())
    }

    #[test]
    fn bad_citation_retries_once_with_the_rejected_citation() -> Result<(), JudgeError> {
        let (d, synth) = deps(vec!["not in the rule", "gain that much life"]);
        let v = futures::executor::block_on(judge(&d, &q(), &[]))?;
        assert_eq!(v.citations().len(), 1);
        #[allow(clippy::unwrap_used)]
        let seen = synth.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert!(matches!(&seen.get(1), Some((0, Some(Citation::Rule { quote, .. }))) if quote == "not in the rule"));
        Ok(())
    }

    #[test]
    fn bad_citation_twice_is_an_error() {
        let (d, synth) = deps(vec!["bad one", "bad two"]);
        let r = futures::executor::block_on(judge(&d, &q(), &[]));
        assert!(matches!(r, Err(JudgeError::BadCitation(Citation::Rule { quote, .. })) if quote == "bad two"));
        #[allow(clippy::unwrap_used)]
        let n = synth.seen.lock().unwrap().len();
        assert_eq!(n, 2);
    }
}
