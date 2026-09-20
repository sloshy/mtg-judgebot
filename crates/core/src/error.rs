//! `JudgeError`: every way the pipeline can fail.

use nonempty::NonEmpty;

use crate::{Ambiguous, Citation, EmptyVerdict, MalformedCitation, Source, UncitedRules};

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
    /// A citation could not be parsed into a `Citation` at all (an empty rule
    /// id, an unknown `kind`). Kept distinct from `Upstream` precisely so
    /// `judge()` retries it: it is the model misspeaking, not a broken adapter.
    #[error("{0}")]
    MalformedCitation(MalformedCitation),
    /// The verdict had no citations (for a CR / Commander answer) or no real
    /// answer text. `judge()` retries synthesis once, exactly as for `BadCitation`.
    #[error("empty verdict: {0}")]
    EmptyVerdict(EmptyVerdict),
    /// The answer's prose names a rule number that none of its citations
    /// quotes. `judge()` retries synthesis once, as for `BadCitation`.
    #[error("uncited rule: {0}")]
    UncitedRules(UncitedRules),
    /// The model declined to answer (`stop_reason == "refusal"`).
    #[error("the model refused to answer")]
    LlmRefused,
    /// Anything from an adapter: HTTP, DB, parsing.
    #[error(transparent)]
    Upstream(#[from] anyhow::Error),
}

impl JudgeError {
    /// Whether this is a failure an *operator* needs the cause of, as opposed
    /// to an ordinary reply the asker can act on: an ambiguous card gets a
    /// "did you mean?", an unknown card gets a spelling hint, an out-of-scope
    /// question gets told so. Those three are the pipeline working.
    ///
    /// It lives here, beside the enum, so both front doors agree on what a
    /// failure is; a new variant must be classified once, and exhaustively.
    #[must_use]
    pub const fn is_operator_failure(&self) -> bool {
        match self {
            JudgeError::AmbiguousCards(_)
            | JudgeError::CardsNotFound(_)
            | JudgeError::OutOfScope(_) => false,
            JudgeError::BadCitation(_)
            | JudgeError::MalformedCitation(_)
            | JudgeError::EmptyVerdict(_)
            | JudgeError::UncitedRules(_)
            | JudgeError::LlmRefused
            | JudgeError::Upstream(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Card, CardId, Category, Citation, Confidence, Face, Layout, MalformedCitation, RuleId,
        Verdict,
    };
    use nonempty::NonEmpty;
    use uuid::Uuid;

    fn card(name: &str) -> Card {
        Card {
            id: CardId::new(Uuid::from_u128(1)),
            name: name.into(),
            layout: Layout::Normal,
            faces: NonEmpty::singleton(Face {
                name: name.into(),
                oracle_text: String::new(),
                mana_cost: String::new(),
                type_line: "Instant".into(),
            }),
        }
    }

    /// Exhaustiveness is the compiler's job; this pins the *choice*, which is
    /// what decides whether an operator ever sees a cause in the logs.
    #[test]
    fn only_pipeline_failures_are_operator_failures() -> Result<(), Box<dyn std::error::Error>> {
        let quiet = [
            JudgeError::AmbiguousCards(NonEmpty::singleton(Ambiguous {
                query: "bob".into(),
                candidates: NonEmpty::singleton(card("Dark Confidant")),
            })),
            JudgeError::CardsNotFound(NonEmpty::singleton("Blak Lotus".to_owned())),
            JudgeError::OutOfScope(Source::Tournament),
            JudgeError::OutOfScope(Source::OutOfScope),
        ];
        for e in &quiet {
            assert!(!e.is_operator_failure(), "{e}");
        }
        let loud = [
            JudgeError::BadCitation(Citation::Rule {
                id: RuleId::try_new("702.15b".to_owned())?,
                quote: crate::Quote::try_new("x")?,
            }),
            JudgeError::MalformedCitation(MalformedCitation::new(r#"{"id":""}"#, "bad RuleId")),
            JudgeError::EmptyVerdict(EmptyVerdict::NoCitations),
            JudgeError::LlmRefused,
            JudgeError::Upstream(anyhow::anyhow!("db is on fire")),
        ];
        for e in &loud {
            assert!(e.is_operator_failure(), "{e}");
        }
        Ok(())
    }

    /// A verdict that fails to validate is loud: it means a synthesis attempt
    /// and its retry were both spent without producing an answer.
    #[test]
    fn a_failed_validation_is_an_operator_failure() -> Result<(), Box<dyn std::error::Error>> {
        let v = Verdict::new("short".into(), Confidence::Low, vec![], Category::Layers);
        let Err(e) = v.validate(&crate::Context::default(), crate::AnswerableSource::Cr) else {
            return Err("a citation-less short answer must not validate".into());
        };
        assert!(e.is_operator_failure(), "{e}");
        Ok(())
    }
}
