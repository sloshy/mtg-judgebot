//! The `recall` evaluator: is every expected rule id among the CR excerpts the
//! synthesizer is *shown*?
//!
//! Retrieval returns far more chunks than the prompt budget renders (a broad
//! category alone can hold 60), so "retrieved" and "shown" differ, and only
//! "shown" reaches the model. The gate scores what is shown, under the
//! production [`Budget`]; ids that were retrieved but cut by the budget are
//! reported separately, because that is a ranking bug rather than a retrieval
//! miss.

use std::{fmt, sync::Arc};

use judge_bot::{
    db::{PgResolver, PgRetriever, Vectors},
    synth::{Budget, shown_rules},
};
use judge_core::{Resolution, Resolver, Retriever, RuleChunk};
use sqlx::PgPool;

use crate::gold::Gold;

/// Per-question outcome.
pub struct Row {
    /// Gold question id.
    pub id: String,
    /// Resolution outcome per card name / nickname, e.g. `Dark Confidant=alias`.
    pub resolutions: Vec<String>,
    /// Expected ids the prompt shows (leaf ids count if their rule row is shown).
    pub hit: Vec<String>,
    /// Expected ids retrieved but cut by the budget (a subset of `missed`).
    pub cut: Vec<String>,
    /// Expected ids the prompt does not show.
    pub missed: Vec<String>,
}

/// Whole-run report.
pub struct Report {
    /// One row per answerable question.
    pub rows: Vec<Row>,
    /// Questions skipped because their source is `Tournament` / `OutOfScope`.
    pub skipped: usize,
}

impl Report {
    /// Fraction of expected rule ids the prompt shows, over all scored questions (0 when nothing was expected).
    #[must_use]
    pub fn recall(&self) -> f64 {
        let hit: usize = self.rows.iter().map(|r| r.hit.len()).sum();
        ratio(hit, self.total())
    }

    /// Fraction of expected rule ids retrieved at all, shown or not.
    #[must_use]
    pub fn retrieved(&self) -> f64 {
        let retrieved: usize = self.rows.iter().map(|r| r.hit.len() + r.cut.len()).sum();
        ratio(retrieved, self.total())
    }

    fn total(&self) -> usize {
        self.rows.iter().map(|r| r.hit.len() + r.missed.len()).sum()
    }
}

fn ratio(n: usize, total: usize) -> f64 {
    if total == 0 {
        return 0.0;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "counts of gold questions, far below 2^53"
    )]
    let r = n as f64 / total as f64;
    r
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let width = self
            .rows
            .iter()
            .map(|r| r.id.len())
            .max()
            .unwrap_or(8)
            .max(8);
        writeln!(
            f,
            "{:<width$}  {:>5}  {:<40}  cards",
            "question", "shown", "missed (* = retrieved, cut by the budget)"
        )?;
        for r in &self.rows {
            let total = r.hit.len() + r.missed.len();
            let missed: Vec<String> = r
                .missed
                .iter()
                .map(|m| {
                    if r.cut.contains(m) {
                        format!("{m}*")
                    } else {
                        m.clone()
                    }
                })
                .collect();
            writeln!(
                f,
                "{:<width$}  {:>2}/{:<2}  {:<40}  {}",
                r.id,
                r.hit.len(),
                total,
                missed.join(" "),
                r.resolutions.join(", ")
            )?;
        }
        let hit: usize = self.rows.iter().map(|r| r.hit.len()).sum();
        writeln!(
            f,
            "\nrecall (shown): {hit}/{} = {:.1}%, retrieved {:.1}%, over {} questions ({} skipped as out of scope)",
            self.total(),
            self.recall() * 100.0,
            self.retrieved() * 100.0,
            self.rows.len(),
            self.skipped
        )
    }
}

/// Run the evaluator over `gold`, with the vector leg when `vectors` is given.
///
/// # Errors
/// On database failure.
pub async fn run(
    pool: &PgPool,
    gold: &Gold,
    vectors: Option<Arc<Vectors>>,
) -> anyhow::Result<Report> {
    let resolver = PgResolver::new(pool.clone());
    let retriever = match vectors {
        Some(v) => PgRetriever::new(pool.clone()).with_vectors(v),
        None => PgRetriever::new(pool.clone()),
    };
    let mut rows = Vec::new();
    let mut skipped = 0;
    for q in &gold.questions {
        if !q.is_answerable() {
            skipped += 1;
            continue;
        }
        let mut resolutions = Vec::new();
        let mut cards = Vec::new();
        for name in q.cards.iter().chain(&q.nicknames_used) {
            let outcome = resolver.resolve(name).await?;
            resolutions.push(describe(name, &outcome));
            if let Resolution::Resolved { card, .. } = outcome
                && !cards.iter().any(|c: &judge_core::Card| c.id == card.id)
            {
                cards.push(card);
            }
        }
        let extraction = q.extraction();
        let question = judge_core::Question {
            thread_id: q.id.clone(),
            text: q.question.clone(),
        };
        let ctx = retriever.retrieve(&question, &cards, &extraction).await?;
        let shown = shown_rules(&ctx, &[], &Budget::default());
        let (mut hit, mut cut, mut missed) = (Vec::new(), Vec::new(), Vec::new());
        for expected in q
            .expected_rule_ids
            .iter()
            .map(super::gold::YamlScalar::as_text)
        {
            if present(shown.iter().copied(), &expected) {
                hit.push(expected);
            } else {
                if present(ctx.rules.iter(), &expected) {
                    cut.push(expected.clone());
                }
                missed.push(expected);
            }
        }
        rows.push(Row {
            id: q.id.clone(),
            resolutions,
            hit,
            cut,
            missed,
        });
    }
    Ok(Report { rows, skipped })
}

/// A leaf id (`613.1d`) counts if the leaf row or its rule row (`613.1`) is among `rules`.
fn present<'a>(mut rules: impl Iterator<Item = &'a RuleChunk>, id: &str) -> bool {
    let rule = id.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    rules.any(|r| {
        let rid: &str = r.id.as_ref();
        rid == id || rid == rule
    })
}

fn describe(name: &str, r: &Resolution) -> String {
    match r {
        Resolution::Resolved { card, via } => {
            if card.name.eq_ignore_ascii_case(name) {
                format!("{name}={via:?}")
            } else {
                format!("{name}={via:?}({})", card.name)
            }
        }
        Resolution::Ambiguous { candidates, .. } => {
            format!("{name}=AMBIGUOUS({})", candidates.len())
        }
        Resolution::NotFound { .. } => format!("{name}=NOT_FOUND"),
    }
}
