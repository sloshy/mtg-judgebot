//! The operations, with their typed inputs and replies. Every MCP tool and
//! every `judge-cli` subcommand is one of these; the transports only parse
//! and print.
//!
//! Replies are data even when the news is bad: an ambiguous card, a rejected
//! verdict and a closed session are all *replies* the agent can act on, and
//! only a broken adapter is an error.

use std::time::Instant;

use judge_bot::{
    discord::{question::pin_card, render},
    session::{AgentThread, Begun, Extracted, Prompt, SessionError, SessionId, Stage, SynthesisPrompt},
};
use judge_core::{
    Ambiguous, CallId, Card, CardId, CardNote, Citation, Confidence, Context, Extraction, GlossaryEntry, JudgeError,
    Question, Rejection, Resolution, RuleChunk, RuleId, Ruling, Source, Unvalidated, Validated, Verdict, judge,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ACQUIRE_WAIT, Toolbox};

/// Candidate names listed for an ambiguous span.
pub const MAX_CHOICES: usize = 10;
/// Most rule chunks one search returns.
pub const MAX_SEARCH: usize = judge_bot::db::MAX_SEARCH;
/// Most pins one `judge` call takes (as the web API).
pub const MAX_PINS: usize = 8;
/// Longest card name, glossary term, pin span or pin name, in characters:
/// every one of these is a query the database runs.
pub const MAX_NAME_CHARS: usize = judge_bot::session::MAX_EXTRACTION_ITEM_CHARS;
/// Longest search query, in characters (it is embedded when Voyage is on).
pub const MAX_QUERY_CHARS: usize = judge_bot::session::MAX_QUESTION_CHARS;
/// Most rule ids one `get_rules` call takes (as a session's lookup round).
pub const MAX_RULE_IDS: usize = judge_bot::session::MAX_LOOKUP_IDS;

/// A "did you mean…?" answered up front: the span as it appears in the
/// question and the full card name meant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Pin {
    /// The span exactly as written in the question.
    pub span: String,
    /// The full Oracle name chosen.
    pub name: String,
}

/// Input of `judge` (the built-in pipeline).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JudgeInput {
    /// The rules question. Write a card as `[[Full Name]]` to pin it.
    pub question: String,
    /// A thread returned by an earlier reply, for follow-up history; omit
    /// for a fresh thread. Only `agent:<uuid>` ids are accepted: an agent
    /// can never name a Discord or web thread.
    #[serde(default)]
    pub thread: Option<AgentThread>,
    /// Ambiguities resolved from an earlier `ambiguous` reply.
    #[serde(default)]
    pub pins: Vec<Pin>,
}

/// One ambiguous span and what it could mean.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AmbiguousSpan {
    /// The span as written.
    pub query: String,
    /// Candidate full names, at most [`MAX_CHOICES`].
    pub choices: Vec<String>,
    /// Whether more candidates existed.
    pub truncated: bool,
}

/// One citation of an answer, with where to read it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CitationView {
    /// `702.19b`, `Ruling (2018-07-13) — Blood Moon`, …
    pub label: String,
    /// A public page for the source, when there is one.
    pub url: Option<String>,
    /// The typed citation as validated.
    pub citation: Citation,
}

/// A validated answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Answer {
    /// The answer text.
    pub answer: String,
    /// Model-reported confidence.
    pub confidence: Confidence,
    /// Rules body the answer draws on.
    pub source: Source,
    /// CR effective date the answer was validated against, `YYYYMMDD`.
    pub cr_version: String,
    /// Category id.
    pub category: String,
    /// Citations in the model's order, each checked against the material.
    pub citations: Vec<CitationView>,
}

