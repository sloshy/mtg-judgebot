//! Port traits. Adapters live in `crates/bot` and `crates/anthropic`.

use async_trait::async_trait;

use crate::{
    CallId, Card, Context, Extraction, JudgeError, Qa, Question, Rejection, Resolution, RuleChunk, RuleId,
    Score, Validated, Verdict, verdict::Unvalidated,
};

/// Pipeline steps 1 + 3: entity extraction and classification (one LLM call).
#[async_trait]
pub trait Extractor: Send + Sync {
    /// Split `q` into card-name spans and rules concepts, and classify it.
    async fn extract(&self, q: &Question, history: &[Qa]) -> Result<Extraction, JudgeError>;
}

/// Pipeline step 2: resolve one card-name span. Never guesses.
#[async_trait]
pub trait Resolver: Send + Sync {
    /// Resolve a single span through the alias → bracket → printed-name → fuzzy ladder.
    async fn resolve(&self, span: &str) -> Result<Resolution, JudgeError>;
}

/// Pipeline step 4: build Context; also serves the synthesizer's `lookup_rules` tool.
#[async_trait]
pub trait Retriever: Send + Sync {
    /// Build the Context for `q`. Thread history is filled in by `judge()`.
    async fn retrieve(&self, q: &Question, cards: &[Card], e: &Extraction) -> Result<Context, JudgeError>;
    /// Fetch full rule chunks by id (expands a subsection id to all rules under it).
    async fn lookup_rules(&self, ids: &[RuleId]) -> Result<Vec<RuleChunk>, JudgeError>;
}

/// Pipeline step 5: produce an unvalidated verdict from Context.
///
/// The synthesizer may extend `ctx` with chunks fetched during its single
/// `lookup_rules` round, so that validation sees them.
#[async_trait]
pub trait Synthesizer: Send + Sync {
    /// Answer `q` from `ctx`. `rejected` is why a previous attempt failed
    /// validation (a bad citation, or an empty verdict), so the model can
    /// correct it on the retry.
    async fn answer(
        &self,
        q: &Question,
        ctx: &mut Context,
        rejected: Option<&Rejection>,
    ) -> Result<Verdict<Unvalidated>, JudgeError>;
}

/// What an embedding will be used for; retrieval quality depends on telling
/// the provider which side of the search a text is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputKind {
    /// Text being indexed (rule chunks, glossary, prior calls).
    Document,
    /// A search query (the user's question / extracted concepts).
    Query,
}

/// Text embeddings (Voyage today; the trait exists so this can change).
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed `texts` in order. Implementations reject an empty slice.
    async fn embed(&self, texts: &[&str], kind: InputKind) -> Result<Vec<Vec<f32>>, JudgeError>;
    /// Vector width produced by `embed`.
    fn dimensions(&self) -> usize;
}

/// Pipeline step 6: persist calls and ratings. Only validated verdicts type-check.
#[async_trait]
pub trait CallStore: Send + Sync {
    /// Store the question, verdict and the ids of the context it was answered from.
    async fn persist(&self, q: &Question, v: &Verdict<Validated>, ctx: &Context) -> Result<CallId, JudgeError>;
    /// Record (or replace) one user's rating of a call.
    async fn rate(&self, call: CallId, user_id: &str, score: Score, is_judge: bool) -> Result<(), JudgeError>;
    /// The last `n` question/answer pairs persisted in `thread_id`, oldest
    /// first: the shape [`crate::judge`] takes as thread history. `n == 0`
    /// yields nothing.
    async fn history(&self, thread_id: &str, n: usize) -> Result<Vec<Qa>, JudgeError>;
}
