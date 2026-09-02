//! The pipeline as a state machine an outside agent steps through.
//!
//! `judge()` *pushes*: it calls the extractor and the synthesizer and waits.
//! An agent driving the pipeline from the outside *pulls*: it asks for the
//! next prompt, does the model's work itself, and hands the result back. So
//! the same steps — extract, resolve, retrieve, one `lookup_rules` round,
//! synthesize, validate, retry once — are here as transitions on a
//! [`Session`], with the retrieved [`Context`] carried along, and every
//! model call replaced by a prompt going out and JSON coming back in.
//!
//! What the type system enforced across `Synth<Fresh | ToolRequested |
//! Final>` and `judge()`'s single retry is enforced here by [`Stage`]: the
//! tool round is refused once used, the second rejection closes the session,
//! and a validated verdict is only ever produced by `Verdict::validate`
//! against the session's own context. A `Session` is a value with no I/O of
//! its own; the ports it needs are passed to each transition, and
//! [`super::Sessions`] wraps that in a store.
//!
//! Sessions are unauthenticated by design (anyone who can reach the tools can
//! open one), so what the API path leaves to `max_tokens` and Discord's
//! message limit is bounded here explicitly: the question, the extraction's
//! spans and concepts, the lookup request, the answer.

use std::sync::LazyLock;

use async_trait::async_trait;
use judge_core::{
    AnswerableSource, CallId, Context, Extraction, JudgeError, MAX_ANSWER_CHARS, Qa, Question, Rejection, Resolver,
    Retriever, RuleChunk, RuleId, Source, Unvalidated, Validated, Verdict, judge::collect_resolved,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    extract,
    synth::{self, Budget, Harness, hydrate_leaf_citations},
};

/// How many synthesis attempts a session gets: the first, and one retry
/// after a rejection, exactly as `judge()`.
pub const MAX_ATTEMPTS: u8 = 2;
/// Longest question accepted, in characters.
pub const MAX_QUESTION_CHARS: usize = 2000;
/// Most card spans and most concepts an extraction may carry.
pub const MAX_EXTRACTION_ITEMS: usize = 20;
/// Longest span or concept, in characters.
pub const MAX_EXTRACTION_ITEM_CHARS: usize = 200;
/// Most ids one `lookup_rules` round may ask for.
pub const MAX_LOOKUP_IDS: usize = 10;

/// The prefix every agent-session thread id carries.
pub const THREAD_PREFIX: &str = "agent:";

/// Identifies one session. Minted by the server, carried by the agent as a
/// plain tool argument (the shape MCP 2026-07-28 prescribes for cross-call
/// state).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(description = "Session id (uuid) returned by begin_session")]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// A fresh random id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for SessionId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.trim().parse().map(Self)
    }
}

/// A thread id an agent session may read history from and file calls under:
/// always `agent:<uuid>`, never anything else.
///
/// Thread ids are how the call store keeps one conversation's history apart
/// from another's, and Discord threads are named by their Discord id. A
/// caller-chosen string would let an agent read a Discord thread's earlier
/// Q&A into its prompts and append its own answer to that thread's history.
/// This type makes that unrepresentable: the only constructors are [`Self::new`]
/// (a fresh id) and parsing a string that already has the prefix and a uuid
/// after it (a follow-up in a thread this front door minted).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(description = "Thread id as returned by an earlier reply: `agent:<uuid>`")]
pub struct AgentThread(String);

impl AgentThread {
    /// A fresh thread.
    #[must_use]
    pub fn new() -> Self {
        Self(format!("{THREAD_PREFIX}{}", Uuid::new_v4()))
    }

    /// The id as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for AgentThread {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AgentThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for AgentThread {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        match s.strip_prefix(THREAD_PREFIX).map(str::parse::<Uuid>) {
            Some(Ok(_)) => Ok(Self(s.to_owned())),
            _ => Err(format!("thread must be an id this server issued ({THREAD_PREFIX}<uuid>), got {s:?}")),
        }
    }
}

impl TryFrom<String> for AgentThread {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<AgentThread> for String {
    fn from(t: AgentThread) -> Self {
        t.0
    }
}

/// Where a session is in the pipeline. Serialized whole into the store.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum Stage {
    /// Steps 1–3 are the agent's: it owes an [`Extraction`].
    AwaitingExtraction {
        /// Thread history handed to the extraction prompt and, later, to the
        /// synthesis material.
        history: Vec<Qa>,
    },
    /// Steps 1–4 are done; the agent owes a verdict.
    AwaitingVerdict {
        /// The extraction's classification, stamped onto the verdict.
        source: AnswerableSource,
        /// Everything the synthesis model sees, and what citations are checked against.
        ctx: Context,
        /// Whether the one `lookup_rules` round has been spent.
        lookup_used: bool,
        /// Why the previous attempt was rejected, rendered into the retry prompt.
        rejected: Option<Rejection>,
        /// Verdicts submitted so far.
        attempts: u8,
    },
    /// The session is over; no transition applies.
    Closed(Outcome),
}