/// Reply of `judge`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JudgeReply {
    /// Answered, and stored as a call.
    Answer {
        /// The answer.
        #[serde(flatten)]
        answer: Answer,
        /// The stored call.
        call: Option<CallId>,
        /// The thread the call was filed under (pass it back for follow-ups).
        thread: AgentThread,
    },
    /// Card spans that matched several cards; ask again with `pins`.
    Ambiguous {
        /// One entry per ambiguous span.
        spans: Vec<AmbiguousSpan>,
    },
    /// Card spans that matched nothing.
    NotFound {
        /// The spans as written.
        names: Vec<String>,
    },
    /// Tournament policy or not a rules question.
    OutOfScope {
        /// The fixed reply.
        message: String,
    },
    /// Every pipeline slot is taken.
    Busy {
        /// Explanation.
        message: String,
    },
    /// This toolbox's `judge` quota for the current window is used up.
    RateLimited {
        /// Explanation.
        message: String,
    },
    /// The built-in pipeline is not configured here; use a session.
    Unavailable {
        /// Explanation.
        message: String,
    },
    /// The pipeline failed to produce a verified answer.
    Error {
        /// Explanation.
        message: String,
    },
}

/// Input of `begin_session`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BeginInput {
    /// The rules question. Write a card as `[[Full Name]]` to pin it.
    pub question: String,
    /// A thread returned by an earlier reply, for follow-up history; omit
    /// for a fresh thread. Only `agent:<uuid>` ids are accepted: an agent
    /// can never name a Discord or web thread.
    #[serde(default)]
    pub thread: Option<AgentThread>,
}

/// A session id on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionInput {
    /// The session.
    pub session: SessionId,
}

/// Input of `submit_extraction`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExtractionInput {
    /// The session.
    pub session: SessionId,
    /// The extraction, matching the schema the session gave.
    pub extraction: Extraction,
}

/// Reply of `submit_extraction`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtractionReply {
    /// Cards resolved and material retrieved: synthesize.
    Ready {
        /// The synthesis prompt.
        #[serde(flatten)]
        prompt: SynthesisPrompt,
    },
    /// A span matched several cards. The session is unchanged: resubmit the
    /// extraction with the span written as `[[Full Name]]`.
    Ambiguous {
        /// One entry per ambiguous span.
        spans: Vec<AmbiguousSpan>,
    },
    /// A span matched nothing. The session is unchanged: fix the span or drop it.
    NotFound {
        /// The spans as written.
        names: Vec<String>,
    },
    /// Not a rules question; the session is closed.
    OutOfScope {
        /// The classification.
        source: Source,
        /// The reply to give the asker.
        message: String,
    },
}

/// Input of `lookup_rules`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LookupInput {
    /// The session.
    pub session: SessionId,
    /// Rule ids (`702.19`, `613.7b`) or whole subsections (`613`).
    pub ids: Vec<RuleId>,
}

/// Rule chunks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Rules {
    /// The chunks, in id order.
    pub rules: Vec<RuleChunk>,
}

/// Input of `submit_verdict`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VerdictInput {
    /// The session.
    pub session: SessionId,
    /// The verdict, matching the schema the session gave.
    pub verdict: Verdict<Unvalidated>,
    /// Store the call once accepted (feeds thread history and the prior-call
    /// examples shown to later questions).
    #[serde(default)]
    pub persist: bool,
}

/// Reply of `submit_verdict`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerdictReply {
    /// Validated; the session is closed.
    Accepted {
        /// The answer.
        #[serde(flatten)]
        answer: Answer,
        /// The stored call, if `persist` was set and storing succeeded.
        call: Option<CallId>,
        /// Why storing failed, if `persist` was set and it did; the verdict
        /// stands and `persist_session` can be retried.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        persist_error: Option<String>,
    },
    /// Rejected; one retry remains.
    Rejected {
        /// Why, in one line.
        reason: String,
        /// The typed rejection.
        rejection: Rejection,
        /// The synthesis prompt again with the rejection notice; answer it.
        retry: SynthesisPrompt,
    },
    /// Rejected again; the session is closed.
    Exhausted {
        /// Why, in one line.
        reason: String,
        /// The typed rejection.
        rejection: Rejection,
    },
}

/// Reply of `persist_session`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Persisted {
    /// The stored call.
    pub call: CallId,
}

