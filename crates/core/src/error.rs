//! `JudgeError`: every way the pipeline can fail.

use nonempty::NonEmpty;

use crate::{Ambiguous, Citation, EmptyVerdict, Source};

/// Every way `judge()` can fail. Discord rendering matches on this exhaustively.
#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    /// One or more card spans matched several cards; ask "did you mean…?".
    #[error("ambiguous card reference(s): {}", .0.iter().map(|a| a.query.as_str()).collect::<Vec<_>>().join(", "))]
    AmbiguousCards(NonEmpty<Ambiguous>),
    /// One or more card spans matched nothing.
    #[error("unknown card(s): {}", .0.iter().map(String::as_str).collect::<Vec<_>>().join(", "))]
    CardsNotFound(NonEmpty<String>),
    /// Tournament policy or not a rules question.
    #[error("question is out of scope ({0:?})")]
    OutOfScope(Source),
    /// A citation referenced something not in Context, or quoted text that is not in it.
    /// `judge()` retries synthesis once before surfacing this.
    #[error("bad citation: {0}")]
    BadCitation(Citation),
    /// The verdict had no citations (for a CR / Commander answer) or no real
    /// answer text. `judge()` retries synthesis once, exactly as for `BadCitation`.
    #[error("empty verdict: {0}")]
    EmptyVerdict(EmptyVerdict),
    /// The model declined to answer (`stop_reason == "refusal"`).
    #[error("the model refused to answer")]
    LlmRefused,
    /// Anything from an adapter: HTTP, DB, parsing.
    #[error(transparent)]
    Upstream(#[from] anyhow::Error),
}
