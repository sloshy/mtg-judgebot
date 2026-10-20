//! Port traits. Adapters live in `crates/bot` and `crates/anthropic`.

use async_trait::async_trait;

use crate::{
    CallId, Card, Context, Earlier, Extraction, FailedCall, FailedCallId, Forgotten, HistoryScope,
    JudgeError, Origin, Qa, Question, Rater, RaterId, Rating, RejectedAttempt, Resolution,
    RuleChunk, RuleId, RunRecord, Tally, Validated, Verdict, verdict::Unvalidated,
};

/// Pipeline steps 1 + 3: entity extraction and classification (one LLM call).
#[async_trait]
pub trait Extractor: Send + Sync {
    /// Split `q` into card-name spans and rules concepts, and classify it,
    /// billing the model call to `tally`.
    async fn extract(
        &self,
        q: &Question,
        history: &[Qa],
        tally: &Tally,
    ) -> Result<Extraction, JudgeError>;
}

/// Pipeline step 2: resolve one card-name span. Never guesses.
#[async_trait]
pub trait Resolver: Send + Sync {
    /// Resolve a single span through the alias → bracket → printed-name → fuzzy resolution order.
    async fn resolve(&self, span: &str) -> Result<Resolution, JudgeError>;
}

/// Pipeline step 4: build Context; also serves the synthesizer's `lookup_rules` tool.
#[async_trait]
pub trait Retriever: Send + Sync {
    /// Build the Context for `q`, billing the query's embedding to `tally`.
    /// Thread history is filled in by `judge()`.
    async fn retrieve(
        &self,
        q: &Question,
        cards: &[Card],
        e: &Extraction,
        tally: &Tally,
    ) -> Result<Context, JudgeError>;
    /// Fetch full rule chunks by id (expands a subsection id to all rules under it).
    async fn lookup_rules(&self, ids: &[RuleId]) -> Result<Vec<RuleChunk>, JudgeError>;
}

/// Pipeline step 5: produce an unvalidated verdict from Context.
///
/// The synthesizer may extend `ctx` with chunks fetched during its single
/// `lookup_rules` round, so that validation sees them.
#[async_trait]
pub trait Synthesizer: Send + Sync {
    /// Answer `q` from `ctx`. `rejected` is the previous attempt that failed
    /// validation (why, and what it answered), so the model can correct it on
    /// the retry. Every model call is billed to `tally`.
    async fn answer(
        &self,
        q: &Question,
        ctx: &mut Context,
        rejected: Option<&RejectedAttempt>,
        tally: &Tally,
    ) -> Result<Verdict<Unvalidated>, JudgeError>;
}

/// What became of a rating ([`CallStore::rate`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rated {
    /// Stored, or replaced the rater's earlier one.
    Recorded,
    /// The call is no longer stored, so there is nothing to rate.
    CallGone,
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
    /// Embed `texts` in order, billing the request to `tally`.
    /// Implementations reject an empty slice.
    async fn embed(
        &self,
        texts: &[&str],
        kind: InputKind,
        tally: &Tally,
    ) -> Result<Vec<Vec<f32>>, JudgeError>;
    /// Vector width produced by `embed`.
    fn dimensions(&self) -> usize;
}

/// Pipeline step 6: persist calls and ratings. Only validated verdicts type-check.
#[async_trait]
pub trait CallStore: Send + Sync {
    /// Store the question, verdict and the ids of the context it was
    /// answered from, with where it came from. A private call is stored too:
    /// [`Self::history`] and the prior-call examples leave it out. The
    /// call's embedding is billed to `tally`.
    async fn persist(
        &self,
        q: &Question,
        origin: &Origin,
        v: &Verdict<Validated>,
        ctx: &Context,
        tally: &Tally,
    ) -> Result<CallId, JudgeError>;
    /// Record (or replace) one rater's rating of a call, with its reason
    /// when it finds fault, or say the call is no longer stored (the
    /// retention deleted it). A reason opens the rating's feedback-queue
    /// item, or reopens it; a Correct closes nothing but drops an item the
    /// operator has not written on. Each write also clears web raters'
    /// addresses older than `FEEDBACK_IP_DAYS`.
    async fn rate(&self, call: CallId, rater: &Rater, rating: &Rating)
    -> Result<Rated, JudgeError>;
    /// The rater's current rating of a call, if they have one. One indexed
    /// read: a Discord click must be answered within 3 s of arriving.
    async fn earlier_rating(
        &self,
        call: CallId,
        rater: RaterId,
    ) -> Result<Option<Earlier>, JudgeError>;
    /// The last `n` question/answer pairs persisted in `thread_id` that
    /// `scope` admits, oldest first: the shape [`crate::judge`] takes as
    /// thread history ([`Origin::history_scope`] says which scope a question
    /// gets). `n == 0` yields nothing.
    async fn history(
        &self,
        thread_id: &str,
        n: usize,
        scope: HistoryScope,
    ) -> Result<Vec<Qa>, JudgeError>;
    /// "Forget me" for the Discord user `user_id`: delete their ratings
    /// with their reasons and feedback-queue items, remove their id and username from the calls they asked (and, on a
    /// private call, its question and answer), and anonymize the failed calls
    /// they asked, returning how many of each.
    async fn forget_user(&self, user_id: &str) -> Result<Forgotten, JudgeError>;
    /// Keep a failed call for troubleshooting, returning its id. The store
    /// bounds the size of every field and the number of rows it keeps.
    async fn record_failure(&self, failed: &FailedCall) -> Result<FailedCallId, JudgeError>;
    /// Record one run of the pipeline (`judge_runs`), whatever its outcome.
    async fn record_run(&self, run: &RunRecord) -> Result<(), JudgeError>;
}