/// Reply of `session_status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionStatus {
    /// The session.
    pub session: SessionId,
    /// Its thread.
    pub thread: String,
    /// The question.
    pub question: String,
    /// `awaiting_extraction`, `awaiting_verdict` or `closed`.
    pub stage: String,
    /// Whether the `lookup_rules` round is still available.
    pub lookup_available: bool,
    /// Verdicts submitted so far while open; `None` once closed.
    pub attempts: Option<u8>,
    /// How it ended, if closed: `answered`, `out_of_scope`, `failed`.
    pub outcome: Option<String>,
}

/// A card name or nickname to resolve.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NameInput {
    /// As a player would write it: full name, nickname, or `[[Full Name]]`.
    pub name: String,
}

/// A card by oracle id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CardInput {
    /// The oracle id (from `resolve_card`).
    pub card: CardId,
}

/// Everything stored about one card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CardInfo {
    /// The card, or `None` if the id is unknown.
    pub card: Option<Card>,
    /// Scryfall rulings, newest first.
    pub rulings: Vec<Ruling>,
    /// Hand-written notes for tricky cards.
    pub notes: Vec<CardNote>,
}

/// Rule ids to fetch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IdsInput {
    /// Rule ids (`702.19`, `613.7b`) or whole subsections (`613`); at most
    /// [`MAX_RULE_IDS`].
    pub ids: Vec<RuleId>,
}

/// A free-text search over the CR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchInput {
    /// Rules vocabulary works best: "lifelink", "state-based actions", "layer 7b".
    pub query: String,
    /// At most this many chunks (default 10, max [`MAX_SEARCH`]).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// A glossary term.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TermInput {
    /// The term, or part of it.
    pub term: String,
}

/// Glossary entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Glossary {
    /// Exact matches first.
    pub entries: Vec<GlossaryEntry>,
}

/// An operation failed for a reason the caller cannot act on beyond retrying.
pub type OpError = anyhow::Error;

impl Toolbox {
    /// The built-in pipeline on `input`.
    ///
    /// # Errors
    /// Only the call store failing to load history is swallowed (logged);
    /// pipeline failures are replies.
    pub async fn judge(&self, input: JudgeInput) -> Result<JudgeReply, OpError> {
        let Some(p) = &self.pipeline else {
            return Ok(JudgeReply::Unavailable {
                message: "the built-in pipeline needs a model on the server (ANTHROPIC_API_KEY, or a judge.toml); use begin_session and do the model work yourself".into(),
            });
        };
        check_question(&input.question)?;
        anyhow::ensure!(input.pins.len() <= MAX_PINS, "at most {MAX_PINS} pins are accepted");
        for p in &input.pins {
            check_short(&p.span, "pin span")?;
            check_short(&p.name, "pin name")?;
        }
        let Ok(Ok(_permit)) = tokio::time::timeout(ACQUIRE_WAIT, self.permits.acquire()).await else {
            return Ok(JudgeReply::Busy { message: render::BUSY.to_owned() });
        };
        // Counted only once a slot is held: the quota is in pipeline runs, and
        // a `busy` reply is not one.
        if !self.allow_judge() {
            return Ok(JudgeReply::RateLimited {
                message: "this client's judge quota for the current window is used up; use a session, or wait".into(),
            });
        }
        let thread = input.thread.unwrap_or_default();
        let text = input.pins.iter().fold(input.question, |t, pin| pin_card(&t, &pin.span, &pin.name));
        let q = Question { thread_id: thread.as_str().to_owned(), text };
        let history = self.history(&q.thread_id).await;
        let (t0, usd0, calls0) = (Instant::now(), p.meter.spent_usd(), p.meter.calls());
        let result = judge(&p.deps, &q, &history).await;
        let captured = p.capture.take(&q);
        tracing::info!(
            thread = %q.thread_id,
            elapsed_ms = t0.elapsed().as_millis(),
            usd = format_args!("{:.4}", p.meter.spent_usd() - usd0),
            llm_calls = p.meter.calls() - calls0,
            ok = result.is_ok(),
            "judge"
        );
        Ok(match result {
            Ok(v) => {
                let call = match &captured {
                    Some(ctx) => match self.calls.persist(&q, &v, ctx).await {
                        Ok(id) => Some(id),
                        Err(e) => {
                            tracing::error!(error = format_args!("{e:#}"), "persist failed; answering anyway");
                            None
                        }
                    },
                    None => None,
                };
                JudgeReply::Answer { answer: answer(&v, captured.as_ref()), call, thread }
            }
            Err(e) => {
                if e.is_operator_failure() {
                    tracing::warn!(thread = %q.thread_id, error = format_args!("{e:#}"), "judge failed");
                }
                judge_error(&e)
            }
        })
    }

