//! What one judge run spent: a [`Tally`] of [`Charge`]s, one per billed
//! model call or embedding request.
//!
//! The spend cap (`judge_llm::SpendMeter`) keeps one total per process, which
//! concurrent runs share, so a run's cost cannot be read off it. Each run
//! makes its own `Tally` instead and passes it, explicitly, to every port that
//! may call a model or an embedder ([`crate::Extractor`],
//! [`crate::Retriever`], [`crate::Synthesizer`], [`crate::Embedder`],
//! [`crate::CallStore::persist`]). The metered adapters add a charge to it
//! when they settle with the shared meter, so the cap is unchanged and the
//! run's figure is its own.
//!
//! A caller with no run to record (an ingest batch, a search) still passes a
//! tally, its own, and may read or drop it.

use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

/// The step of a run a charge belongs to. Stored as
/// `judge_run_charges.stage`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// The extraction and classification call.
    Extraction,
    /// A synthesis request: the first attempt, its retry, or a rerun of a
    /// truncated answer.
    Synthesis,
    /// The synthesis request that carries the `lookup_rules` result.
    ToolRound,
    /// An embedding request: the question's vector search, or the stored
    /// call's vector.
    Embedding,
}

impl Stage {
    /// Every variant, in declaration order.
    pub const ALL: [Self; 4] = [
        Self::Extraction,
        Self::Synthesis,
        Self::ToolRound,
        Self::Embedding,
    ];

    /// The stored id, equal to the serde name.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Extraction => "extraction",
            Self::Synthesis => "synthesis",
            Self::ToolRound => "tool_round",
            Self::Embedding => "embedding",
        }
    }
}

/// The tokens a response reported. An embedding request has only `input`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Uncached input tokens.
    pub input: u64,
    /// Output tokens, reasoning included.
    pub output: u64,
    /// Tokens read from the prompt cache.
    pub cache_read: u64,
    /// Tokens written to the prompt cache.
    pub cache_write: u64,
}

/// What a request, or a run, cost in USD.
///
/// A response that reported no usage was still billed, at an amount nobody
/// here knows. The spend cap keeps the worst case it reserved for it, and
/// the charge says so: [`Cost::WorstCase`], an upper bound rather than a
/// measurement. A sum with any worst case in it is itself one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cost {
    /// What the provider's reported usage costs at the configured price
    /// (0 for a free model).
    Billed(f64),
    /// The usage was not reported: the reservation the cap kept, an upper bound.
    WorstCase(f64),
}

impl Cost {
    /// Nothing, billed.
    pub const ZERO: Self = Self::Billed(0.0);

    /// The amount in USD, whichever kind.
    #[must_use]
    pub const fn usd(self) -> f64 {
        match self {
            Self::Billed(usd) | Self::WorstCase(usd) => usd,
        }
    }

    /// Whether this is (or includes) a worst case rather than a measurement.
    #[must_use]
    pub const fn is_estimate(self) -> bool {
        matches!(self, Self::WorstCase(_))
    }

    /// `self` plus `other`: a worst case if either is.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        let usd = self.usd() + other.usd();
        if self.is_estimate() || other.is_estimate() {
            Self::WorstCase(usd)
        } else {
            Self::Billed(usd)
        }
    }
}

/// One billed request.
#[derive(Clone, Debug, PartialEq)]
pub struct Charge {
    /// The step it was made for.
    pub stage: Stage,
    /// The provider kind (`anthropic`, `openai`, `voyage`).
    pub provider: String,
    /// The model the response named (the configured one for an embedding,
    /// or a response that named none).
    pub model: String,
    /// What the response reported, `None` when it reported nothing.
    pub tokens: Option<Tokens>,
    /// What it cost: billed from the reported usage, or the worst case when
    /// there was none.
    pub cost: Cost,
}

#[derive(Debug, Default)]
struct Ledger {
    charges: Vec<Charge>,
    capped: bool,
}

