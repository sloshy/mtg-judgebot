//! [`CapturingRetriever`]: keeps the [`Context`] a question was answered from,
//! so the Discord layer can hand it to `CallStore::persist` (which records
//! the context ids that prior-call retrieval later filters on).
//!
//! `judge()` returns only the verdict and drops its `Context`; until it also
//! returns the context (the proper fix, in `judge-core`), this decorator
//! around the real retriever is the only place that sees it. The copy taken
//! here predates the synthesizer's `lookup_rules` round, so the persisted
//! `context_ids.rules` can miss chunks the model asked for afterwards.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use judge_core::{Card, Context, Extraction, JudgeError, Question, Retriever, RuleChunk, RuleId};

/// Entries held before the map is cleared outright (a leak guard; entries are
/// normally taken right after `judge()` returns).
const MAX_HELD: usize = 64;

/// Key: the question as `judge()` sees it. Identical questions in flight in
/// the same thread share a key, so each key holds a FIFO of contexts: every
/// run pushes one and every `take` pops one, and no run is left unpersisted.
/// (Two runs of the same text may swap contexts, which are the same retrieval
/// anyway.)
type Key = (String, String);

/// Wraps a [`Retriever`] and remembers the `Context`s built per question.
pub struct CapturingRetriever {
    inner: Arc<dyn Retriever>,
    held: Mutex<HashMap<Key, VecDeque<Context>>>,
}

impl fmt::Debug for CapturingRetriever {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturingRetriever")
            .field("held", &self.len())
            .finish_non_exhaustive()
    }
}

impl CapturingRetriever {
    /// Decorate `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn Retriever>) -> Self {
        Self {
            inner,
            held: Mutex::new(HashMap::new()),
        }
    }

    /// Remove and return the oldest context captured for `q`, if any.
    #[must_use]
    pub fn take(&self, q: &Question) -> Option<Context> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let k = key(q);
        let ctx = held.get_mut(&k).and_then(VecDeque::pop_front);
        if held.get(&k).is_some_and(VecDeque::is_empty) {
            held.remove(&k);
        }
        ctx
    }

    /// Contexts currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(VecDeque::len)
            .sum()
    }

    /// True if nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn key(q: &Question) -> Key {
    (q.thread_id.clone(), q.text.clone())
}

#[async_trait]
impl Retriever for CapturingRetriever {
    async fn retrieve(
        &self,
        q: &Question,
        cards: &[Card],
        e: &Extraction,
    ) -> Result<Context, JudgeError> {
        let ctx = self.inner.retrieve(q, cards, e).await?;
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let total: usize = held.values().map(VecDeque::len).sum();
        if total >= MAX_HELD {
            tracing::warn!(held = total, "captured contexts were never taken; clearing");
            held.clear();
        }
        held.entry(key(q)).or_default().push_back(ctx.clone());
        Ok(ctx)
    }

    async fn lookup_rules(&self, ids: &[RuleId]) -> Result<Vec<RuleChunk>, JudgeError> {
        self.inner.lookup_rules(ids).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_core::{Category, CategoryGuess, Confidence, CrVersion, Source};

    struct Stub;

    #[async_trait]
    impl Retriever for Stub {
        async fn retrieve(
            &self,
            q: &Question,
            _c: &[Card],
            _e: &Extraction,
        ) -> Result<Context, JudgeError> {
            Ok(Context {
                rules: vec![RuleChunk {
                    id: RuleId::try_new("702.19".to_owned()).map_err(anyhow::Error::from)?,
                    parent_id: None,
                    subsection: RuleId::try_new("702".to_owned()).map_err(anyhow::Error::from)?,
                    heading: q.text.clone(),
                    body: String::new(),
                    examples: vec![],
                    cr_version: CrVersion::try_new("20260819".to_owned())
                        .map_err(anyhow::Error::from)?,
                }],
                ..Context::default()
            })
        }
        async fn lookup_rules(&self, ids: &[RuleId]) -> Result<Vec<RuleChunk>, JudgeError> {
            assert_eq!(ids.len(), 1);
            Ok(vec![])
        }
    }

    fn extraction() -> Extraction {
        Extraction {
            card_spans: vec![],
            concepts: vec![],
            primary: CategoryGuess {
                category: Category::Combat,
                confidence: Confidence::High,
            },
            secondary: vec![],
            source: Source::Cr,
        }
    }

    #[tokio::test]
    async fn captures_per_question_and_take_removes() -> Result<(), JudgeError> {
        let r = CapturingRetriever::new(Arc::new(Stub));
        let q1 = Question {
            thread_id: "t".into(),
            text: "one".into(),
        };
        let q2 = Question {
            thread_id: "t".into(),
            text: "two".into(),
        };
        let ctx = r.retrieve(&q1, &[], &extraction()).await?;
        r.retrieve(&q2, &[], &extraction()).await?;
        assert_eq!(r.len(), 2);
        let taken = r.take(&q1);
        assert_eq!(taken.as_ref(), Some(&ctx));
        assert_eq!(
            taken
                .and_then(|c| c.rules.first().map(|r| r.heading.clone()))
                .as_deref(),
            Some("one")
        );
        assert!(r.take(&q1).is_none(), "taken once");
        assert_eq!(r.len(), 1);
        // Same text in another thread is a different key.
        assert!(
            r.take(&Question {
                thread_id: "other".into(),
                text: "two".into()
            })
            .is_none()
        );
        assert!(r.take(&q2).is_some());
        assert!(r.is_empty());
        // lookup_rules passes straight through.
        let id = RuleId::try_new("613".to_owned()).map_err(anyhow::Error::from)?;
        assert!(r.lookup_rules(&[id]).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn identical_questions_in_flight_each_get_a_context() -> Result<(), JudgeError> {
        let r = CapturingRetriever::new(Arc::new(Stub));
        let q = Question {
            thread_id: "t".into(),
            text: "same".into(),
        };
        r.retrieve(&q, &[], &extraction()).await?;
        r.retrieve(&q, &[], &extraction()).await?;
        assert_eq!(r.len(), 2);
        assert!(r.take(&q).is_some());
        assert!(r.take(&q).is_some(), "the second run is persisted too");
        assert!(r.take(&q).is_none());
        assert!(r.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn clears_when_too_many_are_never_taken() -> Result<(), JudgeError> {
        let r = CapturingRetriever::new(Arc::new(Stub));
        for i in 0..MAX_HELD {
            r.retrieve(
                &Question {
                    thread_id: "t".into(),
                    text: i.to_string(),
                },
                &[],
                &extraction(),
            )
            .await?;
        }
        assert_eq!(r.len(), MAX_HELD);
        r.retrieve(
            &Question {
                thread_id: "t".into(),
                text: "last".into(),
            },
            &[],
            &extraction(),
        )
        .await?;
        assert_eq!(r.len(), 1);
        Ok(())
    }
}
