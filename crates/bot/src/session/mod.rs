//! Agent-driven judging: the pipeline in pull mode.
//!
//! [`machine::Session`] is the state machine; [`Sessions`] runs it over a
//! store, so that each step is one call with a session id — the shape both
//! an MCP tool and a `judge-cli` subcommand need, since neither keeps a
//! process alive between steps. Every step loads the session, applies the
//! transition against the real ports (Postgres resolver and retriever, the
//! call store), and saves it back under optimistic concurrency, so two
//! callers stepping one session cannot both win.
//!
//! No model is called here. Whoever drives the session — Claude Code, another
//! agent, a person with a text editor — is the extractor and the synthesizer,
//! and the validation that admits their verdict is the same
//! `Verdict::validate` the Anthropic path uses.

pub mod machine;

use std::{sync::Arc, time::Duration};

use judge_core::{
    CallId, CallStore, Extraction, Qa, Resolver, Retriever, RuleChunk, RuleId, Unvalidated, Validated, Verdict,
};

use crate::{
    db::{PgSessionStore, Saved},
    synth::{Budget, Harness},
};
pub use machine::{
    AgentThread, Extracted, ExtractionPrompt, MAX_ATTEMPTS, MAX_EXTRACTION_ITEM_CHARS, MAX_EXTRACTION_ITEMS,
    MAX_LOOKUP_IDS, MAX_QUESTION_CHARS, Outcome, PersistCall, Session, SessionError, SessionId, Stage,
    SynthesisPrompt, THREAD_PREFIX,
};

/// A session an agent has not touched for this long is dropped.
pub const DEFAULT_TTL: Duration = Duration::from_hours(1);
/// Thread history handed to the prompts.
pub const DEFAULT_HISTORY: usize = 5;

/// The ports and knobs a session step needs.
pub struct Sessions {
    store: PgSessionStore,
    resolver: Arc<dyn Resolver>,
    retriever: Arc<dyn Retriever>,
    history: Arc<dyn CallStore>,
    persist: Arc<dyn PersistCall>,
    budget: Budget,
    harness: Harness,
    history_len: usize,
    ttl: Duration,
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sessions").field("harness", &self.harness).field("ttl", &self.ttl).finish_non_exhaustive()
    }
}

/// What `begin` hands back: the id to carry, the thread for follow-ups, and
/// the first prompt.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct Begun {
    /// The session id every later step takes.
    pub session: SessionId,
    /// The thread the question is in; pass it to `begin` again for a follow-up.
    pub thread: AgentThread,
    /// The extraction prompt.
    pub extraction: ExtractionPrompt,
}

/// The prompt for whichever step a session is at.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Prompt {
    /// Waiting for an extraction.
    Extraction(ExtractionPrompt),
    /// Waiting for a verdict.
    Synthesis(SynthesisPrompt),
}

impl Sessions {
    /// Wire the steps. `harness` decides how the synthesis prompt tells the
    /// agent to look rules up and to answer. `history` supplies earlier Q&As
    /// of the thread; `persist` stores accepted verdicts (usually the same
    /// `PgCallStore`, which implements both).
    #[must_use]
    pub fn new(
        store: PgSessionStore,
        resolver: Arc<dyn Resolver>,
        retriever: Arc<dyn Retriever>,
        history: Arc<dyn CallStore>,
        persist: Arc<dyn PersistCall>,
        harness: Harness,
    ) -> Self {
        Self {
            store,
            resolver,
            retriever,
            history,
            persist,
            budget: Budget::default(),
            harness,
            history_len: DEFAULT_HISTORY,
            ttl: DEFAULT_TTL,
        }
    }

    /// Override the synthesis material budget.
    #[must_use]
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// Override how many earlier Q&As of the thread are loaded.
    #[must_use]
    pub fn with_history_len(mut self, n: usize) -> Self {
        self.history_len = n;
        self
    }