/// How a session ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// A verdict passed validation.
    Answered {
        /// The accepted verdict. Stored unvalidated because `Validated` is
        /// deliberately not deserializable; it is re-validated against `ctx`
        /// (pure, and the same check that admitted it) whenever a
        /// `Verdict<Validated>` is needed, e.g. to persist. Serializing an
        /// unvalidated verdict drops unreadable citations, which is safe here
        /// only because one that validated had none.
        verdict: Verdict<Unvalidated>,
        /// The context it was validated against, after any sub-rule hydration.
        ctx: Box<Context>,
        /// Its source.
        source: AnswerableSource,
        /// The stored call, once persisted.
        call: Option<CallId>,
    },
    /// The extraction classified the question outside the rules bodies.
    OutOfScope {
        /// The classification.
        source: Source,
    },
    /// Both attempts were rejected.
    Failed {
        /// The last rejection.
        rejection: Rejection,
    },
}

/// The extraction step as a prompt the agent runs itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExtractionPrompt {
    /// System prompt: task, taxonomy, source definitions.
    pub system: String,
    /// User turn: thread history and the question.
    pub user: String,
    /// JSON Schema the reply must satisfy.
    pub schema: serde_json::Value,
}

/// The synthesis step as a prompt the agent runs itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SynthesisPrompt {
    /// System prompt, worded for the harness (how to look rules up, how to answer).
    pub system: String,
    /// The material: cards, CR excerpts, rulings, aids, history.
    pub material: String,
    /// The question, preceded by the rejection notice on a retry.
    pub question: String,
    /// JSON Schema the verdict must satisfy.
    pub schema: serde_json::Value,
    /// Whether the one `lookup_rules` round is still available.
    pub lookup_available: bool,
}

/// What a submitted extraction led to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Extracted {
    /// Cards resolved and material retrieved; synthesize now.
    Ready(SynthesisPrompt),
    /// The question is outside the rules bodies; the session is closed.
    OutOfScope {
        /// The classification.
        source: Source,
    },
}

/// Why a transition did not apply.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// No such session (never existed, or expired).
    #[error("no session {0} (unknown, or expired)")]
    Unknown(SessionId),
    /// Another step ran on this session concurrently; reload and retry.
    #[error("session {0} changed underneath this step (a concurrent step ran); re-read it and retry")]
    Conflict(SessionId),
    /// The session is not at the stage this transition needs.
    #[error("session is {actual}; expected {expected}")]
    WrongStage {
        /// The stage the transition needs.
        expected: &'static str,
        /// The stage the session is at.
        actual: &'static str,
    },
    /// The input broke a size or shape rule; the session is unchanged.
    #[error("{0}")]
    Invalid(String),
    /// The `lookup_rules` round was already spent (or forfeited by a retry).
    #[error("the one lookup_rules round has already been used")]
    LookupUsed,
    /// The verdict was rejected; the session is waiting for the retry.
    #[error("verdict rejected ({rejection}); one retry remains")]
    Rejected {
        /// Why.
        rejection: Rejection,
        /// The synthesis prompt again, with the rejection notice rendered in.
        retry: SynthesisPrompt,
    },
    /// The retry was rejected too; the session is closed.
    #[error("verdict rejected again ({rejection}); the session is closed")]
    Exhausted {
        /// Why.
        rejection: Rejection,
    },
    /// Card resolution, retrieval, validation or the store failed. The
    /// actionable ones (`AmbiguousCards`, `CardsNotFound`) leave the session
    /// where it was so the agent can resubmit.
    #[error(transparent)]
    Pipeline(#[from] JudgeError),
}

/// Step 6 for a session: store the accepted verdict as a call, keyed by the
/// session so that doing it twice (a retry after a lost reply, two callers
/// at once) yields the same call rather than two rows in the prior-call pool.
#[async_trait]
pub trait PersistCall: Send + Sync {
    /// Store, or return the call already stored for `session`.
    async fn persist_call(
        &self,
        session: SessionId,
        q: &Question,
        v: &Verdict<Validated>,
        ctx: &Context,
    ) -> Result<CallId, JudgeError>;
}

/// One agent-driven run of the pipeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Its id.
    pub id: SessionId,
    /// The question, in an [`AgentThread`].
    pub question: Question,
    /// Where it is.
    pub stage: Stage,
}

impl Stage {
    /// A short name for error messages.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Stage::AwaitingExtraction { .. } => "awaiting_extraction",
            Stage::AwaitingVerdict { .. } => "awaiting_verdict",
            Stage::Closed(_) => "closed",
        }
    }
}

static EXTRACTION_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(extract::schema);
static VERDICT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(|| serde_json::to_value(schemars::schema_for!(Verdict<Unvalidated>)).unwrap_or_default());

impl Session {
    /// A session at its first step.
    ///
    /// # Errors
    /// `Invalid` for an empty or over-long question.
    pub fn begin(id: SessionId, thread: AgentThread, text: String, history: Vec<Qa>) -> Result<Self, SessionError> {
        if text.trim().is_empty() {
            return Err(SessionError::Invalid("question must not be empty".into()));
        }
        if text.chars().count() > MAX_QUESTION_CHARS {
            return Err(SessionError::Invalid(format!("question must be at most {MAX_QUESTION_CHARS} characters")));
        }
        Ok(Self {
            id,
            question: Question { thread_id: thread.into(), text },
            stage: Stage::AwaitingExtraction { history },
        })
    }