    /// Start a session: the extraction prompt comes back with the id.
    ///
    /// # Errors
    /// The store.
    pub async fn begin_session(&self, input: BeginInput) -> Result<Begun, OpError> {
        Ok(self.sessions.begin(input.thread.unwrap_or_default(), input.question).await?)
    }

    /// The prompt for the session's current step.
    ///
    /// # Errors
    /// Unknown session, or closed.
    pub async fn session_prompt(&self, input: SessionInput) -> Result<Prompt, OpError> {
        Ok(self.sessions.prompt(input.session).await?)
    }

    /// Where a session is.
    ///
    /// # Errors
    /// Unknown session.
    pub async fn session_status(&self, input: SessionInput) -> Result<SessionStatus, OpError> {
        let s = self.sessions.load(input.session).await?;
        let (lookup_available, attempts, outcome) = match &s.stage {
            Stage::AwaitingExtraction { .. } => (true, Some(0), None),
            Stage::AwaitingVerdict { lookup_used, attempts, .. } => (!lookup_used, Some(*attempts), None),
            Stage::Closed(o) => (
                false,
                None,
                Some(
                    match o {
                        judge_bot::session::Outcome::Answered { .. } => "answered",
                        judge_bot::session::Outcome::OutOfScope { .. } => "out_of_scope",
                        judge_bot::session::Outcome::Failed { .. } => "failed",
                    }
                    .to_owned(),
                ),
            ),
        };
        Ok(SessionStatus {
            session: s.id,
            thread: s.question.thread_id,
            question: s.question.text,
            stage: s.stage.name().to_owned(),
            lookup_available,
            attempts,
            outcome,
        })
    }