    /// Override the idle time after which a session expires.
    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Start a session for `text` in `thread` (a fresh one, or one returned
    /// by an earlier `begin`, whose persisted calls become history) and
    /// return the extraction prompt.
    ///
    /// # Errors
    /// `Invalid` for an empty or over-long question; the store.
    pub async fn begin(&self, thread: AgentThread, text: String) -> Result<Begun, SessionError> {
        let history: Vec<Qa> = match self.history.history(thread.as_str(), self.history_len).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = format_args!("{e:#}"), "thread history unavailable; judging without it");
                vec![]
            }
        };
        let session = Session::begin(SessionId::new(), thread.clone(), text, history)?;
        let extraction = session.extraction_prompt(self.history_len)?;
        self.store.insert(&session, self.ttl).await?;
        Ok(Begun { session: session.id, thread, extraction })
    }

    /// The session as stored.
    ///
    /// # Errors
    /// `Unknown`; the store.
    pub async fn load(&self, id: SessionId) -> Result<Session, SessionError> {
        self.store.load(id).await?.ok_or(SessionError::Unknown(id))
    }

    /// Re-issue the prompt for the session's current step.
    ///
    /// # Errors
    /// `Unknown`; `WrongStage` if the session is closed; the store.
    pub async fn prompt(&self, id: SessionId) -> Result<Prompt, SessionError> {
        let s = self.load(id).await?;
        match &s.stage {
            Stage::AwaitingExtraction { .. } => Ok(Prompt::Extraction(s.extraction_prompt(self.history_len)?)),
            Stage::AwaitingVerdict { .. } => Ok(Prompt::Synthesis(s.synthesis_prompt(self.harness, &self.budget)?)),
            Stage::Closed(_) => Err(SessionError::WrongStage { expected: "an open session", actual: "closed" }),
        }
    }

    /// Step: the agent's extraction.
    ///
    /// # Errors
    /// See [`Session::submit_extraction`]; plus `Unknown`, `Conflict`, the store.
    pub async fn submit_extraction(&self, id: SessionId, e: Extraction) -> Result<Extracted, SessionError> {
        let (mut s, version) = self.store.load_versioned(id).await?.ok_or(SessionError::Unknown(id))?;
        let out = s
            .submit_extraction(e, self.resolver.as_ref(), self.retriever.as_ref(), self.harness, &self.budget)
            .await?;
        self.save(&s, version).await?;
        Ok(out)
    }

    /// Step: the one `lookup_rules` round.
    ///
    /// # Errors
    /// See [`Session::lookup_rules`]; plus `Unknown`, `Conflict`, the store.
    pub async fn lookup_rules(&self, id: SessionId, ids: &[RuleId]) -> Result<Vec<RuleChunk>, SessionError> {
        let (mut s, version) = self.store.load_versioned(id).await?.ok_or(SessionError::Unknown(id))?;
        let chunks = s.lookup_rules(ids, self.retriever.as_ref()).await?;
        self.save(&s, version).await?;
        Ok(chunks)
    }

    /// Step: the agent's verdict. A rejection is saved (the session now waits
    /// for the retry) and returned as the error.
    ///
    /// # Errors
    /// See [`Session::submit_verdict`]; plus `Unknown`, `Conflict`, the store.
    pub async fn submit_verdict(&self, id: SessionId, v: Verdict<Unvalidated>) -> Result<Verdict<Validated>, SessionError> {
        let (mut s, version) = self.store.load_versioned(id).await?.ok_or(SessionError::Unknown(id))?;
        let out = s.submit_verdict(v, self.retriever.as_ref(), self.harness, &self.budget).await;
        match &out {
            // The stage moved: save it, whichever way it went.
            Ok(_) | Err(SessionError::Rejected { .. } | SessionError::Exhausted { .. }) => {
                self.save(&s, version).await?;
            }
            // The stage did not move; there is nothing to save.
            Err(
                SessionError::Unknown(_)
                | SessionError::Conflict(_)
                | SessionError::WrongStage { .. }
                | SessionError::Invalid(_)
                | SessionError::LookupUsed
                | SessionError::Pipeline(_),
            ) => {}
        }
        out
    }

    /// Step: persist the accepted verdict as a call. Idempotent: the store
    /// keys the call by session, so a retry or a concurrent call gets the
    /// same id.
    ///
    /// # Errors
    /// See [`Session::persist`]; plus `Unknown`, `Conflict`, the store.
    pub async fn persist(&self, id: SessionId) -> Result<CallId, SessionError> {
        let (mut s, version) = self.store.load_versioned(id).await?.ok_or(SessionError::Unknown(id))?;
        let call = s.persist(self.persist.as_ref()).await?;
        match self.save(&s, version).await {
            Ok(()) => {}
            // A concurrent persist won the save; it stored the same call.
            Err(SessionError::Conflict(_)) => tracing::debug!(session = %id, "persist raced; the call is the same"),
            Err(e) => return Err(e),
        }
        Ok(call)
    }

    async fn save(&self, s: &Session, version: crate::db::Version) -> Result<(), SessionError> {
        match self.store.save(s, version, self.ttl).await? {
            Saved::Yes => Ok(()),
            Saved::Conflict => Err(SessionError::Conflict(s.id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{PgCallStore, PgResolver, PgRetriever, tests::seed};
    use judge_core::{Category, CategoryGuess, Citation, Confidence, RuleId, Source, Verdict};
    use sqlx::PgPool;

    fn sessions(pool: &PgPool) -> Sessions {
        let calls = Arc::new(PgCallStore::new(pool.clone()));
        Sessions::new(
            PgSessionStore::new(pool.clone()),
            Arc::new(PgResolver::new(pool.clone())),
            Arc::new(PgRetriever::new(pool.clone())),
            Arc::clone(&calls) as Arc<dyn CallStore>,
            calls as Arc<dyn PersistCall>,
            Harness::Cli,
        )
    }

    fn extraction() -> Extraction {
        Extraction {
            card_spans: vec!["[[Lightning Bolt]]".into()],
            concepts: vec!["damage".into()],
            primary: CategoryGuess { category: Category::DamageAndLife, confidence: Confidence::High },
            secondary: vec![],
            source: Source::Cr,
        }
    }

    /// End to end against the real adapters, then the two persist races:
    /// a stale version and a repeated call both yield the one stored call.
    #[sqlx::test]
    async fn a_session_over_postgres_persists_exactly_one_call(pool: PgPool) -> anyhow::Result<()> {
        seed(&pool).await?;
        let sessions = sessions(&pool);
        let begun = sessions.begin(AgentThread::new(), "does bolt kill a 3/3?".into()).await?;
        let Extracted::Ready(prompt) = sessions.submit_extraction(begun.session, extraction()).await? else {
            return Err(anyhow::anyhow!("expected ready"));
        };
        assert!(prompt.material.contains("Lightning Bolt"));
        assert!(prompt.system.contains("judge-cli rules"));
        // Quote from the seeded 702.15 body, whichever seeded rule it is; take it from the material.
        let chunk = sessions.load(begun.session).await?;
        let Stage::AwaitingVerdict { ctx, .. } = &chunk.stage else {
            return Err(anyhow::anyhow!("expected awaiting_verdict"));
        };
        let rule = ctx.rules.first().ok_or_else(|| anyhow::anyhow!("no rules retrieved"))?;
        let quote = rule.body.lines().next().unwrap_or_default().to_owned();
        let v = Verdict::new(
            "Yes: three damage to a creature with toughness three is lethal, and it is destroyed as a state-based action.".into(),
            Confidence::High,
            vec![Citation::Rule { id: rule.id.clone(), quote: judge_core::Quote::try_new(quote)? }],
            Category::DamageAndLife,
        );
        sessions.submit_verdict(begun.session, v).await?;

        // Two persists from the same loaded version, then a third from fresh.
        let (s1, v1) = sessions.store.load_versioned(begun.session).await?.ok_or_else(|| anyhow::anyhow!("gone"))?;
        let mut a = s1.clone();
        let mut b = s1;
        let ca = a.persist(sessions.persist.as_ref()).await?;
        let cb = b.persist(sessions.persist.as_ref()).await?;
        assert_eq!(ca, cb, "the store keys the call by session");
        sessions.store.save(&a, v1, DEFAULT_TTL).await?;
        let cc = sessions.persist(begun.session).await?;
        assert_eq!(ca, cc);
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM calls").fetch_one(&pool).await?;
        assert_eq!(n, 1, "exactly one call row");
        // The follow-up sees the answer as history and files under the same agent thread.
        let again = sessions.begin(begun.thread.clone(), "and a 4/4?".into()).await?;
        assert!(again.extraction.user.contains("does bolt kill a 3/3?"));
        let _ = RuleId::try_new("100".to_owned())?;
        Ok(())
    }

    #[sqlx::test]
    async fn unknown_and_conflicting_steps_are_named(pool: PgPool) -> anyhow::Result<()> {
        seed(&pool).await?;
        let sessions = sessions(&pool);
        let missing = SessionId::new();
        assert!(matches!(sessions.prompt(missing).await, Err(SessionError::Unknown(id)) if id == missing));
        let begun = sessions.begin(AgentThread::new(), "q?".into()).await?;
        let (s, stale) = sessions.store.load_versioned(begun.session).await?.ok_or_else(|| anyhow::anyhow!("gone"))?;
        sessions.submit_extraction(begun.session, extraction()).await?;
        assert!(matches!(sessions.save(&s, stale).await, Err(SessionError::Conflict(_))));
        Ok(())
    }
}