/// One run's charges. A cheap handle: clones add to the same list, so an
/// adapter that holds the tally across a conversation (the synthesis
/// typestate) clones it.
#[derive(Clone, Debug, Default)]
pub struct Tally(Arc<Mutex<Ledger>>);

impl Tally {
    /// An empty tally, for one run.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Where a model call for `stage` is billed.
    #[must_use]
    pub fn bill(&self, stage: Stage) -> Bill {
        Bill {
            tally: self.clone(),
            stage,
        }
    }

    /// Add a billed request.
    pub fn charge(&self, charge: Charge) {
        self.lock().charges.push(charge);
    }

    /// Note that the spend cap refused a model call for this run, so the
    /// run's failure is told apart from a provider's.
    pub fn cap_refused(&self) {
        self.lock().capped = true;
    }

    /// Whether the spend cap refused a model call for this run.
    #[must_use]
    pub fn was_capped(&self) -> bool {
        self.lock().capped
    }

    /// The charges so far, in the order they were billed.
    #[must_use]
    pub fn charges(&self) -> Vec<Charge> {
        self.lock().charges.clone()
    }

    /// The run's cost so far: the sum of its charges, a worst case when one
    /// of them reported no usage.
    #[must_use]
    pub fn cost(&self) -> Cost {
        self.lock()
            .charges
            .iter()
            .fold(Cost::ZERO, |sum, c| sum.plus(c.cost))
    }

    /// Billed requests at `stage`.
    #[must_use]
    pub fn count(&self, stage: Stage) -> usize {
        self.lock()
            .charges
            .iter()
            .filter(|c| c.stage == stage)
            .count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Where one model call's charge goes: a run's [`Tally`], under a [`Stage`].
/// `judge_llm::ChatModel::complete` takes one, so a model call cannot be made
/// without saying whose it is.
#[derive(Clone, Debug)]
pub struct Bill {
    tally: Tally,
    stage: Stage,
}

impl Bill {
    /// The stage the call is billed under.
    #[must_use]
    pub const fn stage(&self) -> Stage {
        self.stage
    }

    /// Add the billed call: `provider`, the `model` the response named, its
    /// `tokens` (`None` when it reported no usage) and `cost`.
    pub fn charge(&self, provider: &str, model: &str, tokens: Option<Tokens>, cost: Cost) {
        self.tally.charge(Charge {
            stage: self.stage,
            provider: provider.to_owned(),
            model: model.to_owned(),
            tokens,
            cost,
        });
    }

    /// The spend cap refused the call.
    pub fn cap_refused(&self) {
        self.tally.cap_refused();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_ids_are_the_serde_names() -> Result<(), serde_json::Error> {
        for s in Stage::ALL {
            assert_eq!(serde_json::to_value(s)?, serde_json::json!(s.id()));
        }
        Ok(())
    }

    #[test]
    fn clones_share_one_list_and_an_unreported_charge_makes_the_total_unknown() {
        let tally = Tally::new();
        assert_eq!(tally.cost(), Cost::ZERO);
        tally.bill(Stage::Extraction).charge(
            "anthropic",
            "m",
            Some(Tokens::default()),
            Cost::Billed(0.25),
        );
        let held = tally.clone();
        held.bill(Stage::Synthesis).charge(
            "anthropic",
            "m",
            Some(Tokens::default()),
            Cost::Billed(0.5),
        );
        assert_eq!(tally.cost(), Cost::Billed(0.75));
        assert_eq!(tally.count(Stage::Synthesis), 1);
        held.bill(Stage::Embedding)
            .charge("voyage", "v", None, Cost::WorstCase(0.25));
        assert_eq!(
            tally.cost(),
            Cost::WorstCase(1.0),
            "an upper bound, flagged"
        );
        assert_eq!(tally.charges().len(), 3);
        assert!(!tally.was_capped());
        assert!(!Cost::ZERO.plus(Cost::Billed(1.0)).is_estimate());
        held.bill(Stage::Synthesis).cap_refused();
        assert!(tally.was_capped());
    }
}