    /// The extraction prompt (steps 1 + 3).
    ///
    /// # Errors
    /// `WrongStage` unless the session is awaiting an extraction.
    pub fn extraction_prompt(&self, history_turns: usize) -> Result<ExtractionPrompt, SessionError> {
        let Stage::AwaitingExtraction { history } = &self.stage else {
            return Err(self.wrong_stage("awaiting_extraction"));
        };
        Ok(ExtractionPrompt {
            system: extract::system_prompt(),
            user: extract::user_turn(&self.question, history, history_turns),
            schema: EXTRACTION_SCHEMA.clone(),
        })
    }

    /// Steps 2 + 4 on the agent's extraction: resolve the card spans, build
    /// the context, and move to synthesis.
    ///
    /// # Errors
    /// `WrongStage`; `Invalid` for an over-sized extraction; `Pipeline(AmbiguousCards
    /// | CardsNotFound)` with the stage unchanged, so the agent can resubmit with
    /// the span written as `[[Full Name]]`; `Pipeline(Upstream)` from the store.
    pub async fn submit_extraction(
        &mut self,
        e: Extraction,
        resolver: &dyn Resolver,
        retriever: &dyn Retriever,
        harness: Harness,
        budget: &Budget,
    ) -> Result<Extracted, SessionError> {
        let Stage::AwaitingExtraction { history } = &self.stage else {
            return Err(self.wrong_stage("awaiting_extraction"));
        };
        check_extraction(&e)?;
        let Some(source) = e.source.answerable() else {
            self.stage = Stage::Closed(Outcome::OutOfScope { source: e.source });
            return Ok(Extracted::OutOfScope { source: e.source });
        };
        let mut resolutions = Vec::with_capacity(e.card_spans.len());
        for span in &e.card_spans {
            resolutions.push(resolver.resolve(span).await?);
        }
        let cards = collect_resolved(resolutions)?;
        let mut ctx = retriever.retrieve(&self.question, &cards, &e).await?;
        ctx.history.clone_from(history);
        let prompt = synthesis_prompt(&self.question, &ctx, None, true, harness, budget);
        self.stage = Stage::AwaitingVerdict { source, ctx, lookup_used: false, rejected: None, attempts: 0 };
        Ok(Extracted::Ready(prompt))
    }

    /// The synthesizer's one `lookup_rules` round: fetch `ids` (rules or whole
    /// subsections), add them to the context, and return them.
    ///
    /// # Errors
    /// `WrongStage`; `Invalid` for no ids or too many (the round is not
    /// spent); `LookupUsed` if the round was spent, or forfeited by a retry
    /// (the Anthropic path disables the tool on the retry too).
    pub async fn lookup_rules(&mut self, ids: &[RuleId], retriever: &dyn Retriever) -> Result<Vec<RuleChunk>, SessionError> {
        let Stage::AwaitingVerdict { ctx, lookup_used, .. } = &mut self.stage else {
            return Err(self.wrong_stage("awaiting_verdict"));
        };
        if *lookup_used {
            return Err(SessionError::LookupUsed);
        }
        if ids.is_empty() {
            return Err(SessionError::Invalid("lookup_rules needs at least one rule id".into()));
        }
        if ids.len() > MAX_LOOKUP_IDS {
            return Err(SessionError::Invalid(format!("lookup_rules takes at most {MAX_LOOKUP_IDS} ids per round")));
        }
        let chunks = retriever.lookup_rules(ids).await?;
        for c in &chunks {
            if !ctx.tool_round.contains(&c.id) {
                ctx.tool_round.push(c.id.clone());
            }
        }
        ctx.extend_rules(chunks.clone());
        *lookup_used = true;
        Ok(chunks)
    }