    /// Hand in the extraction; get the synthesis prompt.
    ///
    /// # Errors
    /// Unknown session, wrong stage, or the store.
    pub async fn submit_extraction(&self, input: ExtractionInput) -> Result<ExtractionReply, OpError> {
        match self.sessions.submit_extraction(input.session, input.extraction).await {
            Ok(Extracted::Ready(prompt)) => Ok(ExtractionReply::Ready { prompt }),
            Ok(Extracted::OutOfScope { source }) => {
                Ok(ExtractionReply::OutOfScope { source, message: render::OUT_OF_SCOPE.to_owned() })
            }
            Err(SessionError::Pipeline(JudgeError::AmbiguousCards(spans))) => {
                Ok(ExtractionReply::Ambiguous { spans: spans.iter().map(ambiguous_span).collect() })
            }
            Err(SessionError::Pipeline(JudgeError::CardsNotFound(names))) => {
                Ok(ExtractionReply::NotFound { names: names.into_iter().collect() })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The session's one `lookup_rules` round.
    ///
    /// # Errors
    /// Unknown session, wrong stage, round already used, or the store.
    pub async fn lookup_rules(&self, input: LookupInput) -> Result<Rules, OpError> {
        Ok(Rules { rules: self.sessions.lookup_rules(input.session, &input.ids).await? })
    }

    /// Hand in the verdict; it is validated against the session's material.
    ///
    /// # Errors
    /// Unknown session, wrong stage, or the store. A rejection is a reply.
    pub async fn submit_verdict(&self, input: VerdictInput) -> Result<VerdictReply, OpError> {
        match self.sessions.submit_verdict(input.session, input.verdict).await {
            Ok(v) => {
                // The verdict is accepted and saved whatever happens next:
                // a failed persist or reload is reported, not turned into an
                // error the agent would retry into `WrongStage`.
                let (call, persist_error) = if input.persist {
                    match self.sessions.persist(input.session).await {
                        Ok(id) => (Some(id), None),
                        Err(e) => (None, Some(format!("{e:#}"))),
                    }
                } else {
                    (None, None)
                };
                // The context is in the closed session; use it for citation labels.
                let ctx = match self.sessions.load(input.session).await {
                    Ok(s) => match s.stage {
                        Stage::Closed(judge_bot::session::Outcome::Answered { ctx, .. }) => Some(*ctx),
                        _ => None,
                    },
                    Err(e) => {
                        tracing::warn!(error = format_args!("{e:#}"), "accepted verdict; could not reload its context for labels");
                        None
                    }
                };
                Ok(VerdictReply::Accepted { answer: answer(&v, ctx.as_ref()), call, persist_error })
            }
            Err(SessionError::Rejected { rejection, retry }) => {
                Ok(VerdictReply::Rejected { reason: rejection.to_string(), rejection, retry })
            }
            Err(SessionError::Exhausted { rejection }) => {
                Ok(VerdictReply::Exhausted { reason: rejection.to_string(), rejection })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Store an accepted verdict as a call.
    ///
    /// # Errors
    /// Unknown session, not answered, or the store.
    pub async fn persist_session(&self, input: SessionInput) -> Result<Persisted, OpError> {
        Ok(Persisted { call: self.sessions.persist(input.session).await? })
    }

    /// Resolve a card name the way the pipeline does. Never guesses.
    ///
    /// # Errors
    /// The store.
    pub async fn resolve_card(&self, input: NameInput) -> Result<Resolution, OpError> {
        check_short(&input.name, "name")?;
        Ok(self.resolver.resolve(input.name.trim()).await?)
    }

    /// A card with its rulings and notes.
    ///
    /// # Errors
    /// The store.
    pub async fn card_info(&self, input: CardInput) -> Result<CardInfo, OpError> {
        let (card, rulings, notes) = tokio::try_join!(
            self.library.card(input.card),
            self.library.rulings(input.card),
            self.library.notes(input.card)
        )?;
        Ok(CardInfo { card, rulings, notes })
    }

    /// CR chunks by id.
    ///
    /// # Errors
    /// The store.
    pub async fn get_rules(&self, input: IdsInput) -> Result<Rules, OpError> {
        anyhow::ensure!(!input.ids.is_empty(), "get_rules needs at least one rule id");
        anyhow::ensure!(input.ids.len() <= MAX_RULE_IDS, "get_rules takes at most {MAX_RULE_IDS} ids per call");
        Ok(Rules { rules: self.retriever.lookup_rules(&input.ids).await? })
    }

    /// Search the CR.
    ///
    /// # Errors
    /// The store.
    pub async fn search_rules(&self, input: SearchInput) -> Result<Rules, OpError> {
        let query = input.query.trim();
        anyhow::ensure!(!query.is_empty(), "query must not be empty");
        anyhow::ensure!(query.chars().count() <= MAX_QUERY_CHARS, "query must be at most {MAX_QUERY_CHARS} characters");
        let limit = input.limit.unwrap_or(10).clamp(1, MAX_SEARCH);
        Ok(Rules { rules: self.library.search_rules(query, limit).await? })
    }

    /// Glossary lookup.
    ///
    /// # Errors
    /// The store.
    pub async fn glossary(&self, input: TermInput) -> Result<Glossary, OpError> {
        check_short(&input.term, "term")?;
        Ok(Glossary { entries: self.library.glossary(&input.term).await? })
    }

    async fn history(&self, thread: &str) -> Vec<judge_core::Qa> {
        match self.calls.history(thread, self.history_len).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = format_args!("{e:#}"), "thread history unavailable; judging without it");
                vec![]
            }
        }
    }
}

fn check_question(q: &str) -> Result<(), OpError> {
    anyhow::ensure!(!q.trim().is_empty(), "question must not be empty");
    anyhow::ensure!(
        q.chars().count() <= judge_bot::session::MAX_QUESTION_CHARS,
        "question must be at most {} characters",
        judge_bot::session::MAX_QUESTION_CHARS
    );
    Ok(())
}

/// A name-sized input: non-empty and at most [`MAX_NAME_CHARS`].
fn check_short(s: &str, what: &str) -> Result<(), OpError> {
    anyhow::ensure!(!s.trim().is_empty(), "{what} must not be empty");
    anyhow::ensure!(s.chars().count() <= MAX_NAME_CHARS, "{what} must be at most {MAX_NAME_CHARS} characters");
    Ok(())
}

fn ambiguous_span(a: &Ambiguous) -> AmbiguousSpan {
    AmbiguousSpan {
        query: a.query.clone(),
        choices: a.candidates.iter().map(|c| c.name.clone()).take(MAX_CHOICES).collect(),
        truncated: a.candidates.len() > MAX_CHOICES,
    }
}

/// Shape a failed `judge()`. Exhaustive over `JudgeError`.
fn judge_error(e: &JudgeError) -> JudgeReply {
    match e {
        JudgeError::AmbiguousCards(spans) => JudgeReply::Ambiguous { spans: spans.iter().map(ambiguous_span).collect() },
        JudgeError::CardsNotFound(names) => JudgeReply::NotFound { names: names.iter().cloned().collect() },
        JudgeError::OutOfScope(_) => JudgeReply::OutOfScope { message: render::OUT_OF_SCOPE.to_owned() },
        JudgeError::BadCitation(_)
        | JudgeError::MalformedCitation(_)
        | JudgeError::EmptyVerdict(_)
        | JudgeError::LlmRefused
        | JudgeError::Upstream(_) => JudgeReply::Error { message: render::error(e) },
    }
}

/// Shape a validated verdict; `ctx` supplies card names for labels.
#[must_use]
pub fn answer(v: &Verdict<Validated>, ctx: Option<&Context>) -> Answer {
    Answer {
        answer: v.answer().to_owned(),
        confidence: v.confidence(),
        source: v.source(),
        cr_version: v.cr_version().as_ref().to_owned(),
        category: v.category().id().to_owned(),
        citations: v.citations().iter().map(|c| citation_view(c, ctx)).collect(),
    }
}

fn citation_view(c: &Citation, ctx: Option<&Context>) -> CitationView {
    let card_name = |id| ctx.and_then(|x| x.card(id)).map(|card| card.name.clone());
    let (label, url) = match c {
        Citation::Rule { id, .. } => (id.to_string(), Some(render::rule_url(id))),
        Citation::ScryfallRuling { card, ruling, .. } => {
            let date = ctx.and_then(|x| x.ruling(*card, ruling)).map(|r| format!(" ({})", r.published_at)).unwrap_or_default();
            let label = match card_name(*card) {
                Some(name) => format!("Ruling{date} — {name}"),
                None => format!("Ruling{date} — card {card}"),
            };
            (label, Some(render::scryfall_url(*card)))
        }
        Citation::OracleText { card, face, .. } => {
            let label = match card_name(*card) {
                Some(name) => format!("Oracle text — {name} (face {face})"),
                None => format!("Oracle text — card {card} (face {face})"),
            };
            (label, Some(render::scryfall_url(*card)))
        }
        Citation::PriorCall { id, .. } => (format!("Prior call {id}"), None),
    };
    CitationView { label, url, citation: c.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_core::{Category, RuleId};
    use nonempty::NonEmpty;

    fn answer_fixture() -> anyhow::Result<Answer> {
        Ok(Answer {
            answer: "No.".into(),
            confidence: Confidence::High,
            source: Source::Cr,
            cr_version: "20260819".into(),
            category: Category::Combat.id().to_owned(),
            citations: vec![CitationView {
                label: "702.15b".into(),
                url: Some("https://example".into()),
                citation: Citation::Rule { id: RuleId::try_new("702.15b".to_owned()).map_err(anyhow::Error::from)?, quote: "q".into() },
            }],
        })
    }

    fn prompt_fixture() -> SynthesisPrompt {
        SynthesisPrompt {
            system: "s".into(),
            material: "m".into(),
            question: "q".into(),
            schema: serde_json::json!({"type": "object"}),
            lookup_available: true,
        }
    }

    fn roundtrip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(v: &T) -> anyhow::Result<()> {
        let j = serde_json::to_value(v)?;
        let back: T = serde_json::from_value(j.clone())?;
        anyhow::ensure!(&back == v, "round trip changed {v:?} into {back:?} via {j}");
        Ok(())
    }

    /// The `flatten`-inside-a-tagged-enum replies: `kind` must coexist with
    /// the flattened fields both ways.
    #[test]
    fn flattened_replies_round_trip_with_their_kind() -> anyhow::Result<()> {
        let thread = AgentThread::new();
        let j = JudgeReply::Answer { answer: answer_fixture()?, call: None, thread: thread.clone() };
        roundtrip(&j)?;
        assert_eq!(serde_json::to_value(&j)?.get("kind"), Some(&serde_json::json!("answer")));
        let v = VerdictReply::Accepted { answer: answer_fixture()?, call: None, persist_error: Some("db".into()) };
        roundtrip(&v)?;
        let v = VerdictReply::Rejected {
            reason: "bad".into(),
            rejection: Rejection::Empty(judge_core::EmptyVerdict::NoCitations),
            retry: prompt_fixture(),
        };
        roundtrip(&v)?;
        let e = ExtractionReply::Ready { prompt: prompt_fixture() };
        roundtrip(&e)?;
        let ej = serde_json::to_value(&e)?;
        assert_eq!(ej.get("kind"), Some(&serde_json::json!("ready")));
        assert_eq!(ej.get("lookup_available"), Some(&serde_json::json!(true)));
        Ok(())
    }

    #[test]
    fn ambiguous_spans_are_cut_at_max_choices() {
        let cards: Vec<Card> = (0..(MAX_CHOICES + 3))
            .map(|i| Card {
                id: CardId::new(uuid::Uuid::from_u128(u128::try_from(i + 1).unwrap_or(1))),
                name: format!("Card {i}"),
                layout: judge_core::Layout::Normal,
                faces: NonEmpty::singleton(judge_core::Face {
                    name: format!("Card {i}"),
                    oracle_text: String::new(),
                    mana_cost: String::new(),
                    type_line: String::new(),
                }),
            })
            .collect();
        let Some(candidates) = NonEmpty::from_vec(cards) else { return };
        let span = ambiguous_span(&Ambiguous { query: "card".into(), candidates });
        assert_eq!(span.choices.len(), MAX_CHOICES);
        assert!(span.truncated);
    }

    #[test]
    fn citation_labels_work_with_and_without_a_context() -> anyhow::Result<()> {
        let card = CardId::new(uuid::Uuid::from_u128(7));
        let c = Citation::OracleText { card, face: 0, quote: "x".into() };
        assert!(citation_view(&c, None).label.contains("card 00000000-0000-0000-0000-000000000007"));
        let ctx = Context {
            cards: vec![Card {
                id: card,
                name: "Blood Moon".into(),
                layout: judge_core::Layout::Normal,
                faces: NonEmpty::singleton(judge_core::Face {
                    name: "Blood Moon".into(),
                    oracle_text: "Nonbasic lands are Mountains.".into(),
                    mana_cost: "{2}{R}".into(),
                    type_line: "Enchantment".into(),
                }),
            }],
            ..Context::default()
        };
        assert_eq!(citation_view(&c, Some(&ctx)).label, "Oracle text — Blood Moon (face 0)");
        let r = Citation::Rule { id: RuleId::try_new("702.15b".to_owned()).map_err(anyhow::Error::from)?, quote: "x".into() };
        assert_eq!(citation_view(&r, None).label, "702.15b");
        Ok(())
    }

    #[test]
    fn short_inputs_are_bounded() {
        assert!(check_short("Blood Moon", "name").is_ok());
        assert!(check_short("  ", "name").is_err());
        assert!(check_short(&"x".repeat(MAX_NAME_CHARS + 1), "name").is_err());
    }
}
