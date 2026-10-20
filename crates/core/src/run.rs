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
    /// The caller went away, and the run stopped before its next model call.
    Abandoned,
    /// Anything else from an adapter: HTTP, the database, parsing.
    Upstream,
}

impl Outcome {
    /// Every variant, in declaration order.
    pub const ALL: [Self; 13] = [
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
        Self::Abandoned,
        Self::Upstream,
    ];

    /// How `result` ended, `tally` telling a spend-cap refusal and a run
    /// stopped for a caller that went away apart from another upstream
    /// failure.
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
            Err(JudgeError::Upstream(_)) if tally.was_stopped() => Self::Abandoned,
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
            Self::Abandoned => "abandoned",
            Self::Upstream => "upstream",
        }
    }
}

impl Outcome {
    /// The outcome whose stored id is `id`.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.id() == id)
    }

    /// The kind of ending the dashboard counts it as (#28); `None` for a run
    /// whose caller went away, which says nothing about the judge.
    #[must_use]
    pub const fn group(self) -> Option<OutcomeGroup> {
        match self {
            Self::Answered => Some(OutcomeGroup::Answered),
            Self::Ambiguous => Some(OutcomeGroup::Ambiguous),
            Self::NotFound => Some(OutcomeGroup::NotFound),
            Self::OutOfScope => Some(OutcomeGroup::OutOfScope),
            Self::Refused => Some(OutcomeGroup::Refused),
            Self::BadCitation
            | Self::MalformedCitation
            | Self::EmptyVerdict
            | Self::UncitedRules
            | Self::ToolMisuse
            | Self::AtCap
            | Self::Upstream => Some(OutcomeGroup::Failed),
            Self::Abandoned => None,
        }
    }
}

/// How a run ended, as the dashboard counts it: the six kinds a reader can
/// tell apart, every failure of the pipeline (a citation that never
/// validated, the spend cap, an upstream error) one `Failed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeGroup {
    /// A validated verdict.
    Answered,
    /// A "did you mean?".
    Ambiguous,
    /// A card that matched nothing.
    NotFound,
    /// Tournament policy or not a rules question.
    OutOfScope,
    /// The model declined.
    Refused,
    /// Every other ending but an abandoned run.
    Failed,
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
    fn every_outcome_parses_back_and_only_an_abandoned_run_has_no_group() {
        for o in Outcome::ALL {
            assert_eq!(Outcome::parse(o.id()), Some(o));
            assert_eq!(o.group().is_none(), o == Outcome::Abandoned, "{o}");
        }
        assert_eq!(Outcome::parse("agent"), None);
        assert_eq!(Outcome::AtCap.group(), Some(OutcomeGroup::Failed));
    }

    #[test]
    fn outcome_ids_are_the_serde_names() -> Result<(), serde_json::Error> {
        for o in Outcome::ALL {
            assert_eq!(serde_json::to_value(o)?, serde_json::json!(o.id()));
        }
        Ok(())
    }

    #[test]
    fn an_upstream_failure_is_at_the_cap_or_abandoned_only_when_a_call_was_refused() {
        let tally = Tally::new();
        let failed = Err(JudgeError::Upstream(anyhow::anyhow!("x")));
        assert_eq!(Outcome::of(&failed, &tally), Outcome::Upstream);
        let left = Tally::new();
        left.abandon();
        assert_eq!(
            Outcome::of(&failed, &left),
            Outcome::Upstream,
            "gone, but nothing was left unsent: the failure is its own"
        );
        assert!(left.bill(crate::Stage::Synthesis).stop_if_abandoned());
        assert_eq!(Outcome::of(&failed, &left), Outcome::Abandoned);
        tally.cap_refused();
        assert_eq!(Outcome::of(&failed, &tally), Outcome::AtCap);
        assert_eq!(
            Outcome::of(&Err(JudgeError::LlmRefused), &tally),
            Outcome::Refused
        );
    }
}