    /// Step 5's validation on the agent's verdict.
    ///
    /// # Errors
    /// `WrongStage`; `Rejected` (the session waits for one retry, with the
    /// lookup round forfeited); `Exhausted` (the retry failed too; closed);
    /// `Pipeline(Upstream)` if hydrating cited sub-rules failed, stage unchanged.
    pub async fn submit_verdict(
        &mut self,
        v: Verdict<Unvalidated>,
        retriever: &dyn Retriever,
        harness: Harness,
        budget: &Budget,
    ) -> Result<Verdict<Validated>, SessionError> {
        let Stage::AwaitingVerdict { source, ctx, lookup_used, rejected, attempts } = &mut self.stage else {
            return Err(self.wrong_stage("awaiting_verdict"));
        };
        // Size before validation: an over-long answer is stored and rendered
        // into later prompts whole if accepted, so it is a rejection, and it
        // does not cost a lookup round trip.
        let chars = v.answer().trim().chars().count();
        let rejection = if chars > MAX_ANSWER_CHARS {
            Rejection::Oversized { chars }
        } else {
            hydrate_leaf_citations(&v, ctx, retriever).await?;
            match v.clone().validate(ctx, *source) {
                Ok(validated) => {
                    let outcome =
                        Outcome::Answered { verdict: v, ctx: Box::new(std::mem::take(ctx)), source: *source, call: None };
                    self.stage = Stage::Closed(outcome);
                    return Ok(validated);
                }
                Err(JudgeError::BadCitation(c)) => Rejection::BadCitation(c),
                Err(JudgeError::MalformedCitation(m)) => Rejection::Malformed(m),
                Err(JudgeError::EmptyVerdict(e)) => Rejection::Empty(e),
                Err(other) => return Err(other.into()),
            }
        };
        *attempts += 1;
        if *attempts >= MAX_ATTEMPTS {
            self.stage = Stage::Closed(Outcome::Failed { rejection: rejection.clone() });
            return Err(SessionError::Exhausted { rejection });
        }
        // As in `judge()`: the retry sees the rejection and the tool-round
        // chunks pinned past the budget, and may not call the tool again.
        *lookup_used = true;
        *rejected = Some(rejection.clone());
        let retry = synthesis_prompt(&self.question, ctx, Some(&rejection), false, harness, budget);
        Err(SessionError::Rejected { rejection, retry })
    }

    /// The synthesis prompt as it currently stands (to re-read it, or after
    /// a lookup round).
    ///
    /// # Errors
    /// `WrongStage` unless the session is awaiting a verdict.
    pub fn synthesis_prompt(&self, harness: Harness, budget: &Budget) -> Result<SynthesisPrompt, SessionError> {
        let Stage::AwaitingVerdict { ctx, lookup_used, rejected, .. } = &self.stage else {
            return Err(self.wrong_stage("awaiting_verdict"));
        };
        Ok(synthesis_prompt(&self.question, ctx, rejected.as_ref(), !*lookup_used, harness, budget))
    }

    /// The accepted verdict, re-validated. `None` unless the session closed
    /// with an answer.
    #[must_use]
    pub fn accepted(&self) -> Option<Verdict<Validated>> {
        let Stage::Closed(Outcome::Answered { verdict, ctx, source, .. }) = &self.stage else {
            return None;
        };
        // Cannot fail: it is the verdict `validate` admitted against this
        // very context. If it somehow does, the session has no answer.
        verdict.clone().validate(ctx, *source).ok()
    }

    /// Step 6: store the accepted verdict as a call. Idempotent through the
    /// store ([`PersistCall`]), not only through the `call` field.
    ///
    /// # Errors
    /// `WrongStage` unless the session closed with an answer; `Pipeline` from
    /// the store.
    pub async fn persist(&mut self, store: &dyn PersistCall) -> Result<CallId, SessionError> {
        let Stage::Closed(Outcome::Answered { verdict, ctx, source, call }) = &mut self.stage else {
            return Err(self.wrong_stage("closed with an answer"));
        };
        if let Some(id) = call {
            return Ok(*id);
        }
        let validated = verdict
            .clone()
            .validate(ctx, *source)
            .map_err(|e| JudgeError::Upstream(anyhow::anyhow!("accepted verdict no longer validates: {e}")))?;
        let id = store.persist_call(self.id, &self.question, &validated, ctx).await?;
        *call = Some(id);
        Ok(id)
    }

    fn wrong_stage(&self, expected: &'static str) -> SessionError {
        SessionError::WrongStage { expected, actual: self.stage.name() }
    }
}

/// The size rules on an extraction. Each span is a resolver query and each
/// concept a search term, so their number and length are bounded.
fn check_extraction(e: &Extraction) -> Result<(), SessionError> {
    for (what, items) in [("card_spans", &e.card_spans), ("concepts", &e.concepts)] {
        if items.len() > MAX_EXTRACTION_ITEMS {
            return Err(SessionError::Invalid(format!("{what} holds at most {MAX_EXTRACTION_ITEMS} entries")));
        }
        if let Some(long) = items.iter().find(|s| s.chars().count() > MAX_EXTRACTION_ITEM_CHARS) {
            return Err(SessionError::Invalid(format!(
                "{what} entry {:?}… is over {MAX_EXTRACTION_ITEM_CHARS} characters",
                long.chars().take(40).collect::<String>()
            )));
        }
    }
    Ok(())
}

