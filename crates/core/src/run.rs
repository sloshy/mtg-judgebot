//! The record of one judge run (`judge_runs`): where it came from, how it
//! ended, what it spent and the call or failed call it left.
//!
//! Every built-in pipeline run is recorded, whatever its outcome or
//! audience, by [`crate::CallStore::record_run`]. The charges come from the
//! run's [`crate::Tally`].

use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{CallId, Charge, Interface, JudgeError, Tally, Validated, Verdict};

/// How a run ended. Stored as `judge_runs.outcome`, and the `outcome` field
/// of each interface's log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A validated verdict.
    Answered,
    /// A card span matched several cards ("did you mean?").
    Ambiguous,
    /// A card span matched nothing.
    NotFound,
    /// Tournament policy or not a rules question.
    OutOfScope,
    /// Both attempts cited something not in the material.
    BadCitation,
    /// Both attempts sent a citation that could not be read.
    MalformedCitation,
    /// Both attempts sent no citations or no answer.
    EmptyVerdict,
    /// Both attempts named a rule in the prose they did not cite.
    UncitedRules,
    /// Both attempts misused `lookup_rules`.
    ToolMisuse,
    /// The model declined to answer.
    Refused,
    /// The spend cap refused a model call.
    AtCap,
    /// Anything else from an adapter: HTTP, the database, parsing.
    Upstream,
}

impl Outcome {
    /// Every variant, in declaration order.
    pub const ALL: [Self; 12] = [
        Self::Answered,
        Self::Ambiguous,
        Self::NotFound,
        Self::OutOfScope,
        Self::BadCitation,
        Self::MalformedCitation,
        Self::EmptyVerdict,
        Self::UncitedRules,
        Self::ToolMisuse,
        Self::Refused,
        Self::AtCap,
        Self::Upstream,
    ];

    /// How `result` ended, `tally` telling a spend-cap refusal apart from
    /// another upstream failure.
    #[must_use]
    pub fn of(result: &Result<Verdict<Validated>, JudgeError>, tally: &Tally) -> Self {
        match result {
            Ok(_) => Self::Answered,
            Err(JudgeError::AmbiguousCards(_)) => Self::Ambiguous,
            Err(JudgeError::CardsNotFound(_)) => Self::NotFound,
            Err(JudgeError::OutOfScope(_)) => Self::OutOfScope,
            Err(JudgeError::BadCitation(_)) => Self::BadCitation,
            Err(JudgeError::MalformedCitation(_)) => Self::MalformedCitation,
            Err(JudgeError::EmptyVerdict(_)) => Self::EmptyVerdict,
            Err(JudgeError::UncitedRules(_)) => Self::UncitedRules,
            Err(JudgeError::ToolMisuse(_)) => Self::ToolMisuse,
            Err(JudgeError::LlmRefused) => Self::Refused,
            Err(JudgeError::Upstream(_)) if tally.was_capped() => Self::AtCap,
            Err(JudgeError::Upstream(_)) => Self::Upstream,
        }
    }

    /// The stored id, equal to the serde name.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Ambiguous => "ambiguous",
            Self::NotFound => "not_found",
            Self::OutOfScope => "out_of_scope",
            Self::BadCitation => "bad_citation",
            Self::MalformedCitation => "malformed_citation",
            Self::EmptyVerdict => "empty_verdict",
            Self::UncitedRules => "uncited_rules",
            Self::ToolMisuse => "tool_misuse",
            Self::Refused => "refused",
            Self::AtCap => "at_cap",
            Self::Upstream => "upstream",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// A `failed_calls` row's id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FailedCallId(pub i64);

impl fmt::Display for FailedCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What a run left behind: the stored call, the failed call, or neither (an
/// ambiguous, unknown-card or out-of-scope question, or a store failure).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Left {
    /// The answer, stored.
    Call(CallId),
    /// The failure, stored for the operator.
    Failure(FailedCallId),
    /// Nothing stored.
    Nothing,
}

/// One run, for `judge_runs`.
#[derive(Clone, Debug)]
pub struct RunRecord {
    /// The interface it came in through.
    pub interface: Interface,
    /// Whether the call was private.
    pub private: bool,
    /// How it ended.
    pub outcome: Outcome,
    /// How long it took, from the pipeline's start to the record (the
    /// store's clock gives the finish time).
    pub elapsed: Duration,
    /// Whether the first synthesis attempt was rejected, so the one retry was
    /// attempted (its request may still have failed: see the charges).
    pub retried: bool,
    /// What it spent, in billing order.
    pub charges: Vec<Charge>,
    /// The call or failed call it left.
    pub left: Left,
}

impl RunRecord {
    /// Whether the `lookup_rules` round ran (a billed request carried its result).
    #[must_use]
    pub fn tool_round(&self) -> bool {
        self.charges
            .iter()
            .any(|c| c.stage == crate::Stage::ToolRound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_ids_are_the_serde_names() -> Result<(), serde_json::Error> {
        for o in Outcome::ALL {
            assert_eq!(serde_json::to_value(o)?, serde_json::json!(o.id()));
        }
        Ok(())
    }

    #[test]
    fn an_upstream_failure_is_at_the_cap_only_when_the_cap_refused() {
        let tally = Tally::new();
        let failed = Err(JudgeError::Upstream(anyhow::anyhow!("x")));
        assert_eq!(Outcome::of(&failed, &tally), Outcome::Upstream);
        tally.cap_refused();
        assert_eq!(Outcome::of(&failed, &tally), Outcome::AtCap);
        assert_eq!(
            Outcome::of(&Err(JudgeError::LlmRefused), &tally),
            Outcome::Refused
        );
    }
}