/// Render the synthesis prompt. Once the lookup round has run, its chunks
/// are pinned past the budget (they were appended last and would otherwise
/// be the first the budget drops); the Anthropic path needs that only on the
/// retry because the tool result rides in the conversation. Agents get a line
/// telling them the lookup round is gone; the Anthropic prompt says nothing
/// because the tool is simply absent from the request.
fn synthesis_prompt(
    q: &Question,
    ctx: &Context,
    rejected: Option<&Rejection>,
    lookup_available: bool,
    harness: Harness,
    budget: &Budget,
) -> SynthesisPrompt {
    let pinned: &[RuleId] = if lookup_available { &[] } else { &ctx.tool_round };
    let mut question = synth::render_question(q, ctx, rejected);
    if !lookup_available && harness != Harness::Tool {
        question.push_str("\n(The rules lookup has been used for this question; answer from the material shown.)\n");
    }
    SynthesisPrompt {
        system: synth::system_prompt(harness),
        material: synth::render_material(ctx, pinned, budget),
        question,
        schema: VERDICT_SCHEMA.clone(),
        lookup_available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_core::{
        Card, CardId, Category, CategoryGuess, Citation, Confidence, CrVersion, EmptyVerdict, Face, Layout,
        MalformedCitation, MatchedVia, PriorCall, Resolution, Ruling, ruling_key,
    };
    use nonempty::NonEmpty;
    use std::sync::Mutex;

    const BODY: &str = "Damage dealt by a source with lifelink causes its controller to gain that much life.";
    const LEAF: &str = "702.15b Damage dealt by a source with lifelink causes that source's controller to gain that much life.";

    fn rid(id: &str) -> anyhow::Result<RuleId> {
        Ok(RuleId::try_new(id.to_owned())?)
    }

    fn rule(id: &str) -> anyhow::Result<RuleChunk> {
        let parent = id.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        let leaf = parent != id;
        Ok(RuleChunk {
            id: rid(id)?,
            parent_id: if leaf { Some(rid(parent)?) } else { None },
            subsection: rid("702")?,
            heading: "Lifelink".into(),
            body: if leaf { LEAF.into() } else { format!("{BODY}\n{LEAF}") },
            examples: vec![],
            cr_version: CrVersion::try_new("20260819".to_owned())?,
        })
    }

    fn card(n: u128, name: &str) -> Card {
        Card {
            id: CardId::new(Uuid::from_u128(n)),
            name: name.into(),
            layout: Layout::Normal,
            faces: NonEmpty::singleton(Face {
                name: name.into(),
                oracle_text: "Lifelink".into(),
                mana_cost: "{W}".into(),
                type_line: "Creature".into(),
            }),
        }
    }

    struct Ports {
        lookups: Mutex<Vec<Vec<RuleId>>>,
        fail_lookup: bool,
    }

    #[async_trait]
    impl Resolver for Ports {
        async fn resolve(&self, span: &str) -> Result<Resolution, JudgeError> {
            Ok(match span {
                "urza" => Resolution::Ambiguous {
                    query: span.into(),
                    candidates: NonEmpty::from((card(1, "Urza, Lord High Artificer"), vec![card(2, "Urza's Saga")])),
                    via: MatchedVia::Fuzzy,
                },
                "[[Urza's Saga]]" => Resolution::Resolved { card: card(2, "Urza's Saga"), via: MatchedVia::Bracket },
                _ => Resolution::NotFound { query: span.into() },
            })
        }
    }

    #[async_trait]
    impl Retriever for Ports {
        async fn retrieve(&self, _q: &Question, cards: &[Card], _e: &Extraction) -> Result<Context, JudgeError> {
            // A populated context: every kind of material the store must round-trip.
            let text = "Urza's Saga's third chapter ability can find a card with mana value 0.";
            let ruling = Ruling {
                card: CardId::new(Uuid::from_u128(2)),
                key: ruling_key("2021-06-18", text),
                published_at: "2021-06-18".into(),
                text: text.into(),
            };
            let prior = PriorCall {
                id: CallId::new(Uuid::from_u128(9)),
                question: "does lifelink stack?".into(),
                answer: "No, multiple instances are redundant.".into(),
                category: Category::Combat,
                citations: vec![Citation::Rule { id: rid("702.15")?, quote: BODY.into() }],
                cr_version: CrVersion::try_new("20260819".to_owned()).map_err(anyhow::Error::from)?,
                rating: 2.5,
                rating_count: 3,
            };
            Ok(Context {
                cards: cards.to_vec(),
                rules: vec![rule("702.15")?],
                rulings: if cards.is_empty() { vec![] } else { vec![ruling] },
                glossary: vec![judge_core::GlossaryEntry { term: "Lifelink".into(), text: "A keyword ability.".into() }],
                prior: vec![prior],
                notes: cards.iter().map(|c| judge_core::CardNote { card: c.id, note: "tricky".into() }).collect(),
                ..Context::default()
            })
        }
        async fn lookup_rules(&self, ids: &[RuleId]) -> Result<Vec<RuleChunk>, JudgeError> {
            if self.fail_lookup {
                return Err(anyhow::anyhow!("db is down").into());
            }
            self.lookups.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(ids.to_vec());
            Ok(ids.iter().map(|id| rule(id.as_ref())).collect::<anyhow::Result<Vec<_>>>()?)
        }
    }

    struct Store(Mutex<Vec<SessionId>>);

    #[async_trait]
    impl PersistCall for Store {
        async fn persist_call(
            &self,
            session: SessionId,
            _q: &Question,
            _v: &Verdict<Validated>,
            _ctx: &Context,
        ) -> Result<CallId, JudgeError> {
            let mut seen = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let n = seen.iter().position(|s| *s == session).unwrap_or_else(|| {
                seen.push(session);
                seen.len() - 1
            });
            Ok(CallId::new(Uuid::from_u128(u128::try_from(n + 1).unwrap_or(1))))
        }
    }

    fn ports() -> Ports {
        Ports { lookups: Mutex::new(vec![]), fail_lookup: false }
    }

    fn extraction(spans: &[&str], source: Source) -> Extraction {
        Extraction {
            card_spans: spans.iter().map(|s| (*s).to_owned()).collect(),
            concepts: vec!["lifelink".into()],
            primary: CategoryGuess { category: Category::Combat, confidence: Confidence::High },
            secondary: vec![],
            source,
        }
    }

    fn session() -> anyhow::Result<Session> {
        Ok(Session::begin(
            SessionId::new(),
            AgentThread::new(),
            "does lifelink stack?".into(),
            vec![Qa { question: "earlier".into(), answer: "yes".into() }],
        )?)
    }

    fn verdict(quote: &str) -> anyhow::Result<Verdict<Unvalidated>> {
        Ok(Verdict::new(
            "No. Multiple instances of lifelink are redundant; the life gain happens once.".into(),
            Confidence::High,
            vec![Citation::Rule { id: rid("702.15")?, quote: quote.into() }],
            Category::Combat,
        ))
    }

    async fn ready(s: &mut Session, p: &Ports, spans: &[&str]) -> anyhow::Result<SynthesisPrompt> {
        match s.submit_extraction(extraction(spans, Source::Cr), p, p, Harness::Mcp, &Budget::default()).await? {
            Extracted::Ready(prompt) => Ok(prompt),
            Extracted::OutOfScope { .. } => Err(anyhow::anyhow!("unexpected")),
        }
    }

    #[test]
    fn agent_threads_are_minted_here_or_not_at_all() {
        let t = AgentThread::new();
        assert!(t.as_str().starts_with("agent:"));
        assert_eq!(t.as_str().parse::<AgentThread>().ok(), Some(t.clone()));
        for bad in ["1234567890", "web:0b6a0f4e-1c5b-4a2e-9d3e-7f4c1b2a3d4e", "agent:", "agent:nope", ""] {
            assert!(bad.parse::<AgentThread>().is_err(), "{bad:?} must not parse");
        }
        let j: Result<AgentThread, _> = serde_json::from_str("\"1234567890\"");
        assert!(j.is_err(), "deserialization is validated too");
    }

    #[test]
    fn begin_bounds_the_question() {
        let long = "x".repeat(MAX_QUESTION_CHARS + 1);
        assert!(matches!(Session::begin(SessionId::new(), AgentThread::new(), long, vec![]), Err(SessionError::Invalid(_))));
        assert!(matches!(Session::begin(SessionId::new(), AgentThread::new(), "  ".into(), vec![]), Err(SessionError::Invalid(_))));
    }

    #[test]
    fn extraction_prompt_carries_history_question_and_schema() -> anyhow::Result<()> {
        let s = session()?;
        let p = s.extraction_prompt(5)?;
        assert!(p.system.contains("Taxonomy"));
        assert!(p.user.contains("Q: earlier"));
        assert!(p.user.contains("does lifelink stack?"));
        assert_eq!(p.schema.pointer("/required").and_then(|r| r.as_array()).map(Vec::len), Some(4));
        assert!(VERDICT_SCHEMA.pointer("/properties/citations").is_some(), "verdict schema is real, not null");
        Ok(())
    }

    #[tokio::test]
    async fn the_happy_path_answers_and_persists_once() -> anyhow::Result<()> {
        let (ports, store) = (ports(), Store(Mutex::new(vec![])));
        let mut s = session()?;
        let prompt = ready(&mut s, &ports, &[]).await?;
        assert!(prompt.lookup_available);
        assert!(prompt.system.contains("call the `lookup_rules` tool ONCE, with"));
        assert!(prompt.system.contains("`submit_verdict`"));
        assert!(prompt.material.contains("[702.15]"));
        assert!(prompt.material.contains("Q: earlier"), "history rendered into the material");
        assert!(prompt.question.ends_with("does lifelink stack?\n"));
        let validated = s.submit_verdict(verdict(BODY)?, &ports, Harness::Mcp, &Budget::default()).await?;
        assert_eq!(validated.cr_version().as_ref(), "20260819");
        assert!(s.accepted().is_some());
        let first = s.persist(&store).await?;
        let second = s.persist(&store).await?;
        assert_eq!(first, second, "persist is idempotent");
        assert!(matches!(s.stage, Stage::Closed(Outcome::Answered { call: Some(_), .. })));
        Ok(())
    }

    #[tokio::test]
    async fn a_rejection_gets_one_retry_then_closes() -> anyhow::Result<()> {
        let p = ports();
        let mut s = session()?;
        ready(&mut s, &p, &[]).await?;
        let Err(SessionError::Rejected { retry, .. }) =
            s.submit_verdict(verdict("not in the rule")?, &p, Harness::Cli, &Budget::default()).await
        else {
            return Err(anyhow::anyhow!("expected a rejection"));
        };
        assert!(retry.question.contains("Previous attempt rejected"));
        assert!(!retry.lookup_available);
        assert!(retry.question.contains("lookup has been used"));
        let id = rid("702.19")?;
        assert!(
            matches!(s.lookup_rules(std::slice::from_ref(&id), &p).await, Err(SessionError::LookupUsed)),
            "retry forfeits the round"
        );
        let Err(SessionError::Exhausted { .. }) =
            s.submit_verdict(verdict("still wrong")?, &p, Harness::Cli, &Budget::default()).await
        else {
            return Err(anyhow::anyhow!("expected exhaustion"));
        };
        assert!(matches!(s.stage, Stage::Closed(Outcome::Failed { .. })));
        assert!(s.accepted().is_none());
        assert!(matches!(
            s.submit_verdict(verdict(BODY)?, &p, Harness::Cli, &Budget::default()).await,
            Err(SessionError::WrongStage { actual: "closed", .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn an_oversized_answer_is_a_rejection_before_validation() -> anyhow::Result<()> {
        let p = ports();
        let mut s = session()?;
        ready(&mut s, &p, &[]).await?;
        let big = Verdict::new(
            "x".repeat(MAX_ANSWER_CHARS + 1),
            Confidence::High,
            vec![Citation::Rule { id: rid("702.15")?, quote: BODY.into() }],
            Category::Combat,
        );
        let Err(SessionError::Rejected { rejection: Rejection::Oversized { chars }, retry }) =
            s.submit_verdict(big, &p, Harness::Mcp, &Budget::default()).await
        else {
            return Err(anyhow::anyhow!("expected an oversized rejection"));
        };
        assert_eq!(chars, MAX_ANSWER_CHARS + 1);
        assert!(retry.question.contains("characters long"));
        assert!(
            p.lookups.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty(),
            "no hydration for a rejected size"
        );
        Ok(())
    }

    #[tokio::test]
    async fn the_lookup_round_is_once_bounded_and_pinned_into_the_prompt() -> anyhow::Result<()> {
        let p = ports();
        let mut s = session()?;
        ready(&mut s, &p, &[]).await?;
        assert!(matches!(s.lookup_rules(&[], &p).await, Err(SessionError::Invalid(_))), "empty request");
        let many: Vec<RuleId> = (0..=MAX_LOOKUP_IDS).map(|i| rid(&format!("{}", 100 + i))).collect::<anyhow::Result<_>>()?;
        assert!(matches!(s.lookup_rules(&many, &p).await, Err(SessionError::Invalid(_))), "too many");
        assert!(s.synthesis_prompt(Harness::Mcp, &Budget::default())?.lookup_available, "neither spent the round");
        let id = rid("702.19")?;
        let chunks = s.lookup_rules(std::slice::from_ref(&id), &p).await?;
        assert_eq!(chunks.len(), 1);
        assert!(matches!(s.lookup_rules(std::slice::from_ref(&id), &p).await, Err(SessionError::LookupUsed)));
        // A budget with room for one chunk: the fetched chunk must still be rendered.
        let tight = Budget { max_rule_chunks: 1, ..Budget::default() };
        let prompt = s.synthesis_prompt(Harness::Mcp, &tight)?;
        assert!(!prompt.lookup_available);
        assert!(prompt.material.contains("[702.19]"), "fetched chunk is pinned past the budget");
        let v = Verdict::new(
            "Lifelink and the fetched rule together decide this question in favour of no.".into(),
            Confidence::Medium,
            vec![Citation::Rule { id, quote: BODY.into() }],
            Category::Combat,
        );
        s.submit_verdict(v, &p, Harness::Mcp, &Budget::default()).await?;
        Ok(())
    }

    #[tokio::test]
    async fn ambiguity_leaves_the_session_open_for_a_pinned_resubmission() -> anyhow::Result<()> {
        let p = ports();
        let mut s = session()?;
        let r = s.submit_extraction(extraction(&["urza"], Source::Cr), &p, &p, Harness::Mcp, &Budget::default()).await;
        assert!(matches!(r, Err(SessionError::Pipeline(JudgeError::AmbiguousCards(_)))));
        assert!(matches!(s.stage, Stage::AwaitingExtraction { .. }));
        let r = s.submit_extraction(extraction(&["nope"], Source::Cr), &p, &p, Harness::Mcp, &Budget::default()).await;
        assert!(matches!(r, Err(SessionError::Pipeline(JudgeError::CardsNotFound(_)))));
        let prompt = ready(&mut s, &p, &["[[Urza's Saga]]"]).await?;
        assert!(prompt.material.contains("Urza's Saga"));
        Ok(())
    }

    #[tokio::test]
    async fn an_oversized_extraction_is_refused_without_resolving() -> anyhow::Result<()> {
        let p = ports();
        let mut s = session()?;
        let mut e = extraction(&[], Source::Cr);
        e.concepts = (0..=MAX_EXTRACTION_ITEMS).map(|i| i.to_string()).collect();
        assert!(matches!(
            s.submit_extraction(e, &p, &p, Harness::Mcp, &Budget::default()).await,
            Err(SessionError::Invalid(_))
        ));
        let mut e = extraction(&[], Source::Cr);
        e.card_spans = vec!["x".repeat(MAX_EXTRACTION_ITEM_CHARS + 1)];
        assert!(matches!(
            s.submit_extraction(e, &p, &p, Harness::Mcp, &Budget::default()).await,
            Err(SessionError::Invalid(_))
        ));
        assert!(matches!(s.stage, Stage::AwaitingExtraction { .. }));
        Ok(())
    }

    #[tokio::test]
    async fn out_of_scope_closes_the_session() -> anyhow::Result<()> {
        let p = ports();
        let mut s = session()?;
        let r = s.submit_extraction(extraction(&[], Source::Tournament), &p, &p, Harness::Cli, &Budget::default()).await?;
        assert_eq!(r, Extracted::OutOfScope { source: Source::Tournament });
        assert!(matches!(s.stage, Stage::Closed(Outcome::OutOfScope { source: Source::Tournament })));
        assert!(matches!(s.extraction_prompt(5), Err(SessionError::WrongStage { .. })));
        Ok(())
    }

    #[tokio::test]
    async fn wrong_stage_transitions_are_refused_without_side_effects() -> anyhow::Result<()> {
        let p = ports();
        let mut s = session()?;
        assert!(matches!(
            s.submit_verdict(verdict(BODY)?, &p, Harness::Mcp, &Budget::default()).await,
            Err(SessionError::WrongStage { expected: "awaiting_verdict", actual: "awaiting_extraction" })
        ));
        assert!(matches!(s.lookup_rules(&[], &p).await, Err(SessionError::WrongStage { .. })));
        assert!(matches!(s.persist(&Store(Mutex::new(vec![]))).await, Err(SessionError::WrongStage { .. })));
        assert!(p.lookups.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty());
        Ok(())
    }

    /// A store failure during sub-rule hydration is not a rejection: no
    /// attempt is spent and the stage is exactly as before.
    #[tokio::test]
    async fn a_store_failure_in_validation_leaves_the_stage_untouched() -> anyhow::Result<()> {
        let mut p = ports();
        let mut s = session()?;
        ready(&mut s, &p, &[]).await?;
        let before = s.clone();
        p.fail_lookup = true;
        let leaf = Verdict::new(
            "No. Multiple instances of lifelink are redundant; the life gain happens once.".into(),
            Confidence::High,
            vec![Citation::Rule { id: rid("702.15b")?, quote: "gain that much life".into() }],
            Category::Combat,
        );
        assert!(matches!(
            s.submit_verdict(leaf, &p, Harness::Mcp, &Budget::default()).await,
            Err(SessionError::Pipeline(JudgeError::Upstream(_)))
        ));
        assert_eq!(s, before);
        Ok(())
    }

    /// The whole machine round-trips through JSON, which is how the store
    /// holds it — with a populated context, and at every stage including a
    /// leaf citation hydrated during validation.
    #[tokio::test]
    async fn the_session_survives_serialization_at_every_stage() -> anyhow::Result<()> {
        let p = ports();
        let mut s = session()?;
        let roundtrip = |s: &Session| -> anyhow::Result<Session> {
            let j = serde_json::to_string(s)?;
            Ok(serde_json::from_str(&j)?)
        };
        assert_eq!(roundtrip(&s)?, s);
        ready(&mut s, &p, &["[[Urza's Saga]]"]).await?;
        assert_eq!(roundtrip(&s)?, s, "with cards, rulings, glossary, prior calls and notes");
        let _ = s.submit_verdict(verdict("wrong")?, &p, Harness::Mcp, &Budget::default()).await;
        assert_eq!(roundtrip(&s)?, s, "with a pending BadCitation rejection");
        // A leaf citation: the parent 702.15 is in the context, 702.15b is hydrated.
        let leaf = Verdict::new(
            "No. Multiple instances of lifelink are redundant; the life gain happens once.".into(),
            Confidence::High,
            vec![Citation::Rule { id: rid("702.15b")?, quote: "that source's controller to gain".into() }],
            Category::Combat,
        );
        s.submit_verdict(leaf, &p, Harness::Mcp, &Budget::default()).await?;
        let back = roundtrip(&s)?;
        assert_eq!(back, s);
        assert!(back.accepted().is_some(), "the hydrated context is what was stored, so the leaf still validates");

        // The other rejection kinds, and a failed close, round-trip too.
        for rejection in [
            Rejection::Malformed(MalformedCitation::new(r#"{"id":""}"#, "bad RuleId")),
            Rejection::Empty(EmptyVerdict::ShortAnswer { chars: 3 }),
            Rejection::Empty(EmptyVerdict::NoCitations),
            Rejection::Oversized { chars: 9000 },
        ] {
            let closed = Session { stage: Stage::Closed(Outcome::Failed { rejection }), ..s.clone() };
            assert_eq!(roundtrip(&closed)?, closed);
        }
        Ok(())
    }
}
